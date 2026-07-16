//! The local Greeks / IV fill-in engine and the style-keyed analytics sidecar
//! (`docs/01-domain-model.md` §7).
//!
//! `optionstratlib::chains::OptionData` persists only four analytic fields per
//! strike — `implied_volatility`, `delta_call`, `delta_put`, and one shared
//! `gamma` — with no `theta`/`vega`/`rho` and no per-leg gamma. The chain UI
//! advertises full per-leg Greeks, so ChainView reconciles the two with a
//! **two-source** model: what the chain persists, plus the [`GreeksSidecar`]
//! keyed by the style-bearing [`InstrumentKey`] (§4) so a call leg and a put leg
//! of one strike get **separate** entries — preserving venue IV/gamma that the
//! shared `OptionData` fields can only hold once per strike without call/put
//! collision.
//!
//! # This is DOMAIN code — no ratatui, no provider, no app/ui
//!
//! The engine speaks `optionstratlib` types and domain types only. It never
//! imports a provider adapter, the application, or a UI type (the issue #22 arch
//! test enforces the domain has no back-edge). It is driven by the market/tick
//! **event** and cached by [`PricingInputs::input_generation`]; `draw` only
//! **reads** the sidecar through the read-only accessors ([`GreeksSidecar::get`])
//! and performs no pricing and no mutation.
//!
//! # The math lives UPSTREAM — no hand-rolled Black-Scholes or root-finder
//!
//! [`compute_leg_greeks`] builds an [`optionstratlib::Options`] from the strike,
//! resolved IV, absolute expiry, style, and [`PricingInputs`], then calls the
//! `optionstratlib` Greeks free functions (`delta`, `gamma`, `theta`, `vega`,
//! `rho`, re-exported at `optionstratlib::greeks::*`) and inverts IV through
//! [`Options::calculate_implied_volatility`]. ChainView never hand-rolls the
//! options math (`CLAUDE.md` "Key Decisions").
//!
//! # Verified `optionstratlib` 0.18.0 API (deviations from the §7 v0.17.2 sketch)
//!
//! - The Greeks free functions are `optionstratlib::greeks::{delta, gamma, theta,
//!   vega, rho}`, each `fn(&Options) -> Result<Decimal, GreeksError>`. The §7
//!   sketch named `optionstratlib::greeks::equations::*`, but `equations` is a
//!   **private** submodule in 0.18.0; the functions are re-exported one level up.
//! - IV inversion uses the **method** [`Options::calculate_implied_volatility`]
//!   `(&self, Decimal) -> Result<Positive, VolatilityError>` — a deterministic,
//!   single-threaded binary search — in preference to the free
//!   `optionstratlib::volatility::implied_volatility`, which runs a `rayon`
//!   parallel grid search (its result is order-stable but its execution is not a
//!   deterministic kernel).
//! - `ExpirationDate` is an enum `{ Days(Positive), DateTime(DateTime<Utc>) }`.
//!   Its `DateTime`-variant `get_years`/`get_days` read `Utc::now()` (and mutate a
//!   process-global reference), which would make the kernel wall-clock-dependent.
//!   So the engine derives a **deterministic** relative day count from
//!   `expiration_utc − as_of` and prices with `ExpirationDate::Days(t)` — the
//!   analytical Greeks and the BS repricing both read `get_years()`, which for the
//!   `Days` variant is `days / 365` with no clock and no global read.
//!
//! # Absolute-UTC expiry only
//!
//! The absolute expiry that keys the sidecar and sets the time-to-expiry is read
//! from the chain via `OptionChain::get_expiration()`. A relative
//! `ExpirationDate::Days` must never reach the pricing kernel — the adapter
//! resolved a relative offset at the seam in v0.1 (§4) — so a non-absolute chain
//! expiry is asserted in dev and defensively skipped (no local analytics) in
//! release rather than priced against the wall clock.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::greeks::{delta, gamma, rho, theta, vega};
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle, OptionType, Options, Side};

use super::events::{GreeksOrigin, GreeksRow};
use super::identity::InstrumentKey;
use crate::error::ChainViewError;

// --- Documented pricing defaults (`docs/01-domain-model.md` §7) ---------------

/// The zero-config default annualized risk-free rate, as a decimal.
///
/// Sourced as `0` until a dedicated config knob lands: ChainView has no
/// risk-free-rate configuration field yet, and a conservative zero rate keeps the
/// local analytics reproducible and honest rather than assuming a hidden value.
/// The caller overrides [`PricingInputs::rate`] once a real source exists.
pub const DEFAULT_RISK_FREE_RATE: Decimal = Decimal::ZERO;

/// The zero-config default annualized dividend yield, as a decimal.
///
/// Sourced as `0` until a dedicated config knob lands, for the same reason as
/// [`DEFAULT_RISK_FREE_RATE`]. The caller overrides [`PricingInputs::dividend`].
pub const DEFAULT_DIVIDEND_YIELD: Positive = Positive::ZERO;

/// Seconds in one day — the divisor that turns an absolute
/// `expiration_utc − as_of` span into fractional days for the deterministic
/// `ExpirationDate::Days` pricing input.
const SECONDS_PER_DAY: i64 = 86_400;

// --- Pricing model and quote-selection policy ---------------------------------

