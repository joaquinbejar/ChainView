//! Terminal screens, the pure draw dispatch, and the synchronous render loop
//! (`docs/02-tui-architecture.md` §7, §8, §9).
//!
//! # The draw path is pure over `&App` and does no I/O
//!
//! [`render`] is a **pure function of app state**: it takes `&App` (never
//! `&mut`), lays out the frame, and paints — it never `.await`s, performs I/O, or
//! builds a heavy structure (an `OptionChain`, a `GraphData`, a `TimelineCursor`
//! belong in the domain layer and are *borrowed* by a widget, `docs/02` §7).
//! Because it takes `&App`, a draw cannot mutate app state — the purity guarantee
//! is enforced by the signature.
//!
//! # The dispatch is total and wildcard-free
//!
//! Screen identity is **mode-scoped** ([`LiveScreen`](crate::app::LiveScreen) /
//! [`ReplayScreen`](crate::app::ReplayScreen)), so an out-of-mode pair is
//! unrepresentable and [`render`] is a **total** match: the mode first, then an
//! exhaustive match over that mode's screens, with **no `_` arm**. Adding a screen
//! variant forces the matching mode arm to be revisited by the compiler — the same
//! exhaustiveness discipline `Mode`/`AppEvent` use (`CLAUDE.md` "Key Decisions").
//! A screen that is not reachable is one you can never navigate to
//! ([09](crate::app)/#14), so it never reaches [`render`], and there is no
//! "unavailable" render arm.
//!
//! # This issue's scope (#13)
//!
//! This lands the pure draw dispatch, the placeholder screen bodies, the
//! event-driven render loop ([`driver`]), the two-level key dispatch, and the
//! tick/input task seams the supervisor (#11) owns. The concrete screen bodies —
//! the chain matrix (#18), the theme/keymap/help-overlay **content** (#14), and
//! the render goldens (#19) — land in later issues; the screen `draw`/`handle_key`
//! functions here are honest placeholders (a titled block), never fabricated data.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, LiveScreen, Mode, ReplayScreen};

pub mod chain;
pub mod depth;
pub mod driver;
pub mod payoff;
pub mod replay;
pub mod surface;

// ---------------------------------------------------------------------------
// The root layout: status bar (top), screen body (middle), hint line (bottom).
// ---------------------------------------------------------------------------

/// The three regions of the root layout (`docs/05-views-and-ux.md` §8): a
/// one-line status bar on top, the screen body in the middle, and a one-line
/// hint/keybar on the bottom. The help overlay floats over the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootLayout {
    /// The one-line status bar region (top).
    pub status: Rect,
    /// The screen body region (middle) — where the active screen draws.
    pub body: Rect,
    /// The one-line hint/keybar region (bottom).
    pub hint: Rect,
}

