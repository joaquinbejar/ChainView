//! Off-draw replay equity geometry (#35, `docs/05-views-and-ux.md` §5,
//! `docs/04-replay-mode.md` §4).
//!
//! This module owns [`EquityGeometry`]: the timeline cursor's as-of equity slice
//! (`equity[..=head]`) projected into **one**
//! [`GraphData::Series`](optionstratlib::visualization::GraphData) (step → equity, in
//! **integer cents**) plus the exact peak-drawdown figure — entirely in the
//! **application** layer and entirely **off** the draw path. It is invoked from
//! [`LoadedReplay`](super::LoadedReplay) on load and on every cursor move (a seek or a
//! playback tick), never from `terminal.draw` (`docs/02-tui-architecture.md` §7).
//!
//! # The drawdown is seeded from the run's opening capital
//!
//! The running peak that the drawdown measures against is seeded from the run's
//! **opening capital** ([`CapitalConfig::capital_cents`](crate::replay::CapitalConfig),
//! the #29 reconcile), not from the first equity row. So a strategy that *loses* on
//! step 0 shows its true drawdown against the opening capital at the head, instead of a
//! misleading `$0` (the loss vs the opening balance would otherwise be invisible).
//! Money stays integer cents here; the `$` conversion is the ui edge only.
//!
//! # Forward moves extend incrementally; a backward seek rebuilds
//!
//! A playback tick (or a forward `StepBy`) advances the head by a few steps. Rather
//! than rebuild the whole series and rescan the entire visible history for the peak on
//! every tick (quadratic over a long bundle), [`EquityGeometry::extend_forward`] folds
//! **only the newly-visible tail** into the running peak (O(new points)) and appends
//! its samples to the cached series — re-striding only on the rare downsample-boundary
//! crossing. An arbitrary backward seek (`SeekTo` to an earlier step) cannot extend, so
//! [`EquityGeometry::rebuild`] recomputes from the seed over the shorter slice. The
//! result is a **pure function of the bundle + cursor**: an incremental extend to a
//! head lands on exactly the same geometry a from-scratch build at that head would.
//!
//! # Money stays integer cents until the render edge
//!
//! The equity series y-axis is authored in **integer cents** as [`Decimal`] (an
//! exact fixed-point value, never `f64`), so the domain carries no floating money
//! (`CLAUDE.md` numeric policy). The #23 `GraphData → ratatui` adapter converts the
//! `Decimal` cents to plot `f64` at the UI edge; the replay screen formats the axis
//! labels and the peak figure to `$` there, the single cents→`$` seam.
//!
//! # The drawn series is bounded (frame budget)
//!
//! An `N`-step run can hold up to `MAX_TABLE_ROWS` equity points; plotting all of
//! them would make the draw `O(full backtest)`. So the series is **stride-sampled**
//! to at most [`MAX_EQUITY_POINTS`] points here (off the draw path), keeping the
//! **first and last** sample so the head and the run start stay on screen and the
//! plot bounds cover the head. The peak-drawdown figure is computed over the **full**
//! slice, so the label stays exact regardless of the visual downsample
//! (`docs/06-performance.md`, the render frame budget).

use optionstratlib::prelude::Decimal;
use optionstratlib::visualization::{GraphData, Series2D, TraceMode};

use crate::replay::EquityPoint;

/// The series name the equity chart renders (surfaced in the adapter's projection).
pub(crate) const EQUITY_SERIES_NAME: &str = "equity";

/// The maximum number of points the equity series carries after stride sampling, so
/// the draw plots `O(MAX_EQUITY_POINTS)` regardless of the run length (the render
/// frame budget). Chosen a little above a wide terminal's braille-cell width so the
/// downsample is visually lossless on any real terminal.
pub(crate) const MAX_EQUITY_POINTS: usize = 512;

