//! Off-draw vol-smile / Greek-curve / single-expiry-surface geometry (#47,
//! `docs/05-views-and-ux.md` §4).
//!
//! This module turns the live [`ChainStore`] snapshot into the three
//! [`GraphData`](optionstratlib::visualization::GraphData) shapes the Surface screen
//! renders — the **vol smile**, a **Greek curve**, and the **single-expiry
//! surface** — entirely in the **application** layer and entirely **off** the draw
//! path (`docs/02-tui-architecture.md` §7). ChainView never computes the smile /
//! curve / surface geometry itself: every build calls the `optionstratlib`
//! `VolatilitySmile` / `BasicCurves` / `BasicSurfaces` traits and wraps the result
//! with the [`From`] conversions — the geometry is upstream math, never hand-rolled.
//!
//! # The zero-IV smile fill (#47 map 🟠)
//!
//! `OptionChain::smile()` reads each strike's `implied_volatility` verbatim, so a
//! strike whose IV is the absent-`0` sentinel enters the curve as `(strike, 0.0)`
//! and craters the wings. Before any build, [`prepared_chain`] overlays a **reliable
//! IV per strike** from the #24 Greeks sidecar (venue-supplied or locally inverted),
//! and a strike with **no** reliable IV is **dropped** (never fabricated to `0`) —
//! the honesty rule. The `Positive` plausibility gate mirrors the chain matrix (#25)
//! and the payoff t+0 curve (#27): a **venue** IV is trusted as-is; a **local**
//! inversion must clear [`MIN_PLAUSIBLE_LOCAL_IV`].
//!
//! # The #24 clock trap and its frozen-Days resolution
//!
//! A `Greek` or `Price` curve / surface prices Black-Scholes, which reads
//! `ExpirationDate::get_years()`. For the chain's stored `DateTime` expiry that
//! reads `Utc::now()` on **every** evaluation — non-deterministic, and it would
//! break the #50 / #51 goldens. [`prepared_chain`] therefore re-stamps every option
//! to `ExpirationDate::Days(dte)`, where `dte = days_between(as_of, expiry)` is a
//! deterministic day-count from a **stored** reference instant
//! ([`ChainStore::last_full_poll`], never `Utc::now()`) — for the `Days` variant
//! `get_years()` is `days / 365`, clock-free. So a Greek / Price curve or surface is
//! a **pure function of `(chain, sidecar, as_of)`**: identical inputs yield an
//! identical series (the determinism test asserts it). The vol smile and the
//! `Volatility`-axis curve are already clock-free.
//!
//! # The surface `z` is a Greek / Price — never IV
//!
//! `BasicSurfaces::surface` rejects the `Volatility` axis: the 3D view is a
//! Greek/Price over `(strike, volatility)`, and the only true IV artifact is the 2D
//! smile (the map's RED correction). [`build_surface`] refuses the `Volatility` axis
//! rather than fabricating an "IV surface".

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{BasicCurves, BasicSurfaces, Decimal, Positive, VolatilitySmile};
use optionstratlib::visualization::{GraphData, Series2D, Surface3D};
use optionstratlib::{ExpirationDate, OptionStyle, Side};

use super::SurfaceAxis;
use crate::chain::{ChainStore, GreeksOrigin, InstrumentKey, MIN_PLAUSIBLE_LOCAL_IV};

/// The option style the smile / curve / surface price against. The surface screen
/// has no call/put toggle (that is a chain-matrix gesture), so the geometry defaults
/// to the **call** leg — matching the chain's default focused leg
/// ([`LegFocus::Call`](super::LegFocus)).
const GEOMETRY_STYLE: OptionStyle = OptionStyle::Call;

/// The position side the curve / surface price against — a unit **long** contract,
/// matching the sidecar's pricing convention (`src/chain/greeks.rs`).
const GEOMETRY_SIDE: Side = Side::Long;

