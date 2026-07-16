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
use std::time::Duration;

use chrono::{DateTime, Utc};
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::greeks::{delta, gamma, rho, theta, vega};
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle, OptionType, Options, Side};

use super::events::{GreeksOrigin, GreeksRow, QUOTE_STALE_AFTER};
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

/// The smallest **locally computed** implied volatility, as a fraction
/// (`0.005` = 0.5%; IV is a fraction, so `0.4922` renders `49.22%`), that is
/// economically plausible enough to feed a display analytic.
///
/// A live listed option quoting real premium with a sub-0.5% IV is almost always
/// a mispriced/garbage **local** inversion — e.g. a Deribit inverse, BTC-settled
/// contract whose premium is denominated in the wrong currency (issue #83) — not a
/// real quote; the same reasoning as the exact-zero absent-IV sentinel. This is the
/// **domain** home of that floor so both IV consumers share one definition: the
/// chain matrix (`src/ui/chain.rs`, #25) clears a sub-floor `ComputedLocally` IV to
/// `—`, and the payoff t+0 curve (`src/app/payoff_build.rs`, #27) treats a sub-floor
/// `ComputedLocally` leg IV as "no reliable IV" and renders the t+0 curve
/// unavailable while the IV-independent expiration curve still renders. A **venue**
/// (`Provider`) IV is trusted and never floored.
pub const MIN_PLAUSIBLE_LOCAL_IV: Decimal = Decimal::from_parts(5, 0, 0, false, 3);

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
    /// The two-sided quote the IV inversion would price against went **stale**:
    /// its stream receipt clock is older than [`QUOTE_STALE_AFTER`] relative to
    /// the analytics `as_of`, so the leg is NOT inverted (a silent quote is never
    /// dressed up as a fresh [`Computed`](LegStatus::Computed) IV). Local
    /// analytics stay `None`; any venue `iv`/`gamma` is preserved. Distinct from
    /// [`Stale`](LegStatus::Stale), which is the OPTION expiring (non-positive time
    /// to expiry), not the quote going silent.
    StaleQuote,
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

// --- Per-leg quote freshness (the IV-inversion freshness seam) -----------------

/// The per-leg **stream-quote receipt clocks** the local IV inversion consults so
/// it never inverts a stale quote (`docs/01-domain-model.md` §7, §5.1).
///
/// `optionstratlib::OptionData` carries no receipt time, so on its own the kernel
/// cannot tell a fresh two-sided quote from one whose feed went silent minutes
/// ago — and inverting the silent one would badge a stale number as
/// [`Computed`](LegStatus::Computed), as trustworthy-looking as a live one. The
/// store owns the authoritative per-instrument quote-receipt clock (issue #7's
/// freshness sidecar, `quote_received`); it hands the engine a **read-only
/// snapshot** of those clocks as PURE DATA, keyed by the same style-bearing
/// [`InstrumentKey`] the engine builds per leg.
///
/// Passing the clocks as data — never a store handle, never a wall-clock read —
/// keeps the kernel deterministic: the freshness decision is a comparison of two
/// receipt-domain timestamps (a leg's `received` against
/// [`PricingInputs::as_of`]) against the documented [`QUOTE_STALE_AFTER`]
/// threshold. A leg with **no entry** carries no known stream-quote staleness — a
/// poll-seeded quote is as fresh as the chain structure it arrived with — so its
/// premium is inverted exactly as before; the empty [`QuoteClocks::new`] is the
/// zero-config "no freshness signal" input.
#[derive(Debug, Clone, Default)]
pub struct QuoteClocks {
    /// The last stream-quote `received_time` per style-bearing leg. Private so the
    /// lookup stays read-through [`received`](QuoteClocks::received).
    received: HashMap<InstrumentKey, DateTime<Utc>>,
}

