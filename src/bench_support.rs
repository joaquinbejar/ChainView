//! Bench-only constructors for the three v0.1 hot-path benches (`benches/*`,
//! issue #21), compiled **only** under the `bench` Cargo feature.
//!
//! # Why this exists
//!
//! A `benches/*.rs` file is a **separate crate** that links `chainview` and sees
//! only its **public** API. The three hot paths this issue benchmarks are not on
//! that surface: the pure draw dispatch reaches the chain matrix through
//! `render` (public) but a *populated* [`App`] is assembled from `pub(crate)` and
//! test-only constructors; [`ChainStore`] merge, the [`EventBridge`] coalescer,
//! and the Deribit `ticker.`/`book.` normalize seam are `pub(crate)`. This module
//! exposes exactly the constructors the benches need — a populated render
//! [`App`], a seeded [`ChainStore`] via the merge harness, a scripted
//! [`MarketUpdate`] burst, and the Deribit payload → coalescing-merge path — over
//! the crate's own public types.
//!
//! # It is an INTERNAL, UNSTABLE harness — not a SemVer surface
//!
//! **This module carries NO SemVer guarantee.** It is an internal benchmarking
//! harness owned by ChainView's `bench` feature; its types, signatures, and very
//! existence may change or be removed in **any** release — including a patch —
//! **without notice**. Do NOT depend on it from a downstream crate. The whole
//! module is `#[cfg(feature = "bench")]`, OFF by default: a normal build never
//! compiles it, and enabling `--features bench` is explicitly documented as
//! opting into an unstable, internal-benchmarking-only surface that is EXCLUDED
//! from the semver-governed public API (`docs/SEMVER.md`, `docs/06-performance.md`
//! §4). Every value it produces is a fixture/synthetic — no live venue, no
//! socket, no wall clock — so the benches are deterministic and re-runnable
//! (`docs/TESTING.md` §11).

use std::time::Duration;

use chrono::{DateTime, Utc};
use optionstratlib::OptionStyle;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use tokio::sync::mpsc;

use crate::app::{
    App, BridgeSenders, EventBridge, LiveScreen, LiveState, Mode, ScreenLoad, SourceBinding,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSource, ChainStore, ContractSpecFingerprint, ExerciseStyle,
    ExpirySource, Instrument, InstrumentKey, MarketUpdate, MergeOutcome, ProviderId,
    SettlementStyle, StreamHealth,
};
use crate::config::ThemeChoice;
use crate::event::Command;
use crate::providers::deribit::{BenchProducerStaging, bench_stream_burst, deribit_capabilities};

/// The absolute-UTC expiry every synthetic instrument and chain shares —
/// `2025-06-27T08:00:00Z`, the Deribit fixture expiry.
const EXPIRY_MS: i64 = 1_751_011_200_000;

/// The synthetic strike ladder base + step (arbitrary, positive, evenly spaced).
const STRIKE_BASE: f64 = 10_000.0;
const STRIKE_STEP: f64 = 250.0;

/// The synthetic underlying every bench chain uses.
const UNDERLYING: &str = "BTC";

// ---------------------------------------------------------------------------
// Small infallible-for-controlled-input constructors (no unwrap/expect).
// ---------------------------------------------------------------------------

/// A [`Positive`] from a controlled bench value; a non-positive/`NaN` input (a
/// bench bug) degrades to [`Positive::ZERO`] rather than panicking.
fn pos(value: f64) -> Positive {
    match Positive::new(value) {
        Ok(p) => p,
        Err(_) => Positive::ZERO,
    }
}

/// The reserved Deribit [`ProviderId`]; the literal satisfies the grammar, so the
/// error arm is genuinely unreachable — the documented infallible-for-this-literal
/// pattern the adapter itself uses (no `unwrap`/`expect`).
fn deribit_pid() -> ProviderId {
    match ProviderId::new("deribit") {
        Ok(id) => id,
        Err(_) => unreachable!("`deribit` is a valid, reserved provider id literal"),
    }
}

/// The shared absolute-UTC expiry instant.
fn expiry_utc() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(EXPIRY_MS).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// A deterministic base wall-clock instant used as the `received_time`/poll
/// stamp; fixed so the benches read no clock and stay reproducible.
fn base_time() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The `i`-th synthetic strike price.
fn strike_at(index: usize) -> f64 {
    let offset = u32::try_from(index).map(f64::from).unwrap_or(0.0);
    STRIKE_BASE + offset * STRIKE_STEP
}

/// The Deribit economic-equivalence fingerprint the synthetic legs share
/// (cash-settled, European, `USD`-quoted) — the shape the real adapter emits.
fn synthetic_spec() -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: 1,
        settlement: SettlementStyle::Cash,
        exercise: ExerciseStyle::European,
        quote_currency: "USD".to_owned(),
        venue_product_code: UNDERLYING.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Synthetic chain + subscribed legs.
