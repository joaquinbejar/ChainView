//! Off-draw payoff-curve geometry (#27, `docs/05-views-and-ux.md` §4).
//!
//! This module samples the committed builder legs into **one**
//! [`GraphData::Series`](optionstratlib::visualization::GraphData) per curve mode
//! — the expiration payoff and the t+0 curve — and derives the break-even points,
//! entirely in the **application** layer and entirely **off** the draw path. It is
//! invoked from [`PayoffBuilder::commit`](super::PayoffBuilder) and the market-tick
//! refresh, never from `terminal.draw` (`docs/02-tui-architecture.md` §7).
//!
//! # Why it builds a `Series` itself
//!
//! `optionstratlib`'s `Graph::graph_data()` for a multi-leg strategy returns a
//! `GraphData::MultiSeries`, which ChainView's #23 adapter renders as
//! `Empty(Unsupported)` (deferred to #47). So #27 samples
//! [`Profit`](optionstratlib::pricing::Profit)-equivalent per-leg P&L across a
//! price grid into a single `Series2D` the #23 adapter projects directly — the
//! curve geometry is `optionstratlib` math, never hand-rolled Black-Scholes.
//!
//! # Frozen entry premium — the two curves share one cost basis (#27 SF-1)
//!
//! On `commit` the per-leg entry premiums (the chain mid, P0) are **frozen** into a
//! [`Position`] vector cached on the committed strategy. Both curves price against
//! that frozen basis: the expiration payoff and the t+0 reprice read the frozen P0,
//! never the live chain mid. So after a quote moves (P0→P1) the t+0 tick refresh
//! (`rebuild_tplus0`) mutates **only** the sampled underlying and the current per-leg
//! IV — the premium stays P0 — making the t+0 curve a locked-entry mark-to-market
//! that shows the accrued unrealized P&L at spot and still converges to the frozen
//! expiration line at the wings. Re-reading the current mark as the entry premium
//! each tick (the prior bug) hid unrealized P&L and split the two curves' cost basis.
//!
//! # Split IV requirement: expiration is IV-free, t+0 needs a plausible IV (#27 SF-2)
//!
//! The **expiration** payoff and the break-evens are IV-independent, so they build
//! from the frozen marks alone and render whenever every leg has a mark. The **t+0**
//! curve prices Black-Scholes, so it requires a *plausible* IV per leg: a leg IV that
//! is absent, or [`GreeksOrigin::ComputedLocally`] and below
//! [`MIN_PLAUSIBLE_LOCAL_IV`] (0.5%, the #25 floor relocated to the domain), makes the
//! t+0 curve **unavailable** (an empty series → the "t+0 unavailable" state) while the
//! expiration curve still renders. A **venue** (`Provider`) IV is trusted as-is, never
//! floored — exactly as in the chain matrix (#25). This gates out a #83-mispriced
//! inverse-contract IV that would otherwise collapse the t+0 curve onto expiration.
//!
//! # Determinism
//!
//! Every input is read from the borrowed [`ChainStore`] snapshot — the marks, the
//! per-leg IV (the #24 sidecar), the underlying, and a **stored** reference instant
//! ([`ChainStore::last_full_poll`]) — never `Utc::now()` and never an RNG. Time to
//! expiry is priced with [`ExpirationDate::Days`], whose year fraction is
//! `days / 365` (clock-free). The build is a pure function of `(legs, store
//! snapshot)`, so identical inputs yield an identical series.
//!
//! # The break-even scan is grid-derived, not the ctor scan
//!
//! `CustomStrategy::new` runs an unbounded `O(underlying / 0.01)` break-even scan
//! in its constructor (~6M iterations at BTC spot) that would freeze the render
//! thread on every commit; per the #27 API map's prohibitive-scan tolerance note,
//! the break-evens are instead read off the expiration series' sign changes
//! (linear-interpolated), an `O(grid)` pass. No `CustomStrategy` is constructed on
//! **any** path, so the tick refresh trivially never runs the scan.

