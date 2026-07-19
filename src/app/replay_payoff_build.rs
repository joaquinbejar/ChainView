//! Off-draw **replay payoff-at-head** geometry (#49, `docs/04-replay-mode.md` §6,
//! `docs/05-views-and-ux.md` §5).
//!
//! This module derives the payoff of the **open position at the scrub head** —
//! resolved from `positions.parquet` by the timeline cursor (#33) — into the same
//! shapes the live payoff (#27) produces: an **expiration** payoff
//! [`GraphData::Series`](optionstratlib::visualization::GraphData) sampled from
//! `optionstratlib`'s `Profit`/`Payoff` math, its break-even prices, and the
//! position's current **mark-to-market** P&L. It runs entirely in the **application**
//! layer and entirely **off** the draw path: it is invoked from
//! [`LoadedReplay`](super::LoadedReplay) on load and on every cursor move (a seek or a
//! playback tick), never from `terminal.draw` (`docs/02-tui-architecture.md` §7).
//!
//! # The data source is the bundle, not a live chain (#49 vs #27)
//!
//! The live payoff (#27) prices committed builder legs against a streaming
//! [`ChainStore`](crate::chain::ChainStore). This build differs **only** in its data
//! source: the open [`PositionRow`]s at the head. Each leg's strike, style and
//! expiry live **only inside the `contract_id` join key** — `positions.parquet`
//! carries no structured strike/style columns (`docs/04-replay-mode.md` §2.2) — so
//! they are recovered with the crate's already-validated
//! [`parse_contract_id`](crate::replay::parse_contract_id) (#32 proved every id
//! parses before this runs).
//!
//! # Money is integer cents → `Positive` at this checked seam
//!
//! Every monetary column (`strike_cents`, `avg_price_cents`, `unrealized_cents`) is
//! integer cents. The current mark-to-market P&L ([`net_unrealized_cents`]) is the
//! **checked integer sum of the writer's own per-row `unrealized_cents`** — the writer
//! already applied its contract-multiplier and fee conventions, so the reader never
//! re-derives it. (A `(mark − avg_price) · qty` recompute drops the multiplier and reads
//! 100x too small for a standard 100-multiplier option — the second-pass #108 finding.)
//! It stays exact **integer cents**, formatted to `$` only at the render edge (#49 draw).
//!
//! The strikes/premiums that shape the **expiration curve** convert into an
//! `optionstratlib` `Positive` dollar amount **here**, through [`positive_from_cents`], a
//! **checked** `u64 → i64 → Decimal(scale 2) → Positive` path, and the price grid
//! ([`price_grid`]) is built entirely in `Decimal` — **no `f64` on the money route**
//! (`CLAUDE.md` numeric policy). The #23 adapter converts to plot `f64` only at the UI
//! edge, where the finite gate drops any non-finite coordinate.
//!
//! # The expiration curve is **per contract** (the bundle carries no multiplier)
//!
//! `optionstratlib`'s payoff applies each leg's `quantity`, but the bundle carries the
//! venue **contract multiplier** only implicitly inside the writer's `unrealized_cents`
//! — there is no structured multiplier column (`docs/04-replay-mode.md` §2.2). So the
//! expiration series y-axis is **per-contract-notional** dollars, not portfolio dollars,
//! and is labelled as such ([`REPLAY_SERIES_NAME`]); the panel must not present it as
//! portfolio dollars until a multiplier enters the coordinated bundle contract (an ADR /
//! schema event, owned by `architect`). The **break-even** underlying prices are
//! multiplier-independent and exact regardless.
//!
//! # No bit-exact upstream reprice (the honest scope, `docs/04-replay-mode.md` §6)
//!
//! The bundle does **not** carry the step's underlying spot, IV, rate or dividend, so
//! a bit-exact t+0 reprice is **not derivable** and is never fabricated. The
//! **expiration** payoff is fully determined by the legs alone (it is IV- and
//! time-independent), so it is the honest curve this panel renders; the row `mark`
//! contributes the current **mark-to-market** reference level, not a repriced curve.
//! The DTE is computed from each leg's `expiration_ns` (recovered from the join key)
//! against the head step's `ts_ns` on the deterministic `Days` convention — a
//! clock-free span (never `Utc::now`), so the build is a pure function of the open set
//! and the head timestamp.

