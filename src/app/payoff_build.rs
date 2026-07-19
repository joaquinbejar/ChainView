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
//! (`rebuild_tplus0`) mutates **only** the sampled underlying, the current per-leg
//! IV, and the time-to-expiry (so it theta-decays as the app is held open, #27 SF-3)
//! — the entry premium stays P0 — making the t+0 curve a locked-entry
//! mark-to-market that shows the accrued unrealized P&L at spot and still converges to
//! the frozen expiration line at the wings. Re-reading the current mark as the entry
//! premium each tick (the prior bug) hid unrealized P&L and split the two curves' cost
//! basis.
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
//! (the commit-time [`ChainStore::last_full_poll`] seeds the frozen entry positions;
//! the latest `ChainStore::analytics_as_of` reprices the t+0 DTE each rebuild, #27
//! SF-3) — never `Utc::now()` and never an RNG. Time to expiry is priced with
//! [`ExpirationDate::Days`], whose year fraction is `days / 365` (clock-free). The
//! build is a pure function of `(legs, store snapshot)`, so identical inputs yield an
//! identical series.
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
/// stays frozen (P0) and only the sampled underlying, the current per-leg IV, and the
/// time-to-expiry (recomputed from the latest analytics instant, so the curve
/// theta-decays, #27 SF-3) move. Returns the empty series when any leg lacks a
/// plausible IV or the expiry is no longer in the future (the "t+0 unavailable"
/// state). Constructs **no** `CustomStrategy` and touches neither the expiration
/// series nor the break-evens — the tick-path refresh that never runs the break-even
/// scan (#27 SF-1).
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
/// plausible per-leg IV and the fresh time-to-expiry (#27 SF-3), then sample across
/// `grid`, or the empty series when any leg lacks a plausible IV or the expiry is no
/// longer in the future (the "t+0 unavailable" state). The frozen entry premium is
/// never re-read.
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

