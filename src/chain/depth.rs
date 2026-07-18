//! The live order-book depth store: bounded per-instrument book slots with
//! `change_id` continuity tracking (`docs/01-domain-model.md` §5,
//! `docs/03-data-providers.md` §8).
//!
//! [`DepthStore`] folds each normalized [`DepthLadder`] a depth-capable provider
//! emits (`MarketUpdate::Depth`, Deribit option instruments today) into a slot
//! keyed by the ladder's provider-agnostic [`InstrumentKey`]. The depth screen
//! (`src/ui/depth.rs`) renders the book of the **selected** contract by looking it
//! up here — so the store keeps a book per subscribed leg, not one global slot.
//!
//! # This is DOMAIN code — no ratatui, no provider
//!
//! The store speaks the domain [`DepthLadder`] / [`InstrumentKey`] only; it never
//! imports a UI type or a provider adapter. Draw only *reads* a [`DepthBook`]; every
//! mutation happens on the market **event** ([`DepthStore::apply`]), never in `draw`
//! (`CLAUDE.md` "Module Boundaries").
//!
//! # Memory is bounded
//!
//! `book`s are capped at [`MAX_DEPTH_BOOKS`]: a new instrument beyond the cap is
//! **dropped and counted** ([`DepthStore::dropped_capacity`]) rather than growing
//! without bound, and an update for an already-tracked instrument overwrites its
//! slot in place (latest-value-wins, mirroring the coalescing channel), so the map
//! is bounded by the subscribed instrument universe of one `(provider, underlying,
//! expiry)` chain regardless of how many book frames stream.
//!
//! # `change_id` continuity — the gap/resync signal (`docs/03-data-providers.md` §8)
//!
//! Each [`DepthLadder`] carries the upstream [`change_id`](DepthLadder::change_id).
//! On every fold the store compares it against the last applied one via
//! [`depth_continues`] and records a [`DepthStatus`]: a sequence that **advances**
//! (or repeats) stays [`Fresh`](DepthStatus::Fresh); a sequence that **regresses**
//! (the venue re-seeded a fresh book after a resubscribe / reset) or that **loses**
//! its `change_id` flips to [`ResyncNeeded`](DepthStatus::ResyncNeeded), which the
//! screen surfaces as the "resyncing" state rather than trusting a discontinuous
//! book.
//!
//! This monotonic model is correct because the Deribit adapter subscribes the
//! **grouped** order-book channel (`book.{instrument}.{group}.{depth}.{interval}`,
//! `src/providers/deribit.rs`), where **every frame is a complete aggregated
//! snapshot** of the top levels — not a raw delta. A frame therefore never needs
//! merging against its predecessor: a forward `change_id` skip is a benignly
//! **coalesced snapshot** (the sink collapsed intermediate full books
//! latest-value-wins), so treating a forward skip as continuous never renders a
//! torn book. Only a **regression** (a venue re-seed) or a **lost** `change_id` is
//! a real discontinuity, and those are the resync triggers.
//!
//! The **resync action itself** — re-fetching the book snapshot when
//! [`ResyncNeeded`](DepthStatus::ResyncNeeded) is set — is the **adapter's** job and
//! lands with the Deribit book path (issue #51, `docs/03-data-providers.md` §5);
//! this module provides only the continuity model and the display signal it drives.
//!
//! **#51 hand-off note.** #51 adds TESTS for the grouped book path (its spec's Out
//! excludes changing this mechanism). Its scripted "a skip triggers resync"
//! acceptance line must be reconciled against these grouped-snapshot semantics: a
//! forward `change_id` skip is **benign** (a coalesced full snapshot), so it must
//! **not** trigger a resync — the resync triggers are a `change_id` **regression**
//! and a **lost** sequence. #51's brief should carry this correction.

use std::collections::HashMap;

use super::events::DepthLadder;
use super::identity::InstrumentKey;

/// The hard cap on tracked books, so the store can never grow without bound
/// (`docs/03-data-providers.md` §8). Default **1024** — far above the subscribed
/// leg count of one option chain (~80–200 contracts × call/put), so a real chain
/// never hits it; a pathological venue that streamed books for unlisted symbols is
/// bounded here.
pub const MAX_DEPTH_BOOKS: usize = 1024;