/// The number of volatility samples the single-expiry surface sweeps across. The
/// surface is `strikes × VOL_SAMPLES`, so a small vector keeps the grid linear in
/// the strike count (no perf landmine, per the #47 map).
const VOL_SAMPLES: usize = 5;

/// Seconds per day for the deterministic day-count.
const SECONDS_PER_DAY: i64 = 86_400;

/// An empty `GraphData::Series` — the "no smile / curve yet" input the #23 adapter
/// renders as `Empty(NoData)` (the deliberate "insufficient IV" state).
#[must_use]
pub(crate) fn empty_series() -> GraphData {
    GraphData::Series(Series2D::default())
}

/// An empty `GraphData::GraphSurface` — the "no surface" input the #23 adapter
/// renders as `Empty(NoData)` (the deliberate "insufficient IV" surface state).
#[must_use]
pub(crate) fn empty_surface() -> GraphData {
    GraphData::GraphSurface(Surface3D::default())
}

/// A deliberately-malformed `GraphData::Series` (a single `x`, no `y` — mismatched
/// lengths) that the #23 adapter projects to `Empty(Degenerate)` ("degenerate
/// geometry"), distinct from the insufficient-IV `Empty(NoData)` (P3-01). A **hard**
/// `curve()` build `Err` (the geometry could not be priced) routes here so the screen
/// reports a distinguishable "cannot build" state rather than folding into the
/// insufficient-IV empty state.
#[must_use]
fn degenerate_series() -> GraphData {
    GraphData::Series(Series2D {
        x: vec![Decimal::ZERO],
        y: Vec::new(),
        ..Series2D::default()
    })
}

/// A deliberately-malformed `GraphData::GraphSurface` (a single `x`, no `y`/`z`) that
/// the #23 adapter projects to `Empty(Degenerate)` — the surface analogue of
/// [`degenerate_series`] (P3-01). A **hard** `surface()` build `Err` routes here,
/// distinct from the refused-`Volatility`-axis and insufficient-IV `NoData` paths.
#[must_use]
fn degenerate_surface() -> GraphData {
    GraphData::GraphSurface(Surface3D {
        x: vec![Decimal::ZERO],
        y: Vec::new(),
        z: Vec::new(),
        ..Surface3D::default()
    })
}

/// Build the **vol smile** (IV vs strike) from `store`, off the draw path. Reliable
/// per-strike IV is overlaid first (the zero-IV fill) and no-IV strikes dropped, then
/// `OptionChain::smile()` (infallible — the map's RED correction) is wrapped by
/// [`GraphData::from`]. An empty prepared chain yields [`empty_series`] → the
/// "insufficient IV" state.
#[must_use]
pub(crate) fn build_smile(store: &ChainStore) -> GraphData {
    match prepared_chain(store) {
        Some(chain) => named(GraphData::from(chain.smile()), "IV smile"),
        None => empty_series(),
    }
}

/// Build a **Greek / IV / Price curve** (the `axis` metric vs strike) from `store`
/// on the frozen-`Days` chain, off the draw path. An IV-sparse expiry (no prepared
/// chain) projects to the insufficient-IV `Empty(NoData)`; a **hard** `CurveError`
/// (the geometry could not be priced) routes to [`degenerate_series`] →
/// `Empty(Degenerate)` ("degenerate geometry"), a distinguishable state (P3-01) —
/// never a panic, never a fabricated curve.
#[must_use]
pub(crate) fn build_curve(store: &ChainStore, axis: SurfaceAxis) -> GraphData {
    let Some(chain) = prepared_chain(store) else {
        return empty_series();
    };
    match chain.curve(&axis.to_basic(), &GEOMETRY_STYLE, &GEOMETRY_SIDE) {
        Ok(curve) => named(
            GraphData::from(curve),
            &format!("{} vs strike", axis.metric_name()),
        ),
        Err(_) => degenerate_series(),
    }
}

