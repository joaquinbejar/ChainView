//! End-to-end integration tests driven through the PUBLIC surface (issue #22,
//! `docs/TESTING.md` §7, `docs/03-data-providers.md` §11–§12).
//!
//! This file is a **separate crate** that links `chainview` and sees only its
//! public API — so it proves the **external-provider conformance**: a test-only
//! [`FauxProvider`] with a non-reserved `ProviderId("faux")` implements **only**
//! the public [`Provider`] port and reaches parity with a built-in **through the
//! public surface alone**, with no built-in special-casing
//! ([`docs/03-data-providers.md` §11](../docs/03-data-providers.md)):
//!
//! - it registers through `ChainViewApp::builder().register(..)` and resolves as
//!   a live source exactly like a built-in;
//! - its `fetch_chain` returns the NAMED [`ChainFetch`] artifact (never a bare
//!   `OptionChain`), and **subscription + a forced reconnect resubscribe off the
//!   fresh `ChainFetch.aliases`** — no bare chain, no symbol re-derivation
//!   (ADR-0009 / `docs/03-data-providers.md` §5);
//! - it plugs into the ADR-0009 supervised composition seam
//!   ([`spawn_supervised_subscription`]) identically to a built-in;
//! - its `fetch` folds into the public [`ChainStore`] with **domain parity** — the
//!   same data under a built-in id yields byte-identical leg state, and a streaming
//!   quote merges `Applied` — proving the `fetch -> store fold -> stream merge` path
//!   is id-agnostic through the port alone;
//! - it renders **end-to-end** through the public [`render`] entry (the external id
//!   in the status bar, the faux chain matrix in the body), with the reserved- and
//!   duplicate-id collisions surfacing as the TYPED [`RegistryError`] variants;
//! - its declared [`ProviderCapabilities`] gate the screens — the gate is TOTAL
//!   over the declared caps, **never** a `ProviderId` match.
//!
//! # What is deliberately NOT here (the public/in-crate split)
//!
//! The **committed golden** render-parity proof — the faux chain rendered to the
//! byte-exact `chain/deribit_btc_atm` golden — lives in-crate in
//! `src/tests_integration.rs` (`#[cfg(test)]`), because it needs `pub(crate)`
//! internals this external crate cannot reach: the `ui::chain::draw` body (the
//! public [`render`] wraps it in an id-bearing status bar, so a full-frame
//! byte-identity assertion is the WRONG shape — the UI *displays* the id as a
//! label but never *gates* on it) and the `assert_golden`/`buffer_to_text` golden
//! harness (promoting either would widen the semver-governed API, the same reason
//! the #19 goldens live in-crate). This file instead proves parity through the
//! **public** surface: domain store-fold equivalence and a public-`render`
//! end-to-end draw. Every test here is deterministic (no socket, no wall-clock
//! wait) and finishes far under the 10 s bound.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::Positive;
use optionstratlib::{ExpirationDate, OptionStyle};
use proptest::prelude::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use chainview::{
    AliasCatalog, App, ChainCapability, ChainFetch, ChainPollCapability, ChainSnapshot,
    ChainSource, ChainStore, ChainViewApp, ChainViewError, Command, Config,
    ContractSpecFingerprint, EventBridge, ExerciseStyle, ExpirySource, FinalTeardown,
    GreeksCapability, Instrument, InstrumentKey, LiveScreen, LiveState, MarketUpdate,
    MarketUpdateSink, MergeOutcome, Mode, ModeSelect, Provider, ProviderCapabilities,
    ProviderError, ProviderId, QuoteUpdate, RESERVED_PROVIDER_IDS, RegistryError, ScreenLoad,
    SettlementStyle, SourceBinding, StreamHealth, SubscriptionHandle, SubscriptionRequest,
    Supervisor, ThemeChoice, UnderlyingRef, ViewState, is_screen_reachable, render,
    spawn_supervised_subscription,};

// --- Test constructors (no unwrap/expect/indexing per the ruleset) -----------

#[track_caller]
fn pid(id: &str) -> ProviderId {
    match ProviderId::new(id) {
        Ok(p) => p,
        Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
    }
}

#[track_caller]
fn pos(value: f64) -> Positive {
    match Positive::new(value) {
        Ok(p) => p,
        Err(e) => panic!("invalid test positive `{value}`: {e}"),
    }
}

/// The fixed absolute-UTC expiry every faux chain shares.
#[track_caller]
fn expiry_utc() -> DateTime<Utc> {
    match DateTime::<Utc>::from_timestamp(1_751_011_200, 0) {
        Some(t) => t,
        None => panic!("valid fixed expiry timestamp"),
    }
}

