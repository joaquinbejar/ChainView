//! The two-class bounded, coalescing provider -> app bridge
//! (`docs/02-tui-architecture.md` §5, `docs/06-performance.md` §3.2).
//!
//! This is the seam that joins the **async** data layer (provider tasks on
//! `tokio`) to the **synchronous** render loop's fan-in ([`App::on_event`]).
//! Provider updates arrive over **bounded** `tokio::sync::mpsc` channels only —
//! an unbounded channel is forbidden because it hides backpressure until the OOM
//! killer reveals it (§5). Updates fall into two classes and the policy is
//! **coalesce the high-frequency class, always-enqueue the control class**, so a
//! burst of quotes can never starve a health or structure event:
//!
//! - **Coalesced class — [`Quote`](MarketUpdate::Quote) /
//!   [`Greeks`](MarketUpdate::Greeks) / [`Depth`](MarketUpdate::Depth),
//!   last-value-wins.** They ride the bounded coalesced channel (capacity from
//!   `config.channel_capacity`). On the consumer side the [`EventBridge`] drains
//!   that channel into a per-instrument [`StagingMap`] keyed by [`InstrumentKey`]
//!   — **one slot per instrument** — then flushes the current values into the
//!   app. The map is bounded by the subscribed instrument count N, never by burst
//!   rate or session length: a second update for an instrument before the flush
//!   overwrites the staged value (a dropped intermediate quote is not a
//!   correctness loss — the chain shows the freshest price). Within a slot a
//!   quote, a Greeks row, and a depth ladder are kept independently
//!   ([`StagedInstrument`]) so a Greeks refresh never clobbers a pending quote —
//!   the slot count stays exactly N.
//! - **Control class — [`Chain`](MarketUpdate::Chain) /
//!   [`Health`](MarketUpdate::Health).** Low-frequency and **never coalesced or
//!   dropped**: they ride a separate, small bounded control channel the fan-in
//!   drains **first** each wakeup (priority), so a `Health(Reconnecting)` or a
//!   fresh `Chain` is delivered promptly even while the coalesced channel is
//!   saturated with stale quote traffic.
//!
//! # Allocation discipline (HP-3, `docs/06-performance.md` §3.2)
//!
//! The staging map **reuses** its allocation across bursts. A flush
//! ([`StagingMap::drain_into`]) empties the map with [`HashMap::drain`], which
//! retains the bucket allocation, and an [`Unsubscribe`](Command::Unsubscribe)
//! prune uses [`HashMap::retain`], which also retains it — so once the map has
//! grown to fit the N subscribed instruments it performs **zero steady-state
//! per-burst allocation**. A repeat update for an already-staged instrument (the
//! hot burst case) allocates nothing: a key is cloned only on the first insert
//! for that instrument.
//!
//! # Lifecycle
//!
//! A staging slot is created on the first update for an instrument, overwritten
//! on each subsequent update, and **removed** when the instrument is
//! unsubscribed (a [`Command::Unsubscribe`] drained from the render -> data
//! command channel), so the map's bound tracks the live subscription set. On
//! shutdown the map is dropped with the bridge.
//!
//! # This issue's scope (#10) and the seams left for later
//!
//! This lands the bridge, the staging map, and the priority drain. The provider
//! adapter that *produces* updates (#16), the task supervisor that owns the
//! channel ends and the reconnect loop (#11), and the render loop that calls
//! [`EventBridge::pump`] between frames (#13) are separate issues. The seams are
//! explicit: [`EventBridge::new`] hands back the [`BridgeSenders`] (the producer
//! halves the supervisor wires to the adapters and to [`App`]), and
//! [`EventBridge::pump`] takes a command `route` closure the supervisor fills in
//! to forward each drained [`Command`] to the provider layer.

use std::collections::HashMap;

use optionstratlib::ExpirationDate;
use tokio::sync::mpsc;

use super::App;
use crate::chain::{DepthLadder, GreeksRow, InstrumentKey, MarketUpdate, QuoteUpdate};
use crate::event::{AppEvent, Command};

/// Capacity of the small bounded **control** channel that carries
/// [`Chain`](MarketUpdate::Chain) / [`Health`](MarketUpdate::Health)
/// (`docs/02-tui-architecture.md` §5). Small because the control class is
/// low-frequency and never coalesced; the fan-in drains it fully before the
/// coalesced channel each wakeup, so it never backs up behind a quote burst.
pub const CONTROL_CHANNEL_CAPACITY: usize = 64;

/// Capacity of the small bounded **command** channel (render -> data) that
/// carries [`Command`] (`docs/02-tui-architecture.md` §5). Small because a
/// command storm is impossible — commands are user-driven (a keypress, a scrub),
/// never machine-generated at tick rate.
pub const COMMAND_CHANNEL_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// The per-instrument coalescing staging map (the consumer-side conflater).
// ---------------------------------------------------------------------------

