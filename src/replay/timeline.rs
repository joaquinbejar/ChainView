//! The replay **timeline cursor** — the scrub model that turns an integer
//! `step` position into "what is realized as of now" across the four validated
//! bundle tables (`docs/04-replay-mode.md` §4,
//! `docs/01-domain-model.md` §10).
//!
//! [`TimelineCursor`] holds the current scrub [`position`](TimelineCursor::position)
//! plus one per-table index (`fills_ix`, `equity_ix`, `positions_ix`,
//! `greeks_ix`), and nothing else — it is [`Copy`], borrows no table, and
//! duplicates no row. Every query takes the [`LoadedBundle`] the cursor was built
//! from and returns a **borrowed slice** (or a small derived `Vec` of borrows), so
//! scrubbing is allocation-light on the hot path and the render stays a pure
//! projection.
//!
//! # The one clock is `step`
//!
//! The canonical position is the integer `step` column shared by every table;
//! `ts_ns` is **display only** and is never a seek or sort unit
//! (`docs/01-domain-model.md` §10). A [`SeekTo`] is expressed against `step`.
//!
//! # Two seek paths, one invariant
//!
//! [`TimelineCursor::seek`] resolves each per-table index so it satisfies the
//! single invariant **"the index is the count of rows with `step <= position`"**
//! (equivalently, [`slice::partition_point`] of `step <= position`), by one of two
//! paths:
//!
//! - an **incremental** [`SeekTo::StepBy`] (`←`/`→`, or a playback tick) walks each
//!   index ±1 from its current value — **O(rows moved)**, never a rescan from
//!   zero: a single-step scrub is O(1) on the one-row-per-step `equity`/`greeks`
//!   clocks and O(rows on the crossed step) for the sparse `fills`/`positions`;
//! - an **arbitrary** [`SeekTo::Step`] (`Home`/`End`/a large jump) resolves each
//!   index by [`slice::partition_point`] **binary search** — **O(log n)**.
//!
//! Both land on the *same* index for the same clamped `position`, so stepping
//! incrementally from `0` to any step `S` yields byte-identical cursor state to
//! seeking directly to `S` (the `incremental_equals_arbitrary` property). There is
//! **no** O(1) arbitrary seek and this module does not claim one.
//!
//! # Within-step phase — post-fill as-of
//!
//! A `step` slice is the state **after** that step's fills executed. IronCondor
//! writes a leg's **terminal** `positions` row (still positive quantity, carrying
//! `exit_reason`) at its closing step, immediately before the separate closing
//! fill. [`open_positions`](TimelineCursor::open_positions) is therefore the
//! **latest non-terminal `positions` row per `position_id` with `step <=
//! position`, excluding any `position_id` whose terminal (`exit_reason`-bearing)
//! row is at `step <= position`**. A leg closed at or before the cursor is not
//! shown open (and is excluded from the deferred payoff, #49); a leg with
//! `open_at_end = true` and no terminal row stays open through `end_step`. This is
//! the *single* algorithm for open positions, selection, and payoff, so opening
//! and closing at the same step resolve deterministically — the terminal row wins
//! the exclusion (`docs/04-replay-mode.md` §4).
//!
//! # Playback
//!
//! [`Playback`] models pause/play at [`PlaybackSpeed`] `×1/×2/×5/×10`; one
//! scheduled tick advances `position` by the speed's quantum
//! ([`Playback::tick_seek`] → [`SeekTo::StepBy`]), and the cursor's clamp makes
//! playback **stop at `end_step` without wrapping**. This module models the
//! quantum and the stop-at-end rule only — the tick cadence and the
//! `AppEvent::ReplaySeek` fan-in are the app's (issue #34).
//!
//! # Invariants relied on (verified by the #32 validation chain, never re-checked)
//!
//! The cursor consumes an already-validated [`LoadedBundle`] and **relies** on the
//! load-time guarantees of `docs/04-replay-mode.md` §5, so it re-validates
//! nothing:
//!
//! 1. **`equity_curve` / `greeks_attribution` are one row per step over a
//!    contiguous 0-based domain `0..N` sharing the same `N`** (§5 step 7). The
//!    cursor derives [`end_step`](TimelineCursor::end_step) as `N - 1` and treats
//!    the last visible `equity`/`greeks` row as the `step == position` row
//!    ([`head_equity`](TimelineCursor::head_equity),
//!    [`head_greeks`](TimelineCursor::head_greeks)).
//! 2. **Every table is non-decreasing on its stated step sort key** (§5 step 4).
//!    [`slice::partition_point`] and the incremental walk are correct only on a
//!    step-sorted table.
//! 3. **Every `fills` / `positions` step is a member of `0..N`** (§5 step 7), so a
//!    `position` clamped to `0..=end_step` spans the whole table domain.
//! 4. **`position_id` maps to a stable leg identity and every `quantity > 0`**
//!    (§5 steps 8/10), so [`open_positions`](TimelineCursor::open_positions) groups
//!    by `position_id` as a stable key and every row is a real leg.

use std::collections::{BTreeMap, BTreeSet};

use crate::event::SeekTo;

use super::{EquityPoint, Fill, GreeksAttribution, LoadedBundle, PositionRow};

// ---------------------------------------------------------------------------
// Playback model (`docs/04-replay-mode.md` §4).
// ---------------------------------------------------------------------------

/// A selectable playback speed — the multiplier applied to the one-`step`
/// playback quantum (`docs/04-replay-mode.md` §4). A tick at `×N` advances the
/// scrub head by `N` steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PlaybackSpeed {
    /// One step per scheduled advance (the default).
    #[default]
    X1,
    /// Two steps per scheduled advance.
    X2,
    /// Five steps per scheduled advance.
    X5,
    /// Ten steps per scheduled advance.
    X10,
}

