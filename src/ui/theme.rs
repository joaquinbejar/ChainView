//! The auto/dark/light theme, the modal help-overlay renderer, the truthful
//! status bar / keybar, and the `NO_COLOR` fallback (`docs/05-views-and-ux.md`
//! §7, §8).
//!
//! # The keymap is the source of truth; this module renders it
//!
//! The single-source keybinding map lives in the **application** layer
//! ([`crate::app::keymap`]) so both the dispatch and this overlay read one table
//! (`docs/05-views-and-ux.md` §3). This module depends on it
//! ([`help_sections`](crate::app::keymap)) to **generate** the help overlay —
//! `ui → application`, the allowed direction — and holds no dispatch logic itself,
//! so a bound key and its documentation cannot drift.
//!
//! # Color is never the only signal; `NO_COLOR` is honored
//!
//! Every color-encoded state also carries a glyph or text marker — the at-spot row
//! carries [`AT_SPOT_MARKER`], the P&L sign carries `+`/`−`, the tick direction
//! carries `▲`/`▼`/`·` — so the UI is legible on a monochrome terminal and to
//! color-blind users (`docs/05-views-and-ux.md` §7, `CLAUDE.md` accessibility
//! policy). When `NO_COLOR` is set ([`Theme::no_color`]), [`Theme`] drops every
//! foreground/background color and falls back to intensity + markers only; the
//! markers are always present regardless of color.
//!
//! # The draw path stays pure
//!
//! Every function here is a pure projection over borrowed [`App`] state: the
//! spinner/clock advances off [`App::tick_count`], never a wall clock read in
//! `draw`, and no I/O or heavy compute happens while rendering
//! (`docs/02-tui-architecture.md` §7). The responsive chain column-drop **order**
//! ([`greek_columns_for_slots`]) and the cross-screen too-small guard
//! ([`is_too_small`]) live here; the chain-matrix body that consumes them lands in
//! #18.

use optionstratlib::prelude::{Decimal, Positive};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::keymap::{Binding, help_sections};
use crate::app::{
    App, BundleLoad, LiveScreen, LiveState, Mode, Playback, ReplayScreen, ReplayState, ScreenLoad,
    is_replay_screen_reachable, live_screen_name, replay_screen_name,
};
use crate::chain::{StreamHealth, TickDir};
use crate::config::ThemeChoice;

// ===========================================================================
// Theme + palette (auto/dark/light, NO_COLOR fallback).
// ===========================================================================

/// The resolved color variant a [`Theme`] paints with
/// (`docs/05-views-and-ux.md` §7). An optional user override is deferred past v1;
/// v0.1 ships the built-in variants only, resolved from [`ThemeChoice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ThemeVariant {
    /// A palette tuned for a dark terminal background (the zero-config base).
    #[default]
    Dark,
    /// A palette tuned for a light terminal background.
    Light,
}

/// The resolved theme the draw path reads: the color variant plus the `NO_COLOR`
/// flag (`docs/05-views-and-ux.md` §7).
///
/// Built once per frame from [`App::theme`] and [`App::no_color`] by
/// [`Theme::resolve`]; every semantic style getter honors [`no_color`](Self::no_color)
/// so a color-encoded state degrades to intensity + markers only, never color
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    /// The resolved color variant.
    pub variant: ThemeVariant,
    /// Whether color is disabled (`NO_COLOR` / `--no-color` / config). When set,
    /// every semantic style resolves to intensity/markers only, with no color.
    pub no_color: bool,
}

impl Theme {
    /// Resolve the concrete theme from the user's [`ThemeChoice`] and the
    /// `no_color` flag (`docs/05-views-and-ux.md` §7).
    ///
    /// `Auto` resolves to a palette built from the 16 ANSI-named colors, which the
    /// terminal maps to its own palette — legible on **both** dark and light
    /// backgrounds with zero config ([ADR-0003]). Terminal-background detection is
    /// terminal I/O and cannot happen in the pure draw path, so `Auto` uses the
    /// adaptive ANSI base rather than probing.
    ///
    /// [ADR-0003]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0003-zero-config-first-run.md
    #[must_use]
    pub fn resolve(choice: ThemeChoice, no_color: bool) -> Self {
        let variant = match choice {
            // The adaptive ANSI base reads on both; `Auto` uses the dark tuning,
            // which relies only on named colors the terminal remaps to its theme.
            ThemeChoice::Auto | ThemeChoice::Dark => ThemeVariant::Dark,
            ThemeChoice::Light => ThemeVariant::Light,
        };
        Self { variant, no_color }
    }

    /// A semantic style: `color` foreground plus `modifier`, or — under
    /// [`no_color`](Self::no_color) — only `modifier`, so no color code is emitted
    /// but the intensity (bold/dim) survives.
    #[must_use]
    fn semantic(self, color: Color, modifier: Modifier) -> Style {
        let base = Style::new().add_modifier(modifier);
        if self.no_color { base } else { base.fg(color) }
    }