/// The exercise / pricing model the local engine applies
/// (`docs/01-domain-model.md` §7).
///
/// `European` is the only variant in v1 and is the documented default — never
/// silently assumed. American-exercise pricing is out of v1 scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PricingModel {
    /// European exercise (only at expiry) — index / cash-settled options.
    #[default]
    European,
}

/// Which premium the IV inversion prices against (`docs/01-domain-model.md` §7).
///
/// Regardless of the choice, a **crossed** or **absent** quote yields no IV — a
/// crossed pair never produces a bogus number. Because the persisted
/// `OptionData` carries no last-traded price, [`QuoteSelect::Last`] resolves to
/// the current chain mid (the freshest premium the chain model holds); it stays
/// in the public surface so the projection layer (#25) can drive it once a
/// last-carrying view exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum QuoteSelect {
    /// The stored mid of the two-sided, uncrossed quote (the default).
    #[default]
    Mid,
    /// The last-traded premium; falls back to the chain mid (no last on the
    /// persisted model).
    Last,
    /// The recomputed `(bid + ask) / 2` of the two-sided, uncrossed quote.
    BidAsk,
}

/// The outcome recorded per leg after a local fill, surfaced later as the
/// computed-Greeks / stale badge (`docs/01-domain-model.md` §7).
///
/// Only the observable input conditions are distinguished: a leg is never shown
/// a stale computed number, so every non-[`Computed`](LegStatus::Computed) status
/// clears the leg's local analytics to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum LegStatus {
    /// Local analytics were computed successfully for this leg.
    Computed,
    /// No usable premium and no venue IV — nothing to price from; local
    /// analytics stay `None` (the default before any fill).
    #[default]
    NoInput,
    /// The IV-inversion quote was crossed — no IV was inverted, local analytics
    /// stay `None`.
    Crossed,
    /// The input was stale/expired: the time to expiry is non-positive as of
    /// `as_of` — local analytics stay `None`.
    Stale,
    /// `optionstratlib` returned a `GreeksError` or the IV inversion failed to
    /// converge — the local analytics are cleared to `None`.
    SolverError,
}

// --- Pricing inputs -----------------------------------------------------------

/// Every input to the local pricing pass, each with its source, unit, and as-of
/// (`docs/01-domain-model.md` §7).
///
/// - `model` — the exercise/pricing type; [`PricingModel::European`] by default,
///   documented, never silently assumed.
/// - `spot` — the underlying price (`Positive`); source: the venue
///   underlying/index stream; as-of: its `received_time`.
/// - `rate` — the annualized risk-free rate (`Decimal`); source: config default
///   ([`DEFAULT_RISK_FREE_RATE`]) until a knob lands; unit: annualized decimal.
/// - `dividend` — the annualized dividend yield (`Positive`); source: config
///   default ([`DEFAULT_DIVIDEND_YIELD`]); unit: annualized decimal.
/// - `as_of` — the freshness stamp of the analytics: the max `received_time` of
///   the inputs. It is also the **deterministic reference instant** the engine
///   measures time-to-expiry from, so the kernel never reads the wall clock.
/// - `quote_for_iv` — the [`QuoteSelect`] policy for the IV-inversion premium.
/// - `input_generation` — the cache key; it bumps whenever any input above
///   changes, so an unchanged generation does no recompute work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingInputs {
    /// The exercise/pricing model (documented default `European`).
    pub model: PricingModel,
    /// The underlying price; from the venue underlying/index stream, as of its
    /// `received_time`.
    pub spot: Positive,
    /// The annualized risk-free rate, as a decimal; config default.
    pub rate: Decimal,
    /// The annualized dividend yield, as a decimal; config default.
    pub dividend: Positive,
    /// The analytics freshness stamp and the deterministic time-to-expiry
    /// reference instant (the max `received_time` of the inputs).
    pub as_of: DateTime<Utc>,
    /// Which premium the IV inversion prices against.
    pub quote_for_iv: QuoteSelect,
    /// The cache key; bumps on any input change.
    pub input_generation: u64,
}

impl PricingInputs {
    /// Build inputs from the essentials — `spot`, the `as_of` freshness/reference
    /// instant, and the `input_generation` cache key — defaulting the model to
    /// `European`, the rate/dividend to the documented [`DEFAULT_RISK_FREE_RATE`]
    /// / [`DEFAULT_DIVIDEND_YIELD`], and the IV-quote policy to [`QuoteSelect::Mid`].
    ///
    /// The public fields let a caller override `rate`, `dividend`, `model`, or
    /// `quote_for_iv` after construction.
    #[must_use]
    pub fn new(spot: Positive, as_of: DateTime<Utc>, input_generation: u64) -> Self {
        Self {
            model: PricingModel::European,
            spot,
            rate: DEFAULT_RISK_FREE_RATE,
            dividend: DEFAULT_DIVIDEND_YIELD,
            as_of,
            quote_for_iv: QuoteSelect::Mid,
            input_generation,
        }
    }
}

// --- The per-leg analytics and the sidecar ------------------------------------

