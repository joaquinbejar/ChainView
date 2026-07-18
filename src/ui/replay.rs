//! The replay screen — equity curve + drawdown, per-Greek P&L attribution, and the
//! per-trade drill-down (`docs/05-views-and-ux.md` §5, §6,
//! `docs/04-replay-mode.md` §4, §6).
//!
//! [`draw`] renders the **bundle-load lifecycle** first (the states-first rule,
//! `docs/05-views-and-ux.md` §6) — the loading spinner and the retryable error — and
//! then, once the bundle is [`BundleLoad::Ready`], the populated body: the **equity
//! curve** (up to the scrub head, with the head marked and a drawdown indicator), the
//! **run-level P&L attribution** at the head (theta / delta / vega / spread capture /
//! fees / residual, **displayed as authored** by IronCondor, never recomputed), and
//! the **trade drill-down** (the fills list under the head, stepped with `,` / `.`,
//! and the selected fill's contract/side/qty/price/fees/slippage/mode). Every state
//! renders deliberately — an empty run (no rows / no fills) is an explicit empty
//! state, never a blank void or a corrupt chart.
//!
//! # The draw path is pure
//!
//! [`draw`] takes `&ReplayState` (never `&mut`), the pre-projected equity
//! [`GraphProjection`](crate::ui::graph::GraphProjection) (built **off** the draw path
//! by [`ViewState::sync`](crate::ViewState) from the cached
//! [`equity_graph`](crate::app::LoadedReplay::equity_graph)), and the `Copy` resolved
//! [`Theme`] + tick. It reads the cached projection and the timeline cursor's O(1)
//! as-of head rows — no `GraphData` build, no attribution recomputation, no I/O, no
//! state mutation (`docs/02-tui-architecture.md` §7). The seek-time-contracted
//! open-position set is **not** read here (a fill carries its own position context —
//! contract/side/qty — so the drill-down never scans it per frame).
//!
//! # Money is integer cents → `$` at this edge only
//!
//! Every monetary value stays integer cents in the domain/state and is formatted to a
//! `$` string here, the single cents→`$` seam ([`fmt_cents_abs`],
//! `docs/05-views-and-ux.md` §5). Checked integer arithmetic only — no `f64` money.
//! The equity chart's y-axis labels are the sole `$` derived from the projection's
//! `f64` plot bounds (display geometry, not accounting); every non-finite display
//! float is guarded to `—` before it paints.
//!
//! [`handle_key`] is pure over `&mut ReplayState`: a scrub key returns an
//! [`AppEvent::ReplaySeek`], a play/pause/speed key an [`AppEvent::ReplayControl`],
//! and a `,` / `.` drill-down key steps the in-memory selection directly (no I/O, no
//! `.await`) — the render loop's view-signature diff schedules the redraw.

use crossterm::event::KeyEvent;
use optionstratlib::OptionStyle;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph, Wrap};

use crate::app::keymap::{KeyChord, ReplayAction, resolve_replay};
use crate::app::{BundleLoad, LoadedReplay, ReplayState};
use crate::event::{AppEvent, ReplayControl, SeekTo};
use crate::replay::{ExecMode, Fill, GreeksAttribution, PositionSide};
use crate::ui::graph::{EmptyReason, GraphProjection, ProjectedSeries};
use crate::ui::theme::{Theme, pnl_sign_char, pnl_sign_span, run_label, sanitize, spinner_frame};

/// The em dash rendered for an unknown / absent value — never a fabricated `0`
/// (`docs/01-domain-model.md` §8).
const EM_DASH: &str = "—";

/// The fixed width of the drawdown side panel beside the equity chart.
const DRAWDOWN_PANEL_WIDTH: u16 = 18;

/// The reserved height of the selected-fill detail panel under the fills list.
const FILL_DETAIL_HEIGHT: u16 = 3;

/// The fixed label column width in the attribution panel (fits every term label).
const ATTRIB_LABEL_WIDTH: usize = 12;

/// The fixed amount column width in the attribution panel (`$` magnitude, padded so
/// the bars align).
const ATTRIB_AMOUNT_WIDTH: usize = 11;

/// The maximum attribution bar length in cells.
const ATTRIB_MAX_BAR: usize = 14;

// ===========================================================================
// The draw entry point + the bundle-load lifecycle (states first).
// ===========================================================================

/// Draw the replay screen for `state` into `area` — a pure render over the borrowed
/// bundle/timeline model and the pre-projected `equity` series
/// (`docs/02-tui-architecture.md` §7). Matched exhaustively over [`BundleLoad`]:
///
/// - [`BundleLoad::Loading`] → a centered tick-driven spinner + "loading bundle
///   `<run>`…" (the §6 loading idiom, in lock-step with the status-bar spinner).
/// - [`BundleLoad::Error`] → the bounded, wrapped bundle-error message + an explicit
///   "press `R` to retry" affordance (glyph-prefixed, `NO_COLOR`-safe).
/// - [`BundleLoad::Ready`] → the populated body (equity + drawdown, attribution,
///   drill-down), or the deliberate empty state for a zero-row run.
///
/// `equity` is the ui view-cache's projection of the cached equity series, computed
/// **off** the draw path; this paint builds no `GraphData` and recomputes no
/// attribution. `theme`/`tick` are `Copy`, so the draw stays pure over borrowed state.
pub fn draw(
    state: &ReplayState,
    equity: &GraphProjection,
    frame: &mut Frame,
    area: Rect,
    theme: Theme,
    tick: u64,
) {
    match &state.bundle {
        BundleLoad::Loading => draw_loading(frame, area, theme, &run_label(state), tick),
        BundleLoad::Error { message } => draw_error(frame, area, theme, message),
        BundleLoad::Ready(loaded) => {
            draw_ready(frame, area, theme, &run_label(state), loaded, equity);
        }
    }
}

/// Draw the bundle-loading state: a centered tick-driven spinner + "loading bundle
/// `<run>`…" plus a dim secondary hint — the §6 loading idiom, using the shared
/// [`spinner_frame`] so the body and status-bar spinners advance in lock-step. The
/// venue-independent `run` label is sanitized at this render edge.
fn draw_loading(frame: &mut Frame, area: Rect, theme: Theme, run: &str, tick: u64) {
    draw_state_body(
        frame,
        area,
        theme,
        Text::from(vec![
            Line::from(Span::styled(
                format!("{} loading bundle {}…", spinner_frame(tick), sanitize(run)),
                theme.accent(),
            )),
            Line::from(Span::styled("reading the bundle tables", theme.dim())),
        ]),
    );
}

/// Draw the bundle-error state: the bounded, wrapped error message with the last
/// inner row reserved for an always-visible "press `R` to retry" affordance, so a
/// long message can never clip the retry hint. The `!` glyph prefix carries the
/// state without color, `NO_COLOR`-safe; the message is sanitized at this render edge.
fn draw_error(frame: &mut Frame, area: Rect, theme: Theme, message: &str) {
    let block = Block::bordered().title(Span::styled("Replay", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [msg_area, hint_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("! {}", sanitize(message)),
            theme.warning(),
        )))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true }),
        msg_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("press R to retry", theme.dim())))
            .alignment(Alignment::Center),
        hint_area,
    );
}

