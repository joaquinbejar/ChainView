//! The chain-matrix screen (strikes × call/put: bid/ask/mark/IV/Greeks)
//! (`docs/05-views-and-ux.md` §2.1, §6, §8; `docs/01-domain-model.md` §8).
//!
//! # States first, then the happy path
//!
//! [`draw`] renders the **loading**, **empty**, and provider-**error** states
//! before the populated matrix (the ChainView states-first agent-workflow rule,
//! `CLAUDE.md`), driven off [`LiveState::load`](crate::LiveState) and the store's
//! emptiness/health per the `docs/05` §2.1 prerequisite/recovery matrix:
//!
//! - [`ScreenLoad::Loading`] → a centered tick-driven spinner + "connecting to
//!   `<provider>`…".
//! - [`ScreenLoad::Ready`] with an empty chain → "no data for `<underlying>
//!   <expiry>`" + a hint.
//! - [`ScreenLoad::Error`] → the actionable message + the `r` reconnect
//!   affordance.
//!
//! A live feed that drops does **not** blank the screen: the last chain renders
//! **dimmed** with a `◐ stale` / `↻ reconnecting (n)` badge
//! ([`health_span`](crate::ui::theme::health_span)).
//!
//! # The draw path is pure
//!
//! [`draw`] takes `&LiveState` (never `&mut`) plus the `Copy` resolved [`Theme`]
//! and tick counter, and projects [`ChainRow`]/[`LegView`] view models **at draw
//! time**, borrowed from the store's [`OptionChain`] — no computation, no pricing,
//! no `GraphData`, no I/O, no state mutation (`docs/02-tui-architecture.md` §7).
//! View models are the only place display formatting happens; the domain stays
//! numeric (`docs/01-domain-model.md` §8).
//!
//! # Projection is honest: per-field precedence, `—` never a fabricated `0`
//!
//! Each Greek is resolved by the per-field precedence of `docs/01-domain-model.md`
//! §7 ([`resolve_leg`]): `delta` prefers the venue per-leg value and falls back to
//! the local sidecar; `gamma` comes from the style-keyed [`LegGreeks`]
//! (venue-or-local per its origin); `theta`/`vega` are always locally computed.
//! `iv` has a three-level precedence ([`resolve_iv`]): a per-style **venue** IV from
//! the sidecar, then — for the **call leg only** — the shared
//! [`OptionData::implied_volatility`] (§7 documents this shared field as the
//! call-side value, so the **put leg gets no shared fallback**, avoiding a call/put
//! IV collision), then a **locally computed** sidecar IV. A field is `Some` only
//! when a real value resolved — projection never invents one, and a missing value
//! renders `—` (an em dash), never a fabricated `0` (`docs/01-domain-model.md` §5,
//! §7, §8).
//!
//! Two honesty guards on IV (both documented at [`project_iv`]/[`project_local_iv`]):
//! an IV of **exactly zero** is the venue's absent-IV sentinel (the upstream
//! `OptionChain::add_option` takes a **non-`Option`** IV, so an absent IV defaults
//! to `Positive::ZERO`), so a bare `0` IV projects to `None` and renders `—`; and a
//! **locally computed** IV below [`MIN_PLAUSIBLE_LOCAL_IV`] (0.5%) is economically
//! implausible for a live quote — the same reasoning as the exact-zero sentinel — so
//! it is cleared to `—` rather than painting a fabricated-looking near-zero
//! percentage. A **venue** (`Provider`) IV is trusted and never floored.
//!
//! The origin glyph (`~`) badges the **actual computed cell** — an `iv`/`gamma`/
//! `theta`/`vega`/`delta` value whose resolved origin is
//! [`GreeksOrigin::ComputedLocally`] — never the trustworthy venue field beside it,
//! so a mixed-origin row (venue delta + local theta) badges the local theta, not the
//! delta. The row-level [`greeks_origin`](LegView::greeks_origin) still rolls up to
//! [`GreeksOrigin::ComputedLocally`] whenever any present field is local.
//!
//! # Color is never the only signal
//!
//! The shared strike column shades by the `K/S` [`StrikeRelation`] bucket (not an
//! ITM/OTM label) and carries the `◀ATM` marker on the nearest listed strike; the
//! bid/ask cells carry a `▲`/`▼`/`·` tick-direction glyph; the stale badge pairs a
//! glyph with text — all legible under `NO_COLOR`
//! (`docs/05-views-and-ux.md` §7, `CLAUDE.md` accessibility policy).

use chrono::{DateTime, Datelike, Utc};
use crossterm::event::KeyEvent;
use optionstratlib::OptionStyle;
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use crate::app::keymap::{ChainAction, KeyChord, resolve_chain};
use crate::app::{LegFocus, LiveState, ScreenLoad};
use crate::chain::{ChainStore, GreeksOrigin, InstrumentKey, LegGreeks, StreamHealth, TickDir};
use crate::event::AppEvent;
use crate::ui::theme::{
    GreekColumn, GreekColumns, StrikeRelation, Theme, greek_columns_for_slots, health_span,
    sanitize, spinner_frame, strike_relation_marker_span, tick_dir_span,
};

// ===========================================================================
// View models — projected at draw time, borrowed from the store, never owned.
// ===========================================================================

/// One option leg (a call or a put) at one strike, projected from an
/// [`OptionData`] and the store's style-keyed analytics sidecar at draw time
/// (`docs/01-domain-model.md` §7, §8).
///
/// Each analytic field is resolved by the per-field precedence of §7 (`resolve_leg`):
/// `delta` prefers the venue value (`OptionData::delta_call` / `delta_put`) and
/// falls back to the local sidecar; `gamma` comes from the style-keyed [`LegGreeks`]
/// (venue-or-local per its origin); `iv` follows the three-level `resolve_iv`
/// precedence (per-style venue → the call-only shared `OptionData::implied_volatility`
/// → locally computed); `theta`/`vega` are always locally computed. A field is `Some`
/// only when a real value resolved — projection never invents one, and a `None` field
/// renders `—`, never a fabricated `0` (`project_iv`/`project_local_iv` also clear the
/// venue's absent-IV zero sentinel and a sub-plausibility local IV to `None`).
///
/// Each resolvable-from-venue-or-local field carries its resolved [`GreeksOrigin`]
/// so the origin glyph badges the **actual computed cell**, not a trustworthy venue
/// cell beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegView {
    /// Best bid, or `None` when the feed omits it (renders `—`).
    pub bid: Option<Positive>,
    /// Best ask, or `None` when the feed omits it (renders `—`).
    pub ask: Option<Positive>,
    /// Mid/mark price, or `None` when a side is missing (renders `—`).
    pub mark: Option<Positive>,
    /// Implied volatility resolved by `resolve_iv`, or `None` when unavailable, the
    /// venue's absent-IV zero sentinel (`project_iv`), **or** a sub-plausibility
    /// locally-computed value (`project_local_iv`) — renders `—`, never `0.00%`.
    pub iv: Option<Positive>,
    /// Where the resolved `iv` came from: [`GreeksOrigin::Provider`] for a per-style
    /// venue IV or the shared call-side field, [`GreeksOrigin::ComputedLocally`] for a
    /// local inversion. Drives the origin glyph on the IV cell (only when `iv` is
    /// `Some` and local).
    pub iv_origin: GreeksOrigin,
    /// Delta — the venue per-leg value when present, else the local sidecar
    /// fallback; `None` when neither resolved (renders `—`).
    pub delta: Option<Decimal>,
    /// Where the resolved `delta` came from: [`GreeksOrigin::Provider`] for the venue
    /// per-leg value, [`GreeksOrigin::ComputedLocally`] for the local fallback. Drives
    /// the origin glyph on the delta cell (only when `delta` is `Some` and local).
    pub delta_origin: GreeksOrigin,
    /// Gamma from the style-keyed sidecar (venue-or-local), or `None` (renders `—`).
    pub gamma: Option<Decimal>,
    /// Where the resolved `gamma` came from (venue-or-local per its sidecar origin).
    /// Drives the origin glyph on the gamma cell (only when `gamma` is `Some` and
    /// local).
    pub gamma_origin: GreeksOrigin,
    /// Theta from the style-keyed sidecar — always locally computed; `None` until
    /// the local fill runs (renders `—`). Badged with the origin glyph whenever
    /// present.
    pub theta: Option<Decimal>,
    /// Vega from the style-keyed sidecar — always locally computed; `None` until
    /// the local fill runs (renders `—`). Badged with the origin glyph whenever
    /// present.
    pub vega: Option<Decimal>,
    /// Where this leg's rendered Greeks came from, rolled up across the resolved
    /// fields: [`GreeksOrigin::ComputedLocally`] when **any** present field is
    /// locally computed (so a mixed-origin row — venue delta + local vega — is
    /// honestly labelled), else [`GreeksOrigin::Provider`]. The per-cell origin
    /// glyph is driven by the per-field origins above, not by this rollup.
    pub greeks_origin: GreeksOrigin,
    /// The decayed last-tick direction of the bid, read from the store's retained
    /// baseline (`▲`/`▼`/`·`), cleared to `Flat` when the feed goes stale.
    pub bid_dir: TickDir,
    /// The decayed last-tick direction of the ask.
    pub ask_dir: TickDir,
}

impl LegView {
    /// Whether the given greek column's **present** resolved value is a
    /// locally-computed one — the per-cell origin-glyph predicate. `theta`/`vega`
    /// are always [`GreeksOrigin::ComputedLocally`], so they badge whenever present;
    /// `delta`/`gamma` badge only when their resolved origin is local. A `None` field
    /// is never local (an em dash is never badged).
    #[must_use]
    fn greek_is_local(&self, greek: GreekColumn) -> bool {
        match greek {
            GreekColumn::Delta => {
                self.delta.is_some() && matches!(self.delta_origin, GreeksOrigin::ComputedLocally)
            }
            GreekColumn::Gamma => {
                self.gamma.is_some() && matches!(self.gamma_origin, GreeksOrigin::ComputedLocally)
            }
            GreekColumn::Theta => self.theta.is_some(),
            GreekColumn::Vega => self.vega.is_some(),
        }
    }

    /// Whether the **present** resolved `iv` is locally computed — the IV cell's
    /// origin-glyph predicate. A venue/shared (`Provider`) IV and a `None` IV are
    /// never badged.
    #[must_use]
    fn iv_is_local(&self) -> bool {
        self.iv.is_some() && matches!(self.iv_origin, GreeksOrigin::ComputedLocally)
    }
}

/// One strike row of the chain matrix — the call and put legs plus the shared,
/// option-style-**independent** `K/S` relation that shades the strike column
/// (`docs/01-domain-model.md` §8).
///
/// [`strike_relation`](ChainRow::strike_relation) is deliberately **not** an
/// ITM/OTM label: a call and a put at one strike have opposite ITM/OTM status, so
/// no single label on a shared strike row can be truthful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainRow {
    /// The strike price.
    pub strike: Positive,
    /// The call leg at this strike.
    pub call: LegView,
    /// The put leg at this strike.
    pub put: LegView,
    /// Where the strike sits relative to spot (`K/S`) — shades the strike column.
    pub strike_relation: StrikeRelation,
}

/// Project the **call** leg from an [`OptionData`] and its style-keyed
/// [`LegGreeks`], resolving each Greek by the §7 precedence ([`resolve_leg`]). The
/// venue delta is [`OptionData::delta_call`]; `gamma`/`theta`/`vega` come from the
/// sidecar. The shared [`OptionData::implied_volatility`] is threaded in as the
/// call-leg IV fallback (§7 documents the shared field as the **call-side** value),
/// so a seed-only snapshot with no per-style venue IV yet still shows the honest
/// venue IV rather than a locally-computed near-zero.
#[must_use]
fn project_call(
    od: &OptionData,
    leg: Option<&LegGreeks>,
    bid_dir: TickDir,
    ask_dir: TickDir,
) -> LegView {
    resolve_leg(
        od.call_bid,
        od.call_ask,
        od.call_middle,
        od.delta_call,
        Some(od.implied_volatility),
        leg,
        (bid_dir, ask_dir),
    )
}

/// Project the **put** leg from an [`OptionData`] and its style-keyed
/// [`LegGreeks`]. The venue delta is [`OptionData::delta_put`]; `iv`/`gamma` are the
/// **put** sidecar entry — split per style, so an unequal call/put iv/gamma both
/// survive (`docs/01-domain-model.md` §7), unlike the shared upstream fields.
///
/// The put passes **no** shared-IV fallback: the shared [`OptionData::implied_volatility`]
/// is the call-side value (§7), so handing it to the put would reintroduce the exact
/// call/put IV collision the style-keyed sidecar exists to prevent.
#[must_use]
fn project_put(
    od: &OptionData,
    leg: Option<&LegGreeks>,
    bid_dir: TickDir,
    ask_dir: TickDir,
) -> LegView {
    resolve_leg(
        od.put_bid,
        od.put_ask,
        od.put_middle,
        od.delta_put,
        None,
        leg,
        (bid_dir, ask_dir),
    )
}