    /// The accent style for headers, keys, and the focused affordance.
    #[must_use]
    pub fn accent(self) -> Style {
        self.semantic(Color::Cyan, Modifier::BOLD)
    }

    /// The dim style for secondary text (never colored, so it is `NO_COLOR`-safe
    /// already).
    #[must_use]
    pub fn dim(self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    /// The style for a transient warning/hint in the keybar.
    #[must_use]
    pub fn warning(self) -> Style {
        self.semantic(Color::Yellow, Modifier::BOLD)
    }

    /// The style for a stream-health badge, exhaustive over [`StreamHealth`].
    #[must_use]
    pub fn health_style(self, health: &StreamHealth) -> Style {
        match health {
            StreamHealth::Live => self.semantic(Color::Green, Modifier::BOLD),
            StreamHealth::Stale { .. } | StreamHealth::Reconnecting { .. } => {
                self.semantic(Color::Yellow, Modifier::BOLD)
            }
        }
    }

    /// The shading style for the shared strike column, keyed on the `K/S`
    /// [`StrikeRelation`] bucket (`docs/05-views-and-ux.md` §7). Deliberately not
    /// an ITM/OTM label. Under `NO_COLOR` the shading drops and the `◀ATM` marker
    /// plus the numeric strike ordering convey the relation.
    #[must_use]
    pub fn strike_relation_style(self, rel: StrikeRelation) -> Style {
        match rel {
            StrikeRelation::BelowSpot => self.semantic(Color::Green, Modifier::empty()),
            StrikeRelation::AtSpot => self.semantic(Color::Yellow, Modifier::BOLD),
            StrikeRelation::AboveSpot => self.semantic(Color::Red, Modifier::empty()),
        }
    }

    /// The style for a price-direction indicator, exhaustive over [`TickDir`].
    #[must_use]
    pub fn tick_dir_style(self, dir: TickDir) -> Style {
        match dir {
            TickDir::Up => self.semantic(Color::Green, Modifier::empty()),
            TickDir::Down => self.semantic(Color::Red, Modifier::empty()),
            TickDir::Flat => self.dim(),
        }
    }

    /// The style for a signed P&L value: red when negative, green when
    /// non-negative. The `+`/`−` sign ([`pnl_sign_char`]) carries the sign under
    /// `NO_COLOR`.
    #[must_use]
    pub fn pnl_style(self, is_negative: bool) -> Style {
        if is_negative {
            self.semantic(Color::Red, Modifier::empty())
        } else {
            self.semantic(Color::Green, Modifier::empty())
        }
    }
}

// ===========================================================================
// Color-independent markers + view-model buckets.
// ===========================================================================

/// The marker on the at-spot strike row (`docs/05-views-and-ux.md` §7) — the
/// color-independent signal that survives a monochrome terminal.
pub const AT_SPOT_MARKER: &str = "◀ATM";

/// The default at-the-money band, as the `|K/S − 1|` tolerance that buckets a
/// strike as [`StrikeRelation::AtSpot`] (`docs/01-domain-model.md` §8). `0.005`
/// is 0.5%.
pub const ATM_BAND_PERMILLE: i64 = 5;

/// A row-level, option-style-**independent** relation of the strike to spot,
/// defined once as the `K/S` bucket (`docs/01-domain-model.md` §8).
///
/// Deliberately **not** an ITM/OTM label: a call and a put at one strike have
/// opposite ITM/OTM status, so no single label on a shared strike row can be
/// truthful. This value shades only the shared **strike column** by where the
/// strike sits relative to spot. It is a UI view-model bucket (computed at
/// projection time, borrowed, never stored); the chain-matrix body that renders it
/// lands in #18.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StrikeRelation {
    /// `K < S · (1 − band)` — the call leg is ITM here, the put OTM.
    BelowSpot,
    /// `|K/S − 1| ≤ band` — the nearest listed strike(s) to spot.
    AtSpot,
    /// `K > S · (1 + band)` — the put leg is ITM here, the call OTM.
    AboveSpot,
}