/// The per-leg analytics `OptionData` cannot hold, resolved for one style-bearing
/// [`InstrumentKey`] (`docs/01-domain-model.md` §7).
///
/// `iv` and `gamma` carry a per-style [`GreeksOrigin`]: `Provider` when the venue
/// supplied this style's value (folded in by [`GreeksSidecar::apply_venue_greeks`]),
/// else `ComputedLocally`. `delta` is the **local fallback** only — the venue
/// per-leg delta lives on `OptionData.delta_call`/`delta_put`, and the projection
/// layer (#25) prefers it — so `delta_origin` is always `ComputedLocally`.
/// `theta`/`vega`/`rho` are **always** `ComputedLocally`: `OptionData` cannot
/// store them and a venue's streamed theta/vega/rho is deliberately **discarded**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegGreeks {
    /// Implied volatility for this style — venue-supplied or locally inverted.
    pub iv: Option<Positive>,
    /// Whether `iv` came from the venue or local inversion.
    pub iv_origin: GreeksOrigin,
    /// The **local** delta fallback (the venue delta stays on `OptionData`).
    pub delta: Option<Decimal>,
    /// Always [`GreeksOrigin::ComputedLocally`] — the sidecar holds only local delta.
    pub delta_origin: GreeksOrigin,
    /// Gamma for this style — venue-supplied or locally computed.
    pub gamma: Option<Decimal>,
    /// Whether `gamma` came from the venue or local computation.
    pub gamma_origin: GreeksOrigin,
    /// Theta — always locally computed.
    pub theta: Option<Decimal>,
    /// Vega — always locally computed.
    pub vega: Option<Decimal>,
    /// Rho — always locally computed.
    pub rho: Option<Decimal>,
    /// Always [`GreeksOrigin::ComputedLocally`].
    pub theta_origin: GreeksOrigin,
    /// Always [`GreeksOrigin::ComputedLocally`].
    pub vega_origin: GreeksOrigin,
    /// Always [`GreeksOrigin::ComputedLocally`].
    pub rho_origin: GreeksOrigin,
    /// The outcome of the last local fill for this leg.
    pub status: LegStatus,
}

impl Default for LegGreeks {
    /// The empty leg: every analytic `None`, every origin `ComputedLocally`, and
    /// [`LegStatus::NoInput`].
    fn default() -> Self {
        Self {
            iv: None,
            iv_origin: GreeksOrigin::ComputedLocally,
            delta: None,
            delta_origin: GreeksOrigin::ComputedLocally,
            gamma: None,
            gamma_origin: GreeksOrigin::ComputedLocally,
            theta: None,
            vega: None,
            rho: None,
            theta_origin: GreeksOrigin::ComputedLocally,
            vega_origin: GreeksOrigin::ComputedLocally,
            rho_origin: GreeksOrigin::ComputedLocally,
            status: LegStatus::NoInput,
        }
    }
}

/// Per-instrument analytics not representable on `OptionData`, keyed by the
/// canonical style-bearing [`InstrumentKey`] (`docs/01-domain-model.md` §7).
///
/// A call leg and a put leg of one strike get **separate** entries, so venue IV
/// and gamma — which the shared `OptionData` fields can hold only once per strike
/// — are preserved per style without call/put collision. The sidecar is never
/// persisted to a bundle.
///
/// Recompute is cached by generation: [`compute_leg_greeks`] does no work when
/// the sidecar's [`computed_generation`](GreeksSidecar::computed_generation)
/// already equals the inputs' `input_generation`.
#[derive(Debug, Clone, Default)]
pub struct GreeksSidecar {
    /// One entry per style-bearing key. Private so the origin/status invariants
    /// stay inside this type; read through [`get`](GreeksSidecar::get).
    by_key: HashMap<InstrumentKey, LegGreeks>,
    /// The `input_generation` the local fields were last computed for, or `None`
    /// before any compute pass.
    computed_generation: Option<u64>,
}

impl GreeksSidecar {
    /// An empty sidecar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The analytics for one leg, or `None` when no entry exists — the read-only
    /// accessor the projection layer (#25) and `draw` use. `draw` never mutates.
    #[must_use]
    pub fn get(&self, key: &InstrumentKey) -> Option<&LegGreeks> {
        self.by_key.get(key)
    }

    /// The number of legs with an entry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// True when the sidecar holds no leg.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// The `input_generation` the local fields were last computed for, or `None`
    /// before any compute pass — the cache key [`compute_leg_greeks`] compares.
    #[must_use]
    pub fn computed_generation(&self) -> Option<u64> {
        self.computed_generation
    }

    /// Fold a venue-supplied [`GreeksRow`] into the style-keyed entry: record its
    /// per-style `iv` and `gamma` with the row's origin, and **discard** the
    /// venue `theta`/`vega`/`rho` (the sidecar holds only the local value for
    /// those three) and the venue `delta` (which lives on `OptionData`).
    ///
    /// This preserves unequal call and put IV/gamma losslessly regardless of the
    /// order the two legs arrive in, because each style keys a separate entry.
    /// An absent field leaves the prior value in place.
    pub fn apply_venue_greeks(&mut self, row: &GreeksRow) {
        let entry = self.by_key.entry(row.instrument.key.clone()).or_default();
        if let Some(iv) = row.iv {
            entry.iv = Some(iv);
            entry.iv_origin = row.origin;
        }
        if let Some(gamma) = row.gamma {
            entry.gamma = Some(gamma);
            entry.gamma_origin = row.origin;
        }
    }
}

// --- The local fill-in engine -------------------------------------------------

