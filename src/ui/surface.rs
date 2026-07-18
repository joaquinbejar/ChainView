//! The volatility smile / Greek-curve / single-expiry-surface screen (#47,
//! `docs/05-views-and-ux.md` §4).
//!
//! Three views, cycled by `x` (`smile → curve → surface → smile`): the **vol smile**
//! (`VolatilitySmile::smile()`, IV vs strike), a **Greek / IV / Price curve**
//! (`BasicCurves::curve`, the `g`/`G` axis vs strike), and the **single-expiry
//! surface** (`BasicSurfaces::surface`, the axis Greek/Price over strike ×
//! volatility). Every `GraphData` is built and cached in the **application** layer
//! ([`SurfacePanel`](crate::app::SurfacePanel)) and projected **off** the draw path by
//! [`ViewState::sync`](crate::ViewState); [`draw`] reads only the cached
//! [`GraphProjection`] and never builds geometry, prices, or performs I/O
//! (`docs/02-tui-architecture.md` §7).
//!
//! # States first
//!
//! [`draw`] renders the **loading** state (a tick-driven spinner while the first
//! chain streams), the provider **error** state, and the deliberate **insufficient
//! IV** empty state — each a first-class, centered body, never a blank void — before
//! the smile / curve / surface happy paths.
//!
//! # Honest axes and the map's corrections
//!
//! IV is a **fraction** upstream (`0.20` = 20%); the smile / IV-curve `y`-axis is
//! formatted as a **percent at the render edge** only. The 3D surface's `z` is a
//! **Greek or Price — never IV** (`BasicSurfaces` rejects the volatility axis), so the
//! surface view honestly reports that the `IV` axis has no surface projection and
//! points back to the smile. The surface heat map maps `z` intensity to a
//! `NO_COLOR`-safe glyph ramp (light → dense), so its structure survives a monochrome
//! terminal (color is never the only signal).

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph};

use crate::app::keymap::{KeyChord, SurfaceAction, resolve_surface};
use crate::app::{LiveState, ScreenLoad, SurfaceAxis, SurfaceView};
use crate::chain::StreamHealth;
use crate::event::AppEvent;
use crate::ui::graph::{EmptyReason, GraphProjection, ProjectedSeries, ProjectedSurface};
use crate::ui::theme::{Theme, health_span, sanitize, spinner_frame};

/// The glyph ramp for the surface heat map, light → dense — the `NO_COLOR`-safe
/// intensity signal (a present `z` cell maps to one of these by its normalized value,
/// so the surface reads on a monochrome terminal, `docs/05-views-and-ux.md` §7).
///
/// The ramp is **all ink**: it starts at `·`, never a blank space (SF-03). A space
/// has no ink, so it is reserved **strictly** for a data gap (a `None` cell, painted
/// by [`cell_span`]) — a present cell at minimum intensity renders `·`, keeping a
/// present-at-minimum cell visibly distinct from a gap on any terminal.
const RAMP: [char; 7] = ['·', ':', '+', '*', '#', '%', '@'];

/// The em dash for an unknown value — never a fabricated `0`.
const EM_DASH: &str = "—";

// ===========================================================================
// The draw entry point + states (states first).
// ===========================================================================

/// Draw the surface screen for `state` into `area` — a pure render over the borrowed
/// panel state and the pre-projected `surface` geometry (`docs/02-tui-architecture.md`
/// §7). `surface` is the ui view-cache's projection, computed **off** the draw path by
/// [`ViewState::sync`](crate::ViewState); this paint builds no `GraphData` and prices
/// nothing.
///
/// States first (`docs/05-views-and-ux.md` §6): the **loading** spinner, the provider
/// **error** message, then — on a ready feed — the active view (smile / curve /
/// surface) or its deliberate **insufficient IV** empty state. Never a blank, never a
/// panic.
pub fn draw(
    state: &LiveState,
    surface: &GraphProjection,
    frame: &mut Frame,
    area: Rect,
    theme: Theme,
    tick: u64,
) {
    let panel = &state.surface;
    // A dropped stream never blanks the surface: the last-known smile/curve/surface
    // renders **dimmed** with the stream-health badge in the title, mirroring the
    // chain/depth stale idiom (`docs/05-views-and-ux.md` §6). The health is the
    // OVERLAY-AWARE health (source ∪ overlay, #118): the surface geometry consumes
    // Greeks/IV a standalone overlay may supply, so a stale overlay dims + badges the
    // surface even while the chain source is live. A borrowed read, so the draw stays
    // pure.
    let health = state.overlay_aware_health();
    let stale = !matches!(health, StreamHealth::Live);
    let block = Block::bordered().title(title_line(panel.view(), panel.axis(), theme, health));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &state.load {
        ScreenLoad::Loading => {
            draw_state_body(
                frame,
                inner,
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
                inner,
                Text::from(vec![
                    Line::from(Span::styled(
                        format!("! {}", sanitize(message)),
                        theme.warning(),
                    )),
                    Line::from(Span::styled("press r to reconnect", theme.dim())),
                ]),
            );
        }
        ScreenLoad::Ready => match panel.view() {
            SurfaceView::Smile => {
                draw_series(frame, inner, theme, surface, "strike", "IV", true, stale)
            }
            SurfaceView::Curve => draw_series(
                frame,
                inner,
                theme,
                surface,
                "strike",
                panel.axis().label(),
                panel.axis() == SurfaceAxis::Volatility,
                stale,
            ),
            SurfaceView::Surface => draw_surface(frame, inner, theme, panel.axis(), surface, stale),
        },
    }
}

