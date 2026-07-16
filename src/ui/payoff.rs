//! The payoff-diagram screen — the multi-leg builder, its states, and the payoff
//! **curve** (`docs/05-views-and-ux.md` §3, §4, §6).
//!
//! # Scope: the builder interaction (#26) and the curve render (#27)
//!
//! [`handle_key`] drives the application-layer
//! [`PayoffBuilder`](crate::app::PayoffBuilder) state machine (append the chain's
//! focused leg, edit the cursor leg, validate + commit, discard, toggle the curve
//! mode). [`draw`] renders the builder's **states** first — the empty hint, the
//! in-progress leg list, the inline per-leg validation errors — and then, once a
//! strategy commits, the payoff **line chart** (the expiration or t+0 curve the `t`
//! toggle selects) with the break-even points and current spot marked, or an honest
//! "curve unavailable" state when the committed legs cannot be priced. States FIRST,
//! never a blank or a panic.
//!
//! # The draw path is pure
//!
//! [`draw`] takes `&LiveState` (never `&mut`), the pre-projected
//! [`GraphProjection`](crate::ui::graph::GraphProjection) (built **off** the draw
//! path by [`ViewState::sync`](crate::ViewState)), and the `Copy` resolved
//! [`Theme`]. It reads the cached projection and the borrowed
//! [`OptionChain`](optionstratlib::chains::chain::OptionChain) marks at draw time —
//! no `GraphData` build, no pricing, no I/O, no state mutation
//! (`docs/02-tui-architecture.md` §7). [`handle_key`] is pure over `&mut LiveState`:
//! it mutates the builder, performs no I/O, and never `.await`s.
//!
//! # Color is never the only signal
//!
//! The side carries a `BUY`/`SELL` text label, the cursor leg a `▸` glyph + bold, a
//! validation error a leading `!`, the committed strategy a `✓` glyph, and the chart
//! its spot/break-even values as text plus distinct marker shapes — all legible
//! under `NO_COLOR` (`docs/05-views-and-ux.md` §7).

use crossterm::event::KeyEvent;
use optionstratlib::OptionStyle;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::Positive;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph};

use crate::app::atm_index_of;
use crate::app::keymap::{KeyChord, PayoffAction, resolve_payoff};
use crate::app::{BuilderLeg, CurveMode, LegFocus, LiveState, PayoffBuilder, ReplayState, Side};
use crate::event::AppEvent;
use crate::ui::graph::{EmptyReason, GraphProjection, ProjectedSeries};
use crate::ui::theme::{StrikeRelation, Theme};

// ===========================================================================
// The draw entry point + the builder states (states first).
// ===========================================================================

/// Draw the live multi-leg payoff screen for `state` into `area` — a pure render
/// over the borrowed builder, chain, and the pre-projected payoff `payoff`
/// (`docs/02-tui-architecture.md` §7). `payoff` is the ui view-cache's projection,
/// computed **off** the draw path by [`ViewState::sync`](crate::ViewState); this
/// paint builds no `GraphData` and prices nothing.
///
/// States first (`docs/05-views-and-ux.md` §6): the **empty** hint ("add a leg with
/// `a`"), then the in-progress **leg list** with the cursor leg marked and each
/// leg's current mark, then any inline per-leg **validation errors**, and — once a
/// strategy commits — the payoff **line chart** (the [`curve`](crate::app::CurveMode)
/// the `t` toggle selects) with the break-even points and the current spot marked,
/// or an honest "curve unavailable" state when the committed legs could not be
/// priced. Never a blank, never a panic.
pub fn draw(
    state: &LiveState,
    payoff: &GraphProjection,
    frame: &mut Frame,
    area: Rect,
    theme: Theme,
) {
    let builder = &state.payoff_builder;
    let chain = state.store.chain();

    let title = Line::from(vec![
        Span::styled("Payoff", theme.accent()),
        Span::styled(format!("  {}", builder.curve().label()), theme.dim()),
    ]);
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Empty state: no legs and nothing committed — the honest "add a leg" hint,
    // vertically centered so it reads as a deliberate state (§6).
    if builder.is_empty() && builder.committed().is_none() {
        draw_empty(frame, inner, theme);
        return;
    }

    // Committed state: the payoff line chart (or an honest "curve unavailable"
    // state), never the in-progress leg list.
    if builder.committed().is_some() {
        match payoff.ready() {
            Some(series) => draw_committed_chart(frame, inner, theme, builder, chain, series),
            None => {
                draw_curve_unavailable(frame, inner, theme, builder, chain, payoff.empty_reason())
            }
        }
        return;
    }

    // In-progress / invalid builder states (the leg list + inline errors, §3, §6).
    draw_builder(frame, inner, theme, builder, chain);
}