/// The two strikes the faux chain lists.
const STRIKES: [f64; 2] = [60_000.0, 61_000.0];

/// The Deribit-shaped economic-equivalence fingerprint the faux legs share.
fn faux_spec(underlying: &str) -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: 1,
        settlement: SettlementStyle::Cash,
        exercise: ExerciseStyle::European,
        quote_currency: "USD".to_owned(),
        venue_product_code: underlying.to_owned(),
    }
}

// =============================================================================
// FauxProvider: an EXTERNAL adapter using ONLY the public Provider port.
// =============================================================================

/// A test-only external provider implementing **only** the public [`Provider`]
/// port under a non-reserved id — the proof the port is a real public surface,
/// not a built-in convenience (`docs/03-data-providers.md` §11).
///
/// Every `fetch_chain` stamps a monotonically increasing generation into its
/// alias symbols, so a re-fetch yields **fresh** aliases (distinct from the prior
/// set) — letting a test assert a forced reconnect resubscribes off the fresh
/// `ChainFetch.aliases`, never a cached/derived set. Every `subscribe` records
/// the native symbols it was handed, so the resubscription set is inspectable.
struct FauxProvider {
    id: ProviderId,
    capabilities: ProviderCapabilities,
    /// The native symbols handed to each `subscribe`/resubscribe, in call order.
    subscribe_log: Arc<Mutex<Vec<Vec<String>>>>,
    /// The next `fetch_chain` generation — stamped into alias symbols.
    fetch_gen: Arc<AtomicU64>,
    /// The recoverable-disconnect trigger: firing it makes the supervised
    /// reconnect loop treat the socket as dropped, back off, refetch the chain, and
    /// resubscribe off the FRESH aliases — the real reconnect seam (issue #22,
    /// finding 6), not a handle drop.
    disconnect: Arc<Notify>,
}

impl FauxProvider {
    fn with_caps(id: ProviderId, capabilities: ProviderCapabilities) -> Self {
        Self {
            id,
            capabilities,
            subscribe_log: Arc::new(Mutex::new(Vec::new())),
            fetch_gen: Arc::new(AtomicU64::new(0)),
            disconnect: Arc::new(Notify::new()),
        }
    }

    /// A chain-capable faux provider (the built-in-parity shape).
    fn chainful(id: ProviderId) -> Self {
        Self::with_caps(
            id,
            ProviderCapabilities::builder()
                .chain(ChainCapability::Assemble)
                .greeks(GreeksCapability::Provided)
                .chain_poll(ChainPollCapability::Poll {
                    interval_hint_secs: 2,
                })
                .build(),
        )
    }

    fn subscribe_log(&self) -> Arc<Mutex<Vec<Vec<String>>>> {
        Arc::clone(&self.subscribe_log)
    }

    /// A trigger clone the test fires to simulate a recoverable socket disconnect.
    fn disconnect_trigger(&self) -> Arc<Notify> {
        Arc::clone(&self.disconnect)
    }

    /// Build a fully-normalized [`ChainFetch`] whose alias symbols carry the next
    /// generation, so successive fetches yield fresh (distinct) aliases.
    fn build_fetch(&self, underlying: &str) -> ChainFetch {
        let generation = self.fetch_gen.fetch_add(1, Ordering::SeqCst);
        build_faux_fetch(&self.id, underlying, generation)
    }
}

/// Record one (re)subscription's native-symbol set into the shared log.
fn record_subscription(log: &Arc<Mutex<Vec<Vec<String>>>>, legs: &[Instrument]) {
    let symbols: Vec<String> = legs
        .iter()
        .map(|instrument| instrument.native_symbol.clone())
        .collect();
    match log.lock() {
        Ok(mut guard) => guard.push(symbols),
        Err(_) => panic!("faux subscribe_log mutex poisoned"),
    }
}