// ---------------------------------------------------------------------------

/// Build a populated `strikes`-strike [`OptionChain`] with every call/put field
/// present (bid/ask/IV/delta/gamma/volume/OI), so the chain matrix renders its
/// fullest form and the merge does real per-row work.
fn synthetic_chain(strikes: usize) -> OptionChain {
    let spot = pos(STRIKE_BASE + (strike_span(strikes) / 2.0));
    let mut chain = OptionChain::new(UNDERLYING, spot, expiry_utc().to_rfc3339(), None, None);
    for index in 0..strikes {
        chain.add_option(
            pos(strike_at(index)),
            Some(pos(1.05)),            // call_bid
            Some(pos(1.25)),            // call_ask
            Some(pos(2.05)),            // put_bid
            Some(pos(2.35)),            // put_ask
            pos(0.4922),                // implied_volatility (non-Option)
            Some(Decimal::new(55, 2)),  // delta_call  0.55
            Some(Decimal::new(-45, 2)), // delta_put  -0.45
            Some(Decimal::new(1, 2)),   // gamma  0.01
            Some(pos(12.0)),            // volume
            Some(100),                  // open_interest
            None,                       // extra_fields
        );
    }
    chain
}

/// The full strike span (last minus first) for centering the synthetic spot.
fn strike_span(strikes: usize) -> f64 {
    match strikes.checked_sub(1) {
        Some(last) => strike_at(last) - STRIKE_BASE,
        None => 0.0,
    }
}

/// The `2·strikes` subscribed legs (a call and a put per strike) plus their alias
/// catalog, all under the Deribit [`ProviderId`] with keys matching
/// [`synthetic_chain`]. These are the `N` instruments the NFR-15 staging bound is
/// measured against.
fn synthetic_legs(strikes: usize) -> (Vec<Instrument>, AliasCatalog) {
    let provider = deribit_pid();
    let mut legs = Vec::new();
    let mut aliases = AliasCatalog::new();
    for index in 0..strikes {
        for style in [OptionStyle::Call, OptionStyle::Put] {
            let instrument = Instrument {
                key: InstrumentKey {
                    underlying: UNDERLYING.to_owned(),
                    expiration_utc: expiry_utc(),
                    strike: pos(strike_at(index)),
                    style,
                },
                provider: provider.clone(),
                native_symbol: format!("{UNDERLYING}-{index}-{}", style.as_str()),
                stream_symbol: None,
                spec: synthetic_spec(),
            };
            aliases.insert(instrument.clone());
            legs.push(instrument);
        }
    }
    (legs, aliases)
}

/// Seed a [`ChainStore`] from an `strikes`-strike synthetic Deribit chain — the
/// HP-3 merge target. Public so a bench can also seed a store directly.
#[must_use]
pub fn seeded_store(strikes: usize) -> ChainStore {
    let (_legs, aliases) = synthetic_legs(strikes);
    ChainStore::seed(
        ChainFetch::new(
            synthetic_chain(strikes),
            ExpirySource::new(UNDERLYING, expiry_utc(), deribit_pid()),
            aliases,
        ),
        ChainSource::Merged,
        Duration::from_secs(2),
        base_time(),
    )
}

// ---------------------------------------------------------------------------
// HP-1 / HP-2: a populated live App on the ready chain matrix.
// ---------------------------------------------------------------------------

/// A live [`App`] on the [`LiveScreen::Chain`] screen in [`ScreenLoad::Ready`],
/// bound to a Deribit source and seeded with an `strikes`-strike populated chain
/// — the fullest reachable render screen (HP-1) and the fan-in target (HP-2). The
/// command channel's receiver is dropped: no bench folds a `Command`-emitting
/// event, so the render/fan-in paths never depend on delivery.
#[must_use]
pub fn live_ready_app(strikes: usize) -> App {
    let (tx, _rx) = mpsc::channel::<Command>(64);
    let mut live = LiveState::new(
        SourceBinding::new(deribit_pid(), deribit_capabilities(), StreamHealth::Live),
        seeded_store(strikes),
    );
    live.screen = LiveScreen::Chain;
    live.load = ScreenLoad::Ready;
    let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
    app.mark_drawn();
    app
}

/// A scripted [`MarketUpdate`] burst for the fan-in (HP-2): a fresh, non-crossed
/// `ticker.`+`book.` refresh for every one of the `2·strikes` legs, normalized
/// through the **real** Deribit seam. `round` perturbs the values so successive
/// bursts fold real last-value-wins work rather than re-applying an identical
/// value. Deterministic and owned (no clone in the caller's timed loop).
#[must_use]
pub fn market_burst(strikes: usize, round: u64) -> Vec<MarketUpdate> {
    let (legs, _aliases) = synthetic_legs(strikes);
    bench_stream_burst(&legs, round, base_time())
}