/// Draw the in-progress (or invalid) builder: the leg list with the cursor leg
/// marked and each leg's current mark, then any inline per-leg validation errors.
fn draw_builder(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    builder: &PayoffBuilder,
    chain: &OptionChain,
) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(builder.legs().len() + 3);
    let cursor = builder.cursor();
    for (idx, leg) in builder.legs().iter().enumerate() {
        lines.push(leg_line(leg, idx == cursor, leg.mark_in(chain), theme));
    }
    if !builder.errors().is_empty() {
        lines.push(Line::from(""));
        for err in builder.errors() {
            lines.push(Line::from(vec![
                Span::styled("! ", theme.warning()),
                Span::styled(err.to_string(), theme.warning()),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Draw the committed payoff **line chart**: a two-line header (the committed leg
/// count, the current spot, and the break-even prices as text — legible under
/// `NO_COLOR`) over a ratatui [`Chart`] of the projected series, with a dim zero
/// reference line, the break-even points, and the current spot overlaid as markers
/// (`docs/05-views-and-ux.md` §4). The markers are resolved at the UI edge from the
/// app's cached break-even set and the chain's spot — nothing is recomputed here.
fn draw_committed_chart(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    builder: &PayoffBuilder,
    chain: &OptionChain,
    series: &ProjectedSeries,
) {
    let [header, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(committed_header(theme, builder, chain)),
        header,
    );

    let [x_min, x_max] = series.x_bounds();
    // A dim zero reference line across the x-range, so break-evens read visually.
    let zero_line = [(x_min, 0.0), (x_max, 0.0)];
    // Break-even markers on the zero line, kept in-range and finite.
    let breakevens: Vec<(f64, f64)> = builder
        .break_even_points()
        .iter()
        .map(Positive::to_f64)
        .filter(|x| x.is_finite() && *x >= x_min && *x <= x_max)
        .map(|x| (x, 0.0))
        .collect();
    // The current-spot marker on the zero line, when finite and in-range.
    let spot = chain.underlying_price.to_f64();
    let spot_pts: Vec<(f64, f64)> = if spot.is_finite() && spot >= x_min && spot <= x_max {
        vec![(spot, 0.0)]
    } else {
        Vec::new()
    };

    let mut datasets = vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme.dim())
            .data(&zero_line),
        Dataset::default()
            .name(series.name().to_owned())
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme.accent())
            .data(series.points()),
    ];
    if !breakevens.is_empty() {
        datasets.push(
            Dataset::default()
                .name("break-even")
                .marker(Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(theme.warning())
                .data(&breakevens),
        );
    }
    if !spot_pts.is_empty() {
        datasets.push(
            // The spot marker uses a distinct `Block` marker SHAPE (vs the break-even
            // `Dot`) so it reads under NO_COLOR — shape, not color, differentiates it:
            // the "at spot" style and `warning()` both resolve to the same yellow+bold
            // tint (ux P3-01).
            Dataset::default()
                .name("spot")
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(theme.strike_relation_style(StrikeRelation::AtSpot))
                .data(&spot_pts),
        );
    }

    // Extend the DRAWN y-bounds to include 0 so the zero reference line, the spot
    // marker, and the break-even markers — all pinned to y=0 — never clip when the
    // P&L window sits entirely above or below zero (common for a fresh position's t+0
    // curve, ux P2-01). Payoff-local: the generic `graph.rs` adapter is untouched (a
    // #47 vol smile must not be forced through 0). The projection's cached labels are
    // for the series' own bounds, so regenerate the y-axis labels for the widened
    // range at this UI edge or they would misalign.
    let y_bounds = y_bounds_including_zero(series.y_bounds());
    let y_labels = payoff_y_labels(y_bounds);

    let chart = Chart::new(datasets)
        .x_axis(
            Axis::default()
                .title("underlying")
                .bounds(series.x_bounds())
                .labels(axis_labels(series.x_labels()))
                .style(theme.dim()),
        )
        .y_axis(
            Axis::default()
                .title("P&L")
                .bounds(y_bounds)
                .labels(y_labels)
                .style(theme.dim()),
        );
    frame.render_widget(chart, body);
}

/// Extend the P&L series' y-bounds to include `0` (ux P2-01). The zero reference
/// line, the current-spot marker, and the break-even markers are all pinned to
/// `y = 0`; when the P&L window sits entirely above or below zero (common for a
/// fresh position's t+0 curve) ratatui would clip those y=0 overlays. Widening the
/// **drawn** bounds keeps them on screen. Payoff-local — the generic `graph.rs`
/// adapter is deliberately not widened (a #47 vol smile must not be forced through
/// 0). Both endpoints are finite (post-`finite_xy`), so the `min`/`max` are total.
#[must_use]
fn y_bounds_including_zero(bounds: [f64; 2]) -> [f64; 2] {
    let [lo, hi] = bounds;
    [lo.min(0.0), hi.max(0.0)]
}

/// Regenerate the `[min, mid, max]` y-axis labels for the payoff-local **widened**
/// y-bounds (the projection's cached labels are computed on the series' own bounds,
/// so a widened range would misalign them). Two-decimal, matching the projection's
/// numeric style (`graph.rs`).
#[must_use]
fn payoff_y_labels(bounds: [f64; 2]) -> Vec<Span<'static>> {
    let [min, max] = bounds;
    let mid = min + (max - min) / 2.0;
    vec![
        Span::raw(format!("{min:.2}")),
        Span::raw(format!("{mid:.2}")),
        Span::raw(format!("{max:.2}")),
    ]
}

/// Draw the honest "committed, but the curve can't be priced" state: the committed
/// header over a warning message keyed on the empty-projection reason, then the leg
/// list — never a blank and never a fabricated chart (`docs/05-views-and-ux.md` §4,
/// §6). A committed strategy whose legs lack an IV or a future expiry lands here.
fn draw_curve_unavailable(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    builder: &PayoffBuilder,
    chain: &OptionChain,
    reason: Option<EmptyReason>,
) {
    let mut lines = committed_header(theme, builder, chain);
    lines.push(Line::from(vec![
        Span::styled("! ", theme.warning()),
        Span::styled(
            unavailable_reason(builder.curve(), builder.has_expiration_curve(), reason),
            theme.warning(),
        ),
    ]));
    lines.push(Line::from(""));
    let cursor = builder.cursor();
    for (idx, leg) in builder.legs().iter().enumerate() {
        lines.push(leg_line(leg, idx == cursor, leg.mark_in(chain), theme));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The two-line committed header: the `✓` leg-count summary, then the current spot
/// and the break-even prices as text (so the numbers survive `NO_COLOR`).
#[must_use]
fn committed_header(
    theme: Theme,
    builder: &PayoffBuilder,
    chain: &OptionChain,
) -> Vec<Line<'static>> {
    let n = builder.legs().len();
    let summary = Line::from(vec![
        Span::styled("✓ ", theme.accent()),
        Span::styled(format!("committed {n} {}", leg_word(n)), theme.accent()),
        Span::styled(format!("   curve {}", builder.curve().label()), theme.dim()),
    ]);
    let marks = Line::from(vec![
        Span::styled("spot ", theme.dim()),
        Span::raw(fmt_price(Some(chain.underlying_price))),
        Span::styled("   break-even ", theme.dim()),
        Span::raw(fmt_break_evens(builder.break_even_points())),
    ]);
    vec![summary, marks]
}

/// The message for a committed strategy with no renderable curve, keyed on the
/// active [`CurveMode`], whether the (IV-independent) expiration curve renders, and
/// the projection's [`EmptyReason`] (exhaustive, no wildcard) — an honest reason,
/// never a fabricated line.
///
/// The **t+0** curve alone can be unavailable purely for lack of a reliable IV
/// (a leg's IV is absent or a sub-plausibility local inversion) while the expiration
/// curve still renders — a specific, honest state (#27 SF-2). Otherwise the generic
/// message reflects the expiration requirement (marks + a future expiry).
#[must_use]
fn unavailable_reason(
    curve: CurveMode,
    expiration_available: bool,
    reason: Option<EmptyReason>,
) -> &'static str {
    if curve == CurveMode::TPlus0 && expiration_available {
        return "t+0 unavailable — no reliable IV";
    }
    match reason {
        Some(EmptyReason::NoData) => "payoff curve unavailable — needs marks and a future expiry",
        Some(EmptyReason::Degenerate) => "payoff curve unavailable — degenerate geometry",
        Some(EmptyReason::Unsupported) => "payoff curve unavailable — unsupported geometry",
        None => "payoff curve unavailable",
    }
}

/// Format the break-even prices as `b1, b2` text, or `—` when there are none — the
/// `—`-not-`0` honesty rule. Each value uses the SAME [`fmt_price`] formatter as the
/// header's spot value, so adjacent numbers render consistently (ux P3-03).
#[must_use]
fn fmt_break_evens(points: &[Positive]) -> String {
    if points.is_empty() {
        return EM_DASH.to_owned();
    }
    points
        .iter()
        .map(|p| fmt_price(Some(*p)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wrap the precomputed axis-label strings as owned [`Span`]s for
/// [`Axis::labels`](ratatui::widgets::Axis::labels) — no per-frame numeric
/// formatting (the labels were computed off the draw path on the projection).
#[must_use]
fn axis_labels(labels: &[String]) -> Vec<Span<'static>> {
    labels
        .iter()
        .map(|label| Span::raw(label.clone()))
        .collect()
}

/// Draw the empty builder state — the "add a leg with `a`" hint, vertically and
/// horizontally centered in `inner` so it reads as a deliberate state, never a
/// blank void (`docs/05-views-and-ux.md` §6).
fn draw_empty(frame: &mut Frame, inner: Rect, theme: Theme) {
    let text = Text::from(vec![
        Line::from(Span::styled("add a leg with `a`", theme.accent())),
        Line::from(Span::styled(
            "on the chain: focus c / p, then a",
            theme.dim(),
        )),
        Line::from(Span::styled(
            "here: a adds the focused (or ATM) strike",
            theme.dim(),
        )),
    ]);
    let height = u16::try_from(text.height())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

/// One builder-leg line: `[▸] SIDE  qty×  strike C/P   mark <m>`. The cursor leg
/// carries a `▸` glyph and bold weight; the side is a `BUY`/`SELL` text label tinted
/// green (buy) / red (sell), and the mark renders `—` when unknown — all legible
/// under `NO_COLOR`.
#[must_use]
fn leg_line(
    leg: &BuilderLeg,
    is_cursor: bool,
    mark: Option<Positive>,
    theme: Theme,
) -> Line<'static> {
    let cursor_marker = if is_cursor { "▸ " } else { "  " };
    let spans = vec![
        Span::raw(cursor_marker.to_owned()),
        // The side is red for a sell (short), green for a buy (long); the BUY/SELL
        // text carries the signal under NO_COLOR.
        Span::styled(
            format!("{:<4}", leg.side.label()),
            theme.pnl_style(leg.side == Side::Sell),
        ),
        Span::raw(format!(
            " {}× {} {}   mark {}",
            leg.qty,
            fmt_strike(leg.strike),
            style_glyph(leg.style),
            fmt_price(mark),
        )),
    ];
    let line = Line::from(spans);
    if is_cursor {
        line.style(Style::new().add_modifier(Modifier::BOLD))
    } else {
        line
    }
}

/// The singular/plural word for a leg count.
#[must_use]
fn leg_word(count: usize) -> &'static str {
    if count == 1 { "leg" } else { "legs" }
}

/// The single-letter style glyph (`C` call / `P` put), exhaustive with no wildcard.
#[must_use]
fn style_glyph(style: OptionStyle) -> &'static str {
    match style {
        OptionStyle::Call => "C",
        OptionStyle::Put => "P",
    }
}

/// Format a strike, trailing zeros stripped, or `—` for the non-finite sentinel.
#[must_use]
fn fmt_strike(strike: Positive) -> String {
    if strike == Positive::INFINITY {
        return EM_DASH.to_owned();
    }
    format!("{}", strike.round_to(2))
}

/// Format a mark to two decimals, or `—` when absent — the `—`-not-`0` honesty rule
/// (`docs/01-domain-model.md` §8), guarding the `Positive` infinity sentinel so a
/// non-finite value never paints.
#[must_use]
fn fmt_price(value: Option<Positive>) -> String {
    match value {
        Some(price) if price != Positive::INFINITY => format!("{price:.2}"),
        _ => EM_DASH.to_owned(),
    }
}

/// The em dash rendered for an unknown value — never a fabricated `0`.
const EM_DASH: &str = "—";

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a live-payoff-local key, returning any follow-on [`AppEvent`]
/// (`docs/02-tui-architecture.md` §9). Pure over `&mut LiveState` — no I/O, no
/// pricing, no `.await`.
///
/// The chord resolves **through the single keybinding map**
/// ([`resolve_payoff`](crate::resolve_payoff), `src/app/keymap.rs`), so the builder
/// dispatch and the help overlay read one table and cannot drift. Each action drives
/// the application-layer [`PayoffBuilder`](crate::app::PayoffBuilder): `a` appends
/// the chain's focused leg, `x` removes the cursor leg, `+`/`-` change the cursor
/// leg's quantity (the concrete direction read from the shared chord), `s` toggles
/// its side, `Enter` validates + commits, `Esc` discards, and `t` toggles the curve
/// mode. Every mutation is local state, so `handle_key` returns `None`; the render
/// loop detects the builder's revision change and redraws
/// (`docs/02-tui-architecture.md` §8).
#[must_use]
pub fn handle_key(state: &mut LiveState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_payoff(chord)? {
        PayoffAction::AddLeg => {
            append_focused_leg(state);
            None
        }
        PayoffAction::RemoveLeg => {
            state.payoff_builder.remove_cursor();
            None
        }
        PayoffAction::Quantity => {
            adjust_qty(state, chord);
            None
        }
        PayoffAction::ToggleSide => {
            state.payoff_builder.toggle_cursor_side();
            None
        }
        PayoffAction::Commit => {
            commit(state);
            None
        }
        PayoffAction::Cancel => {
            state.payoff_builder.discard();
            None
        }
        PayoffAction::ToggleCurve => {
            state.payoff_builder.toggle_curve();
            None
        }
    }
}

/// Append the chain's currently-focused leg (`a`): the cursor strike + focused
/// call/put, long by default. A leg with no focused row yet falls back to the
/// nearest-spot strike ([`atm_index_of`]), so a fresh Payoff screen still builds a
/// sensible leg; an empty chain appends nothing.
///
/// Shared with the **chain** screen (`src/ui/chain.rs`): the chain-side `a`
/// (`ChainAction::AddLeg`) calls this same helper so the headline gesture — focus a
/// strike with `c`/`p`, press `a` to add it to the builder — appends through one code
/// path (ui→ui is allowed). A successful append bumps the builder revision (via
/// [`PayoffBuilder::append`](crate::app::PayoffBuilder)), which the driver's
/// `live_view_sig` diffs to mark the frame dirty.
pub(crate) fn append_focused_leg(state: &mut LiveState) {
    let chain = state.store.chain();
    let len = chain.options.len();
    if len == 0 {
        return;
    }
    let row = match state.selection.focused_row {
        Some(row) if row < len => Some(row),
        // No cursor yet (or one stale past a shrunk chain): fall back to ATM.
        _ => atm_index_of(chain),
    };
    let Some(row) = row else {
        return;
    };
    let Some(od) = chain.options.iter().nth(row) else {
        return;
    };
    let strike = od.strike_price;
    let style = style_of(state.selection.focused_leg);
    state.payoff_builder.append(strike, style);
}

/// Increment or decrement the cursor leg's quantity, read from the concrete chord —
/// the shared [`PayoffAction::Quantity`] binds both `+` and `-`.
fn adjust_qty(state: &mut LiveState, chord: KeyChord) {
    match chord {
        KeyChord::Char('+') => state.payoff_builder.increment_qty(),
        // The Quantity action binds only `+`/`-`; `-` (and any defensive fallback)
        // decrements.
        _ => state.payoff_builder.decrement_qty(),
    }
}

/// Validate + commit the built strategy (`Enter`) against the current store
/// snapshot. Uses disjoint field borrows (`store` read immutably, `payoff_builder`
/// mutated) so the validation and the off-draw geometry build (the payoff series +
/// break-evens, sampled from `optionstratlib`) read the in-memory store — no I/O.
fn commit(state: &mut LiveState) {
    let LiveState {
        store,
        payoff_builder,
        ..
    } = state;
    // Freshness (#26) is derived inside commit from the store's stream-quote
    // receipt clocks + the analytics reference instant, so a leg whose feed died
    // is rejected with StaleMark; the geometry build (#27) also runs there, off
    // the draw path. Still a pure read - no I/O, no wall clock.
    let _ = payoff_builder.commit(store);
}

/// The [`OptionStyle`] of a focused leg, exhaustive with no wildcard.
#[must_use]
fn style_of(leg: LegFocus) -> OptionStyle {
    match leg {
        LegFocus::Call => OptionStyle::Call,
        LegFocus::Put => OptionStyle::Put,
    }
}

// ===========================================================================
// Replay payoff (v0.5) — unchanged pure seams.
// ===========================================================================

/// Draw the replay payoff (the open position at the head) for `state` into `area`
/// — a pure render. Placeholder body until v0.5 (`docs/ROADMAP.md`).
pub fn draw_replay(_state: &ReplayState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(
        frame,
        area,
        "Payoff",
        "replay payoff at the head lands in v0.5",
    );
}

/// Handle a replay-payoff-local key, returning any follow-on [`AppEvent`]. Pure —
/// no I/O. Lands with the replay payoff (v0.5).
#[must_use]
pub fn handle_key_replay(_state: &mut ReplayState, _key: KeyEvent) -> Option<AppEvent> {
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::OptionStyle;
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::{Positive, ToPrimitive};
    use optionstratlib::visualization::GraphData;
    use proptest::prelude::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::{draw, handle_key, payoff_y_labels, y_bounds_including_zero};
    use crate::app::{BuilderLeg, CurveMode, LegError, LegFocus, LiveState, Side, SourceBinding};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ContractSpecFingerprint, ExerciseStyle,
        ExpirySource, GreeksOrigin, GreeksRow, Instrument, InstrumentKey, ProviderId,
        SettlementStyle, StreamHealth,
    };
    use crate::config::ThemeChoice;
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crate::ui::graph::{GraphProjection, project};
    use crate::ui::theme::Theme;

    /// Project the builder's active payoff series exactly as the ui view-cache
    /// would (off the draw path) — the `&GraphProjection` the screen's `draw`
    /// reads.
    #[must_use]
    fn projection(state: &LiveState) -> GraphProjection {
        project(state.payoff_builder.active_graph_data())
    }

    const EXP: i64 = 1_700_000_000;
    /// A strike present in the chain WITH a mark.
    const FULL_A: f64 = 60_000.0;
    /// A second strike present in the chain WITH a mark.
    const FULL_B: f64 = 62_000.0;
    /// A strike present in the chain but WITHOUT a mark (no bids/asks).
    const BARE: f64 = 64_000.0;

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

    /// A strike row with both call/put mids populated at **realistic** BTC premiums
    /// (thousands of USD), so the store's local IV inversion lands well above the
    /// [`MIN_PLAUSIBLE_LOCAL_IV`](crate::chain::MIN_PLAUSIBLE_LOCAL_IV) floor (~10%)
    /// and the t+0 curve renders. A tiny premium (a few USD at a 60 000 strike) would
    /// invert to a sub-0.5% garbage IV, which #27 SF-2 deliberately gates out — see
    /// [`subfloor_row`].
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

    /// A strike row with marks present but **tiny** premiums (and an absent chain IV
    /// sentinel), so the store's local IV inversion is a sub-0.5% `ComputedLocally`
    /// value the #27 SF-2 floor rejects: the expiration curve (mark-only) still
    /// renders while the t+0 curve degrades to "unavailable — no reliable IV".
    fn subfloor_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: Positive::ZERO,
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    /// A strike row with no quotes, so `call_middle`/`put_middle` stay `None` — the
    /// "no mark" case validation rejects.
    fn bare_row(strike: f64) -> OptionData {
        OptionData {
            strike_price: pos(strike),
            ..Default::default()
        }
    }

    fn chain() -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(FULL_A), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(full_row(FULL_A));
        let _ = chain.options.insert(full_row(FULL_B));
        let _ = chain.options.insert(bare_row(BARE));
        chain
    }

    fn store() -> ChainStore {
        ChainStore::seed(
            ChainFetch::new(
                chain(),
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
            .greeks(GreeksCapability::Provided)
            .build()
    }

    fn live_state() -> LiveState {
        LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store(),
        )
    }

    /// A chain whose expiration is in the PAST relative to the seed instant, so the
    /// payoff geometry cannot be priced (a non-positive DTE) even though the marks
    /// are present — the "curve unavailable" degradation fixture.
    fn past_chain() -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(FULL_A), "2020-01-01".to_owned(), None, None);
        let _ = chain.options.insert(full_row(FULL_A));
        let _ = chain.options.insert(full_row(FULL_B));
        chain
    }

    fn past_live_state() -> LiveState {
        let store = ChainStore::seed(
            ChainFetch::new(
                past_chain(),
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        );
        LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store,
        )
    }

    /// A live state whose FULL_A leg's only IV source is a sub-0.5% LOCAL inversion
    /// (tiny premiums + an absent chain IV) — the #27 SF-2 fixture where expiration
    /// renders but the t+0 curve is honestly unavailable.
    fn subfloor_live_state() -> LiveState {
        let mut chain = OptionChain::new("BTC", pos(FULL_A), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(subfloor_row(FULL_A));
        let _ = chain.options.insert(subfloor_row(FULL_B));
        let store = ChainStore::seed(
            ChainFetch::new(
                chain,
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        );
        LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store,
        )
    }

    /// A store identical to [`store`] except the FULL_A **call** mark is ~500 higher
    /// (mid 3050 → 3550) — the "the quote moved up" snapshot the SF-1 test swaps in to
    /// drive a mark change (P0→P1) after commit.
    fn higher_mark_store() -> ChainStore {
        let mut a = full_row(FULL_A);
        a.call_bid = Some(pos(3_500.0));
        a.call_ask = Some(pos(3_600.0));
        a.set_mid_prices();
        let mut chain = OptionChain::new("BTC", pos(FULL_A), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(a);
        let _ = chain.options.insert(full_row(FULL_B));
        let _ = chain.options.insert(bare_row(BARE));
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

    /// The absolute expiry the test chains resolve to (the parser maps the
    /// `"2025-06-27"` string to an absolute instant), so an injected greeks row keys
    /// the SAME sidecar entry the payoff resolution reads — not the `ExpirySource`'s
    /// `utc(EXP)`.
    #[track_caller]
    fn resolved_expiry() -> DateTime<Utc> {
        let chain = OptionChain::new("BTC", pos(FULL_A), "2025-06-27".to_owned(), None, None);
        match chain.get_expiration() {
            Some(optionstratlib::ExpirationDate::DateTime(dt)) => dt,
            other => panic!("expected an absolute-UTC chain expiry, got {other:?}"),
        }
    }

    /// The identity for a FULL_A leg, so a test can inject a venue [`GreeksRow`] the
    /// store folds into its style-keyed sidecar.
    fn instrument(strike: f64, style: OptionStyle) -> Instrument {
        Instrument {
            key: InstrumentKey {
                underlying: "BTC".to_owned(),
                expiration_utc: resolved_expiry(),
                strike: pos(strike),
                style,
            },
            provider: pid("deribit"),
            native_symbol: format!("BTC-{strike}-{style:?}"),
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

    /// A venue (`Provider`-origin) [`GreeksRow`] carrying only `iv` — the store keeps
    /// a venue IV verbatim across its recompute (venue wins), so this survives to gate
    /// the sidecar IV origin under test.
    fn venue_iv_row(strike: f64, style: OptionStyle, iv: f64) -> GreeksRow {
        GreeksRow {
            instrument: instrument(strike, style),
            iv: Some(pos(iv)),
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(EXP),
        }
    }

    /// The y (P&L) at the grid sample nearest `target` in a committed curve's cached
    /// `GraphData::Series` — a read of the projected geometry, never a recompute. The
    /// grid snaps every strike (FIX 5), so a strike/spot `target` hits an exact point.
    #[track_caller]
    fn series_y_near(graph: &GraphData, target: f64) -> f64 {
        match graph {
            GraphData::Series(series) => {
                let mut best: Option<(f64, f64)> = None;
                for (x, y) in series.x.iter().zip(series.y.iter()) {
                    let xf = x.to_f64().unwrap_or(f64::MAX);
                    let yf = y.to_f64().unwrap_or(0.0);
                    let dist = (xf - target).abs();
                    if best.is_none_or(|(bd, _)| dist < bd) {
                        best = Some((dist, yf));
                    }
                }
                match best {
                    Some((_, y)) => y,
                    None => panic!("expected a non-empty series"),
                }
            }
            other => panic!("expected a Series, got {other:?}"),
        }
    }

    fn press(state: &mut LiveState, code: KeyCode) {
        let _ = handle_key(state, KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// The leg at `idx` (a `Copy` [`BuilderLeg`]), via `.get()` — never an unchecked
    /// index (per the ruleset).
    #[track_caller]
    fn nth_leg(state: &LiveState, idx: usize) -> BuilderLeg {
        match state.payoff_builder.legs().get(idx) {
            Some(leg) => *leg,
            None => panic!("expected a leg at index {idx}"),
        }
    }

    /// Focus `row` on the chain so `a` appends that strike.
    fn focus(state: &mut LiveState, row: usize) {
        state.selection.focused_row = Some(row);
    }

    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        }
    }

    #[track_caller]
    fn render(state: &LiveState, width: u16, height: u16) {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let payoff = projection(state);
        let mut term = terminal(width, height);
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, &payoff, frame, area, theme)) {
            Ok(_) => {}
            Err(e) => panic!("payoff draw failed: {e}"),
        }
    }

    // --- `a` appends the focused leg, in order -------------------------------

    #[test]
    fn test_add_leg_appends_focused_strike_and_style_in_order() {
        let mut state = live_state();
        // Focus the first strike as a call, add it.
        focus(&mut state, 0);
        state.selection.focused_leg = LegFocus::Call;
        press(&mut state, KeyCode::Char('a'));
        // Focus the second strike as a put, add it.
        focus(&mut state, 1);
        state.selection.focused_leg = LegFocus::Put;
        press(&mut state, KeyCode::Char('a'));

        assert_eq!(state.payoff_builder.legs().len(), 2, "two legs appended");
        assert_eq!(
            nth_leg(&state, 0).strike,
            pos(FULL_A),
            "first leg is the first focus"
        );
        assert_eq!(nth_leg(&state, 0).style, OptionStyle::Call);
        assert_eq!(
            nth_leg(&state, 1).strike,
            pos(FULL_B),
            "second leg is the second focus"
        );
        assert_eq!(nth_leg(&state, 1).style, OptionStyle::Put);
        // The cursor tracks the most recently added leg.
        assert_eq!(state.payoff_builder.cursor(), 1);
        // A fresh leg is long, quantity one.
        assert_eq!(nth_leg(&state, 1).side, Side::Buy);
        assert_eq!(nth_leg(&state, 1).qty, 1);
    }

    #[test]
    fn test_add_leg_with_no_focus_falls_back_to_atm() {
        let mut state = live_state();
        // No focused row: `a` still appends the nearest-spot strike (FULL_A = spot).
        assert!(state.selection.focused_row.is_none());
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(state.payoff_builder.legs().len(), 1);
        assert_eq!(nth_leg(&state, 0).strike, pos(FULL_A));
    }

    // --- `x`/`+`/`-`/`s` edit ONLY the cursor leg ----------------------------

    #[test]
    fn test_edits_touch_only_the_cursor_leg() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // leg 0
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // leg 1 (cursor here)

        // `+` twice, `s` once — all on the cursor leg (index 1).
        press(&mut state, KeyCode::Char('+'));
        press(&mut state, KeyCode::Char('+'));
        press(&mut state, KeyCode::Char('s'));

        assert_eq!(nth_leg(&state, 0).qty, 1, "leg 0 quantity is untouched");
        assert_eq!(
            nth_leg(&state, 0).side,
            Side::Buy,
            "leg 0 side is untouched"
        );
        assert_eq!(
            nth_leg(&state, 1).qty,
            3,
            "only the cursor leg's quantity changed"
        );
        assert_eq!(
            nth_leg(&state, 1).side,
            Side::Sell,
            "only the cursor leg's side toggled"
        );

        // `-` once brings the cursor leg back to 2.
        press(&mut state, KeyCode::Char('-'));
        assert_eq!(nth_leg(&state, 1).qty, 2);
    }

    #[test]
    fn test_remove_cursor_leg_keeps_cursor_in_bounds() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // leg 0
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // leg 1 (cursor)
        assert_eq!(state.payoff_builder.cursor(), 1);

        // Remove the cursor leg: it steps back to the new last leg.
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.payoff_builder.legs().len(), 1);
        assert_eq!(
            state.payoff_builder.cursor(),
            0,
            "cursor clamps into bounds"
        );
        assert_eq!(nth_leg(&state, 0).strike, pos(FULL_A));

        // Remove the last remaining leg → empty, cursor at 0.
        press(&mut state, KeyCode::Char('x'));
        assert!(state.payoff_builder.is_empty());
        assert_eq!(state.payoff_builder.cursor(), 0);
        // A further remove on the empty builder is a safe no-op.
        press(&mut state, KeyCode::Char('x'));
        assert!(state.payoff_builder.is_empty());
    }

    // --- `Enter` validation: valid commits, invalid does not -----------------

    #[test]
    fn test_enter_commits_a_valid_strategy() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // FULL_A, has a mark
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // FULL_B, has a mark
        press(&mut state, KeyCode::Enter);

        match state.payoff_builder.committed() {
            Some(committed) => assert_eq!(committed.legs().len(), 2),
            None => panic!("a valid strategy must commit"),
        }
        assert!(
            state.payoff_builder.errors().is_empty(),
            "no errors on commit"
        );
    }

    #[test]
    fn test_validate_rejects_a_stale_stream_mark_with_stale_mark() {
        use crate::chain::{InstrumentKey, QuoteClocks};
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // FULL_A, has a mark
        let chain = state.store.chain();
        let (strike, style) = match state.payoff_builder.legs().first() {
            Some(leg) => (leg.strike, leg.style),
            None => panic!("a leg was appended"),
        };
        // The leg's stream quote was received 120s before as_of - far past
        // QUOTE_STALE_AFTER (5s) - so validation must reject it with StaleMark
        // rather than committing a cached midpoint from a dead feed.
        let as_of = utc(EXP);
        let mut clocks = QuoteClocks::new();
        clocks.insert(
            InstrumentKey {
                underlying: chain.symbol.clone(),
                expiration_utc: match chain.get_expiration() {
                    Some(optionstratlib::ExpirationDate::DateTime(dt)) => dt,
                    other => panic!("fixture chain must carry an absolute expiry, got {other:?}"),
                },
                strike,
                style,
            },
            as_of - chrono::Duration::seconds(120),
        );
        match state.payoff_builder.validate(chain, &clocks, as_of) {
            Err(errors) => assert!(
                errors
                    .iter()
                    .any(|e| matches!(e, LegError::StaleMark { idx: 0 })),
                "the stale leg reports StaleMark, got {errors:?}"
            ),
            Ok(_) => panic!("a stale-marked leg must not validate"),
        }
        // The SAME leg with no recorded clock (poll-seeded) still validates - the
        // #24 ungated convention.
        let empty = QuoteClocks::new();
        assert!(
            state.payoff_builder.validate(chain, &empty, as_of).is_ok(),
            "a leg with no stream clock is not gated"
        );
    }

    #[test]
    fn test_enter_on_empty_reports_error_and_does_not_commit() {
        let mut state = live_state();
        press(&mut state, KeyCode::Enter);
        assert!(
            state.payoff_builder.committed().is_none(),
            "empty never commits"
        );
        assert_eq!(state.payoff_builder.errors(), &[LegError::Empty]);
    }

    #[test]
    fn test_enter_on_zero_qty_reports_error_and_does_not_commit() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // qty 1, has a mark
        press(&mut state, KeyCode::Char('-')); // qty 0
        assert_eq!(nth_leg(&state, 0).qty, 0);
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_none());
        assert_eq!(
            state.payoff_builder.errors(),
            &[LegError::ZeroQty { idx: 0 }]
        );
    }

    #[test]
    fn test_enter_on_missing_mark_reports_error_and_does_not_commit() {
        let mut state = live_state();
        // Index 2 is the bare strike with no mark.
        focus(&mut state, 2);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_none());
        assert_eq!(
            state.payoff_builder.errors(),
            &[LegError::NoMark { idx: 0 }]
        );
    }

    #[test]
    fn test_edit_after_failed_commit_clears_stale_errors() {
        let mut state = live_state();
        focus(&mut state, 2);
        press(&mut state, KeyCode::Char('a')); // bare strike
        press(&mut state, KeyCode::Enter); // fails: no mark
        assert!(!state.payoff_builder.errors().is_empty());
        // Any edit clears the stale errors (and any stale commit).
        press(&mut state, KeyCode::Char('s'));
        assert!(state.payoff_builder.errors().is_empty());
    }

    #[test]
    fn test_edit_after_commit_clears_the_committed_strategy() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_some());
        // Editing invalidates the committed snapshot so #27 never draws a stale curve.
        press(&mut state, KeyCode::Char('+'));
        assert!(state.payoff_builder.committed().is_none());
    }

    // --- `Esc` discards → empty ----------------------------------------------

    #[test]
    fn test_esc_discards_to_the_empty_state() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(state.payoff_builder.legs().len(), 2);
        press(&mut state, KeyCode::Esc);
        assert!(state.payoff_builder.is_empty(), "Esc clears the strategy");
        assert!(state.payoff_builder.committed().is_none());
        assert!(state.payoff_builder.errors().is_empty());
        assert_eq!(state.payoff_builder.cursor(), 0);
    }

    // --- `t` toggles the curve mode (state lives here; render in #27) ---------

    #[test]
    fn test_toggle_curve_flips_the_mode() {
        let mut state = live_state();
        let before = state.payoff_builder.curve();
        press(&mut state, KeyCode::Char('t'));
        assert_ne!(
            state.payoff_builder.curve(),
            before,
            "`t` toggles the curve mode"
        );
        press(&mut state, KeyCode::Char('t'));
        assert_eq!(state.payoff_builder.curve(), before, "`t` toggles back");
    }

    // --- Every builder state renders without panic ----------------------------

    #[test]
    fn test_render_empty_partial_invalid_committed_states() {
        // Empty.
        let mut state = live_state();
        render(&state, 80, 24);
        // Partial (in progress).
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        render(&state, 80, 24);
        // Invalid (a failed commit with an inline error).
        focus(&mut state, 2);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        render(&state, 80, 24);
        // Committed (edit the bare leg out, then commit the valid remainder) — the
        // expiration chart renders.
        press(&mut state, KeyCode::Char('x')); // drop the bare leg (cursor on it)
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_some());
        render(&state, 120, 40);
        // Committed t+0 — the toggled curve renders too.
        press(&mut state, KeyCode::Char('t'));
        render(&state, 120, 40);
    }

    // --- #27: the committed strategy builds a renderable payoff curve ---------

    #[test]
    fn test_commit_builds_a_nonempty_payoff_series_and_ready_projection() {
        let mut state = live_state();
        focus(&mut state, 0);
        state.selection.focused_leg = LegFocus::Call;
        press(&mut state, KeyCode::Char('a')); // FULL_A call (has a mark + IV)
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // FULL_B call
        press(&mut state, KeyCode::Char('s')); // sell it → a vertical spread
        press(&mut state, KeyCode::Enter);
        assert!(
            state.payoff_builder.committed().is_some(),
            "a valid spread commits"
        );
        // The active (expiration) curve is a non-empty single Series.
        match state.payoff_builder.active_graph_data() {
            GraphData::Series(series) => {
                assert!(!series.x.is_empty(), "the payoff curve is sampled");
                assert_eq!(series.x.len(), series.y.len(), "x and y are paired");
            }
            other => panic!("expected a single Series, got {other:?}"),
        }
        assert!(
            projection(&state).ready().is_some(),
            "the committed curve projects Ready",
        );
    }

    #[test]
    fn test_toggle_curve_switches_active_series_and_bumps_graph_revision() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        let expiration = state.payoff_builder.active_graph_data().clone();
        let rev = state.payoff_builder.graph_revision();
        press(&mut state, KeyCode::Char('t')); // → t+0
        assert_ne!(
            state.payoff_builder.graph_revision(),
            rev,
            "the toggle reprojects the other curve",
        );
        let tplus0 = state.payoff_builder.active_graph_data().clone();
        assert_ne!(
            expiration, tplus0,
            "the active series switches expiration → t+0",
        );
    }

    #[test]
    fn test_cursor_edit_does_not_reproject_but_commit_and_clear_do() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        // A cursor-only edit with nothing committed does not bump graph_revision.
        let graph_rev = state.payoff_builder.graph_revision();
        press(&mut state, KeyCode::Char('+'));
        assert_eq!(
            state.payoff_builder.graph_revision(),
            graph_rev,
            "a cursor edit never reprojects the (empty) curve",
        );
        // Committing reprojects (the curve appears)…
        press(&mut state, KeyCode::Enter);
        let committed_rev = state.payoff_builder.graph_revision();
        assert_ne!(committed_rev, graph_rev, "a commit reprojects");
        // …and an edit that clears the commit reprojects back to the empty series.
        press(&mut state, KeyCode::Char('+'));
        assert_ne!(
            state.payoff_builder.graph_revision(),
            committed_rev,
            "clearing the committed curve reprojects",
        );
        match state.payoff_builder.active_graph_data() {
            GraphData::Series(series) => assert!(series.x.is_empty(), "back to the empty series"),
            other => panic!("expected the empty Series, got {other:?}"),
        }
    }

    #[test]
    fn test_refresh_tplus0_on_unchanged_snapshot_is_a_stable_noop() {
        // On the t+0 curve, re-pricing against the SAME store snapshot yields the
        // same series (deterministic), so graph_revision is unchanged — and the
        // refresh reprices the legs directly, never reconstructing a strategy.
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('t')); // → t+0
        let rev = state.payoff_builder.graph_revision();
        let LiveState {
            store,
            payoff_builder,
            ..
        } = &mut state;
        payoff_builder.refresh_tplus0(store);
        assert_eq!(
            payoff_builder.graph_revision(),
            rev,
            "an unchanged snapshot reprices to the same series (no reprojection)",
        );
    }

    #[test]
    fn test_committed_curve_unavailable_on_expired_chain_is_honest_not_fabricated() {
        // A committed strategy whose legs cannot be priced (a past expiry → a
        // non-positive DTE) yields an empty active series → an Empty projection →
        // the deliberate "curve unavailable" state, never a fabricated chart or a
        // panic. The legs still validate (their marks are present).
        let mut state = past_live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert!(
            state.payoff_builder.committed().is_some(),
            "the legs validate (marks present), even though the curve can't price",
        );
        assert!(
            projection(&state).ready().is_none(),
            "no curve is fabricated for an unpriceable strategy",
        );
        render(&state, 80, 24); // the "curve unavailable" state renders without panic
    }

    #[test]
    fn test_commit_geometry_is_deterministic_across_identical_stores() {
        // The build is a pure function of (legs, store snapshot): committing the
        // same spread against two independently-seeded, identical stores yields
        // byte-for-byte identical expiration and t+0 series (no clock, no RNG).
        let build = || {
            let mut state = live_state();
            focus(&mut state, 0);
            press(&mut state, KeyCode::Char('a'));
            focus(&mut state, 1);
            press(&mut state, KeyCode::Char('a'));
            press(&mut state, KeyCode::Char('s'));
            press(&mut state, KeyCode::Enter);
            let expiration = state.payoff_builder.active_graph_data().clone();
            press(&mut state, KeyCode::Char('t'));
            let tplus0 = state.payoff_builder.active_graph_data().clone();
            (expiration, tplus0)
        };
        assert_eq!(build(), build(), "identical inputs yield identical curves");
    }

    // --- #27 SF-1: the t+0 curve freezes the commit-time entry premium ---------

    #[test]
    fn test_tplus0_freezes_entry_premium_and_reflects_unrealized_pnl() {
        // SF-1: commit a long ATM call at mark P0, switch to the t+0 curve, then drive
        // a mark increase (P0→P1) while committed. The refreshed t+0 must reflect the
        // ACCRUED unrealized P&L at spot (≈ P1−P0), NOT re-anchor to ~0 as the prior
        // re-read-the-mark bug did, and still converge to the FROZEN expiration curve
        // at the wings — both curves share one frozen cost basis.
        let mut state = live_state(); // FULL_A call mid ≈ 3050 (plausible local IV)
        focus(&mut state, 0);
        state.selection.focused_leg = LegFocus::Call;
        press(&mut state, KeyCode::Char('a')); // long FULL_A (= spot) call
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('t')); // → t+0
        assert_eq!(state.payoff_builder.curve(), CurveMode::TPlus0);

        // At commit the mark equals the entry premium, so the unrealized P&L at spot
        // is ~0 (BS(spot, IV0) ≈ P0).
        let before = series_y_near(state.payoff_builder.active_graph_data(), FULL_A);
        assert!(
            before.abs() < 20.0,
            "at commit the t+0 P&L at spot is ~0 (mark == entry), got {before}",
        );

        // Drive a mark increase (P0 3050 → P1 3550) and refresh the t+0 curve while it
        // is the shown one.
        state.store = higher_mark_store();
        {
            let LiveState {
                store,
                payoff_builder,
                ..
            } = &mut state;
            payoff_builder.refresh_tplus0(store);
        }

        // (a) The t+0 at spot now reflects the accrued unrealized P&L (≈ +500) — it did
        // NOT re-anchor to 0, because the entry premium stayed frozen at P0.
        let after = series_y_near(state.payoff_builder.active_graph_data(), FULL_A);
        let accrued = after - before;
        assert!(
            (accrued - 500.0).abs() < 60.0,
            "t+0 accrued ≈ the mark move of 500 (before {before}, after {after}, accrued {accrued})",
        );

        // (b) The t+0 still converges to the FROZEN expiration curve at the deep-OTM
        // wing (the grid's low endpoint) — one shared cost basis.
        let wing = FULL_A * 0.7;
        let t0_wing = series_y_near(state.payoff_builder.active_graph_data(), wing);
        press(&mut state, KeyCode::Char('t')); // → expiration (frozen at commit)
        let exp_wing = series_y_near(state.payoff_builder.active_graph_data(), wing);
        assert!(
            (t0_wing - exp_wing).abs() < 100.0,
            "t+0 converges to the frozen expiration at the wing (t0 {t0_wing}, exp {exp_wing})",
        );
    }

    // --- #27 SF-2: split IV requirement (expiration IV-free, t+0 gated) ---------

    #[test]
    fn test_subfloor_local_iv_renders_expiration_but_marks_tplus0_unavailable() {
        // SF-2: a leg whose only IV is a sub-0.5% LOCAL inversion (a #83-style
        // mispricing) must not feed a fabricated t+0 curve. The mark-only expiration
        // curve still renders; the t+0 curve is the honest "unavailable" state.
        let mut state = subfloor_live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // long FULL_A call (mark present, sub-floor IV)
        press(&mut state, KeyCode::Enter);
        assert!(
            state.payoff_builder.committed().is_some(),
            "marks present → the legs validate",
        );
        // The expiration curve (default, IV-independent) renders.
        assert!(
            state.payoff_builder.has_expiration_curve(),
            "the expiration curve renders without any IV",
        );
        assert!(
            projection(&state).ready().is_some(),
            "the expiration curve projects Ready",
        );
        // The t+0 curve is unavailable — an empty series, not a silently flat curve.
        press(&mut state, KeyCode::Char('t'));
        assert_eq!(state.payoff_builder.curve(), CurveMode::TPlus0);
        assert!(
            projection(&state).ready().is_none(),
            "t+0 is unavailable for a sub-floor local IV, never fabricated",
        );
        match state.payoff_builder.active_graph_data() {
            GraphData::Series(series) => {
                assert!(series.x.is_empty(), "the t+0 series is empty (unavailable)");
            }
            other => panic!("expected an empty Series, got {other:?}"),
        }
        render(&state, 80, 24); // the "t+0 unavailable — no reliable IV" state renders
    }

    #[test]
    fn test_venue_iv_below_floor_is_not_gated_and_tplus0_renders() {
        // A VENUE (`Provider`) IV is trusted as-is and never floored (exactly as #25):
        // even a sub-0.5% venue IV prices the t+0 curve, unlike a sub-floor local one.
        let mut state = subfloor_live_state();
        // Overlay a venue IV (below the plausibility floor) for both legs; the store
        // keeps a venue IV verbatim across its recompute, so it wins the resolution.
        {
            let store = &mut state.store;
            let _ = store.apply_greeks(&venue_iv_row(FULL_A, OptionStyle::Call, 0.003));
            let _ = store.apply_greeks(&venue_iv_row(FULL_B, OptionStyle::Call, 0.003));
        }
        focus(&mut state, 0);
        state.selection.focused_leg = LegFocus::Call;
        press(&mut state, KeyCode::Char('a')); // long FULL_A call
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('t')); // → t+0
        assert_eq!(state.payoff_builder.curve(), CurveMode::TPlus0);
        assert!(
            projection(&state).ready().is_some(),
            "a sub-floor VENUE IV still prices the t+0 curve (never gated)",
        );
    }

    // --- ux P2-01: the drawn y-bounds always include the zero line -------------

    #[test]
    fn test_y_bounds_always_include_zero_and_labels_track_the_widening() {
        // A P&L window entirely above zero widens DOWN to 0; entirely below widens UP
        // to 0; a straddling window is unchanged — so the zero line + y=0 markers never
        // clip. The regenerated endpoint labels match the widened range.
        assert_eq!(y_bounds_including_zero([10.0, 50.0]), [0.0, 50.0]);
        assert_eq!(y_bounds_including_zero([-50.0, -10.0]), [-50.0, 0.0]);
        assert_eq!(y_bounds_including_zero([-5.0, 5.0]), [-5.0, 5.0]);
        // Labels are [min, mid, max] over the WIDENED bounds (two-decimal, aligned).
        let labels: Vec<String> = payoff_y_labels([0.0, 50.0])
            .into_iter()
            .map(|s| s.content.into_owned())
            .collect();
        assert_eq!(labels, ["0.00", "25.00", "50.00"]);
    }

    #[test]
    fn test_draw_committed_is_pure_and_builds_no_graphdata() {
        // The draw path reads only the cached projection: drawing the committed
        // chart (and the t+0 chart) mutates no builder state and reprojects nothing
        // (draw builds no GraphData and prices nothing).
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        let before = state.payoff_builder.active_graph_data().clone();
        let before_rev = state.payoff_builder.graph_revision();
        render(&state, 120, 40);
        render(&state, 40, 12); // a tight body, still no panic
        assert_eq!(
            state.payoff_builder.active_graph_data(),
            &before,
            "draw builds or mutates no GraphData",
        );
        assert_eq!(
            state.payoff_builder.graph_revision(),
            before_rev,
            "draw reprojects nothing",
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// Any sequence of builder keys, over any focused row, leaves a builder state
        /// that draws into a `TestBackend` without panic (`docs/TESTING.md` §3,
        /// `render_never_panics`) — empty, partial, invalid, or committed.
        #[test]
        fn prop_payoff_builder_any_key_sequence_renders(
            keys in proptest::collection::vec(0u8..8, 0..24),
            focus_row in 0usize..4,
            width in 40u16..120,
            height in 8u16..40,
        ) {
            let mut state = live_state();
            state.selection.focused_row = Some(focus_row);
            for k in keys {
                let code = match k {
                    0 => KeyCode::Char('a'),
                    1 => KeyCode::Char('x'),
                    2 => KeyCode::Char('+'),
                    3 => KeyCode::Char('-'),
                    4 => KeyCode::Char('s'),
                    5 => KeyCode::Enter,
                    6 => KeyCode::Esc,
                    _ => KeyCode::Char('t'),
                };
                press(&mut state, code);
            }
            let theme = Theme::resolve(ThemeChoice::Auto, false);
            let payoff = projection(&state);
            let mut term = terminal(width, height);
            let area = Rect::new(0, 0, width, height);
            match term.draw(|frame| draw(&state, &payoff, frame, area, theme)) {
                Ok(_) => {}
                Err(e) => prop_assert!(false, "payoff draw failed at {width}x{height}: {e}"),
            }
        }
    }
}