/// A generation-stamped [`ChainFetch`] — a free fn so the spawned reconnect loop
/// can refetch off the moved state (it cannot borrow `&FauxProvider`).
fn build_faux_fetch(id: &ProviderId, underlying: &str, generation: u64) -> ChainFetch {
    let mut chain = OptionChain::new(
        underlying,
        pos(60_000.0),
        "2025-06-27".to_owned(),
        None,
        None,
    );
    let mut aliases = AliasCatalog::new();
    for strike in STRIKES {
        chain.add_option(
            pos(strike),
            Some(pos(1.0)),
            Some(pos(1.2)),
            Some(pos(2.0)),
            Some(pos(2.4)),
            pos(0.5),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        for style in [OptionStyle::Call, OptionStyle::Put] {
            aliases.insert(Instrument {
                key: InstrumentKey {
                    underlying: underlying.to_owned(),
                    expiration_utc: expiry_utc(),
                    strike: pos(strike),
                    style,
                },
                provider: id.clone(),
                // The generation stamp is what makes a re-fetch's aliases FRESH.
                native_symbol: format!(
                    "FAUX-{underlying}-{strike}-{}-g{generation}",
                    style.as_str()
                ),
                stream_symbol: Some(format!(
                    "faux-stream-{strike}-{}-g{generation}",
                    style.as_str()
                )),
                spec: faux_spec(underlying),
            });
        }
    }
    ChainFetch::new(
        chain,
        ExpirySource::new(underlying, expiry_utc(), id.clone()),
        aliases,
    )
}

/// A streaming-current [`ChainSnapshot`] control-class backfill from a fetch.
fn faux_snapshot(fetch: &ChainFetch) -> ChainSnapshot {
    ChainSnapshot {
        chain_key: (
            fetch.expiry_source.provider.clone(),
            fetch.expiry_source.underlying.clone(),
            fetch.expiry_source.expiration_utc,
        ),
        chain: fetch.chain.clone(),
        aliases: fetch.aliases.clone(),
        source: ChainSource::Merged,
        health: StreamHealth::Live,
        last_full_poll: Some(fetch.expiry_source.expiration_utc),
    }
}

#[async_trait]
impl Provider for FauxProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        Ok(vec![UnderlyingRef::new("BTC")])
    }

    async fn fetch_chain(
        &self,
        underlying: &str,
        _expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        // A fresh generation per poll -> fresh aliases (the reconnect backfill
        // resubscribes off THESE, never a stale/derived set).
        Ok(self.build_fetch(underlying))
    }

    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        mut sink: MarketUpdateSink,
    ) -> Result<SubscriptionHandle, ProviderError> {
        // Record EXACTLY the legs the caller handed us (taken from
        // ChainFetch.aliases) — proving subscription is off the fetch artifact,
        // never a bare OptionChain or a re-derivation.
        record_subscription(&self.subscribe_log, &req.instruments);
        // A control-class Health(Live) flows through the sink to the render side —
        // the port carries normalized domain data across the seam.
        let _ = sink
            .send(MarketUpdate::Health(self.id.clone(), StreamHealth::Live))
            .await;

        // The adapter-owned reconnect loop — exactly the Deribit shape (ADR-0009):
        // it selects on the supervisor's child token (a clean stop) and a
        // recoverable disconnect trigger. On a disconnect it surfaces
        // `Reconnecting`, refetches the chain (FRESH aliases), resubscribes off
        // those, and emits the backfill `Chain` — the real reconnect/backfill seam
        // (issue #22, finding 6), not a handle drop.
        let cancel = req.cancel;
        let loop_cancel = cancel.clone();
        let id = self.id.clone();
        let underlying = req.underlying.clone();
        let log = Arc::clone(&self.subscribe_log);
        let fetch_gen = Arc::clone(&self.fetch_gen);
        let disconnect = Arc::clone(&self.disconnect);
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = loop_cancel.cancelled() => break,
                    () = disconnect.notified() => {
                        // Recoverable disconnect -> reconnect + backfill.
                        let _ = sink
                            .send(MarketUpdate::Health(
                                id.clone(),
                                StreamHealth::Reconnecting { attempt: 1 },
                            ))
                            .await;
                        let generation = fetch_gen.fetch_add(1, Ordering::SeqCst);
                        let fetch = build_faux_fetch(&id, &underlying, generation);
                        let legs: Vec<Instrument> =
                            fetch.aliases.instruments().cloned().collect();
                        // Resubscribe off the FRESH aliases (record them), then emit
                        // the reconciled structure as a control-class Chain backfill,
                        // then Live.
                        record_subscription(&log, &legs);
                        let _ = sink.send(MarketUpdate::Chain(faux_snapshot(&fetch))).await;
                        let _ = sink
                            .send(MarketUpdate::Health(id.clone(), StreamHealth::Live))
                            .await;
                    }
                }
            }
        });
        Ok(SubscriptionHandle::spawned(cancel, join))
    }
}

/// A no-op final teardown (no real TTY) for the supervisor in these tests.
struct NoopTeardown;

impl FinalTeardown for NoopTeardown {
    fn run(self: Box<Self>) {}
}

