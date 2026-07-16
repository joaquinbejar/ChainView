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
//! - its declared [`ProviderCapabilities`] gate the screens — the gate is TOTAL
//!   over the declared caps, **never** a `ProviderId` match.
//!
//! The complementary in-crate live-path golden render lives in
//! `src/tests_integration.rs` (`#[cfg(test)]`), because it needs `pub(crate)`
//! internals this public crate cannot reach. Every test here is deterministic
//! (no socket, no wall-clock wait) and finishes far under the 10 s bound.

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
use tokio_util::sync::CancellationToken;

use chainview::{
    AliasCatalog, ChainCapability, ChainFetch, ChainPollCapability, ChainViewApp, Config,
    ContractSpecFingerprint, EventBridge, ExerciseStyle, ExpirySource, FinalTeardown,
    GreeksCapability, Instrument, InstrumentKey, LiveScreen, MarketUpdate, MarketUpdateSink,
    ModeSelect, Provider, ProviderCapabilities, ProviderError, ProviderId, RESERVED_PROVIDER_IDS,
    SettlementStyle, StreamHealth, SubscriptionHandle, SubscriptionRequest, Supervisor,
    ThemeChoice, UnderlyingRef, is_screen_reachable, spawn_supervised_subscription,
};

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
    /// The native symbols handed to each `subscribe`, in call order.
    subscribe_log: Arc<Mutex<Vec<Vec<String>>>>,
    /// The next `fetch_chain` generation — stamped into alias symbols.
    fetch_gen: Arc<AtomicU64>,
}

impl FauxProvider {
    fn with_caps(id: ProviderId, capabilities: ProviderCapabilities) -> Self {
        Self {
            id,
            capabilities,
            subscribe_log: Arc::new(Mutex::new(Vec::new())),
            fetch_gen: Arc::new(AtomicU64::new(0)),
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

    /// Build a fully-normalized [`ChainFetch`] whose alias symbols carry
    /// `generation`, so successive fetches yield fresh (distinct) aliases.
    fn build_fetch(&self, underlying: &str, generation: u64) -> ChainFetch {
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
                    provider: self.id.clone(),
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
            ExpirySource::new(underlying, expiry_utc(), self.id.clone()),
            aliases,
        )
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
        let generation = self.fetch_gen.fetch_add(1, Ordering::SeqCst);
        Ok(self.build_fetch(underlying, generation))
    }

    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        mut sink: MarketUpdateSink,
    ) -> Result<SubscriptionHandle, ProviderError> {
        // Record EXACTLY the legs the caller handed us (taken from
        // ChainFetch.aliases) — proving subscription is off the fetch artifact,
        // never a bare OptionChain or a re-derivation.
        let symbols: Vec<String> = req
            .instruments
            .iter()
            .map(|instrument| instrument.native_symbol.clone())
            .collect();
        match self.subscribe_log.lock() {
            Ok(mut guard) => guard.push(symbols),
            Err(_) => panic!("faux subscribe_log mutex poisoned"),
        }
        // A control-class Health(Live) flows through the sink to the render side —
        // the port carries normalized domain data across the seam.
        let _ = sink
            .send(MarketUpdate::Health(self.id.clone(), StreamHealth::Live))
            .await;
        // A cooperative reconnect-style loop that stops on the supervisor's child
        // token — exactly the Deribit adapter shape, so the ordered join can await
        // it (ADR-0009).
        let cancel = req.cancel;
        let loop_cancel = cancel.clone();
        let join = tokio::spawn(async move {
            loop_cancel.cancelled().await;
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

// =============================================================================
// 2. fetch_chain returns the NAMED ChainFetch; a forced reconnect RESUBSCRIBES
//    off the fresh ChainFetch.aliases (no bare OptionChain, no re-derivation).
// =============================================================================

#[tokio::test]
async fn test_faux_reconnect_resubscribes_off_fresh_chainfetch_aliases() {
    let faux = FauxProvider::chainful(pid("faux"));
    let log = faux.subscribe_log();
    let exp = ExpirationDate::DateTime(expiry_utc());
    let (mut bridge, senders) = EventBridge::new(64);

    // (a) POLL leg: fetch_chain returns the NAMED ChainFetch artifact.
    let fetch1 = match faux.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("faux fetch_chain must succeed, got {e}"),
    };
    let aliases1 = sorted_aliases(&fetch1);
    assert!(
        !aliases1.is_empty(),
        "the fetch artifact carries the per-leg alias catalog to (re)subscribe off"
    );

    // (b) SUBSCRIBE off the returned ChainFetch.aliases (never a re-derivation).
    let legs1: Vec<Instrument> = fetch1.aliases.instruments().cloned().collect();
    let cancel1 = CancellationToken::new();
    let req1 = SubscriptionRequest::new("BTC", expiry_utc(), legs1, cancel1.clone());
    let handle1 = match faux.subscribe(req1, senders.market_update_sink()).await {
        Ok(handle) => handle,
        Err(e) => panic!("faux subscribe must succeed, got {e}"),
    };
    // Force the reconnect: drop the handle (stops the loop, exactly as an outage
    // would trigger the adapter-owned reconnect).
    drop(handle1);

    // (c) RECONNECT BACKFILL: re-fetch -> FRESH aliases (distinct from fetch1),
    // then resubscribe off THOSE.
    let fetch2 = match faux.fetch_chain("BTC", &exp).await {
        Ok(fetch) => fetch,
        Err(e) => panic!("faux re-fetch on reconnect must succeed, got {e}"),
    };
    let aliases2 = sorted_aliases(&fetch2);
    assert_ne!(
        aliases1, aliases2,
        "a reconnect re-fetch yields FRESH aliases, not the stale set (§5 backfill = current state)"
    );
    let legs2: Vec<Instrument> = fetch2.aliases.instruments().cloned().collect();
    let cancel2 = CancellationToken::new();
    let req2 = SubscriptionRequest::new("BTC", expiry_utc(), legs2, cancel2.clone());
    let handle2 = match faux.subscribe(req2, senders.market_update_sink()).await {
        Ok(handle) => handle,
        Err(e) => panic!("faux resubscribe must succeed, got {e}"),
    };
    drop(handle2);

    // (d) The recorded subscriptions prove the load-bearing contract: subscribe #1
    // used fetch #1's aliases EXACTLY, and the RESUBSCRIBE used the FRESH fetch #2
    // aliases — never a bare OptionChain, never a symbol re-derivation.
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
    assert_eq!(
        second, aliases2,
        "the RESUBSCRIBE used the FRESH fetch #2 ChainFetch.aliases, not the stale set"
    );

    // The port carried a control-class Health(Live) to the render side each time.
    let mut healths = 0usize;
    bridge.pump_into(
        |update| {
            if matches!(update, MarketUpdate::Health(_, StreamHealth::Live)) {
                healths = healths.checked_add(1).unwrap_or(healths);
            }
        },
        |_command| {},
    );
    assert!(
        healths >= 1,
        "a control-class Health flowed through the port to the render bridge"
    );
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
    match extra {
        Ok(None) => {}
        other => panic!("expected the faux task registered under the supervisor, got {other:?}"),
    }

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
        prop_assert!(result.is_err(), "a reserved id used externally is refused");
    }
}