/// The framed-block title: `Surface  <view>` plus a view-appropriate axis span.
///
/// - **Smile** (SF-01): the axis is a curve/surface concern, so it renders as a **dim,
///   non-load-bearing hint** (`axis: <axis> (curve/surface)`). This gives `g`/`G` a
///   visible effect in the smile view — the title moves — without implying the smile's
///   IV-vs-strike geometry depends on the axis.
/// - **Curve**: the accent axis label (the axis the curve plots).
/// - **Surface** (SF-02): the accent axis label — **except** the `Volatility` axis,
///   which the surface body refuses (the 3D `z` is a Greek/Price, never IV). There the
///   title is annotated `IV (n/a)` so it never asserts an "IV surface" the body denies.
///
/// Exhaustive over [`SurfaceView`] with no wildcard arm. The stream-health badge is
/// appended when the feed is not live (`◐ stale` / `↻ reconnecting (n)`), so a dropped
/// stream is honestly signalled while the last-known geometry stays on screen dimmed
/// (`docs/05-views-and-ux.md` §6). The badge glyph+text is `NO_COLOR`-safe.
#[must_use]
fn title_line(
    view: SurfaceView,
    axis: SurfaceAxis,
    theme: Theme,
    health: &StreamHealth,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled("Surface", theme.accent()),
        Span::styled(format!("  {}", view.label()), theme.dim()),
    ];
    match view {
        SurfaceView::Smile => spans.push(Span::styled(
            format!("  axis: {} (curve/surface)", axis.label()),
            theme.dim(),
        )),
        SurfaceView::Curve => {
            spans.push(Span::styled(format!("  {}", axis.label()), theme.accent()));
        }
        SurfaceView::Surface => {
            if axis == SurfaceAxis::Volatility {
                spans.push(Span::styled("  IV (n/a)", theme.warning()));
            } else {
                spans.push(Span::styled(format!("  {}", axis.label()), theme.accent()));
            }
        }
    }
    if !matches!(health, StreamHealth::Live) {
        spans.push(Span::raw("  "));
        spans.push(health_span(health, theme));
    }
    Line::from(spans)
}

// ===========================================================================
// The 2D line views: the smile and the Greek/IV/Price curve.
// ===========================================================================

/// Draw a 2D line view (the smile or a Greek curve): the projected series as a
/// ratatui [`Chart`], or the deliberate "insufficient IV" empty state when the
/// projection carries no renderable series. `y_percent` formats the `y`-axis as a
/// percent (IV is a fraction upstream). `stale` dims the plotted line when the stream
/// dropped, so the last-known curve reads as visibly untrusted beneath the title badge
/// (`docs/05-views-and-ux.md` §6).
#[allow(clippy::too_many_arguments)]
fn draw_series(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    projection: &GraphProjection,
    x_title: &str,
    y_title: &str,
    y_percent: bool,
    stale: bool,
) {
    let Some(series) = projection.ready() else {
        draw_state_body(
            frame,
            inner,
            insufficient_text(theme, projection.empty_reason()),
        );
        return;
    };
    draw_chart(
        frame, inner, theme, series, x_title, y_title, y_percent, stale,
    );
}

/// Render the projected `series` as a ratatui line [`Chart`] with the numeric axis
/// bounds and precomputed labels — a pure paint over the cached projection. When
/// `stale`, the series line is drawn dim (never a bright, trusted-looking curve over a
/// stale badge), mirroring the chain/depth stale idiom (`docs/05-views-and-ux.md` §6).
#[allow(clippy::too_many_arguments)]
fn draw_chart(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    series: &ProjectedSeries,
    x_title: &str,
    y_title: &str,
    y_percent: bool,
    stale: bool,
) {
    let series_style = if stale { theme.dim() } else { theme.accent() };
    let dataset = Dataset::default()
        .name(series.name().to_owned())
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(series_style)
        .data(series.points());
    let y_labels = if y_percent {
        percent_labels(series.y_bounds())
    } else {
        raw_labels(series.y_labels())
    };
    let chart = Chart::new(vec![dataset])
        .x_axis(
            Axis::default()
                .title(x_title.to_owned())
                .bounds(series.x_bounds())
                .labels(raw_labels(series.x_labels()))
                .style(theme.dim()),
        )
        .y_axis(
            Axis::default()
                .title(y_title.to_owned())
                .bounds(series.y_bounds())
                .labels(y_labels)
                .style(theme.dim()),
        );
    frame.render_widget(chart, inner);
}

// ===========================================================================
// The 3D surface heat map.
// ===========================================================================