/// A live [`Config`] selecting `provider`, with the zero-config defaults for the
/// rest (built directly — every field is public — so registration is exercised
/// without touching the environment/file).
fn live_config(provider: &str) -> Config {
    Config {
        provider: pid(provider),
        underlying: "BTC".to_owned(),
        refresh_interval: Duration::from_secs(2),
        tick_interval: Duration::from_millis(250),
        channel_capacity: 1024,
        log_file: None,
        theme: ThemeChoice::Auto,
        no_color: false,
        providers: BTreeMap::new(),
        mode: ModeSelect::Live,
    }
}

/// The sorted native-symbol set of a fetch's alias catalog (iteration order is
/// unspecified, so callers compare sorted).
fn sorted_aliases(fetch: &ChainFetch) -> Vec<String> {
    let mut symbols: Vec<String> = fetch
        .aliases
        .instruments()
        .map(|instrument| instrument.native_symbol.clone())
        .collect();
    symbols.sort();
    symbols
}

fn sorted(mut symbols: Vec<String>) -> Vec<String> {
    symbols.sort();
    symbols
}

// =============================================================================
// 1. An external faux provider registers + resolves like a built-in.
// =============================================================================

#[test]
fn test_faux_external_provider_registers_and_resolves_like_a_builtin() {
    // Registration is through the PUBLIC builder; a chain-capable external
    // provider resolves as a live source identically to a built-in (the
    // composite-source guard reads its declared capabilities, never its id).
    let result = ChainViewApp::builder()
        .register(FauxProvider::chainful(pid("faux")))
        .with_config(live_config("faux"))
        .run();
    assert!(
        result.is_ok(),
        "an external chain-capable provider resolves as a live source: {result:?}"
    );
}

#[test]
fn test_faux_provider_uses_a_valid_non_reserved_id() {
    let faux = FauxProvider::chainful(pid("faux"));
    assert_eq!(faux.id().as_str(), "faux");
    assert!(
        !faux.id().is_reserved(),
        "an external adapter must use a non-reserved id"
    );
}

#[test]
fn test_faux_external_provider_duplicate_registration_is_typed_error() {
    // Two external registrations under one id are a TYPED `RegistryError::DuplicateId`
    // through the PUBLIC builder — never a panic or a silent last-writer-wins
    // (`docs/03-data-providers.md` §11.2). The whole path is exercised with only
    // re-exported items, proving the external duplicate story compiles + surfaces
    // typed against the public surface.
    let result = ChainViewApp::builder()
        .register(FauxProvider::chainful(pid("faux")))
        .register(FauxProvider::chainful(pid("faux")))
        .with_config(live_config("faux"))
        .run();
    match result {
        Err(ChainViewError::Registry(RegistryError::DuplicateId(id))) => {
            assert_eq!(id.as_str(), "faux");
        }
        other => panic!("expected DuplicateId(faux) from the public surface, got {other:?}"),
    }
}

#[test]
fn test_faux_external_provider_reserved_id_registration_is_typed_error() {
    // An external registration under a RESERVED built-in id is a TYPED
    // `RegistryError::ReservedId` through the PUBLIC builder — an external adapter
    // can never masquerade as a built-in or shadow its config namespace
    // (`docs/03-data-providers.md` §11.2). The registry surfaces the build-phase
    // collision BEFORE config resolution, so `run` reports the typed variant with
    // the offending id. This is the OTHER typed collision (the companion of the
    // duplicate story above), proven against the public surface alone.
    let result = ChainViewApp::builder()
        .register(FauxProvider::chainful(pid("deribit")))
        .with_config(live_config("deribit"))
        .run();
    match result {
        Err(ChainViewError::Registry(RegistryError::ReservedId(id))) => {
            assert_eq!(id.as_str(), "deribit");
        }
        other => panic!("expected ReservedId(deribit) from the public surface, got {other:?}"),
    }
}

// =============================================================================
// 2. fetch_chain returns the NAMED ChainFetch; a forced reconnect RESUBSCRIBES
//    off the fresh ChainFetch.aliases (no bare OptionChain, no re-derivation).
// =============================================================================