/// The cached, off-draw equity geometry for the replay screen: the stride-sampled
/// step→cents series and the exact peak-drawdown figure over the as-of slice, both
/// maintained incrementally on a forward cursor move and rebuilt on a backward seek.
///
/// The peak drawdown is seeded from the run's opening capital (`seed_cents`), so a
/// step-0 loss shows its true drawdown against the opening balance. Determinism: for
/// any head, the incrementally-extended geometry equals a from-scratch [`build`]
/// (equity slice + seed → geometry is a pure function).
///
/// [`build`]: EquityGeometry::build
#[derive(Debug)]
pub(crate) struct EquityGeometry {
    /// The stride-sampled equity line series (step → equity **cents**) up to the head.
    graph: GraphData,
    /// The running maximum equity in integer cents (`>= seed_cents`) — the peak the
    /// drawdown measures against, folded forward as new points become visible.
    running_peak_cents: i64,
    /// The worst `equity − running_peak` seen over the as-of slice, in `(−∞, 0]`.
    peak_drawdown_cents: i64,
    /// The drawdown baseline: the run's opening capital in integer cents, the seed the
    /// running peak starts from (constant across every rebuild of this run).
    seed_cents: i64,
    /// The number of raw visible equity points folded so far (equals the cursor's
    /// `equity_ix`) — the boundary an [`extend_forward`](Self::extend_forward) folds
    /// from.
    raw_len: usize,
}

impl EquityGeometry {
    /// Build the geometry from scratch over the as-of equity slice `equity`
    /// (`equity[..=head]`), seeding the running peak from `seed_cents` (the run's
    /// opening capital). The peak drawdown is exact over the **full** slice; the series
    /// is stride-sampled to at most [`MAX_EQUITY_POINTS`] points. Off the draw path.
    #[must_use]
    pub(crate) fn build(equity: &[EquityPoint], seed_cents: i64) -> Self {
        let mut running_peak_cents = seed_cents;
        let mut peak_drawdown_cents = 0i64;
        fold_peak(equity, &mut running_peak_cents, &mut peak_drawdown_cents);
        Self {
            graph: build_series(equity),
            running_peak_cents,
            peak_drawdown_cents,
            seed_cents,
            raw_len: equity.len(),
        }
    }

    /// Extend the geometry forward to the new, longer-or-equal as-of slice `equity`,
    /// folding **only the newly-visible tail** into the running peak (O(new points))
    /// and appending its samples to the cached series — never a rescan of the full
    /// history. The series is appended in place when the sampling stride is unchanged;
    /// it re-strides only on the rare downsample-boundary crossing (~every
    /// [`MAX_EQUITY_POINTS`] steps). A no-op when no new equity rows are visible (a
    /// forward step within an equity gap).
    ///
    /// The caller guarantees `equity.len() >= self.raw_len()` (a forward move); a
    /// backward move takes [`rebuild`](Self::rebuild) instead.
    pub(crate) fn extend_forward(&mut self, equity: &[EquityPoint]) {
        let old_len = self.raw_len;
        let new_len = equity.len();
        debug_assert!(
            new_len >= old_len,
            "extend_forward is forward-only; a backward move must rebuild",
        );
        if new_len <= old_len {
            return; // no newly-visible equity rows (a forward step within a gap)
        }
        // Peak: fold only the newly-visible tail — O(new points), never a rescan.
        if let Some(tail) = equity.get(old_len..) {
            fold_peak(
                tail,
                &mut self.running_peak_cents,
                &mut self.peak_drawdown_cents,
            );
        }
        // Series: append the new samples in place when the stride is unchanged; the
        // append reproduces exactly what a full rebuild would sample. Re-stride only on
        // a boundary crossing (rare), preserving incremental == full at every head.
        let stride = stride_for(new_len);
        if stride == stride_for(old_len)
            && let GraphData::Series(series) = &mut self.graph
        {
            append_series(series, equity, old_len, new_len, stride);
        } else {
            self.graph = build_series(equity);
        }
        self.raw_len = new_len;
    }