impl PlaybackSpeed {
    /// Every speed, slowest to fastest — for a UI speed picker.
    pub const ALL: [Self; 4] = [Self::X1, Self::X2, Self::X5, Self::X10];

    /// The positive step multiplier (`1`, `2`, `5`, or `10`) — the display value
    /// for a `×N` badge. Always small (fits every integer type).
    #[must_use]
    pub const fn multiplier(self) -> u32 {
        match self {
            Self::X1 => 1,
            Self::X2 => 2,
            Self::X5 => 5,
            Self::X10 => 10,
        }
    }

    /// The signed forward step delta one playback tick applies at this speed — the
    /// [`SeekTo::StepBy`] quantum (`+1/+2/+5/+10`). Returned as `i32` directly (no
    /// cast) so the seek stays checked end to end.
    #[must_use]
    pub const fn quantum(self) -> i32 {
        match self {
            Self::X1 => 1,
            Self::X2 => 2,
            Self::X5 => 5,
            Self::X10 => 10,
        }
    }

    /// The next faster speed, **clamped at `×10`** (it does not wrap) — so a
    /// speed-up key can be held without cycling back to `×1`.
    #[must_use]
    pub const fn faster(self) -> Self {
        match self {
            Self::X1 => Self::X2,
            Self::X2 => Self::X5,
            Self::X5 | Self::X10 => Self::X10,
        }
    }

    /// The next slower speed, **clamped at `×1`** (it does not wrap).
    #[must_use]
    pub const fn slower(self) -> Self {
        match self {
            Self::X10 => Self::X5,
            Self::X5 => Self::X2,
            Self::X2 | Self::X1 => Self::X1,
        }
    }
}

/// The replay playback state (`docs/04-replay-mode.md` §4): paused, or playing at
/// a [`PlaybackSpeed`]. Playback **stops at `end_step`** and never wraps — that
/// clamp lives in [`TimelineCursor::seek`], so this type models the quantum and
/// the play/pause transition only, not the tick timer (the tick cadence is the
/// app's, issue #34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Playback {
    /// Playback is paused (the default on load).
    #[default]
    Paused,
    /// Playback is running at the given speed.
    Playing {
        /// The active speed multiplier.
        speed: PlaybackSpeed,
    },
}

impl Playback {
    /// Start playback at `speed`.
    #[must_use]
    pub const fn playing(speed: PlaybackSpeed) -> Self {
        Self::Playing { speed }
    }

    /// Whether playback is currently running.
    #[must_use]
    pub const fn is_playing(self) -> bool {
        matches!(self, Self::Playing { .. })
    }

    /// The active [`PlaybackSpeed`], or `None` while paused.
    #[must_use]
    pub const fn speed(self) -> Option<PlaybackSpeed> {
        match self {
            Self::Paused => None,
            Self::Playing { speed } => Some(speed),
        }
    }

    /// Toggle between paused and playing: pause when playing, or resume at
    /// `resume_speed` when paused. The pure state transition a play/pause key
    /// drives (issue #34 wires the key).
    #[must_use]
    pub const fn toggled(self, resume_speed: PlaybackSpeed) -> Self {
        match self {
            Self::Paused => Self::Playing {
                speed: resume_speed,
            },
            Self::Playing { .. } => Self::Paused,
        }
    }

    /// The [`SeekTo`] one scheduled playback tick applies to the cursor, or `None`
    /// while paused. Playing at `×N` yields [`SeekTo::StepBy`]`(N)`; the cursor
    /// clamps the result to `end_step`, so playback **stops at the end of the tape
    /// and never wraps** (`docs/04-replay-mode.md` §4). This is the quantum only —
    /// the tick cadence is the app's (issue #34).
    #[must_use]
    pub const fn tick_seek(self) -> Option<SeekTo> {
        match self {
            Self::Paused => None,
            Self::Playing { speed } => Some(SeekTo::StepBy(speed.quantum())),
        }
    }
}

// ---------------------------------------------------------------------------
// TimelineCursor (`docs/01-domain-model.md` §10).
// ---------------------------------------------------------------------------

/// The scrub position over a validated [`LoadedBundle`]: the current integer
/// `step` plus one per-table index for "as of `position`" slicing
/// (`docs/01-domain-model.md` §10).
///
/// The cursor is [`Copy`] and holds **only** indices — it never borrows or copies
/// a table, so it is cheap to snapshot (the `incremental_equals_arbitrary`
/// property compares two cursors by value). Every query takes the same
/// [`LoadedBundle`] it was constructed from and returns borrowed rows; passing a
/// *different* bundle yields meaningless indices (a caller contract, not
/// re-validated).
///
/// [`position`](Self::position) and [`end_step`](Self::end_step) are read-only
/// ACCESSORS (a screen renders "step `position` of `end_step`"); the fields are
/// private so the cursor can only move through [`seek`](Self::seek), which
/// re-establishes the index invariant — a desynced direct write is
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineCursor {
    /// The last step of the tape (the first is always `0`). For an empty run this
    /// is `0` and every slice is empty.
    end_step: u32,
    /// The current scrub step — the one clock (`ts_ns` is display only). Always in
    /// `0..=end_step` after any [`seek`](Self::seek).
    position: u32,
    /// Count of `fills` rows with `step <= position` (the exclusive upper bound of
    /// the visible slice `&fills[..fills_ix]`).
    fills_ix: usize,
    /// Count of `equity` rows with `step <= position`.
    equity_ix: usize,
    /// Count of `positions` rows with `step <= position`.
    positions_ix: usize,
    /// Count of `greeks` rows with `step <= position`.
    greeks_ix: usize,
}