#[tokio::test]
async fn test_faux_reconnect_resubscribes_off_fresh_chainfetch_aliases() {
    let faux = FauxProvider::chainful(pid("faux"));
    let log = faux.subscribe_log();
    let disconnect = faux.disconnect_trigger();
    let exp = ExpirationDate::DateTime(expiry_utc());
    let (mut bridge, senders) = EventBridge::new(64);

    // (a) POLL leg: fetch_chain returns the NAMED ChainFetch artifact + its aliases.
    let fetch1 = match faux.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("faux fetch_chain must succeed, got {e}"),
    };
    let aliases1 = sorted_aliases(&fetch1);
    assert!(
        !aliases1.is_empty(),
        "the fetch artifact carries the per-leg alias catalog to (re)subscribe off"
    );

    // (b) SUBSCRIBE off the returned ChainFetch.aliases (records subscription #1).
    let legs1: Vec<Instrument> = fetch1.aliases.instruments().cloned().collect();
    let cancel = CancellationToken::new();
    let req = SubscriptionRequest::new("BTC", expiry_utc(), legs1, cancel.clone());
    let handle = match faux.subscribe(req, senders.market_update_sink()).await {
        Ok(handle) => handle,
        Err(e) => panic!("faux subscribe must succeed, got {e}"),
    };

    // (c) Force a RECOVERABLE DISCONNECT (NOT a handle drop): the adapter-owned
    // reconnect loop surfaces Reconnecting, re-fetches the chain (FRESH aliases),
    // resubscribes off THOSE, and emits the backfill Chain — the real §5 seam.
    disconnect.notify_one();

    // Drive the loop to completion of the reconnect: drain the bridge while yielding
    // until the resubscribe is recorded AND the backfill Chain has flowed through.
    let mut chains = 0usize;
    let mut lives = 0usize;
    let mut reconnecting = 0usize;
    let mut resubscribed = false;
    for _ in 0..2_000 {
        bridge.pump_into(
            |update| match update {
                MarketUpdate::Chain(_) => chains = chains.checked_add(1).unwrap_or(chains),
                MarketUpdate::Health(_, StreamHealth::Live) => {
                    lives = lives.checked_add(1).unwrap_or(lives);
                }
                MarketUpdate::Health(_, StreamHealth::Reconnecting { .. }) => {
                    reconnecting = reconnecting.checked_add(1).unwrap_or(reconnecting);
                }
                MarketUpdate::Health(_, StreamHealth::Stale { .. })
                | MarketUpdate::Quote(_)
                | MarketUpdate::Greeks(_)
                | MarketUpdate::Depth(_) => {}
            },
            |_command| {},
        );
        resubscribed = log.lock().map(|g| g.len() >= 2).unwrap_or(false);
        if resubscribed && chains >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        resubscribed,
        "the reconnect recorded a second (re)subscription"
    );

    // (d) The recorded subscriptions: #1 = fetch #1's aliases EXACTLY; the resubscribe
    // used FRESH aliases (a distinct, higher-generation set), never the stale one.
    let recorded = match log.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => panic!("faux subscribe_log mutex poisoned"),
    };
    assert_eq!(recorded.len(), 2, "two subscriptions were recorded");
    let first = match recorded.first() {
        Some(first) => sorted(first.clone()),
        None => panic!("expected the initial subscription"),
    };
    let second = match recorded.get(1) {
        Some(second) => sorted(second.clone()),
        None => panic!("expected the resubscription"),
    };
    assert_eq!(
        first, aliases1,
        "the initial subscription used fetch #1's ChainFetch.aliases exactly"
    );
    assert_ne!(
        first, second,
        "the RESUBSCRIBE used FRESH re-fetched aliases, not the stale set (§5 backfill = current state)"
    );

    // (e) The reconnect drove the control-class path end to end: Reconnecting, a
    // Chain backfill, and a resurfaced Live all reached the render bridge.
    assert!(reconnecting >= 1, "the disconnect surfaced Reconnecting");
    assert!(chains >= 1, "the reconnect emitted a Chain backfill");
    assert!(lives >= 1, "the reconnect resurfaced Live");

    // Clean stop: cancel the loop and drop the handle.
    cancel.cancel();
    drop(handle);
}

// =============================================================================
// 3. The faux provider plugs into the ADR-0009 supervised composition seam
//    identically to a built-in (spawn_supervised_subscription is public).
// =============================================================================

#[tokio::test]
async fn test_faux_provider_plugs_into_supervision_like_a_builtin() {
    let provider: Arc<dyn Provider> = Arc::new(FauxProvider::chainful(pid("faux")));
    let exp = ExpirationDate::DateTime(expiry_utc());
    let fetch = match provider.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("faux fetch_chain must succeed, got {e}"),
    };
    let legs: Vec<Instrument> = fetch.aliases.instruments().cloned().collect();

    let (_bridge, senders) = EventBridge::new(64);
    let mut supervisor = Supervisor::new(Box::new(NoopTeardown));
    // The SAME public seam the live run-loop uses — no built-in special-casing.
    let extra = spawn_supervised_subscription(
        &provider,
        "BTC",
        expiry_utc(),
        legs,
        &senders,
        &mut supervisor,
    )
    .await;
    // A streaming provider is watched under the supervisor and hands back the
    // per-provider cancel handle (Codex finding #3): the caller keeps it to cancel
    // ONLY this provider's subtree.
    let subscription = match extra {
        Ok(Some(subscription)) => subscription,
        other => panic!("expected the faux task registered under the supervisor, got {other:?}"),
    };
    assert_eq!(
        subscription.provider().as_str(),
        "faux",
        "the cancel handle names the supervised provider"
    );

    supervisor.request_quit();
    let cause = supervisor.run().await;
    assert!(
        cause.is_clean(),
        "the faux provider's supervised reconnect loop joins cleanly on quit"
    );
}