use chrono::{DateTime, Utc};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::model::Position;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::visualization::{GraphData, Series2D, TraceMode};
use optionstratlib::{ExpirationDate, OptionType, Options, Side as OptionSide};

use super::{BuilderLeg, Side};
use crate::chain::{
    ChainStore, DEFAULT_DIVIDEND_YIELD, DEFAULT_RISK_FREE_RATE, GreeksOrigin, InstrumentKey,
    MIN_PLAUSIBLE_LOCAL_IV,
};

/// The number of price samples per payoff curve. A fixed point count keeps the
/// series bounded regardless of the underlying's magnitude (a BTC spot near
/// 60 000 would otherwise blow a fixed-step grid to millions of points).
const GRID_POINTS: usize = 121;

/// The fraction of the anchored strike/spot range added below the lowest and above
/// the highest anchor, so the payoff wings and every kink are visible.
const GRID_MARGIN: f64 = 0.3;

/// Seconds per day for the deterministic day-count.
const SECONDS_PER_DAY: i64 = 86_400;

/// A valid placeholder IV used only to construct the frozen entry positions. The
/// expiration payoff is IV-independent (it never reads it) and the t+0 reprice
/// overwrites it per leg with the current plausible IV, so its value is immaterial —
/// it exists only because [`Options`] requires a strictly-positive volatility.
const ENTRY_IV_PLACEHOLDER: Positive = Positive::ONE;

/// The projected geometry for a committed strategy: the shared price grid, the
/// **frozen** commit-time entry positions, the two curve series (expiration + t+0),
/// and the break-even points — all built off the draw path.
pub(crate) struct PayoffGeometry {
    /// The shared underlying-price x-grid both series and the tick refresh reuse.
    pub(crate) grid: Vec<Positive>,
    /// The frozen commit-time positions (entry premium P0 per leg). Both curves and
    /// the t+0 tick refresh reprice these — the premium never re-reads the live mark.
    pub(crate) entry_positions: Vec<Position>,
    /// The expiration payoff as a single `GraphData::Series` (price → P&L).
    pub(crate) expiration: GraphData,
    /// The t+0 (mark-based) curve as a single `GraphData::Series` (price → P&L), or
    /// an empty series when any leg lacks a plausible IV (the "t+0 unavailable" state).
    pub(crate) tplus0: GraphData,
    /// The break-even underlying prices, read off the expiration series.
    pub(crate) break_evens: Vec<Positive>,
}

/// An empty `GraphData::Series` — the "no curve yet" projection input that the #23
/// adapter renders as `Empty(NoData)` (the deliberate "add a leg" / "curve
/// unavailable" state).
#[must_use]
pub(crate) fn empty_series() -> GraphData {
    GraphData::Series(Series2D::default())
}

/// Build the full payoff geometry for `legs` against the `store` snapshot, or
/// `None` when the legs cannot be priced for **expiration** (a missing mark or a
/// non-future expiry). The t+0 curve requires a plausible IV per leg on top of that
/// and degrades to an empty series when one is missing — the expiration curve still
/// renders. Off the draw path — invoked only from `commit`.
#[must_use]
pub(crate) fn build_geometry(legs: &[BuilderLeg], store: &ChainStore) -> Option<PayoffGeometry> {
    let chain = store.chain();
    // Freeze the entry premiums (P0) at commit; both curves share this cost basis.
    let entry_positions = build_entry_positions(legs, store)?;
    let grid = price_grid(legs, chain.underlying_price)?;
    let expiration = expiration_series(&entry_positions, &grid);
    let break_evens = break_even_points(&expiration);
    // The t+0 curve needs a plausible IV per leg; unavailable → an empty series.
    let tplus0 = tplus0_curve(&entry_positions, legs, store, &grid);
    Some(PayoffGeometry {
        grid,
        entry_positions,
        expiration,
        tplus0,
        break_evens,
    })
}