use optionstratlib::model::Position;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::visualization::GraphData;
use optionstratlib::{ExpirationDate, OptionType, Options, Side as OptionSide};

use super::payoff_build::{break_even_points, empty_series, expiration_series};
use crate::chain::{DEFAULT_DIVIDEND_YIELD, DEFAULT_RISK_FREE_RATE};
use crate::replay::{PositionRow, PositionSide, parse_contract_id};

/// The number of price samples per payoff curve — the fixed point count that keeps
/// the series bounded regardless of the underlying's magnitude (mirrors #27).
const GRID_POINTS: usize = 121;

/// The grid margin as exact `Decimal` tenths (`3/10` = 0.3): the fraction of the strike
/// range added below the lowest and above the highest strike so the payoff wings and
/// every kink stay visible (mirrors #27's margin). Integer parts → `Decimal`, so the
/// money-derived grid is built with **no `f64`** ([`price_grid`]).
const GRID_MARGIN_TENTHS: i64 = 3;

/// The name given to the replay expiration series, carrying the honest **per-contract**
/// caveat: the bundle carries no contract-multiplier column, so this curve's y-axis is
/// per-contract-notional dollars, never portfolio dollars (see the module docs).
// Short enough for ratatui's legend-width constraint (a longer label makes the
// whole legend vanish); the ALWAYS-VISIBLE per-contract disclosure lives on the
// panel caveat line (src/ui/payoff.rs), which cannot be elided.
const REPLAY_SERIES_NAME: &str = "payoff @ expiration";

/// Nanoseconds per calendar day — the deterministic `Days` day-count divisor for the
/// head DTE (integer ns → `Decimal` days, so no `f64` enters the day-count).
const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// A valid placeholder IV used only to construct the payoff positions. The expiration
/// payoff is IV-independent (it never reads it), so its value is immaterial — it
/// exists only because [`Options`] requires a strictly-positive volatility (mirrors
/// #27's `ENTRY_IV_PLACEHOLDER`).
const IV_PLACEHOLDER: Positive = Positive::ONE;

/// The projected replay payoff-at-head geometry: the expiration payoff series, its
/// break-even prices, the current net mark-to-market P&L in **integer cents**, and
/// the count of open legs at the head — all built off the draw path and cached on
/// [`LoadedReplay`](super::LoadedReplay).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplayPayoffGeometry {
    /// The expiration payoff as a single `GraphData::Series` (underlying → P&L in
    /// **per-contract-notional** dollars — the bundle carries no contract multiplier, so
    /// this y-axis is per contract, never portfolio dollars; see the module docs), or an
    /// empty series when there is no open position at the head (the "flat at this step"
    /// state) or the legs cannot be priced.
    pub(crate) graph: GraphData,
    /// The break-even underlying prices, read off the expiration series (empty when
    /// the curve is flat/degenerate).
    pub(crate) break_evens: Vec<Positive>,
    /// The current net mark-to-market P&L in **integer cents** (signed), or `None`
    /// when the head is flat (no open leg) or the sum overflows. It is the **checked sum
    /// of the writer's own per-row `unrealized_cents`** across the open legs — the
    /// writer-authoritative figure that already applies the contract multiplier and fee
    /// conventions (the reader never re-derives it, so a standard 100-multiplier option is
    /// not 100x too small).
    pub(crate) mark_pnl_cents: Option<i64>,
    /// The number of open legs at the head (`0` ⇒ the "flat at this step" state).
    pub(crate) open_legs: usize,
}

