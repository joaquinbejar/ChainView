//! The live chain store: the deterministic poll -> stream merge over the
//! `optionstratlib` chain model (`docs/01-domain-model.md` §6,
//! `docs/03-data-providers.md` §4).
//!
//! [`ChainStore`] owns one normalized [`OptionChain`] for one `(provider,
//! underlying, expiry)` and keeps it streaming-current. It folds a
//! [`QuoteUpdate`] / [`GreeksRow`] into the matching row, maintains a
//! per-instrument freshness watermark and per-component staleness, keeps a
//! retained/decayed price-direction baseline, and reconciles structure across
//! polls with **bounded generations**, tombstones, and a capped
//! pending-unknown-strike buffer — so a de-listed strike is never resurrected,
//! and memory stays bounded regardless of how polls and stream updates
//! interleave: the pending buffer is hard-capped at [`MAX_PENDING`], and the
//! tombstone / instrument / overlay-refused sets are bounded by the strike
//! universe of one `(provider, underlying, expiry)` chain (a finite ladder),
//! not by the number of updates.
//!
//! # This is DOMAIN code — no ratatui
//!
//! The store speaks `optionstratlib` types only ([`OptionChain`], [`OptionData`],
//! [`Positive`], [`OptionStyle`]); it never imports a UI type. Draw only *reads*
//! the projection ([`ChainStore::bid_dir`] / [`ChainStore::quote_freshness`]) —
//! every direction and pricing mutation happens on the market/tick **event**
//! ([`ChainStore::apply_quote`] / [`ChainStore::apply_greeks`] /
//! [`ChainStore::apply_poll`] / [`ChainStore::apply_health`]), never in `draw`
//! (`CLAUDE.md` "Module Boundaries").
//!
//! # The strike-keyed clone / patch / re-insert row update
//!
//! Upstream stores rows in a `BTreeSet<OptionData>` ordered by strike with no
//! mutable row lookup, so an update is applied by taking the row at its strike
//! out (via a strike-only probe, since [`OptionData`]'s `Ord` is its
//! `strike_price`), clone-and-patching only the fields for the update's
//! [`OptionStyle`], recomputing the touched side's `*_middle`, and re-inserting.
//! The opposite leg and every untouched field are preserved and set ordering is
//! unchanged (`docs/01-domain-model.md` §6 invariants).
//!
//! # Field-fold rules (`docs/03-data-providers.md` §3)
//!
//! A **rejected field** keeps its prior value (an absent `Option` leaves the
//! prior in place). A **crossed** update (`ask < bid`, or a zero ask on a
//! non-zero bid) rejects the **whole** update and keeps the prior row. Because
//! prices/IV arrive here already validated into [`Positive`] (NaN/Inf/negative
//! rejected at the adapter seam, issue #15), the store only enforces the
//! crossed/zero rule; a negative is unrepresentable at this boundary.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};
use optionstratlib::OptionStyle;
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::Positive;

use super::events::{
    ChainSnapshot, ChainSource, DIRECTION_DECAY, FEED_DELAY_WARN, GREEKS_STALE_AFTER, GreeksRow,
    QUOTE_STALE_AFTER, QuoteUpdate, StreamHealth, chain_stale_after,
};
use super::fetch::{AliasCatalog, ChainFetch};
use super::identity::{InstrumentKey, ProviderId};

// --- Bounded-merge tuning constants (`docs/03-data-providers.md` §4) ----------

/// The maximum number of unknown-strike stream updates held pending a poll that
/// introduces the strike (`docs/03-data-providers.md` §4). On overflow the
/// **oldest** entry is dropped and counted ([`ChainStore::dropped_overflow`]),
/// so the buffer can never grow without bound. Default **256** per chain.
pub const MAX_PENDING: usize = 256;

/// The pending-buffer per-entry TTL for a given `refresh_interval`:
/// *`refresh_interval` + [`CHAIN_STALE_SLACK`](super::events::CHAIN_STALE_SLACK)*
/// — one poll of headroom (`docs/03-data-providers.md` §4). An unknown-strike
/// update older than this that never appeared in a snapshot is dropped, never
/// resurrected.
///
/// The addition saturates to [`Duration::MAX`] rather than overflowing
/// (structurally impossible under the config `refresh_interval` ceiling, but
/// total for any input) — it delegates to
/// [`chain_stale_after`](super::events::chain_stale_after), which fixes the same
/// *`refresh_interval` + slack* formula.
#[must_use]
pub fn pending_ttl(refresh_interval: Duration) -> Duration {
    chain_stale_after(refresh_interval)
}

// --- Projected view enums (read by draw; never mutated there) ------------------

/// The direction of the most recent change to a price, for the bid-up/ask-down
/// indicator (`docs/01-domain-model.md` §6, §8).
///
/// Projected from the retained baseline in the store, never stored on
/// [`OptionData`]. A strictly higher value is [`Up`](TickDir::Up), strictly lower
/// is [`Down`](TickDir::Down), an equal value leaves the prior direction
/// unchanged, and a first-ever value is [`Flat`](TickDir::Flat). The indicator
/// decays to `Flat` after [`DIRECTION_DECAY`] with no further change and is
/// cleared to `Flat` immediately when the feed goes stale/reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TickDir {
    /// The last change raised the price.
    Up,
    /// The last change lowered the price.
    Down,
    /// No change yet, decayed, or cleared by a stale/reconnecting feed.
    #[default]
    Flat,
}

/// How current one component (quote, Greeks, or chain structure) is, derived
/// from the two clocks of `docs/01-domain-model.md` §5.1.
///
/// Precedence when several apply: [`Absent`](Freshness::Absent) (never received)
/// wins, then [`Stale`](Freshness::Stale) (no update within its threshold,
/// measured from `received_time`), then [`Delayed`](Freshness::Delayed) (the feed
/// is live but the venue `event_time` lags past [`FEED_DELAY_WARN`]), else
/// [`Fresh`](Freshness::Fresh).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Within its staleness threshold and not lagging.
    Fresh,
    /// Live, but the venue data lags by `by` past [`FEED_DELAY_WARN`]; a negative
    /// skew (venue clock ahead of ours) is clamped to zero.
    Delayed {
        /// How far the venue `event_time` lags behind now (>= zero).
        by: Duration,
    },
    /// Past its staleness threshold — badged stale even while the connection is
    /// up (a silent-but-connected feed).
    Stale {
        /// When the component was last fresh (its last `received_time`).
        since: DateTime<Utc>,
    },
    /// No update for this component has ever been recorded.
    Absent,
}

/// The outcome of folding one streaming update into the store — surfaced so the
/// caller (the app fan-in, issue #9) can log/badge without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MergeOutcome {
    /// The update patched its row (or a pending update was applied on a poll).
    Applied,
    /// The update's strike is not (yet) in the chain and was buffered pending a
    /// poll that introduces it.
    Buffered,
    /// The update's `event_time` was below the per-instrument watermark — a late
    /// straggler, dropped and counted.
    DroppedOutOfOrder,
    /// The update's strike was de-listed by a prior poll (tombstoned) — dropped,
    /// never resurrected.
    DroppedTombstoned,
    /// The merged quote would be crossed (`ask < bid`, or a zero ask on a
    /// non-zero bid) — the whole update is rejected and the prior row kept.
    DroppedCrossed,
    /// A cross-provider overlay leg whose [`ContractSpecFingerprint`] disagreed
    /// with the source leg — refused, the source leg kept, the leg badged
    /// overlay-refused ([`ChainStore::is_overlay_refused`]).
    ///
    /// [`ContractSpecFingerprint`]: super::identity::ContractSpecFingerprint
    OverlayRefused,
}

// --- Per-instrument freshness / direction state (private) ---------------------