// =============================================================================
// 4. The UI gate is TOTAL over declared capabilities, NEVER a ProviderId match.
// =============================================================================

#[test]
fn test_screen_gating_is_capability_driven_never_provider_id() {
    // Two faux providers with IDENTICAL capabilities but DIFFERENT ids gate every
    // screen identically — the gate reads only the declared caps (the render-side
    // special-casing the arch test also forbids).
    let caps = ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        .depth(false)
        .greeks(GreeksCapability::ComputedLocally)
        .build();
    let a = FauxProvider::with_caps(pid("faux"), caps);
    let b = FauxProvider::with_caps(pid("othervendor"), caps);
    assert_eq!(a.capabilities(), b.capabilities());
    for screen in [
        LiveScreen::Chain,
        LiveScreen::Depth,
        LiveScreen::Surface,
        LiveScreen::Payoff,
    ] {
        assert_eq!(
            is_screen_reachable(screen, &a.capabilities()),
            is_screen_reachable(screen, &b.capabilities()),
            "gating for {screen:?} is id-independent",
        );
    }
    // The declared caps drive reachability: depth:false -> no depth screen;
    // greeks:ComputedLocally -> a surface screen; a chain -> chain + payoff.
    assert!(!is_screen_reachable(LiveScreen::Depth, &a.capabilities()));
    assert!(is_screen_reachable(LiveScreen::Surface, &a.capabilities()));
    assert!(is_screen_reachable(LiveScreen::Chain, &a.capabilities()));
    assert!(is_screen_reachable(LiveScreen::Payoff, &a.capabilities()));
}

// =============================================================================
// 5. The `fetch -> store fold -> stream merge -> render` path reaches DOMAIN and
//    RENDER parity through the port ALONE — driven with only public items.
// =============================================================================

/// A fixed as-of / poll-receipt instant for the store-fold + render checks, so
/// the merge and the rendered frame are byte-stable across machines (no wall
/// clock). Distinct from the fixture's own contract expiry.
#[track_caller]
fn as_of() -> DateTime<Utc> {
    match DateTime::<Utc>::from_timestamp(1_700_000_000, 0) {
        Some(t) => t,
        None => panic!("valid fixed as-of timestamp"),
    }
}

/// Seed a [`ChainStore`] from a fetch through the PUBLIC domain surface — the
/// `fetch -> store fold` step a built-in takes, with no adapter internals.
fn seed_store(fetch: ChainFetch) -> ChainStore {
    ChainStore::seed(fetch, ChainSource::Merged, Duration::from_secs(2), as_of())
}

/// The per-strike `(strike, call_bid, call_ask, put_bid, put_ask)` view of a
/// store's normalized `OptionChain` — the id-independent leg state two stores are
/// compared on (`OptionChain` derives no `PartialEq`, so parity is asserted over
/// this projection).
type Leg = (
    Positive,
    Option<Positive>,
    Option<Positive>,
    Option<Positive>,
    Option<Positive>,
);

fn chain_legs(store: &ChainStore) -> Vec<Leg> {
    store
        .chain()
        .options
        .iter()
        .map(|od| {
            (
                od.strike_price,
                od.call_bid,
                od.call_ask,
                od.put_bid,
                od.put_ask,
            )
        })
        .collect()
}

/// An idempotent streaming [`QuoteUpdate`] for one faux leg, carrying the same
/// bid/ask the seeded chain already holds, so folding it is a real within-provider
/// `apply_quote` merge (`MergeOutcome::Applied`) with no value drift. `provider`
/// equals the store's source provider, so the overlay gate is a no-op.
fn faux_quote(
    provider: &ProviderId,
    strike: Positive,
    style: OptionStyle,
    bid: Positive,
    ask: Positive,
) -> QuoteUpdate {
    QuoteUpdate {
        instrument: Instrument {
            key: InstrumentKey {
                underlying: "BTC".to_owned(),
                expiration_utc: expiry_utc(),
                strike,
                style,
            },
            provider: provider.clone(),
            native_symbol: format!("FAUX-BTC-{strike}-{}", style.as_str()),
            stream_symbol: None,
            spec: faux_spec("BTC"),
        },
        bid: Some(bid),
        ask: Some(ask),
        last: None,
        bid_size: None,
        ask_size: None,
        event_time: None,
        received_time: as_of(),
    }
}