/// Rebuild **only** the t+0 series by repricing the **frozen** `entry_positions`
/// against the current `store` snapshot on the committed `grid`: the entry premium
/// stays frozen (P0) and only the sampled underlying and the current per-leg IV
/// move. Returns the empty series when any leg lacks a plausible IV (the "t+0
/// unavailable" state). Constructs **no** `CustomStrategy` and touches neither the
/// expiration series nor the break-evens — the tick-path refresh that never runs the
/// break-even scan (#27 SF-1).
#[must_use]
pub(crate) fn rebuild_tplus0(
    legs: &[BuilderLeg],
    store: &ChainStore,
    grid: &[Positive],
    entry_positions: &[Position],
) -> GraphData {
    tplus0_curve(entry_positions, legs, store, grid)
}

/// Build the **frozen** commit-time `optionstratlib` positions for `legs` from the
/// `store` snapshot, or `None` when any leg lacks a mark or the expiry is not a
/// future absolute instant. The premium is the chain mid **at commit** (P0, frozen
/// for the life of the commit), the rate/dividend the documented defaults, and the
/// time to expiry a deterministic `Days` offset from a stored reference instant. IV
/// is **not** required here — the expiration payoff is IV-independent and the t+0
/// reprice resolves a plausible IV per leg — so the option is constructed with
/// [`ENTRY_IV_PLACEHOLDER`], never read by expiration and overwritten for t+0.
#[must_use]
fn build_entry_positions(legs: &[BuilderLeg], store: &ChainStore) -> Option<Vec<Position>> {
    let chain = store.chain();
    let expiration_utc = absolute_expiry(chain)?;
    let as_of = store.last_full_poll()?;
    let dte = days_between(as_of, expiration_utc)?;
    let mut positions = Vec::with_capacity(legs.len());
    for leg in legs {
        let premium = leg.mark_in(chain)?;
        let quantity = Positive::new(f64::from(leg.qty)).ok()?;
        let side = match leg.side {
            Side::Buy => OptionSide::Long,
            Side::Sell => OptionSide::Short,
        };
        let option = Options::new(
            OptionType::European,
            side,
            chain.symbol.clone(),
            leg.strike,
            ExpirationDate::Days(dte),
            ENTRY_IV_PLACEHOLDER,
            quantity,
            chain.underlying_price,
            DEFAULT_RISK_FREE_RATE,
            leg.style,
            DEFAULT_DIVIDEND_YIELD,
            None,
        );
        // The reference instant doubles as the position's open date; the payoff
        // math reads only `ExpirationDate::Days`, so the date never enters the
        // curve — it is carried for determinism, never a wall-clock read.
        positions.push(Position::new(
            option,
            premium,
            as_of,
            Positive::ZERO,
            Positive::ZERO,
            None,
            None,
        ));
    }
    Some(positions)
}

/// The t+0 curve for the frozen `entry_positions`: reprice them with the current
/// plausible per-leg IV and sample across `grid`, or the empty series when any leg
/// lacks a plausible IV (the "t+0 unavailable — no reliable IV" state). The frozen
/// entry premium is never re-read.
#[must_use]
fn tplus0_curve(
    entry_positions: &[Position],
    legs: &[BuilderLeg],
    store: &ChainStore,
    grid: &[Positive],
) -> GraphData {
    match repriced_for_tplus0(entry_positions, legs, store) {
        Some(priced) => tplus0_series(&priced, grid),
        None => empty_series(),
    }
}

/// Clone the frozen `entry_positions` and set each leg's **current** plausible IV
/// for the t+0 reprice — mutating only the volatility (the underlying is swept per
/// grid sample in [`tplus0_pnl`]) and never the frozen entry premium. `None` when any
/// leg lacks a plausible IV, so the t+0 curve degrades to unavailable rather than
/// mixing a real IV with a fabricated one.
#[must_use]
fn repriced_for_tplus0(
    entry_positions: &[Position],
    legs: &[BuilderLeg],
    store: &ChainStore,
) -> Option<Vec<Position>> {
    let ivs = resolve_leg_ivs(legs, store)?;
    let mut priced = Vec::with_capacity(entry_positions.len());
    for (position, iv) in entry_positions.iter().zip(ivs.iter()) {
        let mut position = position.clone();
        position.option.implied_volatility = *iv;
        priced.push(position);
    }
    Some(priced)
}