impl ReplayPayoffGeometry {
    /// The deliberate **flat** geometry — an empty series, no break-evens, no mark
    /// P&L, zero open legs. The head has no open position, so the payoff panel renders
    /// its "flat at this step" empty state (never a fabricated curve).
    #[must_use]
    fn flat() -> Self {
        Self {
            graph: empty_series(),
            break_evens: Vec::new(),
            mark_pnl_cents: None,
            open_legs: 0,
        }
    }
}

/// Build the replay payoff-at-head geometry from the cursor's cached open-position
/// set `open` and the head step's `head_ts_ns` (for the deterministic DTE).
///
/// `open` is the timeline cursor's [`open_positions`](crate::TimelineCursor::open_positions)
/// result — cached on state at seek time, never re-derived per frame. An empty set is
/// the "flat at this step" state. Off the draw path, a pure function of `(open,
/// head_ts_ns)`.
#[must_use]
pub(crate) fn build(open: &[&PositionRow], head_ts_ns: Option<i64>) -> ReplayPayoffGeometry {
    if open.is_empty() {
        return ReplayPayoffGeometry::flat();
    }
    let open_legs = open.len();
    let mark_pnl_cents = net_unrealized_cents(open);
    let positions = build_positions(open, head_ts_ns);
    let Some(grid) = price_grid(&positions) else {
        // The legs could not be priced (an unparseable id or a degenerate strike
        // range): an honest empty series, never a fabricated line. The open-leg count
        // and mark P&L still surface in the header.
        return ReplayPayoffGeometry {
            graph: empty_series(),
            break_evens: Vec::new(),
            mark_pnl_cents,
            open_legs,
        };
    };
    let graph = per_contract_expiration_series(&positions, &grid);
    let break_evens = break_even_points(&graph);
    ReplayPayoffGeometry {
        graph,
        break_evens,
        mark_pnl_cents,
        open_legs,
    }
}

/// The shared `optionstratlib` expiration payoff series, relabelled with the honest
/// **per-contract** caveat ([`REPLAY_SERIES_NAME`]). The curve math is identical to the
/// live payoff (#27) — only the label differs, because the bundle carries no contract
/// multiplier so the y-axis is per-contract-notional dollars, not portfolio dollars (see
/// the module docs). A non-`Series` result (never produced by [`expiration_series`]) is
/// passed through unchanged.
#[must_use]
fn per_contract_expiration_series(positions: &[Position], grid: &[Positive]) -> GraphData {
    match expiration_series(positions, grid) {
        GraphData::Series(mut series) => {
            REPLAY_SERIES_NAME.clone_into(&mut series.name);
            GraphData::Series(series)
        }
        other => other,
    }
}

/// The net current mark-to-market P&L in **integer cents** across the open legs: the
/// **checked sum of the writer's own per-row `unrealized_cents`**. The writer already
/// applied its contract-multiplier and fee conventions and encoded the long/short sign,
/// so the reader sums the field verbatim and never re-derives it (a `(mark − avg) · qty`
/// recompute drops the multiplier and reads 100x too small for a standard 100-multiplier
/// option). All checked integer arithmetic — no `f64` on the money path; `None` on an
/// (absurd) overflow so the header reads `—` rather than a wrapped figure.
#[must_use]
fn net_unrealized_cents(open: &[&PositionRow]) -> Option<i64> {
    let mut total: i64 = 0;
    for row in open {
        total = total.checked_add(row.unrealized_cents)?;
    }
    Some(total)
}