impl TimelineCursor {
    /// The last step of the tape (the first is always `0`).
    #[must_use]
    pub fn end_step(&self) -> u32 {
        self.end_step
    }

    /// The current scrub step — always in `0..=end_step` after any
    /// [`seek`](Self::seek).
    #[must_use]
    pub fn position(&self) -> u32 {
        self.position
    }

    /// Build a cursor over `bundle` at `position = 0` with each index resolved by
    /// binary search (`docs/01-domain-model.md` §10).
    ///
    /// [`end_step`](Self::end_step) is `N - 1` for an `N`-step run and `0` for an
    /// empty run — relying on the #32 guarantee that `equity_curve` is one row per
    /// step over the contiguous domain `0..N` (module docs, reliance 1).
    #[must_use]
    pub fn new(bundle: &LoadedBundle) -> Self {
        let mut cursor = Self {
            end_step: end_step_of(bundle),
            position: 0,
            fills_ix: 0,
            equity_ix: 0,
            positions_ix: 0,
            greeks_ix: 0,
        };
        cursor.resolve_arbitrary(bundle);
        cursor
    }

    /// Whether the cursor is at the first step (`position == 0`).
    #[must_use]
    pub const fn is_at_start(self) -> bool {
        self.position == 0
    }

    /// Whether the cursor is at the last step (`position == end_step`) — the point
    /// at which playback stops.
    #[must_use]
    pub const fn is_at_end(self) -> bool {
        self.position >= self.end_step
    }

    /// Move the cursor by `to`, re-establishing every per-table index for the new
    /// clamped `position` (`docs/04-replay-mode.md` §4).
    ///
    /// - [`SeekTo::StepBy`] walks each index **incrementally** from its current
    ///   value (O(rows moved), never a rescan) — the O(1) play-head / `←`/`→` path;
    /// - [`SeekTo::Step`] resolves each index by **binary search** (O(log n)) — the
    ///   `Home`/`End`/large-jump path.
    ///
    /// The target `position` is clamped to `0..=end_step` with checked arithmetic,
    /// so a step past either edge (including a playback tick at the end) is a no-op
    /// clamp, never an overflow or a wrap. Both paths land on the same index
    /// invariant, so the result depends only on the final `position`.
    pub fn seek(&mut self, to: SeekTo, bundle: &LoadedBundle) {
        match to {
            SeekTo::Step(target) => {
                self.position = target.min(self.end_step);
                self.resolve_arbitrary(bundle);
            }
            SeekTo::StepBy(delta) => {
                self.position = self.clamped_step_by(delta);
                self.resolve_incremental(bundle);
            }
        }
    }

    /// Apply one scheduled **playback advance**: seek forward by `playback`'s
    /// quantum ([`Playback::tick_seek`]), or leave the cursor unchanged when paused
    /// (`docs/04-replay-mode.md` §4). The forward step is a [`SeekTo::StepBy`], so
    /// the seek clamps at [`end_step`](Self::end_step) — playback **stops at the end
    /// of the tape and never wraps**. This is the model of the quantum and the
    /// stop-at-end rule; the tick cadence that calls it is the app's (issue #34).
    pub fn advance_playback(&mut self, playback: Playback, bundle: &LoadedBundle) {
        if let Some(seek) = playback.tick_seek() {
            self.seek(seek, bundle);
        }
    }

    /// The clamped `position + delta`, computed in `i64` so `position ± delta`
    /// cannot overflow at either edge, then clamped to `0..=end_step`.
    fn clamped_step_by(&self, delta: i32) -> u32 {
        let raw = i64::from(self.position) + i64::from(delta);
        let clamped = raw.clamp(0, i64::from(self.end_step));
        // `clamped` is in `0..=end_step`, so it always fits `u32`; the fallback is
        // unreachable and keeps the conversion checked (never an `as` cast).
        u32::try_from(clamped).unwrap_or(self.end_step)
    }

    /// Resolve every index by binary search on its step-sorted table — the
    /// arbitrary O(log n) seek (relies on module reliance 2, step-sorted tables).
    fn resolve_arbitrary(&mut self, bundle: &LoadedBundle) {
        let pos = self.position;
        self.fills_ix = bundle.fills.partition_point(|f| f.step <= pos);
        self.equity_ix = bundle.equity.partition_point(|e| e.step <= pos);
        self.positions_ix = bundle.positions.partition_point(|p| p.step <= pos);
        self.greeks_ix = bundle.greeks.partition_point(|g| g.step <= pos);
    }

    /// Resolve every index by walking ±1 from its current value — the incremental
    /// O(rows moved) seek, never a rescan (relies on module reliance 2).
    fn resolve_incremental(&mut self, bundle: &LoadedBundle) {
        let pos = self.position;
        self.fills_ix = walk_index(&bundle.fills, self.fills_ix, pos, |f| f.step).0;
        self.equity_ix = walk_index(&bundle.equity, self.equity_ix, pos, |e| e.step).0;
        self.positions_ix = walk_index(&bundle.positions, self.positions_ix, pos, |p| p.step).0;
        self.greeks_ix = walk_index(&bundle.greeks, self.greeks_ix, pos, |g| g.step).0;
    }