/// Resolve a **plausible** t+0 IV for every leg (in leg order), or `None` when any
/// leg lacks one — the all-or-nothing gate that makes the t+0 curve unavailable.
#[must_use]
fn resolve_leg_ivs(legs: &[BuilderLeg], store: &ChainStore) -> Option<Vec<Positive>> {
    let expiration_utc = absolute_expiry(store.chain())?;
    let mut ivs = Vec::with_capacity(legs.len());
    for leg in legs {
        ivs.push(resolve_plausible_iv(store, leg, expiration_utc)?);
    }
    Some(ivs)
}

/// The absolute-UTC expiry of `chain`, or `None` when the chain carries a relative
/// (`Days`) or unparseable expiry — matching the #24 sidecar, which keys on the
/// same absolute instant. Exhaustive over [`ExpirationDate`] with no wildcard.
#[must_use]
fn absolute_expiry(chain: &OptionChain) -> Option<DateTime<Utc>> {
    match chain.get_expiration()? {
        ExpirationDate::DateTime(dt) => Some(dt),
        ExpirationDate::Days(_) => None,
    }
}

/// Resolve a leg's t+0 pricing IV with the #25 plausibility gate: the #24 sidecar
/// value (venue-supplied or locally inverted) if usable and plausible, else the
/// chain's per-strike (venue) IV. `None` when no leg IV is a usable, plausible
/// volatility — which makes the t+0 curve unavailable while the (IV-independent)
/// expiration curve still renders.
///
/// The sidecar value claims the resolution when usable: a `Provider` value is
/// trusted as-is, a `ComputedLocally` value must additionally clear
/// [`MIN_PLAUSIBLE_LOCAL_IV`] or it is rejected (no silent fall-through to the chain
/// field), exactly mirroring the chain matrix (#25). The chain per-strike fallback
/// is the shared venue IV, trusted as-is.
#[must_use]
fn resolve_plausible_iv(
    store: &ChainStore,
    leg: &BuilderLeg,
    expiration_utc: DateTime<Utc>,
) -> Option<Positive> {
    let chain = store.chain();
    let key = InstrumentKey {
        underlying: chain.symbol.clone(),
        expiration_utc,
        strike: leg.strike,
        style: leg.style,
    };
    if let Some(sidecar) = store.leg_greeks(&key)
        && let Some(iv) = sidecar.iv
        && usable_iv(iv)
    {
        return plausible_leg_iv(iv, sidecar.iv_origin);
    }
    let od = chain
        .options
        .iter()
        .find(|o| o.strike_price == leg.strike)?;
    usable_iv(od.implied_volatility).then_some(od.implied_volatility)
}

/// Apply the #25 plausibility gate to a usable leg IV: a `Provider` (venue) IV is
/// trusted as-is; a `ComputedLocally` IV must clear [`MIN_PLAUSIBLE_LOCAL_IV`] (0.5%)
/// or it is rejected (`None`), so a #83-mispriced near-zero local inversion never
/// feeds Black-Scholes and silently collapses the t+0 curve onto expiration.
#[must_use]
fn plausible_leg_iv(iv: Positive, origin: GreeksOrigin) -> Option<Positive> {
    match origin {
        GreeksOrigin::Provider => Some(iv),
        GreeksOrigin::ComputedLocally => (iv.to_dec() >= MIN_PLAUSIBLE_LOCAL_IV).then_some(iv),
    }
}

/// Whether an implied volatility is usable for pricing: strictly positive and not
/// the non-finite [`Positive::INFINITY`] sentinel.
#[must_use]
fn usable_iv(iv: Positive) -> bool {
    iv > Positive::ZERO && iv != Positive::INFINITY
}