    /// Rebuild the geometry from scratch over `equity`, preserving the run's opening
    /// capital seed — the path a backward / arbitrary seek takes (the head moved to an
    /// earlier step, so the cached tail cannot be extended). O(visible).
    pub(crate) fn rebuild(&mut self, equity: &[EquityPoint]) {
        *self = Self::build(equity, self.seed_cents);
    }

    /// The cached equity `GraphData` (step → cents), for the ui view-cache to project
    /// off the draw path. Not read by `draw`.
    #[must_use]
    pub(crate) fn graph(&self) -> &GraphData {
        &self.graph
    }

    /// The peak drawdown up to the head in **integer cents** (`<= 0`), measured against
    /// the opening-capital-seeded running peak. A `Copy` read — never a per-frame scan.
    #[must_use]
    pub(crate) fn peak_drawdown_cents(&self) -> i64 {
        self.peak_drawdown_cents
    }

    /// The number of raw visible equity points folded so far (the cursor's
    /// `equity_ix`), so the caller can dispatch forward-extend vs full-rebuild off a
    /// cheap length compare.
    #[must_use]
    pub(crate) fn raw_len(&self) -> usize {
        self.raw_len
    }
}

/// The sampling stride for a slice of `n` raw points: `ceil(n / MAX_EQUITY_POINTS)`,
/// floored at 1 so the sample count stays at or below the cap and forward progress is
/// guaranteed even when `n <= MAX_EQUITY_POINTS`.
fn stride_for(n: usize) -> usize {
    n.div_ceil(MAX_EQUITY_POINTS).max(1)
}

/// Fold `points` into the running peak + worst-drawdown accumulators: the running peak
/// climbs to any new high, and the worst drawdown tracks the most-negative
/// `equity − running_peak`. Checked integer arithmetic only — no `f64` money; an absurd
/// underflow is skipped (defensive) rather than fabricating a `0` drawdown.
fn fold_peak(points: &[EquityPoint], running_peak_cents: &mut i64, worst_cents: &mut i64) {
    for point in points {
        if point.equity_cents > *running_peak_cents {
            *running_peak_cents = point.equity_cents;
        }
        if let Some(drawdown) = point.equity_cents.checked_sub(*running_peak_cents)
            && drawdown < *worst_cents
        {
            *worst_cents = drawdown;
        }
    }
}

/// Build the stride-sampled equity line series (step → equity **cents**) from the
/// as-of slice `equity`, at most [`MAX_EQUITY_POINTS`] points, always retaining the
/// first and last sample. An empty slice yields an empty series the #23 adapter renders
/// as the deliberate "no equity rows" empty state — never a fabricated line.
fn build_series(equity: &[EquityPoint]) -> GraphData {
    let mut xs: Vec<Decimal> = Vec::new();
    let mut ys: Vec<Decimal> = Vec::new();
    push_sampled(equity, &mut xs, &mut ys);
    GraphData::Series(Series2D {
        x: xs,
        y: ys,
        name: EQUITY_SERIES_NAME.to_owned(),
        mode: TraceMode::Lines,
        line_color: None,
        line_width: Some(2.0),
    })
}

/// Stride-sample `equity` into the parallel `(step, equity_cents)` `Decimal` vectors,
/// keeping the grid multiples of the stride plus the **last** point (the head). A slice
/// at or below the cap is copied verbatim.
fn push_sampled(equity: &[EquityPoint], xs: &mut Vec<Decimal>, ys: &mut Vec<Decimal>) {
    let n = equity.len();
    let Some(last) = n.checked_sub(1) else {
        return; // empty slice → an empty series
    };
    let stride = stride_for(n);
    let mut idx = 0usize;
    let mut pushed_last = false;
    while idx < n {
        if let Some(point) = equity.get(idx) {
            push_point(point, xs, ys);
            if idx == last {
                pushed_last = true;
            }
        }
        idx = match idx.checked_add(stride) {
            Some(next) => next,
            None => break,
        };
    }
    // The stride may skip the final point; append it so the head is always plotted
    // and the axis bounds cover it.
    if !pushed_last && let Some(point) = equity.get(last) {
        push_point(point, xs, ys);
    }
}