/// Resolve one leg's [`LegView`] by the per-field §7 precedence — a **pure** read
/// over the venue row fields and the style-keyed sidecar [`LegGreeks`], inventing
/// nothing and pricing nothing:
///
/// - **delta**: the venue per-leg value (origin `Provider`) when present, else the
///   local sidecar `delta` (origin `ComputedLocally`).
/// - **iv**: the three-level [`resolve_iv`] precedence — a per-style venue sidecar
///   IV, then the call-only `shared_iv` fallback, then a floored local inversion.
/// - **gamma**: the sidecar `gamma` with its origin (venue-or-local per style).
/// - **theta/vega**: the sidecar values — always locally computed.
///
/// `shared_iv` is the shared [`OptionData::implied_volatility`] for the **call** leg
/// and `None` for the put (`project_call`/`project_put`), because that shared field
/// is the call-side value (§7).
///
/// [`greeks_origin`](LegView::greeks_origin) rolls up to
/// [`GreeksOrigin::ComputedLocally`] when any resolved, present field is local. Each
/// field also carries its own resolved [`GreeksOrigin`], so the per-cell origin glyph
/// badges the actual computed cell. A field that resolves to `None` renders `—`.
#[must_use]
fn resolve_leg(
    bid: Option<Positive>,
    ask: Option<Positive>,
    mark: Option<Positive>,
    venue_delta: Option<Decimal>,
    shared_iv: Option<Positive>,
    leg: Option<&LegGreeks>,
    dirs: (TickDir, TickDir),
) -> LegView {
    let (bid_dir, ask_dir) = dirs;
    // delta: venue first, else the local sidecar fallback.
    let (delta, delta_origin) = match venue_delta {
        Some(value) => (Some(value), GreeksOrigin::Provider),
        None => match leg.and_then(|g| g.delta) {
            Some(value) => (Some(value), GreeksOrigin::ComputedLocally),
            None => (None, GreeksOrigin::Provider),
        },
    };
    // iv: the three-level precedence (sidecar-venue → call-only shared → local floored).
    let (iv, iv_origin) = resolve_iv(shared_iv, leg);
    // gamma: the style-keyed sidecar, venue-or-local per its origin.
    let (gamma, gamma_origin) = match leg.and_then(|g| g.gamma.map(|value| (value, g.gamma_origin)))
    {
        Some((value, origin)) => (Some(value), origin),
        None => (None, GreeksOrigin::Provider),
    };
    // theta / vega: always locally computed when present.
    let theta = leg.and_then(|g| g.theta);
    let vega = leg.and_then(|g| g.vega);
    // Roll the origin up: any present, locally-computed field labels the row local.
    let delta_local = delta.is_some() && matches!(delta_origin, GreeksOrigin::ComputedLocally);
    let iv_local = iv.is_some() && matches!(iv_origin, GreeksOrigin::ComputedLocally);
    let gamma_local = gamma.is_some() && matches!(gamma_origin, GreeksOrigin::ComputedLocally);
    let any_local = delta_local || iv_local || gamma_local || theta.is_some() || vega.is_some();
    let greeks_origin = if any_local {
        GreeksOrigin::ComputedLocally
    } else {
        GreeksOrigin::Provider
    };
    LegView {
        bid,
        ask,
        mark,
        iv,
        iv_origin,
        delta,
        delta_origin,
        gamma,
        gamma_origin,
        theta,
        vega,
        greeks_origin,
        bid_dir,
        ask_dir,
    }
}

/// Resolve one leg's IV by the three-level §7 precedence, returning the value and its
/// resolved [`GreeksOrigin`] (a **pure** read — no pricing):
///
/// 1. **per-style venue IV** from the sidecar ([`LegGreeks::iv`] with origin
///    `Provider`), routed through [`project_iv`] (only the exact-zero absent sentinel
///    clears a venue IV — it is never floored);
/// 2. **`shared_iv`** — the shared [`OptionData::implied_volatility`] the **call** leg
///    passes and the put passes `None` (§7 call-side field), origin `Provider`;
/// 3. **locally computed** sidecar IV ([`LegGreeks::iv`] with origin
///    `ComputedLocally`), routed through [`project_local_iv`] so a sub-plausibility
///    near-zero degrades to `None`, origin `ComputedLocally`.
///
/// The sidecar IV carries exactly one origin, so levels 1 and 3 are mutually
/// exclusive; the shared fallback sits between them, above the floored local value.
#[must_use]
fn resolve_iv(
    shared_iv: Option<Positive>,
    leg: Option<&LegGreeks>,
) -> (Option<Positive>, GreeksOrigin) {
    if let Some((value, origin)) = leg.and_then(|g| g.iv.map(|v| (v, g.iv_origin))) {
        match origin {
            // A per-style VENUE IV wins outright (only the absent-zero sentinel clears).
            GreeksOrigin::Provider => {
                if let Some(iv) = project_iv(value) {
                    return (Some(iv), GreeksOrigin::Provider);
                }
                // exact-zero venue sentinel: fall through to the shared field.
            }
            // A LOCAL sidecar IV ranks below the shared venue fallback (call only) and
            // is subject to the plausibility floor.
            GreeksOrigin::ComputedLocally => {
                if let Some(iv) = shared_iv.and_then(project_iv) {
                    return (Some(iv), GreeksOrigin::Provider);
                }
                return (project_local_iv(value), GreeksOrigin::ComputedLocally);
            }
        }
    }
    // No usable sidecar IV: the call-only shared field (the put passes `None`).
    if let Some(iv) = shared_iv.and_then(project_iv) {
        return (Some(iv), GreeksOrigin::Provider);
    }
    (None, GreeksOrigin::Provider)
}

/// The smallest implied volatility, **as a fraction** (`0.005` = 0.5%; IV is stored
/// as a fraction, so `0.4922` renders `49.22%`), that a **locally computed** IV must
/// reach to be plausible enough to display.
///
/// A live listed option quoting real premium with a sub-0.5% IV is economically
/// implausible — the same reasoning as the exact-zero absent-IV sentinel
/// ([`project_iv`]): such a near-zero value is almost always a mispriced/garbage
/// local inversion (e.g. a Deribit inverse, BTC-settled contract whose premium is
/// priced as USD), not a real quote. A locally-computed IV below this floor is
/// cleared to `—` rather than painted as a fabricated-looking near-zero percentage.
/// A **venue** (`Provider`) IV is trusted and never floored — the exact-zero
/// sentinel already handles a venue absent-zero.
const MIN_PLAUSIBLE_LOCAL_IV: Decimal = Decimal::from_parts(5, 0, 0, false, 3);

/// Project the non-`Option` [`OptionData::implied_volatility`] into a
/// `LegView.iv` **honestly** (the "Absent-IV vs 0% IV" decision from #15).
///
/// The upstream `OptionChain::add_option` takes a **non-`Option`** `Positive` IV,
/// so the Deribit adapter defaults an absent IV to `Positive::ZERO` — a row that
/// carries `0` cannot distinguish "venue sent no IV" from a genuine "IV = 0". A
/// listed option that is being quoted always has a strictly positive IV (a zero IV
/// prices the option at pure intrinsic, economically implausible for a live
/// quote), so a bare `0` is the absent-sentinel, **not** a real quote. This
/// projects it to `None` — the matrix renders `—`, exactly the honesty the Greeks
/// columns use — so a fabricated-looking `0.00%` never renders as a live IV. A
/// strictly positive IV projects to `Some(iv)`.
///
/// This guard clears the exact-zero **venue** sentinel; [`project_local_iv`] adds the
/// stronger sub-plausibility floor that applies to **locally computed** IVs only.
#[must_use]
fn project_iv(iv: Positive) -> Option<Positive> {
    if iv.is_zero() { None } else { Some(iv) }
}

/// Project a **locally computed** IV honestly: clear both the venue absent-zero
/// sentinel ([`project_iv`]) **and** any value below the plausibility floor
/// [`MIN_PLAUSIBLE_LOCAL_IV`] to `None`, so a mispriced near-zero local inversion
/// degrades to `—` instead of painting a fake percentage. A value at or above the
/// floor (e.g. a legitimate IG-equities local IV, always ≫ 0.5%) projects to
/// `Some(iv)`. This applies to `ComputedLocally`-origin IVs only — a venue IV is
/// trusted via [`project_iv`] and never floored.
#[must_use]
fn project_local_iv(iv: Positive) -> Option<Positive> {
    match project_iv(iv) {
        Some(value) if value.to_dec() >= MIN_PLAUSIBLE_LOCAL_IV => Some(value),
        _ => None,
    }
}

/// Project one strike row: both legs plus the shared `K/S` relation.
///
/// The direction indicators are read from the store's retained/decayed baseline as
/// of `as_of` — the tick-stamped wall clock threaded in from [`App::now`], so a
/// marker decays on wall-time while `draw` itself reads no wall clock; `None` (no
/// reference instant) yields `Flat`. The per-leg Greeks come from the store's
/// cached style-keyed sidecar ([`ChainStore::leg_greeks`]) — a read, never a
/// recompute. Building the per-leg [`InstrumentKey`] clones the (short)
/// underlying ticker, which is why projection runs for the **visible** rows only.
///
/// [`App::now`]: crate::app::App::now
#[must_use]
fn project_row(
    od: &OptionData,
    spot: Positive,
    store: &ChainStore,
    underlying: &str,
    expiration: DateTime<Utc>,
    as_of: Option<DateTime<Utc>>,
) -> ChainRow {
    let strike = od.strike_price;
    let call_key = leg_key(underlying, expiration, strike, OptionStyle::Call);
    let put_key = leg_key(underlying, expiration, strike, OptionStyle::Put);
    let (call_bid_dir, call_ask_dir) = leg_dirs(store, &call_key, as_of);
    let (put_bid_dir, put_ask_dir) = leg_dirs(store, &put_key, as_of);
    ChainRow {
        strike,
        call: project_call(od, store.leg_greeks(&call_key), call_bid_dir, call_ask_dir),
        put: project_put(od, store.leg_greeks(&put_key), put_bid_dir, put_ask_dir),
        strike_relation: StrikeRelation::classify(strike, spot),
    }
}

/// The store key for one `(underlying, expiry, strike, style)` leg — the read key
/// for both the direction baseline and the analytics sidecar.
#[must_use]
fn leg_key(
    underlying: &str,
    expiration: DateTime<Utc>,
    strike: Positive,
    style: OptionStyle,
) -> InstrumentKey {
    InstrumentKey {
        underlying: underlying.to_owned(),
        expiration_utc: expiration,
        strike,
        style,
    }
}

/// The `(bid_dir, ask_dir)` for one leg as of `as_of`, read from the store's
/// decayed baseline. Both are `Flat` when there is no reference instant.
#[must_use]
fn leg_dirs(
    store: &ChainStore,
    key: &InstrumentKey,
    as_of: Option<DateTime<Utc>>,
) -> (TickDir, TickDir) {
    let Some(now) = as_of else {
        return (TickDir::Flat, TickDir::Flat);
    };
    (store.bid_dir(key, now), store.ask_dir(key, now))
}

// ===========================================================================
// The draw entry point + the loading / empty / error states (states first).
// ===========================================================================

/// Draw the chain matrix for the live `state` into `area` — a pure render
/// (`docs/02-tui-architecture.md` §7). The empty/loading/error states render
/// before the populated matrix (the states-first rule); the store is borrowed,
/// never recomputed. `theme` (resolved, `NO_COLOR`-aware), `tick` (for the loading
/// spinner), and `now` (the tick-stamped wall clock the tick-direction markers
/// decay against) are all `Copy`, so purity holds — `draw` reads `now`, never a
/// wall clock.
pub fn draw(
    state: &LiveState,
    frame: &mut Frame,
    area: Rect,
    theme: Theme,
    tick: u64,
    now: DateTime<Utc>,
) {
    let chain = state.store.chain();
    match &state.load {
        ScreenLoad::Loading => {
            // A consistent two-line body (primary + secondary hint), matching the
            // empty/error states, vertically centered so it reads as a deliberate
            // state rather than content that failed to fill.
            draw_state_body(
                frame,
                area,
                theme,
                Text::from(vec![
                    Line::from(Span::styled(
                        format!(
                            "{} connecting to {}…",
                            spinner_frame(tick),
                            sanitize(state.source.provider.as_str())
                        ),
                        theme.accent(),
                    )),
                    Line::from(Span::styled("waiting for the first chain", theme.dim())),
                ]),
            );
        }
        ScreenLoad::Error { message } => {
            draw_state_body(
                frame,
                area,
                theme,
                Text::from(vec![
                    Line::from(Span::styled(
                        format!("! {}", sanitize(message)),
                        theme.warning(),
                    )),
                    Line::from(Span::styled("press r to reconnect", theme.dim())),
                ]),
            );
        }
        ScreenLoad::Ready => {
            if chain.options.is_empty() {
                draw_state_body(
                    frame,
                    area,
                    theme,
                    Text::from(vec![
                        Line::from(Span::styled(
                            format!(
                                "no data for {} {}",
                                sanitize(&chain.symbol),
                                sanitize(&chain.get_expiration_date())
                            ),
                            theme.dim(),
                        )),
                        Line::from(Span::styled(
                            "no strikes yet - press r to reconnect",
                            theme.dim(),
                        )),
                    ]),
                );
            } else {
                draw_matrix(state, frame, area, theme, now);
            }
        }
    }
}