/// The deterministic fractional-day span from `as_of` to `expiration_utc`, or
/// `None` when the span is non-positive (an expired input). Integer seconds and
/// `Decimal`, so no `f64` enters the day-count — mirrors the #24 sidecar's kernel.
#[must_use]
fn days_between(as_of: DateTime<Utc>, expiration_utc: DateTime<Utc>) -> Option<Positive> {
    let seconds = expiration_utc.signed_duration_since(as_of).num_seconds();
    if seconds <= 0 {
        return None;
    }
    let days = Decimal::from(seconds).checked_div(Decimal::from(SECONDS_PER_DAY))?;
    Positive::new_decimal(days).ok()
}

/// The shared underlying-price x-grid: a fixed [`GRID_POINTS`] samples spanning the
/// leg strikes and the spot widened by [`GRID_MARGIN`] each side, or `None` when
/// the range is degenerate (non-finite or collapsed).
#[must_use]
fn price_grid(legs: &[BuilderLeg], spot: Positive) -> Option<Vec<Positive>> {
    let spot_f = spot.to_f64();
    let mut lo = spot_f;
    let mut hi = spot_f;
    for leg in legs {
        let strike = leg.strike.to_f64();
        lo = lo.min(strike);
        hi = hi.max(strike);
    }
    let lo = lo * (1.0 - GRID_MARGIN);
    let hi = hi * (1.0 + GRID_MARGIN);
    if !lo.is_finite() || !hi.is_finite() || hi <= lo {
        return None;
    }
    let span = hi - lo;
    let last = GRID_POINTS.checked_sub(1)?;
    let divisor = f64::from(u16::try_from(last).ok()?);
    let mut grid = Vec::with_capacity(GRID_POINTS + legs.len());
    for i in 0..GRID_POINTS {
        let numerator = f64::from(u16::try_from(i).ok()?);
        let x = lo + span * (numerator / divisor);
        if let Ok(point) = Positive::new(x) {
            grid.push(point);
        }
    }
    // Snap every leg STRIKE into the grid as an explicit sample point (FIX 5, #27,
    // architect/dpe #28 note). The break-even sign-change scan and the
    // piecewise-linear reconstruction are exact only when no payoff kink falls
    // strictly between two grid samples; anchoring each strike makes the
    // reconstruction and every between-adjacent-strike crossing exact for typical
    // spreads. Each strike lies within (lo, hi) by construction (`lo`/`hi` widen the
    // strike span), so it never extends the range. A remaining tolerance — two
    // break-even crossings closer together than one uniform step — is a #28 concern,
    // not addressed by this snap.
    for leg in legs {
        if leg.strike.to_f64().is_finite() {
            grid.push(leg.strike);
        }
    }
    // Sort ascending and drop exact duplicates so the series stays monotonic for the
    // linear-interpolation break-even scan (a strike may coincide with a uniform
    // sample).
    grid.sort_by_key(|point| point.to_dec());
    grid.dedup();
    if grid.len() < 2 {
        return None;
    }
    Some(grid)
}

/// Sample the expiration payoff (Σ per-leg `pnl_at_expiration`) across `grid` into a
/// single `GraphData::Series`. A price point whose P&L cannot be computed is
/// skipped (never fabricated), and the #23 adapter's finite gate is the final
/// guard at the render edge.
#[must_use]
fn expiration_series(positions: &[Position], grid: &[Positive]) -> GraphData {
    let mut xs = Vec::with_capacity(grid.len());
    let mut ys = Vec::with_capacity(grid.len());
    for price in grid {
        if let Some(pnl) = expiration_pnl(positions, price) {
            xs.push(price.to_dec());
            ys.push(pnl);
        }
    }
    series("payoff @ expiration", xs, ys)
}

/// Sample the t+0 (mark-based) curve across `grid` into a single
/// `GraphData::Series`, per the #27 recipe
/// `y(S) = expiration(S) + Σ_legs[ signed_BS(S)·qty − intrinsic(S) ]`. A price point
/// whose pricing errors is skipped.
#[must_use]
fn tplus0_series(positions: &[Position], grid: &[Positive]) -> GraphData {
    let mut xs = Vec::with_capacity(grid.len());
    let mut ys = Vec::with_capacity(grid.len());
    for price in grid {
        if let Some(pnl) = tplus0_pnl(positions, price) {
            xs.push(price.to_dec());
            ys.push(pnl);
        }
    }
    series("payoff @ t+0", xs, ys)
}