impl StrikeRelation {
    /// Bucket a `strike` against `spot` by the `K/S` ratio, using the documented
    /// [`ATM_BAND_PERMILLE`] tolerance (`docs/01-domain-model.md` §8).
    ///
    /// Computed with [`Decimal`] and checked multiplication only (no division, so
    /// no rounding policy is needed); a zero spot or an arithmetic overflow falls
    /// back to the neutral [`AtSpot`](Self::AtSpot) rather than panicking, so a
    /// degenerate quote can never crash a frame.
    #[must_use]
    pub fn classify(strike: Positive, spot: Positive) -> Self {
        let spot_dec = spot.to_dec();
        if spot_dec == Decimal::ZERO {
            return Self::AtSpot;
        }
        let band = Decimal::new(ATM_BAND_PERMILLE, 3);
        let strike_dec = strike.to_dec();
        let lower = Decimal::ONE
            .checked_sub(band)
            .and_then(|f| spot_dec.checked_mul(f));
        let upper = Decimal::ONE
            .checked_add(band)
            .and_then(|f| spot_dec.checked_mul(f));
        let (Some(lower), Some(upper)) = (lower, upper) else {
            return Self::AtSpot;
        };
        if strike_dec < lower {
            Self::BelowSpot
        } else if strike_dec > upper {
            Self::AboveSpot
        } else {
            Self::AtSpot
        }
    }
}

/// The color-independent marker for a strike relation: the [`AT_SPOT_MARKER`] for
/// the at-spot row, empty otherwise (`docs/05-views-and-ux.md` §7). Below/above
/// spot are conveyed by the marked at-spot row plus the numeric strike ordering,
/// so they need no separate glyph.
#[must_use]
pub fn strike_relation_marker(rel: StrikeRelation) -> &'static str {
    match rel {
        StrikeRelation::AtSpot => AT_SPOT_MARKER,
        StrikeRelation::BelowSpot | StrikeRelation::AboveSpot => "",
    }
}

/// The color-independent glyph for a price-direction indicator
/// (`docs/01-domain-model.md` §8): `▲` up, `▼` down, `·` flat.
#[must_use]
pub fn tick_dir_glyph(dir: TickDir) -> char {
    match dir {
        TickDir::Up => '▲',
        TickDir::Down => '▼',
        TickDir::Flat => '·',
    }
}

/// The color-independent sign for a signed value: `−` when negative, `+`
/// otherwise (`docs/05-views-and-ux.md` §7).
#[must_use]
pub fn pnl_sign_char(is_negative: bool) -> char {
    if is_negative { '−' } else { '+' }
}

/// The at-spot marker as a styled [`Span`] — the marker text plus the strike-column
/// style. Under `NO_COLOR` the style carries no color but the `◀ATM` text remains.
#[must_use]
pub fn strike_relation_marker_span(rel: StrikeRelation, theme: Theme) -> Span<'static> {
    Span::styled(
        strike_relation_marker(rel).to_owned(),
        theme.strike_relation_style(rel),
    )
}

/// A price-direction glyph as a styled [`Span`]. Under `NO_COLOR` the style carries
/// no color but the `▲`/`▼`/`·` glyph remains.
#[must_use]
pub fn tick_dir_span(dir: TickDir, theme: Theme) -> Span<'static> {
    Span::styled(tick_dir_glyph(dir).to_string(), theme.tick_dir_style(dir))
}

/// A P&L sign glyph as a styled [`Span`]. Under `NO_COLOR` the style carries no
/// color but the `+`/`−` sign remains.
#[must_use]
pub fn pnl_sign_span(is_negative: bool, theme: Theme) -> Span<'static> {
    Span::styled(
        pnl_sign_char(is_negative).to_string(),
        theme.pnl_style(is_negative),
    )
}

// ===========================================================================
// Responsive chain columns + too-small guard.
// ===========================================================================

/// A droppable analytic column of the chain matrix
/// (`docs/05-views-and-ux.md` §8). Δ is always shown; the rest drop as width
/// shrinks in [`GREEK_DROP_ORDER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GreekColumn {
    /// Delta — the primary Greek, never dropped.
    Delta,
    /// Gamma — dropped first as width shrinks.
    Gamma,
    /// Theta — dropped last.
    Theta,
    /// Vega — dropped second.
    Vega,
}

/// The order optional Greek columns are **dropped** as terminal width shrinks
/// (`docs/05-views-and-ux.md` §8): `Γ` first, then `ν`, then `Θ`. Δ is never in
/// this list — it is always shown.
pub const GREEK_DROP_ORDER: [GreekColumn; 3] =
    [GreekColumn::Gamma, GreekColumn::Vega, GreekColumn::Theta];

/// Which optional Greek columns are visible for a given number of extra column
/// slots that fit (`docs/05-views-and-ux.md` §8). Δ is always visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GreekColumns {
    /// Delta — always `true`.
    pub delta: bool,
    /// Gamma — shown only when there is width for all three optional columns.
    pub gamma: bool,
    /// Vega — shown when there is width for at least two.
    pub vega: bool,
    /// Theta — shown when there is width for at least one (dropped last).
    pub theta: bool,
}

