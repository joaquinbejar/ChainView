//! Off-draw replay equity geometry (#35, `docs/05-views-and-ux.md` §5,
//! `docs/04-replay-mode.md` §4).
//!
//! This module projects the timeline cursor's as-of equity slice
//! (`equity[..=head]`) into **one** [`GraphData::Series`](optionstratlib::visualization::GraphData)
//! (step → equity, in **integer cents**) and derives the peak-drawdown figure —
//! entirely in the **application** layer and entirely **off** the draw path. It is
//! invoked from [`LoadedReplay`](super::LoadedReplay) on load and on every cursor
//! move (a seek or a playback tick), never from `terminal.draw`
//! (`docs/02-tui-architecture.md` §7).
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

/// Build the equity line series (step → equity **cents**) from the as-of equity
/// slice `equity` (`equity[..=head]`), stride-sampled to at most
/// [`MAX_EQUITY_POINTS`] points. An empty slice yields an empty series the #23
/// adapter renders as the deliberate "no equity rows" empty state — never a
/// fabricated line. Off the draw path.
#[must_use]
pub(crate) fn build_equity_series(equity: &[EquityPoint]) -> GraphData {
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
/// keeping at most [`MAX_EQUITY_POINTS`] points and always including the **last**
/// point (the head). A slice at or below the cap is copied verbatim.
fn push_sampled(equity: &[EquityPoint], xs: &mut Vec<Decimal>, ys: &mut Vec<Decimal>) {
    let n = equity.len();
    let Some(last) = n.checked_sub(1) else {
        return; // empty slice → an empty series
    };
    // `div_ceil` keeps the sample count at or below the cap; `.max(1)` guarantees
    // forward progress even when `n <= MAX_EQUITY_POINTS`.
    let stride = n.div_ceil(MAX_EQUITY_POINTS).max(1);
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

/// Append one equity point as `(step, equity_cents)` `Decimal`s — exact, no `f64`.
fn push_point(point: &EquityPoint, xs: &mut Vec<Decimal>, ys: &mut Vec<Decimal>) {
    xs.push(Decimal::from(point.step));
    ys.push(Decimal::from(point.equity_cents));
}

/// The **peak drawdown** in integer cents over the as-of equity slice `equity`: the
/// most-negative running `equity − running_max_equity`, in `(−∞, 0]`. Computed over
/// the **full** slice (never the downsampled series) with checked integer arithmetic
/// — no `f64` money — so the figure is exact. `0` for an empty slice or a run that
/// only ever climbed.
#[must_use]
pub(crate) fn peak_drawdown_cents(equity: &[EquityPoint]) -> i64 {
    let mut peak: Option<i64> = None;
    let mut worst: i64 = 0;
    for point in equity {
        let running_peak = match peak {
            Some(cur) if cur >= point.equity_cents => cur,
            _ => point.equity_cents,
        };
        peak = Some(running_peak);
        // `equity − running_peak` is `<= 0`; a checked underflow on an absurd input
        // is skipped (defensive) rather than fabricating a `0` drawdown.
        if let Some(drawdown) = point.equity_cents.checked_sub(running_peak)
            && drawdown < worst
        {
            worst = drawdown;
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use optionstratlib::prelude::Decimal;
    use optionstratlib::visualization::GraphData;

    use super::{EQUITY_SERIES_NAME, MAX_EQUITY_POINTS, build_equity_series, peak_drawdown_cents};
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

    #[test]
    fn test_build_equity_series_maps_step_to_equity_cents_in_order() {
        // Each point projects (step → equity_cents) as exact Decimals, in order — the
        // cents are preserved until the render edge (no premature $ conversion).
        let equity = vec![point(0, 1_000), point(1, 1_050), point(2, 990)];
        let graph = build_equity_series(&equity);
        let (x, y) = xy(&graph);
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
    fn test_build_equity_series_empty_is_empty_series() {
        // No rows → an empty series the #23 adapter renders as the deliberate empty
        // state, never a fabricated point.
        let graph = build_equity_series(&[]);
        let (x, y) = xy(&graph);
        assert!(x.is_empty() && y.is_empty());
        match &graph {
            GraphData::Series(series) => assert_eq!(series.name, EQUITY_SERIES_NAME),
            other => panic!("expected a Series, got {other:?}"),
        }
    }

    #[test]
    fn test_build_equity_series_downsamples_but_keeps_first_and_last() {
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
        let graph = build_equity_series(&equity);
        let (x, y) = xy(&graph);
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
    fn test_peak_drawdown_cents_is_the_worst_dip_from_the_running_peak() {
        // 1000 → 1200 (new peak) → 900 (dd −300) → 1100 (recover) → 800 (dd −400 from
        // the 1200 peak). The worst is −400.
        let equity = vec![
            point(0, 1_000),
            point(1, 1_200),
            point(2, 900),
            point(3, 1_100),
            point(4, 800),
        ];
        assert_eq!(peak_drawdown_cents(&equity), -400);
    }

    #[test]
    fn test_peak_drawdown_cents_zero_when_monotonic_or_empty() {
        assert_eq!(peak_drawdown_cents(&[]), 0, "empty run has no drawdown");
        let climbing = vec![point(0, 100), point(1, 200), point(2, 300)];
        assert_eq!(
            peak_drawdown_cents(&climbing),
            0,
            "a run that only climbs never draws down",
        );
    }

    #[test]
    fn test_peak_drawdown_cents_handles_negative_equity() {
        // Equity can go negative; the drawdown is still exact integer cents.
        let equity = vec![point(0, 500), point(1, -300)];
        assert_eq!(peak_drawdown_cents(&equity), -800);
    }
}