/// The store's per-[`InstrumentKey`] sidecar: the freshness clocks, the
/// price-direction baseline, and the out-of-order tally. Kept off [`OptionData`]
/// (which cannot hold it) and never persisted.
#[derive(Debug, Clone, Default)]
struct InstrumentState {
    /// `max(event_time)` seen for this key — the out-of-order guard (§5.1).
    watermark: Option<DateTime<Utc>>,
    /// When the last quote for this key was received (staleness clock).
    quote_received: Option<DateTime<Utc>>,
    /// The venue `event_time` of the last applied quote (feed-delay clock).
    quote_event_time: Option<DateTime<Utc>>,
    /// When the last Greeks row for this key was received (staleness clock).
    greeks_received: Option<DateTime<Utc>>,
    /// The venue `event_time` of the last applied Greeks row (feed-delay clock).
    greeks_event_time: Option<DateTime<Utc>>,
    /// The previous bid used for the direction compare.
    prev_bid: Option<Positive>,
    /// The previous ask used for the direction compare.
    prev_ask: Option<Positive>,
    /// The retained bid direction (before decay).
    bid_dir: TickDir,
    /// The retained ask direction (before decay).
    ask_dir: TickDir,
    /// When the bid last changed (decay clock).
    bid_changed_at: Option<DateTime<Utc>>,
    /// When the ask last changed (decay clock).
    ask_changed_at: Option<DateTime<Utc>>,
    /// How many out-of-order updates were dropped for this key.
    dropped_stale: u64,
}

/// A streaming update held in the pending-unknown-strike buffer.
#[derive(Debug, Clone)]
enum PendingUpdate {
    /// A buffered quote refresh.
    Quote(QuoteUpdate),
    /// A buffered Greeks/IV refresh.
    Greeks(GreeksRow),
}

/// One entry in the bounded pending buffer: the update plus the strike it is
/// waiting on and when it was buffered (its TTL clock).
#[derive(Debug, Clone)]
struct PendingEntry {
    /// The buffered update.
    update: PendingUpdate,
    /// The strike the update is waiting for a poll to introduce.
    strike: Positive,
    /// When the entry was buffered — the TTL is measured from here.
    inserted_at: DateTime<Utc>,
}

// --- The store ----------------------------------------------------------------

/// The normalized, streaming-current chain for one `(provider, underlying,
/// expiry)` (`docs/01-domain-model.md` §6).
///
/// Single-threaded-owned by the application loop — it holds no lock and is
/// mutated only on the market/tick event, never in `draw`. Assembled from a
/// [`ChainFetch`] via [`seed`](ChainStore::seed), carrying the same
/// [`AliasCatalog`] forward with no re-derivation.
#[derive(Debug)]
pub struct ChainStore {
    /// `(source provider, underlying, absolute-UTC expiry)`.
    chain_key: (ProviderId, String, DateTime<Utc>),
    /// The normalized `optionstratlib` chain — the source of truth draw reads.
    chain: OptionChain,
    /// The per-leg native+stream alias catalog carried forward from the fetch.
    aliases: AliasCatalog,
    /// How the chain is being kept current.
    source: ChainSource,
    /// How current the chain is.
    health: StreamHealth,
    /// The wall-time of the last full poll.
    last_full_poll: Option<DateTime<Utc>>,
    /// The configured re-poll cadence — the chain-staleness and pending-TTL base.
    refresh_interval: Duration,
    /// The monotonic snapshot generation, bumped per [`apply_poll`](ChainStore::apply_poll).
    generation: u64,
    /// Strikes present in a prior poll but absent now — never resurrected.
    tombstones: HashSet<Positive>,
    /// The bounded pending-unknown-strike buffer (FIFO, capped at [`MAX_PENDING`]).
    pending: VecDeque<PendingEntry>,
    /// How many pending entries were dropped on overflow.
    dropped_overflow: u64,
    /// Per-instrument freshness / direction sidecar.
    instruments: HashMap<InstrumentKey, InstrumentState>,
    /// Legs whose latest cross-provider overlay was refused on a spec mismatch.
    overlay_refused: HashSet<InstrumentKey>,
}

impl ChainStore {
    /// Seed the store from the first poll's [`ChainFetch`], carrying its
    /// [`AliasCatalog`] forward unchanged. `source` declares how the chain is
    /// kept current, `refresh_interval` fixes the chain-staleness and pending-TTL
    /// base, and `now` stamps the first poll.
    #[must_use]
    pub fn seed(
        fetch: ChainFetch,
        source: ChainSource,
        refresh_interval: Duration,
        now: DateTime<Utc>,
    ) -> Self {
        let ChainFetch {
            chain,
            expiry_source,
            aliases,
        } = fetch;
        // On the (structurally impossible) overflow, fall back to the strike
        // count rather than `usize::MAX` — a checked op with a non-MAX fallback,
        // so it is neither a magic saturating call nor a panic. Capacity is only
        // a hint, so an under-estimate is harmless.
        let strikes = chain.options.len();
        let instrument_cap = strikes.checked_mul(2).unwrap_or(strikes);
        let chain_key = (
            expiry_source.provider,
            expiry_source.underlying,
            expiry_source.expiration_utc,
        );
        Self {
            chain_key,
            chain,
            aliases,
            source,
            health: StreamHealth::Live,
            last_full_poll: Some(now),
            refresh_interval,
            generation: 1,
            tombstones: HashSet::new(),
            pending: VecDeque::new(),
            dropped_overflow: 0,
            instruments: HashMap::with_capacity(instrument_cap),
            overlay_refused: HashSet::new(),
        }
    }

    /// Reconcile structure against a fresh poll: bump the generation, tombstone
    /// de-listed strikes, clear the tombstone of any re-listed strike, replace
    /// the structure and alias catalog with the fetch's (carried forward, no
    /// re-derivation), then apply any pending updates whose strike is now present
    /// and expire the rest.
    pub fn apply_poll(&mut self, fetch: ChainFetch, now: DateTime<Utc>) {
        self.generation = self.generation.checked_add(1).unwrap_or(self.generation);
        let ChainFetch {
            chain,
            expiry_source,
            aliases,
        } = fetch;

        let new_strikes: HashSet<Positive> = chain.options.iter().map(|o| o.strike_price).collect();
        // Strikes present before and absent now are de-listed -> tombstone.
        for old in &self.chain.options {
            if !new_strikes.contains(&old.strike_price) {
                let _ = self.tombstones.insert(old.strike_price);
            }
        }
        // A strike that reappears is a genuine re-listing -> clear its tombstone.
        for strike in &new_strikes {
            let _ = self.tombstones.remove(strike);
        }

        self.chain = chain;
        self.aliases = aliases;
        self.chain_key = (
            expiry_source.provider,
            expiry_source.underlying,
            expiry_source.expiration_utc,
        );
        self.last_full_poll = Some(now);

        self.drain_pending(now);
    }

    /// Fold a [`QuoteUpdate`] into its row: gate a cross-provider overlay, drop an
    /// out-of-order update, then either patch the present row (crossed rejects the
    /// whole update), buffer an unknown strike, or drop a tombstoned one.
    pub fn apply_quote(&mut self, update: &QuoteUpdate) -> MergeOutcome {
        let key = &update.instrument.key;
        if let Some(outcome) = self.gate_overlay(key, &update.instrument.provider) {
            return outcome;
        }
        if self.is_out_of_order(key, update.event_time) {
            return MergeOutcome::DroppedOutOfOrder;
        }
        let strike = key.strike;
        if self.contains_strike(strike) {
            if self.apply_quote_to_row(update) {
                let _ = self.overlay_refused.remove(key);
                MergeOutcome::Applied
            } else {
                MergeOutcome::DroppedCrossed
            }
        } else if self.tombstones.contains(&strike) {
            MergeOutcome::DroppedTombstoned
        } else if update.bid.is_some() && update.ask.is_some() && is_crossed(update.bid, update.ask)
        {
            MergeOutcome::DroppedCrossed
        } else {
            self.buffer_pending(
                PendingUpdate::Quote(update.clone()),
                strike,
                update.received_time,
            );
            MergeOutcome::Buffered
        }
    }