/// Clone the frozen `entry_positions` and reprice each leg for the t+0 curve —
/// setting its **current** plausible IV and its **fresh** time-to-expiry (the
/// underlying is swept per grid sample in [`tplus0_pnl`]) — while never touching the
/// frozen entry premium. `None` when any leg lacks a plausible IV, or when the
/// time-to-expiry is no longer a future span, so the t+0 curve degrades to
/// unavailable rather than mixing a real input with a fabricated one.
///
/// # Theta decay (#27 SF-3)
///
/// The frozen `entry_positions` carry the DTE captured **at commit**; repricing them
/// against that stale DTE would freeze the t+0 curve at the entry instant's
/// time-to-expiry, so held open for hours it would never theta-decay. The DTE is
/// therefore recomputed here from the **latest** analytics instant
/// (`ChainStore::analytics_as_of` — the deterministic clock the #24 kernel already
/// advances on each data-changing fold, never `Utc::now()`) against the chain's
/// absolute expiry, so a later refresh prices a smaller DTE and the curve decays
/// toward the (frozen) expiration line. Only the DTE and IV move; the entry premium
/// (`Position::premium`) is untouched — the SF-1 frozen cost basis is preserved.
#[must_use]
fn repriced_for_tplus0(
    entry_positions: &[Position],
    legs: &[BuilderLeg],
    store: &ChainStore,
) -> Option<Vec<Position>> {
    let ivs = resolve_leg_ivs(legs, store)?;
    // Recompute the time-to-expiry from the latest analytics instant (arriving as
    // data, never a wall-clock read) so the reprice stays deterministic and the t+0
    // curve theta-decays as the app is held open.
    let expiration_utc = absolute_expiry(store.chain())?;
    let expiration = ExpirationDate::Days(days_between(store.analytics_as_of(), expiration_utc)?);
    let mut priced = Vec::with_capacity(entry_positions.len());
    for (position, iv) in entry_positions.iter().zip(ivs.iter()) {
        let mut position = position.clone();
        position.option.implied_volatility = *iv;
        position.option.expiration_date = expiration;
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
    use std::time::Duration;

    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::model::Position;
    use optionstratlib::prelude::{Decimal, Positive};
    use optionstratlib::pricing::Profit;
    use optionstratlib::strategies::custom::CustomStrategy;
    use optionstratlib::visualization::{GraphData, Series2D};
    use optionstratlib::{ExpirationDate, OptionStyle, OptionType, Options, Side as OptionSide};

    use super::{
        BuilderLeg, GreeksOrigin, MIN_PLAUSIBLE_LOCAL_IV, Side, break_even_points, build_geometry,
        expiration_series, plausible_leg_iv, rebuild_tplus0, tplus0_series,
    };
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ContractSpecFingerprint, ExerciseStyle,
        ExpirySource, GreeksRow, Instrument, InstrumentKey, ProviderId, SettlementStyle,
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

    // --- #27 SF-3: the t+0 DTE recomputes from the latest analytics instant ------
    //
    // The frozen entry positions carry the commit-time DTE; the t+0 reprice must
    // recompute it from the store's `analytics_as_of` so the curve theta-decays as
    // the app is held open, while the entry premium stays frozen. A VENUE IV is
    // pinned so only the DTE varies between rebuilds — a locally-inverted IV would
    // recalibrate to the mark at each DTE and mask the decay.

    /// The chain expiry string every store here shares — it parses to an absolute
    /// instant well after both seed instants, so the DTE is a positive future span.
    const CHAIN_EXPIRY: &str = "2025-06-27";
    /// An early analytics instant (~591 days to expiry).
    const AS_OF_EARLY: i64 = 1_700_000_000;
    /// A later analytics instant (~69 days to expiry) — a smaller DTE, same chain.
    const AS_OF_LATE: i64 = 1_745_000_000;

    #[track_caller]
    fn utc(secs: i64) -> chrono::DateTime<chrono::Utc> {
        match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
            Some(t) => t,
            None => panic!("invalid test timestamp: {secs}"),
        }
    }

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("invalid provider id `{id}`: {e}"),
        }
    }

    /// The absolute expiry `CHAIN_EXPIRY` parses to — the key the injected venue IV
    /// row and the payoff IV resolution share.
    #[track_caller]
    fn resolved_expiry() -> chrono::DateTime<chrono::Utc> {
        let chain = OptionChain::new("BTC", pos(100.0), CHAIN_EXPIRY.to_owned(), None, None);
        match chain.get_expiration() {
            Some(ExpirationDate::DateTime(dt)) => dt,
            other => panic!("expected an absolute chain expiry, got {other:?}"),
        }
    }

    /// An ATM call row with a present mark and a plausible chain IV.
    fn atm_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(9.0)),
            call_ask: Some(pos(11.0)),
            put_bid: Some(pos(9.0)),
            put_ask: Some(pos(11.0)),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    /// A one-strike chain at `spot`.
    fn atm_chain(spot: f64) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(spot), CHAIN_EXPIRY.to_owned(), None, None);
        let _ = chain.options.insert(atm_row(spot));
        chain
    }

    /// The identity for the ATM call leg, keyed at the resolved chain expiry so the
    /// injected venue IV lands on the sidecar entry the payoff resolution reads.
    fn instrument(spot: f64) -> Instrument {
        Instrument {
            key: InstrumentKey {
                underlying: "BTC".to_owned(),
                expiration_utc: resolved_expiry(),
                strike: pos(spot),
                style: OptionStyle::Call,
            },
            provider: pid("deribit"),
            native_symbol: "BTC-CALL".to_owned(),
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

    /// A venue (`Provider`-origin) greeks row pinning the leg IV to a CONSTANT: it
    /// survives every recompute (venue wins over local inversion), so only the DTE
    /// varies between rebuilds — a locally-inverted IV would recalibrate to the mark
    /// at each DTE and hide the decay.
    fn venue_iv_row(spot: f64, iv: f64, received: i64) -> GreeksRow {
        GreeksRow {
            instrument: instrument(spot),
            iv: Some(pos(iv)),
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(received),
        }
    }

    /// Seed a store at `spot` and advance its deterministic `analytics_as_of` to
    /// `as_of` by folding a venue IV row (Provider origin, IV 0.5) — the instant the
    /// t+0 DTE recompute reads, with the IV pinned constant across both instants.
    fn store_at(spot: f64, as_of: i64) -> ChainStore {
        let mut store = ChainStore::seed(
            ChainFetch::new(
                atm_chain(spot),
                ExpirySource::new("BTC", resolved_expiry(), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(as_of),
        );
        let _ = store.apply_greeks(&venue_iv_row(spot, 0.5, as_of));
        store
    }

    /// A single long ATM call leg.
    fn atm_call_leg(spot: f64) -> Vec<BuilderLeg> {
        vec![BuilderLeg {
            strike: pos(spot),
            style: OptionStyle::Call,
            side: Side::Buy,
            qty: 1,
        }]
    }

    #[test]
    fn test_tplus0_theta_decays_when_the_analytics_instant_advances() {
        // SF-3: reprice the SAME frozen positions against a LATER analytics instant
        // (a smaller DTE). The t+0 curve must move TOWARD the (frozen) expiration line
        // at every sample — never away — and observably so near ATM, because BS →
        // intrinsic as time-to-expiry shrinks. The venue IV is pinned, so only the DTE
        // moves.
        let spot = 100.0;
        let legs = atm_call_leg(spot);
        let early_store = store_at(spot, AS_OF_EARLY);
        let late_store = store_at(spot, AS_OF_LATE);

        let geometry = match build_geometry(&legs, &early_store) {
            Some(g) => g,
            None => panic!("the ATM call geometry prices at the early instant"),
        };
        // The early t+0 curve (larger DTE) is the one `build_geometry` cached.
        let (early,) = xy(&geometry.tplus0);
        let (exp,) = xy(&geometry.expiration);
        // Reprice the SAME frozen positions at the later instant (smaller DTE).
        let late = rebuild_tplus0(
            &legs,
            &late_store,
            &geometry.grid,
            &geometry.entry_positions,
        );
        let (late,) = xy(&late);

        assert_eq!(
            early.x, late.x,
            "the shared grid is unchanged by the reprice"
        );
        assert_eq!(early.x, exp.x, "expiration shares the grid too");
        assert_eq!(
            late.x.len(),
            geometry.grid.len(),
            "one sample per grid point"
        );

        let eps = Decimal::new(1, 6);
        let mut decayed = false;
        for ((e, l), x) in early.y.iter().zip(late.y.iter()).zip(exp.y.iter()) {
            // Toward expiration: the later (smaller-DTE) curve sits at or below the
            // earlier one and at or above the frozen expiration line (time value ≥ 0
            // and monotone in DTE at r = 0).
            assert!(
                *l <= *e + eps,
                "later t+0 at/below earlier (theta-decay direction): early {e}, late {l}",
            );
            assert!(
                *l >= *x - eps,
                "later t+0 stays at/above the frozen expiration: late {l}, exp {x}",
            );
            if *e - *l > Decimal::ONE {
                decayed = true;
            }
        }
        assert!(
            decayed,
            "theta decay is observable — the curve dropped toward expiration somewhere",
        );
        assert_ne!(
            early.y, late.y,
            "the t+0 curve changed with the fresh, smaller DTE"
        );
    }

    #[test]
    fn test_tplus0_rebuild_keeps_the_entry_premium_frozen() {
        // The DTE recompute must NOT re-base the entry premium (SF-1 preserved): the
        // frozen positions carry P0 = the commit-time mark, and a rebuild at a later
        // instant reads them read-only. Prove the premium is untouched and equals the
        // commit-time chain mid — never re-read from the later store.
        let spot = 100.0;
        let legs = atm_call_leg(spot);
        let early_store = store_at(spot, AS_OF_EARLY);
        let late_store = store_at(spot, AS_OF_LATE);

        let geometry = match build_geometry(&legs, &early_store) {
            Some(g) => g,
            None => panic!("the ATM call geometry prices at the early instant"),
        };
        let commit_mark = legs
            .first()
            .and_then(|leg| leg.mark_in(early_store.chain()));
        let p0: Vec<Positive> = geometry.entry_positions.iter().map(|p| p.premium).collect();
        assert_eq!(
            p0.first().copied(),
            commit_mark,
            "the frozen premium is the commit-time mark P0",
        );

        // Rebuild at the later instant (fresh DTE) — the frozen basis is borrowed
        // read-only, so a reprice can never re-base it.
        let _ = rebuild_tplus0(
            &legs,
            &late_store,
            &geometry.grid,
            &geometry.entry_positions,
        );
        let p_after: Vec<Positive> = geometry.entry_positions.iter().map(|p| p.premium).collect();
        assert_eq!(
            p0, p_after,
            "a later-instant rebuild never re-bases the frozen entry premium",
        );
    }

    #[test]
    fn test_tplus0_rebuild_is_deterministic_across_identical_inputs() {
        // The reprice reads the instant as data (`analytics_as_of`), never a wall
        // clock or RNG, so two rebuilds from identical inputs are byte-for-byte
        // identical.
        let spot = 100.0;
        let legs = atm_call_leg(spot);
        let late_store = store_at(spot, AS_OF_LATE);
        let geometry = match build_geometry(&legs, &store_at(spot, AS_OF_EARLY)) {
            Some(g) => g,
            None => panic!("the ATM call geometry prices at the early instant"),
        };
        let first = rebuild_tplus0(
            &legs,
            &late_store,
            &geometry.grid,
            &geometry.entry_positions,
        );
        let second = rebuild_tplus0(
            &legs,
            &late_store,
            &geometry.grid,
            &geometry.entry_positions,
        );
        assert_eq!(
            first, second,
            "identical inputs yield an identical t+0 series"
        );
    }
}