/// A Ready live chain [`App`] bound to `provider`'s declared `caps` and the seeded
/// `store` — the state the render loop hands [`render`] for a live source. The
/// command channel is created outside any runtime (only an awaited send/recv would
/// need one) and never touched by the pure draw path.
fn live_chain_app(store: ChainStore, provider: ProviderId, caps: ProviderCapabilities) -> App {
    let mut live = LiveState::new(
        SourceBinding::new(provider, caps, StreamHealth::Live),
        store,
    );
    live.screen = LiveScreen::Chain;
    live.load = ScreenLoad::Ready;
    let (tx, _rx) = mpsc::channel::<Command>(16);
    App::new(Mode::Live(live), ThemeChoice::Auto, tx)
}

/// Render `app` full-frame through the PUBLIC [`render`] entry into a fixed
/// 120x40 `TestBackend`, returning the visible buffer as text (one row per line,
/// trailing spaces trimmed). This is the external-crate view of the render path:
/// no `pub(crate)` golden harness, no `ui::chain::draw` — only public `render` +
/// [`ViewState`].
#[track_caller]
fn render_full_frame_text(app: &App) -> String {
    let mut term = match Terminal::new(TestBackend::new(120, 40)) {
        Ok(t) => t,
        Err(e) => panic!("TestBackend construction failed: {e}"),
    };
    let mut view = ViewState::new();
    view.sync(app);
    match term.draw(|frame| render(app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("full-frame render failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

/// A local buffer-to-text: each cell's visible symbol, row by row, trailing spaces
/// trimmed. The in-crate golden helper is `pub(crate)` and so unreachable here —
/// re-deriving the trivial projection keeps this proof on the public surface.
fn buffer_to_text(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                line.push_str(cell.symbol());
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn test_faux_fetch_folds_into_chainstore_like_a_builtin_source() {
    let exp = ExpirationDate::DateTime(expiry_utc());

    // The external faux source: fetch -> the NAMED ChainFetch (never a bare chain).
    let faux = FauxProvider::chainful(pid("faux"));
    let faux_fetch = match faux.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("faux fetch_chain must succeed, got {e}"),
    };
    // The SAME chain data under a RESERVED built-in id, used ONLY to build the
    // comparison store — never registered (reservation gates `register`, not
    // construction). The domain fold reads the normalized `OptionChain`, never the
    // provider id, so both stores must hold identical leg state.
    let builtin = FauxProvider::chainful(pid("deribit"));
    let builtin_fetch = match builtin.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("the built-in-shaped fetch_chain must succeed, got {e}"),
    };

    let mut faux_store = seed_store(faux_fetch);
    let builtin_store = seed_store(builtin_fetch);

    // (a) fetch -> normalize: the faux fetch became an optionstratlib `OptionChain`
    // carrying exactly its two declared strikes.
    let strikes: Vec<Positive> = faux_store
        .chain()
        .options
        .iter()
        .map(|od| od.strike_price)
        .collect();
    assert!(
        strikes == vec![pos(60_000.0), pos(61_000.0)],
        "the faux fetch normalized into the expected OptionChain strikes"
    );

    // (b) DOMAIN parity: the external-id and built-in-id stores hold byte-identical
    // leg state — the fold is id-agnostic (the deribit-shaped-path parity the
    // conformance contract names).
    assert!(
        chain_legs(&faux_store) == chain_legs(&builtin_store),
        "the store state matches the same data folded through a built-in-shaped source"
    );

    // (c) stream merge: an idempotent quote on a present leg folds `Applied` — the
    // poll->stream merge path treats an external provider's leg like a built-in's.
    let outcome = faux_store.apply_quote(&faux_quote(
        &pid("faux"),
        pos(60_000.0),
        OptionStyle::Call,
        pos(1.0),
        pos(1.2),
    ));
    assert_eq!(
        outcome,
        MergeOutcome::Applied,
        "a streaming quote folds into the faux chain exactly as a built-in's would"
    );
}

#[tokio::test]
async fn test_faux_provider_renders_end_to_end_through_public_render() {
    // The faux external source drives the render path END TO END through the PUBLIC
    // `render` entry — the external-crate view of "runs end-to-end and renders",
    // with NO built-in special-casing.
    let exp = ExpirationDate::DateTime(expiry_utc());
    let faux = FauxProvider::chainful(pid("faux"));
    let caps = faux.capabilities();
    let fetch = match faux.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("faux fetch_chain must succeed, got {e}"),
    };
    let app = live_chain_app(seed_store(fetch), pid("faux"), caps);
    let text = render_full_frame_text(&app);

    // The external provider id renders in the status bar (a DISPLAY label — the UI
    // shows the id but never GATES on it), and the faux chain's underlying plus
    // BOTH strikes render in the chain matrix body: the external source reaches the
    // screen through the public render path with no id special-casing.
    assert!(
        text.contains("faux"),
        "the external provider id renders in the status bar:\n{text}"
    );
    assert!(
        text.contains("BTC"),
        "the faux chain's underlying renders through the public render path:\n{text}"
    );
    assert!(
        text.contains("60000") && text.contains("61000"),
        "the faux chain's strikes render in the matrix body:\n{text}"
    );
}

// --- proptest helpers --------------------------------------------------------

/// A strategy over grammar-valid, NON-reserved provider ids.
fn valid_custom_id() -> impl Strategy<Value = ProviderId> {
    "[a-z][a-z0-9]{1,10}"
        .prop_map(ProviderId::new)
        .prop_filter("valid, non-reserved id", |r| {
            r.as_ref().is_ok_and(|p| !p.is_reserved())
        })
        .prop_map(|r| match r {
            Ok(p) => p,
            Err(e) => panic!("filtered id was invalid: {e}"),
        })
}

fn chain_capability(idx: u8) -> ChainCapability {
    match idx % 4 {
        0 => ChainCapability::Native,
        1 => ChainCapability::Assemble,
        2 => ChainCapability::Partial,
        _ => ChainCapability::None,
    }
}

fn greeks_capability(idx: u8) -> GreeksCapability {
    match idx % 3 {
        0 => GreeksCapability::Provided,
        1 => GreeksCapability::ComputedLocally,
        _ => GreeksCapability::None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, max_shrink_iters: 50_000, ..ProptestConfig::default() })]

    /// `capabilities_total` over the faux provider (`docs/TESTING.md` §3): a
    /// provider self-declares a COMPLETE `ProviderCapabilities`, and screen gating
    /// is a TOTAL function of those declared caps — never the id. The id is
    /// generated freely and never consulted: the gate takes only `&caps`.
    #[test]
    fn prop_capabilities_total_over_faux_provider(
        id in valid_custom_id(),
        depth in any::<bool>(),
        chain_idx in any::<u8>(),
        greeks_idx in any::<u8>(),
    ) {
        let chain = chain_capability(chain_idx);
        let greeks = greeks_capability(greeks_idx);
        let caps = ProviderCapabilities::builder()
            .chain(chain)
            .depth(depth)
            .greeks(greeks)
            .build();
        let faux = FauxProvider::with_caps(id, caps);
        // The provider reports EXACTLY its declared, complete capabilities.
        prop_assert_eq!(faux.capabilities(), caps);
        // Gating is derived from the declared caps alone (id-independent).
        let chain_ok = !matches!(chain, ChainCapability::None);
        let greeks_ok = !matches!(greeks, GreeksCapability::None);
        prop_assert_eq!(is_screen_reachable(LiveScreen::Chain, &caps), chain_ok);
        prop_assert_eq!(is_screen_reachable(LiveScreen::Payoff, &caps), chain_ok);
        prop_assert_eq!(is_screen_reachable(LiveScreen::Depth, &caps), depth);
        prop_assert_eq!(is_screen_reachable(LiveScreen::Surface, &caps), greeks_ok);
    }

    /// An external registration using ANY reserved built-in id is refused through
    /// the public builder — the faux external path can never masquerade as a
    /// built-in (`docs/03-data-providers.md` §11.2).
    #[test]
    fn prop_faux_registration_rejects_every_reserved_id(idx in 0usize..RESERVED_PROVIDER_IDS.len()) {
        let id_str = match RESERVED_PROVIDER_IDS.get(idx) {
            Some(id) => *id,
            None => return Ok(()),
        };
        let id = match ProviderId::new(id_str) {
            Ok(p) => p,
            Err(e) => panic!("reserved id `{id_str}` must be grammar-valid: {e}"),
        };
        let result = ChainViewApp::builder()
            .register(FauxProvider::chainful(id))
            .with_config(live_config(id_str))
            .run();
        prop_assert!(
            matches!(
                result,
                Err(ChainViewError::Registry(RegistryError::ReservedId(_)))
            ),
            "a reserved id used externally is refused as the typed ReservedId, got {result:?}"
        );
    }
}