/// Draw the single-expiry surface heat map, or the honest empty state. The
/// `Volatility` axis has **no** surface projection — the `z` is a Greek/Price, never
/// IV (the #47 map) — so it reports that and points back to the smile; otherwise the
/// projected grid renders as a glyph ramp, or the "insufficient IV" state when empty.
/// `stale` dims the whole grid when the stream dropped, so the 3D heat view reads as
/// visibly untrusted beneath the title badge — the same stale idiom as the 2D chart
/// (`docs/05-views-and-ux.md` §6).
fn draw_surface(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    axis: SurfaceAxis,
    projection: &GraphProjection,
    stale: bool,
) {
    if axis == SurfaceAxis::Volatility {
        draw_state_body(
            frame,
            inner,
            Text::from(vec![
                Line::from(Span::styled(
                    "IV has no surface — the 3D z is a Greek/Price",
                    theme.warning(),
                )),
                Line::from(Span::styled("press x for the IV smile", theme.dim())),
            ]),
        );
        return;
    }
    let Some(surface) = projection.ready_surface() else {
        draw_state_body(
            frame,
            inner,
            insufficient_text(theme, projection.empty_reason()),
        );
        return;
    };
    draw_heatmap(frame, inner, theme, surface, stale);
}

/// Paint the projected surface as a character heat map: a header (the `z` metric and
/// its numeric range, `NO_COLOR`-safe), the glyph grid (columns downsampled to fit
/// the width, rows to the height), and a footer (the strike range and the glyph-ramp
/// legend). O(visible cells) — the grid was precomputed off the draw path. When
/// `stale`, every present cell is dimmed ([`cell_span`]) so the grid never reads
/// bright/trusted under a `◐ stale` badge; the ramp glyph's shape still carries the
/// structure (`NO_COLOR`-safe).
fn draw_heatmap(
    frame: &mut Frame,
    inner: Rect,
    theme: Theme,
    surface: &ProjectedSurface,
    stale: bool,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Header: the z (Greek/Price) metric and its range as text (numbers survive
    // NO_COLOR); the vol range on the y axis, annotated top/bottom so the vertical
    // direction is unambiguous — the top row is the highest vol (P3-02).
    let header_line = Line::from(vec![
        Span::styled(format!("z {}  ", sanitize(surface.name())), theme.accent()),
        Span::styled(
            format!(
                "[{} .. {}]",
                label_at(surface.z_labels(), 0),
                label_at(surface.z_labels(), 2)
            ),
            theme.dim(),
        ),
        Span::styled(
            format!(
                "  vol {}% top .. {}% bottom",
                fmt_percent(surface.y_bounds()[1]),
                fmt_percent(surface.y_bounds()[0])
            ),
            theme.dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(header_line), header);

    // The grid body — sampled to fit the body rect.
    let rows = surface.rows();
    let width = usize::from(body.width);
    let height = usize::from(body.height);
    let col_count = rows.first().map_or(0, Vec::len);
    let row_idx = sample_indices(rows.len(), height);
    let col_idx = sample_indices(col_count, width);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(row_idx.len());
    for r in &row_idx {
        let Some(row) = rows.get(*r) else { continue };
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_idx.len());
        for c in &col_idx {
            let cell = row.get(*c).copied().flatten();
            spans.push(cell_span(cell, theme, stale));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), body);

    // Footer: the strike range under the columns + the glyph-ramp legend (low→high).
    let ramp: String = RAMP.iter().collect();
    let footer_line = Line::from(vec![
        Span::styled(
            format!(
                "strike {} .. {}",
                label_at(surface.x_labels(), 0),
                label_at(surface.x_labels(), 2)
            ),
            theme.dim(),
        ),
        Span::styled(format!("   low {ramp} high"), theme.dim()),
    ]);
    frame.render_widget(Paragraph::new(footer_line), footer);
}

/// One heat-map cell as a styled [`Span`]: the ramp glyph for its normalized `z`
/// intensity (a gap is a blank), tinted by intensity so color reinforces — but never
/// replaces — the glyph (`NO_COLOR`-safe). When `stale`, every present cell is dimmed
/// regardless of intensity, so a dropped stream never paints a bright, trusted-looking
/// heat grid under the `◐ stale` badge — the ramp glyph shape still carries the
/// structure (`docs/05-views-and-ux.md` §6, #57 P2-01).
#[must_use]
fn cell_span(cell: Option<f64>, theme: Theme, stale: bool) -> Span<'static> {
    match cell {
        Some(intensity) => {
            let glyph = ramp_glyph(intensity);
            let style = if stale {
                theme.dim()
            } else if intensity >= 0.66 {
                theme.accent()
            } else if intensity >= 0.33 {
                theme.warning()
            } else {
                theme.dim()
            };
            Span::styled(glyph.to_string(), style)
        }
        None => Span::raw(" "),
    }
}

/// The ramp glyph for a normalized `[0, 1]` intensity — the light→dense index into
/// [`RAMP`], clamped so an out-of-range value never indexes out of bounds.
#[must_use]
fn ramp_glyph(intensity: f64) -> char {
    let last = RAMP.len().saturating_sub(1);
    let clamped = intensity.clamp(0.0, 1.0);
    // `last` fits a `u8` (RAMP is 7 long); round to the nearest ramp step.
    let idx = (clamped * last as f64).round();
    let idx = if idx.is_finite() { idx as usize } else { 0 };
    RAMP.get(idx.min(last)).copied().unwrap_or(' ')
}

/// The evenly-spaced sample indices to fit `len` items into `max` slots: all indices
/// when they fit, else `max` indices spread across `0..len`. Never an unchecked index
/// and never `saturating_*` in the arithmetic — a `0`/`1` `max` degrades cleanly.
#[must_use]
fn sample_indices(len: usize, max: usize) -> Vec<usize> {
    if len == 0 || max == 0 {
        return Vec::new();
    }
    if len <= max {
        return (0..len).collect();
    }
    let mut out = Vec::with_capacity(max);
    for i in 0..max {
        // idx = round(i * (len - 1) / (max - 1)), computed without a division-by-zero
        // (max > 1 here since len > max >= 1) and clamped into range.
        let numerator = i.checked_mul(len.max(1) - 1).unwrap_or(0);
        let denominator = (max - 1).max(1);
        out.push((numerator / denominator).min(len - 1));
    }
    out.dedup();
    out
}

// ===========================================================================
// Shared render helpers.
// ===========================================================================

/// The empty-state body, keyed on the projection's [`EmptyReason`] (exhaustive, no
/// wildcard) — an honest, **reason-specific** primary line and hint, never a blank
/// frame. Each reason states its own cause and its own next step:
///
/// - `NoData` (the common case): the deliberate "insufficient IV" state — the current
///   expiry has no reliable IV samples to build from; a reconnect may bring fresh data.
/// - `Degenerate` (P3-01): a **hard** curve/surface build `Err` — the geometry could
///   not be priced — a distinguishable "degenerate geometry" state, not "insufficient
///   IV"; a reconnect may bring a chain that prices.
/// - `Unsupported` (P3-03): a shape this view does not project. Its hint **drops** the
///   "press r to reconnect" line — reconnecting cannot make an unsupported view render,
///   so that hint would be misleading.
#[must_use]
fn insufficient_text(theme: Theme, reason: Option<EmptyReason>) -> Text<'static> {
    let (primary, hint) = match reason {
        Some(EmptyReason::NoData) | None => (
            "insufficient IV for this expiry",
            Some("no reliable IV samples yet — press r to reconnect"),
        ),
        Some(EmptyReason::Degenerate) => (
            "degenerate geometry — cannot build this view",
            Some("the expiry did not price — press r to reconnect"),
        ),
        Some(EmptyReason::Unsupported) => ("this view is not available", None),
    };
    let mut lines = vec![Line::from(Span::styled(
        primary.to_owned(),
        theme.warning(),
    ))];
    if let Some(hint) = hint {
        lines.push(Line::from(Span::styled(hint.to_owned(), theme.dim())));
    }
    Text::from(lines)
}