/// The `change_id` continuity status of a tracked [`DepthBook`]
/// (`docs/03-data-providers.md` §8).
///
/// A ChainView closed set matched exhaustively with no wildcard arm. Fieldless, so
/// `#[repr(u8)]` per the ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum DepthStatus {
    /// The `change_id` sequence is advancing — the book is current.
    #[default]
    Fresh,
    /// A `change_id` discontinuity was seen (a regression / lost sequence); a
    /// snapshot resync is needed (the adapter re-fetches in #51). The last ladder is
    /// still shown, badged "resyncing", rather than trusting a discontinuous book.
    ResyncNeeded,
}

/// One instrument's latest depth book: the newest [`DepthLadder`] plus its
/// [`DepthStatus`] (`docs/01-domain-model.md` §5).
#[derive(Debug, Clone)]
pub struct DepthBook {
    ladder: DepthLadder,
    status: DepthStatus,
}

impl DepthBook {
    /// The latest normalized ladder (best-first `bids` / `asks`), borrowed for the
    /// render.
    #[must_use]
    pub fn ladder(&self) -> &DepthLadder {
        &self.ladder
    }

    /// The book's `change_id` continuity status.
    #[must_use]
    pub fn status(&self) -> DepthStatus {
        self.status
    }

    /// Whether the book saw a `change_id` discontinuity and is awaiting a snapshot
    /// resync (the screen badges it "resyncing").
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        matches!(self.status, DepthStatus::ResyncNeeded)
    }

    /// The total number of price levels across both sides — the scroll bound the
    /// depth screen clamps against. The sum of two `Vec` lengths cannot overflow
    /// `usize` (memory would be exhausted long first), so a plain add is total here.
    #[must_use]
    pub fn level_count(&self) -> usize {
        self.ladder.bids.len() + self.ladder.asks.len()
    }
}

/// The bounded per-instrument depth store (`docs/01-domain-model.md` §5,
/// `docs/03-data-providers.md` §8).
#[derive(Debug, Clone, Default)]
pub struct DepthStore {
    /// The latest book per subscribed instrument, keyed by its provider-agnostic
    /// [`InstrumentKey`]. Bounded at [`MAX_DEPTH_BOOKS`].
    books: HashMap<InstrumentKey, DepthBook>,
    /// How many new-instrument book updates were dropped because the store was at
    /// [`MAX_DEPTH_BOOKS`] capacity — a monotonic counter, never resurrected.
    dropped_capacity: u64,
}

impl DepthStore {
    /// An empty depth store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one [`DepthLadder`] into the slot for its instrument, returning whether
    /// the update was **applied** (stored) — `true` for an in-place overwrite or a
    /// fresh seed, `false` when a *new* instrument is dropped at the
    /// [`MAX_DEPTH_BOOKS`] cap.
    ///
    /// An existing slot recomputes its [`DepthStatus`] from the `change_id`
    /// continuity ([`depth_continues`]) against the prior ladder, then overwrites in
    /// place (latest-value-wins). A first ladder for an instrument **seeds** the slot
    /// as [`DepthStatus::Fresh`] (a seed re-establishes the sequence). This performs
    /// no I/O and reads no wall clock.
    pub fn apply(&mut self, ladder: DepthLadder) -> bool {
        let key = ladder.instrument.key.clone();
        match self.books.get_mut(&key) {
            Some(book) => {
                book.status = if depth_continues(book.ladder.change_id, ladder.change_id) {
                    DepthStatus::Fresh
                } else {
                    DepthStatus::ResyncNeeded
                };
                book.ladder = ladder;
                true
            }
            None => {
                if self.books.len() >= MAX_DEPTH_BOOKS {
                    // A monotonic drop counter; hold at the current value on the
                    // (unreachable) u64 overflow rather than `saturating_add` (banned)
                    // — mirrors the reconnect loop's `attempt.checked_add(1)` idiom.
                    self.dropped_capacity = self
                        .dropped_capacity
                        .checked_add(1)
                        .unwrap_or(self.dropped_capacity);
                    return false;
                }
                let _ = self.books.insert(
                    key,
                    DepthBook {
                        ladder,
                        status: DepthStatus::Fresh,
                    },
                );
                true
            }
        }
    }