/// Fill in the local Greeks and IV the chain model cannot hold, for every leg of
/// `chain`, writing the result into `sink` (`docs/01-domain-model.md` §7).
///
/// For each `(strike, style)` leg the engine resolves a pricing IV — the venue
/// value when one was folded in ([`GreeksSidecar::apply_venue_greeks`], origin
/// `Provider`), else a local inversion of the selected premium (origin
/// `ComputedLocally`) — builds an [`optionstratlib::Options`], and calls the
/// `optionstratlib` Greeks functions. `theta`/`vega`/`rho` are always written
/// locally; `gamma`/`iv` keep a venue value when present and are filled locally
/// otherwise; `delta` is the local fallback only.
///
/// A **crossed** or **stale/expired** input, or an `optionstratlib` solver
/// failure, **clears** the affected leg's local analytics to `None` and records
/// the reason in [`LegGreeks::status`] — never a stale computed number. A
/// venue-supplied `iv`/`gamma` on a cleared leg is preserved.
///
/// Recompute is **cached by generation**: when `sink.computed_generation()`
/// already equals `ctx.input_generation`, the call is a no-op. This engine is
/// event-driven — call it on the market/tick event, never from `draw`.
///
/// # Errors
///
/// Returns [`ChainViewError`] to match the port-level fill contract; the current
/// implementation records every per-leg failure in the leg's status and returns
/// `Ok(())`. A chain whose expiry is not an absolute UTC instant (a v0.1
/// invariant violation) is asserted in dev and produces no analytics in release.
pub fn compute_leg_greeks(
    chain: &OptionChain,
    ctx: &PricingInputs,
    sink: &mut GreeksSidecar,
) -> Result<(), ChainViewError> {
    // Cache: an unchanged generation does no work.
    if sink.computed_generation == Some(ctx.input_generation) {
        return Ok(());
    }

    // The absolute expiry keys the sidecar and sets the time-to-expiry. A
    // relative `Days` (or an unparseable expiry) must never reach the kernel —
    // the adapter resolves expiry to an absolute instant at the seam (§4).
    let expiration_utc = match chain.get_expiration() {
        Some(ExpirationDate::DateTime(dt)) => dt,
        other => {
            debug_assert!(
                !matches!(other, Some(ExpirationDate::Days(_))),
                "compute_leg_greeks requires an absolute-UTC chain expiry; a relative \
                 Days offset must be resolved at the adapter seam"
            );
            // Defensive skip: mark the generation handled so a malformed chain
            // degrades to "no local analytics" without a per-event busy loop.
            sink.computed_generation = Some(ctx.input_generation);
            return Ok(());
        }
    };

    for od in &chain.options {
        for style in [OptionStyle::Call, OptionStyle::Put] {
            compute_one_leg(od, style, &chain.symbol, expiration_utc, ctx, sink);
        }
    }

    sink.computed_generation = Some(ctx.input_generation);
    Ok(())
}

/// Compute (or clear) one `(strike, style)` leg and store the result in `sink`.
fn compute_one_leg(
    od: &OptionData,
    style: OptionStyle,
    symbol: &str,
    expiration_utc: DateTime<Utc>,
    ctx: &PricingInputs,
    sink: &mut GreeksSidecar,
) {
    let key = InstrumentKey {
        underlying: symbol.to_owned(),
        expiration_utc,
        strike: od.strike_price,
        style,
    };
    let existing = sink.by_key.get(&key).copied().unwrap_or_default();

    // Deterministic time-to-expiry in fractional days from `as_of` — never the
    // wall clock. A non-positive span is a stale/expired input.
    let Some(days) = days_between(ctx.as_of, expiration_utc) else {
        let _ = sink
            .by_key
            .insert(key, cleared_entry(existing, LegStatus::Stale));
        return;
    };

    // Resolve the pricing IV: a venue value wins; otherwise invert locally.
    let (pricing_iv, iv_origin) = if existing.iv_origin == GreeksOrigin::Provider
        && let Some(iv) = existing.iv
    {
        (iv, GreeksOrigin::Provider)
    } else {
        match select_iv_premium(od, style, ctx.quote_for_iv) {
            QuotePick::Crossed => {
                let _ = sink
                    .by_key
                    .insert(key, cleared_entry(existing, LegStatus::Crossed));
                return;
            }
            QuotePick::Absent => {
                let _ = sink
                    .by_key
                    .insert(key, cleared_entry(existing, LegStatus::NoInput));
                return;
            }
            QuotePick::Premium(premium) => {
                match invert_iv(
                    symbol.to_owned(),
                    od.strike_price,
                    days,
                    style,
                    premium,
                    ctx,
                ) {
                    Some(iv) => (iv, GreeksOrigin::ComputedLocally),
                    None => {
                        let _ = sink
                            .by_key
                            .insert(key, cleared_entry(existing, LegStatus::SolverError));
                        return;
                    }
                }
            }
        }
    };

    // Price the leg and compute the Greeks. Any solver failure clears the leg.
    let option = build_option(
        symbol.to_owned(),
        od.strike_price,
        days,
        pricing_iv,
        style,
        ctx,
    );
    let (Ok(local_delta), Ok(local_gamma), Ok(local_theta), Ok(local_vega), Ok(local_rho)) = (
        delta(&option),
        gamma(&option),
        theta(&option),
        vega(&option),
        rho(&option),
    ) else {
        let _ = sink
            .by_key
            .insert(key, cleared_entry(existing, LegStatus::SolverError));
        return;
    };

    // Gamma keeps a venue value when present; otherwise it is the local value.
    let (gamma_value, gamma_origin) = if existing.gamma_origin == GreeksOrigin::Provider
        && let Some(g) = existing.gamma
    {
        (Some(g), GreeksOrigin::Provider)
    } else {
        (Some(local_gamma), GreeksOrigin::ComputedLocally)
    };

    let entry = LegGreeks {
        iv: Some(pricing_iv),
        iv_origin,
        delta: Some(local_delta),
        delta_origin: GreeksOrigin::ComputedLocally,
        gamma: gamma_value,
        gamma_origin,
        theta: Some(local_theta),
        vega: Some(local_vega),
        rho: Some(local_rho),
        theta_origin: GreeksOrigin::ComputedLocally,
        vega_origin: GreeksOrigin::ComputedLocally,
        rho_origin: GreeksOrigin::ComputedLocally,
        status: LegStatus::Computed,
    };
    let _ = sink.by_key.insert(key, entry);
}