/// Append the samples for the forward window `[old_len, new_len)` into the cached
/// `series`, matching exactly what [`build_series`] would sample for `new_len` (so an
/// incremental extend equals a full rebuild). The caller guarantees the stride is
/// unchanged across the window.
///
/// The three moves: (1) drop the old **forced-last** sample if the previous head was a
/// non-grid point (it is re-derived below); (2) append every grid multiple of `stride`
/// in `[old_len, new_len)`; (3) force the new head sample when it is not itself a grid
/// point, so the head and the axis bounds always cover the last step.
fn append_series(
    series: &mut Series2D,
    equity: &[EquityPoint],
    old_len: usize,
    new_len: usize,
    stride: usize,
) {
    // (1) Drop the previous non-grid forced-last sample; the loop below re-derives the
    // tail for the new head.
    if old_len >= 1 && !(old_len - 1).is_multiple_of(stride) {
        let _ = series.x.pop();
        let _ = series.y.pop();
    }
    // (2) Append every grid sample in `[old_len, new_len)` (the multiples of `stride`).
    let mut idx = round_up_multiple(old_len, stride);
    while idx < new_len {
        if let Some(point) = equity.get(idx) {
            push_point_into(series, point);
        }
        idx = match idx.checked_add(stride) {
            Some(next) => next,
            None => break,
        };
    }
    // (3) Force the new head sample when it is not a grid point.
    if new_len >= 1
        && !(new_len - 1).is_multiple_of(stride)
        && let Some(point) = equity.get(new_len - 1)
    {
        push_point_into(series, point);
    }
}

/// The smallest multiple of `stride` (`>= 1`) that is `>= a`. `a` is a slice length,
/// bounded well below `usize::MAX`, so the addition cannot overflow in practice.
fn round_up_multiple(a: usize, stride: usize) -> usize {
    let rem = a % stride;
    if rem == 0 { a } else { a + (stride - rem) }
}

/// Append one equity point as `(step, equity_cents)` `Decimal`s — exact, no `f64`.
fn push_point(point: &EquityPoint, xs: &mut Vec<Decimal>, ys: &mut Vec<Decimal>) {
    xs.push(Decimal::from(point.step));
    ys.push(Decimal::from(point.equity_cents));
}

/// Append one equity point directly into a [`Series2D`]'s parallel vectors — the
/// incremental counterpart of [`push_point`].
fn push_point_into(series: &mut Series2D, point: &EquityPoint) {
    series.x.push(Decimal::from(point.step));
    series.y.push(Decimal::from(point.equity_cents));
}

#[cfg(test)]
mod tests {
    use optionstratlib::prelude::Decimal;
    use optionstratlib::visualization::GraphData;

    use super::{EQUITY_SERIES_NAME, EquityGeometry, MAX_EQUITY_POINTS};
    use crate::replay::EquityPoint;

    fn point(step: u32, equity_cents: i64) -> EquityPoint {
        EquityPoint {
            step,
            ts_ns: 1_700_000_000_000_000_000 + i64::from(step),
            cash_cents: equity_cents,
            position_value_cents: 0,
            equity_cents,
            drawdown: 0.0,
        }
    }

    #[track_caller]
    fn xy(graph: &GraphData) -> (&Vec<Decimal>, &Vec<Decimal>) {
        match graph {
            GraphData::Series(series) => (&series.x, &series.y),
            other => panic!("expected a single Series, got {other:?}"),
        }
    }

    /// The peak drawdown of a slice built from scratch with `seed` — the reference the
    /// incremental path must match.
    fn full_drawdown(equity: &[EquityPoint], seed: i64) -> i64 {
        EquityGeometry::build(equity, seed).peak_drawdown_cents()
    }

    /// The `equity[..k]` prefix without panicking indexing (`clippy::indexing_slicing`
    /// is denied in tests); `k <= equity.len()` in every caller, so the fallback is
    /// unreachable.
    fn head(equity: &[EquityPoint], k: usize) -> &[EquityPoint] {
        equity.get(..k).unwrap_or(equity)
    }