impl QuoteClocks {
    /// An empty set of clocks — no leg carries a known stream-quote receipt time,
    /// so every premium inverts as if fresh (the caller supplies no freshness
    /// signal).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or overwrite) the last stream-quote `received_time` for one leg.
    /// The store calls this per instrument when it builds the snapshot it hands
    /// the engine.
    pub fn insert(&mut self, key: InstrumentKey, received: DateTime<Utc>) {
        let _ = self.received.insert(key, received);
    }

    /// The last stream-quote `received_time` for one leg, or `None` when the leg
    /// has no recorded stream quote (a poll-seeded or never-streamed leg).
    #[must_use]
    pub fn received(&self, key: &InstrumentKey) -> Option<DateTime<Utc>> {
        self.received.get(key).copied()
    }

    /// The number of legs with a recorded receipt clock.
    #[must_use]
    pub fn len(&self) -> usize {
        self.received.len()
    }

    /// True when no leg has a recorded receipt clock.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.received.is_empty()
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
/// A **crossed**, **stale-quote**, or **stale/expired** input, or an
/// `optionstratlib` solver failure, **clears** the affected leg's local analytics
/// to `None` and records the reason in [`LegGreeks::status`] — never a stale
/// computed number. A venue-supplied `iv`/`gamma` on a cleared leg is preserved.
///
/// `quotes` carries the per-leg stream-quote receipt clocks (`QuoteClocks`): a
/// leg whose two-sided quote is older than [`QUOTE_STALE_AFTER`] relative to
/// `ctx.as_of` is NOT locally inverted — it is cleared to
/// [`LegStatus::StaleQuote`] and renders an em dash — so a silent feed's last
/// quote is never dressed up as a fresh [`Computed`](LegStatus::Computed) IV. The
/// freshness decision is a deterministic comparison of two receipt-domain
/// timestamps (never a wall-clock read); a leg absent from `quotes` inverts as
/// before. Freshness gates the **local inversion only** — a venue-supplied IV is
/// used regardless.
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
    quotes: &QuoteClocks,
    sink: &mut GreeksSidecar,
) -> Result<(), ChainViewError> {
    // Cache: an unchanged generation does no work.
    if sink.computed_generation == Some(ctx.input_generation) {
        return Ok(());
    }

    let Some(expiration_utc) = resolve_chain_expiry(chain, ctx, sink) else {
        return Ok(());
    };

    for od in &chain.options {
        for style in [OptionStyle::Call, OptionStyle::Put] {
            compute_one_leg(od, style, &chain.symbol, expiration_utc, ctx, quotes, sink);
        }
    }

    sink.computed_generation = Some(ctx.input_generation);
    Ok(())
}

/// Recompute ONLY the named dirty `(strike, style)` legs into `sink` — the hot-path
/// counterpart to the full-chain [`compute_leg_greeks`] (`docs/01-domain-model.md`
/// §7).
///
/// An applied stream quote / Greeks row touches a SINGLE leg (one strike, one
/// style), so the chain store folds it in O(changed legs) by repricing just that
/// leg rather than every strike on every tick — the full-chain pass is reserved
/// for the poll path, where a fresh snapshot legitimately invalidates every leg.
/// Each named leg is priced EXACTLY as the full pass would price it — same
/// [`PricingInputs`], same per-leg [`QuoteClocks`] freshness gate, same venue
/// iv/gamma preference and same [`LegStatus`] clearing — so a dirty recompute of a
/// leg is bit-identical to the full recompute of that leg; the untouched legs are
/// deliberately left at their prior generation, refreshed by the next full poll.
///
/// Unlike [`compute_leg_greeks`] there is NO generation short-circuit: the caller
/// has bumped `input_generation` for the leg it changed and this pass always
/// reprices the named legs, then records `input_generation` as the sidecar's
/// computed generation (so a later full pass at that generation is a cache no-op).
/// A named leg whose strike is absent from `chain` has nothing to price and is
/// skipped (defensive — the store only marks a present strike dirty).
///
/// # Errors
///
/// Mirrors [`compute_leg_greeks`]: every per-leg outcome is recorded in the leg's
/// status and `Ok(())` is returned; a chain whose expiry is not an absolute UTC
/// instant degrades to "no local analytics".
pub fn compute_dirty_legs(
    chain: &OptionChain,
    ctx: &PricingInputs,
    quotes: &QuoteClocks,
    dirty: &[(Positive, OptionStyle)],
    sink: &mut GreeksSidecar,
) -> Result<(), ChainViewError> {
    let Some(expiration_utc) = resolve_chain_expiry(chain, ctx, sink) else {
        return Ok(());
    };

    for &(strike, style) in dirty {
        if let Some(od) = chain.options.get(&probe_row(strike)) {
            compute_one_leg(od, style, &chain.symbol, expiration_utc, ctx, quotes, sink);
        }
    }

    sink.computed_generation = Some(ctx.input_generation);
    Ok(())
}