/// Draw a state body (loading / empty / error) inside the framed "Chain" block,
/// **vertically centered** in the available height and horizontally centered — a
/// first-class, deliberate-looking state, never a blank void or a top-anchored
/// fragment. All three states share this two-line baseline.
fn draw_state_body(frame: &mut Frame, area: Rect, theme: Theme, text: Text<'static>) {
    let block = Block::bordered().title(Span::styled("Chain", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Reserve exactly the text height and center it in the body; `Flex::Center`
    // does the geometry, so there is no manual arithmetic (and no `saturating_*`).
    let height = u16::try_from(text.height())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

// ===========================================================================
// The populated matrix.
// ===========================================================================

/// The width (columns) of a numeric cell.
const NUM_W: u16 = 8;
/// The width (columns) of the shared center strike cell (fits `◀ATM`).
const STRIKE_W: u16 = 10;
/// Approximate width consumed by the always-present columns (10 numeric cells +
/// the strike cell + inter-column spacing) — the base of the greek-slot budget.
const BASE_W: u16 = 100;
/// Approximate width one optional greek **slot** costs (one numeric cell per
/// side + spacing).
const SLOT_W: u16 = 18;

/// Draw the populated strike × call/put matrix with ATM anchoring, the shaded
/// strike column, responsive greek columns, and the stale/reconnecting badge.
///
/// `now` is the tick-stamped wall clock the tick-direction markers decay against
/// (`docs/01-domain-model.md` §6); it is read here, never a wall clock.
fn draw_matrix(state: &LiveState, frame: &mut Frame, area: Rect, theme: Theme, now: DateTime<Utc>) {
    let store = &state.store;
    let chain = store.chain();
    let spot = chain.underlying_price;
    let health = store.health();
    let stale = !matches!(health, StreamHealth::Live);
    // The tick-direction indicators decay against the tick-stamped wall clock, so a
    // bid-up/ask-down marker fades ~3 s after its last change on wall-time — NOT
    // pinned to `last_full_poll`, which would freeze the marker until the next poll.
    let as_of = Some(now);

    // The underlying/expiry for the per-leg InstrumentKey come from the store's
    // canonical chain key (absolute UTC), not the display strings.
    let key = store.chain_key();
    let underlying = key.1.clone();
    let expiration = key.2;

    let block = Block::bordered().title(matrix_title(chain, expiration, health, theme));
    let inner = block.inner(area);

    // The mandatory column set (strike + bid/ask/mark + IV + Δ) needs BASE_W inner
    // cols; below that the table would clip into a corrupt chain, so show an honest
    // "widen the terminal" state instead (`docs/05-views-and-ux.md` §8). Greek
    // columns still drop responsively ABOVE this floor via `greek_slots_for_width`,
    // and Δ stays present in every rendered chain (the theme-layer invariant).
    if inner.width < BASE_W {
        draw_state_body(
            frame,
            area,
            theme,
            Text::from(vec![
                Line::from(Span::styled("chain needs a wider terminal", theme.dim())),
                Line::from(""),
                Line::from(Span::styled(
                    format!("widen to at least {} cols", BASE_W + 2),
                    theme.dim(),
                )),
            ]),
        );
        return;
    }

    // v0.2 column set: Δ (always) plus the optional Γ/ν/Θ that fit at this width,
    // dropped in the `Γ → ν → Θ` order the #14 `greek_columns_for_slots` primitive
    // fixes (Θ retained first, Γ last) — now that the style-keyed analytics sidecar
    // populates all of them per leg.
    let greek_cols = greek_columns_for_slots(greek_slots_for_width(inner.width));
    let plan = columns(greek_cols);
    let widths: Vec<Constraint> = plan.iter().map(|col| col_width(*col)).collect();
    // The body height is the inner height minus the two-row header (the Calls/Puts
    // super-header line plus the per-column label line).
    let visible = floor_sub(usize::from(inner.height), 2);

    let strikes: Vec<&OptionData> = chain.options.iter().collect();
    let len = strikes.len();
    // The ATM index is cached off-draw on `LiveState` (recomputed only on a poll),
    // so the per-frame cost stays O(visible rows), not O(full ladder).
    let atm = state.atm_index();
    let anchor = clamp_anchor(state.selection.focused_row, atm, len);
    // The explicit user cursor (clamped to the current chain), distinct from the
    // ATM anchor used for scrolling before any row is focused.
    let selected = state
        .selection
        .focused_row
        .and_then(|row| (row < len).then_some(row));
    let start = window_start(anchor.unwrap_or(0), visible, len);

    let header = header_row(&plan, theme);
    let mut rows: Vec<Row> = Vec::with_capacity(visible.min(len));
    for (idx, od) in strikes.iter().enumerate().skip(start).take(visible) {
        let od: &OptionData = od;
        let row = project_row(od, spot, store, &underlying, expiration, as_of);
        let is_atm = atm == Some(idx);
        let is_selected = selected == Some(idx);
        let cells: Vec<Cell> = plan
            .iter()
            .map(|col| {
                col_cell(
                    *col,
                    &row,
                    theme,
                    is_selected,
                    state.selection.focused_leg,
                    is_atm,
                )
            })
            .collect();
        let mut table_row = Row::new(cells);
        if is_selected {
            table_row = table_row.style(Style::new().add_modifier(Modifier::BOLD));
        }
        rows.push(table_row);
    }

    let mut table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1);
    // Never blank on a dropped stream: the last chain renders dimmed, the badge in
    // the title carries the honest state.
    if stale {
        table = table.style(theme.dim());
    }
    frame.render_widget(table, area);
}

/// The block title `<symbol>  exp <date>  spot <S>`, with the stream-health badge
/// appended when the feed is not live (`◐ stale` / `↻ reconnecting (n)`).
///
/// The venue-controlled `symbol` is sanitized at this render edge; the expiry is
/// formatted from the canonical [`DateTime<Utc>`] as a bare date (every listed
/// contract settles at 08:00 UTC, so the time/offset are noise), so it carries no
/// venue bytes.
#[must_use]
fn matrix_title(
    chain: &OptionChain,
    expiration: DateTime<Utc>,
    health: &StreamHealth,
    theme: Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(sanitize(&chain.symbol), theme.accent()),
        Span::raw(format!("  exp {}", fmt_expiry_date(expiration))),
        Span::raw(format!("  spot {}", fmt_strike(chain.underlying_price))),
    ];
    if !matches!(health, StreamHealth::Live) {
        spans.push(Span::raw("  "));
        spans.push(health_span(health, theme));
    }
    Line::from(spans)
}

/// The number of optional greek **slots** (0..=3) that fit at `width` — the budget
/// fed to the `Γ → ν → Θ` drop order (`greek_columns_for_slots`, `theme.rs`): `0`
/// keeps Δ only, `1` adds Θ, `2` adds ν, `3` adds Γ (Δ is always present). A rough,
/// truncation-safe estimate: below the budget the matrix keeps only Δ and the
/// price/IV columns and the [`Table`] clips gracefully.
#[must_use]
fn greek_slots_for_width(width: u16) -> usize {
    // `a - b` floored at zero; `max` guarantees the subtraction never underflows
    // (the ruleset bans `saturating_sub`, and `checked_sub(..).unwrap_or(0)` trips
    // clippy's manual-saturating lint).
    let over = width.max(BASE_W) - BASE_W;
    usize::from(over / SLOT_W).min(3)
}

/// `a - b` floored at zero, spelled so it can never underflow — the ruleset bans
/// `saturating_sub` and `checked_sub(..).unwrap_or(0)` trips clippy's
/// manual-saturating lint, so the floor is `a.max(b) - b`.
#[must_use]
fn floor_sub(a: usize, b: usize) -> usize {
    a.max(b) - b
}

/// The scroll anchor: the clamped user cursor if present, else the ATM index, else
/// row 0 — never an out-of-range index (`docs/02-tui-architecture.md`, `.get()` +
/// fallback discipline).
#[must_use]
fn clamp_anchor(focused: Option<usize>, atm: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match focused {
        Some(row) if row < len => Some(row),
        // A poll shrank the chain under the cursor: fall back to the last row.
        Some(_) => Some(floor_sub(len, 1)),
        None => atm.or(Some(0)),
    }
}

/// The first visible row index so `anchor` stays on screen, clamped to `[0, len -
/// visible]`. Uses checked arithmetic only (no `saturating_*`, per the ruleset).
#[must_use]
fn window_start(anchor: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let half = visible / 2;
    let ideal = floor_sub(anchor, half);
    let max_start = floor_sub(len, visible);
    ideal.min(max_start)
}

// ===========================================================================
// Column plan — one ordered list drives header, widths, and cells consistently.
// ===========================================================================

/// One column of the matrix. A single ordered [`columns`] list is derived from the
/// visible greek columns, and header labels, width constraints, and per-row cells
/// are all mapped from it, so they can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainCol {
    /// A call-side greek column.
    CallGreek(GreekColumn),
    /// Call implied volatility.
    CallIv,
    /// Call bid (carries a tick-direction glyph).
    CallBid,
    /// Call ask (carries a tick-direction glyph).
    CallAsk,
    /// Call mark/mid.
    CallMark,
    /// The shared center strike column (shaded by relation, `◀ATM` marker).
    Strike,
    /// Put bid (carries a tick-direction glyph).
    PutBid,
    /// Put ask (carries a tick-direction glyph).
    PutAsk,
    /// Put mark/mid.
    PutMark,
    /// Put implied volatility.
    PutIv,
    /// A put-side greek column.
    PutGreek(GreekColumn),
}

/// Build the ordered v0.2 column plan: the call side mirrors the put side around
/// the center strike
/// (`Δ [Γ] [ν] [Θ] IV Bid Ask Mark | Strike | Bid Ask Mark IV [Θ] [ν] [Γ] Δ`). Δ is
/// always present; the optional Γ/ν/Θ are included per `greeks` — the responsive
/// set the `Γ → ν → Θ` drop order yields (Θ retained first, Γ last). The optional
/// greeks sit between Δ and IV on the call side and mirror on the put side, so the
/// plan stays a clean mirror at every width.
#[must_use]
fn columns(greeks: GreekColumns) -> Vec<ChainCol> {
    let mut plan = Vec::new();
    // Call side: Δ, then the optional greeks (outer→inner: Γ, ν, Θ), then IV/prices.
    plan.push(ChainCol::CallGreek(GreekColumn::Delta));
    if greeks.gamma {
        plan.push(ChainCol::CallGreek(GreekColumn::Gamma));
    }
    if greeks.vega {
        plan.push(ChainCol::CallGreek(GreekColumn::Vega));
    }
    if greeks.theta {
        plan.push(ChainCol::CallGreek(GreekColumn::Theta));
    }
    plan.push(ChainCol::CallIv);
    plan.push(ChainCol::CallBid);
    plan.push(ChainCol::CallAsk);
    plan.push(ChainCol::CallMark);
    plan.push(ChainCol::Strike);
    plan.push(ChainCol::PutBid);
    plan.push(ChainCol::PutAsk);
    plan.push(ChainCol::PutMark);
    plan.push(ChainCol::PutIv);
    // Put greeks mirror the call side (inner→outer: Θ, ν, Γ, then Δ outermost).
    if greeks.theta {
        plan.push(ChainCol::PutGreek(GreekColumn::Theta));
    }
    if greeks.vega {
        plan.push(ChainCol::PutGreek(GreekColumn::Vega));
    }
    if greeks.gamma {
        plan.push(ChainCol::PutGreek(GreekColumn::Gamma));
    }
    plan.push(ChainCol::PutGreek(GreekColumn::Delta));
    plan
}

/// The width constraint for a column (fixed, so the header and rows align).
#[must_use]
fn col_width(col: ChainCol) -> Constraint {
    match col {
        ChainCol::Strike => Constraint::Length(STRIKE_W),
        _ => Constraint::Length(NUM_W),
    }
}

/// The header label for a column.
#[must_use]
fn col_header(col: ChainCol) -> &'static str {
    match col {
        ChainCol::CallGreek(greek) | ChainCol::PutGreek(greek) => greek_label(greek),
        ChainCol::CallIv | ChainCol::PutIv => "IV",
        ChainCol::CallBid | ChainCol::PutBid => "Bid",
        ChainCol::CallAsk | ChainCol::PutAsk => "Ask",
        ChainCol::CallMark | ChainCol::PutMark => "Mark",
        ChainCol::Strike => "Strike",
    }
}

/// Which leg a column belongs to (`None` for the shared strike column) — drives
/// the leg-focus emphasis on the selected row.
#[must_use]
fn col_side(col: ChainCol) -> Option<LegFocus> {
    match col {
        ChainCol::CallGreek(_)
        | ChainCol::CallIv
        | ChainCol::CallBid
        | ChainCol::CallAsk
        | ChainCol::CallMark => Some(LegFocus::Call),
        ChainCol::PutGreek(_)
        | ChainCol::PutIv
        | ChainCol::PutBid
        | ChainCol::PutAsk
        | ChainCol::PutMark => Some(LegFocus::Put),
        ChainCol::Strike => None,
    }
}

/// The single-glyph label for a greek column.
#[must_use]
fn greek_label(greek: GreekColumn) -> &'static str {
    match greek {
        GreekColumn::Delta => "Δ",
        GreekColumn::Gamma => "Γ",
        GreekColumn::Vega => "ν",
        GreekColumn::Theta => "Θ",
    }
}

/// The `Calls` / `Puts` super-header marker for a column — placed on the two
/// `Mark` columns that flank the center `Strike`, so the mirror halves are labeled
/// unambiguously (`Calls  Strike  Puts`) without relying on color. Every other
/// column has no super-header text.
#[must_use]
fn group_label(col: ChainCol) -> &'static str {
    match col {
        ChainCol::CallMark => "Calls",
        ChainCol::PutMark => "Puts",
        _ => "",
    }
}