    #[test]
    fn test_build_maps_step_to_equity_cents_in_order() {
        // Each point projects (step → equity_cents) as exact Decimals, in order — the
        // cents are preserved until the render edge (no premature $ conversion).
        let equity = vec![point(0, 1_000), point(1, 1_050), point(2, 990)];
        let geo = EquityGeometry::build(&equity, 0);
        let (x, y) = xy(geo.graph());
        assert_eq!(x.len(), 3, "one sample per step");
        assert_eq!(y.len(), x.len(), "x and y are paired");
        assert_eq!(x, &[Decimal::from(0), Decimal::from(1), Decimal::from(2)]);
        assert_eq!(
            y,
            &[
                Decimal::from(1_000),
                Decimal::from(1_050),
                Decimal::from(990)
            ],
            "y is exact integer cents, never dollars",
        );
    }

    #[test]
    fn test_build_empty_is_empty_series() {
        // No rows → an empty series the #23 adapter renders as the deliberate empty
        // state, never a fabricated point.
        let geo = EquityGeometry::build(&[], 0);
        let (x, y) = xy(geo.graph());
        assert!(x.is_empty() && y.is_empty());
        assert_eq!(geo.peak_drawdown_cents(), 0, "no rows → no drawdown");
        match geo.graph() {
            GraphData::Series(series) => assert_eq!(series.name, EQUITY_SERIES_NAME),
            other => panic!("expected a Series, got {other:?}"),
        }
    }

    #[test]
    fn test_build_downsamples_but_keeps_first_and_last() {
        // A run past the cap is stride-sampled to <= MAX_EQUITY_POINTS, and the first
        // and last steps are always retained so the head and the start stay on screen.
        let n = MAX_EQUITY_POINTS * 3 + 7;
        let equity: Vec<EquityPoint> = (0..n)
            .map(|i| {
                point(
                    u32::try_from(i).unwrap_or(u32::MAX),
                    1_000 + i64::try_from(i).unwrap_or(0),
                )
            })
            .collect();
        let geo = EquityGeometry::build(&equity, 0);
        let (x, y) = xy(geo.graph());
        assert!(
            x.len() <= MAX_EQUITY_POINTS,
            "downsampled at or below the cap"
        );
        assert_eq!(x.len(), y.len());
        assert_eq!(x.first(), Some(&Decimal::from(0)), "first step retained");
        let last_step = u32::try_from(n - 1).unwrap_or(u32::MAX);
        assert_eq!(
            x.last(),
            Some(&Decimal::from(last_step)),
            "last step retained"
        );
    }

    #[test]
    fn test_peak_drawdown_is_the_worst_dip_from_the_running_peak() {
        // 1000 → 1200 (new peak) → 900 (dd −300) → 1100 (recover) → 800 (dd −400 from
        // the 1200 peak). Seeded at the opening balance 1000, the worst is −400.
        let equity = vec![
            point(0, 1_000),
            point(1, 1_200),
            point(2, 900),
            point(3, 1_100),
            point(4, 800),
        ];
        assert_eq!(full_drawdown(&equity, 1_000), -400);
    }

    #[test]
    fn test_peak_drawdown_zero_when_monotonic_or_empty() {
        assert_eq!(full_drawdown(&[], 0), 0, "empty run has no drawdown");
        let climbing = vec![point(0, 100), point(1, 200), point(2, 300)];
        assert_eq!(
            full_drawdown(&climbing, 100),
            0,
            "a run that only climbs from the opening balance never draws down",
        );
    }

    #[test]
    fn test_peak_drawdown_handles_negative_equity() {
        // Equity can go negative; the drawdown is still exact integer cents.
        let equity = vec![point(0, 500), point(1, -300)];
        assert_eq!(full_drawdown(&equity, 500), -800);
    }