/// Build the `optionstratlib` positions for the open legs — the entry premium is the
/// leg's `avg_price_cents` (converted at the checked cents→`Positive` seam), the DTE
/// the deterministic head span. A leg whose `contract_id` fails to parse or whose
/// cents do not convert is **skipped** (never a panic, never a fabricated leg); on a
/// validated bundle every leg converts.
#[must_use]
fn build_positions(open: &[&PositionRow], head_ts_ns: Option<i64>) -> Vec<Position> {
    let mut positions = Vec::with_capacity(open.len());
    for row in open {
        let Ok(parsed) = parse_contract_id(&row.contract_id) else {
            continue;
        };
        let (Some(strike), Some(premium)) = (
            positive_from_cents(parsed.strike_cents),
            positive_from_cents(row.avg_price_cents),
        ) else {
            continue;
        };
        let Ok(quantity) = Positive::new_decimal(Decimal::from(row.quantity)) else {
            continue;
        };
        let side = match row.side {
            PositionSide::Long => OptionSide::Long,
            PositionSide::Short => OptionSide::Short,
        };
        let dte = head_dte(parsed.expiration_ns, head_ts_ns);
        let option = Options::new(
            OptionType::European,
            side,
            parsed.underlying.clone(),
            strike,
            ExpirationDate::Days(dte),
            IV_PLACEHOLDER,
            quantity,
            // The expiration P&L is evaluated at the grid sample (`pnl_at_expiration`
            // takes the price), so the option's own `underlying_price` is never read;
            // the strike is a sensible, in-range placeholder.
            strike,
            DEFAULT_RISK_FREE_RATE,
            parsed.style,
            DEFAULT_DIVIDEND_YIELD,
            None,
        );
        positions.push(Position::new(
            option,
            premium,
            // The head step's instant as the open date, resolved infallibly and
            // clock-free (`from_timestamp_nanos`, never `Utc::now`). The expiration
            // P&L reads only `ExpirationDate::Days`, so the date never enters the
            // curve — it is carried for determinism.
            chrono::DateTime::from_timestamp_nanos(head_ts_ns.unwrap_or(0)),
            Positive::ZERO,
            Positive::ZERO,
            None,
            None,
        ));
    }
    positions
}

/// The deterministic fractional-day span from the head step's `head_ts_ns` to the
/// leg's `expiration_ns`, on the `Days` convention (integer nanoseconds → `Decimal`
/// days, clock-free). Falls back to a nominal one day when the head timestamp is
/// absent or the span is non-positive — the expiration payoff is time-independent, so
/// the DTE never changes the curve; it only keeps each constructed leg honest.
#[must_use]
fn head_dte(expiration_ns: i64, head_ts_ns: Option<i64>) -> Positive {
    let span = head_ts_ns
        .and_then(|head| expiration_ns.checked_sub(head))
        .filter(|ns| *ns > 0)
        .and_then(|ns| Decimal::from(ns).checked_div(Decimal::from(NANOS_PER_DAY)))
        .and_then(|days| Positive::new_decimal(days).ok());
    span.unwrap_or(Positive::ONE)
}

/// The checked cents→`Positive` seam: `u64` cents → `i64` → `Decimal` at scale 2 (an
/// exact dollar amount) → `Positive`. **No `f64` on the money path** (`CLAUDE.md`).
/// `None` when the cents value does not fit `i64` (an absurd input), so the caller
/// skips the leg rather than fabricating a price.
#[must_use]
fn positive_from_cents(cents: u64) -> Option<Positive> {
    let mantissa = i64::try_from(cents).ok()?;
    Positive::new_decimal(Decimal::new(mantissa, 2)).ok()
}