// ---------------------------------------------------------------------------
// HP-3: the Deribit payload → coalescing-merge harness (with the NFR-15 probe).
// ---------------------------------------------------------------------------

/// Fold one coalesced-class update into the store, returning whether it applied
/// (real merge work). Mirrors [`crate::LiveState`]'s market fold: quotes and
/// Greeks patch a row; depth has no store path yet; control-class updates are not
/// staged here.
fn fold_into_store(store: &mut ChainStore, update: MarketUpdate) -> bool {
    match update {
        MarketUpdate::Quote(quote) => matches!(store.apply_quote(&quote), MergeOutcome::Applied),
        MarketUpdate::Greeks(greeks) => {
            matches!(store.apply_greeks(&greeks), MergeOutcome::Applied)
        }
        MarketUpdate::Depth(_) | MarketUpdate::Chain(_) | MarketUpdate::Health(_, _) => false,
    }
}

/// The HP-3 harness: the busiest provider path end to end — a Deribit
/// `ticker.`+`book.` burst normalized through the real adapter seam, published on
/// the **bounded** [`EventBridge`] coalesced channel, drained through the
/// per-instrument staging map, and merged into a [`ChainStore`].
///
/// Owns the store, the bridge, the live producer senders (kept alive so the
/// bounded channels never close mid-run), the subscribed legs, and the
/// deterministic `received` stamp. Exposes both the timed merge unit
/// ([`run_burst`](Self::run_burst)) and the NFR-15 bounded-memory probe
/// ([`staging_bound`](Self::staging_bound)) — the staging bound is *measured*
/// (slots ≤ `N`, capacity flat), not asserted from the design.
pub struct ChainMergeHarness {
    store: ChainStore,
    bridge: EventBridge,
    senders: BridgeSenders,
    producer: BenchProducerStaging,
    legs: Vec<Instrument>,
    received: DateTime<Utc>,
}

impl ChainMergeHarness {
    /// Build the harness over an `strikes`-strike synthetic Deribit chain
    /// (`2·strikes` subscribed legs), with the coalesced channel bounded at
    /// `channel_capacity`. A capacity below the burst size forces the coalescer to
    /// exercise its overflow path.
    #[must_use]
    pub fn new(strikes: usize, channel_capacity: usize) -> Self {
        let (legs, _aliases) = synthetic_legs(strikes);
        let store = seeded_store(strikes);
        let (bridge, senders) = EventBridge::new(channel_capacity);
        Self {
            store,
            bridge,
            senders,
            producer: BenchProducerStaging::new(),
            legs,
            received: base_time(),
        }
    }

    /// The subscribed-leg count `N` — the bound the staging map must stay within.
    #[must_use]
    pub fn legs(&self) -> usize {
        self.legs.len()
    }

    /// Run one burst: normalize a fresh `ticker.`+`book.` burst for every leg
    /// through the Deribit seam, publish it on the bounded coalesced channel, and
    /// pump the coalesced result into the store. Returns how many updates the
    /// store applied. This is the timed HP-3 unit (payload → coalescing merge).
    pub fn run_burst(&mut self, round: u64) -> usize {
        self.publish(round);
        let store = &mut self.store;
        let mut applied: usize = 0;
        self.bridge.pump_into(
            |update| {
                if fold_into_store(store, update) {
                    applied = applied.checked_add(1).unwrap_or(applied);
                }
            },
            |_command| {},
        );
        applied
    }

    /// Publish one burst and coalesce it **without** flushing, returning the
    /// staging bound `(slots, capacity)` — the NFR-15 O(N) demonstration: `slots`
    /// stays ≤ `N` and `capacity` stays flat across bursts regardless of how many
    /// updates were pushed. Flushes into the store afterward so the next probe
    /// starts clean with the allocation retained.
    pub fn staging_bound(&mut self, round: u64) -> (usize, usize) {
        self.publish(round);
        self.bridge.coalesce_pending();
        let bound = (self.bridge.staged_len(), self.bridge.staged_capacity());
        let store = &mut self.store;
        self.bridge.pump_into(
            |update| {
                let _ = fold_into_store(store, update);
            },
            |_command| {},
        );
        bound
    }

    /// The store's bounded pending-buffer occupancy (≤ `MAX_PENDING`) — a second
    /// NFR-15 bound the merge cannot grow with tick volume.
    #[must_use]
    pub fn store_pending(&self) -> usize {
        self.store.pending_len()
    }