/// One staged slot for one instrument: the latest of each coalesced update kind
/// (`docs/02-tui-architecture.md` §5).
///
/// A quote, a Greeks row, and a depth ladder touch **different** chain fields, so
/// they are held independently — last-value-wins **per kind** — and a Greeks
/// refresh never clobbers a pending quote. There is still exactly **one slot per
/// instrument** in the map, so the map stays O(N) in the subscribed count.
#[derive(Debug, Default)]
struct StagedInstrument {
    /// The freshest staged quote for this instrument, if any.
    quote: Option<QuoteUpdate>,
    /// The freshest staged Greeks row for this instrument, if any.
    greeks: Option<GreeksRow>,
    /// The freshest staged depth ladder for this instrument, if any.
    depth: Option<DepthLadder>,
}

/// The consumer-side per-instrument staging map: the conflater that collapses a
/// burst of coalesced-class updates to one slot per [`InstrumentKey`]
/// (`docs/02-tui-architecture.md` §5, `docs/06-performance.md` §3.2).
///
/// Bounded by the subscribed instrument count N — never by burst rate or session
/// length — and reuses its allocation across bursts (HP-3).
#[derive(Debug, Default)]
struct StagingMap {
    /// One slot per instrument, keyed by the provider-agnostic
    /// [`InstrumentKey`]. Reused across bursts: `drain`/`retain` keep the bucket
    /// allocation.
    slots: HashMap<InstrumentKey, StagedInstrument>,
}

impl StagingMap {
    /// An empty staging map. It grows once to fit the N subscribed instruments,
    /// then reuses that allocation.
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// Coalesce one **high-frequency** update into its instrument's slot,
    /// last-value-wins per kind. A control-class update
    /// ([`Chain`](MarketUpdate::Chain) / [`Health`](MarketUpdate::Health)) does
    /// not belong in the staging map — it is returned unchanged as `Some(_)` so
    /// the caller folds it directly (never dropped), keeping this match total
    /// over the closed [`MarketUpdate`] set with no wildcard arm.
    fn stage(&mut self, update: MarketUpdate) -> Option<MarketUpdate> {
        match update {
            MarketUpdate::Quote(quote) => {
                if let Some(slot) = self.slot_mut(&quote.instrument.key) {
                    slot.quote = Some(quote);
                }
                None
            }
            MarketUpdate::Greeks(greeks) => {
                if let Some(slot) = self.slot_mut(&greeks.instrument.key) {
                    slot.greeks = Some(greeks);
                }
                None
            }
            MarketUpdate::Depth(depth) => {
                if let Some(slot) = self.slot_mut(&depth.instrument.key) {
                    slot.depth = Some(depth);
                }
                None
            }
            control @ (MarketUpdate::Chain(_) | MarketUpdate::Health(_, _)) => Some(control),
        }
    }

    /// A mutable reference to `key`'s slot, creating it on first use.
    ///
    /// The key is cloned **only** when the slot is vacant, so a repeat update for
    /// an already-staged instrument (the hot burst case) allocates nothing — the
    /// HP-3 zero-steady-state-allocation discipline. Returns `None` only if the
    /// just-ensured entry somehow could not be re-borrowed, which the caller
    /// treats as a no-op rather than an `expect` (the lint policy forbids
    /// `expect`).
    fn slot_mut(&mut self, key: &InstrumentKey) -> Option<&mut StagedInstrument> {
        if !self.slots.contains_key(key) {
            let _ = self.slots.insert(key.clone(), StagedInstrument::default());
        }
        self.slots.get_mut(key)
    }

    /// Remove every slot belonging to an unsubscribed `(underlying, expiration)`
    /// chain, so the map's bound tracks the live subscription set. `retain` keeps
    /// the bucket allocation for reuse (HP-3).
    fn remove_subscription(&mut self, underlying: &str, expiration: &ExpirationDate) {
        self.slots
            .retain(|key, _| !key_in_subscription(key, underlying, expiration));
    }

    /// Flush the current staged values into `sink`, emptying the map but
    /// **retaining** its allocation ([`HashMap::drain`]). Each slot emits its
    /// present quote, then Greeks, then depth (a deterministic per-slot order;
    /// the three touch different fields so their order is immaterial to the
    /// store fold).
    fn drain_into<F: FnMut(MarketUpdate)>(&mut self, sink: &mut F) {
        for (_key, staged) in self.slots.drain() {
            let StagedInstrument {
                quote,
                greeks,
                depth,
            } = staged;
            if let Some(quote) = quote {
                sink(MarketUpdate::Quote(quote));
            }
            if let Some(greeks) = greeks {
                sink(MarketUpdate::Greeks(greeks));
            }
            if let Some(depth) = depth {
                sink(MarketUpdate::Depth(depth));
            }
        }
    }

    /// The number of staged instruments (slots) — bounded by N.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.len()
    }

    /// The allocated slot capacity, for asserting the reuse discipline (HP-3).
    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.slots.capacity()
    }
}