/// Resolve which Greek columns are visible given `extra_slots`, the number of
/// optional Greek columns that fit after the always-present price/IV/Δ columns
/// (`docs/05-views-and-ux.md` §8).
///
/// Encodes the drop **order** `Γ → ν → Θ`: `Θ` is dropped last (retained first),
/// then `ν`, then `Γ`. So `0` slots shows Δ only; `1` adds `Θ`; `2` adds `ν`; `3`
/// adds `Γ`. The actual per-column widths and the render live in the chain matrix
/// (#18); this fixes only the policy.
#[must_use]
pub fn greek_columns_for_slots(extra_slots: usize) -> GreekColumns {
    GreekColumns {
        delta: true,
        theta: extra_slots >= 1,
        vega: extra_slots >= 2,
        gamma: extra_slots >= 3,
    }
}

/// The minimum terminal width below which any screen shows the "widen the terminal"
/// state instead of a body (`docs/05-views-and-ux.md` §8).
pub const MIN_WIDTH: u16 = 40;
/// The minimum terminal height below which any screen shows the "widen the
/// terminal" state instead of a body (`docs/05-views-and-ux.md` §8).
pub const MIN_HEIGHT: u16 = 8;

/// Whether `area` is below the minimum renderable size
/// (`docs/05-views-and-ux.md` §8). When true, [`render`](crate::render) shows the
/// cross-screen "widen the terminal" hint rather than a corrupt layout.
#[must_use]
pub fn is_too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

/// Draw the cross-screen "widen the terminal" state (`docs/05-views-and-ux.md`
/// §8) — the first-class too-small render, never a corrupt layout. Legible without
/// color (text + explicit size), so it survives `NO_COLOR`.
pub fn draw_too_small(frame: &mut Frame, area: Rect, theme: Theme) {
    let text = Text::from(vec![
        Line::from(Span::styled("widen the terminal", theme.warning())),
        Line::from(Span::styled(
            format!(
                "have {}x{}, need at least {MIN_WIDTH}x{MIN_HEIGHT}",
                area.width, area.height
            ),
            theme.dim(),
        )),
    ]);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}

// ===========================================================================
// Status bar, keybar/hint, and the modal help overlay.
// ===========================================================================

/// The braille spinner frames, cycled by the tick counter — the only motion the
/// status bar shows (`docs/05-views-and-ux.md` §7: motion reserved for the loading
/// spinner and the replay play-head).
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The spinner frame for a tick count — a pure projection of [`App::tick_count`],
/// never a wall-clock read.
#[must_use]
fn spinner_frame(tick: u64) -> char {
    let len = SPINNER_FRAMES.len() as u64;
    let idx = (tick % len) as usize;
    SPINNER_FRAMES.get(idx).copied().unwrap_or(' ')
}

/// Strip control/escape characters from a venue- or user-controlled string before
/// it reaches the render edge (`docs/SECURITY.md` terminal escape-sequence
/// hygiene). Applied only to short, display-only strings on the status line.
#[must_use]
fn sanitize(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect()
}

/// The health badge glyph, exhaustive over [`StreamHealth`]. Paired with the text
/// so the badge survives a monochrome terminal.
#[must_use]
fn health_glyph(health: &StreamHealth) -> char {
    match health {
        StreamHealth::Live => '●',
        StreamHealth::Stale { .. } => '◐',
        StreamHealth::Reconnecting { .. } => '↻',
    }
}

/// The health badge text, exhaustive over [`StreamHealth`].
#[must_use]
fn health_text(health: &StreamHealth) -> String {
    match health {
        StreamHealth::Live => "live".to_owned(),
        StreamHealth::Stale { .. } => "stale".to_owned(),
        StreamHealth::Reconnecting { attempt } => format!("reconnecting ({attempt})"),
    }
}

/// The stream-health badge as a styled [`Span`] — glyph plus text, so the state is
/// legible without color.
#[must_use]
pub fn health_span(health: &StreamHealth, theme: Theme) -> Span<'static> {
    Span::styled(
        format!("{} {}", health_glyph(health), health_text(health)),
        theme.health_style(health),
    )
}

/// Whether the live view is in a motion state (loading or reconnecting), which
/// animates the spinner.
#[must_use]
fn live_in_motion(live: &LiveState) -> bool {
    matches!(live.load, ScreenLoad::Loading)
        || matches!(live.source.health, StreamHealth::Reconnecting { .. })
}

/// Whether the replay view is in a motion state (bundle loading or playing).
#[must_use]
fn replay_in_motion(replay: &ReplayState) -> bool {
    matches!(replay.bundle, BundleLoad::Loading) || matches!(replay.play, Playback::Playing { .. })
}

/// A short label for the loaded bundle directory (its final path component).
#[must_use]
fn run_label(replay: &ReplayState) -> String {
    replay
        .dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| replay.dir.to_string_lossy().into_owned())
}