/// The IV-inversion premium pick for one leg.
enum QuotePick {
    /// A usable two-sided, uncrossed premium.
    Premium(Positive),
    /// The two-sided quote is crossed — no premium.
    Crossed,
    /// No two-sided quote — no premium.
    Absent,
}

/// Select the IV-inversion premium per the [`QuoteSelect`] policy. A one-sided or
/// absent quote yields [`QuotePick::Absent`]; a crossed pair yields
/// [`QuotePick::Crossed`] — never a bogus premium.
#[must_use]
fn select_iv_premium(od: &OptionData, style: OptionStyle, select: QuoteSelect) -> QuotePick {
    let (bid, ask, middle) = match style {
        OptionStyle::Call => (od.call_bid, od.call_ask, od.call_middle),
        OptionStyle::Put => (od.put_bid, od.put_ask, od.put_middle),
    };
    let (Some(bid), Some(ask)) = (bid, ask) else {
        return QuotePick::Absent;
    };
    if is_crossed(bid, ask) {
        return QuotePick::Crossed;
    }
    let recomputed = midpoint(bid, ask);
    let premium = match select {
        // The persisted model carries no last-traded price, so `Last` and `Mid`
        // both prefer the stored mid (falling back to the recomputed midpoint).
        QuoteSelect::Mid | QuoteSelect::Last => middle.unwrap_or(recomputed),
        QuoteSelect::BidAsk => recomputed,
    };
    QuotePick::Premium(premium)
}

/// Invert IV from a market premium via the deterministic
/// [`Options::calculate_implied_volatility`] binary search. `None` on any solver
/// failure (no convergence, degenerate inputs).
#[must_use]
fn invert_iv(
    symbol: String,
    strike: Positive,
    days: Positive,
    style: OptionStyle,
    premium: Positive,
    ctx: &PricingInputs,
) -> Option<Positive> {
    // The starting IV is a placeholder; the search overwrites it internally.
    let option = build_option(symbol, strike, days, Positive::ONE, style, ctx);
    option.calculate_implied_volatility(premium.to_dec()).ok()
}

/// Build an `optionstratlib::Options` for the leg, pricing with a **deterministic**
/// `ExpirationDate::Days(days)` derived from `as_of` (never the wall clock) and a
/// unit long position.
#[must_use]
fn build_option(
    symbol: String,
    strike: Positive,
    days: Positive,
    iv: Positive,
    style: OptionStyle,
    ctx: &PricingInputs,
) -> Options {
    let option_type = match ctx.model {
        PricingModel::European => OptionType::European,
    };
    Options::new(
        option_type,
        Side::Long,
        symbol,
        strike,
        ExpirationDate::Days(days),
        iv,
        Positive::ONE,
        ctx.spot,
        ctx.rate,
        style,
        ctx.dividend,
        None,
    )
}

/// A cleared leg: local analytics `None`, `status` recorded, and any
/// venue-supplied `iv`/`gamma` preserved — never a stale computed number.
#[must_use]
fn cleared_entry(existing: LegGreeks, status: LegStatus) -> LegGreeks {
    let mut cleared = LegGreeks::default();
    if existing.iv_origin == GreeksOrigin::Provider && existing.iv.is_some() {
        cleared.iv = existing.iv;
        cleared.iv_origin = GreeksOrigin::Provider;
    }
    if existing.gamma_origin == GreeksOrigin::Provider && existing.gamma.is_some() {
        cleared.gamma = existing.gamma;
        cleared.gamma_origin = GreeksOrigin::Provider;
    }
    cleared.status = status;
    cleared
}

/// The deterministic fractional-day span from `as_of` to `expiration_utc`, or
/// `None` when the span is non-positive (a stale/expired input). Computed in
/// integer seconds and `Decimal`, so no `f64` enters the kernel.
#[must_use]
fn days_between(as_of: DateTime<Utc>, expiration_utc: DateTime<Utc>) -> Option<Positive> {
    let seconds = expiration_utc.signed_duration_since(as_of).num_seconds();
    if seconds <= 0 {
        return None;
    }
    let days = Decimal::from(seconds).checked_div(Decimal::from(SECONDS_PER_DAY))?;
    Positive::new_decimal(days).ok()
}

/// True when a bid/ask pair is crossed: `ask < bid`, or a zero ask on a non-zero
/// bid — mirrors the chain store's crossed rule (`docs/03-data-providers.md` §3).
#[must_use]
fn is_crossed(bid: Positive, ask: Positive) -> bool {
    ask < bid || (ask.is_zero() && !bid.is_zero())
}