/// The shared underlying-price x-grid for the expiration series: [`GRID_POINTS`]
/// samples spanning the leg strikes widened by the grid margin (0.3) each side, with
/// each strike snapped in as an exact sample so the break-even sign-change scan is exact.
/// `None` when there are no priceable legs or the range is degenerate (mirrors #27's
/// grid without a live spot anchor).
///
/// The whole grid is built in `Decimal` (the strikes are bundle strike **money**), so
/// the bounds and every sample stay exact — **no `f64` on the money route**; the plot
/// `f64` is produced only later, by the #23 adapter at the UI edge (`CLAUDE.md`).
#[must_use]
fn price_grid(positions: &[Position]) -> Option<Vec<Positive>> {
    let mut bounds: Option<(Decimal, Decimal)> = None;
    for position in positions {
        let strike = position.option.strike_price.to_dec();
        bounds = Some(match bounds {
            Some((lo, hi)) => (lo.min(strike), hi.max(strike)),
            None => (strike, strike),
        });
    }
    let (lo, hi) = bounds?;
    // Widen the strike span by the grid margin (0.3) each side, all in exact `Decimal`.
    let margin = Decimal::new(GRID_MARGIN_TENTHS, 1);
    let lo = lo.checked_mul(Decimal::ONE.checked_sub(margin)?)?;
    let hi = hi.checked_mul(Decimal::ONE.checked_add(margin)?)?;
    if hi <= lo {
        return None;
    }
    let span = hi.checked_sub(lo)?;
    let last = GRID_POINTS.checked_sub(1)?;
    let divisor = Decimal::from(u32::try_from(last).ok()?);
    let mut grid = Vec::with_capacity(GRID_POINTS + positions.len());
    for i in 0..GRID_POINTS {
        let numerator = Decimal::from(u32::try_from(i).ok()?);
        // x = lo + span·i / (GRID_POINTS − 1); multiply before divide to keep precision.
        let offset = span.checked_mul(numerator)?.checked_div(divisor)?;
        let x = lo.checked_add(offset)?;
        if let Ok(point) = Positive::new_decimal(x) {
            grid.push(point);
        }
    }
    // Snap every leg STRIKE in as an explicit sample so no payoff kink falls strictly
    // between two grid samples — the break-even reconstruction stays exact.
    for position in positions {
        if position.option.strike_price != Positive::INFINITY {
            grid.push(position.option.strike_price);
        }
    }
    grid.sort_by_key(|point| point.to_dec());
    grid.dedup();
    if grid.len() < 2 {
        return None;
    }
    Some(grid)
}

#[cfg(test)]
mod tests {
    use optionstratlib::prelude::Decimal;
    use optionstratlib::visualization::GraphData;

    use super::{REPLAY_SERIES_NAME, build, net_unrealized_cents, positive_from_cents};
    use crate::replay::{PositionRow, PositionSide};

    const HEAD_TS: i64 = 1_700_000_000_000_000_000;
    const EXP_NS: i64 = 1_735_286_400_000_000_000;

    /// A `positions` row whose join key encodes `strike_cents` / `style` (the columns
    /// `positions.parquet` does not carry), plus the entry/mark cents and the writer's
    /// own `unrealized_cents` (the multiplier-applied MTM the reader sums verbatim). The
    /// `build`/`net_unrealized_cents` seams never read `position_id`/`exit_reason` (the
    /// cursor already resolved the open set), so they are fixed here.
    fn row(
        style: char,
        strike_cents: u64,
        side: PositionSide,
        quantity: u32,
        avg_price_cents: u64,
        mark_cents: u64,
        unrealized_cents: i64,
    ) -> PositionRow {
        PositionRow {
            step: 0,
            ts_ns: HEAD_TS,
            position_id: 1,
            trade_id: 7,
            contract_id: format!("v1:BTC:{EXP_NS}:{strike_cents}:{style}"),
            side,
            quantity,
            avg_price_cents,
            mark_cents,
            unrealized_cents,
            stale_mark: false,
            exit_reason: None,
            open_at_end: false,
        }
    }

    #[track_caller]
    fn series_x_len(graph: &GraphData) -> usize {
        match graph {
            GraphData::Series(series) => {
                assert_eq!(series.x.len(), series.y.len(), "x and y are paired");
                series.x.len()
            }
            other => panic!("expected a single Series, got {other:?}"),
        }
    }

    // --- cents → Positive is a checked, f64-free seam ------------------------

    #[test]
    fn test_positive_from_cents_is_exact_dollars() {
        // 12_345 cents = $123.45 exactly (Decimal scale 2, never an f64 round-trip).
        match positive_from_cents(12_345) {
            Some(p) => assert_eq!(p.to_dec(), optionstratlib::prelude::Decimal::new(12_345, 2)),
            None => panic!("12_345 cents must convert"),
        }
        assert!(
            positive_from_cents(0).is_some(),
            "zero cents is a valid $0.00"
        );
    }

    // --- flat state: no open legs → an empty series --------------------------