    /// Normalize and publish one burst onto the bounded coalesced channel through
    /// the **producer-side overwrite-on-full conflater** (the NFR-15
    /// [`crate::providers::deribit`] `ProducerStaging`,
    /// `docs/02-tui-architecture.md` §5): a full channel STAGES the freshest value
    /// per instrument (last-value-wins per kind) rather than dropping it. At the
    /// documented default capacity — above the burst size — nothing ever stages,
    /// so this delivers the identical stream the HP-3 baseline measured; below the
    /// burst size (exercised by the saturation test) the producer genuinely
    /// overflows and the newest survives.
    fn publish(&mut self, round: u64) {
        let _ = self.producer.publish_burst(
            &self.senders.tx_coalesced,
            &self.legs,
            round,
            self.received,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use optionstratlib::prelude::Positive;
    use tokio::sync::mpsc;

    use super::{base_time, pos, synthetic_legs};
    use crate::chain::{InstrumentKey, MarketUpdate};
    use crate::providers::deribit::BenchProducerStaging;

    /// The `base` quote/depth value `bench_stream_burst` assigns to `round` — a
    /// mirror of the adapter's formula so the test can name the exact freshest
    /// value the newest burst carries.
    fn round_base(round: u64) -> f64 {
        let step = u32::try_from(round % 16).unwrap_or(0);
        1.0 + f64::from(step) * 0.05
    }

    /// NFR-15 producer overwrite-on-full under genuine saturation
    /// (`docs/02-tui-architecture.md` §5): with the bounded channel capped FAR
    /// below one burst, publishing an `old` then a `new` burst without draining
    /// between must (a) never permanently drop an instrument, and (b) let the
    /// freshest (`new`) quote and depth WIN the overwrite (last-value-wins). This
    /// is the property the HP-3 harness's default (non-saturating) capacity never
    /// exercises, proved here rather than assumed from the design.
    #[test]
    fn producer_overwrite_on_full_keeps_newest_under_saturation() {
        const STRIKES: usize = 8; // 16 legs -> a 48-update burst
        const CAPACITY: usize = 4; // FAR below the burst: the channel truly saturates
        let (legs, _aliases) = synthetic_legs(STRIKES);
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(CAPACITY);
        let mut producer = BenchProducerStaging::new();
        let received = base_time();
        let old = 3_u64; // round_base(3) = 1.15
        let new = 9_u64; // round_base(9) = 1.45 — distinct from `old`

        // Two bursts with the consumer NOT draining between: the channel saturates
        // and the producer stages the residue, the `new` burst overwriting `old`.
        assert!(producer.publish_burst(&tx, &legs, old, received));
        assert!(producer.publish_burst(&tx, &legs, new, received));

        // Drain to quiescence, recording the LAST value seen per instrument/kind:
        // the residue is retried onto the channel as it drains (the streaming
        // loop's flush tick), so every staged value eventually reaches the consumer.
        let mut quote_bid: HashMap<InstrumentKey, Positive> = HashMap::new();
        let mut depth_bid: HashMap<InstrumentKey, Positive> = HashMap::new();
        let mut greeks_seen: HashSet<InstrumentKey> = HashSet::new();
        loop {
            assert!(producer.flush(&tx), "the consumer receiver stays alive");
            let mut drained = false;
            while let Ok(update) = rx.try_recv() {
                drained = true;
                match update {
                    MarketUpdate::Quote(quote) => {
                        if let Some(bid) = quote.bid {
                            let _ = quote_bid.insert(quote.instrument.key, bid);
                        }
                    }
                    MarketUpdate::Depth(depth) => {
                        if let Some(best) = depth.bids.first().map(|level| level.price) {
                            let _ = depth_bid.insert(depth.instrument.key, best);
                        }
                    }
                    MarketUpdate::Greeks(greeks) => {
                        let _ = greeks_seen.insert(greeks.instrument.key);
                    }
                    MarketUpdate::Chain(_) | MarketUpdate::Health(_, _) => {}
                }
            }
            if !producer.has_pending() && !drained {
                break;
            }
        }

        // (a) Nothing was permanently dropped: every subscribed leg survived.
        assert_eq!(quote_bid.len(), legs.len(), "every leg's quote survived");
        assert_eq!(depth_bid.len(), legs.len(), "every leg's depth survived");
        assert_eq!(greeks_seen.len(), legs.len(), "every leg's Greeks survived");

        // (b) The newest value won the overwrite (last-value-wins), never the stale.
        let newest = pos(round_base(new));
        let stale = pos(round_base(old));
        assert_ne!(newest, stale, "the two rounds carry distinct values");
        for bid in quote_bid.values() {
            assert_eq!(*bid, newest, "the freshest quote won under saturation");
        }
        for bid in depth_bid.values() {
            assert_eq!(*bid, newest, "the freshest depth won under saturation");
        }
    }
}