    /// Fold a [`GreeksRow`] into its row: gate a cross-provider overlay, drop an
    /// out-of-order update, then either patch the present row (an absent field
    /// keeps the prior value), buffer an unknown strike, or drop a tombstoned one.
    ///
    /// `theta`/`vega`/`rho` have no [`OptionData`] field and are intentionally not
    /// folded here — the analytics sidecar that holds them lands in v0.2
    /// (`docs/01-domain-model.md` §7).
    pub fn apply_greeks(&mut self, update: &GreeksRow) -> MergeOutcome {
        let key = &update.instrument.key;
        if let Some(outcome) = self.gate_overlay(key, &update.instrument.provider) {
            return outcome;
        }
        if self.is_out_of_order(key, update.event_time) {
            return MergeOutcome::DroppedOutOfOrder;
        }
        let strike = key.strike;
        if self.contains_strike(strike) {
            self.apply_greeks_to_row(update);
            let _ = self.overlay_refused.remove(key);
            MergeOutcome::Applied
        } else if self.tombstones.contains(&strike) {
            MergeOutcome::DroppedTombstoned
        } else {
            self.buffer_pending(
                PendingUpdate::Greeks(update.clone()),
                strike,
                update.received_time,
            );
            MergeOutcome::Buffered
        }
    }

    /// Record a stream health transition. A stale or reconnecting feed clears
    /// every retained price-direction indicator to [`TickDir::Flat`] immediately
    /// — a stale feed never shows a live arrow (`docs/01-domain-model.md` §6).
    pub fn apply_health(&mut self, health: StreamHealth) {
        match health {
            StreamHealth::Stale { .. } | StreamHealth::Reconnecting { .. } => {
                for state in self.instruments.values_mut() {
                    state.bid_dir = TickDir::Flat;
                    state.ask_dir = TickDir::Flat;
                }
            }
            StreamHealth::Live => {}
        }
        self.health = health;
    }

    // --- Read-only projections (draw reads these; it never mutates) -----------

    /// The normalized chain — the source of truth draw renders.
    #[must_use]
    pub fn chain(&self) -> &OptionChain {
        &self.chain
    }

    /// The per-leg alias catalog carried forward from the poll.
    #[must_use]
    pub fn aliases(&self) -> &AliasCatalog {
        &self.aliases
    }

    /// How the chain is being kept current.
    #[must_use]
    pub fn source(&self) -> ChainSource {
        self.source
    }

    /// How current the chain is.
    #[must_use]
    pub fn health(&self) -> &StreamHealth {
        &self.health
    }

    /// `(source provider, underlying, absolute-UTC expiry)`.
    #[must_use]
    pub fn chain_key(&self) -> &(ProviderId, String, DateTime<Utc>) {
        &self.chain_key
    }

    /// The wall-time of the last full poll, or `None` before the first.
    #[must_use]
    pub fn last_full_poll(&self) -> Option<DateTime<Utc>> {
        self.last_full_poll
    }

    /// The current pending-buffer occupancy (never exceeds [`MAX_PENDING`]).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// How many pending entries have been dropped on overflow so far.
    #[must_use]
    pub fn dropped_overflow(&self) -> u64 {
        self.dropped_overflow
    }

    /// How many out-of-order updates have been dropped for one instrument.
    #[must_use]
    pub fn dropped_stale(&self, key: &InstrumentKey) -> u64 {
        self.instruments.get(key).map_or(0, |s| s.dropped_stale)
    }

    /// True when `strike` was de-listed by a poll and not since re-listed.
    #[must_use]
    pub fn is_tombstoned(&self, strike: Positive) -> bool {
        self.tombstones.contains(&strike)
    }

    /// True when a row for `strike` is currently in the chain.
    #[must_use]
    pub fn contains_strike(&self, strike: Positive) -> bool {
        self.chain.options.contains(&probe_row(strike))
    }

    /// True when this leg's latest cross-provider overlay was refused on a spec
    /// mismatch (the source leg was kept) — drives the overlay-refused badge.
    #[must_use]
    pub fn is_overlay_refused(&self, key: &InstrumentKey) -> bool {
        self.overlay_refused.contains(key)
    }

    /// The decayed bid-direction indicator for one instrument as of `now`.
    #[must_use]
    pub fn bid_dir(&self, key: &InstrumentKey, now: DateTime<Utc>) -> TickDir {
        match self.instruments.get(key) {
            Some(state) => decayed(state.bid_dir, state.bid_changed_at, now),
            None => TickDir::Flat,
        }
    }

    /// The decayed ask-direction indicator for one instrument as of `now`.
    #[must_use]
    pub fn ask_dir(&self, key: &InstrumentKey, now: DateTime<Utc>) -> TickDir {
        match self.instruments.get(key) {
            Some(state) => decayed(state.ask_dir, state.ask_changed_at, now),
            None => TickDir::Flat,
        }
    }

    /// How fresh this instrument's quote is as of `now` (§5.1).
    #[must_use]
    pub fn quote_freshness(&self, key: &InstrumentKey, now: DateTime<Utc>) -> Freshness {
        match self.instruments.get(key) {
            Some(state) => classify(
                state.quote_received,
                state.quote_event_time,
                now,
                QUOTE_STALE_AFTER,
            ),
            None => Freshness::Absent,
        }
    }

    /// How fresh this instrument's Greeks are as of `now` (§5.1).
    #[must_use]
    pub fn greeks_freshness(&self, key: &InstrumentKey, now: DateTime<Utc>) -> Freshness {
        match self.instruments.get(key) {
            Some(state) => classify(
                state.greeks_received,
                state.greeks_event_time,
                now,
                GREEKS_STALE_AFTER,
            ),
            None => Freshness::Absent,
        }
    }

    /// How fresh the chain **structure** is as of `now`: stale once the last poll
    /// ages past *`refresh_interval` + slack* (§5.1). Structure carries no venue
    /// `event_time`, so it is never [`Freshness::Delayed`].
    #[must_use]
    pub fn chain_freshness(&self, now: DateTime<Utc>) -> Freshness {
        let threshold = chain_stale_after(self.refresh_interval);
        match self.last_full_poll {
            Some(polled) => {
                if age_between(polled, now) > threshold {
                    Freshness::Stale { since: polled }
                } else {
                    Freshness::Fresh
                }
            }
            None => Freshness::Absent,
        }
    }

    /// Assemble a [`ChainSnapshot`] message from the current state, carrying the
    /// [`AliasCatalog`] forward (`docs/01-domain-model.md` §6). This clones the
    /// chain and catalog for the message; the store retains its own copies.
    #[must_use]
    pub fn snapshot(&self) -> ChainSnapshot {
        ChainSnapshot {
            chain_key: self.chain_key.clone(),
            chain: self.chain.clone(),
            aliases: self.aliases.clone(),
            source: self.source,
            health: self.health.clone(),
            last_full_poll: self.last_full_poll,
        }
    }

    // --- Internal merge helpers ------------------------------------------------

    /// The cross-provider overlay gate. Returns `Some(OverlayRefused)` and records
    /// the refused leg when the overlay feed's fingerprint disagrees with the
    /// source leg's; `None` when the merge may proceed (a within-provider update,
    /// or a matching cross-provider fingerprint). The within-provider case never
    /// trips the gate — the source and overlay provider are identical.
    fn gate_overlay(&mut self, key: &InstrumentKey, overlay: &ProviderId) -> Option<MergeOutcome> {
        let source = &self.chain_key.0;
        if overlay == source {
            return None;
        }
        if self
            .aliases
            .overlay_compatible(key, source, overlay)
            .is_err()
        {
            let _ = self.overlay_refused.insert(key.clone());
            return Some(MergeOutcome::OverlayRefused);
        }
        None
    }