/// Whether `key` belongs to the unsubscribed `(underlying, expiration)` chain.
///
/// A relative [`ExpirationDate::Days`] cannot be compared to a key's absolute
/// `expiration_utc` without a reference clock, which the synchronous fan-in
/// deliberately lacks (it reads no wall clock), so a `Days`-scoped unsubscribe
/// prunes **every** staged expiry for the underlying. An absolute
/// [`ExpirationDate::DateTime`] prunes precisely the matching expiry — the common
/// case, since instrument keys always carry the absolute expiry resolved at the
/// adapter seam (#15).
fn key_in_subscription(key: &InstrumentKey, underlying: &str, expiration: &ExpirationDate) -> bool {
    if key.underlying != underlying {
        return false;
    }
    match expiration {
        ExpirationDate::DateTime(instant) => key.expiration_utc == *instant,
        ExpirationDate::Days(_) => true,
    }
}

// ---------------------------------------------------------------------------
// The producer-side sender handles.
// ---------------------------------------------------------------------------

/// The producer-side halves of the bridge's three bounded channels, handed back
/// by [`EventBridge::new`] (`docs/02-tui-architecture.md` §5).
///
/// A `Provider::subscribe` call takes exactly **one**
/// `mpsc::Sender<MarketUpdate>`, so an adapter cannot hold both halves: the
/// composition seam (#22, per ADR-0009) wires an adapter's `subscribe` sender to
/// **[`tx_coalesced`]**, and a control-class update (`Chain` / `Health`) that
/// consequently arrives on the coalesced channel is **folded directly** by the
/// bridge rather than dropped — the misrouted-control fallback in `StagingMap`
/// (`stage` returns a control update to the caller for a direct fold). The true
/// **two-sender priority routing** — a separate control channel drained first
/// each wakeup — is the consumer bridge's design that #22 reconciles; until then
/// [`tx_control`] carries only updates a control-aware producer (a future poll
/// task, or the seam wiring) puts on it. [`tx_command`] is the render -> data
/// half [`App`] holds (an event handler that needs I/O emits a [`Command`] on
/// it). All three are cheap to `Clone`, so a per-provider-task sender is a clone
/// of one shared half.
///
/// [`tx_control`]: BridgeSenders::tx_control
/// [`tx_coalesced`]: BridgeSenders::tx_coalesced
/// [`tx_command`]: BridgeSenders::tx_command
#[derive(Debug, Clone)]
pub struct BridgeSenders {
    /// The control channel sender (`Chain` / `Health`), drained first each
    /// wakeup — never coalesced.
    pub tx_control: mpsc::Sender<MarketUpdate>,
    /// The coalesced channel sender (`Quote` / `Greeks` / `Depth`),
    /// last-value-wins per instrument on the consumer side.
    pub tx_coalesced: mpsc::Sender<MarketUpdate>,
    /// The render -> data command channel sender that [`App`] holds.
    pub tx_command: mpsc::Sender<Command>,
}

// ---------------------------------------------------------------------------
// The fan-in bridge.
// ---------------------------------------------------------------------------

/// The two-class fan-in that drains the bounded provider channels and folds the
/// coalesced result into [`App::on_event`] (`docs/02-tui-architecture.md` §5).
///
/// Owns the consumer (receiver) halves plus a per-instrument staging map (the
/// consumer-side conflater). The render loop (#13) drives it with
/// [`pump`](EventBridge::pump) between frames; the drain uses `try_recv` only, so
/// it never `.await`s and never blocks — a slow provider can never freeze a
/// frame. Constructed via [`new`](EventBridge::new), which also returns the
/// [`BridgeSenders`] the data layer wires up.
#[derive(Debug)]
pub struct EventBridge {
    /// Control class (`Chain` / `Health`), drained first (priority).
    rx_control: mpsc::Receiver<MarketUpdate>,
    /// Coalesced class (`Quote` / `Greeks` / `Depth`), conflated into `staging`.
    rx_coalesced: mpsc::Receiver<MarketUpdate>,
    /// Render -> data commands; an `Unsubscribe` prunes `staging`.
    rx_command: mpsc::Receiver<Command>,
    /// The per-instrument conflater (bounded by N, allocation reused).
    staging: StagingMap,
}

impl EventBridge {
    /// Create the bridge and its three bounded channels, returning the receiver
    /// side (the bridge) and the [`BridgeSenders`] the data layer wires up.
    ///
    /// `coalesced_capacity` is the high-frequency channel's capacity, taken from
    /// `config.channel_capacity`; it is clamped to at least `1` because a
    /// `tokio::sync::mpsc` channel requires a positive buffer (the config range
    /// `[64, 65536]` already guarantees this, so the clamp is defensive). The
    /// control and command channels use the small fixed
    /// [`CONTROL_CHANNEL_CAPACITY`] / [`COMMAND_CHANNEL_CAPACITY`]. **No unbounded
    /// channel exists on the provider -> app path.**
    #[must_use = "the returned BridgeSenders must be wired to the data layer and App"]
    pub fn new(coalesced_capacity: usize) -> (Self, BridgeSenders) {
        let coalesced_capacity = coalesced_capacity.max(1);
        let (tx_control, rx_control) = mpsc::channel(CONTROL_CHANNEL_CAPACITY);
        let (tx_coalesced, rx_coalesced) = mpsc::channel(coalesced_capacity);
        let (tx_command, rx_command) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let bridge = Self {
            rx_control,
            rx_coalesced,
            rx_command,
            staging: StagingMap::new(),
        };
        let senders = BridgeSenders {
            tx_control,
            tx_coalesced,
            tx_command,
        };
        (bridge, senders)
    }