/// Resolve the chain's absolute-UTC expiry for a pricing pass, or degrade to "no
/// local analytics" when the chain expiry is not an absolute instant.
///
/// The absolute expiry keys the sidecar and sets the time-to-expiry. A relative
/// `Days` (or an unparseable expiry) must never reach the kernel — the adapter
/// resolves expiry to an absolute instant at the seam (§4). On the (invariant-
/// violating) non-absolute path the generation is marked handled so a malformed
/// chain degrades without a per-event busy loop, and `None` is returned so the
/// caller returns without pricing.
fn resolve_chain_expiry(
    chain: &OptionChain,
    ctx: &PricingInputs,
    sink: &mut GreeksSidecar,
) -> Option<DateTime<Utc>> {
    match chain.get_expiration() {
        Some(ExpirationDate::DateTime(dt)) => Some(dt),
        other => {
            debug_assert!(
                !matches!(other, Some(ExpirationDate::Days(_))),
                "local Greeks require an absolute-UTC chain expiry; a relative \
                 Days offset must be resolved at the adapter seam"
            );
            sink.computed_generation = Some(ctx.input_generation);
            None
        }
    }
}

/// Compute (or clear) one `(strike, style)` leg and store the result in `sink`.
fn compute_one_leg(
    od: &OptionData,
    style: OptionStyle,
    symbol: &str,
    expiration_utc: DateTime<Utc>,
    ctx: &PricingInputs,
    quotes: &QuoteClocks,
    sink: &mut GreeksSidecar,
) {
    let key = InstrumentKey {
        underlying: symbol.to_owned(),
        expiration_utc,
        strike: od.strike_price,
        style,
    };
    let existing = sink.by_key.get(&key).copied().unwrap_or_default();

    // Per-leg quote freshness (§5.1): a stream quote whose receipt clock is older
    // than the documented threshold relative to `as_of` must not be inverted. A
    // leg with no recorded clock (poll-seeded / never-streamed) is not gated.
    let quote_stale = quotes
        .received(&key)
        .is_some_and(|received| quote_is_stale(received, ctx.as_of));

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
        match select_iv_premium(od, style, ctx.quote_for_iv, quote_stale) {
            QuotePick::Crossed => {
                let _ = sink
                    .by_key
                    .insert(key, cleared_entry(existing, LegStatus::Crossed));
                return;
            }
            QuotePick::Stale => {
                let _ = sink
                    .by_key
                    .insert(key, cleared_entry(existing, LegStatus::StaleQuote));
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
    /// A usable two-sided, uncrossed, fresh premium.
    Premium(Positive),
    /// The two-sided quote is crossed — no premium.
    Crossed,
    /// The two-sided quote is uncrossed but its stream receipt clock is stale
    /// beyond [`QUOTE_STALE_AFTER`] — no premium (never invert a silent quote).
    Stale,
    /// No two-sided quote — no premium.
    Absent,
}

/// Select the IV-inversion premium per the [`QuoteSelect`] policy. A one-sided or
/// absent quote yields [`QuotePick::Absent`]; a crossed pair yields
/// [`QuotePick::Crossed`]; a two-sided uncrossed pair whose stream quote is
/// `quote_stale` yields [`QuotePick::Stale`] — never a bogus or stale premium.
///
/// `quote_stale` is a caller-computed freshness input (a receipt-domain timestamp
/// comparison, not a wall-clock read), so this selection stays deterministic. It
/// is checked only after the quote is known two-sided and uncrossed: an absent or
/// crossed quote reports its own condition regardless of freshness.
#[must_use]
fn select_iv_premium(
    od: &OptionData,
    style: OptionStyle,
    select: QuoteSelect,
    quote_stale: bool,
) -> QuotePick {
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
    if quote_stale {
        return QuotePick::Stale;
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

/// True when a stream quote received at `received` is stale relative to the
/// analytics reference instant `as_of` — its age exceeds [`QUOTE_STALE_AFTER`].
///
/// Both timestamps are in ChainView's receipt-time domain (`received_time`), so
/// this is a pure, deterministic comparison of two data inputs — never a
/// wall-clock read. It mirrors the store's `classify` (`> threshold` is stale, so
/// exactly at the threshold is still fresh) and clamps a non-positive age (a
/// `received` at or after `as_of`, from clock skew) to fresh.
#[must_use]
pub(crate) fn quote_is_stale(received: DateTime<Utc>, as_of: DateTime<Utc>) -> bool {
    let age = as_of
        .signed_duration_since(received)
        .to_std()
        .unwrap_or(Duration::ZERO);
    age > QUOTE_STALE_AFTER
}

/// A strike-only probe row for a `BTreeSet<OptionData>` lookup — `OptionData`'s
/// `Ord` is its `strike_price`, so a probe carrying just the strike locates the
/// real row (mirrors the chain store's `probe_row`), letting the dirty-leg pass
/// find one strike's row without scanning the whole chain.
#[must_use]
fn probe_row(strike: Positive) -> OptionData {
    OptionData {
        strike_price: strike,
        ..Default::default()
    }
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

    /// Compute with no per-leg freshness signal (every quote inverts as fresh).
    #[track_caller]
    fn compute(chain: &OptionChain, ctx: &PricingInputs, sink: &mut GreeksSidecar) {
        compute_with(chain, ctx, &QuoteClocks::new(), sink);
    }

    /// Compute with an explicit set of per-leg quote receipt clocks.
    #[track_caller]
    fn compute_with(
        chain: &OptionChain,
        ctx: &PricingInputs,
        quotes: &QuoteClocks,
        sink: &mut GreeksSidecar,
    ) {
        match compute_leg_greeks(chain, ctx, quotes, sink) {
            Ok(()) => {}
            Err(e) => panic!("compute_leg_greeks failed: {e}"),
        }
    }

    /// A `QuoteClocks` with one leg's stream quote received at `received` seconds.
    fn clocks_with(strike: f64, style: OptionStyle, received: i64) -> QuoteClocks {
        let mut clocks = QuoteClocks::new();
        clocks.insert(key(strike, style), utc(received));
        clocks
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

    // --- Quote freshness gates the local IV inversion ------------------------

    #[test]
    fn test_compute_leg_greeks_stale_quote_is_not_inverted() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        // The call's stream quote arrived 10 s before `as_of` -> stale (> 5 s), so
        // its two-sided quote must NOT be inverted into a `Computed` IV.
        let clocks = clocks_with(60_000.0, OptionStyle::Call, AS_OF_BEFORE - 10);
        let mut sink = GreeksSidecar::new();
        compute_with(&chain, &ctx, &clocks, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::StaleQuote);
        // No local IV was inverted and every local Greek stays cleared.
        assert!(call.iv.is_none());
        assert!(call.delta.is_none());
        assert!(call.gamma.is_none());
        assert!(call.theta.is_none());
        assert!(call.vega.is_none());
        assert!(call.rho.is_none());
        // The put carries no receipt clock, so it inverts as before.
        let put = leg(&sink, 60_000.0, OptionStyle::Put);
        assert_eq!(put.status, LegStatus::Computed);
        assert!(put.iv.is_some());
        assert_eq!(put.iv_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_compute_leg_greeks_fresh_quote_is_still_inverted() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        // Received 4 s before `as_of` -> fresh (< 5 s): inverted as usual.
        let clocks = clocks_with(60_000.0, OptionStyle::Call, AS_OF_BEFORE - 4);
        let mut sink = GreeksSidecar::new();
        compute_with(&chain, &ctx, &clocks, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.status, LegStatus::Computed);
        assert!(call.iv.is_some());
        assert_eq!(call.iv_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_compute_leg_greeks_stale_quote_threshold_boundary() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);

        // Exactly at the 5 s threshold is still fresh (`> threshold` is stale).
        let at_threshold = clocks_with(60_000.0, OptionStyle::Call, AS_OF_BEFORE - 5);
        let mut sink_at = GreeksSidecar::new();
        compute_with(&chain, &ctx, &at_threshold, &mut sink_at);
        assert_eq!(
            leg(&sink_at, 60_000.0, OptionStyle::Call).status,
            LegStatus::Computed
        );

        // One second past the threshold is stale.
        let past_threshold = clocks_with(60_000.0, OptionStyle::Call, AS_OF_BEFORE - 6);
        let mut sink_past = GreeksSidecar::new();
        compute_with(&chain, &ctx, &past_threshold, &mut sink_past);
        assert_eq!(
            leg(&sink_past, 60_000.0, OptionStyle::Call).status,
            LegStatus::StaleQuote
        );
    }

    #[test]
    fn test_compute_leg_greeks_stale_quote_does_not_touch_venue_iv_path() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        let mut sink = GreeksSidecar::new();
        // A venue IV is present, so the venue-IV path wins regardless of how stale
        // the leg's own quote clock is — freshness gates only the local inversion.
        sink.apply_venue_greeks(&venue_greeks(60_000.0, OptionStyle::Call, Some(0.55), None));
        let clocks = clocks_with(60_000.0, OptionStyle::Call, AS_OF_BEFORE - 3_600);
        compute_with(&chain, &ctx, &clocks, &mut sink);

        let call = leg(&sink, 60_000.0, OptionStyle::Call);
        assert_eq!(call.iv, Some(pos(0.55)));
        assert_eq!(call.iv_origin, GreeksOrigin::Provider);
        assert_eq!(call.status, LegStatus::Computed);
        assert!(call.theta.is_some());
    }

    #[test]
    fn test_compute_leg_greeks_stale_quote_decision_is_deterministic() {
        let chain = atm_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        let clocks = clocks_with(60_000.0, OptionStyle::Call, AS_OF_BEFORE - 10);
        let mut a = GreeksSidecar::new();
        let mut b = GreeksSidecar::new();
        compute_with(&chain, &ctx, &clocks, &mut a);
        compute_with(&chain, &ctx, &clocks, &mut b);
        // The freshness decision is a pure timestamp comparison: identical inputs
        // give bit-identical leg entries.
        assert_eq!(
            leg(&a, 60_000.0, OptionStyle::Call),
            leg(&b, 60_000.0, OptionStyle::Call)
        );
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

    // --- Dirty-leg recompute (the O(changed legs) hot path) ------------------

    /// A two-strike chain — the fixture for the dirty-leg scoping tests.
    fn two_strike_chain() -> OptionChain {
        chain_of(&[
            od(
                60_000.0,
                Some(3_000.0),
                Some(3_100.0),
                Some(2_000.0),
                Some(2_100.0),
            ),
            od(
                61_000.0,
                Some(2_600.0),
                Some(2_700.0),
                Some(2_400.0),
                Some(2_500.0),
            ),
        ])
    }

    /// Recompute exactly the named dirty legs with no per-leg freshness signal.
    #[track_caller]
    fn compute_dirty(
        chain: &OptionChain,
        ctx: &PricingInputs,
        dirty: &[(Positive, OptionStyle)],
        sink: &mut GreeksSidecar,
    ) {
        match compute_dirty_legs(chain, ctx, &QuoteClocks::new(), dirty, sink) {
            Ok(()) => {}
            Err(e) => panic!("compute_dirty_legs failed: {e}"),
        }
    }

    #[test]
    fn test_compute_dirty_legs_reprices_only_the_named_legs() {
        let chain = two_strike_chain();
        // Seed every leg with a full pass at generation 1.
        let mut sink = GreeksSidecar::new();
        compute(&chain, &inputs(AS_OF_BEFORE, 1), &mut sink);
        let k1_call_before = leg(&sink, 60_000.0, OptionStyle::Call);
        let k1_put_before = leg(&sink, 60_000.0, OptionStyle::Put);
        let k2_call_before = leg(&sink, 61_000.0, OptionStyle::Call);

        // A dirty pass at a DIFFERENT as-of (longer time to expiry) for one leg only.
        // The named leg must change; the others must be byte-identical.
        compute_dirty(
            &chain,
            &inputs(AS_OF_BEFORE - 10_000_000, 2),
            &[(pos(60_000.0), OptionStyle::Call)],
            &mut sink,
        );
        assert_ne!(leg(&sink, 60_000.0, OptionStyle::Call), k1_call_before);
        assert_eq!(leg(&sink, 60_000.0, OptionStyle::Put), k1_put_before);
        assert_eq!(leg(&sink, 61_000.0, OptionStyle::Call), k2_call_before);
        // The generation is recorded so a later full pass at it is a cache no-op.
        assert_eq!(sink.computed_generation(), Some(2));
    }

    #[test]
    fn test_compute_dirty_legs_matches_full_pass_for_the_named_leg() {
        let chain = two_strike_chain();
        let ctx = inputs(AS_OF_BEFORE, 1);
        // The full pass prices every leg.
        let mut full = GreeksSidecar::new();
        compute(&chain, &ctx, &mut full);
        // A dirty pass on a FRESH sink prices only the named leg — and must produce
        // a bit-identical entry to the full pass for that leg.
        let mut dirty = GreeksSidecar::new();
        compute_dirty(
            &chain,
            &ctx,
            &[(pos(60_000.0), OptionStyle::Call)],
            &mut dirty,
        );
        assert_eq!(
            dirty.get(&key(60_000.0, OptionStyle::Call)),
            full.get(&key(60_000.0, OptionStyle::Call))
        );
        // The unnamed leg was never priced on the dirty sink.
        assert_eq!(dirty.get(&key(61_000.0, OptionStyle::Call)), None);
    }

    #[test]
    fn test_compute_dirty_legs_absent_strike_is_skipped() {
        let chain = two_strike_chain();
        let mut sink = GreeksSidecar::new();
        // A dirty key whose strike is not in the chain prices nothing and does not
        // panic, but still records the handled generation.
        compute_dirty(
            &chain,
            &inputs(AS_OF_BEFORE, 3),
            &[(pos(99_999.0), OptionStyle::Call)],
            &mut sink,
        );
        assert_eq!(sink.get(&key(99_999.0, OptionStyle::Call)), None);
        assert!(sink.is_empty());
        assert_eq!(sink.computed_generation(), Some(3));
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

                let clocks = QuoteClocks::new();
                let mut a = GreeksSidecar::new();
                let mut b = GreeksSidecar::new();
                match (
                    compute_leg_greeks(&chain, &ctx, &clocks, &mut a),
                    compute_leg_greeks(&chain, &ctx, &clocks, &mut b),
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