    /// Apply the per-instrument watermark rule (§5.1): an update whose
    /// `event_time` is below the watermark is out-of-order (returns `true`, drop +
    /// count); otherwise the watermark advances. An update with no `event_time`
    /// orders by receipt and never advances the event-time watermark.
    /// Get (or first-time create) the per-instrument sidecar. The owned
    /// [`InstrumentKey`] is cloned **only** on an instrument's first sighting; every
    /// later update on the hot merge path takes the `get_mut` branch with no
    /// allocation (the per-update path `bench_chain_merge` HP-3 targets).
    fn instrument_state_mut(&mut self, key: &InstrumentKey) -> &mut InstrumentState {
        if !self.instruments.contains_key(key) {
            self.instruments
                .insert(key.clone(), InstrumentState::default());
        }
        self.instruments
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("instrument state was just inserted"))
    }

    fn is_out_of_order(&mut self, key: &InstrumentKey, event_time: Option<DateTime<Utc>>) -> bool {
        let Some(event_time) = event_time else {
            return false;
        };
        let state = self.instrument_state_mut(key);
        if let Some(watermark) = state.watermark {
            if event_time < watermark {
                // Checked increment with a self fallback (not `u64::MAX`), so it
                // is a checked op rather than a magic saturating call.
                state.dropped_stale = state
                    .dropped_stale
                    .checked_add(1)
                    .unwrap_or(state.dropped_stale);
                return true;
            }
        }
        state.watermark = Some(event_time);
        false
    }

    /// Patch a present row with a quote and, on success, record the receipt and
    /// direction baseline. Returns `false` when the merged quote is crossed (the
    /// whole update is rejected and the prior row kept).
    fn apply_quote_to_row(&mut self, update: &QuoteUpdate) -> bool {
        if self.patch_quote_row(update) {
            self.record_quote(&update.instrument.key, update);
            true
        } else {
            false
        }
    }

    /// The clone / patch / re-insert for a quote. Takes the row at the strike,
    /// clone-patches only the update's style side, recomputes that side's
    /// `*_middle`, and re-inserts. Returns `false` (row unchanged) when the merged
    /// quote is crossed.
    fn patch_quote_row(&mut self, update: &QuoteUpdate) -> bool {
        let key = &update.instrument.key;
        let Some(existing) = self.chain.options.get(&probe_row(key.strike)) else {
            return false;
        };
        let mut row = existing.clone();
        let (prior_bid, prior_ask) = match key.style {
            OptionStyle::Call => (row.call_bid, row.call_ask),
            OptionStyle::Put => (row.put_bid, row.put_ask),
        };
        let new_bid = update.bid.or(prior_bid);
        let new_ask = update.ask.or(prior_ask);
        if is_crossed(new_bid, new_ask) {
            return false;
        }
        match key.style {
            OptionStyle::Call => {
                row.call_bid = new_bid;
                row.call_ask = new_ask;
                row.call_middle = midpoint(new_bid, new_ask);
            }
            OptionStyle::Put => {
                row.put_bid = new_bid;
                row.put_ask = new_ask;
                row.put_middle = midpoint(new_bid, new_ask);
            }
        }
        let _ = self.chain.options.replace(row);
        true
    }

    /// The clone / patch / re-insert for a Greeks row. A rejected (absent) field
    /// keeps the prior value; the opposite leg's delta and untouched fields are
    /// preserved. `implied_volatility` and `gamma` are shared per strike upstream,
    /// so the last style to arrive wins them (the per-style sidecar in v0.2 fixes
    /// this, `docs/01-domain-model.md` §7).
    fn apply_greeks_to_row(&mut self, update: &GreeksRow) {
        let key = &update.instrument.key;
        let Some(existing) = self.chain.options.get(&probe_row(key.strike)) else {
            return;
        };
        let mut row = existing.clone();
        match key.style {
            OptionStyle::Call => {
                if let Some(delta) = update.delta {
                    row.delta_call = Some(delta);
                }
            }
            OptionStyle::Put => {
                if let Some(delta) = update.delta {
                    row.delta_put = Some(delta);
                }
            }
        }
        if let Some(gamma) = update.gamma {
            row.gamma = Some(gamma);
        }
        if let Some(iv) = update.iv {
            row.implied_volatility = iv;
        }
        let _ = self.chain.options.replace(row);
        self.record_greeks(key, update);
    }

    /// Record a quote's receipt clocks and advance its bid/ask direction baseline.
    fn record_quote(&mut self, key: &InstrumentKey, update: &QuoteUpdate) {
        let state = self.instrument_state_mut(key);
        state.quote_received = Some(update.received_time);
        if update.event_time.is_some() {
            state.quote_event_time = update.event_time;
        }
        if let Some(bid) = update.bid {
            update_dir(
                &mut state.bid_dir,
                &mut state.prev_bid,
                &mut state.bid_changed_at,
                bid,
                update.received_time,
            );
        }
        if let Some(ask) = update.ask {
            update_dir(
                &mut state.ask_dir,
                &mut state.prev_ask,
                &mut state.ask_changed_at,
                ask,
                update.received_time,
            );
        }
    }

    /// Record a Greeks row's receipt clocks.
    fn record_greeks(&mut self, key: &InstrumentKey, update: &GreeksRow) {
        let state = self.instrument_state_mut(key);
        state.greeks_received = Some(update.received_time);
        if update.event_time.is_some() {
            state.greeks_event_time = update.event_time;
        }
    }

    /// Buffer an unknown-strike update, dropping the oldest entry (counted) when
    /// the buffer is at [`MAX_PENDING`].
    fn buffer_pending(
        &mut self,
        update: PendingUpdate,
        strike: Positive,
        inserted_at: DateTime<Utc>,
    ) {
        if self.pending.len() >= MAX_PENDING {
            let _ = self.pending.pop_front();
            self.dropped_overflow = self
                .dropped_overflow
                .checked_add(1)
                .unwrap_or(self.dropped_overflow);
        }
        self.pending.push_back(PendingEntry {
            update,
            strike,
            inserted_at,
        });
    }

    /// Apply or expire the pending buffer after a poll: a pending update whose
    /// strike is now present is applied and cleared; one whose strike is
    /// tombstoned or past its TTL is dropped (never resurrected); one still
    /// awaiting a genuine new listing within its TTL is retained.
    fn drain_pending(&mut self, now: DateTime<Utc>) {
        let ttl = pending_ttl(self.refresh_interval);
        let entries = std::mem::take(&mut self.pending);
        let mut retained: VecDeque<PendingEntry> = VecDeque::with_capacity(entries.len());
        for entry in entries {
            if self.contains_strike(entry.strike) {
                match &entry.update {
                    PendingUpdate::Quote(quote) => {
                        let _ = self.apply_quote_to_row(quote);
                    }
                    PendingUpdate::Greeks(greeks) => {
                        self.apply_greeks_to_row(greeks);
                    }
                }
            } else if self.tombstones.contains(&entry.strike)
                || age_between(entry.inserted_at, now) > ttl
            {
                // Tombstoned or expired: dropped, never resurrected.
            } else {
                retained.push_back(entry);
            }
        }
        self.pending = retained;
    }
}

// --- Free helpers -------------------------------------------------------------

/// A strike-only probe row for a `BTreeSet<OptionData>` lookup — [`OptionData`]'s
/// `Ord` is its `strike_price`, so a probe carrying just the strike locates the
/// real row.
fn probe_row(strike: Positive) -> OptionData {
    OptionData {
        strike_price: strike,
        ..Default::default()
    }
}

