//! The replay screen — equity curve, P&L attribution by Greek, per-trade
//! drill-down (`docs/05-views-and-ux.md` §5, §6).
//!
//! The real screen renders `replay-expert`'s data model at the scrub head
//! (equity + drawdown, by-Greek attribution displayed as authored, fills) — the
//! timeline/bundle math lives upstream, borrowed by the widget. [`draw`] renders
//! the **bundle-load lifecycle** — the loading spinner and the retryable error —
//! first (the states-first rule, `docs/05-views-and-ux.md` §6), with the populated
//! `Ready` body (equity curve, attribution, drill-down) landing in #35. Both
//! [`draw`] and [`handle_key`] are pure — no I/O, no `.await`.
//!
//! [`handle_key`] demonstrates the two-level dispatch producing a follow-on event:
//! a scrub key returns an [`AppEvent::ReplaySeek`] and a play/pause/speed key an
//! [`AppEvent::ReplayControl`], both of which the render loop folds through
//! [`App::on_event`](crate::App::on_event) — the seek moves the in-memory timeline
//! cursor and the control folds into playback state, so the widget itself performs
//! no I/O and never `.await`s (`docs/02-tui-architecture.md` §9,
//! `docs/04-replay-mode.md` §4).

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::keymap::{KeyChord, ReplayAction, resolve_replay};
use crate::app::{BundleLoad, ReplayState};
use crate::event::{AppEvent, ReplayControl, SeekTo};
use crate::ui::theme::{Theme, run_label, sanitize, spinner_frame};