/// Draw the always-truthful one-line status bar (`docs/05-views-and-ux.md` §8):
/// provider / health / clock / mode in Live, run id / mode in Replay. The spinner
/// advances off [`App::tick_count`], never a wall clock, so the draw stays pure.
pub fn draw_status(app: &App, frame: &mut Frame, area: Rect, theme: Theme) {
    let mut spans = vec![Span::styled("chainview", theme.accent()), Span::raw("  ")];
    match &app.mode {
        Mode::Live(live) => {
            spans.push(Span::styled("live", theme.dim()));
            spans.push(Span::raw("  "));
            spans.push(Span::raw(sanitize(&live.store.chain().symbol)));
            spans.push(Span::raw("  "));
            spans.push(Span::raw(sanitize(live.source.provider.as_str())));
            spans.push(Span::raw("  "));
            spans.push(health_span(&live.source.health, theme));
            if live_in_motion(live) {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    spinner_frame(app.tick_count).to_string(),
                    theme.accent(),
                ));
            }
        }
        Mode::Replay(replay) => {
            spans.push(Span::styled("replay", theme.dim()));
            spans.push(Span::raw("  "));
            spans.push(Span::raw(sanitize(&run_label(replay))));
            if replay_in_motion(replay) {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    spinner_frame(app.tick_count).to_string(),
                    theme.accent(),
                ));
            }
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Append a `{slot} {name}` keybar entry, marking an unavailable screen with
/// parentheses + a dim style — a color-independent signal.
fn push_slot(
    spans: &mut Vec<Span<'static>>,
    slot: u8,
    name: &'static str,
    reachable: bool,
    theme: Theme,
) {
    if reachable {
        spans.push(Span::styled(format!("{slot} "), theme.accent()));
        spans.push(Span::raw(format!("{name}  ")));
    } else {
        spans.push(Span::styled(format!("{slot} ({name})  "), theme.dim()));
    }
}

/// Build the generated keybar line from the map's screen slots for the active mode,
/// dimming (and parenthesizing) unavailable screens.
#[must_use]
fn keybar_line(app: &App, theme: Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match &app.mode {
        Mode::Live(live) => {
            for (slot, screen) in [
                (1u8, LiveScreen::Chain),
                (2, LiveScreen::Depth),
                (3, LiveScreen::Surface),
                (4, LiveScreen::Payoff),
            ] {
                push_slot(
                    &mut spans,
                    slot,
                    live_screen_name(screen),
                    live.screen_reachable(screen),
                    theme,
                );
            }
        }
        Mode::Replay(_) => {
            for (slot, screen) in [(1u8, ReplayScreen::Replay), (2, ReplayScreen::Payoff)] {
                push_slot(
                    &mut spans,
                    slot,
                    replay_screen_name(screen),
                    is_replay_screen_reachable(screen),
                    theme,
                );
            }
        }
    }
    spans.push(Span::styled("? help  ", theme.dim()));
    spans.push(Span::styled("q quit", theme.dim()));
    Line::from(spans)
}

/// Draw the one-line hint/keybar (`docs/05-views-and-ux.md` §8): the transient
/// status hint when present (an unavailable-screen flash), else the "close help"
/// note while the overlay is open, else the generated keybar. The transient hint
/// carries a `!` marker so it reads without color.
pub fn draw_hint(app: &App, frame: &mut Frame, area: Rect, theme: Theme) {
    let line = if let Some(hint) = &app.status_hint {
        Line::from(vec![
            Span::styled("! ", theme.warning()),
            Span::styled(sanitize(hint), theme.warning()),
        ])
    } else if app.help_open {
        Line::from(Span::styled("?/Esc  close help", theme.dim()))
    } else {
        keybar_line(app, theme)
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The outer width of the two-column help overlay box.
const HELP_WIDTH: u16 = 72;

/// The fixed label-column width inside a help row (keys on the left, help on the
/// right); terse help text (`src/app/keymap.rs`) fits the remaining column width on
/// an 80-column terminal without truncation.
const HELP_LABEL_WIDTH: usize = 10;

/// One rendered section for a help column: the accent title, then one row per
/// binding, or — for a **deferred** screen with no bindings yet — a "not available"
/// note so every screen is still listed (`docs/05-views-and-ux.md` §2, fix SF-04).
fn render_section(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    bindings: &[&'static Binding],
    theme: Theme,
) {
    lines.push(Line::from(Span::styled(title.to_owned(), theme.accent())));
    if bindings.is_empty() {
        lines.push(Line::from(Span::styled(
            "  not available yet".to_owned(),
            theme.dim(),
        )));
        return;
    }
    for binding in bindings {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<HELP_LABEL_WIDTH$} ", binding.keys_label),
                theme.accent(),
            ),
            Span::raw(binding.help.to_owned()),
        ]));
    }
}