/// The expiration P&L at `price`: the sum of each leg's `pnl_at_expiration`, or
/// `None` on any pricing error or checked-overflow.
#[must_use]
fn expiration_pnl(positions: &[Position], price: &Positive) -> Option<Decimal> {
    let mut total = Decimal::ZERO;
    for position in positions {
        let pnl = position.pnl_at_expiration(&Some(price)).ok()?;
        total = total.checked_add(pnl)?;
    }
    Some(total)
}

/// The t+0 P&L at `price`: the expiration P&L plus each leg's time value
/// (`signed_BS(price)·qty − intrinsic(price)`), or `None` on any pricing error.
/// The Black-Scholes and intrinsic values are `optionstratlib`'s, never
/// hand-rolled.
#[must_use]
fn tplus0_pnl(positions: &[Position], price: &Positive) -> Option<Decimal> {
    let mut total = expiration_pnl(positions, price)?;
    for position in positions {
        let mut option = position.option.clone();
        option.underlying_price = *price;
        let bs = option.calculate_price_black_scholes().ok()?;
        let mark = bs.checked_mul(option.quantity.to_dec())?;
        let intrinsic = option.intrinsic_value(*price).ok()?;
        let time_value = mark.checked_sub(intrinsic)?;
        total = total.checked_add(time_value)?;
    }
    Some(total)
}