    /// The fan-in wakeup: drain every pending update and command, folding the
    /// coalesced result into `app` (`docs/02-tui-architecture.md` §5).
    ///
    /// Ergonomic wrapper over [`pump_into`](EventBridge::pump_into) that folds
    /// each drained update through [`App::on_event`]. `route` receives every
    /// drained [`Command`] so the data layer (#11) can act on it (reconnect,
    /// resubscribe, seek…); this bridge additionally prunes the staging map on a
    /// [`Command::Unsubscribe`]. Synchronous — it never `.await`s.
    pub fn pump<R: FnMut(Command)>(&mut self, app: &mut App, route: R) {
        self.pump_into(|update| app.on_event(AppEvent::Market(update)), route);
    }

    /// The fan-in wakeup, folding each coalesced update into an arbitrary `sink`
    /// (`docs/02-tui-architecture.md` §5). The order is the whole point of the
    /// two-class design:
    ///
    /// 1. **Control first (priority):** drain `Chain` / `Health` fully into
    ///    `sink`, so health can never sit behind a quote burst.
    /// 2. **Coalesce:** drain the high-frequency channel into the staging map
    ///    (one slot per instrument, last-value-wins). A control-class update
    ///    misrouted onto this channel is folded directly, never dropped.
    /// 3. **Commands:** drain the render -> data channel, pruning the staging map
    ///    on each [`Command::Unsubscribe`] (so this wakeup's staged updates for a
    ///    just-unsubscribed instrument are dropped) and forwarding every command
    ///    to `route`.
    /// 4. **Flush:** emit the staged current values into `sink`.
    ///
    /// The drain is `try_recv`-based and reads no wall clock, so it is fully
    /// deterministic and testable with a mock producer.
    pub fn pump_into<S, R>(&mut self, mut sink: S, mut route: R)
    where
        S: FnMut(MarketUpdate),
        R: FnMut(Command),
    {
        self.drain_control(&mut sink);
        self.coalesce(&mut sink);
        self.drain_commands(&mut route);
        self.flush(&mut sink);
    }

    /// Drain the control channel fully into `sink` (priority, never coalesced).
    fn drain_control<F: FnMut(MarketUpdate)>(&mut self, sink: &mut F) {
        while let Ok(update) = self.rx_control.try_recv() {
            sink(update);
        }
    }

    /// Drain the coalesced channel into the staging map. A misrouted control
    /// update is folded directly through `sink` (never dropped).
    fn coalesce<F: FnMut(MarketUpdate)>(&mut self, sink: &mut F) {
        while let Ok(update) = self.rx_coalesced.try_recv() {
            if let Some(control) = self.staging.stage(update) {
                sink(control);
            }
        }
    }

    /// Drain the command channel, pruning the staging map on an `Unsubscribe`
    /// and forwarding every command to `route`.
    fn drain_commands<R: FnMut(Command)>(&mut self, route: &mut R) {
        while let Ok(command) = self.rx_command.try_recv() {
            if let Command::Unsubscribe {
                underlying,
                expiration,
            } = &command
            {
                self.staging.remove_subscription(underlying, expiration);
            }
            route(command);
        }
    }

    /// Flush the staged current values into `sink`, retaining the map allocation.
    fn flush<F: FnMut(MarketUpdate)>(&mut self, sink: &mut F) {
        self.staging.drain_into(sink);
    }
}

#[cfg(test)]
impl EventBridge {
    /// The number of staged instruments — test-only view of the O(N) bound.
    fn staged_len(&self) -> usize {
        self.staging.len()
    }

    /// The staging map's allocated capacity — test-only view of the HP-3 reuse.
    fn staged_capacity(&self) -> usize {
        self.staging.capacity()
    }

    /// Drain the coalesced channel into the staging map **without** flushing, so
    /// a test can inspect the staged bound before it is emitted. A misrouted
    /// control update is dropped here (production uses `pump`/`pump_into`, which
    /// fold it); tests drive only coalesced-class updates through this helper.
    fn coalesce_pending(&mut self) {
        let mut discard = |_update: MarketUpdate| {};
        self.coalesce(&mut discard);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use optionstratlib::{ExpirationDate, OptionStyle};

    use super::{BridgeSenders, EventBridge, StagingMap};
    use crate::app::{App, LiveState, Mode, ReplayState, SourceBinding};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ContractSpecFingerprint, DepthLadder,
        ExerciseStyle, ExpirySource, GreeksOrigin, GreeksRow, Instrument, InstrumentKey,
        MarketUpdate, ProviderId, QuoteUpdate, SettlementStyle, StreamHealth,
    };
    use crate::config::ThemeChoice;
    use crate::event::Command;
    use crate::providers::{
        ChainCapability, ChainPollCapability, GreeksCapability, ProviderCapabilities,
    };