    /// The equity curve up to and including the head step — the `drawdown`-shaded
    /// series the equity screen renders (`docs/04-replay-mode.md` §4).
    #[must_use]
    pub fn visible_equity<'b>(&self, bundle: &'b LoadedBundle) -> &'b [EquityPoint] {
        bundle.equity.get(..self.equity_ix).unwrap_or(&[])
    }

    /// The per-step P&L attribution rows with `step <= position` — the **as-of
    /// attribution slice** up to and including the head step
    /// (`docs/04-replay-mode.md` §4). ChainView never recomputes or **sums** the
    /// decomposition: the replay screen renders the **head-step row**
    /// ([`head_greeks`](Self::head_greeks)) as authored, so this full slice is
    /// **currently unused by the screen** — it is retained for the deferred
    /// attribution-history / #49 payoff work.
    #[must_use]
    pub fn visible_greeks<'b>(&self, bundle: &'b LoadedBundle) -> &'b [GreeksAttribution] {
        bundle.greeks.get(..self.greeks_ix).unwrap_or(&[])
    }

    /// Every fill with `step <= position` — the drill-down history up to the head.
    #[must_use]
    pub fn visible_fills<'b>(&self, bundle: &'b LoadedBundle) -> &'b [Fill] {
        bundle.fills.get(..self.fills_ix).unwrap_or(&[])
    }

    /// Every `positions` row with `step <= position`, terminal rows included — the
    /// raw as-of window. [`open_positions`](Self::open_positions) is the filtered,
    /// post-fill open set most callers want.
    #[must_use]
    pub fn visible_positions<'b>(&self, bundle: &'b LoadedBundle) -> &'b [PositionRow] {
        bundle.positions.get(..self.positions_ix).unwrap_or(&[])
    }

    /// The `equity_curve` row at the head step (`step == position`), or `None` for
    /// an empty run. Relies on reliance 1: the last visible equity row is the
    /// head-step row.
    #[must_use]
    pub fn head_equity<'b>(&self, bundle: &'b LoadedBundle) -> Option<&'b EquityPoint> {
        bundle.equity.get(self.equity_ix.checked_sub(1)?)
    }

    /// The `greeks_attribution` row at the head step (`step == position`), or
    /// `None` for an empty run. Relies on reliance 1.
    #[must_use]
    pub fn head_greeks<'b>(&self, bundle: &'b LoadedBundle) -> Option<&'b GreeksAttribution> {
        bundle.greeks.get(self.greeks_ix.checked_sub(1)?)
    }

    /// The fill(s) at exactly the head step (`step == position`) — the rows a
    /// drill-down highlights under the scrub head. Empty when no fill landed on the
    /// head step. A realistic order that walked several price levels appears as
    /// several rows here.
    #[must_use]
    pub fn head_fills<'b>(&self, bundle: &'b LoadedBundle) -> &'b [Fill] {
        let lo = bundle.fills.partition_point(|f| f.step < self.position);
        bundle.fills.get(lo..self.fills_ix).unwrap_or(&[])
    }

    /// The **post-fill open-position set** at the head step
    /// (`docs/04-replay-mode.md` §4): the latest non-terminal `positions` row per
    /// `position_id` with `step <= position`, **excluding** any `position_id` whose
    /// terminal (`exit_reason`-bearing) row is at `step <= position`.
    ///
    /// A leg closed at or before the cursor is not shown open; an opening and a
    /// closing row at the *same* step resolve deterministically (the terminal row
    /// wins the exclusion); a leg with `open_at_end = true` and no terminal row
    /// stays open through `end_step`. This is the single algorithm the drill-down
    /// and the deferred payoff (#49) both read, so the two screens can never
    /// disagree.
    ///
    /// The result is ordered by `position_id` (deterministic) and borrows into the
    /// bundle. It is `O(k log k)` in the `k` visible position rows, not on the draw
    /// path — call it on a seek, not per frame. Relies on reliance 4 (stable
    /// `position_id`, positive `quantity`).
    #[must_use]
    pub fn open_positions<'b>(&self, bundle: &'b LoadedBundle) -> Vec<&'b PositionRow> {
        let visible = self.visible_positions(bundle);
        // A single pass: track the latest row per leg (rows are step-ascending, so a
        // later insert wins) and the set of legs whose terminal row is in view.
        let mut latest: BTreeMap<u64, &PositionRow> = BTreeMap::new();
        let mut closed: BTreeSet<u64> = BTreeSet::new();
        for row in visible {
            if row.exit_reason.is_some() {
                let _ = closed.insert(row.position_id);
            }
            let _ = latest.insert(row.position_id, row);
        }
        latest
            .into_iter()
            .filter(|(id, _)| !closed.contains(id))
            .map(|(_, row)| row)
            .collect()
    }
}

/// The last step of `bundle` — `equity_curve` length minus one, or `0` for an
/// empty run. Relies on the #32 guarantee that `equity_curve` spans the
/// contiguous 0-based domain `0..N` (module reliance 1). The `try_from` fallback
/// is unreachable (`N <= MAX_TABLE_ROWS < u32::MAX`) and keeps the conversion
/// checked.
fn end_step_of(bundle: &LoadedBundle) -> u32 {
    // Explicit empty-run case (no banned saturating method, no underflow by
    // construction); the narrowing stays checked with an unreachable fallback
    // (`N <= MAX_TABLE_ROWS < u32::MAX`).
    let last = match bundle.equity.len() {
        0 => 0,
        n => n - 1,
    };
    u32::try_from(last).unwrap_or(u32::MAX)
}