/// Read the break-even underlying prices off the expiration series' zero crossings
/// (linear-interpolated), an `O(grid)` pass — never the `CustomStrategy` ctor scan.
/// Returns an empty vector for a non-`Series` `GraphData` (exhaustive, no wildcard).
#[must_use]
fn break_even_points(expiration: &GraphData) -> Vec<Positive> {
    let series = match expiration {
        GraphData::Series(series) => series,
        GraphData::MultiSeries(_) | GraphData::GraphSurface(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut prev: Option<(Decimal, Decimal)> = None;
    for (x, y) in series.x.iter().zip(series.y.iter()) {
        if let Some((px, py)) = prev
            && crosses_zero(py, *y)
            && let Some(root) = interpolate_zero(px, py, *x, *y)
            && let Ok(point) = Positive::new_decimal(root)
        {
            out.push(point);
        }
        prev = Some((*x, *y));
    }
    out
}

/// Whether the P&L changes sign (touching or crossing zero) between two
/// consecutive samples.
#[must_use]
fn crosses_zero(prev: Decimal, next: Decimal) -> bool {
    (prev < Decimal::ZERO && next >= Decimal::ZERO)
        || (prev > Decimal::ZERO && next <= Decimal::ZERO)
}

/// The linearly-interpolated underlying price where the segment `(x0, y0)–(x1, y1)`
/// crosses zero, or `None` when the segment is flat (no unique root).
#[must_use]
fn interpolate_zero(x0: Decimal, y0: Decimal, x1: Decimal, y1: Decimal) -> Option<Decimal> {
    let dy = y1.checked_sub(y0)?;
    if dy == Decimal::ZERO {
        return None;
    }
    let dx = x1.checked_sub(x0)?;
    let step = y0.checked_mul(dx)?.checked_div(dy)?;
    x0.checked_sub(step)
}

/// Assemble a named 2-point-mode `Series2D` line from paired coordinate vectors.
#[must_use]
fn series(name: &str, x: Vec<Decimal>, y: Vec<Decimal>) -> GraphData {
    GraphData::Series(Series2D {
        x,
        y,
        name: name.to_owned(),
        mode: TraceMode::Lines,
        line_color: None,
        line_width: Some(2.0),
    })
}

#[cfg(test)]
mod tests {
    use optionstratlib::model::Position;
    use optionstratlib::prelude::{Decimal, Positive};
    use optionstratlib::pricing::Profit;
    use optionstratlib::strategies::custom::CustomStrategy;
    use optionstratlib::visualization::{GraphData, Series2D};
    use optionstratlib::{ExpirationDate, OptionStyle, OptionType, Options, Side as OptionSide};

    use super::{
        GreeksOrigin, MIN_PLAUSIBLE_LOCAL_IV, break_even_points, expiration_series,
        plausible_leg_iv, tplus0_series,
    };

    // --- Constructors (no unwrap/expect/indexing per the ruleset) ------------

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    /// A single European option position at a small spot, so a `CustomStrategy`
    /// cross-check runs its ctor break-even scan cheaply.
    fn leg(side: OptionSide, style: OptionStyle, strike: f64, premium: f64) -> Position {
        let option = Options::new(
            OptionType::European,
            side,
            "TEST".to_owned(),
            pos(strike),
            ExpirationDate::Days(pos(30.0)),
            pos(0.3),
            Positive::ONE,
            pos(100.0),
            Decimal::ZERO,
            style,
            Positive::ZERO,
            None,
        );
        Position::new(
            option,
            pos(premium),
            match chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0) {
                Some(dt) => dt,
                None => panic!("bad fixed test instant"),
            },
            Positive::ZERO,
            Positive::ZERO,
            None,
            None,
        )
    }

    /// A long call vertical with a real 2.0 net debit: buy the 100 call at 3.0,
    /// sell the 105 call at 1.0 — so the P&L crosses zero (a true break-even).
    fn call_spread() -> Vec<Position> {
        vec![
            leg(OptionSide::Long, OptionStyle::Call, 100.0, 3.0),
            leg(OptionSide::Short, OptionStyle::Call, 105.0, 1.0),
        ]
    }

    fn grid() -> Vec<Positive> {
        [80.0, 90.0, 100.0, 102.5, 105.0, 110.0, 120.0]
            .into_iter()
            .map(pos)
            .collect()
    }

    #[track_caller]
    fn xy(graph: &GraphData) -> (&Series2D,) {
        match graph {
            GraphData::Series(series) => (series,),
            GraphData::MultiSeries(_) | GraphData::GraphSurface(_) => {
                panic!("expected a single Series, got {graph:?}")
            }
        }
    }

    #[track_caller]
    fn assert_close(actual: Decimal, expected: Decimal) {
        let diff = (actual - expected).abs();
        assert!(
            diff < Decimal::new(1, 4),
            "expected {expected}, got {actual} (diff {diff})",
        );
    }

    // --- The expiration series equals optionstratlib's calculate_profit_at ----

    #[test]
    fn test_expiration_series_matches_calculate_profit_at() {
        // A #27-produced expiration curve must equal `optionstratlib`'s own
        // `Profit::calculate_profit_at` at every sampled price (the geometry #28's
        // goldens will pin, produced here). `CustomStrategy::new` is only used in
        // THIS cross-check test (small spot → cheap scan), never in production.
        let positions = call_spread();
        let grid = grid();
        let expiration = expiration_series(&positions, &grid);
        let (series,) = xy(&expiration);
        assert_eq!(series.x.len(), grid.len(), "one sample per grid point");

        let strategy = match CustomStrategy::new(
            "t".to_owned(),
            "TEST".to_owned(),
            "spread".to_owned(),
            pos(100.0),
            positions,
            pos(0.01),
            1_000,
            Positive::ONE,
        ) {
            Ok(s) => s,
            Err(e) => panic!("CustomStrategy::new failed: {e}"),
        };
        for (x, y) in series.x.iter().zip(series.y.iter()) {
            let price = match Positive::new_decimal(*x) {
                Ok(p) => p,
                Err(e) => panic!("grid x not positive `{x}`: {e}"),
            };
            let expected = match strategy.calculate_profit_at(&price) {
                Ok(v) => v,
                Err(e) => panic!("calculate_profit_at failed at {price}: {e}"),
            };
            assert_close(*y, expected);
        }
    }

    // --- The t+0 curve is a distinct, well-formed series ---------------------

    #[test]
    fn test_tplus0_series_is_nonempty_and_differs_from_expiration() {
        // The t+0 curve carries time value, so it is a DIFFERENT series from the
        // expiration payoff (they converge only at the wings).
        let positions = call_spread();
        let grid = grid();
        let expiration = expiration_series(&positions, &grid);
        let tplus0 = tplus0_series(&positions, &grid);
        let (exp,) = xy(&expiration);
        let (t0,) = xy(&tplus0);
        assert_eq!(t0.x.len(), grid.len(), "t+0 samples every grid point");
        assert_ne!(exp.y, t0.y, "the t+0 curve differs from expiration");
    }

    // --- Break-evens read off the expiration sign changes --------------------

    #[test]
    fn test_break_even_points_from_expiration_sign_changes() {
        // A long call spread paid a debit → one break-even between the strikes.
        let positions = call_spread();
        // A fine grid so the sign change is captured near the true root.
        let grid: Vec<Positive> = (0..=200).map(|i| pos(60.0 + f64::from(i))).collect();
        let expiration = expiration_series(&positions, &grid);
        let break_evens = break_even_points(&expiration);
        assert!(
            !break_evens.is_empty(),
            "a debit call spread has a break-even between its strikes",
        );
        for be in &break_evens {
            let value = be.to_f64();
            assert!(
                (100.0..=105.0).contains(&value),
                "break-even {value} sits between the 100 and 105 strikes",
            );
        }
    }

    #[test]
    fn test_break_even_points_of_non_series_is_empty() {
        // A non-`Series` GraphData (deferred #47 variants) yields no break-evens —
        // the exhaustive, wildcard-free match.
        let multi = GraphData::MultiSeries(vec![Series2D::default()]);
        assert!(break_even_points(&multi).is_empty());
    }

    // --- Determinism: same inputs → identical series -------------------------

    #[test]
    fn test_series_build_is_deterministic() {
        // The build is a pure function of (positions, grid): no clock, no RNG, so
        // two builds from identical inputs are byte-for-byte identical.
        let grid = grid();
        let first = tplus0_series(&call_spread(), &grid);
        let second = tplus0_series(&call_spread(), &grid);
        assert_eq!(first, second, "identical inputs yield an identical series");
        let e1 = expiration_series(&call_spread(), &grid);
        let e2 = expiration_series(&call_spread(), &grid);
        assert_eq!(e1, e2, "the expiration series is deterministic too");
    }

    // --- The #25 plausibility gate on the t+0 leg IV (SF-2) ------------------

    #[test]
    fn test_plausible_leg_iv_floors_local_but_trusts_venue() {
        // A sub-floor value (below MIN_PLAUSIBLE_LOCAL_IV = 0.5%): rejected when it was
        // computed LOCALLY (a #83-mispriced near-zero inversion), trusted when it is a
        // venue value — exactly the #25 chain-matrix policy, shared via the domain
        // constant.
        let sub_floor = match Positive::new_decimal(MIN_PLAUSIBLE_LOCAL_IV) {
            Ok(floor) => match Positive::new(floor.to_f64() / 2.0) {
                Ok(v) => v,
                Err(e) => panic!("half-floor is positive: {e}"),
            },
            Err(e) => panic!("floor is positive: {e}"),
        };
        assert_eq!(
            plausible_leg_iv(sub_floor, GreeksOrigin::ComputedLocally),
            None,
            "a sub-floor LOCAL IV is rejected (t+0 unavailable, not a fake curve)",
        );
        assert_eq!(
            plausible_leg_iv(sub_floor, GreeksOrigin::Provider),
            Some(sub_floor),
            "a sub-floor VENUE IV is trusted as-is (never floored)",
        );
        // A plausible local IV (well above the floor) passes.
        let plausible = pos(0.35);
        assert_eq!(
            plausible_leg_iv(plausible, GreeksOrigin::ComputedLocally),
            Some(plausible),
            "a plausible local IV passes the floor",
        );
    }
}