    #[test]
    fn test_build_flat_when_no_open_positions() {
        let geometry = build(&[], Some(HEAD_TS));
        assert_eq!(geometry.open_legs, 0, "no legs at the head");
        assert_eq!(geometry.mark_pnl_cents, None, "flat has no mark P&L");
        assert!(geometry.break_evens.is_empty());
        assert_eq!(series_x_len(&geometry.graph), 0, "the flat series is empty");
    }

    // --- a real open position builds an expiration curve + break-even --------

    #[test]
    fn test_build_long_call_has_expiration_curve_and_break_even() {
        // A single long call at strike $60,000 bought for $125 (12_500 cents), marked
        // at $118 (11_800 cents). The expiration payoff crosses zero one tick above the
        // strike (strike + premium), so there is exactly one break-even, and the curve
        // is a real, non-empty series.
        let legs = [row(
            'C',
            6_000_000,
            PositionSide::Long,
            1,
            12_500,
            11_800,
            -700,
        )];
        let open: Vec<&PositionRow> = legs.iter().collect();
        let geometry = build(&open, Some(HEAD_TS));
        assert_eq!(geometry.open_legs, 1);
        assert!(
            series_x_len(&geometry.graph) >= 2,
            "a priced leg yields a sampled curve",
        );
        assert_eq!(
            geometry.break_evens.len(),
            1,
            "a long call has one break-even (strike + premium)",
        );
        for be in &geometry.break_evens {
            let value = be.to_f64();
            assert!(
                (60_000.0..=61_000.0).contains(&value),
                "the break-even sits just above the strike: {value}",
            );
        }
    }

    // --- mark P&L: the writer's summed `unrealized_cents`, verbatim ----------