/// Build the **single-expiry surface** (`axis` Greek/Price over strike × volatility)
/// from `store` on the frozen-`Days` chain, off the draw path. The `Volatility` axis
/// is refused — the 3D `z` is a Greek/Price, never IV (the map's RED correction), so
/// it degrades to [`empty_surface`] (the screen renders its own "IV has no surface"
/// state); an IV-sparse expiry degrades to the insufficient-IV `Empty(NoData)`. A
/// **hard** `SurfaceError` (a strike whose option cannot be built) routes to
/// [`degenerate_surface`] → `Empty(Degenerate)` (P3-01), never a fabricated surface.
#[must_use]
pub(crate) fn build_surface(store: &ChainStore, axis: SurfaceAxis) -> GraphData {
    // IV is not a surface axis: the 3D z is a Greek/Price (the map's RED correction).
    if axis == SurfaceAxis::Volatility {
        return empty_surface();
    }
    let Some(chain) = prepared_chain(store) else {
        return empty_surface();
    };
    let vols = vol_samples(&chain);
    if vols.is_empty() {
        return empty_surface();
    }
    match chain.surface(
        &axis.to_basic(),
        &GEOMETRY_STYLE,
        Some(vols),
        &GEOMETRY_SIDE,
    ) {
        Ok(surface) => named(
            GraphData::from(surface),
            &format!("{} surface", axis.metric_name()),
        ),
        Err(_) => degenerate_surface(),
    }
}

/// Overlay a meaningful legend name onto a `From`-built `GraphData`, leaving the
/// upstream geometry untouched (the map: use the `From` conversions, do not
/// hand-build the points). Exhaustive over [`GraphData`] with no wildcard.
#[must_use]
fn named(mut graph: GraphData, name: &str) -> GraphData {
    match &mut graph {
        GraphData::Series(series) => series.name = name.to_owned(),
        GraphData::GraphSurface(surface) => surface.name = name.to_owned(),
        GraphData::MultiSeries(_) => {}
    }
    graph
}

/// Build the IV-overlaid, frozen-`Days`, no-IV-dropped chain the three builders
/// share, or `None` when no strike carries a reliable IV or the expiry is not a
/// future absolute instant.
///
/// Each surviving option is stamped with the chain symbol, underlying, a reliable
/// per-strike IV, and `ExpirationDate::Days(dte)` — the four fields
/// `Options::try_from(&OptionData)` requires for the curve / surface pricing — so
/// `BasicCurves`/`BasicSurfaces` never read the wall clock.
#[must_use]
fn prepared_chain(store: &ChainStore) -> Option<OptionChain> {
    let chain = store.chain();
    let expiration_utc = absolute_expiry(chain)?;
    let as_of = store.last_full_poll()?;
    let dte = days_between(as_of, expiration_utc)?;
    let symbol = chain.symbol.clone();
    let underlying = chain.underlying_price;
    let expiry = ExpirationDate::Days(dte);

    let mut options: BTreeSet<OptionData> = BTreeSet::new();
    for od in &chain.options {
        // Drop strikes with no reliable IV — never fabricate a zero-IV wing.
        let Some(iv) = resolve_strike_iv(store, &symbol, od, expiration_utc) else {
            continue;
        };
        let mut od = od.clone();
        od.symbol = Some(symbol.clone());
        od.underlying_price = Some(Box::new(underlying));
        od.expiration_date = Some(expiry);
        od.implied_volatility = iv;
        let _ = options.insert(od);
    }
    if options.is_empty() {
        return None;
    }
    let mut prepared = chain.clone();
    prepared.options = options;
    Some(prepared)
}

