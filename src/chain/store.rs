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
use super::greeks::{
    GreeksSidecar, LegGreeks, PricingInputs, QuoteClocks, compute_dirty_legs, compute_leg_greeks,
};
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
    /// `max(event_time)` of an applied update carrying a venue `event_time` — the
    /// out-of-order guard for timestamped ticks (§5.1). Advanced only on
    /// acceptance (see [`commit_watermark`]), never during the pre-check.
    watermark: Option<DateTime<Utc>>,
    /// `max(received_time)` of an applied update with **no** venue `event_time` —
    /// the receipt-order guard for timestamp-less ticks (§5.1), so they order by
    /// arrival instead of skipping the discipline. Advanced only on acceptance.
    receipt_watermark: Option<DateTime<Utc>>,
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
    /// The style-keyed local Greeks/IV analytics sidecar (`docs/01` §7). Filled on
    /// the market/tick event (never in `draw`) by [`compute_leg_greeks`], preserving
    /// venue iv/gamma per style and computing theta/vega/rho locally. `draw` only
    /// reads it through [`leg_greeks`](ChainStore::leg_greeks).
    sidecar: GreeksSidecar,
    /// The pricing-input cache key: bumped whenever an applied fold changes option
    /// data (a poll, an applied quote, or an applied Greeks row), so a recompute
    /// fires only when an input actually changed and is a cache no-op otherwise.
    input_generation: u64,
    /// The deterministic analytics reference instant — the last data-changing fold's
    /// timestamp — the sidecar prices time-to-expiry from. Never `Utc::now()`, so
    /// the local analytics stay reproducible (`docs/01` §7, issue #24).
    analytics_as_of: DateTime<Utc>,
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
        let mut store = Self {
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
            sidecar: GreeksSidecar::new(),
            input_generation: 1,
            analytics_as_of: now,
        };
        // Fill the local analytics for the seeded chain so the first frame renders
        // Greeks/IV before any stream update arrives (the initial poll is data).
        store.recompute_sidecar();
        store
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
        let delisted: HashSet<Positive> = self
            .chain
            .options
            .iter()
            .map(|o| o.strike_price)
            .filter(|strike| !new_strikes.contains(strike))
            .collect();
        for strike in &delisted {
            let _ = self.tombstones.insert(*strike);
        }
        // A strike that reappears is a genuine re-listing -> clear its tombstone.
        for strike in &new_strikes {
            let _ = self.tombstones.remove(strike);
        }
        // A de-listed strike leaves no residue: prune its per-instrument sidecar
        // and any overlay-refused mark, so both maps stay bounded by the live
        // ladder, never by the number of updates.
        if !delisted.is_empty() {
            self.instruments
                .retain(|key, _| !delisted.contains(&key.strike));
            self.overlay_refused
                .retain(|key| !delisted.contains(&key.strike));
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
        // A fresh structure (and any drained venue Greeks) is a data change: refresh
        // the local analytics once, deterministically, from the poll instant.
        self.on_option_data_changed(now);
    }

    /// Fold a [`QuoteUpdate`] into its row: gate a cross-provider overlay, drop an
    /// out-of-order update, then either patch the present row (crossed rejects the
    /// whole update), buffer an unknown strike, or drop a tombstoned one.
    pub fn apply_quote(&mut self, update: &QuoteUpdate) -> MergeOutcome {
        let key = &update.instrument.key;
        if let Some(outcome) = self.gate_overlay(key, &update.instrument.provider) {
            return outcome;
        }
        if self.is_out_of_order(key, update.event_time, update.received_time) {
            self.count_dropped_stale(key);
            return MergeOutcome::DroppedOutOfOrder;
        }
        let strike = key.strike;
        if self.contains_strike(strike) {
            if self.apply_quote_to_row(update) {
                let _ = self.overlay_refused.remove(key);
                // A new premium re-drives the local IV inversion / Greeks for THIS
                // leg only — never the whole chain (issue #25).
                self.on_leg_data_changed(update.received_time, key);
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
    /// The venue `iv`/`gamma` are additionally folded **per style** into the
    /// [`GreeksSidecar`], and the local `theta`/`vega`/`rho` — which have no
    /// [`OptionData`] field — are filled by the recompute the applied fold triggers
    /// (`docs/01-domain-model.md` §7, issue #24/#25).
    pub fn apply_greeks(&mut self, update: &GreeksRow) -> MergeOutcome {
        let key = &update.instrument.key;
        if let Some(outcome) = self.gate_overlay(key, &update.instrument.provider) {
            return outcome;
        }
        if self.is_out_of_order(key, update.event_time, update.received_time) {
            self.count_dropped_stale(key);
            return MergeOutcome::DroppedOutOfOrder;
        }
        let strike = key.strike;
        if self.contains_strike(strike) {
            self.apply_greeks_to_row(update);
            let _ = self.overlay_refused.remove(key);
            // New venue iv/gamma + a refreshed local theta/vega/rho fill for THIS
            // leg only — never the whole chain (issue #25).
            self.on_leg_data_changed(update.received_time, key);
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

    /// The style-keyed local analytics for one leg, or `None` when no entry exists
    /// (`docs/01-domain-model.md` §7). The read-only accessor the chain-matrix
    /// projection uses at draw time — it borrows the cached sidecar, mutates
    /// nothing, and triggers no pricing. The sidecar is refreshed only on a
    /// data-changing fold ([`apply_quote`](ChainStore::apply_quote) /
    /// [`apply_greeks`](ChainStore::apply_greeks) /
    /// [`apply_poll`](ChainStore::apply_poll)), never here.
    #[must_use]
    pub fn leg_greeks(&self, key: &InstrumentKey) -> Option<&LegGreeks> {
        self.sidecar.get(key)
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

    /// Get (or first-time create) the per-instrument sidecar. The owned
    /// [`InstrumentKey`] is cloned **only** on an instrument's first sighting; every
    /// later update on the hot merge path takes the `get_mut` branch with no
    /// allocation (the per-update path `bench_chain_merge` HP-3 targets). Reached
    /// only from the record path (on acceptance), so a rejected / buffered /
    /// tombstoned update never allocates a sidecar entry — the `instruments` map
    /// stays bounded by the live ladder, not by the update count.
    fn instrument_state_mut(&mut self, key: &InstrumentKey) -> &mut InstrumentState {
        if !self.instruments.contains_key(key) {
            self.instruments
                .insert(key.clone(), InstrumentState::default());
        }
        self.instruments
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("instrument state was just inserted"))
    }

    /// The per-instrument out-of-order guard (§5.1). **Read-only**: a never-seen
    /// key has no sidecar (and so no watermark) and is never out-of-order, without
    /// allocating a permanent entry during the pre-check. A timestamped update is
    /// ordered against the event-time watermark; a timestamp-less update is ordered
    /// by its `received_time` against the receipt watermark, so it follows the
    /// ordering discipline by arrival rather than skipping it. Neither watermark is
    /// advanced here — that happens only on acceptance (see [`commit_watermark`]),
    /// so a rejected update never advances it.
    fn is_out_of_order(
        &self,
        key: &InstrumentKey,
        event_time: Option<DateTime<Utc>>,
        received_time: DateTime<Utc>,
    ) -> bool {
        let Some(state) = self.instruments.get(key) else {
            return false;
        };
        match event_time {
            Some(event_time) => state.watermark.is_some_and(|w| event_time < w),
            None => state.receipt_watermark.is_some_and(|w| received_time < w),
        }
    }

    /// Increment one instrument's out-of-order tally. Only ever reached after
    /// [`is_out_of_order`](Self::is_out_of_order) returned `true`, which requires a
    /// watermark and therefore an existing sidecar — so this never allocates.
    fn count_dropped_stale(&mut self, key: &InstrumentKey) {
        if let Some(state) = self.instruments.get_mut(key) {
            // Checked increment with a self fallback (not `u64::MAX`), so it is a
            // checked op rather than a magic saturating call.
            state.dropped_stale = state
                .dropped_stale
                .checked_add(1)
                .unwrap_or(state.dropped_stale);
        }
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
    /// preserved. The shared upstream `implied_volatility`/`gamma` fields are still
    /// patched (the last style to arrive wins those single slots), but the
    /// **authoritative** per-leg iv/gamma are folded losslessly **per style** into
    /// the [`GreeksSidecar`] — so unequal call/put iv/gamma never collide, and the
    /// projection reads the sidecar, not the shared field (`docs/01` §7).
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
        // Preserve the venue iv/gamma per style (the recompute keeps them and fills
        // the local theta/vega/rho around them); venue theta/vega/rho are discarded.
        self.sidecar.apply_venue_greeks(update);
        self.record_greeks(key, update);
    }

    /// Record a quote's receipt clocks and advance its bid/ask direction baseline.
    fn record_quote(&mut self, key: &InstrumentKey, update: &QuoteUpdate) {
        let state = self.instrument_state_mut(key);
        state.quote_received = Some(update.received_time);
        commit_watermark(
            &mut state.watermark,
            &mut state.receipt_watermark,
            update.event_time,
            update.received_time,
        );
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
        commit_watermark(
            &mut state.watermark,
            &mut state.receipt_watermark,
            update.event_time,
            update.received_time,
        );
        if update.event_time.is_some() {
            state.greeks_event_time = update.event_time;
        }
    }

    /// Mark that a WHOLE-CHAIN fold changed the option data — the poll/seed path,
    /// where a fresh snapshot (new spot, re-listed strikes, drained venue Greeks)
    /// legitimately invalidates every leg. Advance the analytics reference instant
    /// to the fold's timestamp, bump the pricing-input cache key, and refresh the
    /// local analytics for the whole chain once. The timestamp is a store instant
    /// (the poll's own time), never the wall clock, so the fill stays deterministic
    /// (`docs/01` §7).
    ///
    /// A single applied stream update takes the O(1) [`on_leg_data_changed`] path
    /// instead — repricing the whole chain on every quote would put a full-chain
    /// Black-Scholes pass on the event fan-in and defeat the bounded-lag discipline
    /// the coalescing channel exists for (issue #25).
    fn on_option_data_changed(&mut self, as_of: DateTime<Utc>) {
        self.analytics_as_of = as_of;
        // Checked increment with a self fallback (not `u64::MAX`), so it is a checked
        // op rather than a banned saturating call.
        self.input_generation = self
            .input_generation
            .checked_add(1)
            .unwrap_or(self.input_generation);
        self.recompute_sidecar();
    }

    /// Mark that an applied stream fold changed exactly ONE leg's option data (a
    /// quote's premium or a Greeks row's venue iv/gamma): advance the analytics
    /// reference instant, bump the pricing-input cache key, and reprice ONLY that
    /// `(strike, style)` leg — never the whole chain (issue #25). The fold path is
    /// therefore O(changed legs), not O(chain), so a busy stream cannot block the
    /// event fan-in with a full-chain repricing per quote.
    ///
    /// The dirty leg is priced against the same inputs the full-chain pass would
    /// use (same spot, same `as_of`, same per-leg quote clock), so its analytics
    /// are bit-identical to a full recompute of that leg; the untouched legs keep
    /// their prior generation until the next poll legitimately reprices the chain.
    /// The timestamp is a store instant (the update's own `received_time`), never
    /// the wall clock, so the fill stays deterministic (`docs/01` §7).
    fn on_leg_data_changed(&mut self, as_of: DateTime<Utc>, key: &InstrumentKey) {
        self.analytics_as_of = as_of;
        self.input_generation = self
            .input_generation
            .checked_add(1)
            .unwrap_or(self.input_generation);
        self.recompute_leg(key);
    }

    /// Reprice the single dirty `(strike, style)` leg via the #24 engine, cached by
    /// the pricing `input_generation`. Hands the engine a one-entry
    /// [`QuoteClocks`] — only the dirty leg's own stream-quote receipt clock gates
    /// its local IV inversion, so building the snapshot stays O(1) rather than
    /// O(instruments) (the same per-leg [`StaleQuote`] semantics as the full pass,
    /// scoped to one leg). The fallible engine result is deliberately discarded for
    /// the same reason as [`recompute_sidecar`]: every per-leg outcome is recorded
    /// in the leg's status and there is no error to surface.
    ///
    /// [`StaleQuote`]: super::greeks::LegStatus::StaleQuote
    fn recompute_leg(&mut self, key: &InstrumentKey) {
        let ctx = PricingInputs::new(
            self.chain.underlying_price,
            self.analytics_as_of,
            self.input_generation,
        );
        // Only the dirty leg's own quote clock can gate its inversion, so snapshot
        // just that one clock (keyed exactly as the full pass keys it — by the
        // instrument's own key), never the whole `instruments` map.
        let mut clocks = QuoteClocks::new();
        if let Some(received) = self.instruments.get(key).and_then(|s| s.quote_received) {
            clocks.insert(key.clone(), received);
        }
        let dirty = [(key.strike, key.style)];
        let _ = compute_dirty_legs(&self.chain, &ctx, &clocks, &dirty, &mut self.sidecar);
    }

    /// A read-only snapshot of the per-instrument stream-quote receipt clocks —
    /// freshness as **data** for a caller-side gate (the payoff builder's
    /// commit-time `StaleMark` validation, #26), keyed exactly as the engine
    /// keys them. Only instruments with a recorded stream quote appear, so a
    /// poll-seeded leg is absent (ungated), mirroring the #24 convention.
    #[must_use]
    pub(crate) fn quote_clocks(&self) -> QuoteClocks {
        let mut clocks = QuoteClocks::new();
        for (key, state) in &self.instruments {
            if let Some(received) = state.quote_received {
                clocks.insert(key.clone(), received);
            }
        }
        clocks
    }

    /// The pricing reference instant of the latest analytics pass — the same
    /// `as_of` the #24 kernel gates freshness against, exposed so a caller-side
    /// staleness check (#26 commit validation) uses the identical clock domain.
    #[must_use]
    pub(crate) fn analytics_as_of(&self) -> DateTime<Utc> {
        self.analytics_as_of
    }

    /// Recompute the style-keyed local analytics for the whole chain via the #24
    /// engine, cached by the pricing `input_generation`. `compute_leg_greeks` records every
    /// per-leg outcome in the leg's status and returns `Ok(())` (a malformed,
    /// non-absolute chain expiry degrades to "no local analytics" internally), so
    /// the fallible result is deliberately discarded — there is no error to surface
    /// and no credential/panic path here.
    fn recompute_sidecar(&mut self) {
        let ctx = PricingInputs::new(
            self.chain.underlying_price,
            self.analytics_as_of,
            self.input_generation,
        );
        // Hand the engine a read-only snapshot of the per-instrument stream-quote
        // receipt clocks (#24): a leg whose quote went stale beyond the documented
        // threshold is NOT locally inverted (it becomes the honest `StaleQuote`
        // status), while a leg with no stream clock (poll-seeded) computes as
        // before. Freshness reaches the kernel as pure data, never a wall clock.
        let mut clocks = QuoteClocks::new();
        for (key, state) in &self.instruments {
            if let Some(received) = state.quote_received {
                clocks.insert(key.clone(), received);
            }
        }
        let _ = compute_leg_greeks(&self.chain, &ctx, &clocks, &mut self.sidecar);
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

/// Advance an accepted update's ordering watermark. A timestamped update raises
/// the event-time watermark by `max` (so a drained-pending straggler can never
/// lower it); a timestamp-less update raises the receipt-order watermark by its
/// `received_time`. Called **only** on acceptance (the
/// [`ChainStore::apply_quote`] / [`ChainStore::apply_greeks`] record paths), so a
/// rejected, buffered, or tombstoned update never advances either watermark
/// (§5.1).
fn commit_watermark(
    event_watermark: &mut Option<DateTime<Utc>>,
    receipt_watermark: &mut Option<DateTime<Utc>>,
    event_time: Option<DateTime<Utc>>,
    received_time: DateTime<Utc>,
) {
    match event_time {
        Some(event_time) => {
            *event_watermark = Some((*event_watermark).map_or(event_time, |w| w.max(event_time)));
        }
        None => {
            *receipt_watermark =
                Some((*receipt_watermark).map_or(received_time, |w| w.max(received_time)));
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
    use crate::chain::greeks::{LegGreeks, LegStatus};
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

    /// The absolute expiry the seeded chain resolves to — the instant the local
    /// analytics sidecar keys on (via `OptionChain::get_expiration`). The synthetic
    /// `InstrumentKey`s elsewhere in this module use `utc(EXP)`, but the sidecar
    /// keys on the chain's own resolved expiry, so a sidecar read (and a venue
    /// Greeks fold meant to survive the recompute) must use this instant.
    #[track_caller]
    fn resolved_exp(store: &ChainStore) -> chrono::DateTime<chrono::Utc> {
        match store.chain().get_expiration() {
            Some(optionstratlib::ExpirationDate::DateTime(dt)) => dt,
            other => panic!("expected an absolute-UTC chain expiry, got {other:?}"),
        }
    }

    /// A sidecar read key for `(strike, style)` at the store's resolved expiry.
    fn leg_key(store: &ChainStore, strike: f64, style: OptionStyle) -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: resolved_exp(store),
            strike: pos(strike),
            style,
        }
    }

    /// A venue Greeks row at an explicit expiry (so its key matches the sidecar's
    /// compute key). Carries venue theta/vega/rho that the sidecar deliberately
    /// discards.
    fn greeks_exp(
        exp: chrono::DateTime<chrono::Utc>,
        strike: f64,
        style: OptionStyle,
        iv: Option<f64>,
        delta: Option<Decimal>,
        gamma: Option<Decimal>,
    ) -> GreeksRow {
        GreeksRow {
            instrument: Instrument {
                key: InstrumentKey {
                    underlying: "BTC".to_owned(),
                    expiration_utc: exp,
                    strike: pos(strike),
                    style,
                },
                provider: pid("deribit"),
                native_symbol: format!("BTC-{strike}-{}", style.as_str()),
                stream_symbol: None,
                spec: spec(1),
            },
            iv: iv.map(pos),
            delta,
            gamma,
            theta: Some(dec(-9, 1)),
            vega: Some(dec(8, 1)),
            rho: Some(dec(7, 1)),
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

    /// A two-strike seed (60000 + 61000, both two-sided call/put) — the fixture for
    /// the dirty-recompute proofs, where one leg is streamed and the untouched legs
    /// are checked for identity.
    fn seed_two() -> ChainStore {
        let chain = chain_with(&[
            row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4)),
            row(61_000.0, Some(1.5), Some(1.8), Some(2.5), Some(2.9)),
        ]);
        ChainStore::seed(
            fetch_for(chain, "deribit", AliasCatalog::new()),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        )
    }

    /// The sidecar analytics for one `(strike, style)` leg at the store's resolved
    /// expiry, or a panic naming the missing leg.
    #[track_caller]
    fn leg_of(store: &ChainStore, strike: f64, style: OptionStyle) -> LegGreeks {
        match store.leg_greeks(&leg_key(store, strike, style)) {
            Some(g) => *g,
            None => panic!("expected a sidecar entry for strike {strike} {style:?}"),
        }
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

    // --- Local analytics sidecar: seeded, folded, and generation-cached ------

    #[test]
    fn test_sidecar_seeded_fills_local_theta_vega_rho() {
        // The seed poll is data: `compute_leg_greeks` fills local theta/vega/rho and
        // inverts IV locally for the seeded strike, before any stream update.
        let store = seed_single();
        let leg = match store.leg_greeks(&leg_key(&store, 60_000.0, OptionStyle::Call)) {
            Some(g) => *g,
            None => panic!("expected a seeded sidecar entry"),
        };
        assert!(leg.theta.is_some());
        assert!(leg.vega.is_some());
        assert!(leg.rho.is_some());
        assert_eq!(leg.theta_origin, GreeksOrigin::ComputedLocally);
        assert_eq!(leg.vega_origin, GreeksOrigin::ComputedLocally);
        // No venue Greeks yet, so IV/gamma were inverted/computed locally.
        assert_eq!(leg.iv_origin, GreeksOrigin::ComputedLocally);
        assert_eq!(leg.gamma_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_apply_greeks_folds_venue_iv_gamma_into_sidecar_per_style() {
        let mut store = seed_single();
        let exp = resolved_exp(&store);
        let outcome = store.apply_greeks(&greeks_exp(
            exp,
            60_000.0,
            OptionStyle::Call,
            Some(0.42),
            Some(dec(-6, 1)),
            Some(dec(1, 2)),
        ));
        assert_eq!(outcome, MergeOutcome::Applied);
        let call = match store.leg_greeks(&leg_key(&store, 60_000.0, OptionStyle::Call)) {
            Some(g) => *g,
            None => panic!("expected a call sidecar entry"),
        };
        // Venue iv/gamma preserved with Provider origin; venue theta/vega/rho are
        // discarded and refilled locally around them.
        assert_eq!(call.iv, Some(pos(0.42)));
        assert_eq!(call.iv_origin, GreeksOrigin::Provider);
        assert_eq!(call.gamma, Some(dec(1, 2)));
        assert_eq!(call.gamma_origin, GreeksOrigin::Provider);
        assert!(call.theta.is_some());
        assert_eq!(call.theta_origin, GreeksOrigin::ComputedLocally);
        // The put leg keeps its own independent (locally computed) entry — no
        // call/put collision on the shared upstream iv/gamma slots.
        let put = match store.leg_greeks(&leg_key(&store, 60_000.0, OptionStyle::Put)) {
            Some(g) => *g,
            None => panic!("expected a put sidecar entry"),
        };
        assert_eq!(put.gamma_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_input_generation_bumps_only_on_applied_data_change() {
        let mut store = seed_single();
        let gen_seed = store.input_generation;
        // The seed recompute already ran and cached this generation.
        assert_eq!(store.sidecar.computed_generation(), Some(gen_seed));
        let before = match store.leg_greeks(&leg_key(&store, 60_000.0, OptionStyle::Call)) {
            Some(g) => *g,
            None => panic!("expected a seed entry"),
        };

        // A crossed quote is dropped: no data change, no generation bump, no
        // recompute — the cached analytics are untouched (the cache no-op).
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(2.0),
                Some(1.5),
                None,
                EXP + 100
            )),
            MergeOutcome::DroppedCrossed
        );
        assert_eq!(
            store.input_generation, gen_seed,
            "a dropped update does not bump the pricing generation"
        );
        let after = match store.leg_greeks(&leg_key(&store, 60_000.0, OptionStyle::Call)) {
            Some(g) => *g,
            None => panic!("expected a seed entry"),
        };
        assert_eq!(
            before, after,
            "a no-op fold leaves the cached analytics intact"
        );

        // An applied quote bumps the generation and re-fills the cache to match.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.4),
                Some(1.6),
                None,
                EXP + 101
            )),
            MergeOutcome::Applied
        );
        assert!(store.input_generation > gen_seed);
        assert_eq!(
            store.sidecar.computed_generation(),
            Some(store.input_generation)
        );
    }

    // --- Dirty recompute: an applied quote reprices only the affected leg -----

    #[test]
    fn test_applied_quote_recomputes_only_the_dirty_leg() {
        let mut store = seed_two();
        // Snapshot every seeded leg BEFORE the stream update.
        let k1_call_before = leg_of(&store, 60_000.0, OptionStyle::Call);
        let k1_put_before = leg_of(&store, 60_000.0, OptionStyle::Put);
        let k2_call_before = leg_of(&store, 61_000.0, OptionStyle::Call);
        let k2_put_before = leg_of(&store, 61_000.0, OptionStyle::Put);
        let gen_before = store.input_generation;

        // A stream quote for ONE leg (60000 Call) with a far-future receipt, so the
        // analytics reference instant jumps ~460 days: a FULL recompute at this
        // as-of would visibly shrink EVERY other leg's time-to-expiry (its
        // theta/vega/rho). Only the dirty leg may change.
        let outcome = store.apply_quote(&quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.4),
            Some(1.6),
            None,
            EXP + 40_000_000,
        ));
        assert_eq!(outcome, MergeOutcome::Applied);

        // The pricing generation advanced and the sidecar records it — a full
        // recompute at this generation WOULD have refreshed every leg.
        assert!(store.input_generation > gen_before);
        assert_eq!(
            store.sidecar.computed_generation(),
            Some(store.input_generation)
        );

        // The dirty leg was repriced (new premium + new as-of).
        assert_ne!(leg_of(&store, 60_000.0, OptionStyle::Call), k1_call_before);
        // Its opposite style and every other strike are byte-identical: the scoped
        // recompute never touched them despite the large as-of jump. This is the
        // O(changed legs), not O(chain), proof — under the old full recompute all
        // three would have changed with the advanced reference instant.
        assert_eq!(leg_of(&store, 60_000.0, OptionStyle::Put), k1_put_before);
        assert_eq!(leg_of(&store, 61_000.0, OptionStyle::Call), k2_call_before);
        assert_eq!(leg_of(&store, 61_000.0, OptionStyle::Put), k2_put_before);
    }

    #[test]
    fn test_poll_still_recomputes_every_leg() {
        let mut store = seed_two();
        let k2_call_before = leg_of(&store, 61_000.0, OptionStyle::Call);
        let k2_put_before = leg_of(&store, 61_000.0, OptionStyle::Put);

        // A fresh poll legitimately invalidates every leg: the full-chain recompute
        // reprices ALL strikes at the new poll instant, so even a leg that received
        // no stream update changes (its time-to-expiry shrank ~460 days). The
        // dirty-recompute optimization is confined to the stream path.
        store.apply_poll(
            fetch_for(
                chain_with(&[
                    row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4)),
                    row(61_000.0, Some(1.5), Some(1.8), Some(2.5), Some(2.9)),
                ]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(EXP + 40_000_000),
        );

        assert_ne!(leg_of(&store, 61_000.0, OptionStyle::Call), k2_call_before);
        assert_ne!(leg_of(&store, 61_000.0, OptionStyle::Put), k2_put_before);
    }

    #[test]
    fn test_scoped_recompute_matches_full_recompute_for_dirty_leg() {
        // Faithfulness + correct rendering: the dirty leg's analytics after a scoped
        // recompute must equal what a full-chain recompute would produce for the
        // same premium and as-of, and every other leg must keep valid Greeks.
        let received = EXP + 1_000;
        let mut scoped = seed_two();
        assert_eq!(
            scoped.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.4),
                Some(1.6),
                None,
                received,
            )),
            MergeOutcome::Applied
        );
        let scoped_leg = leg_of(&scoped, 60_000.0, OptionStyle::Call);
        // The dirty leg is a complete, valid computed leg (nothing cleared).
        assert_eq!(scoped_leg.status, LegStatus::Computed);
        assert!(scoped_leg.iv.is_some());
        assert!(scoped_leg.delta.is_some());
        assert!(scoped_leg.theta.is_some());
        assert!(scoped_leg.vega.is_some());
        assert!(scoped_leg.rho.is_some());

        // Force a FULL recompute of the same post-quote chain at the same as-of via
        // a poll carrying the patched premium; the dirty leg must match bit-for-bit.
        let mut full = seed_two();
        full.apply_poll(
            fetch_for(
                chain_with(&[
                    row(60_000.0, Some(1.4), Some(1.6), Some(2.0), Some(2.4)),
                    row(61_000.0, Some(1.5), Some(1.8), Some(2.5), Some(2.9)),
                ]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(received),
        );
        assert_eq!(scoped_leg, leg_of(&full, 60_000.0, OptionStyle::Call));

        // The matrix still renders valid Greeks for every OTHER leg after the
        // partial recompute — touching one leg cleared nothing.
        assert_eq!(
            leg_of(&scoped, 61_000.0, OptionStyle::Call).status,
            LegStatus::Computed
        );
        assert_eq!(
            leg_of(&scoped, 60_000.0, OptionStyle::Put).status,
            LegStatus::Computed
        );
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

    #[test]
    fn test_rejected_update_does_not_advance_watermark() {
        let mut store = seed_single();
        // A applies and sets the event-time watermark to 50.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.0),
                Some(1.2),
                Some(EXP + 50),
                EXP + 100
            )),
            MergeOutcome::Applied
        );
        // B is crossed (ask < bid) and carries a LATER event_time; it is rejected
        // and must NOT advance the watermark past 50.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(2.0),
                Some(1.5),
                Some(EXP + 60),
                EXP + 101
            )),
            MergeOutcome::DroppedCrossed
        );
        // C is a valid tick earlier than B but still >= 50; because B did not
        // advance the watermark, C is a forward tick and applies (under the bug the
        // watermark would sit at 60 and drop C as stale).
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.3),
                Some(1.5),
                Some(EXP + 55),
                EXP + 102
            )),
            MergeOutcome::Applied
        );
        let after = find_row(&store, 60_000.0);
        assert_eq!(after.call_bid, Some(pos(1.3)));
        assert_eq!(after.call_ask, Some(pos(1.5)));
    }

    #[test]
    fn test_no_timestamp_updates_apply_in_receipt_order() {
        let mut store = seed_single();
        // The first timestamp-less update applies and sets the receipt watermark.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.5),
                Some(1.7),
                None,
                EXP + 102
            )),
            MergeOutcome::Applied
        );
        // A timestamp-less update that ARRIVED earlier (lower received_time) is an
        // out-of-order straggler by receipt and is dropped, not applied.
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
            MergeOutcome::DroppedOutOfOrder
        );
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(1.5)));
        // A later-arriving timestamp-less update applies (latest-value-wins).
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(2.5),
                Some(2.7),
                None,
                EXP + 103
            )),
            MergeOutcome::Applied
        );
        assert_eq!(find_row(&store, 60_000.0).call_bid, Some(pos(2.5)));
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
    fn test_rejected_key_burst_does_not_grow_instrument_sidecar() {
        let mut store = seed_single(); // only 60000 is listed
        // A burst of updates for never-listed strikes, each carrying a venue
        // event_time, are buffered (unknown strike) then dropped on overflow. The
        // ordering pre-check must not allocate a sidecar for a never-accepted key,
        // so `instruments` stays empty no matter how many keys stream through.
        for i in 0u32..300 {
            let outcome = store.apply_quote(&quote(
                "deribit",
                70_000.0 + f64::from(i),
                OptionStyle::Call,
                Some(1.0),
                Some(1.2),
                Some(EXP + 200 + i64::from(i)),
                EXP + 200 + i64::from(i),
            ));
            assert_eq!(outcome, MergeOutcome::Buffered);
        }
        // The pending buffer is capped and the sidecar never grew for the rejected
        // keys — under the bug it would hold 300 entries, one per rejected key.
        assert_eq!(store.pending_len(), MAX_PENDING);
        assert_eq!(store.instruments.len(), 0);
        // Only an ACCEPTED update for a listed strike creates sidecar state.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                60_000.0,
                OptionStyle::Call,
                Some(1.0),
                Some(1.2),
                Some(EXP + 600),
                EXP + 600
            )),
            MergeOutcome::Applied
        );
        assert_eq!(store.instruments.len(), 1);
    }

    #[test]
    fn test_delisted_strike_prunes_instrument_sidecar() {
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
        // Give 61000 sidecar state via an accepted update.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                61_000.0,
                OptionStyle::Call,
                Some(1.5),
                Some(1.7),
                Some(EXP + 10),
                EXP + 100
            )),
            MergeOutcome::Applied
        );
        assert_eq!(store.instruments.len(), 1);
        // Poll away 61000 -> tombstone AND prune its sidecar residue.
        store.apply_poll(
            fetch_for(
                chain_with(&[row(60_000.0, Some(1.0), Some(1.2), Some(2.0), Some(2.4))]),
                "deribit",
                AliasCatalog::new(),
            ),
            utc(EXP + 2),
        );
        assert!(store.is_tombstoned(pos(61_000.0)));
        assert_eq!(store.instruments.len(), 0);
        // A later update for the de-listed strike is dropped and does not recreate
        // sidecar state.
        assert_eq!(
            store.apply_quote(&quote(
                "deribit",
                61_000.0,
                OptionStyle::Call,
                Some(9.0),
                Some(9.2),
                Some(EXP + 11),
                EXP + 103
            )),
            MergeOutcome::DroppedTombstoned
        );
        assert_eq!(store.instruments.len(), 0);
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