/// Walk `ix` to `partition_point(|r| step(r) <= new_pos)` **incrementally** from
/// its current value, returning `(new_ix, moves)` where `moves` is the number of
/// single-step index moves taken.
///
/// Only one of the two loops runs (extend forward when `ix` is too low, retract
/// backward when it is too high), so `moves == |new_ix - old_ix|` and the walk is
/// O(rows moved) — it starts from the passed `ix` and can never rescan from zero.
/// It yields the identical index a `partition_point` binary search would, so the
/// incremental and arbitrary seek paths agree. Correct only on a step-sorted table
/// (module reliance 2). `moves` is used by the no-rescan unit test.
fn walk_index<T>(
    rows: &[T],
    mut ix: usize,
    new_pos: u32,
    step: impl Fn(&T) -> u32,
) -> (usize, u64) {
    let mut moves: u64 = 0;
    // Extend forward while the row at `ix` still belongs to the as-of window.
    while let Some(row) = rows.get(ix) {
        if step(row) <= new_pos {
            // Checked increments (saturating_* is banned): `ix < rows.len()` here
            // and `moves <= rows.len()`, so overflow is unreachable; the
            // unwrap_or keeps the walk total rather than wrapping.
            ix = ix.checked_add(1).unwrap_or(ix);
            moves = moves.checked_add(1).unwrap_or(moves);
        } else {
            break;
        }
    }
    // Retract backward while the row just before `ix` now sits past the head.
    while ix > 0 {
        match rows.get(ix - 1) {
            Some(row) if step(row) > new_pos => {
                ix -= 1;
                moves = moves.checked_add(1).unwrap_or(moves);
            }
            _ => break,
        }
    }
    (ix, moves)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use optionstratlib::OptionStyle;
    use proptest::prelude::*;

    use super::{
        EquityPoint, Fill, GreeksAttribution, LoadedBundle, Playback, PlaybackSpeed, PositionRow,
        SeekTo, TimelineCursor, end_step_of, walk_index,
    };
    use crate::replay::{BundleManifest, ExecMode, PositionSide, SUPPORTED_SCHEMA};

    // --- fixtures (small in-memory bundles, no Parquet) ----------------------

    const BASE_TS: i64 = 1_700_000_000_000_000_000;
    const CID: &str = "v1:BTC:1735286400000000000:6000000:C";
    const EXP_NS: i64 = 1_735_286_400_000_000_000;
    const RUN: &str = "run-xyz";

    fn ts(step: u32) -> i64 {
        BASE_TS + i64::from(step)
    }

    fn manifest() -> BundleManifest {
        // The cursor never reads the manifest; this only satisfies `LoadedBundle`.
        let mut row_counts = BTreeMap::new();
        let _ = row_counts.insert("fills".to_owned(), 0);
        let _ = row_counts.insert("equity_curve".to_owned(), 0);
        let _ = row_counts.insert("positions".to_owned(), 0);
        let _ = row_counts.insert("greeks_attribution".to_owned(), 0);
        BundleManifest {
            schema: SUPPORTED_SCHEMA.to_owned(),
            run_id: RUN.to_owned(),
            created_utc: "2026-07-17T00:00:00Z".to_owned(),
            code_version: "0.3.0".to_owned(),
            lockfile_sha256: "deadbeef".to_owned(),
            seed: 1,
            config: serde_json::json!({ "capital_cents": 1_000_000 }),
            strategy: serde_json::json!({ "kind": "iron_condor" }),
            data_source: serde_json::json!({ "kind": "simulator" }),
            metrics: serde_json::json!({}),
            row_counts,
        }
    }

    fn equity_point(step: u32) -> EquityPoint {
        EquityPoint {
            step,
            ts_ns: ts(step),
            cash_cents: 1_000 + i64::from(step),
            position_value_cents: 0,
            equity_cents: 1_000 + i64::from(step),
            drawdown: -0.01,
        }
    }

    fn greeks_row(step: u32) -> GreeksAttribution {
        GreeksAttribution {
            step,
            ts_ns: ts(step),
            theta_pnl_cents: 1,
            delta_pnl_cents: 0,
            vega_pnl_cents: 0,
            spread_capture_cents: 0,
            fees_cents: 0,
            residual_cents: 0,
        }
    }

    fn fill_at(step: u32, order_id: u64) -> Fill {
        Fill {
            step,
            ts_ns: ts(step),
            strategy_run_id: RUN.to_owned(),
            trade_id: order_id,
            position_id: order_id,
            order_id,
            fill_seq: 0,
            underlying: "BTC".to_owned(),
            expiration_ns: EXP_NS,
            contract_id: CID.to_owned(),
            strike_cents: 6_000_000,
            style: OptionStyle::Call,
            side: PositionSide::Long,
            quantity: 1,
            price_cents: 12_500,
            fees_cents: 30,
            slippage_cents: -15,
            mode: ExecMode::Realistic,
        }
    }

    fn position_at(
        step: u32,
        position_id: u64,
        exit_reason: Option<&str>,
        open_at_end: bool,
    ) -> PositionRow {
        PositionRow {
            step,
            ts_ns: ts(step),
            position_id,
            trade_id: 7,
            contract_id: CID.to_owned(),
            side: PositionSide::Short,
            quantity: 1,
            avg_price_cents: 12_000,
            mark_cents: 11_800,
            unrealized_cents: 200,
            stale_mark: false,
            exit_reason: exit_reason.map(ToOwned::to_owned),
            open_at_end,
        }
    }

    /// An `n`-step bundle (dense `equity`/`greeks` over `0..n`) with the supplied
    /// `fills`/`positions`, each sorted into its stated sort key so the cursor's
    /// step-sorted reliance holds.
    fn bundle(n: u32, mut fills: Vec<Fill>, mut positions: Vec<PositionRow>) -> LoadedBundle {
        fills.sort_by_key(|f| (f.step, f.order_id, f.fill_seq));
        positions.sort_by_key(|p| (p.step, p.position_id));
        LoadedBundle {
            manifest: manifest(),
            fills,
            equity: (0..n).map(equity_point).collect(),
            positions,
            greeks: (0..n).map(greeks_row).collect(),
        }
    }

    /// The `position_id`s of the open-position set at the cursor, sorted.
    fn open_ids(cursor: &TimelineCursor, b: &LoadedBundle) -> Vec<u64> {
        cursor
            .open_positions(b)
            .iter()
            .map(|p| p.position_id)
            .collect()
    }

    // --- construction + basic seeks ------------------------------------------

    #[test]
    fn test_new_starts_at_step_zero() {
        let b = bundle(6, vec![], vec![]);
        let c = TimelineCursor::new(&b);
        assert_eq!(c.position, 0);
        assert_eq!(c.end_step, 5);
        assert!(c.is_at_start());
        assert!(!c.is_at_end());
        // Dense equity/greeks: one row visible at step 0.
        assert_eq!(c.equity_ix, 1);
        assert_eq!(c.greeks_ix, 1);
    }

    #[test]
    fn test_seek_step_to_mid_and_last() {
        let b = bundle(6, vec![], vec![]);
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(3), &b);
        assert_eq!(c.position, 3);
        assert_eq!(c.equity_ix, 4);
        c.seek(SeekTo::Step(5), &b);
        assert_eq!(c.position, 5);
        assert!(c.is_at_end());
    }

    #[test]
    fn test_seek_clamps_out_of_range() {
        let b = bundle(4, vec![], vec![]);
        let mut c = TimelineCursor::new(&b);
        // Step past the end clamps to end_step (End semantics).
        c.seek(SeekTo::Step(u32::MAX), &b);
        assert_eq!(c.position, 3);
        // StepBy past the end clamps, never wraps.
        c.seek(SeekTo::StepBy(100), &b);
        assert_eq!(c.position, 3);
        // StepBy below zero clamps to 0 (Home direction).
        c.seek(SeekTo::StepBy(-100), &b);
        assert_eq!(c.position, 0);
        c.seek(SeekTo::StepBy(-1), &b);
        assert_eq!(c.position, 0);
    }

    #[test]
    fn test_home_and_end_land_on_edges() {
        let b = bundle(7, vec![], vec![]);
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(4), &b);
        c.seek(SeekTo::Step(0), &b); // Home
        assert_eq!(c.position, 0);
        c.seek(SeekTo::Step(c.end_step), &b); // End
        assert_eq!(c.position, 6);
    }

    // --- the incremental / arbitrary equivalence at a fixed target -----------

    #[test]
    fn test_incremental_and_arbitrary_agree_on_sparse_tables() {
        // Sparse fills at steps 0, 2, 2, 5; sparse positions at 1, 4.
        let fills = vec![
            fill_at(0, 10),
            fill_at(2, 20),
            fill_at(2, 21),
            fill_at(5, 50),
        ];
        let positions = vec![
            position_at(1, 1, None, false),
            position_at(4, 2, None, false),
        ];
        let b = bundle(6, fills, positions);

        for target in 0..=5 {
            let mut arb = TimelineCursor::new(&b);
            arb.seek(SeekTo::Step(target), &b);

            let mut inc = TimelineCursor::new(&b);
            for _ in 0..target {
                inc.seek(SeekTo::StepBy(1), &b);
            }
            assert_eq!(arb, inc, "paths disagree at target {target}");
        }
    }

    #[test]
    fn test_stepby_walks_incrementally_without_rescan() {
        let equity: Vec<EquityPoint> = (0..100).map(equity_point).collect();
        // From ix=51 (== partition_point at step 50), a +1 to step 51 moves ONE
        // index — a rescan-from-zero would touch 52.
        let (ix, moves) = walk_index(&equity, 51, 51, |e| e.step);
        assert_eq!(ix, 52);
        assert_eq!(moves, 1);
        // A backward jump to step 40 retracts exactly 11.
        let (ix2, moves2) = walk_index(&equity, 52, 40, |e| e.step);
        assert_eq!(ix2, 41);
        assert_eq!(moves2, 11);
        // Re-resolving the same position is a zero-move no-op (idempotent).
        let (ix3, moves3) = walk_index(&equity, 41, 40, |e| e.step);
        assert_eq!(ix3, 41);
        assert_eq!(moves3, 0);
    }

    // --- as-of slice consistency (every widget reflects one step) ------------

    #[test]
    fn test_as_of_slices_are_consistent_at_one_step() {
        let fills = vec![
            fill_at(0, 10),
            fill_at(2, 20),
            fill_at(2, 21),
            fill_at(5, 50),
        ];
        let b = bundle(6, fills, vec![]);
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(2), &b);

        // Head rows reflect exactly step 2.
        match c.head_equity(&b) {
            Some(e) => assert_eq!(e.step, 2),
            None => panic!("head_equity must exist at step 2"),
        }
        match c.head_greeks(&b) {
            Some(g) => assert_eq!(g.step, 2),
            None => panic!("head_greeks must exist at step 2"),
        }
        // Visible slices never exceed the head step.
        assert!(c.visible_equity(&b).iter().all(|e| e.step <= 2));
        assert!(c.visible_fills(&b).iter().all(|f| f.step <= 2));
        assert_eq!(c.visible_equity(&b).len(), 3); // steps 0, 1, 2
        // Head fills are exactly the two step-2 fills.
        let head = c.head_fills(&b);
        assert_eq!(head.len(), 2);
        assert!(head.iter().all(|f| f.step == 2));
        // The last visible greeks row is the head-step row.
        match c.visible_greeks(&b).last() {
            Some(g) => assert_eq!(g.step, 2),
            None => panic!("visible greeks must be non-empty at step 2"),
        }
    }

    #[test]
    fn test_head_fills_empty_when_no_fill_on_head_step() {
        let fills = vec![fill_at(0, 10), fill_at(2, 20)];
        let b = bundle(4, fills, vec![]);
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(1), &b); // no fill at step 1
        assert!(c.head_fills(&b).is_empty());
        assert_eq!(c.visible_fills(&b).len(), 1); // only the step-0 fill
    }

    // --- open-position reconstruction ----------------------------------------

    /// Positions fixture: leg B (open throughout), leg A (opens 1, closes 3), and
    /// leg C (opens and closes at the same step 4).
    fn open_close_bundle() -> LoadedBundle {
        let mut positions = Vec::new();
        // B (id 2): open at every step 0..=5, open_at_end on the last row.
        for s in 0..=5 {
            positions.push(position_at(s, 2, None, s == 5));
        }
        // A (id 1): non-terminal at 1 and 2, terminal at 3.
        positions.push(position_at(1, 1, None, false));
        positions.push(position_at(2, 1, None, false));
        positions.push(position_at(3, 1, Some("target"), false));
        // C (id 3): a single terminal row at step 4 (open + close same step).
        positions.push(position_at(4, 3, Some("stop"), false));
        bundle(6, vec![], positions)
    }

    #[test]
    fn test_open_positions_before_any_open() {
        let b = open_close_bundle();
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(0), &b);
        assert_eq!(open_ids(&c, &b), vec![2]); // only B has opened
    }

    #[test]
    fn test_open_positions_shows_latest_non_terminal_row() {
        let b = open_close_bundle();
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(2), &b);
        assert_eq!(open_ids(&c, &b), vec![1, 2]);
        // Leg A's shown row is its latest non-terminal one (step 2).
        let a_row = c
            .open_positions(&b)
            .into_iter()
            .find(|p| p.position_id == 1);
        match a_row {
            Some(p) => assert_eq!(p.step, 2),
            None => panic!("leg A must be open at step 2"),
        }
    }

    #[test]
    fn test_open_positions_excludes_terminated_leg() {
        let b = open_close_bundle();
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(3), &b); // A's terminal row is at step 3
        assert_eq!(open_ids(&c, &b), vec![2]); // A excluded, B still open
    }

    #[test]
    fn test_open_positions_same_step_open_close_is_excluded() {
        let b = open_close_bundle();
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(4), &b); // C opens and closes at step 4
        assert_eq!(open_ids(&c, &b), vec![2]); // C excluded (terminal wins)
    }

    #[test]
    fn test_open_at_end_leg_stays_open_through_the_end() {
        let b = open_close_bundle();
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(c.end_step), &b);
        assert_eq!(open_ids(&c, &b), vec![2]); // B has no terminal row
        let b_row = c
            .open_positions(&b)
            .into_iter()
            .find(|p| p.position_id == 2);
        match b_row {
            Some(p) => {
                assert_eq!(p.step, 5);
                assert!(p.open_at_end);
            }
            None => panic!("open_at_end leg must be open at end_step"),
        }
    }

    // --- degenerate runs ------------------------------------------------------

    #[test]
    fn test_empty_run_is_panic_free() {
        let b = bundle(0, vec![], vec![]);
        let mut c = TimelineCursor::new(&b);
        assert_eq!(c.end_step, 0);
        assert_eq!(c.position, 0);
        assert!(c.head_equity(&b).is_none());
        assert!(c.head_greeks(&b).is_none());
        assert!(c.visible_equity(&b).is_empty());
        assert!(c.open_positions(&b).is_empty());
        // Every seek is a clamped no-op, never a panic.
        c.seek(SeekTo::StepBy(1), &b);
        c.seek(SeekTo::StepBy(-1), &b);
        c.seek(SeekTo::Step(9), &b);
        assert_eq!(c.position, 0);
    }

    #[test]
    fn test_single_step_run() {
        let b = bundle(1, vec![fill_at(0, 10)], vec![position_at(0, 1, None, true)]);
        let mut c = TimelineCursor::new(&b);
        assert_eq!(c.end_step, 0);
        assert!(c.is_at_start() && c.is_at_end());
        c.seek(SeekTo::StepBy(1), &b);
        assert_eq!(c.position, 0);
        c.seek(SeekTo::Step(9), &b);
        assert_eq!(c.position, 0);
        assert_eq!(open_ids(&c, &b), vec![1]);
        match c.head_equity(&b) {
            Some(e) => assert_eq!(e.step, 0),
            None => panic!("single-step run must have a head equity row"),
        }
    }

    // --- playback model -------------------------------------------------------

    #[test]
    fn test_playback_defaults_paused_and_yields_no_seek() {
        assert_eq!(Playback::default(), Playback::Paused);
        assert!(!Playback::Paused.is_playing());
        assert_eq!(Playback::Paused.tick_seek(), None);
        assert_eq!(Playback::Paused.speed(), None);
    }

    #[test]
    fn test_playback_tick_seek_scales_by_speed() {
        assert_eq!(
            Playback::playing(PlaybackSpeed::X1).tick_seek(),
            Some(SeekTo::StepBy(1))
        );
        assert_eq!(
            Playback::playing(PlaybackSpeed::X2).tick_seek(),
            Some(SeekTo::StepBy(2))
        );
        assert_eq!(
            Playback::playing(PlaybackSpeed::X5).tick_seek(),
            Some(SeekTo::StepBy(5))
        );
        assert_eq!(
            Playback::playing(PlaybackSpeed::X10).tick_seek(),
            Some(SeekTo::StepBy(10))
        );
    }

    #[test]
    fn test_playback_speed_helpers() {
        assert_eq!(PlaybackSpeed::default(), PlaybackSpeed::X1);
        assert_eq!(PlaybackSpeed::ALL.len(), 4);
        assert_eq!(PlaybackSpeed::X2.multiplier(), 2);
        assert_eq!(PlaybackSpeed::X10.quantum(), 10);
        // faster/slower clamp at the ends (they do not wrap).
        assert_eq!(PlaybackSpeed::X1.faster(), PlaybackSpeed::X2);
        assert_eq!(PlaybackSpeed::X10.faster(), PlaybackSpeed::X10);
        assert_eq!(PlaybackSpeed::X10.slower(), PlaybackSpeed::X5);
        assert_eq!(PlaybackSpeed::X1.slower(), PlaybackSpeed::X1);
    }

    #[test]
    fn test_playback_toggle() {
        let paused = Playback::Paused;
        let playing = paused.toggled(PlaybackSpeed::X5);
        assert_eq!(playing, Playback::playing(PlaybackSpeed::X5));
        assert_eq!(playing.toggled(PlaybackSpeed::X1), Playback::Paused);
    }

    #[test]
    fn test_advance_playback_stops_at_end_without_wrapping() {
        let b = bundle(5, vec![], vec![]); // end_step 4
        let playback = Playback::playing(PlaybackSpeed::X2);
        let mut c = TimelineCursor::new(&b);
        let mut seen = Vec::new();
        for _ in 0..10 {
            c.advance_playback(playback, &b);
            seen.push(c.position);
        }
        // 0 -> 2 -> 4 -> 4 -> ... : monotone, clamped at 4, never wraps to 0.
        assert_eq!(seen.first(), Some(&2));
        assert_eq!(c.position, 4);
        assert!(seen.iter().all(|&p| p <= 4));
        assert!(seen.windows(2).all(|w| match w {
            [a, b] => b >= a,
            _ => true,
        }));
    }

    #[test]
    fn test_advance_playback_paused_is_a_no_op() {
        let b = bundle(5, vec![], vec![]);
        let mut c = TimelineCursor::new(&b);
        c.seek(SeekTo::Step(2), &b);
        let before = c;
        c.advance_playback(Playback::Paused, &b);
        assert_eq!(before, c);
    }

    // --- property tests -------------------------------------------------------

    prop_compose! {
        /// A step-sorted bundle: dense `equity`/`greeks` over `0..n`, plus sparse
        /// `fills`/`positions` whose steps are folded into the domain.
        fn arb_bundle()(
            n in 1u32..16,
            fill_steps in prop::collection::vec(0u32..15, 0..24),
            pos_steps in prop::collection::vec(0u32..15, 0..24),
        ) -> LoadedBundle {
            let fills = fill_steps
                .iter()
                .enumerate()
                .map(|(i, s)| fill_at(*s % n, u64::try_from(i).unwrap_or_default()))
                .collect();
            let positions = pos_steps
                .iter()
                .enumerate()
                .map(|(i, s)| position_at(*s % n, u64::try_from(i).unwrap_or_default(), None, false))
                .collect();
            bundle(n, fills, positions)
        }
    }

    fn arb_seek() -> impl Strategy<Value = SeekTo> {
        prop_oneof![
            (0u32..30).prop_map(SeekTo::Step),
            (-6i32..6).prop_map(SeekTo::StepBy),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The binding property: stepping incrementally from 0 to any step `S`
        /// yields exactly the same cursor state as seeking directly to `S`.
        #[test]
        fn incremental_equals_arbitrary(b in arb_bundle(), target in 0u32..30) {
            let clamped = target.min(end_step_of(&b));

            let mut arb = TimelineCursor::new(&b);
            arb.seek(SeekTo::Step(target), &b);

            let mut inc = TimelineCursor::new(&b);
            for _ in 0..clamped {
                inc.seek(SeekTo::StepBy(1), &b);
            }

            prop_assert_eq!(arb, inc);
            prop_assert_eq!(arb.position, clamped);
        }

        /// After any seek sequence, every index is `partition_point(step <=
        /// position)` and the position never escapes `0..=end_step`.
        #[test]
        fn seek_lands_on_last_le_step(
            b in arb_bundle(),
            seeks in prop::collection::vec(arb_seek(), 1..12),
        ) {
            let mut c = TimelineCursor::new(&b);
            for s in seeks {
                c.seek(s, &b);
                prop_assert!(c.position <= c.end_step);
                prop_assert_eq!(c.fills_ix, b.fills.partition_point(|f| f.step <= c.position));
                prop_assert_eq!(c.equity_ix, b.equity.partition_point(|e| e.step <= c.position));
                prop_assert_eq!(
                    c.positions_ix,
                    b.positions.partition_point(|p| p.step <= c.position)
                );
                prop_assert_eq!(c.greeks_ix, b.greeks.partition_point(|g| g.step <= c.position));
            }
        }

        /// Seeking to the same position twice equals seeking once, and `StepBy(0)`
        /// is a no-op.
        #[test]
        fn seek_is_idempotent(b in arb_bundle(), target in 0u32..30) {
            let mut once = TimelineCursor::new(&b);
            once.seek(SeekTo::Step(target), &b);
            let mut twice = once;
            twice.seek(SeekTo::Step(target), &b);
            prop_assert_eq!(once, twice);

            let mut noop = once;
            noop.seek(SeekTo::StepBy(0), &b);
            prop_assert_eq!(once, noop);
        }

        /// A forward step then an equal backward step returns to the same state
        /// whenever neither direction clamps.
        #[test]
        fn step_then_back_returns(b in arb_bundle(), start in 0u32..30, delta in 1i32..6) {
            let mut c = TimelineCursor::new(&b);
            c.seek(SeekTo::Step(start), &b);
            let base = c;
            let d = u32::try_from(delta).unwrap_or(0);
            // Only assert the round-trip when the forward and backward moves both
            // stay inside `0..=end_step` (an unclamped round-trip).
            if base.position >= d && base.position + d <= base.end_step {
                c.seek(SeekTo::StepBy(delta), &b);
                c.seek(SeekTo::StepBy(-delta), &b);
                prop_assert_eq!(base, c);
            }
        }
    }
}