/// Resolve a **reliable** IV for one strike (a single field the smile / pricing use):
/// the #24 sidecar's call- or put-leg IV when usable and plausible, else the chain's
/// own per-strike IV when usable. `None` when the strike has no trustworthy IV — the
/// honesty drop.
#[must_use]
fn resolve_strike_iv(
    store: &ChainStore,
    symbol: &str,
    od: &OptionData,
    expiration_utc: DateTime<Utc>,
) -> Option<Positive> {
    for style in [OptionStyle::Call, OptionStyle::Put] {
        let key = InstrumentKey {
            underlying: symbol.to_owned(),
            expiration_utc,
            strike: od.strike_price,
            style,
        };
        if let Some(sidecar) = store.leg_greeks(&key)
            && let Some(iv) = sidecar.iv
            && usable_iv(iv)
            && let Some(plausible) = plausible_iv(iv, sidecar.iv_origin)
        {
            return Some(plausible);
        }
    }
    // Fall back to the chain's own per-strike IV when it is a usable value (a shared
    // venue IV, trusted as-is), else the strike has no reliable IV.
    usable_iv(od.implied_volatility).then_some(od.implied_volatility)
}

/// The volatility samples the surface sweeps: [`VOL_SAMPLES`] points across the
/// prepared chain's reliable-IV range (an exploratory what-if over the vol axis). A
/// degenerate range (one distinct IV) widens to a small visible band; an empty range
/// yields no samples (the surface then degrades to the empty state).
#[must_use]
fn vol_samples(chain: &OptionChain) -> Vec<Positive> {
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    let mut any = false;
    for od in &chain.options {
        let iv = od.implied_volatility;
        if usable_iv(iv) {
            let v = iv.to_f64();
            if v.is_finite() {
                lo = lo.min(v);
                hi = hi.max(v);
                any = true;
            }
        }
    }
    if !any {
        return Vec::new();
    }
    // A single distinct IV gives the vol axis no extent, so widen to a band around it.
    let (lo, hi) = if hi > lo {
        (lo, hi)
    } else {
        (lo * 0.5, lo * 1.5)
    };
    let Ok(den) = u16::try_from(VOL_SAMPLES.max(2) - 1) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(VOL_SAMPLES);
    for i in 0..VOL_SAMPLES {
        let Ok(num) = u16::try_from(i) else {
            break;
        };
        let t = f64::from(num) / f64::from(den);
        let v = lo + (hi - lo) * t;
        if let Ok(p) = Positive::new(v) {
            out.push(p);
        }
    }
    out.dedup();
    out
}

/// Whether an implied volatility is usable: strictly positive and not the non-finite
/// [`Positive::INFINITY`] sentinel (mirrors the payoff builder's gate, #27).
#[must_use]
fn usable_iv(iv: Positive) -> bool {
    iv > Positive::ZERO && iv != Positive::INFINITY
}

/// Apply the #25 plausibility gate: a `Provider` (venue) IV is trusted as-is; a
/// `ComputedLocally` IV must clear [`MIN_PLAUSIBLE_LOCAL_IV`] or it is rejected — so a
/// #83-mispriced near-zero local inversion never craters a smile wing or a curve.
#[must_use]
fn plausible_iv(iv: Positive, origin: GreeksOrigin) -> Option<Positive> {
    match origin {
        GreeksOrigin::Provider => Some(iv),
        GreeksOrigin::ComputedLocally => (iv.to_dec() >= MIN_PLAUSIBLE_LOCAL_IV).then_some(iv),
    }
}

/// The absolute-UTC expiry of `chain`, or `None` for a relative (`Days`) or
/// unparseable expiry — matching the #24 sidecar, which keys on the same absolute
/// instant. Exhaustive over [`ExpirationDate`] with no wildcard.
#[must_use]
fn absolute_expiry(chain: &OptionChain) -> Option<DateTime<Utc>> {
    match chain.get_expiration()? {
        ExpirationDate::DateTime(dt) => Some(dt),
        ExpirationDate::Days(_) => None,
    }
}