/// Draw a centered state body inside the framed "Replay" block — used by the
/// loading/error/degenerate states so a lifecycle state reads as deliberate, never a
/// blank void. `Flex::Center` does the geometry (no manual arithmetic).
fn draw_state_body(frame: &mut Frame, area: Rect, theme: Theme, text: Text<'static>) {
    let block = Block::bordered().title(Span::styled("Replay", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = u16::try_from(text.height())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

// ===========================================================================
// The Ready body: equity + drawdown, attribution, drill-down.
// ===========================================================================

/// Draw the populated replay body at the scrub head: the framed block titled with the
/// run / execution mode / step position, split into an equity row (the chart + a
/// drawdown indicator) over the attribution + fills row. A zero-row run renders the
/// deliberate empty states inside each panel, never a blank or a panic.
fn draw_ready(
    frame: &mut Frame,
    area: Rect,
    theme: Theme,
    run: &str,
    loaded: &LoadedReplay,
    equity: &GraphProjection,
) {
    let block = Block::bordered().title(Span::styled(replay_title(run, loaded), theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Equity row (top half) over the attribution + fills row (bottom).
    let [equity_row, bottom_row] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Min(0)]).areas(inner);
    // Equity row: the chart, then a fixed-width drawdown indicator.
    let [chart_area, drawdown_area] = Layout::horizontal([
        Constraint::Min(20),
        Constraint::Length(DRAWDOWN_PANEL_WIDTH),
    ])
    .areas(equity_row);
    draw_equity(frame, chart_area, theme, equity);
    draw_drawdown(frame, drawdown_area, theme, loaded);
    // Bottom row: attribution (left) beside the fills drill-down (right).
    let [attribution_area, fills_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(bottom_row);
    draw_attribution(frame, attribution_area, theme, loaded);
    draw_fills(frame, fills_area, theme, loaded);
}

/// The framed-block title: `Replay · <run> · <mode> · step N/END`. The execution mode
/// is read from the first fill (a run-level property), omitted when the run has no
/// fills; the `run` label is sanitized at this edge.
fn replay_title(run: &str, loaded: &LoadedReplay) -> String {
    let step = loaded.cursor.position();
    let end = loaded.cursor.end_step();
    match loaded.bundle.fills.first() {
        Some(fill) => format!(
            "Replay · {} · {} · step {step}/{end}",
            sanitize(run),
            exec_mode_label(fill.mode),
        ),
        None => format!("Replay · {} · step {step}/{end}", sanitize(run)),
    }
}

/// Draw the equity chart panel (up to the head, with the head marked), or the "no
/// equity rows" empty state for a zero-row run.
fn draw_equity(frame: &mut Frame, area: Rect, theme: Theme, equity: &GraphProjection) {
    let block = Block::bordered().title(Span::styled("equity", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match equity.ready() {
        Some(series) => draw_equity_chart(frame, inner, theme, series),
        None => draw_centered_note(
            frame,
            inner,
            theme,
            equity_empty_message(equity.empty_reason()),
        ),
    }
}

/// The equity empty-state message, keyed on the projection's [`EmptyReason`]
/// (exhaustive, no wildcard) — an honest reason, never a fabricated line.
fn equity_empty_message(reason: Option<EmptyReason>) -> &'static str {
    match reason {
        Some(EmptyReason::NoData) | None => "no equity rows in this run",
        Some(EmptyReason::Degenerate) => "equity curve unavailable — degenerate data",
        Some(EmptyReason::Unsupported) => "equity curve unavailable",
    }
}

/// Draw the equity line chart from the pre-projected `series`: the equity line over
/// the step domain, the head marked with a distinct scatter marker (its last point,
/// which is always the head step), and `$`-formatted y-axis labels derived at this
/// edge from the projection's cent bounds.
fn draw_equity_chart(frame: &mut Frame, inner: Rect, theme: Theme, series: &ProjectedSeries) {
    let x_bounds = series.x_bounds();
    let y_bounds = series.y_bounds();
    // The head is the last plotted point (the series always retains the head step).
    let head: Vec<(f64, f64)> = series.points().last().copied().into_iter().collect();
    let mut datasets = vec![
        Dataset::default()
            .name(series.name().to_owned())
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme.accent())
            .data(series.points()),
    ];
    if !head.is_empty() {
        datasets.push(
            // A distinct `Block` marker SHAPE marks the scrub head so it reads under
            // NO_COLOR (shape, not color).
            Dataset::default()
                .name("head")
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(theme.warning())
                .data(&head),
        );
    }
    let chart = Chart::new(datasets)
        .x_axis(
            Axis::default()
                .title("step")
                .bounds(x_bounds)
                .labels(step_axis_labels(x_bounds))
                .style(theme.dim()),
        )
        .y_axis(
            Axis::default()
                .title("$")
                .bounds(y_bounds)
                .labels(equity_y_labels(y_bounds))
                .style(theme.dim()),
        );
    frame.render_widget(chart, inner);
}

/// The `[min, max]` step-axis labels from the projection's x-bounds (integers).
fn step_axis_labels(bounds: [f64; 2]) -> Vec<Span<'static>> {
    let [min, max] = bounds;
    vec![Span::raw(fmt_step(min)), Span::raw(fmt_step(max))]
}

/// The `[min, mid, max]` `$` y-axis labels from the projection's **cent** bounds,
/// converted to dollars at this edge (display geometry only — the accounting values
/// use the exact integer-cent formatter). Non-finite bounds render `—`.
fn equity_y_labels(bounds: [f64; 2]) -> Vec<Span<'static>> {
    let [min, max] = bounds;
    let mid = min + (max - min) / 2.0;
    vec![
        Span::raw(fmt_axis_cents(min)),
        Span::raw(fmt_axis_cents(mid)),
        Span::raw(fmt_axis_cents(max)),
    ]
}

/// Draw the drawdown indicator: the exact **peak** drawdown in `$` (integer cents,
/// signed) and the head row's authored drawdown ratio as a percentage (guarded for
/// `NaN`/`Inf`, `—` when absent). Not the same glyph as the loading spinner or the
/// equity line — an honest, color-independent readout.
fn draw_drawdown(frame: &mut Frame, area: Rect, theme: Theme, loaded: &LoadedReplay) {
    let block = Block::bordered().title(Span::styled("drawdown", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let peak = loaded.peak_drawdown_cents();
    let now = loaded
        .cursor
        .head_equity(&loaded.bundle)
        .map(|point| point.drawdown);
    let lines = vec![
        Line::from(vec![
            Span::styled("peak ", theme.dim()),
            Span::styled(fmt_drawdown_cents(peak), theme.pnl_style(peak < 0)),
        ]),
        Line::from(vec![
            Span::styled("now  ", theme.dim()),
            Span::raw(fmt_drawdown_ratio(now)),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Draw the run-level P&L attribution panel at the head: the by-Greek breakdown
/// (Θ / Δ / ν / spread capture / fees / residual), **displayed as authored** — no
/// recomputation — with `+`/`−` sign glyphs, `$`-formatted magnitudes, and a
/// magnitude-proportional bar. An absent head row (empty run) renders every term as
/// `—`, never a fabricated `0`.
fn draw_attribution(frame: &mut Frame, area: Rect, theme: Theme, loaded: &LoadedReplay) {
    let step = loaded.cursor.position();
    let block = Block::bordered().title(Span::styled(
        format!("P&L attribution @ step {step}"),
        theme.accent(),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = attribution_rows(loaded.cursor.head_greeks(&loaded.bundle));
    let max_abs = rows
        .iter()
        .filter_map(|row| row.value.map(|(_, magnitude)| magnitude))
        .max()
        .unwrap_or(0);
    // Budget the row width label → sign → amount → bar (amount outranks the bar): the
    // bar shrinks first on a narrow panel so the amount keeps room, and if the amount
    // still cannot fit it is elided with a trailing `…` — never a bare, misreadable
    // prefix (`$1` from `$1,930.00`).
    let content_w = usize::from(inner.width);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|row| attribution_line(row, max_abs, content_w, theme))
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// One attribution term: its label and its signed value as `(is_negative, magnitude
/// cents)`, or `None` when the head row is absent (rendered `—`).
struct AttribRow {
    label: &'static str,
    value: Option<(bool, u64)>,
}

impl AttribRow {
    /// A term absent because the run has no attribution row at the head.
    fn absent(label: &'static str) -> Self {
        Self { label, value: None }
    }

    /// A signed integer-cent term (`is_negative` from the sign, magnitude from `|v|`).
    fn signed(label: &'static str, cents: i64) -> Self {
        Self {
            label,
            value: Some((cents < 0, cents.unsigned_abs())),
        }
    }

    /// The fees term: always a **cost**, so a positive fee is a negative P&L
    /// contribution (`docs/04-replay-mode.md` §2.3); a zero fee reads `+$0.00`.
    fn fee(label: &'static str, fees_cents: u64) -> Self {
        Self {
            label,
            value: Some((fees_cents > 0, fees_cents)),
        }
    }
}

/// The six attribution rows, built verbatim from the head [`GreeksAttribution`] row
/// (displayed, never recomputed), or all-absent when there is no head row.
fn attribution_rows(head: Option<&GreeksAttribution>) -> [AttribRow; 6] {
    match head {
        None => [
            AttribRow::absent("Θ theta"),
            AttribRow::absent("Δ delta"),
            AttribRow::absent("ν vega"),
            AttribRow::absent("spread cap."),
            AttribRow::absent("fees"),
            AttribRow::absent("residual"),
        ],
        Some(row) => [
            AttribRow::signed("Θ theta", row.theta_pnl_cents),
            AttribRow::signed("Δ delta", row.delta_pnl_cents),
            AttribRow::signed("ν vega", row.vega_pnl_cents),
            AttribRow::signed("spread cap.", row.spread_capture_cents),
            AttribRow::fee("fees", row.fees_cents),
            AttribRow::signed("residual", row.residual_cents),
        ],
    }
}

/// One attribution line: `<label>  <±>$<magnitude>  <bar>`, or `<label>  —` for an
/// absent term. The sign glyph + colored magnitude carry the sign (legible under
/// `NO_COLOR`); the bar length is proportional to `|value| / max_abs`.
///
/// `content_w` is the panel's inner width. The columns are budgeted **amount before
/// bar**: after the fixed label + the 1-cell sign, the amount takes what it needs and
/// the bar gets any remainder (capped at [`ATTRIB_MAX_BAR`]) — so the bar drops first
/// on a narrow panel. If the amount itself cannot fit, it is elided with a trailing
/// `…` ([`elide`]) so a truncation is always visible, never a bare misreadable prefix.
fn attribution_line(
    row: &AttribRow,
    max_abs: u64,
    content_w: usize,
    theme: Theme,
) -> Line<'static> {
    let label = format!("{:<width$}", row.label, width = ATTRIB_LABEL_WIDTH);
    match row.value {
        None => Line::from(vec![
            Span::raw(label),
            Span::styled(EM_DASH.to_owned(), theme.dim()),
        ]),
        Some((is_negative, magnitude)) => {
            // Room left for the amount + bar after the label and the 1-cell sign.
            let after_sign = floor_sub(content_w, ATTRIB_LABEL_WIDTH + 1);
            let amount = fmt_cents_abs(magnitude);
            let amount_len = amount.chars().count();
            let style = theme.pnl_style(is_negative);
            let mut spans = vec![Span::raw(label), pnl_sign_span(is_negative, theme)];
            if amount_len <= after_sign {
                // The amount fits: pad it toward the alignment column (bounded by the
                // room), then give any remainder to the magnitude bar.
                let pad = ATTRIB_AMOUNT_WIDTH.max(amount_len).min(after_sign);
                spans.push(Span::styled(format!("{amount:<pad$}"), style));
                let bar_room = floor_sub(after_sign, pad).min(ATTRIB_MAX_BAR);
                if bar_room > 0 {
                    spans.push(Span::styled(
                        bar_string(magnitude, max_abs, bar_room),
                        style,
                    ));
                }
            } else {
                // The amount cannot fit: elide it with a trailing `…` (no bar).
                spans.push(Span::styled(elide(&amount, after_sign), style));
            }
            Line::from(spans)
        }
    }
}

/// Truncate `s` to at most `max_width` display columns, marking any cut with a
/// trailing `…` so a truncation is never silent. `s` is ASCII here (a `$`-formatted
/// amount), so `char` count is the column count. Returns `s` unchanged when it already
/// fits, and `""` when there is no room at all.
fn elide(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if s.chars().count() <= max_width {
        return s.to_owned();
    }
    // Keep `max_width - 1` chars for the value and reserve the last cell for `…`.
    let mut out: String = s.chars().take(max_width - 1).collect();
    out.push('…');
    out
}

/// `a - b` floored at zero, spelled so it can never underflow — the ruleset bans
/// `saturating_sub` and `checked_sub(..).unwrap_or(0)` trips clippy's
/// `manual_saturating_arithmetic` lint, so the floor is `a.max(b) - b` (the same
/// idiom as `ui/chain.rs`).
#[must_use]
fn floor_sub(a: usize, b: usize) -> usize {
    a.max(b) - b
}

/// A magnitude-proportional bar of `█` cells: `|value| / max_abs * width`, clamped to
/// `width`. Empty when there is nothing to scale against. Computed in `u128` so the
/// scale multiply cannot overflow.
fn bar_string(magnitude: u64, max_abs: u64, width: usize) -> String {
    if max_abs == 0 || width == 0 {
        return String::new();
    }
    let width_u = u128::try_from(width).unwrap_or(0);
    // `magnitude * width` fits `u128` for any `u64` magnitude and a small cell width,
    // so `checked_mul` never trips; the fallback keeps the arithmetic checked.
    let scaled = match u128::from(magnitude).checked_mul(width_u) {
        Some(product) => product / u128::from(max_abs),
        None => width_u,
    };
    let len = usize::try_from(scaled).unwrap_or(width).min(width);
    "█".repeat(len)
}

/// Draw the fills drill-down panel: the fills list under the head (the most recent
/// that fit, the selection highlighted) over the selected fill's detail — or the "no
/// fills" empty state.
fn draw_fills(frame: &mut Frame, area: Rect, theme: Theme, loaded: &LoadedReplay) {
    let block = Block::bordered().title(Span::styled("fills", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let detail_height = if loaded.selection.is_some() {
        FILL_DETAIL_HEIGHT
    } else {
        0
    };
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(detail_height)]).areas(inner);
    draw_fills_list(frame, list_area, theme, loaded);
    if loaded.selection.is_some() {
        draw_fill_detail(frame, detail_area, theme, loaded);
    }
}

/// Draw the fills list, the drill-down selection highlighted with a `▸` glyph + bold.
/// An empty as-of window renders the deliberate "no fills" state.
/// `O(visible rows on screen)` — never the whole tape.
///
/// The list **follows the selection** (#57 scroll-to-selection). The recent-tape
/// window anchors the newest fill at the bottom; the drill-down selection
/// ([`step_fill`](LoadedReplay::step_fill)) walks the whole as-of tape, so a `,`
/// step can move the selection **above** that default window. When it does, the
/// window scrolls up ([`fills_window_start`]) so the selected fill becomes the top
/// visible row — the highlight stays on screen instead of the earlier v0.3
/// off-window `↑` indicator. The window is a pure function of the on-screen row
/// count, the as-of tape length, and the selection index (the selection is its own
/// scroll anchor), so no off-draw scroll offset is stashed.
fn draw_fills_list(frame: &mut Frame, area: Rect, theme: Theme, loaded: &LoadedReplay) {
    let visible = loaded.cursor.visible_fills(&loaded.bundle);
    if visible.is_empty() {
        draw_centered_note(frame, area, theme, "no fills up to this step");
        return;
    }
    let rows = usize::from(area.height);
    if rows == 0 {
        return;
    }
    let selection = loaded.selection.as_ref();
    // The index of the drill-down selection within the as-of tape (oldest → newest) —
    // the scroll anchor. Resolved OFF the draw path (on selection step / cursor move,
    // #118) and read here in O(1), so the draw never rescans the whole visible tape;
    // only the window `[start, start + rows)` is formatted below (O(visible rows)).
    let selected_ix = loaded.selected_fill_index();
    let start = fills_window_start(visible.len(), rows, selected_ix);
    // The window `[start, start + rows)` in chronological (oldest → newest) order — the
    // selected fill is guaranteed on-screen by `fills_window_start`.
    let lines: Vec<Line<'static>> = visible
        .iter()
        .skip(start)
        .take(rows)
        .map(|fill| fill_line(fill, is_selected(selection, fill), theme))
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// The first visible index of the fills window so the drill-down selection stays on
/// screen (#57 scroll-to-selection). The window shows `rows` fills with the newest
/// anchored at the bottom (the default recent-tape view); when the selection has
/// stepped **above** that default window, the window scrolls up so the selected fill
/// becomes the top visible row — so the highlight follows the selection and never
/// leaves the viewport. Everything fits (`total <= rows`) or a zero-row viewport → `0`.
/// A pure, off-draw-testable function of the tape length, the on-screen row count, and
/// the selection index (never an unchecked subtraction).
#[must_use]
fn fills_window_start(total: usize, rows: usize, selected: Option<usize>) -> usize {
    if rows == 0 || total <= rows {
        return 0;
    }
    // The newest fills anchored at the bottom (`total − rows` is safe: `total > rows`).
    let default_start = total - rows;
    match selected {
        // The selection stepped above the default window → scroll up so it is the top
        // visible row. `sel < default_start`, so `sel` is a valid, in-range start.
        Some(sel) if sel < default_start => sel,
        _ => default_start,
    }
}

/// One fills-list line: `▸ <step> <SIDE> <qty>× <underlying> <C/P> @ <$price>`. The
/// selected fill carries a `▸` glyph + bold; the venue underlying is sanitized.
fn fill_line(fill: &Fill, selected: bool, theme: Theme) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let text = format!(
        "{marker}{:>5} {:<5} {}× {} {} @ {}",
        fill.step,
        side_label(fill.side),
        fill.quantity,
        sanitize(&fill.underlying),
        style_glyph(fill.style),
        fmt_cents_abs(fill.price_cents),
    );
    let line = Line::from(Span::raw(text));
    if selected {
        line.style(theme.accent().add_modifier(Modifier::BOLD))
    } else {
        line
    }
}

/// Draw the selected fill's detail (its **position context** + execution details):
/// the contract, side/qty/price, and fees/slippage/mode — all from the fill itself,
/// never a per-fill Greek split it cannot derive (`docs/04-replay-mode.md` §6). Money
/// from integer cents; the contract id is sanitized.
fn draw_fill_detail(frame: &mut Frame, area: Rect, theme: Theme, loaded: &LoadedReplay) {
    let Some(fill) = loaded.selection.as_ref() else {
        return;
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("▸ ", theme.accent()),
            Span::raw(sanitize(&fill.contract_id)),
        ]),
        Line::from(vec![
            Span::raw(format!("{} {}× @ ", side_label(fill.side), fill.quantity)),
            Span::raw(fmt_cents_abs(fill.price_cents)),
        ]),
        Line::from(vec![
            Span::styled("fees ", theme.dim()),
            Span::raw(fmt_cents_abs(fill.fees_cents)),
            Span::styled("  slip ", theme.dim()),
            Span::raw(fmt_signed_cents(fill.slippage_cents)),
            Span::styled("  ", theme.dim()),
            Span::raw(exec_mode_label(fill.mode).to_owned()),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Draw a centered, dim single-line note (a panel-local empty state).
fn draw_centered_note(frame: &mut Frame, area: Rect, theme: Theme, message: &str) {
    let text = Text::from(Line::from(Span::styled(message.to_owned(), theme.dim())));
    let height = 1u16.min(area.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

// ===========================================================================
// Value helpers (labels, glyphs, and the cents→$ edge formatters).
// ===========================================================================

/// Whether `fill` is the drilled-into selection, compared by its unique
/// `(step, order_id, fill_seq)` identity.
fn is_selected(selection: Option<&Fill>, fill: &Fill) -> bool {
    selection.is_some_and(|selected| {
        selected.step == fill.step
            && selected.order_id == fill.order_id
            && selected.fill_seq == fill.fill_seq
    })
}

/// The color-independent side label, exhaustive over [`PositionSide`].
fn side_label(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

/// The single-letter option-style glyph (`C` call / `P` put), exhaustive.
fn style_glyph(style: OptionStyle) -> &'static str {
    match style {
        OptionStyle::Call => "C",
        OptionStyle::Put => "P",
    }
}

/// The execution-mode label, exhaustive over [`ExecMode`].
fn exec_mode_label(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Naive => "naive",
        ExecMode::Realistic => "realistic",
    }
}

/// Format a **non-negative** integer-cent magnitude as `$1,234.56` — the single
/// cents→`$` seam (`docs/05-views-and-ux.md` §5). Integer arithmetic only, no `f64`
/// money; thousands-grouped dollars with two-decimal cents.
fn fmt_cents_abs(magnitude: u64) -> String {
    let dollars = magnitude / 100;
    let cents = magnitude % 100;
    format!("${}.{cents:02}", group_thousands(dollars))
}

/// Format a **signed** integer-cent value with a leading `+`/`−` glyph, e.g.
/// `−$0.15` / `+$0.00`. The sign glyph carries the sign under `NO_COLOR`.
fn fmt_signed_cents(cents: i64) -> String {
    let sign = if cents < 0 { '−' } else { '+' };
    format!("{sign}{}", fmt_cents_abs(cents.unsigned_abs()))
}

/// Format the peak drawdown (integer cents, `<= 0`): `$0.00` at a peak, else
/// `−$<magnitude>` — always negative, so the `−` glyph carries the sign.
fn fmt_drawdown_cents(cents: i64) -> String {
    if cents == 0 {
        return fmt_cents_abs(0);
    }
    format!("−{}", fmt_cents_abs(cents.unsigned_abs()))
}

/// Format the authored drawdown ratio as a percentage, or `—` when absent or a
/// non-finite (`NaN`/`Inf`) display float — guarded before it paints. The magnitude is
/// formatted `.abs()` behind the shared U+2212 `−` / `+` sign glyph
/// ([`pnl_sign_char`]), so a negative reads with the same `−` as every other signed
/// value (never an ASCII `-`).
fn fmt_drawdown_ratio(ratio: Option<f64>) -> String {
    match ratio {
        Some(value) if value.is_finite() => {
            let pct = value * 100.0;
            format!("{}{:.1}%", pnl_sign_char(pct < 0.0), pct.abs())
        }
        _ => EM_DASH.to_owned(),
    }
}

/// Format a `$` equity axis label from a **cent** plot bound (display geometry, not
/// accounting), or `—` for a non-finite bound. Whole dollars, thousands-grouped like
/// the attribution amounts (a `$1,000,000` axis stays legible — the wider gutter is
/// accepted); a negative bound carries the shared U+2212 `−` sign.
fn fmt_axis_cents(cents: f64) -> String {
    if !cents.is_finite() {
        return EM_DASH.to_owned();
    }
    let dollars = cents / 100.0;
    let sign = if dollars < 0.0 { "−" } else { "" };
    // Round to whole dollars via `{:.0}` (a digit string, never an `as` cast), then
    // group thousands.
    let digits = format!("{:.0}", dollars.abs());
    format!("{sign}${}", group_digits(&digits))
}

/// Format a step-axis label from an (integer-valued) plot bound, or `—` when
/// non-finite; clamped at `0` so a degenerate bound never reads negative.
fn fmt_step(value: f64) -> String {
    if !value.is_finite() {
        return EM_DASH.to_owned();
    }
    format!("{:.0}", value.max(0.0))
}

/// Insert thousands separators into a non-negative integer's decimal digits
/// (`1234567` → `1,234,567`), no `f64` and no `as` cast.
fn group_thousands(value: u64) -> String {
    group_digits(&value.to_string())
}

/// Insert thousands separators into an already-formatted **non-negative decimal-digit
/// string** (`"1234567"` → `"1,234,567"`). The caller supplies pure ASCII digits (a
/// `u64::to_string` or a `{:.0}` whole-number format), so this is the shared grouping
/// core for both the money seam ([`group_thousands`]) and the equity axis
/// ([`fmt_axis_cents`]); no `f64`, no `as` cast.
fn group_digits(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (len - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(char::from(*byte));
    }
    out
}

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a replay-local key, returning any follow-on [`AppEvent`]
/// (`docs/05-views-and-ux.md` §3). Pure — no I/O, no `.await`.
///
/// The key is resolved **through the single keybinding map**
/// ([`resolve_replay`], `src/app/keymap.rs`) so the dispatch and the help overlay
/// cannot drift. The scrubs (`←`/`→`/`h`/`l` step, `Home`/`End` jump) return an
/// [`AppEvent::ReplaySeek`]; play/pause (`Space`) and speed (`+`/`-`) an
/// [`AppEvent::ReplayControl`]; and the fill drill-down (`,` / `.`) steps the
/// in-memory [`selection`](LoadedReplay::selection) directly (an in-memory move, not
/// I/O), returning `None` — the render loop's view-signature diff schedules the
/// redraw.
#[must_use]
pub fn handle_key(state: &mut ReplayState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_replay(chord, state.screen)? {
        ReplayAction::StepBack => Some(AppEvent::ReplaySeek(SeekTo::StepBy(-1))),
        ReplayAction::StepForward => Some(AppEvent::ReplaySeek(SeekTo::StepBy(1))),
        ReplayAction::JumpStart => Some(AppEvent::ReplaySeek(SeekTo::Step(0))),
        // The cursor clamps `Step` to `end_step`, so `u32::MAX` lands on the last
        // step regardless of the tape length — no need to read the cursor here.
        ReplayAction::JumpEnd => Some(AppEvent::ReplaySeek(SeekTo::Step(u32::MAX))),
        ReplayAction::PlayPause => Some(AppEvent::ReplayControl(ReplayControl::PlayPause)),
        ReplayAction::SpeedSlower => Some(AppEvent::ReplayControl(ReplayControl::SpeedSlower)),
        ReplayAction::SpeedFaster => Some(AppEvent::ReplayControl(ReplayControl::SpeedFaster)),
        ReplayAction::PrevFill => {
            let _ = state.step_fill(false);
            None
        }
        ReplayAction::NextFill => {
            let _ = state.step_fill(true);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::OptionStyle;
    use optionstratlib::visualization::{GraphData, Series2D};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;

    use super::{
        ATTRIB_LABEL_WIDTH, AttribRow, attribution_line, bar_string, draw, fill_line,
        fmt_axis_cents, fmt_cents_abs, fmt_drawdown_cents, fmt_drawdown_ratio, fmt_signed_cents,
        group_thousands, handle_key,
    };
    use crate::app::tests_support::loaded_bundle;
    use crate::app::{BundleLoad, ReplayState};
    use crate::config::ThemeChoice;
    use crate::event::{BundleLoadResult, SeekTo};
    use crate::replay::{
        BundleManifest, EquityPoint, ExecMode, Fill, GreeksAttribution, LoadedBundle, PositionSide,
        SUPPORTED_SCHEMA,
    };
    use crate::ui::graph::{GraphProjection, project};
    use crate::ui::theme::Theme;

    // --- fixtures -------------------------------------------------------------

    const BASE_TS: i64 = 1_700_000_000_000_000_000;
    const CID: &str = "v1:BTC:1735286400000000000:6000000:C";

    fn manifest() -> BundleManifest {
        let mut row_counts = BTreeMap::new();
        let _ = row_counts.insert("fills".to_owned(), 0u64);
        let _ = row_counts.insert("equity_curve".to_owned(), 0u64);
        let _ = row_counts.insert("positions".to_owned(), 0u64);
        let _ = row_counts.insert("greeks_attribution".to_owned(), 0u64);
        BundleManifest {
            schema: SUPPORTED_SCHEMA.to_owned(),
            run_id: "run-xyz".to_owned(),
            created_utc: "2026-07-17T00:00:00Z".to_owned(),
            code_version: "0.3.0".to_owned(),
            lockfile_sha256: "deadbeef".to_owned(),
            seed: 1,
            config: serde_json::json!({ "capital_cents": 1_000_000 }),
            strategy: serde_json::json!({}),
            data_source: serde_json::json!({}),
            metrics: serde_json::json!({}),
            row_counts,
        }
    }

    fn equity(step: u32, equity_cents: i64, drawdown: f64) -> EquityPoint {
        EquityPoint {
            step,
            ts_ns: BASE_TS + i64::from(step),
            cash_cents: equity_cents,
            position_value_cents: 0,
            equity_cents,
            drawdown,
        }
    }

    fn greeks(step: u32) -> GreeksAttribution {
        GreeksAttribution {
            step,
            ts_ns: BASE_TS + i64::from(step),
            theta_pnl_cents: 193_000,
            delta_pnl_cents: -42_000,
            vega_pnl_cents: 31_000,
            spread_capture_cents: 18_000,
            fees_cents: 500,
            residual_cents: -6_000,
        }
    }

    fn fill(step: u32, order_id: u64, side: PositionSide) -> Fill {
        Fill {
            step,
            ts_ns: BASE_TS + i64::from(step),
            strategy_run_id: "run-xyz".to_owned(),
            trade_id: order_id,
            position_id: order_id,
            order_id,
            fill_seq: 0,
            underlying: "BTC".to_owned(),
            expiration_ns: 1_735_286_400_000_000_000,
            contract_id: CID.to_owned(),
            strike_cents: 6_000_000,
            style: OptionStyle::Call,
            side,
            quantity: 1,
            price_cents: 235,
            fees_cents: 30,
            slippage_cents: -15,
            mode: ExecMode::Realistic,
        }
    }

    /// A populated bundle: equity/greeks over `0..n` and fills at steps 0, 0, 1.
    fn rich_bundle(n: u32) -> LoadedBundle {
        let fills = vec![
            fill(0, 10, PositionSide::Short),
            fill(0, 11, PositionSide::Long),
            fill(1, 20, PositionSide::Long),
        ];
        LoadedBundle {
            manifest: manifest(),
            fills,
            equity: (0..n)
                .map(|s| equity(s, 1_000 + i64::from(s) * 10, -0.02))
                .collect(),
            positions: Vec::new(),
            greeks: (0..n).map(greeks).collect(),
        }
    }

    /// A bundle with one fill per step over `0..n` (distinct order ids **and** a
    /// distinct per-step quantity `step + 1`), so the recent-tape fills window is
    /// smaller than the fill count at a modest render height and each fill line is
    /// visually distinguishable — the setup for the scroll-to-selection (#57) render.
    fn many_fills_bundle(n: u32) -> LoadedBundle {
        LoadedBundle {
            manifest: manifest(),
            fills: (0..n)
                .map(|s| {
                    let mut f = fill(s, u64::from(s) + 100, PositionSide::Long);
                    f.quantity = s.checked_add(1).unwrap_or(1);
                    f
                })
                .collect(),
            equity: (0..n)
                .map(|s| equity(s, 1_000 + i64::from(s) * 10, -0.02))
                .collect(),
            positions: Vec::new(),
            greeks: (0..n).map(greeks).collect(),
        }
    }

    /// The concatenated text of a rendered [`Line`] (span contents joined), for
    /// asserting the width-budgeted attribution rendering.
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn theme() -> Theme {
        Theme::resolve(ThemeChoice::Auto, false)
    }

    fn state_from(bundle: LoadedBundle) -> ReplayState {
        let mut state = ReplayState::new(PathBuf::from("/tmp/run-xyz"));
        state.apply_load_result(BundleLoadResult::Loaded(Box::new(bundle)));
        state
    }

    /// Project the loaded equity series exactly as the ui view-cache would (off the
    /// draw path) — the `&GraphProjection` the screen's `draw` reads.
    fn equity_projection(state: &ReplayState) -> GraphProjection {
        match state.loaded() {
            Some(loaded) => project(loaded.equity_graph()),
            None => project(&GraphData::Series(Series2D::default())),
        }
    }

    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        }
    }

    #[track_caller]
    fn rendered(state: &ReplayState, tick: u64, width: u16, height: u16) -> String {
        let projection = equity_projection(state);
        let mut term = terminal(width, height);
        match term.draw(|frame| {
            let area = frame.area();
            draw(state, &projection, frame, area, theme(), tick);
        }) {
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

    fn loading_state() -> ReplayState {
        ReplayState::new(PathBuf::from("/tmp/run-2025"))
    }

    fn error_state(message: &str) -> ReplayState {
        let mut state = ReplayState::new(PathBuf::from("/tmp/run-2025"));
        state.apply_load_result(BundleLoadResult::Failed(message.to_owned()));
        state
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // --- lifecycle states (retained from #34) --------------------------------

    #[test]
    fn test_draw_loading_shows_spinner_and_run_label() {
        let state = loading_state();
        assert!(matches!(state.bundle, BundleLoad::Loading));
        let text = rendered(&state, 0, 80, 24);
        assert!(text.contains("loading bundle"), "loading label present");
        assert!(text.contains("run-2025"), "the run/dir label is shown");
        assert!(text.contains('⠋'), "the loading spinner is shown");
    }

    #[test]
    fn test_draw_error_shows_message_and_retry_hint() {
        let state = error_state("manifest.json is malformed");
        let text = rendered(&state, 0, 80, 24);
        assert!(
            text.contains("manifest.json is malformed"),
            "the error message is shown"
        );
        assert!(
            text.contains("press R to retry"),
            "the R retry affordance is shown"
        );
    }

    // --- Ready: populated body -----------------------------------------------

    #[test]
    fn test_draw_ready_shows_equity_attribution_and_fills() {
        // A populated bundle at the head renders every panel: the equity chart, the
        // by-Greek attribution (as authored), and the fills drill-down.
        let mut state = state_from(rich_bundle(6));
        // Seek to the last step so all fills are visible and the head is at the end.
        let _ = state.seek(SeekTo::Step(u32::MAX));
        let text = rendered(&state, 0, 120, 40);
        assert!(text.contains("equity"), "the equity panel is titled");
        assert!(text.contains("drawdown"), "the drawdown panel is shown");
        assert!(
            text.contains("P&L attribution"),
            "the attribution panel is shown"
        );
        assert!(text.contains("theta"), "a by-Greek term label is shown");
        assert!(text.contains("fills"), "the fills panel is shown");
        // Attribution money is formatted from cents to `$` at this edge.
        assert!(
            text.contains("$1,930.00"),
            "theta cents render as $1,930.00: {text:?}"
        );
        // The sign glyphs carry the P&L sign under NO_COLOR.
        assert!(text.contains('−'), "a negative term shows the − glyph");
    }

    #[test]
    fn test_draw_ready_empty_run_renders_deliberate_empty_states() {
        // A valid but zero-row run (no equity, no fills, no greeks) never blanks or
        // panics — each panel shows its explicit empty state.
        let state = state_from(loaded_bundle(0));
        assert!(matches!(state.bundle, BundleLoad::Ready(_)));
        let text = rendered(&state, 0, 120, 40);
        assert!(
            text.contains("no equity rows"),
            "the equity empty state is shown"
        );
        assert!(text.contains("no fills"), "the fills empty state is shown");
        // The attribution panel renders `—` for every absent term, never a fake 0.
        assert!(text.contains('—'), "absent attribution renders an em dash");
    }

    #[test]
    fn test_draw_never_panics_over_states_and_small_sizes() {
        let states = [
            loading_state(),
            error_state(
                "bundle failed: a long error message that must wrap across a narrow \
                 body without panicking or clipping the retry affordance off-screen",
            ),
            state_from(loaded_bundle(0)),
            state_from(rich_bundle(6)),
        ];
        for state in &states {
            for (w, h) in [(1u16, 1u16), (5, 3), (12, 4), (40, 8), (80, 24), (200, 60)] {
                let _ = rendered(state, 7, w, h);
            }
        }
    }

    // --- drill-down selection stepping ---------------------------------------

    #[test]
    fn test_next_fill_selects_and_steps_over_visible_fills() {
        let mut state = state_from(rich_bundle(6));
        let _ = state.seek(SeekTo::Step(u32::MAX)); // all three fills visible
        // No selection yet → `.` lands on the most recent visible fill (step 1).
        let follow = handle_key(&mut state, press(KeyCode::Char('.')));
        assert!(
            follow.is_none(),
            "a drill-down key emits no follow-on event"
        );
        match state.loaded().and_then(|l| l.selection.as_ref()) {
            Some(fill) => assert_eq!(fill.step, 1, "`.` from none selects the most recent fill"),
            None => panic!("`.` must select a fill"),
        }
        // `,` steps to the previous (older) fill — the step-0 order 11 fill.
        let _ = handle_key(&mut state, press(KeyCode::Char(',')));
        match state.loaded().and_then(|l| l.selection.as_ref()) {
            Some(fill) => {
                assert_eq!(fill.step, 0);
                assert_eq!(fill.order_id, 11, "`,` steps back one fill");
            }
            None => panic!("selection must persist"),
        }
    }

    #[test]
    fn test_prev_next_fill_clamp_at_the_ends() {
        let mut state = state_from(rich_bundle(6));
        let _ = state.seek(SeekTo::Step(u32::MAX));
        // Walk back past the start: clamps at the oldest visible fill (order 10).
        for _ in 0..8 {
            let _ = handle_key(&mut state, press(KeyCode::Char(',')));
        }
        match state.loaded().and_then(|l| l.selection.as_ref()) {
            Some(fill) => assert_eq!(fill.order_id, 10, "`,` clamps at the oldest fill"),
            None => panic!("selection must exist"),
        }
        // Walk forward past the end: clamps at the newest visible fill (step 1).
        for _ in 0..8 {
            let _ = handle_key(&mut state, press(KeyCode::Char('.')));
        }
        match state.loaded().and_then(|l| l.selection.as_ref()) {
            Some(fill) => assert_eq!(fill.step, 1, "`.` clamps at the newest fill"),
            None => panic!("selection must exist"),
        }
    }

    #[test]
    fn test_drill_down_on_empty_fills_is_a_safe_noop() {
        // A run with no fills: stepping the drill-down never panics and never sets a
        // fabricated selection.
        let mut state = state_from(loaded_bundle(4));
        let _ = handle_key(&mut state, press(KeyCode::Char('.')));
        assert!(
            state.loaded().and_then(|l| l.selection.as_ref()).is_none(),
            "no fills → no selection",
        );
    }

    #[test]
    fn test_selection_follows_the_head_when_scrubbing_back() {
        // Drill into the step-1 fill at the end, then scrub back to step 0 — the
        // selected fill is no longer visible, so the selection clamps to None.
        let mut state = state_from(rich_bundle(6));
        let _ = state.seek(SeekTo::Step(u32::MAX));
        let _ = handle_key(&mut state, press(KeyCode::Char('.'))); // selects step 1
        assert!(state.loaded().and_then(|l| l.selection.as_ref()).is_some());
        let _ = state.seek(SeekTo::Step(0)); // step-1 fill scrubs out of view
        assert!(
            state.loaded().and_then(|l| l.selection.as_ref()).is_none(),
            "the selection follows the visible fills at the cursor (clamped to None)",
        );
    }

    // --- scroll-to-selection: the fills list follows the selection (#57) ------

    #[test]
    fn test_off_window_selection_scrolls_into_view() {
        // Twelve fills, all visible at the head; step the selection to the OLDEST fill
        // (step 0, quantity 1), which sits far above the default recent-tape window at a
        // modest height. The list must SCROLL UP so the selected fill is on screen — the
        // #57 follow-the-selection polish that replaced the earlier off-window `↑`
        // indicator.
        let mut state = state_from(many_fills_bundle(12));
        let _ = state.seek(SeekTo::Step(u32::MAX));
        for _ in 0..20 {
            let _ = handle_key(&mut state, press(KeyCode::Char(',')));
        }
        match state.loaded().and_then(|l| l.selection.as_ref()) {
            Some(fill) => assert_eq!(fill.step, 0, "`,` clamps at the oldest fill"),
            None => panic!("a selection must exist"),
        }
        let selected = match state.loaded().and_then(|l| l.selection.clone()) {
            Some(f) => f,
            None => panic!("a selection must exist"),
        };
        let newest = match state.loaded().and_then(|l| l.bundle.fills.last().cloned()) {
            Some(f) => f,
            None => panic!("the bundle has fills"),
        };
        // At a height where the fills list shows only a couple of rows, the window has
        // scrolled to the top: the selected step-0 fill renders highlighted (the `▸`
        // row built by `fill_line`), and the newest fill (step 11) is scrolled off.
        let text = rendered(&state, 0, 120, 16);
        let selected_row = line_text(&fill_line(&selected, true, theme()));
        let newest_row = line_text(&fill_line(&newest, false, theme()));
        assert!(
            !text.contains("selected fill ↑"),
            "the earlier off-window indicator is gone — the list follows the selection: {text:?}",
        );
        assert!(
            text.contains(&selected_row),
            "the selected step-0 fill scrolled into view, highlighted ({selected_row:?}): {text:?}",
        );
        assert!(
            !text.contains(&newest_row),
            "the newest fill (step 11) scrolled off the top-anchored window: {text:?}",
        );
        assert!(
            text.contains("v1:BTC"),
            "the detail panel still shows the selected fill: {text:?}"
        );
    }

    #[test]
    fn test_in_window_selection_stays_bottom_anchored() {
        // Select the most recent fill (step 11) — always the bottom row of the default
        // window, so the window does NOT scroll: the newest fill is highlighted in place
        // and the oldest (step 0) is not pulled on-screen.
        let mut state = state_from(many_fills_bundle(12));
        let _ = state.seek(SeekTo::Step(u32::MAX));
        let _ = handle_key(&mut state, press(KeyCode::Char('.')));
        let newest = match state.loaded().and_then(|l| l.bundle.fills.last().cloned()) {
            Some(f) => f,
            None => panic!("the bundle has fills"),
        };
        let oldest = match state.loaded().and_then(|l| l.bundle.fills.first().cloned()) {
            Some(f) => f,
            None => panic!("the bundle has fills"),
        };
        let text = rendered(&state, 0, 120, 16);
        assert!(
            !text.contains("selected fill ↑"),
            "no off-window indicator in the follow model: {text:?}"
        );
        assert!(
            text.contains(&line_text(&fill_line(&newest, true, theme()))),
            "the newest fill stays highlighted at the bottom-anchored window: {text:?}",
        );
        assert!(
            !text.contains(&line_text(&fill_line(&oldest, false, theme()))),
            "the oldest fill is not pulled on-screen: {text:?}",
        );
    }

    #[test]
    fn test_fills_window_start_follows_selection() {
        use super::fills_window_start;
        // Everything fits → start 0 regardless of selection.
        assert_eq!(fills_window_start(3, 10, Some(0)), 0);
        assert_eq!(fills_window_start(10, 10, None), 0);
        // A zero-row viewport never indexes.
        assert_eq!(fills_window_start(20, 0, Some(5)), 0);
        // No selection (or a selection already in the bottom window) → newest anchored.
        assert_eq!(fills_window_start(20, 5, None), 15);
        assert_eq!(
            fills_window_start(20, 5, Some(18)),
            15,
            "in-window keeps anchor"
        );
        assert_eq!(
            fills_window_start(20, 5, Some(15)),
            15,
            "at the window top edge"
        );
        // A selection above the default window scrolls up so it is the top visible row.
        assert_eq!(
            fills_window_start(20, 5, Some(3)),
            3,
            "scrolls to the selection"
        );
        assert_eq!(
            fills_window_start(20, 5, Some(0)),
            0,
            "oldest → top of the list"
        );
    }

    #[test]
    fn test_selected_fill_index_is_cached_off_draw() {
        // #118: the fills-list scroll anchor (the selection's index in the as-of tape)
        // is resolved OFF the draw path — on a `,`/`.` step and on a cursor move — and
        // read by draw in O(1), so rendering never rescans the whole visible history.
        let mut state = state_from(many_fills_bundle(12));
        let _ = state.seek(SeekTo::Step(u32::MAX)); // fills 0..12 all visible
        assert_eq!(
            state
                .loaded()
                .and_then(crate::app::LoadedReplay::selected_fill_index),
            None,
            "no selection → no cached index",
        );
        // `.` selects the newest fill (step 11) → the last tape index.
        let _ = handle_key(&mut state, press(KeyCode::Char('.')));
        assert_eq!(
            state
                .loaded()
                .and_then(crate::app::LoadedReplay::selected_fill_index),
            Some(11),
            "`.` caches the newest fill's tape index",
        );
        // `,` steps back one → the cached index decrements.
        let _ = handle_key(&mut state, press(KeyCode::Char(',')));
        assert_eq!(
            state
                .loaded()
                .and_then(crate::app::LoadedReplay::selected_fill_index),
            Some(10),
            "`,` decrements the cached index",
        );
        // The cached index always agrees with a fresh scan of the visible tape.
        match state.loaded() {
            Some(loaded) => {
                let visible = loaded.cursor.visible_fills(&loaded.bundle);
                let scanned = loaded.selection_key().and_then(|key| {
                    visible
                        .iter()
                        .position(|f| (f.step, f.order_id, f.fill_seq) == key)
                });
                assert_eq!(
                    loaded.selected_fill_index(),
                    scanned,
                    "the cached index equals a fresh position scan",
                );
            }
            None => panic!("the bundle is loaded"),
        }
        // Scrub the head back past the selected fill (step 10) → the selection AND its
        // cached index clear together (the off-draw reclamp maintains both).
        let _ = state.seek(SeekTo::Step(5));
        assert_eq!(
            state
                .loaded()
                .and_then(crate::app::LoadedReplay::selected_fill_index),
            None,
            "a selection scrubbed out of the as-of window clears its cached index",
        );
        assert!(
            state.loaded().and_then(|l| l.selection.as_ref()).is_none(),
            "the selection clears together with its index",
        );
    }

    // --- the equity revision drives off-draw re-projection -------------------

    #[test]
    fn test_seek_bumps_equity_revision_but_a_clamped_noop_does_not() {
        let mut state = state_from(rich_bundle(6));
        let rev0 = state
            .loaded()
            .map(crate::app::LoadedReplay::equity_revision)
            .unwrap_or_default();
        let moved = state.seek(SeekTo::Step(3));
        assert!(moved, "a real seek moves the cursor");
        let rev1 = state
            .loaded()
            .map(crate::app::LoadedReplay::equity_revision)
            .unwrap_or_default();
        assert_ne!(
            rev0, rev1,
            "a cursor move bumps the equity revision (re-project)"
        );
        // A seek to the same step is a no-op → no revision bump (no over-invalidation).
        let noop = state.seek(SeekTo::Step(3));
        assert!(!noop, "seeking to the same step is a no-op");
        let rev2 = state
            .loaded()
            .map(crate::app::LoadedReplay::equity_revision)
            .unwrap_or_default();
        assert_eq!(rev1, rev2, "a no-op seek does not re-project");
    }

    // --- drawdown is seeded from the opening capital (#35, Fix 1) -------------

    /// The base manifest with an explicit `config.initial_capital` (the #29 writer
    /// shape) so the loaded payload reads a real opening capital.
    fn manifest_with_capital(initial_capital: u64) -> BundleManifest {
        let mut m = manifest();
        m.config = serde_json::json!({ "initial_capital": initial_capital });
        m
    }

    /// An equity-only bundle over `0..n` with a non-monotonic curve (so playback
    /// crosses drawdowns) and an explicit opening capital.
    fn capital_bundle(n: u32, initial_capital: u64) -> LoadedBundle {
        LoadedBundle {
            manifest: manifest_with_capital(initial_capital),
            fills: Vec::new(),
            equity: (0..n)
                .map(|s| {
                    let wobble = (i64::from(s % 41) - 20) * 1_500;
                    equity(s, 1_000_000 + wobble, 0.0)
                })
                .collect(),
            positions: Vec::new(),
            greeks: Vec::new(),
        }
    }

    #[test]
    fn test_step0_loss_shows_drawdown_vs_opening_capital() {
        // Opening capital $10,000.00 (1_000_000c); step 0 closes at $9,900.00 — a loss
        // on the very first step. Seeding the running peak from the opening capital (not
        // the first equity row) surfaces the −$100.00 step-0 loss as a −10_000c drawdown
        // at the head; a first-row seed would (wrongly) report $0.
        let bundle = LoadedBundle {
            manifest: manifest_with_capital(1_000_000),
            fills: Vec::new(),
            equity: vec![equity(0, 990_000, -0.01), equity(1, 1_050_000, 0.0)],
            positions: Vec::new(),
            greeks: Vec::new(),
        };
        let state = state_from(bundle);
        assert_eq!(
            state
                .loaded()
                .map(crate::app::LoadedReplay::peak_drawdown_cents),
            Some(-10_000),
            "a step-0 loss shows its true drawdown vs the opening capital, not $0",
        );
    }

    #[test]
    fn test_forward_steps_match_a_direct_seek_across_a_stride_boundary() {
        // > MAX_EQUITY_POINTS (512) rows so stepping forward crosses a downsample-stride
        // boundary. Stepping forward one at a time (the incremental extend path) lands on
        // exactly the same cached peak + series as one arbitrary seek to the end (the
        // full-rebuild path): incremental == full, determinism preserved.
        let n = 600u32;
        let cap = 1_000_000u64;
        let mut stepper = state_from(capital_bundle(n, cap));
        for _ in 0..n {
            let _ = stepper.seek(SeekTo::StepBy(1));
        }
        let mut seeker = state_from(capital_bundle(n, cap));
        let _ = seeker.seek(SeekTo::Step(u32::MAX));

        let step_peak = stepper
            .loaded()
            .map(crate::app::LoadedReplay::peak_drawdown_cents);
        let seek_peak = seeker
            .loaded()
            .map(crate::app::LoadedReplay::peak_drawdown_cents);
        assert_eq!(
            step_peak, seek_peak,
            "the incremental forward peak equals the arbitrary-seek rebuild peak",
        );

        // The projected series agree too (same head → same geometry).
        let step_series = match stepper.loaded().map(crate::app::LoadedReplay::equity_graph) {
            Some(GraphData::Series(s)) => (s.x.clone(), s.y.clone()),
            other => panic!("expected a Series, got {other:?}"),
        };
        let seek_series = match seeker.loaded().map(crate::app::LoadedReplay::equity_graph) {
            Some(GraphData::Series(s)) => (s.x.clone(), s.y.clone()),
            other => panic!("expected a Series, got {other:?}"),
        };
        assert_eq!(
            step_series, seek_series,
            "the incremental forward series equals the arbitrary-seek rebuild series",
        );
    }

    #[test]
    fn test_backward_seek_rebuilds_the_drawdown_from_the_seed() {
        // A forward run to the end, then a backward seek to an earlier head: the
        // rebuild path recomputes the drawdown over the shorter slice from the preserved
        // opening-capital seed — equal to loading fresh and seeking directly there.
        let n = 40u32;
        let cap = 1_000_000u64;
        let mut state = state_from(capital_bundle(n, cap));
        let _ = state.seek(SeekTo::Step(u32::MAX)); // forward to the end
        let _ = state.seek(SeekTo::Step(9)); // backward jump → full rebuild

        let mut reference = state_from(capital_bundle(n, cap));
        let _ = reference.seek(SeekTo::Step(9)); // fresh forward-extend to the same head

        assert_eq!(
            state
                .loaded()
                .map(crate::app::LoadedReplay::peak_drawdown_cents),
            reference
                .loaded()
                .map(crate::app::LoadedReplay::peak_drawdown_cents),
            "the backward rebuild reproduces the drawdown at the earlier head",
        );
    }

    // --- money + display formatters ------------------------------------------

    #[test]
    fn test_fmt_cents_abs_groups_thousands_with_two_decimals() {
        assert_eq!(fmt_cents_abs(0), "$0.00");
        assert_eq!(fmt_cents_abs(5), "$0.05");
        assert_eq!(fmt_cents_abs(235), "$2.35");
        assert_eq!(fmt_cents_abs(193_000), "$1,930.00");
        assert_eq!(fmt_cents_abs(123_456_789), "$1,234,567.89");
    }

    #[test]
    fn test_group_thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(12), "12");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn test_fmt_signed_cents_carries_the_sign_glyph() {
        assert_eq!(fmt_signed_cents(0), "+$0.00");
        assert_eq!(fmt_signed_cents(42), "+$0.42");
        assert_eq!(fmt_signed_cents(-15), "−$0.15");
        // i64::MIN magnitude does not overflow (unsigned_abs).
        let _ = fmt_signed_cents(i64::MIN);
    }

    #[test]
    fn test_fmt_drawdown_cents_signs_only_the_negative() {
        assert_eq!(
            fmt_drawdown_cents(0),
            "$0.00",
            "a peak reads $0.00, no sign"
        );
        assert_eq!(fmt_drawdown_cents(-124_000), "−$1,240.00");
    }

    #[test]
    fn test_fmt_drawdown_ratio_guards_nan_inf_and_absent() {
        // The magnitude is formatted `.abs()` behind the shared U+2212 `−` / `+` sign
        // glyph — never an ASCII `-`.
        assert_eq!(fmt_drawdown_ratio(Some(-0.032)), "−3.2%");
        assert!(fmt_drawdown_ratio(Some(-0.032)).starts_with('\u{2212}'));
        assert_eq!(fmt_drawdown_ratio(Some(0.0)), "+0.0%");
        assert_eq!(fmt_drawdown_ratio(None), "—", "absent renders an em dash");
        assert_eq!(fmt_drawdown_ratio(Some(f64::NAN)), "—", "NaN is guarded");
        assert_eq!(
            fmt_drawdown_ratio(Some(f64::INFINITY)),
            "—",
            "Inf is guarded"
        );
    }

    #[test]
    fn test_fmt_axis_cents_groups_thousands_and_guards_non_finite() {
        assert_eq!(fmt_axis_cents(100_000.0), "$1,000");
        assert_eq!(fmt_axis_cents(100_000_000.0), "$1,000,000");
        assert_eq!(fmt_axis_cents(0.0), "$0");
        assert_eq!(fmt_axis_cents(-100_000.0), "−$1,000");
        assert_eq!(fmt_axis_cents(f64::NAN), "—");
        assert_eq!(fmt_axis_cents(f64::INFINITY), "—");
    }

    #[test]
    fn test_attribution_line_elides_amount_at_narrow_width() {
        let theme = theme();
        let row = AttribRow::signed("Θ theta", 193_000); // "$1,930.00"
        // A wide panel shows the complete amount (with room for the bar).
        let wide = line_text(&attribution_line(&row, 193_000, 48, theme));
        assert!(
            wide.contains("$1,930.00"),
            "a wide panel shows the complete amount: {wide:?}"
        );
        // A medium panel drops the BAR before the amount, so the amount stays complete
        // and un-elided (amount outranks bar in priority).
        let medium = line_text(&attribution_line(&row, 193_000, 26, theme));
        assert!(
            medium.contains("$1,930.00") && !medium.contains('…'),
            "the bar drops before the amount is elided: {medium:?}"
        );
        // A narrow panel cannot fit the amount, so it is visibly elided with a trailing
        // `…` — never a bare misreadable prefix like `$1`.
        let narrow = line_text(&attribution_line(&row, 193_000, 16, theme));
        assert!(
            narrow.contains('…'),
            "a narrow panel visibly elides the amount: {narrow:?}"
        );
        assert!(
            !narrow.contains("$1,930.00"),
            "the complete amount does not fit at a narrow width: {narrow:?}"
        );
    }

    #[test]
    fn test_attribution_line_width_floor_holds_at_the_boundary() {
        // The width floors (`floor_sub`) bottom out at zero instead of underflowing.
        let theme = theme();
        let row = AttribRow::signed("Θ theta", 193_000); // "$1,930.00"
        // At width 0 and at exactly label+sign width, there is no room for the amount:
        // the line renders the label + sign only, never a panic, never a bare prefix.
        for w in [0usize, ATTRIB_LABEL_WIDTH + 1] {
            let text = line_text(&attribution_line(&row, 193_000, w, theme));
            assert!(
                text.starts_with("Θ theta"),
                "the label still renders at width {w}: {text:?}",
            );
            assert!(
                !text.contains("$1,930.00"),
                "no room for the amount at width {w}: {text:?}",
            );
        }
        // Exactly one cell past the sign: the floor yields 1, so the amount elides to a
        // lone `…` marker (the boundary of the bar-vs-amount budget).
        let tight = line_text(&attribution_line(
            &row,
            193_000,
            ATTRIB_LABEL_WIDTH + 2,
            theme,
        ));
        assert!(
            tight.contains('…'),
            "one cell past the sign elides to a marker: {tight:?}",
        );
    }

    #[test]
    fn test_bar_string_scales_and_clamps() {
        assert_eq!(bar_string(0, 0, 10), "", "no scale → empty");
        assert_eq!(
            bar_string(5, 10, 10).chars().count(),
            5,
            "half of ten cells"
        );
        assert_eq!(bar_string(10, 10, 10).chars().count(), 10, "full bar");
        assert_eq!(
            bar_string(20, 10, 10).chars().count(),
            10,
            "clamped at width"
        );
        // A huge magnitude cannot overflow the scale multiply.
        let _ = bar_string(u64::MAX, u64::MAX, 12);
    }

    // --- draw purity: drawing mutates nothing (the #28 state-identity pattern) -

    #[test]
    fn test_draw_does_not_mutate_state_or_reproject() {
        // The draw reads the cached projection and the O(1) head rows; it builds no
        // GraphData and recomputes no attribution — so the equity revision and the
        // selection are byte-for-byte unchanged across a draw.
        let mut state = state_from(rich_bundle(6));
        let _ = state.seek(SeekTo::Step(u32::MAX));
        let _ = handle_key(&mut state, press(KeyCode::Char('.')));
        let rev_before = state
            .loaded()
            .map(crate::app::LoadedReplay::equity_revision)
            .unwrap_or_default();
        let sel_before = state.loaded().and_then(|l| l.selection_key());
        let _ = rendered(&state, 3, 120, 40);
        let rev_after = state
            .loaded()
            .map(crate::app::LoadedReplay::equity_revision)
            .unwrap_or_default();
        let sel_after = state.loaded().and_then(|l| l.selection_key());
        assert_eq!(
            rev_before, rev_after,
            "draw must not re-project the equity series"
        );
        assert_eq!(sel_before, sel_after, "draw must not move the selection");
    }
}