/// The midpoint of a bid/ask pair, rounded to 4 dp — matching the chain store's
/// mid convention.
#[must_use]
fn midpoint(bid: Positive, ask: Positive) -> Positive {
    ((bid + ask) / Positive::TWO).round_to(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::identity::{
        ContractSpecFingerprint, ExerciseStyle, Instrument, ProviderId, SettlementStyle,
    };

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

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

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    /// A signed decimal from mantissa + scale (the `dec!` macro needs a direct
    /// `rust_decimal` dependency).
    fn dec(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    /// An absolute-UTC expiry: 2025-06-27 (the chain's expiration string parses to
    /// `2025-06-27T18:30:00Z`).
    const EXPIRY_STRING: &str = "2025-06-27";
    /// A reference instant well before the expiry (2025-01-01) — positive TTE.
    const AS_OF_BEFORE: i64 = 1_735_689_600;
    /// A reference instant after the expiry (2025-12-31) — non-positive TTE.
    const AS_OF_AFTER: i64 = 1_767_139_200;

    fn od(
        strike: f64,
        call_bid: Option<f64>,
        call_ask: Option<f64>,
        put_bid: Option<f64>,
        put_ask: Option<f64>,
    ) -> OptionData {
        let mut data = OptionData {
            strike_price: pos(strike),
            call_bid: call_bid.map(pos),
            call_ask: call_ask.map(pos),
            put_bid: put_bid.map(pos),
            put_ask: put_ask.map(pos),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        data.set_mid_prices();
        data
    }

    fn chain_of(rows: &[OptionData]) -> OptionChain {
        let mut chain =
            OptionChain::new("BTC", pos(60_000.0), EXPIRY_STRING.to_owned(), None, None);
        for row in rows {
            let _ = chain.options.insert(row.clone());
        }
        chain
    }

    /// A one-strike ATM chain: a two-sided call and put around 3000/2000.
    fn atm_chain() -> OptionChain {
        chain_of(&[od(
            60_000.0,
            Some(3_000.0),
            Some(3_100.0),
            Some(2_000.0),
            Some(2_100.0),
        )])
    }

    fn inputs(as_of: i64, generation: u64) -> PricingInputs {
        PricingInputs::new(pos(60_000.0), utc(as_of), generation)
    }

    fn key(strike: f64, style: OptionStyle) -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            // The parser resolves "2025-06-27" to 18:30:00 UTC; the sidecar keys on
            // whatever absolute instant the chain resolves to, so re-derive it.
            expiration_utc: resolved_expiry(),
            strike: pos(strike),
            style,
        }
    }

    /// The absolute expiry the chain resolves to, so a test key matches the
    /// sidecar key the engine builds.
    #[track_caller]
    fn resolved_expiry() -> DateTime<Utc> {
        match chain_of(&[]).get_expiration() {
            Some(ExpirationDate::DateTime(dt)) => dt,
            other => panic!("expected an absolute-UTC chain expiry, got {other:?}"),
        }
    }

    fn instrument(strike: f64, style: OptionStyle) -> Instrument {
        Instrument {
            key: key(strike, style),
            provider: pid("deribit"),
            native_symbol: format!("BTC-{strike}"),
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

    fn venue_greeks(
        strike: f64,
        style: OptionStyle,
        iv: Option<f64>,
        gamma: Option<Decimal>,
    ) -> GreeksRow {
        GreeksRow {
            instrument: instrument(strike, style),
            iv: iv.map(pos),
            delta: Some(dec(-25, 2)),
            gamma,
            // Venue theta/vega/rho are deliberately discarded by the sidecar.
            theta: Some(dec(-1, 1)),
            vega: Some(dec(2, 1)),
            rho: Some(dec(3, 1)),
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(AS_OF_BEFORE),
        }
    }

    #[track_caller]
    fn leg(sink: &GreeksSidecar, strike: f64, style: OptionStyle) -> LegGreeks {
        match sink.get(&key(strike, style)) {
            Some(g) => *g,
            None => panic!("expected a sidecar entry for strike {strike} {style:?}"),
        }
    }

    #[track_caller]
    fn compute(chain: &OptionChain, ctx: &PricingInputs, sink: &mut GreeksSidecar) {
        match compute_leg_greeks(chain, ctx, sink) {
            Ok(()) => {}
            Err(e) => panic!("compute_leg_greeks failed: {e}"),
        }
    }

    // --- Defaults ------------------------------------------------------------

    #[test]
    fn test_pricing_model_default_is_european() {
        assert_eq!(PricingModel::default(), PricingModel::European);
    }

    #[test]
    fn test_quote_select_default_is_mid() {
        assert_eq!(QuoteSelect::default(), QuoteSelect::Mid);
    }

    #[test]
    fn test_pricing_inputs_new_applies_documented_defaults() {
        let ctx = inputs(AS_OF_BEFORE, 1);
        assert_eq!(ctx.model, PricingModel::European);
        assert_eq!(ctx.rate, DEFAULT_RISK_FREE_RATE);
        assert_eq!(ctx.dividend, DEFAULT_DIVIDEND_YIELD);
        assert_eq!(ctx.quote_for_iv, QuoteSelect::Mid);
        assert_eq!(ctx.input_generation, 1);
    }

    // --- Happy path ----------------------------------------------------------

    #[test]
    fn test_compute_leg_greeks_local_inversion_fills_all_analytics() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut sink = GreeksSidecar::new();
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::Computed);
        // IV inverted locally from the two-sided premium.
        assert!(call.iv.is_some());
        assert_eq!(call.iv_origin, GreeksOrigin::ComputedLocally);
        // Every local Greek is present and locally computed.
        assert!(call.delta.is_some());
        assert!(call.gamma.is_some());
        assert!(call.theta.is_some());
        assert!(call.vega.is_some());
        assert!(call.rho.is_some());
        assert_eq!(call.delta_origin, GreeksOrigin::ComputedLocally);
        assert_eq!(call.gamma_origin, GreeksOrigin::ComputedLocally);
        assert_eq!(call.theta_origin, GreeksOrigin::ComputedLocally);
        assert_eq!(call.vega_origin, GreeksOrigin::ComputedLocally);
        assert_eq!(call.rho_origin, GreeksOrigin::ComputedLocally);
        // Both legs of the strike are filled independently.
        let put = leg(&sink, 60_000.0, OptionStyle::Put);
        assert_eq!(put.status, LegStatus::Computed);
        assert!(put.theta.is_some());
    }

    #[test]
    fn test_compute_leg_greeks_prefers_venue_iv_over_local_inversion() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut sink = GreeksSidecar::new();
        // A venue IV arrives for the call before the local pass.
        sink.apply_venue_greeks(&venue_greeks(60_000.0, OptionStyle::Call, Some(0.55), None));
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        // Venue IV wins; theta/vega/rho are still local.
        assert_eq!(call.iv, Some(pos(0.55)));
        assert_eq!(call.iv_origin, GreeksOrigin::Provider);
        assert_eq!(call.theta_origin, GreeksOrigin::ComputedLocally);
        assert!(call.theta.is_some());
        assert_eq!(call.status, LegStatus::Computed);
    }

    #[test]
    fn test_compute_leg_greeks_prefers_venue_gamma_over_local() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut sink = GreeksSidecar::new();
        sink.apply_venue_greeks(&venue_greeks(
            60_000.0,
            OptionStyle::Call,
            Some(0.5),
            Some(dec(7, 4)),
        ));
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.gamma, Some(dec(7, 4)));
        assert_eq!(call.gamma_origin, GreeksOrigin::Provider);
    }

    // --- Crossed / stale / absent inputs clear the leg -----------------------

    #[test]
    fn test_compute_leg_greeks_crossed_quote_leaves_iv_and_greeks_none() {
        // Call ask (1.0) below bid (2.0) — crossed.
        let chain = chain_of(&[od(
            60_000.0,
            Some(2_000.0),
            Some(1_000.0),
            Some(2_000.0),
            Some(2_100.0),
        )]);
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut sink = GreeksSidecar::new();
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::Crossed);
        assert!(call.iv.is_none());
        assert!(call.delta.is_none());
        assert!(call.gamma.is_none());
        assert!(call.theta.is_none());
        assert!(call.vega.is_none());
        assert!(call.rho.is_none());
        // The uncrossed put still computes.
        assert_eq!(
            leg(&sink, 60_000.0, OptionStyle::Put).status,
            LegStatus::Computed
        );
    }

    #[test]
    fn test_compute_leg_greeks_stale_input_leaves_none() {
        let chain = atm_chain();
        // `as_of` after the expiry: non-positive time to expiry.
        let ctx = inputs(AS_OF_AFTER, 1);
        let mut sink = GreeksSidecar::new();
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::Stale);
        assert!(call.iv.is_none());
        assert!(call.theta.is_none());
    }

    #[test]
    fn test_compute_leg_greeks_absent_quote_leaves_none() {
        // A one-sided call (no ask) yields no IV-inversion premium.
        let chain = chain_of(&[od(
            60_000.0,
            Some(3_000.0),
            None,
            Some(2_000.0),
            Some(2_100.0),
        )]);
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut sink = GreeksSidecar::new();
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::NoInput);
        assert!(call.iv.is_none());
        assert!(call.vega.is_none());
    }

    // --- Solver error clears the leg but preserves venue values --------------

    #[test]
    fn test_compute_leg_greeks_solver_error_clears_leg_keeps_venue_iv() {
        let chain = atm_chain();
        // A zero spot drives the Black-Scholes kernel non-finite (ln(0)), so the
        // Greeks functions return a `GreeksError` for every leg.
        let mut ctx = inputs(AS_OF_BEFORE, 1);
        ctx.spot = Positive::ZERO;
        let mut sink = GreeksSidecar::new();
        // A venue IV arrives so the failure is in the Greeks step, not inversion.
        sink.apply_venue_greeks(&venue_greeks(60_000.0, OptionStyle::Call, Some(0.5), None));
        compute(&chain, &ctx, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::SolverError);
        // Local Greeks cleared; the venue IV is preserved.
        assert!(call.theta.is_none());
        assert!(call.vega.is_none());
        assert!(call.delta.is_none());
        assert_eq!(call.iv, Some(pos(0.5)));
        assert_eq!(call.iv_origin, GreeksOrigin::Provider);
    }

    // --- The style-keyed lossless property -----------------------------------

    #[test]
    fn test_apply_venue_greeks_call_then_put_retains_both_legs() {
        let mut sink = GreeksSidecar::new();
        sink.apply_venue_greeks(&venue_greeks(
            60_000.0,
            OptionStyle::Call,
            Some(0.4),
            Some(dec(1, 2)),
        ));
        sink.apply_venue_greeks(&venue_greeks(
            60_000.0,
            OptionStyle::Put,
            Some(0.6),
            Some(dec(2, 2)),
        ));
        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        let put = leg(&sink, 60_000.0, OptionStyle::Put);
        assert_eq!(call.iv, Some(pos(0.4)));
        assert_eq!(call.gamma, Some(dec(1, 2)));
        assert_eq!(put.iv, Some(pos(0.6)));
        assert_eq!(put.gamma, Some(dec(2, 2)));
    }

    #[test]
    fn test_apply_venue_greeks_put_then_call_retains_both_legs() {
        let mut sink = GreeksSidecar::new();
        sink.apply_venue_greeks(&venue_greeks(
            60_000.0,
            OptionStyle::Put,
            Some(0.6),
            Some(dec(2, 2)),
        ));
        sink.apply_venue_greeks(&venue_greeks(
            60_000.0,
            OptionStyle::Call,
            Some(0.4),
            Some(dec(1, 2)),
        ));
        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        let put = leg(&sink, 60_000.0, OptionStyle::Put);
        // Order-independent: neither leg overwrote the other.
        assert_eq!(call.iv, Some(pos(0.4)));
        assert_eq!(call.gamma, Some(dec(1, 2)));
        assert_eq!(put.iv, Some(pos(0.6)));
        assert_eq!(put.gamma, Some(dec(2, 2)));
    }

    #[test]
    fn test_apply_venue_greeks_discards_venue_theta_vega_rho() {
        let mut sink = GreeksSidecar::new();
        // The row carries theta/vega/rho; the sidecar must not adopt them.
        sink.apply_venue_greeks(&venue_greeks(
            60_000.0,
            OptionStyle::Call,
            Some(0.4),
            Some(dec(1, 2)),
        ));
        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert!(call.theta.is_none());
        assert!(call.vega.is_none());
        assert!(call.rho.is_none());
        assert!(call.delta.is_none());
    }

    // --- Caching by input_generation -----------------------------------------

    #[test]
    fn test_compute_leg_greeks_cached_generation_does_no_work() {
        let mut sink = GreeksSidecar::new();
        // First pass on the ATM chain at generation 1.
        compute(&atm_chain(), &inputs(AS_OF_BEFORE, 1), &mut sink);
        let before = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(before.status, LegStatus::Computed);

        // A second call at the SAME generation with a crossed chain must be a
        // no-op — the cached (computed) value is retained.
        let crossed = chain_of(&[od(
            60_000.0,
            Some(2_000.0),
            Some(1_000.0),
            Some(2_000.0),
            Some(2_100.0),
        )]);
        compute(&crossed, &inputs(AS_OF_BEFORE, 1), &mut sink);
        assert_eq!(leg(&sink, 60_000.0, OptionStyle::Call), before);

        // Bumping the generation forces the recompute, which now clears the leg.
        compute(&crossed, &inputs(AS_OF_BEFORE, 2), &mut sink);
        assert_eq!(
            leg(&sink, 60_000.0, OptionStyle::Call).status,
            LegStatus::Crossed
        );
    }

    #[test]
    fn test_compute_leg_greeks_records_computed_generation() {
        let mut sink = GreeksSidecar::new();
        assert_eq!(sink.computed_generation(), None);
        compute(&atm_chain(), &inputs(AS_OF_BEFORE, 7), &mut sink);
        assert_eq!(sink.computed_generation(), Some(7));
    }

    // --- Determinism ---------------------------------------------------------

    #[test]
    fn test_compute_leg_greeks_is_deterministic_for_equal_inputs() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut a = GreeksSidecar::new();
        let mut b = GreeksSidecar::new();
        compute(&chain, &ctx, &mut a);
        compute(&chain, &ctx, &mut b);
        assert_eq!(
            leg(&a, 60_000.0, OptionStyle::Call),
            leg(&b, 60_000.0, OptionStyle::Call)
        );
        assert_eq!(
            leg(&a, 60_000.0, OptionStyle::Put),
            leg(&b, 60_000.0, OptionStyle::Put)
        );
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// The compute kernel is deterministic: identical inputs give
            /// bit-identical sidecar entries over the input space (no wall clock,
            /// no unseeded RNG).
            #[test]
            fn prop_greeks_fill_deterministic(
                spot_ticks in 40_000u32..80_000,
                rate_bp in 0i64..1_000,
                call_bid_ticks in 500u32..5_000,
                spread in 1u32..500,
            ) {
                let bid = f64::from(call_bid_ticks);
                let ask = bid + f64::from(spread);
                let chain = chain_of(&[od(
                    60_000.0,
                    Some(bid),
                    Some(ask),
                    Some(2_000.0),
                    Some(2_100.0),
                )]);
                let mut ctx = inputs(AS_OF_BEFORE, 1);
                ctx.spot = pos(f64::from(spot_ticks));
                ctx.rate = Decimal::new(rate_bp, 4);

                let mut a = GreeksSidecar::new();
                let mut b = GreeksSidecar::new();
                match (
                    compute_leg_greeks(&chain, &ctx, &mut a),
                    compute_leg_greeks(&chain, &ctx, &mut b),
                ) {
                    (Ok(()), Ok(())) => {}
                    other => panic!("compute failed: {other:?}"),
                }
                let ka = key(60_000.0, OptionStyle::Call);
                prop_assert_eq!(a.get(&ka), b.get(&ka));
                let kp = key(60_000.0, OptionStyle::Put);
                prop_assert_eq!(a.get(&kp), b.get(&kp));
            }
        }
    }
}