/// The deterministic fractional-day span from `as_of` to `expiration_utc`, or `None`
/// when the span is non-positive (an expired input). Integer seconds and `Decimal`,
/// so no `f64` enters the day-count — mirrors the #24 sidecar / #27 payoff kernel.
#[must_use]
fn days_between(as_of: DateTime<Utc>, expiration_utc: DateTime<Utc>) -> Option<Positive> {
    let seconds = expiration_utc.signed_duration_since(as_of).num_seconds();
    if seconds <= 0 {
        return None;
    }
    let days = Decimal::from(seconds).checked_div(Decimal::from(SECONDS_PER_DAY))?;
    Positive::new_decimal(days).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::{BasicCurves, Decimal, Positive};
    use optionstratlib::visualization::GraphData;
    use optionstratlib::{ExpirationDate, OptionStyle, Side};

    use super::{SurfaceAxis, build_curve, build_smile, build_surface, named};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, ProviderId,
    };
    use crate::ui::graph::{EmptyReason, project};

    const EXP: i64 = 1_700_000_000;
    const A: f64 = 60_000.0;
    const B: f64 = 62_000.0;
    const C: f64 = 64_000.0;

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

    /// A realistic-premium BTC strike so the local IV inversion lands well above the
    /// plausibility floor (the smile / curve / surface render).
    fn full_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(3_000.0)),
            call_ask: Some(pos(3_100.0)),
            put_bid: Some(pos(2_000.0)),
            put_ask: Some(pos(2_100.0)),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    /// A strike with no quotes and a zero IV sentinel — the "no reliable IV" strike
    /// the smile fill drops (never a zero-IV wing).
    fn bare_row(strike: f64) -> OptionData {
        OptionData {
            strike_price: pos(strike),
            implied_volatility: Positive::ZERO,
            ..Default::default()
        }
    }

    fn store_from(chain: OptionChain) -> ChainStore {
        ChainStore::seed(
            ChainFetch::new(
                chain,
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            // A poll instant well before the 2025-06-27 expiry, so the frozen dte is
            // positive and the local analytics compute.
            utc(1_735_689_600),
        )
    }

    fn full_store() -> ChainStore {
        let mut chain = OptionChain::new("BTC", pos(A), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(full_row(A));
        let _ = chain.options.insert(full_row(B));
        let _ = chain.options.insert(full_row(C));
        store_from(chain)
    }

    #[track_caller]
    fn series_len(graph: &GraphData) -> usize {
        match graph {
            GraphData::Series(s) => {
                assert_eq!(s.x.len(), s.y.len(), "x and y are paired");
                s.x.len()
            }
            other => panic!("expected a Series, got {other:?}"),
        }
    }

    #[track_caller]
    fn surface_points(graph: &GraphData) -> usize {
        match graph {
            GraphData::GraphSurface(s) => {
                assert_eq!(s.x.len(), s.y.len());
                assert_eq!(s.x.len(), s.z.len());
                s.x.len()
            }
            other => panic!("expected a GraphSurface, got {other:?}"),
        }
    }

    // --- The smile fills IV and drops no-IV strikes --------------------------

    #[test]
    fn test_build_smile_is_nonempty_and_no_iv_is_zero() {
        let store = full_store();
        let smile = build_smile(&store);
        let n = series_len(&smile);
        assert!(n >= 3, "one point per reliable strike (>= the 3 seeded)");
        // Every filled IV is strictly positive — no strike cratered to a zero wing.
        if let GraphData::Series(s) = &smile {
            for y in &s.y {
                assert!(*y > optionstratlib::prelude::Decimal::ZERO, "IV filled > 0");
            }
        }
    }

    #[test]
    fn test_build_smile_drops_the_no_iv_strike() {
        // Two priced strikes + one bare (no quotes, zero IV) → the smile has exactly
        // the two reliable strikes, the bare one dropped (honesty), not a zero wing.
        let mut chain = OptionChain::new("BTC", pos(A), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(full_row(A));
        let _ = chain.options.insert(full_row(B));
        let _ = chain.options.insert(bare_row(C));
        let store = store_from(chain);
        assert_eq!(
            series_len(&build_smile(&store)),
            2,
            "the bare strike is dropped"
        );
    }

    #[test]
    fn test_build_smile_empty_chain_is_empty_series() {
        let store = store_from(OptionChain::new(
            "BTC",
            pos(A),
            "2025-06-27".to_owned(),
            None,
            None,
        ));
        assert_eq!(
            series_len(&build_smile(&store)),
            0,
            "no strikes → empty smile"
        );
    }

    // --- The Greek curve builds over strike ----------------------------------

    #[test]
    fn test_build_curve_delta_is_nonempty_series() {
        let store = full_store();
        assert!(
            series_len(&build_curve(&store, SurfaceAxis::Delta)) >= 3,
            "a delta curve has a point per reliable strike",
        );
    }

    // --- The surface builds over strike × vol, refuses the IV axis -----------

    #[test]
    fn test_build_surface_delta_is_nonempty_graphsurface() {
        let store = full_store();
        // 3 strikes × 5 vol samples = up to 15 points (some may be skipped).
        assert!(
            surface_points(&build_surface(&store, SurfaceAxis::Delta)) > 3,
            "a delta surface sweeps strike × vol",
        );
    }

    #[test]
    fn test_build_surface_refuses_the_volatility_axis() {
        // IV has no honest surface projection (z is a Greek/Price) → an empty surface.
        let store = full_store();
        assert_eq!(
            surface_points(&build_surface(&store, SurfaceAxis::Volatility)),
            0,
            "the Volatility axis is refused for the 3D surface",
        );
    }

    // --- Determinism: same inputs → byte-identical geometry (the clock trap) --

    /// The precomputed fixed `Days` a build must stamp: `(expiry - as_of) / 86400`,
    /// derived here **independently** from the store's own instants (never `Utc::now()`).
    #[track_caller]
    fn precomputed_dte(store: &ChainStore) -> Positive {
        let chain = store.chain();
        let expiration_utc = match chain.get_expiration() {
            Some(ExpirationDate::DateTime(dt)) => dt,
            other => panic!("expected an absolute expiry, got {other:?}"),
        };
        let as_of = match store.last_full_poll() {
            Some(t) => t,
            None => panic!("the seeded store carries a poll instant"),
        };
        let seconds = expiration_utc.signed_duration_since(as_of).num_seconds();
        assert!(seconds > 0, "the fixture expiry is in the future");
        let days = match Decimal::from(seconds).checked_div(Decimal::from(86_400_i64)) {
            Some(d) => d,
            None => panic!("day-count division failed"),
        };
        match Positive::new_decimal(days) {
            Ok(p) => p,
            Err(e) => panic!("positive dte: {e}"),
        }
    }

    #[test]
    fn test_greek_curve_prices_a_precomputed_frozen_days_expectation() {
        // Load-bearing (architect item 8): rather than compare two back-to-back builds,
        // assert the geometry against a PRECOMPUTED fixed-Days expectation.
        //
        // (1) Every option the builder prices is stamped EXACTLY the independently
        //     computed fixed Days — a Utc::now() regression would stamp a different
        //     Days here and fail this assertion (the two-pass test cannot catch that,
        //     since two near-simultaneous now() reads still agree).
        // (2) The rendered Theta curve equals the curve of that fixed-Days chain, so
        //     the builder prices nothing but the precomputed expectation.
        let store = full_store();
        let expected_dte = precomputed_dte(&store);
        let prepared = match super::prepared_chain(&store) {
            Some(c) => c,
            None => panic!("the fixture prepares a chain"),
        };
        assert!(
            !prepared.options.is_empty(),
            "the prepared chain has strikes"
        );
        for od in &prepared.options {
            assert_eq!(
                od.expiration_date,
                Some(ExpirationDate::Days(expected_dte)),
                "every priced option carries the precomputed fixed Days, not the wall clock",
            );
        }
        let curve = match prepared.curve(
            &SurfaceAxis::Theta.to_basic(),
            &OptionStyle::Call,
            &Side::Long,
        ) {
            Ok(c) => c,
            Err(e) => panic!("the fixture curve prices: {e:?}"),
        };
        let expected = named(
            GraphData::from(curve),
            &format!("{} vs strike", SurfaceAxis::Theta.metric_name()),
        );
        assert_eq!(
            build_curve(&store, SurfaceAxis::Theta),
            expected,
            "the theta curve equals the fixed-Days expectation",
        );
    }

    #[test]
    fn test_builds_are_deterministic_across_two_passes() {
        // The frozen-Days resolution makes a Greek curve / surface a pure function of
        // (chain, sidecar, as_of). The smile is clock-free and the vega surface is
        // deterministic across two builds; the clock-sensitive Greek curve is pinned to
        // a precomputed fixed-Days expectation in
        // test_greek_curve_matches_a_precomputed_frozen_days_expectation (item 8).
        let a = full_store();
        let b = full_store();
        assert_eq!(build_smile(&a), build_smile(&b), "smile is deterministic");
        assert_eq!(
            build_surface(&a, SurfaceAxis::Vega),
            build_surface(&b, SurfaceAxis::Vega),
            "the vega surface is deterministic (frozen Days, no clock)",
        );
    }

    #[test]
    fn test_hard_build_err_sentinels_project_to_degenerate_not_no_data() {
        // P3-01: a hard curve()/surface() Err routes to the degenerate sentinels, which
        // the #23 adapter projects to Empty(Degenerate) ("degenerate geometry") — a
        // distinguishable state, NOT the insufficient-IV Empty(NoData) the empty
        // sentinels produce.
        assert_eq!(
            project(&super::degenerate_series()).empty_reason(),
            Some(EmptyReason::Degenerate),
            "the curve-Err sentinel projects degenerate geometry",
        );
        assert_eq!(
            project(&super::degenerate_surface()).empty_reason(),
            Some(EmptyReason::Degenerate),
            "the surface-Err sentinel projects degenerate geometry",
        );
        // The insufficient-IV sentinels stay NoData, so the two states are genuinely
        // distinct at the render edge.
        assert_eq!(
            project(&super::empty_series()).empty_reason(),
            Some(EmptyReason::NoData),
        );
        assert_eq!(
            project(&super::empty_surface()).empty_reason(),
            Some(EmptyReason::NoData),
        );
    }

    #[test]
    fn test_curve_price_axis_uses_frozen_days_not_the_wall_clock() {
        // A Price curve prices Black-Scholes, which reads get_years(). With the frozen
        // Days resolution it is clock-free, so a rebuild is byte-identical (a wall-clock
        // read would drift between the two calls).
        let store = full_store();
        let first = build_curve(&store, SurfaceAxis::Price);
        let second = build_curve(&store, SurfaceAxis::Price);
        assert_eq!(first, second, "the price curve is clock-free and stable");
        assert!(series_len(&first) >= 3, "a price curve has points");
    }

    #[test]
    fn test_build_curve_all_greek_axes_are_nonempty() {
        // Every Greek / IV / Price axis prices the call/long leg to a well-formed,
        // non-empty series over the reliable strikes.
        let store = full_store();
        for axis in [
            SurfaceAxis::Delta,
            SurfaceAxis::Gamma,
            SurfaceAxis::Theta,
            SurfaceAxis::Vega,
            SurfaceAxis::Volatility,
            SurfaceAxis::Price,
        ] {
            assert!(
                series_len(&build_curve(&store, axis)) >= 3,
                "the {axis:?} curve has a point per reliable strike",
            );
        }
    }
}