    /// The book for `key`, or `None` when no book has been received for it — the
    /// depth screen renders its "no book yet" empty state in that case.
    #[must_use]
    pub fn book(&self, key: &InstrumentKey) -> Option<&DepthBook> {
        self.books.get(key)
    }

    /// The number of tracked books.
    #[must_use]
    pub fn len(&self) -> usize {
        self.books.len()
    }

    /// Whether no book has been received yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }

    /// How many new-instrument updates were dropped at the [`MAX_DEPTH_BOOKS`] cap.
    #[must_use]
    pub fn dropped_capacity(&self) -> u64 {
        self.dropped_capacity
    }
}

/// Whether a book with `change_id` `next` **continues** the sequence after `prev`
/// (`docs/03-data-providers.md` §8) — the domain's coarse gap signal:
///
/// - `(Some(p), Some(n))` → `n >= p`: the sequence advances (or repeats a coalesced
///   value); a **regression** (`n < p`) means the venue re-seeded a fresh book (a
///   resubscribe / reset), a discontinuity requiring resync.
/// - `(Some(_), None)` → `false`: the sequence was tracked and is now **lost** — a
///   resync boundary.
/// - `(None, _)` → `true`: the first ladder, or a feed that carries no sequence at
///   all — there is no discontinuity to detect, so it never false-flags a resync.
///
/// This treats a **forward skip** as continuous, which is correct for the grouped
/// full-snapshot book channel the adapter subscribes: a coalesced snapshot the
/// channel dropped is not a gap, so a forward skip never spuriously flags a resync.
/// Only a regression or a lost sequence is a real discontinuity. The **re-fetch**
/// that repairs a flagged book is the adapter's job (#51).
#[must_use]
pub fn depth_continues(prev: Option<u64>, next: Option<u64>) -> bool {
    match (prev, next) {
        (Some(p), Some(n)) => n >= p,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use optionstratlib::OptionStyle;
    use optionstratlib::prelude::Positive;

    use super::*;
    use crate::chain::events::DepthLevel;
    use crate::chain::identity::{
        ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId,
        SettlementStyle,
    };

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("invalid provider id `{id}`: {e}"),
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

    fn key(strike: f64, style: OptionStyle) -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(1_700_000_000),
            strike: pos(strike),
            style,
        }
    }

    fn instrument(strike: f64, style: OptionStyle) -> Instrument {
        Instrument {
            key: key(strike, style),
            provider: pid("deribit"),
            native_symbol: "BTC-27JUN25-60000-C".to_owned(),
            stream_symbol: None,
            spec: ContractSpecFingerprint {
                contract_multiplier: 1,
                settlement: SettlementStyle::Cash,
                exercise: ExerciseStyle::European,
                quote_currency: "USD".to_owned(),
                venue_product_code: "BTC".to_owned(),
            },
        }
    }

    fn ladder(strike: f64, style: OptionStyle, change_id: Option<u64>) -> DepthLadder {
        DepthLadder {
            instrument: instrument(strike, style),
            bids: vec![
                DepthLevel {
                    price: pos(60_000.0),
                    size: pos(2.0),
                },
                DepthLevel {
                    price: pos(59_990.0),
                    size: pos(5.0),
                },
            ],
            asks: vec![DepthLevel {
                price: pos(60_010.0),
                size: pos(1.0),
            }],
            event_time: Some(utc(1_700_000_099)),
            received_time: utc(1_700_000_100),
            change_id,
        }
    }

    // --- The continuity model ------------------------------------------------

    #[test]
    fn test_depth_continues_advance_and_repeat_are_continuous() {
        assert!(depth_continues(Some(10), Some(11)), "advance continues");
        assert!(
            depth_continues(Some(10), Some(50)),
            "a forward skip (coalesced drop) still continues — no false resync",
        );
        assert!(depth_continues(Some(10), Some(10)), "a repeat continues");
    }

    #[test]
    fn test_depth_continues_regression_and_lost_sequence_are_gaps() {
        assert!(
            !depth_continues(Some(10), Some(9)),
            "a regression is a resync boundary",
        );
        assert!(
            !depth_continues(Some(10), None),
            "losing the sequence is a resync boundary",
        );
    }

    #[test]
    fn test_depth_continues_seed_and_sequenceless_never_flag() {
        assert!(depth_continues(None, Some(1)), "the first ladder seeds");
        assert!(
            depth_continues(None, None),
            "a feed with no sequence never flags a resync",
        );
    }

    // --- apply: seed / overwrite / gap ---------------------------------------

    #[test]
    fn test_apply_seeds_a_fresh_book() {
        let mut store = DepthStore::new();
        assert!(store.is_empty());
        assert!(store.apply(ladder(60_000.0, OptionStyle::Call, Some(1))));
        let book = match store.book(&key(60_000.0, OptionStyle::Call)) {
            Some(b) => b,
            None => panic!("the seeded book must be retrievable by its key"),
        };
        assert_eq!(book.status(), DepthStatus::Fresh, "a seed is fresh");
        assert!(!book.needs_resync());
        assert_eq!(book.level_count(), 3, "two bids + one ask");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_apply_overwrite_keeps_fresh_on_advancing_change_id() {
        let mut store = DepthStore::new();
        let _ = store.apply(ladder(60_000.0, OptionStyle::Call, Some(1)));
        let _ = store.apply(ladder(60_000.0, OptionStyle::Call, Some(5)));
        let book = store.book(&key(60_000.0, OptionStyle::Call));
        match book {
            Some(b) => {
                assert_eq!(b.status(), DepthStatus::Fresh, "advancing stays fresh");
                assert_eq!(b.ladder().change_id, Some(5), "the latest ladder wins");
            }
            None => panic!("book present"),
        }
        assert_eq!(store.len(), 1, "an overwrite does not grow the store");
    }

    #[test]
    fn test_apply_regression_flags_resync_then_clears_on_resume() {
        let mut store = DepthStore::new();
        let _ = store.apply(ladder(60_000.0, OptionStyle::Call, Some(10)));
        // A regression (the venue re-seeded) flags a resync.
        let _ = store.apply(ladder(60_000.0, OptionStyle::Call, Some(3)));
        match store.book(&key(60_000.0, OptionStyle::Call)) {
            Some(b) => assert!(b.needs_resync(), "a change_id regression flags resync"),
            None => panic!("book present"),
        }
        // The new sequence resumes advancing → the badge clears.
        let _ = store.apply(ladder(60_000.0, OptionStyle::Call, Some(4)));
        match store.book(&key(60_000.0, OptionStyle::Call)) {
            Some(b) => assert_eq!(
                b.status(),
                DepthStatus::Fresh,
                "an advancing resumed sequence clears the resync badge",
            ),
            None => panic!("book present"),
        }
    }

    #[test]
    fn test_apply_tracks_distinct_instruments_separately() {
        let mut store = DepthStore::new();
        let _ = store.apply(ladder(60_000.0, OptionStyle::Call, Some(1)));
        let _ = store.apply(ladder(60_000.0, OptionStyle::Put, Some(1)));
        assert_eq!(
            store.len(),
            2,
            "call and put at one strike are distinct books"
        );
        assert!(store.book(&key(60_000.0, OptionStyle::Call)).is_some());
        assert!(store.book(&key(60_000.0, OptionStyle::Put)).is_some());
        assert!(
            store.book(&key(62_000.0, OptionStyle::Call)).is_none(),
            "an unseen instrument has no book",
        );
    }

    #[test]
    fn test_apply_drops_new_instrument_at_capacity() {
        let mut store = DepthStore::new();
        // Fill to the cap with distinct strikes.
        for i in 0..MAX_DEPTH_BOOKS {
            let strike = 1_000.0 + i as f64;
            assert!(store.apply(ladder(strike, OptionStyle::Call, Some(1))));
        }
        assert_eq!(store.len(), MAX_DEPTH_BOOKS);
        // A brand-new instrument is dropped and counted, not stored.
        assert!(
            !store.apply(ladder(999_999.0, OptionStyle::Call, Some(1))),
            "a new instrument at capacity is dropped",
        );
        assert_eq!(store.len(), MAX_DEPTH_BOOKS, "the cap holds");
        assert_eq!(store.dropped_capacity(), 1);
        // An update for an ALREADY-tracked instrument still applies (no growth).
        assert!(
            store.apply(ladder(1_000.0, OptionStyle::Call, Some(2))),
            "an existing instrument still updates at capacity",
        );
        assert_eq!(store.len(), MAX_DEPTH_BOOKS);
    }
}
