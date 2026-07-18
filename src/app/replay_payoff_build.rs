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
//! Every monetary column (`strike_cents`, `avg_price_cents`, `mark_cents`) is integer
//! cents; the conversion into an `optionstratlib` `Positive` dollar amount happens
//! **here**, through [`positive_from_cents`], a **checked** `u64 → i64 → Decimal(scale
//! 2) → Positive` path with **no `f64` on the money route** (`CLAUDE.md` numeric
//! policy). The current mark-to-market P&L is kept as exact **integer cents**
//! ([`net_mark_pnl_cents`]) and formatted to `$` only at the render edge (#49 draw).
//! The expiration series y-axis is authored in `Decimal` dollars; the #23 adapter
//! converts to plot `f64` at the UI edge, where the finite gate drops any non-finite
//! coordinate.
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

/// The fraction of the strike range added below the lowest and above the highest
/// strike, so the payoff wings and every kink are visible (mirrors #27's margin).
const GRID_MARGIN: f64 = 0.3;

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
    /// dollars), or an empty series when there is no open position at the head (the
    /// "flat at this step" state) or the legs cannot be priced.
    pub(crate) graph: GraphData,
    /// The break-even underlying prices, read off the expiration series (empty when
    /// the curve is flat/degenerate).
    pub(crate) break_evens: Vec<Positive>,
    /// The current net mark-to-market P&L in **integer cents** (signed), or `None`
    /// when the head is flat (no open leg) or the sum overflows. `Σ side · qty ·
    /// (mark − avg_price)` — ChainView's own per-contract figure, not the bundle's
    /// `unrealized_cents` (whose contract multiplier is not carried), so it stays in
    /// the same per-contract units as the dollar expiration curve.
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
    let mark_pnl_cents = net_mark_pnl_cents(open);
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
    let graph = expiration_series(&positions, &grid);
    let break_evens = break_even_points(&graph);
    ReplayPayoffGeometry {
        graph,
        break_evens,
        mark_pnl_cents,
        open_legs,
    }
}

/// The net current mark-to-market P&L in **integer cents** across the open legs:
/// `Σ side · qty · (mark_cents − avg_price_cents)` (long `+`, short `−`). All checked
/// integer arithmetic — no `f64` on the money path; `None` on an (absurd) overflow so
/// the header reads `—` rather than a wrapped figure.
#[must_use]
fn net_mark_pnl_cents(open: &[&PositionRow]) -> Option<i64> {
    let mut total: i64 = 0;
    for row in open {
        let mark = i64::try_from(row.mark_cents).ok()?;
        let avg = i64::try_from(row.avg_price_cents).ok()?;
        let diff = mark.checked_sub(avg)?;
        let signed_qty = i64::from(row.quantity);
        let leg = diff.checked_mul(signed_qty)?;
        let leg = match row.side {
            PositionSide::Long => leg,
            PositionSide::Short => leg.checked_neg()?,
        };
        total = total.checked_add(leg)?;
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
/// samples spanning the leg strikes widened by [`GRID_MARGIN`] each side, with each
/// strike snapped in as an exact sample so the break-even sign-change scan is exact.
/// `None` when there are no priceable legs or the range is degenerate (mirrors #27's
/// grid without a live spot anchor).
#[must_use]
fn price_grid(positions: &[Position]) -> Option<Vec<Positive>> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for position in positions {
        let strike = position.option.strike_price.to_f64();
        lo = lo.min(strike);
        hi = hi.max(strike);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return None;
    }
    let lo = lo * (1.0 - GRID_MARGIN);
    let hi = hi * (1.0 + GRID_MARGIN);
    if !lo.is_finite() || !hi.is_finite() || hi <= lo {
        return None;
    }
    let span = hi - lo;
    let last = GRID_POINTS.checked_sub(1)?;
    let divisor = f64::from(u16::try_from(last).ok()?);
    let mut grid = Vec::with_capacity(GRID_POINTS + positions.len());
    for i in 0..GRID_POINTS {
        let numerator = f64::from(u16::try_from(i).ok()?);
        let x = lo + span * (numerator / divisor);
        if let Ok(point) = Positive::new(x) {
            grid.push(point);
        }
    }
    // Snap every leg STRIKE in as an explicit sample so no payoff kink falls strictly
    // between two grid samples — the break-even reconstruction stays exact.
    for position in positions {
        if position.option.strike_price.to_f64().is_finite() {
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
    use optionstratlib::visualization::GraphData;

    use super::{build, net_mark_pnl_cents, positive_from_cents};
    use crate::replay::{PositionRow, PositionSide};

    const HEAD_TS: i64 = 1_700_000_000_000_000_000;
    const EXP_NS: i64 = 1_735_286_400_000_000_000;

    /// A `positions` row whose join key encodes `strike_cents` / `style` (the columns
    /// `positions.parquet` does not carry), plus the entry/mark cents under test. The
    /// `build`/`net_mark_pnl_cents` seams never read `position_id`/`exit_reason` (the
    /// cursor already resolved the open set), so they are fixed here.
    fn row(
        style: char,
        strike_cents: u64,
        side: PositionSide,
        quantity: u32,
        avg_price_cents: u64,
        mark_cents: u64,
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
            unrealized_cents: 0,
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
        let legs = [row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800)];
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

    // --- mark P&L: signed integer cents, side-aware --------------------------

    #[test]
    fn test_net_mark_pnl_cents_is_side_aware_integer_cents() {
        // Long: (mark 11_800 − entry 12_500) × 1 = −700 cents.
        let long = row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800);
        assert_eq!(net_mark_pnl_cents(&[&long]), Some(-700));
        // Short the same leg: the sign flips to +700 cents.
        let short = row('C', 6_000_000, PositionSide::Short, 1, 12_500, 11_800);
        assert_eq!(net_mark_pnl_cents(&[&short]), Some(700));
        // A two-leg net: −700 + (+700 × 2 qty) = +700.
        let short2 = row('C', 6_000_000, PositionSide::Short, 2, 12_500, 11_800);
        assert_eq!(net_mark_pnl_cents(&[&long, &short2]), Some(700));
    }

    // --- a two-leg spread builds a coherent curve ----------------------------

    #[test]
    fn test_build_vertical_spread_prices_both_legs() {
        // Long 60k call + short 62k call: a real call vertical → a non-empty curve and
        // a break-even between the two strikes.
        let long = row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800);
        let short = row('C', 6_200_000, PositionSide::Short, 1, 5_000, 4_800);
        let open = vec![&long, &short];
        let geometry = build(&open, Some(HEAD_TS));
        assert_eq!(geometry.open_legs, 2);
        assert!(series_x_len(&geometry.graph) >= 2, "both legs priced");
        assert_eq!(
            geometry.mark_pnl_cents,
            // long (11_800 − 12_500) + short −(4_800 − 5_000) = −700 + 200 = −500
            Some(-500),
        );
    }

    // --- determinism: same inputs → identical geometry -----------------------

    #[test]
    fn test_build_is_deterministic() {
        let leg = row('P', 5_800_000, PositionSide::Short, 1, 9_000, 8_500);
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
        let mut bad = row('C', 6_000_000, PositionSide::Long, 1, 12_500, 11_800);
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
        let put = row('P', 6_000_000, PositionSide::Long, 1, 12_500, 11_800);
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