/// Draw a centered two-line state body (loading / error / insufficient) inside the
/// framed block — a first-class, deliberate-looking state, never a blank void or a
/// top-anchored fragment (`docs/05-views-and-ux.md` §6).
fn draw_state_body(frame: &mut Frame, inner: Rect, text: Text<'static>) {
    let height = u16::try_from(text.height())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

/// Wrap precomputed axis-label strings as owned [`Span`]s — no per-frame numeric
/// formatting (the labels were computed off the draw path on the projection).
#[must_use]
fn raw_labels(labels: &[String]) -> Vec<Span<'static>> {
    labels.iter().map(|l| Span::raw(l.clone())).collect()
}

/// The `[min, mid, max]` `y`-axis labels formatted as **percent** (IV is a fraction
/// upstream, `0.20` = `20.0%`), regenerated at the UI edge from the numeric bounds.
#[must_use]
fn percent_labels(bounds: [f64; 2]) -> Vec<Span<'static>> {
    let [lo, hi] = bounds;
    let mid = lo + (hi - lo) / 2.0;
    vec![
        Span::raw(format!("{}%", fmt_percent(lo))),
        Span::raw(format!("{}%", fmt_percent(mid))),
        Span::raw(format!("{}%", fmt_percent(hi))),
    ]
}

/// Format a fraction as a one-decimal percent string body (without the `%`), guarding
/// a non-finite value to the em dash so `NaN` never paints.
#[must_use]
fn fmt_percent(fraction: f64) -> String {
    if fraction.is_finite() {
        format!("{:.1}", fraction * 100.0)
    } else {
        EM_DASH.to_owned()
    }
}