/// True when a bid/ask pair is crossed: `ask < bid`, or a zero ask on a non-zero
/// bid. A zero bid (with any ask >= it) is valid, and a missing side is never
/// crossed (`docs/03-data-providers.md` §3).
fn is_crossed(bid: Option<Positive>, ask: Option<Positive>) -> bool {
    match (bid, ask) {
        (Some(bid), Some(ask)) => ask < bid || (ask.is_zero() && !bid.is_zero()),
        _ => false,
    }
}

/// The midpoint of a bid/ask pair, rounded to 4 dp — matching upstream
/// `OptionData::set_mid_prices`. `None` unless both sides are present.
fn midpoint(bid: Option<Positive>, ask: Option<Positive>) -> Option<Positive> {
    match (bid, ask) {
        (Some(bid), Some(ask)) => Some(((bid + ask) / Positive::TWO).round_to(4)),
        _ => None,
    }
}

/// The non-negative age from `earlier` to `now`, clamping a negative skew (an
/// `earlier` in the future) to zero (§5.1).
fn age_between(earlier: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    now.signed_duration_since(earlier)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

/// Classify a component's freshness from its receipt/event clocks and threshold
/// (§5.1): `Absent` -> `Stale` -> `Delayed` -> `Fresh`.
fn classify(
    received: Option<DateTime<Utc>>,
    event_time: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    threshold: Duration,
) -> Freshness {
    let Some(received) = received else {
        return Freshness::Absent;
    };
    if age_between(received, now) > threshold {
        return Freshness::Stale { since: received };
    }
    if let Some(event_time) = event_time {
        let delay = age_between(event_time, now);
        if delay > FEED_DELAY_WARN {
            return Freshness::Delayed { by: delay };
        }
    }
    Freshness::Fresh
}

/// The direction indicator after decay: `Flat` once the last change is older than
/// [`DIRECTION_DECAY`] (or if there was never a change), else the retained
/// direction.
fn decayed(dir: TickDir, changed_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> TickDir {
    match changed_at {
        Some(changed_at) if age_between(changed_at, now) > DIRECTION_DECAY => TickDir::Flat,
        Some(_) => dir,
        None => TickDir::Flat,
    }
}

/// Advance one side's direction baseline on a new value: a strictly higher value
/// is `Up`, strictly lower is `Down`, an equal value leaves the prior direction
/// (and change time) unchanged, and a first-ever value is `Flat`.
fn update_dir(
    dir: &mut TickDir,
    prev: &mut Option<Positive>,
    changed_at: &mut Option<DateTime<Utc>>,
    value: Positive,
    at: DateTime<Utc>,
) {
    match *prev {
        None => {
            *dir = TickDir::Flat;
            *prev = Some(value);
            *changed_at = Some(at);
        }
        Some(previous) => {
            if value > previous {
                *dir = TickDir::Up;
                *prev = Some(value);
                *changed_at = Some(at);
            } else if value < previous {
                *dir = TickDir::Down;
                *prev = Some(value);
                *changed_at = Some(at);
            }
            // Equal: keep the prior direction, change time, and baseline.
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use optionstratlib::OptionStyle;
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::{Decimal, Positive};
    use proptest::prelude::*;

    use super::{ChainStore, Freshness, MAX_PENDING, MergeOutcome, TickDir, pending_ttl};
    use crate::chain::events::{ChainSource, GreeksOrigin, GreeksRow, QuoteUpdate, StreamHealth};
    use crate::chain::fetch::{AliasCatalog, ChainFetch, ExpirySource};
    use crate::chain::identity::{
        ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId,
        SettlementStyle,
    };

    /// The fixed absolute-UTC expiry every test key shares.
    const EXP: i64 = 1_700_000_000;

    // --- Constructors (no unwrap/expect/indexing per the ruleset) ------------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> chrono::DateTime<chrono::Utc> {
        match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
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

    /// A signed decimal from mantissa + scale (the `dec!` macro needs a direct
    /// `rust_decimal` dependency, so build from the re-exported type).
    fn dec(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    fn refresh() -> Duration {
        Duration::from_secs(2)
    }

    fn spec(multiplier: u32) -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: multiplier,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn ikey(strike: f64, style: OptionStyle) -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(EXP),
            strike: pos(strike),
            style,
        }
    }

    fn instrument_spec(
        provider: &str,
        strike: f64,
        style: OptionStyle,
        multiplier: u32,
    ) -> Instrument {
        Instrument {
            key: ikey(strike, style),
            provider: pid(provider),
            native_symbol: format!("BTC-{strike}-{}", style.as_str()),
            stream_symbol: None,
            spec: spec(multiplier),
        }
    }

    fn instrument(provider: &str, strike: f64, style: OptionStyle) -> Instrument {
        instrument_spec(provider, strike, style, 1)
    }

    fn quote(
        provider: &str,
        strike: f64,
        style: OptionStyle,
        bid: Option<f64>,
        ask: Option<f64>,
        event: Option<i64>,
        received: i64,
    ) -> QuoteUpdate {
        QuoteUpdate {
            instrument: instrument(provider, strike, style),
            bid: bid.map(pos),
            ask: ask.map(pos),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: event.map(utc),
            received_time: utc(received),
        }
    }

    fn greeks(
        provider: &str,
        strike: f64,
        style: OptionStyle,
        iv: Option<f64>,
        delta: Option<Decimal>,
        gamma: Option<Decimal>,
    ) -> GreeksRow {
        GreeksRow {
            instrument: instrument(provider, strike, style),
            iv: iv.map(pos),
            delta,
            gamma,
            theta: None,
            vega: None,
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(EXP + 100),
        }
    }

    fn row(
        strike: f64,
        call_bid: Option<f64>,
        call_ask: Option<f64>,
        put_bid: Option<f64>,
        put_ask: Option<f64>,
    ) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: call_bid.map(pos),
            call_ask: call_ask.map(pos),
            put_bid: put_bid.map(pos),
            put_ask: put_ask.map(pos),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn chain_with(rows: &[OptionData]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for od in rows {
            let _ = chain.options.insert(od.clone());
        }
        chain
    }

    fn fetch_for(chain: OptionChain, provider: &str, aliases: AliasCatalog) -> ChainFetch {
        ChainFetch::new(
            chain,
            ExpirySource::new("BTC", utc(EXP), pid(provider)),
            aliases,
        )
    }

    fn seed_single() -> ChainStore {
        let chain = chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]);
        ChainStore::seed(
            fetch_for(chain, "deribit", AliasCatalog::new()),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        )
    }

    #[track_caller]
    fn find_row(store: &ChainStore, strike: f64) -> OptionData {
        let target = pos(strike);
        match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == target)
        {
            Some(o) => o.clone(),
            None => panic!("expected a row at strike {strike}"),
        }
    }

    // --- Seed + snapshot -----------------------------------------------------

    #[test]
    fn test_seed_carries_alias_catalog_and_key_forward() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", 60_000.0, OptionStyle::Call));
        let chain = chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]);
        let store = ChainStore::seed(
            fetch_for(chain, "deribit", catalog),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        );
        assert_eq!(store.chain_key().0.as_str(), "deribit");
        assert_eq!(store.chain_key().1, "BTC");
        assert_eq!(store.chain_key().2, utc(EXP));
        assert_eq!(store.source(), ChainSource::Merged);
        assert_eq!(store.aliases().len(), 1);
        assert!(matches!(store.health(), StreamHealth::Live));
        assert_eq!(store.last_full_poll(), Some(utc(EXP)));
    }

    #[test]
    fn test_snapshot_reflects_current_state() {
        let store = seed_single();
        let snapshot = store.snapshot();
        assert_eq!(snapshot.chain_key.0.as_str(), "deribit");
        assert_eq!(snapshot.source, ChainSource::Merged);
        assert_eq!(snapshot.chain.symbol, "BTC");
        // The single seed strike registered no alias, so the catalog is empty but
        // carried forward as the same value.
        assert!(snapshot.aliases.is_empty());
        assert_eq!(snapshot.last_full_poll, Some(utc(EXP)));
    }

    // --- Clone/patch row update: opposite leg + midpoints preserved ----------

    #[test]
    fn test_apply_quote_call_then_put_preserve_opposite_leg_and_middles() {
        let mut store = seed_single();
        let key_call = ikey(60_000.0, OptionStyle::Call);
        let key_put = ikey(60_000.0, OptionStyle::Put);

        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.4),
                Some(1.6),
                None,
                EXP + 100
            )),
            MergeOutcome::Applied
        );
        let after_call = find_row(&store, 60_000.0);
        // Touched call side patched + middle recomputed.
        assert_eq!(after_call.call_bid, Some(pos(1.4)));
        assert_eq!(after_call.call_ask, Some(pos(1.6)));
        assert_eq!(after_call.call_middle, Some(pos(1.5)));
        // Opposite (put) leg preserved.
        assert_eq!(after_call.put_bid, Some(pos(2.0)));
        assert_eq!(after_call.put_ask, Some(pos(2.4)));
        assert_eq!(after_call.put_middle, Some(pos(2.2)));

        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Put,
                Some(2.6),
                Some(3.0),
                None,
                EXP + 101
            )),
            MergeOutcome::Applied
        );
        let after_put = find_row(&store, 60_000.0);
        assert_eq!(after_put.put_bid, Some(pos(2.6)));
        assert_eq!(after_put.put_ask, Some(pos(3.0)));
        assert_eq!(after_put.put_middle, Some(pos(2.8)));
        // Earlier call patch preserved.
        assert_eq!(after_put.call_bid, Some(pos(1.4)));
        assert_eq!(after_put.call_middle, Some(pos(1.5)));

        // Direction baselines recorded for both legs.
        assert_eq!(store.bid_dir(&key_call, utc(EXP + 100)), TickDir::Flat);
        assert_eq!(store.bid_dir(&key_put, utc(EXP + 101)), TickDir::Flat);
    }

    #[test]
    fn test_apply_quote_put_then_call_yields_same_row_as_call_then_put() {
        // Order independence of the two legs at one strike.
        let mut a = seed_single();
        let _ = a.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.4),
            Some(1.6),
            None,
            EXP + 100,
        ));
        let _ = a.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Put,
            Some(2.6),
            Some(3.0),
            None,
            EXP + 101,
        ));

        let mut b = seed_single();
        let _ = b.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Put,
            Some(2.6),
            Some(3.0),
            None,
            EXP + 100,
        ));
        let _ = b.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.4),
            Some(1.6),
            None,
            EXP + 101,
        ));

        assert_eq!(find_row(&a, 60_000.0), find_row(&b, 60_000.0));
    }

    // --- Field-fold rules: crossed / zero / missing --------------------------

    #[test]
    fn test_apply_quote_crossed_ask_below_bid_rejects_whole_update() {
        let mut store = seed_single();
        let outcome = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(2.0),
            Some(1.5),
            None,
            EXP + 100,
        ));
        assert_eq!(outcome, MergeOutcome::DroppedCrossed);
        let after = find_row(&store, 60_000.0);
        // Prior row kept in full.
        assert_eq!(after.call_bid, Some(pos(1.0)));
        assert_eq!(after.call_ask, Some(pos(1.2)));
    }

    #[test]
    fn test_apply_quote_zero_ask_on_nonzero_bid_rejects_whole_update() {
        let mut store = seed_single();
        let outcome = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(0.0),
            None,
            EXP + 100,
        ));
        assert_eq!(outcome, MergeOutcome::DroppedCrossed);
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.call_bid, Some(pos(1.0)));
        assert_eq!(after.call_ask, Some(pos(1.2)));
    }

    #[test]
    fn test_apply_quote_zero_bid_is_valid() {
        let mut store = seed_single();
        let outcome = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(0.0),
            Some(0.5),
            None,
            EXP + 100,
        ));
        assert_eq!(outcome, MergeOutcome::Applied);
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.call_bid, Some(pos(0.0)));
        assert_eq!(after.call_ask, Some(pos(0.5)));
        assert_eq!(after.call_middle, Some(pos(0.25)));
    }

    #[test]
    fn test_apply_quote_missing_bid_keeps_prior_bid() {
        let mut store = seed_single();
        let outcome = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            None,
            Some(1.6),
            None,
            EXP + 100,
        ));
        assert_eq!(outcome, MergeOutcome::Applied);
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.call_bid, Some(pos(1.0))); // prior kept
        assert_eq!(after.call_ask, Some(pos(1.6)));
        assert_eq!(after.call_middle, Some(pos(1.3)));
    }

    // --- Greeks fold ---------------------------------------------------------

    #[test]
    fn test_apply_greeks_call_sets_delta_call_keeps_delta_put() {
        let mut od = row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4));
        od.delta_put = Some(dec(-3, 1)); // -0.3
        let chain = chain_with(&[od]);
        let mut store = ChainStore::seed(
            fetch_for(chain, "deribit", AliasCatalog::new()),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        );
        let outcome = store.apply_greeks(&greeks(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(0.4),
            Some(dec(-6, 1)), // -0.6
            Some(dec(1, 2)),  // 0.01
        ));
        assert_eq!(outcome, MergeOutcome::Applied);
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.delta_call, Some(dec(-6, 1)));
        assert_eq!(after.delta_put, Some(dec(-3, 1))); // opposite leg preserved
        assert_eq!(after.gamma, Some(dec(1, 2)));
        assert_eq!(after.implied_volatility, pos(0.4));
    }

    #[test]
    fn test_apply_greeks_missing_field_keeps_prior() {
        let mut od = row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4));
        od.gamma = Some(dec(2, 2)); // 0.02
        let chain = chain_with(&[od]);
        let mut store = ChainStore::seed(
            fetch_for(chain, "deribit", AliasCatalog::new()),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        );
        let _ = store.apply_greeks(&greeks(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            None,
            Some(dec(-5, 1)),
            None,
        ));
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.delta_call, Some(dec(-5, 1)));
        assert_eq!(after.gamma, Some(dec(2, 2))); // absent field kept prior
        assert_eq!(after.implied_volatility, pos(0.5)); // absent iv kept prior
    }

    // --- Freshness / staleness threshold crossings ---------------------------

    #[test]
    fn test_quote_freshness_absent_when_never_received() {
        let store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        assert_eq!(
            store.quote_freshness(&key, utc(EXP + 10)),
            Freshness::Absent
        );
    }

    #[test]
    fn test_quote_freshness_fresh_within_threshold() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        // 3 s < QUOTE_STALE_AFTER (5 s), no event_time so no delay.
        assert_eq!(
            store.quote_freshness(&key, utc(EXP + 103)),
            Freshness::Fresh
        );
    }

    #[test]
    fn test_quote_freshness_stale_past_threshold() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        // 6 s > QUOTE_STALE_AFTER (5 s).
        match store.quote_freshness(&key, utc(EXP + 106)) {
            Freshness::Stale { since } => assert_eq!(since, utc(EXP + 100)),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn test_quote_freshness_delayed_when_event_time_lags() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        // event_time 3 s behind received; query at received.
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            Some(EXP + 97),
            EXP + 100,
        ));
        match store.quote_freshness(&key, utc(EXP + 100)) {
            Freshness::Delayed { by } => assert_eq!(by, Duration::from_secs(3)),
            other => panic!("expected Delayed, got {other:?}"),
        }
    }

    #[test]
    fn test_quote_freshness_negative_skew_clamped_to_zero_is_fresh() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        // event_time ahead of now (venue clock ahead) -> delay clamps to 0.
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            Some(EXP + 110),
            EXP + 100,
        ));
        assert_eq!(
            store.quote_freshness(&key, utc(EXP + 101)),
            Freshness::Fresh
        );
    }

    #[test]
    fn test_greeks_freshness_stale_after_ten_seconds() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_greeks(&greeks(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(0.4),
            Some(dec(-5, 1)),
            None,
        ));
        // Greeks received at EXP+100; 9 s fresh, 11 s stale.
        assert_eq!(
            store.greeks_freshness(&key, utc(EXP + 109)),
            Freshness::Fresh
        );
        assert!(matches!(
            store.greeks_freshness(&key, utc(EXP + 111)),
            Freshness::Stale { .. }
        ));
    }

    #[test]
    fn test_chain_freshness_stale_past_refresh_plus_slack() {
        let store = seed_single();
        // refresh (2 s) + slack (2 s) = 4 s threshold.
        assert_eq!(store.chain_freshness(utc(EXP + 3)), Freshness::Fresh);
        assert!(matches!(
            store.chain_freshness(utc(EXP + 5)),
            Freshness::Stale { .. }
        ));
    }

    // --- Out-of-order / watermark --------------------------------------------

    #[test]
    fn test_out_of_order_update_dropped_and_counted() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.5),
                Some(1.7),
                Some(EXP + 10),
                EXP + 100
            )),
            MergeOutcome::Applied
        );
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(2.0),
                Some(2.2),
                Some(EXP + 9),
                EXP + 101
            )),
            MergeOutcome::DroppedOutOfOrder
        );
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.call_bid, Some(pos(1.5))); // straggler did not overwrite
        assert_eq!(store.dropped_stale(&key), 1);
    }

    #[test]
    fn test_none_event_time_applies_without_advancing_watermark() {
        let mut store = seed_single();
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            Some(EXP + 10),
            EXP + 100,
        ));
        // No event_time -> applies, does not touch the event-time watermark.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(2.0),
                Some(2.2),
                None,
                EXP + 101
            )),
            MergeOutcome::Applied
        );
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(2.0)));
        // A later event_time below the retained watermark (10) is still dropped.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(3.0),
                Some(3.2),
                Some(EXP + 9),
                EXP + 102
            )),
            MergeOutcome::DroppedOutOfOrder
        );
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(2.0)));
    }

    // --- Price direction: retained baseline, decay, stale-clear --------------

    #[test]
    fn test_direction_first_ever_is_flat() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        assert_eq!(store.bid_dir(&key, utc(EXP + 100)), TickDir::Flat);
    }

    #[test]
    fn test_direction_up_then_down_then_equal_keeps_prior() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.5),
            Some(1.7),
            None,
            EXP + 101,
        ));
        assert_eq!(store.bid_dir(&key, utc(EXP + 101)), TickDir::Up);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.2),
            Some(1.4),
            None,
            EXP + 102,
        ));
        assert_eq!(store.bid_dir(&key, utc(EXP + 102)), TickDir::Down);
        // Equal value keeps the prior direction (Down), not Flat.
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.2),
            Some(1.4),
            None,
            EXP + 103,
        ));
        assert_eq!(store.bid_dir(&key, utc(EXP + 103)), TickDir::Down);
    }

    #[test]
    fn test_direction_decays_to_flat_after_threshold() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.5),
            Some(1.7),
            None,
            EXP + 101,
        ));
        // Within DIRECTION_DECAY (3 s).
        assert_eq!(store.bid_dir(&key, utc(EXP + 103)), TickDir::Up);
        // Past DIRECTION_DECAY -> Flat.
        assert_eq!(store.bid_dir(&key, utc(EXP + 105)), TickDir::Flat);
    }

    #[test]
    fn test_direction_cleared_to_flat_on_stale_health() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.5),
            Some(1.7),
            None,
            EXP + 101,
        ));
        assert_eq!(store.bid_dir(&key, utc(EXP + 101)), TickDir::Up);
        store.apply_health(StreamHealth::Stale {
            since: utc(EXP + 102),
        });
        // Immediately cleared to Flat even within the decay window.
        assert_eq!(store.bid_dir(&key, utc(EXP + 101)), TickDir::Flat);
    }

    #[test]
    fn test_direction_cleared_to_flat_on_reconnecting_health() {
        let mut store = seed_single();
        let key = ikey(60_000.0, OptionStyle::Call);
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            None,
            EXP + 100,
        ));
        let _ = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(0.5),
            Some(0.7),
            None,
            EXP + 101,
        ));
        assert_eq!(store.bid_dir(&key, utc(EXP + 101)), TickDir::Down);
        store.apply_health(StreamHealth::Reconnecting { attempt: 1 });
        assert_eq!(store.bid_dir(&key, utc(EXP + 101)), TickDir::Flat);
    }

    // --- Bounded-generation merge: tombstones, pending, no resurrection -------

    #[test]
    fn test_tombstoned_strike_update_dropped_not_resurrected() {
        let mut store = ChainStore::seed(
            fetch_for(
                chain_with(&[
                    row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4)),
                    row(61_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4)),
                ]),
                "deribit",
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        );
        // Poll removes 61000 -> tombstone.
        store.apply_poll(
            fetch_for(
                chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(EXP + 2),
        );
        assert!(store.is_tombstoned(pos(61_000.0)));
        // A stream update for the de-listed strike is dropped, never resurrected.
        let outcome = store.apply_quote(&quote(
            "deribit",
            61_000.0,
            OptionStyle::Call,
            Some(9.0),
            Some(9.2),
            None,
            EXP + 3,
        ));
        assert_eq!(outcome, MergeOutcome::DroppedTombstoned);
        assert!(!store.contains_strike(pos(61_000.0)));
        assert_eq!(store.pending_len(), 0);
    }

    #[test]
    fn test_new_listing_from_pending_applied_on_poll() {
        let mut store = seed_single(); // only 60000
        // Unknown strike -> buffered.
        let outcome = store.apply_quote(&quote(
            "deribit",
            61_000.0,
            OptionStyle::Call,
            Some(5.0),
            Some(5.2),
            None,
            EXP + 1,
        ));
        assert_eq!(outcome, MergeOutcome::Buffered);
        assert_eq!(store.pending_len(), 1);
        // Poll introduces 61000 as a genuine new listing.
        store.apply_poll(
            fetch_for(
                chain_with(&[
                    row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4)),
                    row(61_000.0, Some(0.1), Some(0.2), Some(0.3), Some(0.4)),
                ]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(EXP + 2),
        );
        assert!(store.contains_strike(pos(61_000.0)));
        assert_eq!(store.pending_len(), 0);
        // The buffered quote overlaid onto the new row.
        let after = find_row(&store, 61_000.0);
        assert_eq!(after.call_bid, Some(pos(5.0)));
        assert_eq!(after.call_ask, Some(pos(5.2)));
    }

    #[test]
    fn test_pending_expired_dropped_on_ttl() {
        let mut store = seed_single();
        let _ = store.apply_quote(&quote(
            "deribit",
            61_000.0,
            OptionStyle::Call,
            Some(5.0),
            Some(5.2),
            None,
            EXP,
        ));
        assert_eq!(store.pending_len(), 1);
        let ttl_secs = pending_ttl(refresh()).as_secs() as i64;
        // Poll well past the TTL, 61000 still absent -> dropped, not resurrected.
        store.apply_poll(
            fetch_for(
                chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(EXP + ttl_secs + 1),
        );
        assert_eq!(store.pending_len(), 0);
        assert!(!store.contains_strike(pos(61_000.0)));
    }

    #[test]
    fn test_pending_overflow_drops_oldest_counted() {
        let mut store = seed_single();
        for i in 0u32..300 {
            let outcome = store.apply_quote(&quote(
                "deribit",
                70_000.0 + f64::from(i),
                OptionStyle::Call,
                Some(1.0),
                Some(1.2),
                None,
                EXP + 200,
            ));
            assert_eq!(outcome, MergeOutcome::Buffered);
        }
        assert_eq!(store.pending_len(), MAX_PENDING);
        assert_eq!(store.dropped_overflow(), 300 - MAX_PENDING as u64);
    }

    #[test]
    fn test_relisted_strike_clears_tombstone() {
        let mut store = ChainStore::seed(
            fetch_for(
                chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]),
                "deribit",
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        );
        // Poll away 60000 -> tombstone; chain now empty.
        store.apply_poll(
            fetch_for(chain_with(&[]), "deribit", AliasCatalog::new()),
            utc(EXP + 2),
        );
        assert!(store.is_tombstoned(pos(60_000.0)));
        // Re-list 60000 -> tombstone cleared, row present again.
        store.apply_poll(
            fetch_for(
                chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(EXP + 4),
        );
        assert!(!store.is_tombstoned(pos(60_000.0)));
        assert!(store.contains_strike(pos(60_000.0)));
    }

    // --- Cross-provider overlay gate -----------------------------------------

    fn overlay_store(source_mult: u32, overlay_mult: u32) -> ChainStore {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument_spec(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            source_mult,
        ));
        catalog.insert(instrument_spec(
            "dxlink",
            60_000.0,
            OptionStyle::Call,
            overlay_mult,
        ));
        let chain = chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]);
        ChainStore::seed(
            fetch_for(chain, "deribit", catalog),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        )
    }

    #[test]
    fn test_overlay_refused_on_spec_mismatch_keeps_source() {
        let mut store = overlay_store(1, 100);
        let key = ikey(60_000.0, OptionStyle::Call);
        let outcome = store.apply_quote(&quote(
            "dxlink",
            60_000.0,
            OptionStyle::Call,
            Some(5.0),
            Some(5.2),
            None,
            EXP + 10,
        ));
        assert_eq!(outcome, MergeOutcome::OverlayRefused);
        assert!(store.is_overlay_refused(&key));
        // Source leg kept.
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(1.0)));
    }

    #[test]
    fn test_overlay_applied_on_spec_match() {
        let mut store = overlay_store(1, 1);
        let key = ikey(60_000.0, OptionStyle::Call);
        let outcome = store.apply_quote(&quote(
            "dxlink",
            60_000.0,
            OptionStyle::Call,
            Some(5.0),
            Some(5.2),
            None,
            EXP + 10,
        ));
        assert_eq!(outcome, MergeOutcome::Applied);
        assert!(!store.is_overlay_refused(&key));
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(5.0)));
    }

    #[test]
    fn test_within_provider_merge_bypasses_gate_with_empty_catalog() {
        // Same provider as the source -> the gate is a no-op even with no aliases.
        let mut store = seed_single();
        let outcome = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(5.0),
            Some(5.2),
            None,
            EXP + 10,
        ));
        assert_eq!(outcome, MergeOutcome::Applied);
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(5.0)));
    }

    // --- Property tests ------------------------------------------------------

    /// A strike value for a small universe index (no array indexing).
    fn strike_of(idx: u32) -> f64 {
        60_000.0 + f64::from(idx) * 1_000.0
    }

    fn chain_of(indices: &BTreeSet<u32>) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for &idx in indices {
            let _ = chain.options.insert(row(
                strike_of(idx),
                Some(1.0),
                Some(1.2),
                Some(2.0),
                Some(2.4),
            ));
        }
        chain
    }

    /// One scripted operation in the merge permutation invariant.
    #[derive(Debug, Clone)]
    enum Op {
        /// A full re-poll with the given strike indices.
        Poll(Vec<u32>),
        /// A stream quote for the given strike index.
        Quote(u32),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            proptest::collection::vec(0u32..5, 0..5).prop_map(Op::Poll),
            (0u32..5).prop_map(Op::Quote),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, max_shrink_iters: 20_000, ..ProptestConfig::default() })]

        /// Applying the same quote twice equals applying it once (idempotent fold).
        #[test]
        fn prop_chain_merge_idempotent(
            bid_ticks in 1u32..80,
            spread in 1u32..30,
            event in 0i64..100,
        ) {
            let bid = f64::from(bid_ticks) * 0.1;
            let ask = bid + f64::from(spread) * 0.1;
            let q = quote(
                "deribit", 60_000.0, OptionStyle::Call, Some(bid), Some(ask), Some(EXP + event), EXP + 200,
            );
            let mut once = seed_single();
            let _ = once.apply_quote(&q);
            let mut twice = seed_single();
            let _ = twice.apply_quote(&q);
            let _ = twice.apply_quote(&q);
            prop_assert_eq!(find_row(&once, 60_000.0), find_row(&twice, 60_000.0));
        }

        /// A cross-provider overlay merges a leg only when the fingerprints match;
        /// any mismatch is refused and the source leg is kept.
        #[test]
        fn prop_overlay_spec_gate(overlay_mult in 1u32..250) {
            let mut store = overlay_store(1, overlay_mult);
            let outcome = store.apply_quote(&quote(
                "dxlink", 60_000.0, OptionStyle::Call, Some(5.0), Some(5.2), None, EXP + 10,
            ));
            let after = find_row(&store, 60_000.0);
            if overlay_mult == 1 {
                prop_assert_eq!(outcome, MergeOutcome::Applied);
                prop_assert_eq!(after.call_bid, Some(pos(5.0)));
            } else {
                prop_assert_eq!(outcome, MergeOutcome::OverlayRefused);
                prop_assert_eq!(after.call_bid, Some(pos(1.0)));
            }
        }

        /// No resurrection + bounded memory over arbitrary poll/stream
        /// interleavings: the present strike set always equals the last poll's,
        /// no tombstoned strike is present, and the pending buffer stays bounded.
        #[test]
        fn prop_no_resurrection_and_bounded_memory(ops in proptest::collection::vec(op_strategy(), 0..40)) {
            let all: BTreeSet<u32> = (0u32..5).collect();
            let mut store = ChainStore::seed(
                fetch_for(chain_of(&all), "deribit", AliasCatalog::new()),
                ChainSource::Merged,
                refresh(),
                utc(EXP),
            );
            let mut last_poll = all.clone();
            for (step, op) in (1i64..).zip(ops) {
                match op {
                    Op::Poll(indices) => {
                        let set: BTreeSet<u32> = indices.into_iter().collect();
                        store.apply_poll(
                            fetch_for(chain_of(&set), "deribit", AliasCatalog::new()),
                            utc(EXP + step),
                        );
                        last_poll = set;
                    }
                    Op::Quote(idx) => {
                        let _ = store.apply_quote(&quote(
                            "deribit",
                            strike_of(idx),
                            OptionStyle::Call,
                            Some(1.5),
                            Some(1.7),
                            None,
                            EXP + step,
                        ));
                    }
                }
            }
            for idx in 0u32..5 {
                let strike = pos(strike_of(idx));
                let present = store.contains_strike(strike);
                prop_assert_eq!(present, last_poll.contains(&idx));
                if present {
                    prop_assert!(!store.is_tombstoned(strike));
                }
            }
            prop_assert!(store.pending_len() <= MAX_PENDING);
        }

        /// Out-of-order safety: over any application order of updates with
        /// distinct event_times, the value with the maximum event_time wins —
        /// a late straggler never overwrites a fresher value.
        #[test]
        fn prop_freshness_out_of_order_keeps_max_event_value(
            order in Just((0u32..6).collect::<Vec<u32>>()).prop_shuffle()
        ) {
            let mut store = seed_single();
            for i in order {
                let bid = 1.0 + f64::from(i) * 0.5;
                let ask = bid + 0.5;
                let _ = store.apply_quote(&quote(
                    "deribit",
                    60_000.0,
                    OptionStyle::Call,
                    Some(bid),
                    Some(ask),
                    Some(EXP + i64::from(i)),
                    EXP + 200,
                ));
            }
            // The maximum event index is 5.
            prop_assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(1.0 + 5.0 * 0.5)));
        }
    }
}