    const EXP: i64 = 1_700_000_000;
    const EXP2: i64 = 1_700_086_400;

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> DateTime<Utc> {
        match DateTime::<Utc>::from_timestamp(secs, 0) {
            Some(t) => t,
            None => panic!("invalid test timestamp: {secs}"),
        }
    }

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    fn spec() -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: 1,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn ikey(underlying: &str, exp: i64, strike: f64) -> InstrumentKey {
        InstrumentKey {
            underlying: underlying.to_owned(),
            expiration_utc: utc(exp),
            strike: pos(strike),
            style: OptionStyle::Call,
        }
    }

    fn instrument(underlying: &str, exp: i64, strike: f64) -> Instrument {
        Instrument {
            key: ikey(underlying, exp, strike),
            provider: pid("deribit"),
            native_symbol: format!("{underlying}-{strike}"),
            stream_symbol: None,
            spec: spec(),
        }
    }

    /// A `Quote` update for `(underlying, exp, strike)` carrying `bid` and a
    /// receipt clock, so a later update for the same key is distinguishable.
    fn quote(underlying: &str, exp: i64, strike: f64, bid: f64, received: i64) -> MarketUpdate {
        MarketUpdate::Quote(QuoteUpdate {
            instrument: instrument(underlying, exp, strike),
            bid: Some(pos(bid)),
            ask: Some(pos(bid + 0.2)),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: None,
            received_time: utc(received),
        })
    }

    /// A `Quote` on the default BTC/EXP chain.
    fn q(strike: f64, bid: f64, received: i64) -> MarketUpdate {
        quote("BTC", EXP, strike, bid, received)
    }

    fn greeks(strike: f64, iv: f64) -> MarketUpdate {
        MarketUpdate::Greeks(GreeksRow {
            instrument: instrument("BTC", EXP, strike),
            iv: Some(pos(iv)),
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(EXP),
        })
    }

    fn depth(strike: f64) -> MarketUpdate {
        MarketUpdate::Depth(DepthLadder {
            instrument: instrument("BTC", EXP, strike),
            bids: Vec::new(),
            asks: Vec::new(),
            event_time: None,
            received_time: utc(EXP),
            change_id: None,
        })
    }

    fn health() -> MarketUpdate {
        MarketUpdate::Health(pid("deribit"), StreamHealth::Reconnecting { attempt: 1 })
    }

    fn unsubscribe(underlying: &str, expiration: ExpirationDate) -> Command {
        Command::Unsubscribe {
            underlying: underlying.to_owned(),
            expiration,
        }
    }

    /// The `bid` of a `Quote` update, or `None` for any other variant.
    fn quote_bid(update: &MarketUpdate) -> Option<Positive> {
        match update {
            MarketUpdate::Quote(q) => q.bid,
            MarketUpdate::Greeks(_)
            | MarketUpdate::Depth(_)
            | MarketUpdate::Chain(_)
            | MarketUpdate::Health(_, _) => None,
        }
    }

    /// The strike of any per-instrument update, or `None` for a chain/health.
    fn update_strike(update: &MarketUpdate) -> Option<Positive> {
        match update {
            MarketUpdate::Quote(q) => Some(q.instrument.key.strike),
            MarketUpdate::Greeks(g) => Some(g.instrument.key.strike),
            MarketUpdate::Depth(d) => Some(d.instrument.key.strike),
            MarketUpdate::Chain(_) | MarketUpdate::Health(_, _) => None,
        }
    }

    fn is_health(update: &MarketUpdate) -> bool {
        matches!(update, MarketUpdate::Health(_, _))
    }

    /// Flush a staging map into a fresh vector.
    fn drain(map: &mut StagingMap) -> Vec<MarketUpdate> {
        let mut out = Vec::new();
        {
            let mut sink = |update| out.push(update);
            map.drain_into(&mut sink);
        }
        out
    }

    // --- A live app so the `pump` wrapper can be exercised end to end --------