/// The label at `idx` in a precomputed `[min, mid, max]` label vector, or the em dash
/// when absent — never an unchecked index.
#[must_use]
fn label_at(labels: &[String], idx: usize) -> &str {
    labels.get(idx).map_or(EM_DASH, String::as_str)
}

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a surface-local key, returning any follow-on [`AppEvent`]
/// (`docs/02-tui-architecture.md` §9). Pure over `&mut LiveState` — no I/O, no
/// `.await`, no `GraphData` build in the draw path.
///
/// The chord resolves **through the single keybinding map**
/// ([`resolve_surface`](crate::app::keymap::resolve_surface), `src/app/keymap.rs`), so
/// the dispatch and the help overlay read one table and cannot drift. `g`/`G` cycles
/// the Greek/IV/Price axis (`g` forward, `G` back — the concrete chord chooses the
/// direction); it applies to the curve/surface views, and in the smile view it still
/// advances the title's dim axis hint (SF-01) so the key is never a silent no-op. `x`
/// cycles the view `smile → curve → surface`. Each mutates the application-layer
/// [`SurfacePanel`](crate::app::SurfacePanel), rebuilding the active geometry **off**
/// the draw path and bumping its revision, so `handle_key` returns `None`; the render
/// loop diffs the panel revision and redraws (`docs/02-tui-architecture.md` §8).
#[must_use]
pub fn handle_key(state: &mut LiveState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_surface(chord)? {
        SurfaceAction::CycleGreek => {
            // The shared chord chooses the direction: `G` steps back, `g` (and any
            // defensive fallback) forward.
            let forward = chord != KeyChord::Char('G');
            let LiveState { store, surface, .. } = state;
            surface.cycle_axis(forward, store);
            None
        }
        SurfaceAction::ToggleView => {
            let LiveState { store, surface, .. } = state;
            surface.cycle_view(store);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use super::{cell_span, draw, handle_key, insufficient_text, ramp_glyph, sample_indices};
    use crate::app::{
        LiveState, OverlayBinding, ScreenLoad, SourceBinding, SurfaceAxis, SurfaceView,
    };
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, ProviderId, StreamHealth,
    };
    use crate::config::ThemeChoice;
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crate::ui::golden::buffer_to_text;
    use crate::ui::graph::{EmptyReason, GraphProjection, project};
    use crate::ui::theme::Theme;

    const EXP: i64 = 1_700_000_000;

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

    fn caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .greeks(GreeksCapability::Provided)
            .build()
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
            utc(EXP),
        )
    }

    /// A live state with a 3-strike realistic-premium chain (the smile / curve /
    /// surface render) on the Surface screen, in the given load state.
    fn live_state(load: ScreenLoad) -> LiveState {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(full_row(60_000.0));
        let _ = chain.options.insert(full_row(62_000.0));
        let _ = chain.options.insert(full_row(64_000.0));
        let mut state = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store_from(chain),
        );
        state.screen = crate::app::LiveScreen::Surface;
        state.load = load;
        state
    }

    /// A live state whose only strike has no reliable IV (no quotes, zero IV) — the
    /// "insufficient IV" empty state.
    fn empty_iv_state() -> LiveState {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(OptionData {
            strike_price: pos(60_000.0),
            implied_volatility: Positive::ZERO,
            ..Default::default()
        });
        let mut state = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store_from(chain),
        );
        state.screen = crate::app::LiveScreen::Surface;
        state.load = ScreenLoad::Ready;
        state
    }

    /// Project the panel's active geometry exactly as the ui view-cache would (off
    /// the draw path) — the `&GraphProjection` the screen's `draw` reads.
    #[must_use]
    fn projection(state: &LiveState) -> GraphProjection {
        project(state.surface.active_graph_data())
    }

    #[track_caller]
    fn render(state: &LiveState, width: u16, height: u16) {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let proj = projection(state);
        let mut term = match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        };
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, &proj, frame, area, theme, 0)) {
            Ok(_) => {}
            Err(e) => panic!("surface draw failed: {e}"),
        }
    }

    /// Render the surface screen and return the frame as row-major text — for
    /// asserting the on-screen title / header wording (SF-01/SF-02/P3-02).
    #[track_caller]
    fn render_text(state: &LiveState, width: u16, height: u16) -> String {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let proj = projection(state);
        let mut term = match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        };
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, &proj, frame, area, theme, 0)) {
            Ok(_) => {}
            Err(e) => panic!("surface draw failed: {e}"),
        }
        buffer_to_text(term.backend().buffer())
    }

    /// The first (title) row of the rendered frame.
    #[track_caller]
    fn title_row(state: &LiveState, width: u16, height: u16) -> String {
        render_text(state, width, height)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    fn press(state: &mut LiveState, code: KeyCode) {
        let _ = handle_key(state, KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Whether the plotted **Braille** smile/curve line is drawn in the **accent**
    /// (bright) color. The line draws with Braille markers (`U+2800..=U+28FF`), styled
    /// `theme.accent()` (fg Cyan) when live and `theme.dim()` (no fg) when stale;
    /// ratatui's chart Canvas preserves the plotted color, so an accent-colored line
    /// cell means the line is bright, and its absence means the line is dimmed. A `false`
    /// return therefore means the line is NOT bright (dimmed or absent).
    #[track_caller]
    fn smile_line_is_accent(state: &LiveState, width: u16, height: u16) -> bool {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let proj = projection(state);
        let mut term = match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        };
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, &proj, frame, area, theme, 0)) {
            Ok(_) => {}
            Err(e) => panic!("surface draw failed: {e}"),
        }
        let buffer = term.backend().buffer();
        for y in 0..height {
            for x in 0..width {
                if let Some(cell) = buffer.cell((x, y))
                    && cell
                        .symbol()
                        .chars()
                        .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
                    && cell.fg == Color::Cyan
                {
                    return true;
                }
            }
        }
        false
    }

    // --- Every state renders without panic -----------------------------------

    #[test]
    fn test_render_loading_ready_error_and_empty_states() {
        render(&live_state(ScreenLoad::Loading), 120, 40);
        render(
            &live_state(ScreenLoad::Error {
                message: "provider unreachable".to_owned(),
            }),
            120,
            40,
        );
        // Ready across all three views.
        let mut ready = live_state(ScreenLoad::Ready);
        render(&ready, 120, 40); // smile
        press(&mut ready, KeyCode::Char('x')); // → curve
        render(&ready, 120, 40);
        press(&mut ready, KeyCode::Char('x')); // → surface
        render(&ready, 120, 40);
        // The insufficient-IV empty state.
        render(&empty_iv_state(), 120, 40);
        // Small terminals must not panic either.
        render(&live_state(ScreenLoad::Ready), 40, 8);
    }

    // --- Stale stream badges the surface and never blanks (#57, §6) ----------

    #[test]
    fn test_stale_health_badges_the_surface_title() {
        // A dropped stream never blanks the surface: the last-known smile renders with
        // the stream-health badge in the title (glyph + text, NO_COLOR-safe), so the
        // stale state is honest, not a bright, trusted-looking curve (§6).
        let mut state = live_state(ScreenLoad::Ready);
        state.source.health = StreamHealth::Stale { since: utc(EXP) };
        let title = title_row(&state, 120, 40);
        assert!(
            title.contains("stale"),
            "the stale badge renders in the surface title: {title:?}",
        );
        // The body still renders the last smile (never blanked).
        let text = render_text(&state, 120, 40);
        assert!(
            text.contains("strike"),
            "the last smile still renders: {text:?}"
        );
    }

    #[test]
    fn test_reconnecting_health_badges_the_surface_title() {
        let mut state = live_state(ScreenLoad::Ready);
        state.source.health = StreamHealth::Reconnecting { attempt: 3 };
        let title = title_row(&state, 120, 40);
        assert!(
            title.contains("reconnecting"),
            "the reconnecting badge renders in the surface title: {title:?}",
        );
    }

    #[test]
    fn test_live_health_shows_no_surface_badge() {
        // The control: a live feed shows no health badge (the badge is reserved for the
        // honest stale/reconnecting state).
        let title = title_row(&live_state(ScreenLoad::Ready), 120, 40);
        assert!(
            !title.contains("stale") && !title.contains("reconnecting"),
            "a live feed shows no health badge: {title:?}",
        );
    }

    // --- Overlay-aware health: a stale overlay dims/badges the surface (#118) --

    #[test]
    fn test_stale_overlay_dims_and_badges_the_surface_when_source_is_live() {
        // #118: the surface geometry consumes Greeks/IV that a standalone overlay may
        // supply, so a stale OVERLAY must dim + badge the surface even while the chain
        // SOURCE stays live — never a bright, trusted-looking curve over a stale feed.
        let state = live_state(ScreenLoad::Ready).with_overlay(OverlayBinding::new(
            pid("dxlink"),
            caps(),
            StreamHealth::Stale { since: utc(EXP) },
        ));
        // Only the overlay degraded; the source is still live.
        assert!(
            matches!(state.source.health, StreamHealth::Live),
            "the chain source stays live",
        );
        let title = title_row(&state, 120, 40);
        assert!(
            title.contains("stale"),
            "a stale overlay badges the surface title: {title:?}",
        );
        // The smile still renders (never blanked), but its line is DIMMED — not the
        // bright accent color of a live feed.
        let text = render_text(&state, 120, 40);
        assert!(
            text.contains("strike"),
            "the last smile still renders: {text:?}"
        );
        assert!(
            !smile_line_is_accent(&state, 120, 40),
            "a stale overlay dims the surface line (not accent-colored)",
        );
    }

    #[test]
    fn test_live_overlay_leaves_the_surface_bright() {
        // The control: with BOTH the source and a live overlay, the surface renders
        // bright (accent line) with no health badge — the combine dims only on a real
        // degrade, so it does not over-dim a fully-live surface.
        let state = live_state(ScreenLoad::Ready).with_overlay(OverlayBinding::new(
            pid("dxlink"),
            caps(),
            StreamHealth::Live,
        ));
        let title = title_row(&state, 120, 40);
        assert!(
            !title.contains("stale") && !title.contains("reconnecting"),
            "a live source + live overlay shows no badge: {title:?}",
        );
        assert!(
            smile_line_is_accent(&state, 120, 40),
            "a fully-live surface renders its line bright (accent)",
        );
    }

    // --- `g`/`G` cycles the Greek axis ---------------------------------------

    #[test]
    fn test_g_cycles_axis_forward_and_shift_g_back() {
        let mut state = live_state(ScreenLoad::Ready);
        assert_eq!(state.surface.axis(), SurfaceAxis::Delta);
        press(&mut state, KeyCode::Char('g'));
        assert_eq!(
            state.surface.axis(),
            SurfaceAxis::Gamma,
            "`g` advances the axis"
        );
        press(&mut state, KeyCode::Char('G'));
        assert_eq!(state.surface.axis(), SurfaceAxis::Delta, "`G` steps back");
    }

    // --- `x` cycles smile → curve → surface → smile --------------------------

    #[test]
    fn test_x_cycles_the_view_smile_curve_surface() {
        let mut state = live_state(ScreenLoad::Ready);
        assert_eq!(state.surface.view(), SurfaceView::Smile);
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.surface.view(), SurfaceView::Curve);
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.surface.view(), SurfaceView::Surface);
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(
            state.surface.view(),
            SurfaceView::Smile,
            "cycles back to the smile"
        );
    }

    // --- The active projection is Ready for smile/curve, ReadySurface for 3D --

    #[test]
    fn test_active_projection_shape_matches_the_view() {
        let mut state = live_state(ScreenLoad::Ready);
        assert!(
            projection(&state).ready().is_some(),
            "the smile projects a series"
        );
        press(&mut state, KeyCode::Char('x')); // curve
        assert!(
            projection(&state).ready().is_some(),
            "the curve projects a series"
        );
        press(&mut state, KeyCode::Char('x')); // surface
        assert!(
            projection(&state).ready_surface().is_some(),
            "the surface projects a heat-map grid",
        );
    }

    // --- An IV-sparse expiry routes to the empty projection ------------------

    #[test]
    fn test_empty_iv_expiry_routes_to_empty_projection() {
        let state = empty_iv_state();
        assert!(
            projection(&state).empty_reason().is_some(),
            "no reliable IV → the insufficient-IV empty state, not a fabricated curve",
        );
    }

    // --- The Volatility axis has no surface (z is a Greek/Price) --------------

    #[test]
    fn test_surface_refuses_volatility_axis_but_renders_the_honest_state() {
        // Cycle to the surface view, then to the Volatility axis: the surface build is
        // empty (IV is not a surface axis), and the screen renders the honest state
        // rather than a fabricated IV surface.
        let mut state = live_state(ScreenLoad::Ready);
        press(&mut state, KeyCode::Char('x')); // curve
        press(&mut state, KeyCode::Char('x')); // surface
        // Advance the axis Delta→Gamma→Theta→Vega→Volatility (four `g`s).
        for _ in 0..4 {
            press(&mut state, KeyCode::Char('g'));
        }
        assert_eq!(state.surface.axis(), SurfaceAxis::Volatility);
        assert!(
            projection(&state).ready_surface().is_none(),
            "the Volatility axis yields no surface geometry",
        );
        render(&state, 120, 40); // the honest "IV has no surface" state renders
    }

    // --- SF-01: g/G visibly moves the smile title axis hint ------------------

    #[test]
    fn test_g_moves_the_smile_title_axis_hint() {
        // SF-01: in the Smile view the axis is a curve/surface concern, so it is shown
        // as a dim hint in the title. Pressing `g` must visibly change the rendered
        // title even though the smile body (IV vs strike) is axis-independent — the key
        // is never a silent no-op.
        let mut state = live_state(ScreenLoad::Ready);
        assert_eq!(state.surface.view(), SurfaceView::Smile);
        let before = title_row(&state, 120, 40);
        assert!(
            before.contains("axis:"),
            "the smile title carries the axis hint, got {before:?}",
        );
        assert!(before.contains('Δ'), "the pending axis starts at delta");
        press(&mut state, KeyCode::Char('g'));
        let after = title_row(&state, 120, 40);
        assert_ne!(before, after, "pressing g moves the smile title axis hint");
        assert!(
            after.contains('Γ'),
            "the hint advanced to gamma, got {after:?}"
        );
    }

    // --- SF-02: the refused-axis surface title never asserts an IV surface ----

    #[test]
    fn test_surface_refused_axis_title_is_annotated_not_an_iv_surface() {
        // SF-02: view=Surface, axis=Volatility — the body refuses it (z is a
        // Greek/Price). The title must not assert what the body denies, so it annotates
        // the axis `IV (n/a)`.
        let mut state = live_state(ScreenLoad::Ready);
        press(&mut state, KeyCode::Char('x')); // curve
        press(&mut state, KeyCode::Char('x')); // surface
        for _ in 0..4 {
            press(&mut state, KeyCode::Char('g')); // → Volatility
        }
        assert_eq!(state.surface.axis(), SurfaceAxis::Volatility);
        let title = title_row(&state, 120, 40);
        assert!(
            title.contains("n/a"),
            "the refused surface axis is annotated n/a, got {title:?}",
        );
    }

    // --- P3-02: the heat-map header states the vol-axis direction ------------

    #[test]
    fn test_heatmap_header_notes_the_vol_axis_direction() {
        // P3-02: the surface heat-map header marks the vertical direction (the top row
        // is the highest vol), so the axis is not ambiguous without color.
        let mut state = live_state(ScreenLoad::Ready);
        press(&mut state, KeyCode::Char('x')); // curve
        press(&mut state, KeyCode::Char('x')); // surface (Delta axis → a grid)
        let text = render_text(&state, 120, 40);
        assert!(
            text.contains("top") && text.contains("bottom"),
            "the header marks the vol direction top/bottom",
        );
    }

    // --- The ramp + sampling helpers -----------------------------------------

    #[test]
    fn test_ramp_glyph_maps_intensity_to_light_then_dense() {
        // SF-03: the ramp is all ink — a present cell at minimum intensity renders `·`,
        // never a blank space (a space is reserved for a data gap, see below).
        assert_eq!(
            ramp_glyph(0.0),
            '·',
            "a present minimum-intensity cell is `·`, not a blank",
        );
        assert_eq!(
            ramp_glyph(1.0),
            '@',
            "the highest intensity is the densest glyph"
        );
        // Out-of-range clamps rather than panicking.
        assert_eq!(ramp_glyph(-5.0), '·');
        assert_eq!(ramp_glyph(5.0), '@');
        assert_eq!(ramp_glyph(f64::NAN), '·', "a non-finite intensity is safe");
    }

    #[test]
    fn test_cell_span_present_min_is_dot_and_gap_is_space() {
        // SF-03: a present 0.0-intensity cell renders `·` (ink), while a `None` gap
        // renders a space — so a present-at-minimum cell is never confused with a gap,
        // even on a monochrome terminal where color carries nothing.
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        assert_eq!(
            cell_span(Some(0.0), theme, false).content.as_ref(),
            "·",
            "a present minimum cell has ink",
        );
        assert_eq!(
            cell_span(None, theme, false).content.as_ref(),
            " ",
            "a data gap is a blank space",
        );
    }

    #[test]
    fn test_cell_span_dims_the_heat_glyph_when_stale() {
        // #57 P2-01: a dropped stream dims the 3D heat grid too — a high-intensity cell
        // that would paint bright accent renders dim (only the intensity modifier, no
        // color), while the ramp glyph shape still carries the structure (NO_COLOR-safe).
        // So the heat view never reads bright/trusted under a `◐ stale` badge.
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let bright = cell_span(Some(1.0), theme, false);
        assert!(
            bright.style.fg.is_some() && bright.style.add_modifier.contains(Modifier::BOLD),
            "a live max-intensity cell is bright accent (color + bold)",
        );
        let stale = cell_span(Some(1.0), theme, true);
        assert!(
            stale.style.fg.is_none() && stale.style.add_modifier.contains(Modifier::DIM),
            "a stale max-intensity cell dims (no color, DIM modifier)",
        );
        assert_eq!(
            stale.content.as_ref(),
            "@",
            "the densest ramp glyph still carries the structure when stale",
        );
        // A None gap stays a blank space regardless of stale.
        assert_eq!(cell_span(None, theme, true).content.as_ref(), " ");
    }

    #[test]
    fn test_insufficient_text_hints_are_reason_specific() {
        // P3-01 + P3-03: each EmptyReason states its own cause and its own next step —
        // NoData keeps the reconnect hint, Degenerate is a distinguishable "degenerate
        // geometry" (not folded into "insufficient IV"), and Unsupported drops the
        // misleading reconnect hint.
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let flatten = |reason: Option<EmptyReason>| -> String {
            insufficient_text(theme, reason)
                .lines
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let nodata = flatten(Some(EmptyReason::NoData));
        assert!(
            nodata.contains("insufficient IV"),
            "NoData names the IV cause"
        );
        assert!(
            nodata.contains("reconnect"),
            "NoData keeps the reconnect hint"
        );

        let degenerate = flatten(Some(EmptyReason::Degenerate));
        assert!(
            degenerate.contains("degenerate geometry"),
            "Degenerate is a distinguishable state",
        );
        assert!(
            !degenerate.contains("insufficient IV"),
            "Degenerate is not folded into the insufficient-IV state (P3-01)",
        );

        let unsupported = flatten(Some(EmptyReason::Unsupported));
        assert!(
            unsupported.contains("not available"),
            "Unsupported names itself"
        );
        assert!(
            !unsupported.contains("reconnect"),
            "Unsupported drops the misleading reconnect hint (P3-03)",
        );
    }

    #[test]
    fn test_sample_indices_fits_and_downsamples() {
        assert_eq!(sample_indices(3, 10), vec![0, 1, 2], "fits → all indices");
        assert!(sample_indices(0, 10).is_empty());
        assert!(sample_indices(10, 0).is_empty());
        let sampled = sample_indices(100, 5);
        assert!(sampled.len() <= 5, "downsampled to at most `max`");
        assert!(sampled.iter().all(|&i| i < 100), "every index is in range");
        assert_eq!(
            sampled.first().copied(),
            Some(0),
            "starts at the first column"
        );
    }

    // --- Draw purity: draw builds no GraphData -------------------------------

    #[test]
    fn test_draw_reads_cached_projection_builds_no_graphdata() {
        // `draw` takes `&LiveState` + a borrowed projection; the active GraphData is
        // byte-for-byte unchanged across a draw (the build happens off the draw path).
        let state = live_state(ScreenLoad::Ready);
        let before = state.surface.active_graph_data().clone();
        render(&state, 120, 40);
        assert_eq!(
            state.surface.active_graph_data(),
            &before,
            "a draw must not build or mutate the cached surface GraphData",
        );
    }
}