/// Split `area` into the status bar, body, and hint line
/// (`docs/05-views-and-ux.md` §8).
///
/// Uses [`Layout::areas`], which yields a fixed-size `[Rect; 3]` — so there is no
/// unchecked index, and a zero-width or zero-height `area` yields zero-size
/// regions the widgets render as empty rather than panicking.
#[must_use]
pub fn layout_root(area: Rect) -> RootLayout {
    let [status, body, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    RootLayout { status, body, hint }
}

// ---------------------------------------------------------------------------
// The total, wildcard-free draw dispatch (§7).
// ---------------------------------------------------------------------------

/// Draw the whole frame from `app` — the **pure**, total, wildcard-free draw
/// dispatch (`docs/02-tui-architecture.md` §7).
///
/// Takes `&App` (never `&mut`), so a draw cannot mutate state or perform I/O. The
/// dispatch is a total match — the mode first, then an exhaustive match over that
/// mode's screens with **no `_` arm** — so a new screen forces the matching mode
/// arm to be revisited by the compiler.
pub fn render(app: &App, frame: &mut Frame) {
    let root = layout_root(frame.area());
    draw_status(app, frame, root.status);
    match &app.mode {
        Mode::Live(state) => match state.screen {
            LiveScreen::Chain => chain::draw(state, frame, root.body),
            LiveScreen::Depth => depth::draw(state, frame, root.body),
            LiveScreen::Surface => surface::draw(state, frame, root.body),
            LiveScreen::Payoff => payoff::draw(state, frame, root.body),
        },
        Mode::Replay(state) => match state.screen {
            ReplayScreen::Replay => replay::draw(state, frame, root.body),
            ReplayScreen::Payoff => payoff::draw_replay(state, frame, root.body),
        },
    }
    draw_hint(app, frame, root.hint);
    if app.help_open {
        draw_help_overlay(frame, root.body);
    }
}

// ---------------------------------------------------------------------------
// Status bar, hint line, and help overlay (placeholders — content is #14).
// ---------------------------------------------------------------------------

/// Draw a **minimal** status bar (`docs/05-views-and-ux.md` §8). The full,
/// always-truthful status line — provider health badge, clock, run id — lands with
/// the theme/status work (#14); this placeholder shows only the mode, never a
/// fabricated health badge.
fn draw_status(app: &App, frame: &mut Frame, area: Rect) {
    let label = match &app.mode {
        Mode::Live(_) => "chainview  live",
        Mode::Replay(_) => "chainview  replay",
    };
    let line = Line::from(Span::styled(
        label,
        Style::new().add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw a **minimal** one-line hint/keybar (`docs/05-views-and-ux.md` §8). The
/// full keybar, generated from the single keybinding map, lands with the keymap
/// (#14); this placeholder names only the two always-available globals.
fn draw_hint(app: &App, frame: &mut Frame, area: Rect) {
    let hint = if app.help_open {
        "?/Esc  close help"
    } else {
        "?  help     q  quit"
    };
    let line = Line::from(Span::styled(hint, Style::new().add_modifier(Modifier::DIM)));
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw the modal help overlay (`docs/02-tui-architecture.md` §9,
/// `docs/05-views-and-ux.md` §3) — a placeholder floating over the body. The
/// overlay **content** (the keybinding table generated from the map) lands with
/// the keymap (#14); this proves the modal frame and the [`Clear`] behind it.
fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let overlay = centered_rect(area, 44, 7);
    // Clear the cells behind the overlay so the body does not bleed through.
    frame.render_widget(Clear, overlay);
    let block = Block::bordered().title("Help");
    let text = Text::from(vec![
        Line::from("Keybinding help lands with the keymap (#14)."),
        Line::from(""),
        Line::from(Span::styled(
            "?/Esc  close",
            Style::new().add_modifier(Modifier::DIM),
        )),
    ]);
    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, overlay);
}

// ---------------------------------------------------------------------------
// Shared placeholder body + centered-rect helper.
// ---------------------------------------------------------------------------

/// Render an honest placeholder screen body: a titled bordered block with a
/// dimmed "coming soon" note (`docs/05-views-and-ux.md` §2.1). The real bodies —
/// the chain matrix (#18), depth (v0.5), surface/payoff (v0.2), replay (v0.3) —
/// replace these; a placeholder never fabricates market data.
pub(crate) fn placeholder_body(frame: &mut Frame, area: Rect, title: &str, note: &str) {
    let block = Block::bordered().title(title.to_owned());
    let text = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            note.to_owned(),
            Style::new().add_modifier(Modifier::DIM),
        )),
    ]);
    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// A `width`×`height` rectangle centered in `area`, clamped to `area` so it never
/// escapes its bounds. Uses [`Flex::Center`] so there is no manual geometry
/// arithmetic (and no `saturating_*`, per `rules/global_rules.md`).
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(row);
    cell
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::{layout_root, render};
    use crate::app::tests_support::{live_app_on, replay_app_on};
    use crate::app::{LiveScreen, Mode, ReplayScreen, ScreenLoad};

    /// A `TestBackend`-backed terminal for pure render assertions (no runtime, no
    /// real TTY).
    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend terminal construction failed: {e}"),
        }
    }

    #[test]
    fn test_layout_root_splits_status_body_hint() {
        let root = layout_root(Rect::new(0, 0, 80, 24));
        assert_eq!(root.status.height, 1, "the status bar is one line");
        assert_eq!(root.hint.height, 1, "the hint line is one line");
        assert_eq!(root.body.height, 22, "the body takes the remaining rows");
        assert_eq!(root.status.y, 0);
        assert_eq!(root.body.y, 1);
        assert_eq!(root.hint.y, 23);
    }

    #[test]
    fn test_layout_root_zero_area_yields_zero_regions_without_panic() {
        // A zero-size area must not panic (no unchecked index into the split).
        let root = layout_root(Rect::new(0, 0, 0, 0));
        assert_eq!(root.status.width, 0);
        assert_eq!(root.body.height, 0);
    }

    #[test]
    fn test_render_is_pure_does_not_mutate_app() {
        // `render` takes `&App`, so a draw cannot mutate state — the purity
        // guarantee is enforced by the signature. This test documents it by
        // asserting the observable app state is byte-for-byte unchanged across a
        // draw (dirty/help/quit/screen).
        let app = live_app_on(LiveScreen::Surface, ScreenLoad::Loading, true);
        let before = (app.dirty, app.help_open, app.should_quit);
        let before_screen = match &app.mode {
            Mode::Live(s) => s.screen,
            Mode::Replay(_) => panic!("expected a live app"),
        };
        let mut terminal = terminal(80, 24);
        match terminal.draw(|frame| render(&app, frame)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
        let after = (app.dirty, app.help_open, app.should_quit);
        let after_screen = match &app.mode {
            Mode::Live(s) => s.screen,
            Mode::Replay(_) => panic!("expected a live app"),
        };
        assert_eq!(before, after, "render must not mutate flags");
        assert_eq!(
            before_screen, after_screen,
            "render must not switch screens"
        );
    }

    #[test]
    fn test_render_help_overlay_open_and_closed_never_panics() {
        for help in [false, true] {
            let app = live_app_on(LiveScreen::Chain, ScreenLoad::Loading, help);
            let mut terminal = terminal(100, 30);
            match terminal.draw(|frame| render(&app, frame)) {
                Ok(_) => {}
                Err(e) => panic!("draw failed (help={help}): {e}"),
            }
        }
    }

    #[test]
    fn test_render_every_reachable_live_screen_never_panics() {
        let screens = [
            LiveScreen::Chain,
            LiveScreen::Depth,
            LiveScreen::Surface,
            LiveScreen::Payoff,
        ];
        let loads = [
            ScreenLoad::Loading,
            ScreenLoad::Ready,
            ScreenLoad::Error {
                message: "provider unreachable".to_owned(),
            },
        ];
        for screen in screens {
            for load in &loads {
                let app = live_app_on(screen, load.clone(), false);
                let mut terminal = terminal(120, 40);
                match terminal.draw(|frame| render(&app, frame)) {
                    Ok(_) => {}
                    Err(e) => panic!("draw failed ({screen:?}/{load:?}): {e}"),
                }
            }
        }
    }

    #[test]
    fn test_render_every_reachable_replay_screen_never_panics() {
        for screen in [ReplayScreen::Replay, ReplayScreen::Payoff] {
            let app = replay_app_on(screen, false);
            let mut terminal = terminal(120, 40);
            match terminal.draw(|frame| render(&app, frame)) {
                Ok(_) => {}
                Err(e) => panic!("draw failed ({screen:?}): {e}"),
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, max_shrink_iters: 50_000, ..ProptestConfig::default() })]

        /// Drawing **any reachable app state** into a `TestBackend` never panics
        /// (`docs/TESTING.md` §3 `render_never_panics`). The generated state space
        /// enumerates: both modes; every reachable screen (all four `LiveScreen`s,
        /// both `ReplayScreen`s); help open and closed; every live load state
        /// (`Loading` / `Ready` / `Error`); and any terminal size from 1x1 up —
        /// stressing the near-zero-area layout, the centered help overlay, and the
        /// truncating status/hint lines. The assertion is that the draw completes
        /// (a panic — an unchecked index, an out-of-bounds rect — fails the case).
        #[test]
        fn prop_render_never_panics(
            mode_idx in 0u8..2,
            live_screen_idx in 0u8..4,
            replay_screen_idx in 0u8..2,
            load_idx in 0u8..3,
            help in any::<bool>(),
            width in 1u16..160,
            height in 1u16..60,
        ) {
            let app = if mode_idx == 0 {
                let screen = match live_screen_idx {
                    0 => LiveScreen::Chain,
                    1 => LiveScreen::Depth,
                    2 => LiveScreen::Surface,
                    _ => LiveScreen::Payoff,
                };
                let load = match load_idx {
                    0 => ScreenLoad::Loading,
                    1 => ScreenLoad::Ready,
                    _ => ScreenLoad::Error {
                        message: "provider unreachable".to_owned(),
                    },
                };
                live_app_on(screen, load, help)
            } else {
                let screen = if replay_screen_idx == 0 {
                    ReplayScreen::Replay
                } else {
                    ReplayScreen::Payoff
                };
                replay_app_on(screen, help)
            };
            let mut terminal = terminal(width, height);
            match terminal.draw(|frame| render(&app, frame)) {
                Ok(_) => {}
                Err(e) => prop_assert!(false, "draw failed at {width}x{height}: {e}"),
            }
        }
    }
}