    fn full_caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(true)
            .greeks(GreeksCapability::Provided)
            .chain_poll(ChainPollCapability::Poll {
                interval_hint_secs: 2,
            })
            .build()
    }

    fn row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn chain_with(strikes: &[f64]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for strike in strikes {
            let _ = chain.options.insert(row(*strike));
        }
        chain
    }

    fn store(strikes: &[f64]) -> ChainStore {
        ChainStore::seed(
            ChainFetch::new(
                chain_with(strikes),
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            std::time::Duration::from_secs(2),
            utc(EXP),
        )
    }

    /// A live app, the bridge, and the (still-alive) producer senders, so the
    /// `pump` wrapper folds real updates into a real [`App`]. The returned
    /// `senders` keeps every producer half alive for the test's duration, so no
    /// channel disconnects mid-pump.
    fn live_app_with_bridge() -> (App, EventBridge, BridgeSenders) {
        let (bridge, senders) = EventBridge::new(64);
        let live = LiveState::new(
            SourceBinding::new(pid("deribit"), full_caps(), StreamHealth::Live),
            store(&[60_000.0]),
        );
        let app = App::new(
            Mode::Live(live),
            ThemeChoice::Auto,
            senders.tx_command.clone(),
        );
        (app, bridge, senders)
    }

    // === StagingMap: overwrite / lossless-per-kind / removal =================

    #[test]
    fn test_staging_map_second_quote_same_key_overwrites_first() {
        let mut map = StagingMap::new();
        let _ = map.stage(q(100.0, 1.0, EXP));
        let _ = map.stage(q(100.0, 2.0, EXP + 1));
        assert_eq!(map.len(), 1, "one slot per instrument, overwrite in place");
        let out = drain(&mut map);
        assert_eq!(out.len(), 1, "the two quotes coalesced to one");
        assert_eq!(
            out.first().and_then(quote_bid),
            Some(pos(2.0)),
            "the second update for the same key wins"
        );
    }

    #[test]
    fn test_staging_map_quote_and_greeks_same_key_both_kept() {
        // A quote and a Greeks row for the same instrument touch different chain
        // fields, so they must not clobber each other — still one slot.
        let mut map = StagingMap::new();
        let _ = map.stage(q(100.0, 1.0, EXP));
        let _ = map.stage(greeks(100.0, 0.5));
        assert_eq!(map.len(), 1, "still one slot per instrument");
        let out = drain(&mut map);
        assert_eq!(out.len(), 2, "both the quote and the Greeks flush");
        assert!(out.iter().any(|u| matches!(u, MarketUpdate::Quote(_))));
        assert!(out.iter().any(|u| matches!(u, MarketUpdate::Greeks(_))));
    }

    #[test]
    fn test_staging_map_depth_and_quote_same_key_both_kept() {
        let mut map = StagingMap::new();
        let _ = map.stage(q(100.0, 1.0, EXP));
        let _ = map.stage(depth(100.0));
        assert_eq!(map.len(), 1);
        let out = drain(&mut map);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|u| matches!(u, MarketUpdate::Depth(_))));
    }

    #[test]
    fn test_staging_map_stage_returns_control_update_for_direct_fold() {
        // A control-class update never enters the staging map: it is handed back
        // so the caller folds it directly.
        let mut map = StagingMap::new();
        let returned = map.stage(health());
        assert!(matches!(returned, Some(MarketUpdate::Health(_, _))));
        assert_eq!(map.len(), 0, "control class is never staged");
    }

    #[test]
    fn test_staging_map_remove_subscription_by_datetime_prunes_matching_expiry() {
        // Same underlying, two expiries. An absolute-expiry unsubscribe prunes
        // only the matching one.
        let mut map = StagingMap::new();
        let _ = map.stage(quote("BTC", EXP, 100.0, 1.0, EXP));
        let _ = map.stage(quote("BTC", EXP2, 200.0, 1.0, EXP));
        assert_eq!(map.len(), 2);
        map.remove_subscription("BTC", &ExpirationDate::DateTime(utc(EXP)));
        assert_eq!(map.len(), 1, "only the EXP expiry is pruned");
        let out = drain(&mut map);
        assert_eq!(out.first().and_then(update_strike), Some(pos(200.0)));
    }

    #[test]
    fn test_staging_map_remove_subscription_days_prunes_whole_underlying() {
        // A relative-days unsubscribe cannot resolve an absolute expiry without a
        // clock, so it prunes every staged expiry for the underlying — but leaves
        // other underlyings untouched.
        let mut map = StagingMap::new();
        let _ = map.stage(quote("BTC", EXP, 100.0, 1.0, EXP));
        let _ = map.stage(quote("BTC", EXP2, 200.0, 1.0, EXP));
        let _ = map.stage(quote("ETH", EXP, 300.0, 1.0, EXP));
        assert_eq!(map.len(), 3);
        map.remove_subscription("BTC", &ExpirationDate::Days(pos(7.0)));
        assert_eq!(map.len(), 1, "both BTC expiries pruned, ETH kept");
        let out = drain(&mut map);
        assert_eq!(out.first().and_then(update_strike), Some(pos(300.0)));
    }

    #[test]
    fn test_staging_map_remove_subscription_other_underlying_is_noop() {
        let mut map = StagingMap::new();
        let _ = map.stage(q(100.0, 1.0, EXP));
        map.remove_subscription("ETH", &ExpirationDate::DateTime(utc(EXP)));
        assert_eq!(
            map.len(),
            1,
            "unsubscribing a different underlying keeps BTC"
        );
    }

    // === StagingMap: bounded memory, allocation reuse (HP-3) =================

    #[test]
    fn test_staging_map_drain_retains_capacity_across_bursts() {
        let mut map = StagingMap::new();
        // First burst grows the map to fit N=8 distinct instruments.
        for strike in 1..=8 {
            let _ = map.stage(q(f64::from(strike), 1.0, EXP));
        }
        let capacity_after_first = map.capacity();
        assert!(capacity_after_first >= 8);
        let flushed = drain(&mut map);
        assert_eq!(flushed.len(), 8);
        assert_eq!(map.len(), 0);
        assert_eq!(
            map.capacity(),
            capacity_after_first,
            "drain must retain the allocation (HP-3)"
        );
        // Many more bursts over the SAME instruments never grow the allocation
        // and never grow the slot count — memory is O(N), not O(burst).
        for round in 0..1_000 {
            for strike in 1..=8 {
                let _ = map.stage(q(f64::from(strike), f64::from(round % 5) + 1.0, EXP));
            }
            assert!(map.len() <= 8, "staging is O(N instruments), not O(burst)");
            let _ = drain(&mut map);
            assert_eq!(
                map.capacity(),
                capacity_after_first,
                "no per-burst reallocation on the hot path (HP-3)"
            );
        }
    }

    #[test]
    fn test_staging_map_latest_value_wins_over_a_burst() {
        // Hammer one instrument with many quotes; only the freshest survives.
        let mut map = StagingMap::new();
        for tick in 0..500 {
            let _ = map.stage(q(100.0, f64::from(tick) + 1.0, EXP + i64::from(tick)));
        }
        assert_eq!(map.len(), 1);
        let out = drain(&mut map);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out.first().and_then(quote_bid),
            Some(pos(500.0)),
            "the last-staged value is the one delivered"
        );
    }

    // === EventBridge: priority drain, lifecycle, backpressure ================

    #[test]
    fn test_event_bridge_drains_control_before_coalesced() {
        // Health on the control channel, a quote on the coalesced channel. The
        // control class is folded first regardless of enqueue order.
        let (mut bridge, senders) = EventBridge::new(64);
        let _ = senders.tx_coalesced.try_send(q(100.0, 1.0, EXP));
        let _ = senders.tx_control.try_send(health());
        let mut recorded: Vec<MarketUpdate> = Vec::new();
        bridge.pump_into(|u| recorded.push(u), |_c| {});
        assert!(
            recorded.first().is_some_and(is_health),
            "control (Health) must be folded before any coalesced quote"
        );
        assert!(
            recorded.iter().any(|u| matches!(u, MarketUpdate::Quote(_))),
            "the coalesced quote is still delivered, just after control"
        );
    }

    #[test]
    fn test_event_bridge_health_delivered_while_coalesced_saturated() {
        // Saturate the coalesced channel to its capacity with stale quote
        // traffic, then a Health on control. Health must still be delivered
        // promptly (first), and the quotes coalesce to O(N).
        //
        // Scope: this proves the two properties the consumer-side bridge owns —
        // control-first PRIORITY delivery and the O(N) coalescing bound. It does
        // NOT prove latest-value-wins under a *full* channel: with the current
        // plain `try_send`-drop producer, once the channel fills the newest
        // values are refused, so the delivered quote may be stale. Preserving the
        // freshest value under sustained saturation is the producer-side
        // overwrite-on-full staging pinned in #16 (docs/02 §5); this test is
        // deliberately silent on the delivered value's freshness for that reason.
        let (mut bridge, senders) = EventBridge::new(4);
        for tick in 0..64 {
            // Cycle three instruments so >capacity sends overflow (Full) yet the
            // channel never grows past 4.
            let strike = f64::from(tick % 3) + 100.0;
            let _ = senders.tx_coalesced.try_send(q(
                strike,
                f64::from(tick) + 1.0,
                EXP + i64::from(tick),
            ));
        }
        let _ = senders.tx_control.try_send(health());
        let mut recorded: Vec<MarketUpdate> = Vec::new();
        bridge.pump_into(|u| recorded.push(u), |_c| {});
        assert!(
            recorded.first().is_some_and(is_health),
            "Health is delivered promptly, not behind the quote burst"
        );
        let quote_count = recorded
            .iter()
            .filter(|u| matches!(u, MarketUpdate::Quote(_)))
            .count();
        assert!(
            quote_count <= 3,
            "quotes coalesced to at most N=3 instruments, got {quote_count}"
        );
    }

    #[test]
    fn test_event_bridge_burst_beyond_channel_capacity_keeps_memory_flat() {
        // A burst far beyond the channel capacity, drained in rounds. The staging
        // map stays O(N=3) and its allocation never grows unboundedly.
        let (mut bridge, senders) = EventBridge::new(4);
        let mut capacity_baseline: Option<usize> = None;
        for round in 0..500u32 {
            // Try to shove nine updates into a capacity-4 channel every round:
            // the excess is refused by `try_send` (overflow), never buffered
            // unboundedly.
            for tick in 0..9u32 {
                let strike = f64::from(tick % 3) + 100.0;
                let _ = senders.tx_coalesced.try_send(q(
                    strike,
                    f64::from(round) + 1.0,
                    EXP + i64::from(round),
                ));
            }
            bridge.coalesce_pending();
            assert!(
                bridge.staged_len() <= 3,
                "staging is O(N=3 instruments), not O(burst): round {round}"
            );
            let baseline = *capacity_baseline.get_or_insert_with(|| bridge.staged_capacity());
            assert_eq!(
                bridge.staged_capacity(),
                baseline,
                "the staging allocation never grows unboundedly: round {round}"
            );
            // Flush so the next round starts clean (allocation retained).
            let mut sink = |_u: MarketUpdate| {};
            bridge.flush(&mut sink);
        }
    }

    #[test]
    fn test_event_bridge_every_instrument_receives_latest_value() {
        // Several updates per instrument across three instruments — each
        // instrument's freshest quote is delivered exactly once (lossless for the
        // current value).
        let (mut bridge, senders) = EventBridge::new(64);
        for round in 1..=5 {
            for strike in [100.0, 200.0, 300.0] {
                let _ = senders.tx_coalesced.try_send(q(
                    strike,
                    f64::from(round),
                    EXP + i64::from(round),
                ));
            }
        }
        let mut recorded: Vec<MarketUpdate> = Vec::new();
        bridge.pump_into(|u| recorded.push(u), |_c| {});
        assert_eq!(recorded.len(), 3, "one current value per instrument");
        // Every delivered quote carries the last round's bid (5.0).
        for update in &recorded {
            assert_eq!(quote_bid(update), Some(pos(5.0)), "the freshest bid wins");
        }
        let mut strikes: Vec<Positive> = recorded.iter().filter_map(update_strike).collect();
        strikes.sort();
        assert_eq!(strikes, vec![pos(100.0), pos(200.0), pos(300.0)]);
    }

    #[test]
    fn test_event_bridge_unsubscribe_command_prunes_staged_instrument() {
        // Quotes for two instruments are staged this wakeup, and an Unsubscribe
        // for one arrives on the command channel: that instrument's staged update
        // is dropped (not folded), and the command is still routed downstream.
        let (mut bridge, senders) = EventBridge::new(64);
        let _ = senders
            .tx_coalesced
            .try_send(quote("BTC", EXP, 100.0, 1.0, EXP));
        let _ = senders
            .tx_coalesced
            .try_send(quote("BTC", EXP2, 200.0, 1.0, EXP));
        let _ = senders
            .tx_command
            .try_send(unsubscribe("BTC", ExpirationDate::DateTime(utc(EXP))));
        let mut recorded: Vec<MarketUpdate> = Vec::new();
        let mut routed: Vec<Command> = Vec::new();
        bridge.pump_into(|u| recorded.push(u), |c| routed.push(c));
        assert_eq!(recorded.len(), 1, "the unsubscribed instrument was pruned");
        assert_eq!(recorded.first().and_then(update_strike), Some(pos(200.0)));
        assert_eq!(
            routed.len(),
            1,
            "the command is still routed to the data layer"
        );
        assert!(matches!(routed.first(), Some(Command::Unsubscribe { .. })));
    }

    #[test]
    fn test_event_bridge_command_routed_to_router() {
        // A non-lifecycle command is forwarded verbatim to the route closure.
        let (mut bridge, senders) = EventBridge::new(64);
        let _ = senders.tx_command.try_send(Command::Reconnect);
        let mut routed: Vec<Command> = Vec::new();
        bridge.pump_into(|_u| {}, |c| routed.push(c));
        assert!(matches!(routed.first(), Some(Command::Reconnect)));
    }

    #[test]
    fn test_event_bridge_pump_folds_control_into_live_app() {
        // The ergonomic `pump` wrapper folds a Health update (control class) into
        // a real App through `App::on_event`: the source-side health degrades and
        // the app is marked dirty.
        let (mut app, mut bridge, senders) = live_app_with_bridge();
        app.mark_drawn();
        assert!(!app.dirty);
        let _ = senders.tx_control.try_send(health());
        // Route commands nowhere (no data layer in this issue's scope).
        bridge.pump(&mut app, |_c| {});
        assert!(app.dirty, "the folded Health marked the app dirty");
    }

    #[test]
    fn test_event_bridge_pump_on_replay_app_ignores_market_update() {
        // A market update folded into a replay app is a no-op (no live store),
        // proving `pump` routes through `App::on_event` faithfully.
        let (mut bridge, senders) = EventBridge::new(64);
        let mut app = App::new(
            Mode::Replay(ReplayState::new(PathBuf::from("/bundle"))),
            ThemeChoice::Auto,
            senders.tx_command.clone(),
        );
        app.mark_drawn();
        let _ = senders.tx_coalesced.try_send(q(100.0, 1.0, EXP));
        bridge.pump(&mut app, |_c| {});
        assert!(
            !app.dirty,
            "a live market update is meaningless in replay mode"
        );
    }
}