    #[test]
    fn test_net_unrealized_cents_sums_the_writer_field() {
        // The MTM is the checked sum of the writer's OWN per-row `unrealized_cents`
        // (already signed and multiplier-applied), never a `(mark − avg) · qty`
        // recompute. Here the writer's figures are −700 and +200.
        let long = row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800, -700);
        assert_eq!(net_unrealized_cents(&[&long]), Some(-700));
        let short = row('C', 6_000_000, PositionSide::Short, 1, 12_500, 11_800, 200);
        assert_eq!(net_unrealized_cents(&[&short]), Some(200));
        // A two-leg net simply sums the two writer figures: −700 + 200 = −500.
        assert_eq!(net_unrealized_cents(&[&long, &short]), Some(-500));
    }

    #[test]
    fn test_net_unrealized_cents_is_not_the_multiplier_dropping_recompute() {
        // A standard 100-multiplier option: the writer's `unrealized_cents` carries the
        // 100x the naive `(mark − avg) · qty` recompute drops. The reader must surface the
        // writer's −70_000 (−$700.00), NOT the −700 (−$7.00) it would recompute — the
        // second-pass #108 fix (a standard leg is not 100x too small).
        // `(mark − avg) · qty` at qty 1 (the multiplier-dropping recompute).
        let recompute_cents = 11_800_i64 - 12_500; // −700
        let writer_unrealized = recompute_cents * 100; // −70_000 (multiplier applied)
        let leg = row(
            'C',
            6_000_000,
            PositionSide::Long,
            1,
            12_500,
            11_800,
            writer_unrealized,
        );
        assert_eq!(
            net_unrealized_cents(&[&leg]),
            Some(-70_000),
            "the MTM is the writer's multiplier-applied unrealized",
        );
        assert_ne!(
            net_unrealized_cents(&[&leg]),
            Some(recompute_cents),
            "never the 100x-too-small recompute",
        );
    }

    // --- a two-leg spread builds a coherent curve ----------------------------

    #[test]
    fn test_build_vertical_spread_prices_both_legs() {
        // Long 60k call + short 62k call: a real call vertical → a non-empty curve and
        // a break-even between the two strikes. The header MTM sums the writer's own
        // per-row unrealized figures (−700 + 200 = −500), not a reader recompute.
        let long = row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800, -700);
        let short = row('C', 6_200_000, PositionSide::Short, 1, 5_000, 4_800, 200);
        let open = vec![&long, &short];
        let geometry = build(&open, Some(HEAD_TS));
        assert_eq!(geometry.open_legs, 2);
        assert!(series_x_len(&geometry.graph) >= 2, "both legs priced");
        assert_eq!(geometry.mark_pnl_cents, Some(-500));
    }

    // --- the price grid is exact Decimal, never an f64 round-trip ------------

    #[test]
    fn test_price_grid_is_exact_decimal_not_f64() {
        // The grid's lower bound is min_strike · (1 − 0.3) = 60_000 · 0.7 = exactly
        // $42,000.00 in Decimal, and it is the first (lowest) sample of the expiration
        // series. The old f64 path (0.7_f64 · 60_000.0) landed at 41_999.999… — this pins
        // the Decimal build (Fix: bundle strike money stays Decimal to the UI edge).
        let legs = [row(
            'C',
            6_000_000,
            PositionSide::Long,
            1,
            12_500,
            11_800,
            -700,
        )];
        let open: Vec<&PositionRow> = legs.iter().collect();
        let geometry = build(&open, Some(HEAD_TS));
        match &geometry.graph {
            GraphData::Series(series) => assert_eq!(
                series.x.first().copied(),
                Some(Decimal::from(42_000)),
                "the grid lo is exact Decimal 0.7·60000, no f64 round-trip",
            ),
            other => panic!("expected a series, got {other:?}"),
        }
    }

    // --- the replay curve carries the honest per-contract label --------------

    #[test]
    fn test_replay_series_is_labelled_per_contract() {
        // The bundle carries no contract multiplier, so the curve is per-contract-notional
        // dollars and must be labelled as such (never portfolio dollars).
        let legs = [row(
            'C',
            6_000_000,
            PositionSide::Long,
            1,
            12_500,
            11_800,
            -700,
        )];
        let open: Vec<&PositionRow> = legs.iter().collect();
        let geometry = build(&open, Some(HEAD_TS));
        match &geometry.graph {
            GraphData::Series(series) => assert_eq!(series.name, REPLAY_SERIES_NAME),
            other => panic!("expected a series, got {other:?}"),
        }
    }

    // --- determinism: same inputs → identical geometry -----------------------

    #[test]
    fn test_build_is_deterministic() {
        let leg = row('P', 5_800_000, PositionSide::Short, 1, 9_000, 8_500, 500);
        let open = vec![&leg];
        assert_eq!(
            build(&open, Some(HEAD_TS)),
            build(&open, Some(HEAD_TS)),
            "identical inputs yield an identical geometry (no clock, no RNG)",
        );
    }

    // --- a malformed leg is skipped, never a panic ---------------------------

    #[test]
    fn test_build_skips_unparseable_contract_id_without_panic() {
        let mut bad = row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800, -700);
        bad.contract_id = "not-a-valid-id".to_owned();
        let open = vec![&bad];
        let geometry = build(&open, Some(HEAD_TS));
        // The leg still counts as open (the header stays honest), but it cannot price,
        // so the curve is an empty series rather than a fabricated line.
        assert_eq!(geometry.open_legs, 1);
        assert_eq!(series_x_len(&geometry.graph), 0);
    }

    #[test]
    fn test_style_recovered_from_join_key() {
        // The put leg's style comes only from the `contract_id` (`positions.parquet`
        // carries no `style` column); the build recovers it and prices a put.
        let put = row('P', 6_000_000, PositionSide::Long, 1, 12_500, 11_800, -700);
        let open = vec![&put];
        let geometry = build(&open, Some(HEAD_TS));
        assert_eq!(geometry.open_legs, 1);
        // A long put has one break-even (strike − premium), just below the strike.
        assert_eq!(geometry.break_evens.len(), 1);
        for be in &geometry.break_evens {
            assert!(
                (59_000.0..=60_000.0).contains(&be.to_f64()),
                "the put break-even sits just below the strike: {}",
                be.to_f64(),
            );
        }
    }
}