/// The two-line header row: a `Calls` / `Puts` super-header line above the
/// per-column labels, so which mirror half is calls vs puts is explicit (not a
/// guess from position). Numeric labels are right-aligned, `Strike` centered; the
/// super-header markers are centered over the `Mark` columns flanking the strike.
#[must_use]
fn header_row(plan: &[ChainCol], theme: Theme) -> Row<'static> {
    let cells: Vec<Cell> = plan
        .iter()
        .map(|col| {
            let align = if matches!(col, ChainCol::Strike) {
                Alignment::Center
            } else {
                Alignment::Right
            };
            let group = Line::from(group_label(*col)).alignment(Alignment::Center);
            let label = Line::from(col_header(*col)).alignment(align);
            Cell::from(Text::from(vec![group, label]))
        })
        .collect();
    Row::new(cells).height(2).style(theme.accent())
}

/// The cell for one column of one row, with leg-focus emphasis (an underline on
/// the focused leg's cells) applied on the selected row.
#[must_use]
fn col_cell(
    col: ChainCol,
    row: &ChainRow,
    theme: Theme,
    is_selected: bool,
    focused_leg: LegFocus,
    is_atm: bool,
) -> Cell<'static> {
    let cell = match col {
        ChainCol::CallGreek(greek) => greek_cell(&row.call, greek, theme),
        ChainCol::PutGreek(greek) => greek_cell(&row.put, greek, theme),
        ChainCol::CallIv => origin_num_cell(fmt_iv(row.call.iv), row.call.iv_is_local(), theme),
        ChainCol::PutIv => origin_num_cell(fmt_iv(row.put.iv), row.put.iv_is_local(), theme),
        ChainCol::CallBid => dir_cell(row.call.bid, row.call.bid_dir, theme),
        ChainCol::CallAsk => dir_cell(row.call.ask, row.call.ask_dir, theme),
        ChainCol::CallMark => num_cell(fmt_price(row.call.mark)),
        ChainCol::PutBid => dir_cell(row.put.bid, row.put.bid_dir, theme),
        ChainCol::PutAsk => dir_cell(row.put.ask, row.put.ask_dir, theme),
        ChainCol::PutMark => num_cell(fmt_price(row.put.mark)),
        ChainCol::Strike => strike_cell(row, theme, is_atm),
    };
    // The focused leg is underlined on the selected row — an intensity signal, so
    // it survives NO_COLOR and never fights the tick-direction color.
    match col_side(col) {
        Some(side) if is_selected && side == focused_leg => {
            cell.style(Style::new().add_modifier(Modifier::UNDERLINED))
        }
        _ => cell,
    }
}

/// A greek cell; it carries a subtle `~` origin glyph when **this specific field**
/// is ChainView's local computation ([`LegView::greek_is_local`]) — so on a
/// mixed-origin row the glyph badges the actual computed field (e.g. a local theta),
/// never the trustworthy venue field beside it (e.g. a venue delta), and a leg with
/// a local field but a `None` delta is still badged on that field. The glyph is an
/// intensity/text marker, so it survives `NO_COLOR` (color is never the only signal).
#[must_use]
fn greek_cell(leg: &LegView, greek: GreekColumn, theme: Theme) -> Cell<'static> {
    let value = match greek {
        GreekColumn::Delta => leg.delta,
        GreekColumn::Gamma => leg.gamma,
        GreekColumn::Vega => leg.vega,
        GreekColumn::Theta => leg.theta,
    };
    origin_num_cell(fmt_greek(value), leg.greek_is_local(greek), theme)
}

/// A right-aligned numeric cell that carries a trailing `~` origin glyph when
/// `local` — the single place the origin marker is painted, shared by the Greek
/// cells and the IV cell so they badge consistently. A non-local (venue/shared or
/// absent) value renders as a plain [`num_cell`]. The glyph is a text marker legible
/// under `NO_COLOR`.
#[must_use]
fn origin_num_cell(text: String, local: bool, theme: Theme) -> Cell<'static> {
    if local {
        let line = Line::from(vec![Span::raw(text), Span::styled("~", theme.warning())])
            .alignment(Alignment::Right);
        Cell::from(line)
    } else {
        num_cell(text)
    }
}

/// A right-aligned numeric cell.
#[must_use]
fn num_cell(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).alignment(Alignment::Right))
}

/// A right-aligned price cell with a trailing tick-direction glyph (`▲`/`▼`/`·`) —
/// color-independent, so the direction reads under `NO_COLOR`.
///
/// A **missing** price carries no tick direction, so it renders just the em dash
/// (`—`) with no redundant trailing glyph.
#[must_use]
fn dir_cell(price: Option<Positive>, dir: TickDir, theme: Theme) -> Cell<'static> {
    if price.is_none() {
        return num_cell(fmt_price(price));
    }
    let line = Line::from(vec![
        Span::raw(fmt_price(price)),
        Span::raw(" "),
        tick_dir_span(dir, theme),
    ])
    .alignment(Alignment::Right);
    Cell::from(line)
}

/// The width (display columns) of the [`AT_SPOT_MARKER`](crate::ui::theme::AT_SPOT_MARKER)
/// `◀ATM`, reserved on every strike row so the ATM row does not left-shift its
/// digits out of the ladder.
const ATM_MARKER_W: usize = 4;

/// The shared center strike cell: the strike shaded by its `K/S` relation, with
/// the `◀ATM` marker on the nearest listed strike (both legible under `NO_COLOR`).
///
/// The marker's trailing width is reserved on **every** row (the marker, or an
/// equal-width blank), so the strike digits form a clean vertical ladder — the ATM
/// row no longer jogs the number left relative to the others.
#[must_use]
fn strike_cell(row: &ChainRow, theme: Theme, is_atm: bool) -> Cell<'static> {
    let mut spans = vec![
        Span::styled(
            fmt_strike(row.strike),
            theme.strike_relation_style(row.strike_relation),
        ),
        Span::raw(" "),
    ];
    if is_atm {
        spans.push(strike_relation_marker_span(StrikeRelation::AtSpot, theme));
    } else {
        spans.push(Span::raw(" ".repeat(ATM_MARKER_W)));
    }
    Cell::from(Line::from(spans).alignment(Alignment::Center))
}

// ===========================================================================
// Cell formatting — the ONE place display formatting happens; `—` never `0`.
// ===========================================================================

/// The em dash rendered for any value the provider did not supply — never a
/// fabricated `0` (`docs/01-domain-model.md` §5, §8).
const EM_DASH: &str = "—";

/// Format a price to two decimals, or `—` when absent. Guards the `Positive`
/// infinity sentinel so a non-finite value never paints (rule: guard `f64`
/// `NaN`/`Inf` before it reaches a widget).
#[must_use]
fn fmt_price(value: Option<Positive>) -> String {
    match value {
        Some(price) if price != Positive::INFINITY => format!("{price:.2}"),
        _ => EM_DASH.to_owned(),
    }
}

/// Format IV as a percentage to two decimals, or `—` when absent (including the
/// venue's absent-IV zero already resolved to `None` by [`project_iv`]).
///
/// The `× 100` uses **checked** multiplication: the adapter seam rejects
/// NaN/Inf/negative but **not** magnitude, so a finite-but-absurd IV (`> ~7.9e26`)
/// can survive a hostile/corrupt venue payload, and [`Decimal`]'s `Mul` **panics**
/// on overflow — a render-edge panic a pure draw must never risk (ADR-0007
/// untrusted-input hardening). On overflow it renders `—` rather than panicking.
#[must_use]
fn fmt_iv(value: Option<Positive>) -> String {
    match value {
        Some(iv) if iv != Positive::INFINITY => iv
            .to_dec()
            .checked_mul(Decimal::from(100))
            .map_or_else(|| EM_DASH.to_owned(), |pct| format!("{pct:.2}%")),
        _ => EM_DASH.to_owned(),
    }
}

/// Format a greek to four decimals, or `—` when absent. A [`Decimal`] is
/// fixed-point, so it carries no `NaN`/`Inf` to guard.
#[must_use]
fn fmt_greek(value: Option<Decimal>) -> String {
    match value {
        Some(greek) => format!("{greek:.4}"),
        None => EM_DASH.to_owned(),
    }
}

/// Format an absolute-UTC expiry as a bare calendar date (`2025-06-27`) for the
/// matrix title — a display-edge formatter over the canonical [`DateTime<Utc>`]
/// (the domain stays a `DateTime`, not a display string). Built from the date
/// components, so it needs no `strftime`/locale.
#[must_use]
fn fmt_expiry_date(expiration: DateTime<Utc>) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        expiration.year(),
        expiration.month(),
        expiration.day(),
    )
}

/// Format a strike, trailing zeros stripped (`Positive` `Display` normalizes), so
/// an integer strike reads `60000` and a fractional one keeps its places.
#[must_use]
fn fmt_strike(strike: Positive) -> String {
    if strike == Positive::INFINITY {
        return EM_DASH.to_owned();
    }
    format!("{}", strike.round_to(2))
}

// Every venue-controlled string that reaches this screen's render edge — the
// matrix title symbol/expiry, the empty-state underlying/expiry, the loading
// provider id, and the error message — is routed through the SINGLE shared
// [`sanitize`](crate::ui::theme::sanitize) (`src/ui/theme.rs`, hardened in #19),
// so the chain matrix and the status bar can never neutralize venue bytes
// differently (`docs/SECURITY.md` §6.4).

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a chain-local key, returning any follow-on [`AppEvent`] for the render
/// loop to fold (`docs/02-tui-architecture.md` §9). Pure — no I/O.
///
/// The chord resolves **through the single keybinding map**
/// ([`resolve_chain`](crate::resolve_chain), `src/app/keymap.rs`), so the chain
/// dispatch and the help overlay read one table and cannot drift — there is **no**
/// parallel key table here. Local navigation (strike cursor, leg focus) mutates
/// [`LiveState`] and returns `None` (the render loop detects the [`Selection`]
/// change and redraws, `docs/05-views-and-ux.md` §8). `a` appends the focused leg to
/// the payoff builder — the headline chain→`a`→builder gesture, sharing the Payoff
/// screen's [`append_focused_leg`](crate::ui::payoff) helper. Actions that need I/O
/// (multi-expiry subscribe, underlying switch, drill-in) are resolved but not yet
/// wired — never performed inline; they land with their data plumbing, exactly as the
/// replay screen defers its playback actions.
///
/// [`Selection`]: crate::Selection
#[must_use]
pub fn handle_key(state: &mut LiveState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_chain(chord)? {
        ChainAction::MoveStrike => {
            move_strike(state, chord);
            None
        }
        ChainAction::FocusLeg => {
            focus_leg(state, chord);
            None
        }
        ChainAction::AddLeg => {
            // The headline gesture: focus a strike with `c`/`p`, press `a` to append
            // it to the payoff builder. Reuses the SAME append logic the Payoff screen
            // uses (ui→ui is allowed), which bumps the builder revision so the driver's
            // `live_view_sig` diff marks the frame dirty; an empty chain is a safe
            // no-op.
            crate::ui::payoff::append_focused_leg(state);
            None
        }
        // Resolved through the map, but their I/O plumbing is a later issue: a
        // multi-expiry subscribe path, an underlying list, and the drill-in view.
        // They never perform I/O inline here.
        ChainAction::SwitchExpiry | ChainAction::SwitchUnderlying | ChainAction::Drill => None,
    }
}

/// Move the strike cursor up/down within the chain bounds. The first move from an
/// unset cursor reveals it at the ATM anchor; later moves step by one, clamped —
/// never an out-of-range index.
fn move_strike(state: &mut LiveState, chord: KeyChord) {
    let len = state.store.chain().options.len();
    if len == 0 {
        return;
    }
    let down = matches!(chord, KeyChord::Down | KeyChord::Char('j'));
    let current = clamp_anchor(state.selection.focused_row, None, len);
    let next = match state.selection.focused_row {
        // First move: place the cursor at the cached ATM anchor rather than jumping.
        None => state.atm_index().unwrap_or(0),
        Some(_) => {
            let row = current.unwrap_or(0);
            if down {
                (row + 1).min(floor_sub(len, 1))
            } else {
                floor_sub(row, 1)
            }
        }
    };
    state.selection.focused_row = Some(next);
}