/// The estimated rendered height of a section (title + a row per binding, or the
/// single deferred note), used only to balance the two columns.
#[must_use]
fn section_height(bindings: &[&'static Binding]) -> usize {
    bindings.len().max(1) + 1
}

/// Render a column's sections into lines, with a blank separator between sections.
#[must_use]
fn render_column(
    sections: &[(&'static str, Vec<&'static Binding>)],
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, (title, bindings)) in sections.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        render_section(&mut lines, title, bindings, theme);
    }
    lines
}

/// Draw the modal help overlay (`docs/05-views-and-ux.md` §3, §8): every screen
/// and its keys, grouped by context, generated **from the application-layer map**
/// ([`help_sections`](crate::app::keymap)). Laid out in **two columns** (balanced by
/// height) so every documented key is visible on a standard 80x24 terminal without
/// clipping (fix SF-01); deferred screens still appear with a "not available" note
/// (fix SF-04). Floats over the body with a [`Clear`] behind it. While open,
/// dispatch honors only `?`/`Esc` (handled in `App::dispatch_key_global`); no
/// background action fires.
pub fn draw_help_overlay(app: &App, frame: &mut Frame, area: Rect, theme: Theme) {
    // Balance the sections across two columns (assign each to the shorter column),
    // so the overlay is about half as tall as a single column and fits 80x24.
    let mut left: Vec<(&'static str, Vec<&'static Binding>)> = Vec::new();
    let mut right: Vec<(&'static str, Vec<&'static Binding>)> = Vec::new();
    let mut left_h = 0usize;
    let mut right_h = 0usize;
    for (title, bindings) in help_sections(&app.mode) {
        let height = section_height(&bindings);
        if left_h <= right_h {
            left_h += height;
            left.push((title, bindings));
        } else {
            right_h += height;
            right.push((title, bindings));
        }
    }
    let left_lines = render_column(&left, theme);
    let right_lines = render_column(&right, theme);

    // The overlay height is the taller column plus the two border rows; cap so the
    // `+ 2` cannot overflow `u16` (no unchecked add, no banned `saturating_*`).
    // `centered_rect` clamps the result to `area` regardless.
    let content = left_lines.len().max(right_lines.len());
    let capped = content.min(usize::from(u16::MAX) - 2);
    let height = capped as u16 + 2;
    let overlay = super::centered_rect(area, HELP_WIDTH, height);

    frame.render_widget(Clear, overlay);
    let block = Block::bordered().title("Help  —  ?/Esc to close");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    frame.render_widget(Paragraph::new(Text::from(left_lines)), left_area);
    frame.render_widget(Paragraph::new(Text::from(right_lines)), right_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests_support::{live_app_caps, live_app_on, replay_app_on};
    use crate::app::{LiveScreen, ReplayScreen, ScreenLoad};
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The full-frame text as a single string (row-major), for overlay assertions.
    #[track_caller]
    fn rendered_frame(app: &App, width: u16, height: u16) -> String {
        let mut term = terminal(width, height);
        match term.draw(|frame| crate::render(app, frame)) {
            Ok(_) => {}
            Err(e) => panic!("render failed: {e}"),
        }
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    // --- NO_COLOR strips color but keeps markers -----------------------------

    #[test]
    fn test_theme_no_color_strikes_color_but_keeps_at_spot_marker() {
        let theme = Theme::resolve(ThemeChoice::Auto, true);
        let span = strike_relation_marker_span(StrikeRelation::AtSpot, theme);
        assert!(
            span.style.fg.is_none() && span.style.bg.is_none(),
            "NO_COLOR must not set a foreground/background color",
        );
        assert!(
            span.content.contains(AT_SPOT_MARKER),
            "the ◀ATM marker must survive NO_COLOR",
        );
    }

    #[test]
    fn test_theme_no_color_keeps_tick_and_pnl_markers() {
        let theme = Theme::resolve(ThemeChoice::Dark, true);
        let up = tick_dir_span(TickDir::Up, theme);
        assert!(up.style.fg.is_none() && up.style.bg.is_none());
        assert!(up.content.contains('▲'));
        let down = tick_dir_span(TickDir::Down, theme);
        assert!(down.content.contains('▼'));
        let neg = pnl_sign_span(true, theme);
        assert!(neg.style.fg.is_none() && neg.style.bg.is_none());
        assert!(neg.content.contains('−'));
        let pos = pnl_sign_span(false, theme);
        assert!(pos.content.contains('+'));
    }

    #[test]
    fn test_theme_with_color_sets_foreground_and_keeps_markers() {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let span = tick_dir_span(TickDir::Up, theme);
        assert!(span.style.fg.is_some(), "color mode sets a foreground");
        assert!(
            span.content.contains('▲'),
            "the glyph is present with color too"
        );
    }

    #[test]
    fn test_theme_no_color_health_badge_keeps_glyph_and_text() {
        let theme = Theme::resolve(ThemeChoice::Auto, true);
        let span = health_span(&StreamHealth::Live, theme);
        assert!(span.style.fg.is_none() && span.style.bg.is_none());
        assert!(span.content.contains('●') && span.content.contains("live"));
    }

    // --- Theme resolution ----------------------------------------------------

    #[test]
    fn test_theme_resolve_maps_choice_to_variant() {
        assert_eq!(
            Theme::resolve(ThemeChoice::Auto, false).variant,
            ThemeVariant::Dark
        );
        assert_eq!(
            Theme::resolve(ThemeChoice::Dark, false).variant,
            ThemeVariant::Dark
        );
        assert_eq!(
            Theme::resolve(ThemeChoice::Light, false).variant,
            ThemeVariant::Light
        );
    }

    // --- StrikeRelation K/S bucket -------------------------------------------

    #[track_caller]
    fn pos(v: f64) -> Positive {
        match Positive::new(v) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive {v}: {e}"),
        }
    }

    #[test]
    fn test_strike_relation_classify_buckets_below_at_above() {
        let spot = pos(60_000.0);
        assert_eq!(
            StrikeRelation::classify(pos(50_000.0), spot),
            StrikeRelation::BelowSpot
        );
        assert_eq!(
            StrikeRelation::classify(pos(60_000.0), spot),
            StrikeRelation::AtSpot
        );
        assert_eq!(
            StrikeRelation::classify(pos(70_000.0), spot),
            StrikeRelation::AboveSpot
        );
        // Inside the 0.5% band is still at-spot.
        assert_eq!(
            StrikeRelation::classify(pos(60_100.0), spot),
            StrikeRelation::AtSpot
        );
    }

    #[test]
    fn test_strike_relation_classify_zero_spot_is_at_spot() {
        assert_eq!(
            StrikeRelation::classify(pos(60_000.0), pos(0.0)),
            StrikeRelation::AtSpot
        );
    }

    // --- Responsive column-drop order ----------------------------------------

    #[test]
    fn test_greek_columns_for_slots_drop_order_gamma_vega_theta() {
        assert_eq!(
            greek_columns_for_slots(0),
            GreekColumns {
                delta: true,
                gamma: false,
                vega: false,
                theta: false
            },
        );
        // Theta retained first (dropped last).
        assert!(greek_columns_for_slots(1).theta);
        assert!(!greek_columns_for_slots(1).vega);
        // Vega second.
        assert!(greek_columns_for_slots(2).vega);
        assert!(!greek_columns_for_slots(2).gamma);
        // Gamma last (dropped first).
        assert!(greek_columns_for_slots(3).gamma);
        assert_eq!(GREEK_DROP_ORDER[0], GreekColumn::Gamma);
    }

    // --- Too-small guard ------------------------------------------------------

    #[test]
    fn test_is_too_small_below_minimum() {
        assert!(is_too_small(Rect::new(0, 0, 30, 24)));
        assert!(is_too_small(Rect::new(0, 0, 80, 4)));
        assert!(!is_too_small(Rect::new(0, 0, 80, 24)));
        assert!(!is_too_small(Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT)));
    }

    #[test]
    fn test_draw_too_small_renders_widen_hint() {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let mut term = terminal(30, 6);
        match term.draw(|frame| draw_too_small(frame, frame.area(), theme)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
        let buffer = term.backend().buffer().clone();
        let rendered: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            rendered.contains("widen the terminal"),
            "must show the widen hint"
        );
    }

    // --- Status bar / keybar / help overlay render without panic -------------

    #[test]
    fn test_draw_status_and_hint_render_without_panic() {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let app = live_app_on(LiveScreen::Chain, ScreenLoad::Loading, false);
        let mut term = terminal(80, 24);
        match term.draw(|frame| {
            let area = frame.area();
            draw_status(&app, frame, Rect::new(0, 0, area.width, 1), theme);
            draw_hint(
                &app,
                frame,
                Rect::new(0, area.height - 1, area.width, 1),
                theme,
            );
        }) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
    }

    #[test]
    fn test_draw_help_overlay_renders_bindings_without_panic() {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let app = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, true);
        let mut term = terminal(100, 40);
        match term.draw(|frame| draw_help_overlay(&app, frame, frame.area(), theme)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
    }

    #[test]
    fn test_help_overlay_fits_80x24_shows_every_section() {
        // On the standard trader terminal the two-column overlay must show keys from
        // EVERY section — including the last (Payoff), which clipped off the bottom in
        // the old single-column layout (fix SF-01).
        let app = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, true);
        let text = rendered_frame(&app, 80, 24);
        assert!(text.contains("Move strike"), "a Chain key is visible");
        assert!(
            text.contains("Cycle Greek axis"),
            "a Surface key is visible"
        );
        assert!(
            text.contains("Commit strategy"),
            "a Payoff key (the last section) is visible on 80x24, not clipped",
        );
    }

    #[test]
    fn test_help_overlay_lists_deferred_replay_payoff_section() {
        // Every screen is listed in the overlay (§2): the v0.5 replay Payoff screen
        // has no bindings yet, so it appears as a titled section with a "not
        // available" note rather than being dropped (fix SF-04).
        let app = replay_app_on(ReplayScreen::Replay, true);
        let text = rendered_frame(&app, 80, 24);
        assert!(
            text.contains("Payoff"),
            "the deferred Payoff screen is listed"
        );
        assert!(
            text.contains("not available"),
            "the deferred screen shows a not-available note",
        );
    }

    // --- Reachability skip/hint via dispatch (capability-driven) --------------

    fn caps_no_depth() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(false)
            .greeks(GreeksCapability::Provided)
            .build()
    }

    #[test]
    fn test_dispatch_unavailable_number_key_flashes_hint_and_keeps_screen() {
        // A provider without depth: pressing `2` (Depth) must not switch and must
        // flash a keybar hint naming the screen.
        let mut app = live_app_caps(caps_no_depth());
        let before = match &app.mode {
            Mode::Live(live) => live.screen,
            Mode::Replay(_) => panic!("expected a live app"),
        };
        let _ = app.dispatch_key_global(press(KeyCode::Char('2')));
        let after = match &app.mode {
            Mode::Live(live) => live.screen,
            Mode::Replay(_) => panic!("expected a live app"),
        };
        assert_eq!(before, after, "an unavailable number key must not switch");
        match &app.status_hint {
            Some(hint) => assert!(hint.contains("Depth"), "hint names the screen: {hint}"),
            None => panic!("an unavailable number key must flash a hint"),
        }
    }

    #[test]
    fn test_dispatch_available_number_key_switches_and_clears_hint() {
        let mut app = live_app_caps(caps_no_depth());
        // `3` = Surface (Greeks present) is reachable → switch.
        let _ = app.dispatch_key_global(press(KeyCode::Char('3')));
        match &app.mode {
            Mode::Live(live) => assert_eq!(live.screen, LiveScreen::Surface),
            Mode::Replay(_) => panic!("expected a live app"),
        }
        assert!(app.status_hint.is_none(), "a valid switch leaves no hint");
    }

    #[test]
    fn test_dispatch_tab_skips_unavailable_screen() {
        // Depth is unavailable (no depth capability), so Tab from Chain skips it and
        // lands on the next reachable screen (Surface), never on the Depth body.
        let mut app = live_app_caps(caps_no_depth());
        let _ = app.dispatch_key_global(press(KeyCode::Tab));
        match &app.mode {
            Mode::Live(live) => assert_eq!(
                live.screen,
                LiveScreen::Surface,
                "Tab must skip the unavailable Depth screen",
            ),
            Mode::Replay(_) => panic!("expected a live app"),
        }
    }

    // --- Modal help precedence: only ?/Esc act while open --------------------

    #[test]
    fn test_dispatch_modal_help_honors_only_toggle_keys() {
        let mut app = live_app_caps(caps_no_depth());
        // Open help.
        let _ = app.dispatch_key_global(press(KeyCode::Char('?')));
        assert!(app.help_open);
        let screen_before = match &app.mode {
            Mode::Live(live) => live.screen,
            Mode::Replay(_) => panic!("expected a live app"),
        };
        // A screen-switch key while help is open is swallowed — no switch, no hint,
        // help stays open.
        let _ = app.dispatch_key_global(press(KeyCode::Char('3')));
        assert!(app.help_open, "a swallowed key leaves the overlay open");
        assert!(
            app.status_hint.is_none(),
            "no background hint fires behind help"
        );
        match &app.mode {
            Mode::Live(live) => assert_eq!(
                live.screen, screen_before,
                "no background screen switch fires behind the overlay",
            ),
            Mode::Replay(_) => panic!("expected a live app"),
        }
        // A quit key while help is open is also swallowed (only ?/Esc act).
        let _ = app.dispatch_key_global(press(KeyCode::Char('q')));
        assert!(
            !app.should_quit,
            "q does not quit while the overlay is modal"
        );
        // Esc closes it.
        let _ = app.dispatch_key_global(press(KeyCode::Esc));
        assert!(!app.help_open, "Esc closes the overlay");
    }

    // --- The cross-screen too-small guard is wired into render ----------------

    #[test]
    fn test_render_below_min_size_shows_widen_hint() {
        // The full render path shows the cross-screen "widen the terminal" state
        // below the minimum size instead of a screen body.
        let app = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, false);
        let mut term = terminal(30, 6);
        match term.draw(|frame| crate::render(&app, frame)) {
            Ok(_) => {}
            Err(e) => panic!("render failed: {e}"),
        }
        let buffer = term.backend().buffer().clone();
        let rendered: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            rendered.contains("widen the terminal"),
            "render must show the widen hint below the minimum size",
        );
    }
}