/// Draw the replay screen for `state` into `area` — a pure render over the
/// borrowed bundle/timeline model (never recomputed here, `docs/02-tui-architecture.md`
/// §7). Renders the bundle-load lifecycle first (the states-first rule,
/// `docs/05-views-and-ux.md` §6), matched exhaustively over [`BundleLoad`]:
///
/// - [`BundleLoad::Loading`] → a centered tick-driven spinner + "loading bundle
///   `<run>`…" and a dim secondary hint (the §6 loading idiom, matching the chain
///   screen's connecting state so the two animate in lock-step).
/// - [`BundleLoad::Error`] → the bounded, wrapped bundle-error message + an explicit
///   "press `R` to retry" affordance (glyph-prefixed, `NO_COLOR`-safe), so a
///   malformed bundle is never an invisible failure.
/// - [`BundleLoad::Ready`] → the deliberate hand-off placeholder; the equity curve,
///   P&L attribution, and per-trade drill-down render in #35 — never fabricated data.
///
/// `theme` (resolved, `NO_COLOR`-aware) and `tick` (for the loading spinner) are
/// `Copy`, so the draw stays pure over the borrowed state.
pub fn draw(state: &ReplayState, frame: &mut Frame, area: Rect, theme: Theme, tick: u64) {
    match &state.bundle {
        BundleLoad::Loading => draw_loading(frame, area, theme, &run_label(state), tick),
        BundleLoad::Error { message } => draw_error(frame, area, theme, message),
        BundleLoad::Ready(_) => super::placeholder_body(
            frame,
            area,
            "Replay",
            "equity curve, P&L attribution, and drill-down land in #35",
        ),
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
/// state without color (matching the chain error body), `NO_COLOR`-safe; the message
/// is sanitized at this render edge.
fn draw_error(frame: &mut Frame, area: Rect, theme: Theme, message: &str) {
    let block = Block::bordered().title(Span::styled("Replay", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Reserve the last inner row for the retry hint; the message wraps in the space
    // above it (`Layout::areas` yields zero-size regions on a tiny area, never a
    // panic).
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

/// Draw a centered state body inside the framed "Replay" block — vertically and
/// horizontally centered, so a lifecycle state reads as deliberate, never a blank
/// void or a top-anchored fragment. Mirrors the chain screen's state bodies
/// (`src/ui/chain.rs`); `Flex::Center` does the geometry (no manual arithmetic, no
/// banned `saturating_*`).
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

/// Handle a replay-local key, returning any follow-on [`AppEvent`]
/// (`docs/05-views-and-ux.md` §3). Pure — no I/O; a scrub key returns an
/// [`AppEvent::ReplaySeek`] and a play/pause/speed key an
/// [`AppEvent::ReplayControl`] rather than mutating state inline.
///
/// The key is resolved **through the single keybinding map**
/// ([`resolve_replay`], `src/app/keymap.rs`) so the dispatch and the help overlay
/// cannot drift. The scrubs (`←`/`→`/`h`/`l` step back/forward, `Home`/`End` to the
/// first/last step) are expressed against the one integer replay clock; `End` uses
/// a saturating [`SeekTo::Step`] because the cursor clamps to `end_step`
/// (`docs/04-replay-mode.md` §4). Play/pause (`Space`) and speed (`+`/`-`) fold into
/// [`Playback`](crate::Playback). The fill drill-down (`,` / `.`) lands with the
/// drill-down render (#35+), so those resolve to a no-op placeholder here.
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
        // Fill drill-down navigation lands with the drill-down render (#35+).
        ReplayAction::PrevFill | ReplayAction::NextFill => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::draw;
    use crate::app::tests_support::loaded_bundle;
    use crate::app::{BundleLoad, ReplayState};
    use crate::config::ThemeChoice;
    use crate::event::BundleLoadResult;
    use crate::ui::theme::Theme;

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

    /// Render `state` into a `width`×`height` backend and collect the buffer text
    /// (row-major) for content assertions.
    #[track_caller]
    fn rendered(state: &ReplayState, tick: u64, width: u16, height: u16) -> String {
        let mut term = terminal(width, height);
        match term.draw(|frame| {
            let area = frame.area();
            draw(state, frame, area, theme(), tick);
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

    fn ready_state() -> ReplayState {
        let mut state = ReplayState::new(PathBuf::from("/tmp/run-2025"));
        state.apply_load_result(BundleLoadResult::Loaded(Box::new(loaded_bundle(4))));
        state
    }

    #[test]
    fn test_draw_loading_shows_spinner_and_run_label() {
        let state = loading_state();
        assert!(matches!(state.bundle, BundleLoad::Loading));
        let text = rendered(&state, 0, 80, 24);
        assert!(
            text.contains("loading bundle"),
            "loading label present: {text:?}"
        );
        assert!(text.contains("run-2025"), "the run/dir label is shown");
        // The spinner's first frame (tick 0) is present — the §6 loading idiom.
        assert!(text.contains('⠋'), "the loading spinner is shown");
    }

    #[test]
    fn test_draw_error_shows_message_and_retry_hint() {
        // A malformed bundle's Error must be VISIBLE with a discoverable `R` retry
        // (the reviewed BLOCKER): the body carries both the message and the hint.
        let state = error_state("manifest.json is malformed");
        let text = rendered(&state, 0, 80, 24);
        assert!(
            text.contains("manifest.json is malformed"),
            "the error message is shown: {text:?}",
        );
        assert!(
            text.contains("press R to retry"),
            "the R retry affordance is shown",
        );
    }

    #[test]
    fn test_draw_ready_shows_deferred_placeholder() {
        // Ready keeps the deliberate hand-off placeholder for #35 — never fabricated
        // equity/P&L data.
        let state = ready_state();
        assert!(matches!(state.bundle, BundleLoad::Ready(_)));
        let text = rendered(&state, 0, 80, 24);
        assert!(text.contains("Replay"), "the titled block is shown");
        assert!(text.contains("#35"), "the deferred hand-off note names #35");
    }

    #[test]
    fn test_draw_never_panics_over_states_and_small_sizes() {
        // Every lifecycle state draws without panic at any size (the too-small guard
        // lives in `render`, so `draw` must be robust on its own), including a long
        // wrapping error message.
        let states = [
            loading_state(),
            error_state(
                "bundle failed: a long error message that must wrap across a narrow \
                 body without panicking or clipping the retry affordance off-screen",
            ),
            ready_state(),
        ];
        for state in &states {
            for (w, h) in [(1u16, 1u16), (5, 3), (12, 4), (40, 8), (80, 24), (200, 60)] {
                let _ = rendered(state, 7, w, h);
            }
        }
    }
}