/// Focus the call or put leg (`c` / `p`), revealing the cursor at the ATM anchor if
/// no row is focused yet so the focus has a visible target.
fn focus_leg(state: &mut LiveState, chord: KeyChord) {
    state.selection.focused_leg = match chord {
        KeyChord::Char('p') => LegFocus::Put,
        // The FocusLeg action only binds `c`/`p`; `c` (and any defensive fallback)
        // focuses the call leg.
        _ => LegFocus::Call,
    };
    if state.selection.focused_row.is_none() && !state.store.chain().options.is_empty() {
        state.selection.focused_row = Some(state.atm_index().unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::{Decimal, Positive};
    use optionstratlib::{ExpirationDate, OptionStyle};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{
        ChainRow, LegView, clamp_anchor, draw, greek_slots_for_width, handle_key, project_call,
        project_iv, project_put, project_row, resolve_leg, window_start,
    };
    use crate::app::{LegFocus, LiveScreen, LiveState, Mode, ScreenLoad, Selection, SourceBinding};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, GreeksOrigin, GreeksRow,
        Instrument, InstrumentKey, LegGreeks, ProviderId, QuoteUpdate, StreamHealth, TickDir,
    };
    use crate::chain::{ContractSpecFingerprint, ExerciseStyle, SettlementStyle};
    use crate::config::ThemeChoice;
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crate::ui::theme::{GreekColumn, StrikeRelation, Theme};

    const EXP: i64 = 1_700_000_000;

    // --- Constructors (no unwrap/expect/indexing per the ruleset) ------------

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

    #[track_caller]
    fn pos_dec(value: Decimal) -> Positive {
        match Positive::new_decimal(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive decimal: {e}"),
        }
    }

    fn dec(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    /// A fully-populated call+put row at `strike` with an IV of 50%.
    fn full_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            delta_call: Some(dec(6, 1)),
            delta_put: Some(dec(-4, 1)),
            gamma: Some(dec(1, 2)),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn chain_with(strikes: &[f64]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for strike in strikes {
            let _ = chain.options.insert(full_row(*strike));
        }
        chain
    }

    fn store_with(chain: OptionChain) -> ChainStore {
        ChainStore::seed(
            ChainFetch::new(
                chain,
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        )
    }

    fn caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(true)
            .greeks(GreeksCapability::Provided)
            .build()
    }

    fn live_with(chain: OptionChain, load: ScreenLoad) -> LiveState {
        let mut live = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store_with(chain),
        );
        live.load = load;
        live
    }

    fn spec() -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: 1,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn instrument(strike: f64, style: OptionStyle) -> Instrument {
        Instrument {
            key: InstrumentKey {
                underlying: "BTC".to_owned(),
                expiration_utc: utc(EXP),
                strike: pos(strike),
                style,
            },
            provider: pid("deribit"),
            native_symbol: format!("BTC-{strike}-{}", style.as_str()),
            stream_symbol: None,
            spec: spec(),
        }
    }

    fn quote(strike: f64, style: OptionStyle, bid: f64, ask: f64, received: i64) -> QuoteUpdate {
        QuoteUpdate {
            instrument: instrument(strike, style),
            bid: Some(pos(bid)),
            ask: Some(pos(ask)),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: Some(utc(received)),
            received_time: utc(received),
        }
    }

    /// A `LegGreeks` overriding only the named analytic fields on the empty leg
    /// (every other field `None`, every origin defaulting to `ComputedLocally`).
    fn mk_leg(
        iv: Option<(Positive, GreeksOrigin)>,
        delta: Option<Decimal>,
        gamma: Option<(Decimal, GreeksOrigin)>,
        theta: Option<Decimal>,
        vega: Option<Decimal>,
    ) -> LegGreeks {
        let mut leg = LegGreeks::default();
        if let Some((value, origin)) = iv {
            leg.iv = Some(value);
            leg.iv_origin = origin;
        }
        leg.delta = delta; // the sidecar delta is always the local fallback
        if let Some((value, origin)) = gamma {
            leg.gamma = Some(value);
            leg.gamma_origin = origin;
        }
        leg.theta = theta;
        leg.vega = vega;
        leg
    }

    /// The absolute expiry a chain resolves to — the instant the analytics sidecar
    /// keys on, so a read key and the sidecar agree.
    #[track_caller]
    fn resolved_expiry(chain: &OptionChain) -> DateTime<Utc> {
        match chain.get_expiration() {
            Some(ExpirationDate::DateTime(dt)) => dt,
            other => panic!("expected an absolute-UTC chain expiry, got {other:?}"),
        }
    }

    /// A store whose `ExpirySource` expiry matches the chain's resolved expiry, so
    /// the sidecar's compute keys equal the UI read keys — the setup a
    /// populated-Greeks projection assertion needs.
    fn store_consistent(chain: OptionChain) -> ChainStore {
        let exp = resolved_expiry(&chain);
        ChainStore::seed(
            ChainFetch::new(
                chain,
                ExpirySource::new("BTC", exp, pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        )
    }

    /// A venue Greeks row at an explicit expiry, so its key matches the sidecar's
    /// compute key. Carries venue theta/vega/rho the sidecar deliberately discards.
    fn greeks_at(
        exp: DateTime<Utc>,
        strike: f64,
        style: OptionStyle,
        iv: f64,
        gamma: Decimal,
    ) -> GreeksRow {
        GreeksRow {
            instrument: Instrument {
                key: InstrumentKey {
                    underlying: "BTC".to_owned(),
                    expiration_utc: exp,
                    strike: pos(strike),
                    style,
                },
                provider: pid("deribit"),
                native_symbol: format!("BTC-{strike}-{}", style.as_str()),
                stream_symbol: None,
                spec: spec(),
            },
            iv: Some(pos(iv)),
            delta: None,
            gamma: Some(gamma),
            theta: Some(dec(-1, 1)),
            vega: Some(dec(2, 1)),
            rho: Some(dec(3, 1)),
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(EXP + 10),
        }
    }

    /// A strike row with realistic ATM premiums (so the local IV inversion converges
    /// robustly) plus venue delta/gamma — the fixture the populated-Greeks
    /// projection tests seed.
    fn priced_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(3_000.0)),
            call_ask: Some(pos(3_100.0)),
            put_bid: Some(pos(2_000.0)),
            put_ask: Some(pos(2_100.0)),
            implied_volatility: pos(0.5),
            delta_call: Some(dec(6, 1)),
            delta_put: Some(dec(-4, 1)),
            gamma: Some(dec(1, 2)),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn priced_chain(strikes: &[f64]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for strike in strikes {
            let _ = chain.options.insert(priced_row(*strike));
        }
        chain
    }

    /// A `Ready` live state around a prebuilt store — for render assertions over a
    /// store whose sidecar keys match its read keys.
    fn live_ready_from_store(store: ChainStore) -> LiveState {
        let mut live = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store,
        );
        live.load = ScreenLoad::Ready;
        live
    }

    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        }
    }

    fn theme() -> Theme {
        Theme::resolve(ThemeChoice::Auto, false)
    }

    /// Draw the chain screen for `live` at `width`×`height` and return the frame
    /// text (row-major), for render assertions. Uses a fixed decay-reference `now`
    /// equal to the seed poll instant (markers are `Flat` without applied quotes);
    /// [`rendered_at`] drives a specific `now` for the tick-decay test.
    #[track_caller]
    fn rendered(live: &LiveState, width: u16, height: u16) -> String {
        rendered_at(live, width, height, utc(EXP))
    }

    /// Draw the chain screen at `width`×`height` with an explicit tick-stamped `now`
    /// (the wall-clock the tick-direction markers decay against) and return the
    /// frame text.
    #[track_caller]
    fn rendered_at(live: &LiveState, width: u16, height: u16, now: DateTime<Utc>) -> String {
        let mut term = terminal(width, height);
        match term.draw(|frame| draw(live, frame, frame.area(), theme(), 0, now)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // --- Projection: None iff None -------------------------------------------

    #[test]
    fn test_project_call_leg_none_iff_none_field() {
        // A row with some sides absent: the LegView field is None exactly where the
        // OptionData field is None, never a fabricated value.
        let mut od = OptionData {
            strike_price: pos(60_000.0),
            call_bid: Some(pos(1.0)),
            call_ask: None,
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            delta_call: Some(dec(5, 1)),
            gamma: None,
            ..Default::default()
        };
        od.set_mid_prices();
        // No sidecar entry: the price sides project verbatim, the venue delta wins,
        // gamma/theta/vega are absent (render `—`), and the call iv falls back to the
        // shared od.implied_volatility as a Provider-origin value (the call-leg
        // shared fallback), not a fabricated one.
        let call = project_call(&od, None, TickDir::Flat, TickDir::Flat);
        assert_eq!(call.bid, Some(pos(1.0)), "present bid projects Some");
        assert_eq!(call.ask, None, "absent ask projects None (renders em dash)");
        assert_eq!(call.mark, None, "no mid without both sides -> None");
        assert_eq!(call.delta, Some(dec(5, 1)), "venue delta_call wins");
        assert_eq!(call.gamma, None, "no sidecar entry -> gamma None");
        assert_eq!(
            call.iv,
            Some(pos(0.5)),
            "no sidecar -> call iv falls back to the shared od.implied_volatility"
        );
        assert_eq!(
            call.iv_origin,
            GreeksOrigin::Provider,
            "the shared od IV fallback is Provider-origin (no local glyph)"
        );
        assert_eq!(call.theta, None, "no sidecar entry -> theta None");
        assert_eq!(call.vega, None, "no sidecar entry -> vega None");
        // The shared IV is Provider-origin and no local field resolved, so the row is
        // Provider-origin (no glyph).
        assert_eq!(call.greeks_origin, GreeksOrigin::Provider);
    }

    #[test]
    fn test_project_put_leg_reads_put_side_fields_and_put_sidecar() {
        let od = full_row(60_000.0);
        // The put sidecar entry supplies iv/gamma (per style, not the shared field).
        let put_leg = mk_leg(
            Some((pos(0.6), GreeksOrigin::Provider)),
            None,
            Some((dec(3, 2), GreeksOrigin::Provider)),
            None,
            None,
        );
        let put = project_put(&od, Some(&put_leg), TickDir::Up, TickDir::Down);
        assert_eq!(put.bid, od.put_bid);
        assert_eq!(put.ask, od.put_ask);
        assert_eq!(put.mark, od.put_middle);
        assert_eq!(
            put.delta, od.delta_put,
            "put reads the venue delta_put, not delta_call"
        );
        assert_eq!(
            put.gamma,
            Some(dec(3, 2)),
            "put gamma is the put sidecar entry, not the shared od.gamma"
        );
        assert_eq!(put.iv, Some(pos(0.6)), "put iv is the put sidecar entry");
        assert_eq!(put.bid_dir, TickDir::Up);
        assert_eq!(put.ask_dir, TickDir::Down);
    }

    // --- The absent-IV rule (#15): a bare 0 IV renders `—`, not `0.00%` ------

    #[test]
    fn test_project_iv_zero_projects_none_not_a_fabricated_quote() {
        // The venue's absent-IV sentinel is Positive::ZERO; it must project None so
        // the matrix renders `—`, never a fabricated-looking 0.00%.
        assert_eq!(project_iv(Positive::ZERO), None);
    }

    #[test]
    fn test_project_iv_positive_projects_some() {
        assert_eq!(project_iv(pos(0.5)), Some(pos(0.5)));
    }

    #[test]
    fn test_project_call_leg_absent_iv_zero_sidecar_is_none() {
        let od = OptionData {
            strike_price: pos(60_000.0),
            ..Default::default()
        };
        // The sidecar can carry the venue's absent-IV zero sentinel; it still
        // projects to None so the matrix renders `—`, never a fabricated 0.00%.
        let leg = mk_leg(
            Some((Positive::ZERO, GreeksOrigin::Provider)),
            None,
            None,
            None,
            None,
        );
        let call = project_call(&od, Some(&leg), TickDir::Flat, TickDir::Flat);
        assert_eq!(
            call.iv, None,
            "a zero-sentinel sidecar IV projects None, not Some(0)"
        );
        // The zero-IV field is not a present local field, so no origin glyph.
        assert_eq!(call.greeks_origin, GreeksOrigin::Provider);
    }

    // --- Per-field §7 precedence (resolve_leg) -------------------------------

    #[test]
    fn test_resolve_leg_delta_prefers_venue_over_local() {
        // A local sidecar delta is present, but a venue per-leg delta wins.
        let leg = mk_leg(None, Some(dec(-9, 1)), None, None, None);
        let v = resolve_leg(
            Some(pos(1.0)),
            Some(pos(1.2)),
            Some(pos(1.1)),
            Some(dec(6, 1)),
            None,
            Some(&leg),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(v.delta, Some(dec(6, 1)), "venue delta wins over local");
        // Only the venue delta resolved -> Provider (no glyph).
        assert_eq!(v.greeks_origin, GreeksOrigin::Provider);
    }

    #[test]
    fn test_resolve_leg_delta_falls_back_to_local() {
        // No venue delta -> the local sidecar delta is used, and it badges the row.
        let leg = mk_leg(None, Some(dec(-9, 1)), None, Some(dec(-5, 2)), None);
        let v = resolve_leg(
            None,
            None,
            None,
            None,
            None,
            Some(&leg),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(
            v.delta,
            Some(dec(-9, 1)),
            "no venue delta -> local fallback"
        );
        assert_eq!(v.greeks_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_resolve_leg_iv_gamma_carry_sidecar_origin() {
        // Venue-origin iv/gamma (plus a venue delta) keep the row Provider.
        let venue = mk_leg(
            Some((pos(0.55), GreeksOrigin::Provider)),
            None,
            Some((dec(1, 4), GreeksOrigin::Provider)),
            None,
            None,
        );
        let v = resolve_leg(
            None,
            None,
            None,
            Some(dec(5, 1)),
            None,
            Some(&venue),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(v.iv, Some(pos(0.55)));
        assert_eq!(v.gamma, Some(dec(1, 4)));
        assert_eq!(
            v.greeks_origin,
            GreeksOrigin::Provider,
            "venue iv/gamma + venue delta -> Provider"
        );
        // A locally-computed iv or gamma badges the row local.
        let local = mk_leg(
            Some((pos(0.6), GreeksOrigin::ComputedLocally)),
            None,
            Some((dec(2, 4), GreeksOrigin::ComputedLocally)),
            None,
            None,
        );
        let w = resolve_leg(
            None,
            None,
            None,
            Some(dec(5, 1)),
            None,
            Some(&local),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(w.iv, Some(pos(0.6)));
        assert_eq!(w.gamma, Some(dec(2, 4)));
        assert_eq!(w.greeks_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_resolve_leg_theta_vega_are_always_local() {
        // theta/vega only ever come from the local sidecar; present ones badge the
        // row local even alongside a venue delta (a mixed-origin row).
        let leg = mk_leg(None, None, None, Some(dec(-5, 2)), Some(dec(3, 2)));
        let v = resolve_leg(
            None,
            None,
            None,
            Some(dec(6, 1)),
            None,
            Some(&leg),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(v.theta, Some(dec(-5, 2)));
        assert_eq!(v.vega, Some(dec(3, 2)));
        assert_eq!(
            v.greeks_origin,
            GreeksOrigin::ComputedLocally,
            "venue delta + local vega is a mixed-origin, locally-badged row"
        );
    }

    #[test]
    fn test_resolve_leg_absent_fields_stay_none_render_em_dash() {
        // No venue delta and no sidecar entry: every analytic is None and renders
        // `—`, never a fabricated 0.
        let v = resolve_leg(
            None,
            None,
            None,
            None,
            None,
            None,
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(v.delta, None);
        assert_eq!(v.iv, None);
        assert_eq!(v.gamma, None);
        assert_eq!(v.theta, None);
        assert_eq!(v.vega, None);
        assert_eq!(super::fmt_greek(v.theta), super::EM_DASH);
        assert_eq!(super::fmt_greek(v.delta), super::EM_DASH);
        assert_eq!(v.greeks_origin, GreeksOrigin::Provider);
    }

    // --- IV precedence: shared call-side fallback + the local plausibility floor ---

    #[test]
    fn test_resolve_iv_local_below_floor_clears_to_none() {
        // (a) A locally-computed IV below the plausibility floor (0.5%) is
        // economically implausible for a live quote -> cleared to None (renders `—`),
        // never a fabricated near-zero percentage.
        let leg = mk_leg(
            Some((pos(0.0003), GreeksOrigin::ComputedLocally)),
            None,
            None,
            None,
            None,
        );
        // No shared IV (put-like), so the sub-floor local value is not rescued.
        let v = resolve_leg(
            None,
            None,
            None,
            None,
            None,
            Some(&leg),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(v.iv, None, "a sub-0.5% local IV is floored to None");
        assert_eq!(super::fmt_iv(v.iv), super::EM_DASH);
    }

    #[test]
    fn test_resolve_iv_local_above_floor_still_shows() {
        // (b) A legitimate provider-computed IV (e.g. IG equities, always >> 0.5%)
        // clears the floor and renders as a percentage with the local-origin badge.
        let leg = mk_leg(
            Some((pos(0.25), GreeksOrigin::ComputedLocally)),
            None,
            None,
            None,
            None,
        );
        let v = resolve_leg(
            None,
            None,
            None,
            None,
            None,
            Some(&leg),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(v.iv, Some(pos(0.25)), "an at/above-floor local IV survives");
        assert_eq!(v.iv_origin, GreeksOrigin::ComputedLocally);
        assert!(v.iv_is_local(), "a present local IV badges the IV cell");
        assert_eq!(super::fmt_iv(v.iv), "25.00%");
    }

    #[test]
    fn test_resolve_iv_venue_below_floor_is_never_cleared() {
        // (c) A VENUE (Provider) IV is trusted even below the plausibility floor —
        // only the exact-zero absent sentinel clears a venue IV, never the floor.
        let leg = mk_leg(
            Some((pos(0.001), GreeksOrigin::Provider)),
            None,
            None,
            None,
            None,
        );
        let v = resolve_leg(
            None,
            None,
            None,
            None,
            None,
            Some(&leg),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(
            v.iv,
            Some(pos(0.001)),
            "a sub-floor venue IV is trusted, not floored"
        );
        assert_eq!(v.iv_origin, GreeksOrigin::Provider);
        assert!(!v.iv_is_local(), "a venue IV is never badged local");
    }

    #[test]
    fn test_resolve_iv_call_shared_fallback_is_provider_origin() {
        // (d) The sidecar holds only a near-zero LOCAL IV, but the call passes its
        // shared od.implied_volatility -> the honest venue call-side value wins as a
        // Provider-origin IV (not the floored local garbage).
        let od = OptionData {
            strike_price: pos(60_000.0),
            implied_volatility: pos(0.4922),
            delta_call: Some(dec(6, 1)),
            ..Default::default()
        };
        let leg = mk_leg(
            Some((pos(0.00003), GreeksOrigin::ComputedLocally)),
            None,
            None,
            Some(dec(-1, 1)),
            None,
        );
        let call = project_call(&od, Some(&leg), TickDir::Flat, TickDir::Flat);
        assert_eq!(
            call.iv,
            Some(pos(0.4922)),
            "the shared od IV wins over the near-zero local"
        );
        assert_eq!(
            call.iv_origin,
            GreeksOrigin::Provider,
            "the shared od IV fallback is Provider-origin"
        );
        assert!(!call.iv_is_local(), "no local glyph on the shared venue IV");
    }

    #[test]
    fn test_resolve_iv_put_ignores_shared_field_no_collision() {
        // (e) The shared od.implied_volatility is the CALL-side value (§7). A strike
        // whose shared field is set but whose PUT sidecar has no per-style venue IV
        // shows put IV `—`, never the call's value — the call/put collision the
        // style-keyed sidecar exists to prevent.
        let od = OptionData {
            strike_price: pos(60_000.0),
            implied_volatility: pos(0.4922),
            delta_put: Some(dec(-4, 1)),
            ..Default::default()
        };
        // Put sidecar: a near-zero LOCAL IV (floored) and no venue IV.
        let leg = mk_leg(
            Some((pos(0.002), GreeksOrigin::ComputedLocally)),
            None,
            None,
            Some(dec(-1, 1)),
            None,
        );
        let put = project_put(&od, Some(&leg), TickDir::Flat, TickDir::Flat);
        assert_eq!(
            put.iv, None,
            "the put never inherits the call-side shared IV"
        );
        assert_eq!(super::fmt_iv(put.iv), super::EM_DASH);
    }

    #[test]
    fn test_origin_glyph_badges_present_local_field_not_venue_delta() {
        // (f) The origin glyph appears iff a PRESENT resolved field is local, badging
        // the actual computed cell — not the trustworthy venue delta beside it.
        // Mixed-origin: venue delta (Provider) + local theta.
        let mixed = mk_leg(None, None, None, Some(dec(-5, 2)), None);
        let v = resolve_leg(
            None,
            None,
            None,
            Some(dec(6, 1)),
            None,
            Some(&mixed),
            (TickDir::Flat, TickDir::Flat),
        );
        assert!(
            !v.greek_is_local(GreekColumn::Delta),
            "the venue delta is not badged"
        );
        assert!(
            v.greek_is_local(GreekColumn::Theta),
            "the local theta is badged"
        );
        // A leg with a local theta but NO delta at all (None) still badges the theta,
        // where the old delta-gated glyph would have shown nothing.
        let no_delta = mk_leg(None, None, None, Some(dec(-5, 2)), None);
        let w = resolve_leg(
            None,
            None,
            None,
            None,
            None,
            Some(&no_delta),
            (TickDir::Flat, TickDir::Flat),
        );
        assert_eq!(w.delta, None, "no delta at all");
        assert!(
            !w.greek_is_local(GreekColumn::Delta),
            "an absent delta is never badged"
        );
        assert!(
            w.greek_is_local(GreekColumn::Theta),
            "the local theta still badges with a None delta"
        );
        assert_eq!(
            w.greeks_origin,
            GreeksOrigin::ComputedLocally,
            "rollup preserved: any present local field -> row local"
        );
        // A fully-venue leg (venue delta + venue iv/gamma) badges nothing.
        let venue = mk_leg(
            Some((pos(0.5), GreeksOrigin::Provider)),
            None,
            Some((dec(1, 4), GreeksOrigin::Provider)),
            None,
            None,
        );
        let f = resolve_leg(
            None,
            None,
            None,
            Some(dec(5, 1)),
            None,
            Some(&venue),
            (TickDir::Flat, TickDir::Flat),
        );
        assert!(!f.greek_is_local(GreekColumn::Delta));
        assert!(!f.greek_is_local(GreekColumn::Gamma));
        assert!(!f.iv_is_local());
        assert_eq!(f.greeks_origin, GreeksOrigin::Provider);
    }

    // --- Store-fed projection: sidecar populated, unequal legs survive -------

    #[test]
    fn test_project_row_populates_local_theta_vega_from_sidecar() {
        let chain = priced_chain(&[60_000.0]);
        let exp = resolved_expiry(&chain);
        let store = store_consistent(chain);
        let od = match store.chain().options.iter().next() {
            Some(od) => od.clone(),
            None => panic!("expected one row"),
        };
        let row = project_row(
            &od,
            pos(60_000.0),
            &store,
            "BTC",
            exp,
            store.last_full_poll(),
        );
        // The seed recompute filled the local analytics for the call leg.
        assert!(row.call.theta.is_some(), "local theta populated");
        assert!(row.call.vega.is_some(), "local vega populated");
        assert!(row.call.gamma.is_some(), "local gamma populated");
        // The call iv resolves — the shared od.implied_volatility fallback outranks
        // the local inversion at seed, so it is Some regardless.
        assert!(row.call.iv.is_some(), "call iv resolves");
        // Venue delta is present, so delta resolves to it, but the local theta/vega
        // make the row ComputedLocally (a mixed-origin row).
        assert_eq!(row.call.delta, od.delta_call);
        assert_eq!(row.call.greeks_origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_unequal_call_put_iv_gamma_survive_projection_both_orders() {
        // The shared-field-loss fix: unequal call/put venue iv/gamma both survive,
        // independent of the arrival order (the style-keyed sidecar).
        let project_both = |call_first: bool| -> (LegView, LegView) {
            let chain = priced_chain(&[60_000.0]);
            let exp = resolved_expiry(&chain);
            let mut store = store_consistent(chain);
            let call = greeks_at(exp, 60_000.0, OptionStyle::Call, 0.40, dec(1, 2));
            let put = greeks_at(exp, 60_000.0, OptionStyle::Put, 0.60, dec(2, 2));
            if call_first {
                let _ = store.apply_greeks(&call);
                let _ = store.apply_greeks(&put);
            } else {
                let _ = store.apply_greeks(&put);
                let _ = store.apply_greeks(&call);
            }
            let od = match store.chain().options.iter().next() {
                Some(od) => od.clone(),
                None => panic!("expected one row"),
            };
            let row = project_row(
                &od,
                pos(60_000.0),
                &store,
                "BTC",
                exp,
                store.last_full_poll(),
            );
            (row.call, row.put)
        };
        for call_first in [true, false] {
            let (call, put) = project_both(call_first);
            assert_eq!(
                call.iv,
                Some(pos(0.40)),
                "call iv preserved (order={call_first})"
            );
            assert_eq!(
                put.iv,
                Some(pos(0.60)),
                "put iv preserved (order={call_first})"
            );
            assert_eq!(call.gamma, Some(dec(1, 2)), "call gamma preserved");
            assert_eq!(put.gamma, Some(dec(2, 2)), "put gamma preserved");
            assert_ne!(call.iv, put.iv, "unequal call/put iv both survive");
            assert_ne!(call.gamma, put.gamma, "unequal call/put gamma both survive");
        }
    }

    #[test]
    fn test_populated_matrix_shows_origin_glyph_for_local_greeks() {
        // A consistent-expiry store: the seed recompute fills local theta/vega, so
        // the rows are ComputedLocally and carry the `~` origin glyph.
        let live = live_ready_from_store(store_consistent(priced_chain(&[
            59_000.0, 60_000.0, 61_000.0,
        ])));
        let text = rendered(&live, 160, 20);
        assert!(
            text.contains('~'),
            "a locally-computed row shows the origin glyph"
        );
    }

    // --- StrikeRelation bucketing via the row projection ---------------------

    #[test]
    fn test_project_row_buckets_strike_relation_below_at_above() {
        let chain = chain_with(&[50_000.0, 60_000.0, 70_000.0]);
        let store = store_with(chain);
        let spot = pos(60_000.0);
        let expiration = utc(EXP);
        let below = match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(50_000.0))
        {
            Some(od) => project_row(od, spot, &store, "BTC", expiration, store.last_full_poll()),
            None => panic!("expected a 50000 strike"),
        };
        assert_eq!(below.strike_relation, StrikeRelation::BelowSpot);
        let at = match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(60_000.0))
        {
            Some(od) => project_row(od, spot, &store, "BTC", expiration, store.last_full_poll()),
            None => panic!("expected a 60000 strike"),
        };
        assert_eq!(at.strike_relation, StrikeRelation::AtSpot);
        let above = match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(70_000.0))
        {
            Some(od) => project_row(od, spot, &store, "BTC", expiration, store.last_full_poll()),
            None => panic!("expected a 70000 strike"),
        };
        assert_eq!(above.strike_relation, StrikeRelation::AboveSpot);
    }

    // --- Direction projection reads the store's decayed baseline -------------

    #[test]
    fn test_project_row_projects_rising_bid_direction_up() {
        // Two rising quotes give the store an Up bid direction; the projection
        // reads it (as of the last-poll instant, which precedes the changes, so no
        // decay applies).
        let mut store = store_with(chain_with(&[60_000.0]));
        let _ = store.apply_quote(&quote(60_000.0, OptionStyle::Call, 1.0, 1.2, EXP + 100));
        let _ = store.apply_quote(&quote(60_000.0, OptionStyle::Call, 1.5, 1.7, EXP + 101));
        let od = match store.chain().options.iter().next() {
            Some(od) => od.clone(),
            None => panic!("expected one row"),
        };
        let row = project_row(
            &od,
            pos(60_000.0),
            &store,
            "BTC",
            utc(EXP),
            store.last_full_poll(),
        );
        assert_eq!(row.call.bid_dir, TickDir::Up, "a rising bid projects Up");
        assert_eq!(row.call.ask_dir, TickDir::Up, "a rising ask projects Up");
        // The put leg had no quotes -> Flat.
        assert_eq!(row.put.bid_dir, TickDir::Flat);
    }

    #[test]
    fn test_project_row_no_reference_instant_is_flat() {
        let store = store_with(chain_with(&[60_000.0]));
        let od = match store.chain().options.iter().next() {
            Some(od) => od.clone(),
            None => panic!("expected one row"),
        };
        // as_of = None -> directions default to Flat without touching the store.
        let row = project_row(&od, pos(60_000.0), &store, "BTC", utc(EXP), None);
        assert_eq!(row.call.bid_dir, TickDir::Flat);
        assert_eq!(row.put.ask_dir, TickDir::Flat);
    }

    // --- Windowing / anchoring helpers (no out-of-range index) ---------------

    #[test]
    fn test_clamp_anchor_falls_back_when_cursor_out_of_range() {
        assert_eq!(
            clamp_anchor(Some(2), Some(1), 5),
            Some(2),
            "in-range cursor kept"
        );
        assert_eq!(
            clamp_anchor(Some(9), Some(1), 5),
            Some(4),
            "over-range -> last row"
        );
        assert_eq!(clamp_anchor(None, Some(3), 5), Some(3), "unset -> ATM");
        assert_eq!(clamp_anchor(None, None, 0), None, "empty chain -> None");
    }

    #[test]
    fn test_window_start_keeps_anchor_visible() {
        assert_eq!(window_start(0, 5, 3), 0, "chain fits -> start 0");
        assert_eq!(window_start(10, 5, 20), 8, "centers the anchor");
        assert_eq!(window_start(19, 5, 20), 15, "clamps to the last window");
        assert_eq!(window_start(5, 0, 20), 0, "zero visible -> start 0");
    }

    #[test]
    fn test_greek_slots_for_width_is_bounded_and_grows() {
        assert_eq!(greek_slots_for_width(20), 0, "too narrow -> Delta only");
        assert!(greek_slots_for_width(200) <= 3, "at most three greek slots");
        assert!(
            greek_slots_for_width(200) >= greek_slots_for_width(120),
            "wider fits at least as many",
        );
    }

    // --- v0.2 column set: Δ always + responsive Γ→ν→Θ drop order -------------

    #[test]
    fn test_columns_full_greek_set_honors_drop_order() {
        use crate::ui::theme::greek_columns_for_slots;
        let call_greeks = |plan: &[super::ChainCol]| -> Vec<GreekColumn> {
            plan.iter()
                .filter_map(|col| match col {
                    super::ChainCol::CallGreek(greek) => Some(*greek),
                    _ => None,
                })
                .collect()
        };
        // Δ only at 0 slots; Θ retained first (1), then ν (2), then Γ (3).
        assert_eq!(
            call_greeks(&super::columns(greek_columns_for_slots(0))),
            vec![GreekColumn::Delta],
            "0 slots: Delta only",
        );
        assert_eq!(
            call_greeks(&super::columns(greek_columns_for_slots(1))),
            vec![GreekColumn::Delta, GreekColumn::Theta],
            "1 slot: Delta + Theta (Θ retained first)",
        );
        assert_eq!(
            call_greeks(&super::columns(greek_columns_for_slots(2))),
            vec![GreekColumn::Delta, GreekColumn::Vega, GreekColumn::Theta],
            "2 slots: adds Vega",
        );
        assert_eq!(
            call_greeks(&super::columns(greek_columns_for_slots(3))),
            vec![
                GreekColumn::Delta,
                GreekColumn::Gamma,
                GreekColumn::Vega,
                GreekColumn::Theta,
            ],
            "3 slots: adds Gamma (dropped first as width shrinks)",
        );
        // The put side mirrors the call side (Δ outermost on the far right).
        let put_greeks: Vec<GreekColumn> = super::columns(greek_columns_for_slots(3))
            .iter()
            .filter_map(|col| match col {
                super::ChainCol::PutGreek(greek) => Some(*greek),
                _ => None,
            })
            .collect();
        assert_eq!(
            put_greeks,
            vec![
                GreekColumn::Theta,
                GreekColumn::Vega,
                GreekColumn::Gamma,
                GreekColumn::Delta,
            ],
            "put side mirrors the call side",
        );
    }

    #[test]
    fn test_draw_matrix_greek_columns_are_responsive() {
        let live = live_ready_from_store(store_consistent(priced_chain(&[
            59_000.0, 60_000.0, 61_000.0,
        ])));
        // A common 120-col terminal fits one optional greek: Θ (retained first),
        // not Γ (which needs the widest layout).
        let common = rendered(&live, 120, 20);
        assert!(common.contains("Θ"), "theta column shows at 120 cols");
        assert!(!common.contains("Γ"), "gamma needs a wider terminal");
        // A wide terminal fits all three optional greeks.
        let wide = rendered(&live, 200, 20);
        assert!(wide.contains("Γ"), "gamma shows on a wide terminal");
        assert!(wide.contains("ν"), "vega shows on a wide terminal");
        assert!(wide.contains("Θ"), "theta shows on a wide terminal");
    }

    // --- fmt_iv never panics at the render edge on an absurd magnitude -------

    #[test]
    fn test_fmt_iv_overflowing_magnitude_renders_em_dash_not_panic() {
        // A finite-but-absurd IV can survive the adapter seam (magnitude is not
        // rejected). fmt_iv's checked ×100 renders `—`, never panics (ADR-0007).
        let huge = pos_dec(Decimal::MAX);
        assert_eq!(super::fmt_iv(Some(huge)), super::EM_DASH);
        // A normal IV still formats as a percentage.
        assert_eq!(super::fmt_iv(Some(pos(0.5))), "50.00%");
    }

    // --- States render before the happy path, deliberately, without panic ----

    #[test]
    fn test_draw_loading_state_shows_connecting_to_provider() {
        let live = live_with(chain_with(&[60_000.0]), ScreenLoad::Loading);
        let text = rendered(&live, 120, 20);
        assert!(text.contains("connecting to deribit"), "names the provider");
    }

    #[test]
    fn test_draw_empty_state_shows_no_data_hint() {
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let live = live_with(empty, ScreenLoad::Ready);
        let text = rendered(&live, 120, 20);
        assert!(text.contains("no data for BTC"), "names the underlying");
        assert!(text.contains("2025-06-27"), "names the expiry");
    }

    #[test]
    fn test_draw_error_state_shows_message_and_retry_key() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Loading);
        live.load = ScreenLoad::Error {
            message: "provider unreachable".to_owned(),
        };
        let text = rendered(&live, 120, 20);
        assert!(text.contains("provider unreachable"), "shows the message");
        assert!(text.contains("press r to reconnect"), "shows the retry key");
    }

    #[test]
    fn test_draw_populated_matrix_shows_strike_and_atm_marker() {
        let live = live_with(
            chain_with(&[59_000.0, 60_000.0, 61_000.0]),
            ScreenLoad::Ready,
        );
        let text = rendered(&live, 120, 20);
        assert!(text.contains("60000"), "renders a strike");
        assert!(text.contains("◀ATM"), "marks the ATM strike");
        assert!(text.contains("Strike"), "renders the header");
    }

    #[test]
    fn test_draw_stale_feed_shows_badge_not_blank() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        live.source.health = StreamHealth::Reconnecting { attempt: 2 };
        live.store
            .apply_health(StreamHealth::Reconnecting { attempt: 2 });
        let text = rendered(&live, 120, 20);
        assert!(text.contains("60000"), "the last chain still renders");
        assert!(
            text.contains("reconnecting"),
            "shows the reconnecting badge"
        );
    }

    #[test]
    fn test_draw_absent_iv_row_renders_em_dash_not_zero_percent() {
        // A row whose IV is the absent-sentinel zero must render an em dash, never
        // a fabricated 0.00%.
        let mut od = OptionData {
            strike_price: pos(60_000.0),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            implied_volatility: Positive::ZERO,
            ..Default::default()
        };
        od.set_mid_prices();
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(od);
        let live = live_with(chain, ScreenLoad::Ready);
        let text = rendered(&live, 120, 20);
        assert!(
            text.contains(super::EM_DASH),
            "the absent IV renders an em dash"
        );
        assert!(!text.contains("0.00%"), "no fabricated 0.00% IV");
    }

    #[test]
    fn test_draw_reachable_states_render_across_sizes_without_panic() {
        // The chain screen's reachable states (populated / empty / loading / error /
        // stale) at several sizes never panic (extends render_never_panics to the
        // chain body directly).
        let populated = chain_with(&[58_000.0, 59_000.0, 60_000.0, 61_000.0, 62_000.0]);
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let mut stale = live_with(populated.clone(), ScreenLoad::Ready);
        stale
            .store
            .apply_health(StreamHealth::Stale { since: utc(EXP) });
        stale.source.health = StreamHealth::Stale { since: utc(EXP) };
        let states = [
            live_with(populated.clone(), ScreenLoad::Ready),
            live_with(empty, ScreenLoad::Ready),
            live_with(populated.clone(), ScreenLoad::Loading),
            {
                let mut e = live_with(populated, ScreenLoad::Ready);
                e.load = ScreenLoad::Error {
                    message: "boom".to_owned(),
                };
                e
            },
            stale,
        ];
        for live in &states {
            for (w, h) in [(40u16, 8u16), (80, 24), (120, 40), (160, 50)] {
                let _ = rendered(live, w, h);
            }
        }
    }

    // --- Fix: the tick-direction marker decays on the wall clock, not last poll --

    #[test]
    fn test_draw_direction_marker_decays_on_wall_clock_not_last_poll() {
        // Two rising call quotes give an Up bid/ask direction with `changed_at` at
        // EXP+101. Rendered with `now` at the change the ▲ marker shows; rendered
        // with `now` advanced past the ~3 s decay window it is gone — proving draw
        // decays against the tick-stamped `now` it is passed, NOT `last_full_poll`
        // (pinned at EXP, which would keep the marker until the next poll).
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let _ = live
            .store
            .apply_quote(&quote(60_000.0, OptionStyle::Call, 1.0, 1.2, EXP + 100));
        let _ = live
            .store
            .apply_quote(&quote(60_000.0, OptionStyle::Call, 1.5, 1.7, EXP + 101));
        let fresh = rendered_at(&live, 120, 12, utc(EXP + 101));
        assert!(
            fresh.contains('▲'),
            "a just-risen quote shows the up marker"
        );
        let decayed = rendered_at(&live, 120, 12, utc(EXP + 200));
        assert!(
            !decayed.contains('▲'),
            "past the decay window the marker decays on wall-time, not the last poll",
        );
    }

    // --- Fix: an 80-col terminal shows a widen hint, never a clipped chain -------

    #[test]
    fn test_draw_narrow_terminal_shows_widen_hint_not_clipped_chain() {
        // Below the mandatory-column width the chain would clip; at 40 and 80 cols
        // the screen shows an honest "widen" hint instead of a corrupt/clipped
        // matrix. NO_COLOR-safe (a dim text hint) and the greek drop order is
        // untouched.
        let live = live_with(
            chain_with(&[59_000.0, 60_000.0, 61_000.0]),
            ScreenLoad::Ready,
        );
        for w in [40u16, 80u16] {
            let text = rendered(&live, w, 12);
            assert!(
                text.contains("widen"),
                "at {w} cols the chain shows a widen hint, not a clipped table",
            );
            assert!(
                !text.contains("Strike"),
                "at {w} cols no clipped chain header leaks",
            );
        }
        // Above the mandatory-column width the real chain renders in full.
        let wide = rendered(&live, 120, 20);
        assert!(wide.contains("Strike"), "the chain renders at 120 cols");
        assert!(wide.contains("60000"), "a strike renders at 120 cols");
    }

    // --- Draw purity: draw takes &LiveState and mutates nothing --------------

    #[test]
    fn test_draw_is_pure_leaves_state_unchanged() {
        // `draw` takes `&LiveState`, so it cannot mutate the store, selection, or
        // load; assert the observable state is unchanged across a draw (no pricing
        // call, no mutation, no state flip).
        let live = live_with(
            chain_with(&[59_000.0, 60_000.0, 61_000.0]),
            ScreenLoad::Ready,
        );
        let before_len = live.store.chain().options.len();
        let before_poll = live.store.last_full_poll();
        let before_sel = live.selection;
        let before_health = matches!(live.store.health(), StreamHealth::Live);
        let _ = rendered(&live, 120, 40);
        assert_eq!(
            live.store.chain().options.len(),
            before_len,
            "no rows added/removed"
        );
        assert_eq!(live.store.last_full_poll(), before_poll, "no re-poll");
        assert_eq!(live.selection, before_sel, "selection unchanged");
        assert_eq!(
            matches!(live.store.health(), StreamHealth::Live),
            before_health,
            "health unchanged",
        );
    }

    #[test]
    fn test_draw_leaves_sidecar_unchanged_no_pricing_in_draw() {
        // A consistent-expiry store so the read key hits a real sidecar entry; the
        // projection reads the cached analytics and must invoke no pricing/recompute
        // in `draw` (the entry is byte-identical before and after a draw).
        let chain = priced_chain(&[60_000.0]);
        let exp = resolved_expiry(&chain);
        let live = live_ready_from_store(store_consistent(chain));
        let key = InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: exp,
            strike: pos(60_000.0),
            style: OptionStyle::Call,
        };
        let before = live.store.leg_greeks(&key).copied();
        assert!(before.is_some(), "the seeded sidecar entry is present");
        let _ = rendered(&live, 160, 40);
        let after = live.store.leg_greeks(&key).copied();
        assert_eq!(
            before, after,
            "draw reads the cached sidecar and never recomputes or mutates it",
        );
    }

    // --- handle_key: nav resolves through the keymap, mutates local state ----

    #[test]
    fn test_handle_key_move_strike_down_reveals_cursor_at_atm_then_steps() {
        let mut live = live_with(
            chain_with(&[58_000.0, 60_000.0, 62_000.0]),
            ScreenLoad::Ready,
        );
        assert_eq!(live.selection.focused_row, None, "no cursor initially");
        // First `j` reveals the cursor at the ATM anchor (index 1 for spot 60000).
        assert!(handle_key(&mut live, press(KeyCode::Char('j'))).is_none());
        assert_eq!(live.selection.focused_row, Some(1), "cursor at ATM");
        // Next `j` steps down, clamped to the last row.
        let _ = handle_key(&mut live, press(KeyCode::Char('j')));
        assert_eq!(live.selection.focused_row, Some(2));
        let _ = handle_key(&mut live, press(KeyCode::Down));
        assert_eq!(
            live.selection.focused_row,
            Some(2),
            "clamped at the last row"
        );
        // `k` / Up step back up.
        let _ = handle_key(&mut live, press(KeyCode::Char('k')));
        assert_eq!(live.selection.focused_row, Some(1));
    }

    #[test]
    fn test_handle_key_focus_leg_sets_call_or_put() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let _ = handle_key(&mut live, press(KeyCode::Char('p')));
        assert_eq!(live.selection.focused_leg, LegFocus::Put);
        assert!(
            live.selection.focused_row.is_some(),
            "focus reveals the cursor"
        );
        let _ = handle_key(&mut live, press(KeyCode::Char('c')));
        assert_eq!(live.selection.focused_leg, LegFocus::Call);
    }

    #[test]
    fn test_handle_key_unbound_key_returns_none_and_changes_nothing() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let before = live.selection;
        assert!(handle_key(&mut live, press(KeyCode::Char('z'))).is_none());
        assert_eq!(live.selection, before, "an unbound key changes nothing");
    }

    #[test]
    fn test_handle_key_deferred_actions_are_noops_no_io() {
        // Expiry/underlying/drill resolve through the map but are not yet wired; they
        // return None and perform no I/O, changing no selection. (AddLeg `a` is wired
        // in #26 — covered separately — so it is not in this deferred set.)
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let before = live.selection;
        for code in [
            KeyCode::Char('l'), // SwitchExpiry
            KeyCode::Char(']'), // SwitchUnderlying
            KeyCode::Enter,     // Drill
        ] {
            assert!(handle_key(&mut live, press(code)).is_none());
        }
        assert_eq!(live.selection, before, "deferred actions change no state");
    }

    #[test]
    fn test_handle_key_add_leg_appends_focused_leg_to_builder_and_marks_dirty() {
        // The headline chain→`a`→builder gesture: focus a call with `c`, press `a`, and
        // the focused leg lands in the payoff builder; a second focus+`a` appends in
        // order. Each successful append bumps the builder revision (what the driver
        // diffs to mark the frame dirty).
        let mut live = live_with(
            chain_with(&[58_000.0, 60_000.0, 62_000.0]),
            ScreenLoad::Ready,
        );
        assert!(live.payoff_builder.is_empty(), "builder starts empty");
        let rev0 = live.payoff_builder.revision();

        // `c` focuses the call leg and reveals the cursor at the ATM anchor (index 1 =
        // 60000); `a` appends that focused call.
        let _ = handle_key(&mut live, press(KeyCode::Char('c')));
        assert_eq!(
            live.selection.focused_row,
            Some(1),
            "focus reveals the ATM cursor"
        );
        let _ = handle_key(&mut live, press(KeyCode::Char('a')));
        assert_eq!(live.payoff_builder.legs().len(), 1, "one leg appended");
        let leg0 = match live.payoff_builder.legs().first() {
            Some(leg) => *leg,
            None => panic!("expected a first leg"),
        };
        assert_eq!(leg0.strike, pos(60_000.0), "the focused strike is appended");
        assert_eq!(leg0.style, OptionStyle::Call, "the focused call leg");
        let rev1 = live.payoff_builder.revision();
        assert!(
            rev1 > rev0,
            "a successful append bumps the builder revision (marks the frame dirty)"
        );

        // Step down to 62000, focus the put leg, then `a` appends it AFTER the call.
        let _ = handle_key(&mut live, press(KeyCode::Char('j')));
        let _ = handle_key(&mut live, press(KeyCode::Char('p')));
        let _ = handle_key(&mut live, press(KeyCode::Char('a')));
        assert_eq!(
            live.payoff_builder.legs().len(),
            2,
            "second leg appended in order"
        );
        let leg1 = match live.payoff_builder.legs().get(1) {
            Some(leg) => *leg,
            None => panic!("expected a second leg"),
        };
        assert_eq!(
            leg1.strike,
            pos(62_000.0),
            "second leg is the newly focused strike"
        );
        assert_eq!(leg1.style, OptionStyle::Put, "second leg is a put");
        assert!(
            live.payoff_builder.revision() > rev1,
            "the second append bumps the revision again"
        );
    }

    #[test]
    fn test_handle_key_add_leg_on_empty_chain_is_safe_noop() {
        // An empty chain: `a` appends nothing and leaves the builder untouched (no
        // revision bump), the same bounds-safe no-op as before wiring.
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let mut live = live_with(empty, ScreenLoad::Ready);
        let rev0 = live.payoff_builder.revision();
        let _ = handle_key(&mut live, press(KeyCode::Char('a')));
        assert!(
            live.payoff_builder.is_empty(),
            "no leg appended on an empty chain"
        );
        assert_eq!(
            live.payoff_builder.revision(),
            rev0,
            "a no-op append does not bump the revision"
        );
    }

    #[test]
    fn test_handle_key_move_strike_on_empty_chain_is_noop() {
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let mut live = live_with(empty, ScreenLoad::Ready);
        let _ = handle_key(&mut live, press(KeyCode::Char('j')));
        assert_eq!(
            live.selection.focused_row, None,
            "no cursor on an empty chain"
        );
    }

    // --- Sanity: view models are Copy and round-trip through a live state -----

    #[test]
    fn test_chain_row_and_leg_view_are_constructible_copy_values() {
        let od = full_row(60_000.0);
        let row: ChainRow = ChainRow {
            strike: od.strike_price,
            call: project_call(&od, None, TickDir::Flat, TickDir::Flat),
            put: project_put(&od, None, TickDir::Flat, TickDir::Flat),
            strike_relation: StrikeRelation::AtSpot,
        };
        // Copy: using the value twice compiles without a move error.
        let copy: ChainRow = row;
        let _first: LegView = row.call;
        let _second: LegView = copy.call;
        assert_eq!(row.strike, copy.strike);
    }

    // Keep the imports for a Selection-shaped default used above meaningful.
    #[test]
    fn test_selection_default_focuses_call_leg() {
        let selection = Selection::default();
        assert_eq!(selection.focused_leg, LegFocus::Call);
        assert_eq!(selection.focused_row, None);
    }

    // Ensure the Mode/LiveScreen imports are exercised (a live state renders on the
    // Chain screen).
    #[test]
    fn test_live_state_defaults_to_chain_screen() {
        let live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let app_mode = Mode::Live(live);
        match app_mode {
            Mode::Live(state) => assert_eq!(state.screen, LiveScreen::Chain),
            Mode::Replay(_) => panic!("expected a live mode"),
        }
    }

    // =====================================================================
    // Render goldens (#19, docs/TESTING.md §4) + escape-sequence hygiene
    // (docs/SECURITY.md §6.4). Rendered into a TestBackend at a FIXED 120x40
    // and compared against a committed golden; deterministic (fixed as-of
    // instant / the fixture's own timestamps, no wall clock, no socket), so
    // the bytes are stable across machines.
    // =====================================================================

    /// A hostile venue-controlled underlying carrying an OSC clipboard-write
    /// (`ESC ] 52 … BEL`), a CSI clear-screen (`ESC [ 2J`), a raw newline/tab, and
    /// an 8-bit C1 `CSI` (`0x9B`) — the escape-hygiene probe. Written with `\u{..}`
    /// escapes, so the SOURCE file carries no raw control byte.
    const HOSTILE_SYMBOL: &str = "BTC\u{1b}]52;c;cHduZWQ=\u{7}\u{1b}[2J\nEVIL\t\u{9b}31m";

    /// Seed a [`LiveState`] on the Chain screen from an assembled [`ChainFetch`]
    /// (the adapter-seam output), with a Live source and a fixed as-of instant.
    fn live_from_fetch(fetch: ChainFetch, load: ScreenLoad) -> LiveState {
        let store = ChainStore::seed(fetch, ChainSource::Merged, Duration::from_secs(2), utc(EXP));
        let mut live = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store,
        );
        live.load = load;
        live
    }

    /// Draw the chain body for `live` into a fixed 120x40 `TestBackend` (tick 0,
    /// so the loading spinner frame is fixed) and return the buffer as golden text.
    #[track_caller]
    fn render_chain_golden(live: &LiveState) -> String {
        use crate::ui::golden::{GOLDEN_HEIGHT, GOLDEN_WIDTH, buffer_to_text};
        let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
        match term.draw(|frame| draw(live, frame, frame.area(), theme(), 0, utc(EXP))) {
            Ok(_) => {}
            Err(e) => panic!("golden draw failed: {e}"),
        }
        buffer_to_text(term.backend().buffer())
    }

    #[test]
    fn test_chain_deribit_btc_atm_render_golden() {
        // The populated matrix, assembled from the recorded Deribit fixture through
        // the real adapter seam (fixture -> normalize -> assemble -> ChainStore ->
        // chain::draw).
        let fetch = crate::providers::deribit::fixture_btc_chain_fetch_named("BTC");
        let live = live_from_fetch(fetch, ScreenLoad::Ready);
        let text = render_chain_golden(&live);
        crate::ui::golden::assert_golden("chain", "deribit_btc_atm.txt", &text);
    }

    #[test]
    fn test_chain_loading_render_golden() {
        // The pre-first-frame LOADING state: the vertically-centered spinner +
        // "connecting to deribit". tick 0 fixes the spinner frame, so it is stable.
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let live = live_with(empty, ScreenLoad::Loading);
        let text = render_chain_golden(&live);
        crate::ui::golden::assert_golden("chain", "loading.txt", &text);
    }

    #[test]
    fn test_chain_empty_render_golden() {
        // The EMPTY-Ready state (distinct from loading): "no data for BTC
        // 2025-06-27" + "no strikes yet - press r to reconnect".
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let live = live_with(empty, ScreenLoad::Ready);
        let text = render_chain_golden(&live);
        crate::ui::golden::assert_golden("chain", "empty.txt", &text);
    }

    #[test]
    fn test_chain_provider_error_render_golden() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Loading);
        live.load = ScreenLoad::Error {
            message: "provider unreachable".to_owned(),
        };
        let text = render_chain_golden(&live);
        crate::ui::golden::assert_golden("chain", "provider_error.txt", &text);
    }

    #[test]
    fn test_chain_stale_render_golden() {
        // The stale-feed state (a #18 acceptance criterion): the last chain still
        // renders (dimmed) with a `◐ stale` badge in the title — never blanked,
        // never shown as live. Deterministic (a fixed `since` instant).
        let fetch = crate::providers::deribit::fixture_btc_chain_fetch_named("BTC");
        let mut live = live_from_fetch(fetch, ScreenLoad::Ready);
        live.source.health = StreamHealth::Stale { since: utc(EXP) };
        live.store
            .apply_health(StreamHealth::Stale { since: utc(EXP) });
        let text = render_chain_golden(&live);
        crate::ui::golden::assert_golden("chain", "stale.txt", &text);
    }

    #[test]
    fn test_chain_escape_hygiene_render_golden_renders_inert_text() {
        // A hostile venue-controlled symbol flows through the real adapter seam
        // (the domain keeps the bytes verbatim) into the rendered matrix title;
        // the render edge neutralizes it to inert visible text. The committed
        // golden proves it and carries NO raw escape byte.
        let fetch = crate::providers::deribit::fixture_btc_chain_fetch_named(HOSTILE_SYMBOL);
        let live = live_from_fetch(fetch, ScreenLoad::Ready);
        let text = render_chain_golden(&live);
        assert!(
            !text.contains('\u{1b}'),
            "the rendered hostile symbol must carry no raw ESC byte",
        );
        assert!(
            !text.contains('\u{9b}'),
            "the rendered hostile symbol must carry no 8-bit CSI introducer",
        );
        assert!(
            !text.contains('\u{7}'),
            "the rendered hostile symbol must carry no BEL byte",
        );
        crate::ui::golden::assert_golden("chain", "escape_hygiene.txt", &text);
    }

    #[test]
    fn test_draw_hostile_symbol_renders_inert_across_sizes_without_panic() {
        // The hostile symbol renders as inert text at every size (including the
        // minimum body) — never a panic, never a residual escape/introducer byte.
        let fetch = crate::providers::deribit::fixture_btc_chain_fetch_named(HOSTILE_SYMBOL);
        let live = live_from_fetch(fetch, ScreenLoad::Ready);
        for (w, h) in [(40u16, 8u16), (80, 24), (120, 40), (200, 60)] {
            let text = rendered(&live, w, h);
            assert!(!text.contains('\u{1b}'), "no ESC byte at {w}x{h}");
            assert!(!text.contains('\u{9b}'), "no 8-bit CSI at {w}x{h}");
        }
    }
}