    #[test]
    fn test_peak_drawdown_seeded_from_opening_capital_shows_step0_loss() {
        // A strategy that LOSES on step 0: opening capital $10,000.00 (1_000_000c),
        // step-0 equity $9,900.00 (990_000c). Seeding the running peak from the opening
        // capital (not the first equity row) surfaces the −$100.00 step-0 loss as a
        // −10_000c drawdown at the head — invisible under a first-row seed (which would
        // report $0).
        let equity = vec![point(0, 990_000), point(1, 1_050_000)];
        assert_eq!(
            full_drawdown(&equity, 1_000_000),
            -10_000,
            "step-0 loss is a drawdown vs opening capital, not $0",
        );
        // The old first-row seeding (seed == the first equity row, 990_000c) would
        // report 0 — no dip below the first row.
        assert_eq!(
            full_drawdown(&equity, 990_000),
            0,
            "seeded from the first row, the step-0 loss is (wrongly) invisible",
        );
    }

    #[test]
    fn test_extend_forward_matches_full_rebuild_at_every_head() {
        // The equality proof: an incremental extend to any head lands on exactly the
        // same series + peak a from-scratch build at that head produces — across stride
        // boundaries (n crosses 512 and 1024). A non-monotonic curve dipping below the
        // seed exercises the running-peak fold.
        let n = MAX_EQUITY_POINTS * 2 + 40;
        let seed = 1_000_000i64;
        let equity: Vec<EquityPoint> = (0..n)
            .map(|i| {
                let wobble = (i64::try_from(i % 41).unwrap_or(0) - 20) * 1_500;
                point(u32::try_from(i).unwrap_or(u32::MAX), 1_000_000 + wobble)
            })
            .collect();
        let mut inc = EquityGeometry::build(head(&equity, 1), seed);
        for head_len in 2..=n {
            inc.extend_forward(head(&equity, head_len));
            let full = EquityGeometry::build(head(&equity, head_len), seed);
            assert_eq!(
                xy(inc.graph()),
                xy(full.graph()),
                "series mismatch at head {head_len} (incremental != full)",
            );
            assert_eq!(
                inc.peak_drawdown_cents(),
                full.peak_drawdown_cents(),
                "peak mismatch at head {head_len} (incremental != full)",
            );
            assert_eq!(inc.raw_len(), head_len, "raw_len tracks the head");
        }
    }

    #[test]
    fn test_extend_forward_no_new_rows_is_a_noop() {
        // A forward step within an equity gap adds no new rows; the geometry is
        // unchanged (idempotent), never a spurious rebuild.
        let equity = vec![point(0, 1_000), point(1, 1_050)];
        let mut geo = EquityGeometry::build(&equity, 1_000);
        let before = xy(geo.graph()).0.clone();
        let peak_before = geo.peak_drawdown_cents();
        geo.extend_forward(&equity); // same length → no-op
        assert_eq!(xy(geo.graph()).0, &before);
        assert_eq!(geo.peak_drawdown_cents(), peak_before);
        assert_eq!(geo.raw_len(), equity.len());
    }

    #[test]
    fn test_rebuild_recomputes_from_seed_on_a_backward_seek() {
        // After extending forward to a deep head, a backward seek rebuilds from the
        // preserved opening-capital seed over the shorter slice — equal to a
        // from-scratch build at that shorter head.
        let seed = 1_000_000i64;
        let equity: Vec<EquityPoint> = (0..40)
            .map(|i| point(i, 1_000_000 - i64::from(i) * 2_000))
            .collect();
        let mut geo = EquityGeometry::build(head(&equity, 1), seed);
        geo.extend_forward(head(&equity, 30));
        // A backward seek to head 5 (a shorter slice) rebuilds from the seed.
        geo.rebuild(head(&equity, 5));
        let full = EquityGeometry::build(head(&equity, 5), seed);
        assert_eq!(xy(geo.graph()), xy(full.graph()), "backward rebuild series");
        assert_eq!(
            geo.peak_drawdown_cents(),
            full.peak_drawdown_cents(),
            "backward rebuild peak (seed preserved)",
        );
        assert_eq!(geo.raw_len(), 5);
    }
}
