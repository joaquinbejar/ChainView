//! The replay screen — equity curve, P&L attribution by Greek, per-trade
//! drill-down (`docs/05-views-and-ux.md` §5).
//!
//! The real screen renders `replay-expert`'s data model at the scrub head
//! (equity + drawdown, by-Greek attribution displayed as authored, fills) — the
//! timeline/bundle math lives upstream, borrowed by the widget. This issue (#13)
//! lands the pure `draw`/`handle_key` seam with a placeholder body; the real
//! screen lands in v0.3. Both functions are pure — no I/O, no `.await`.
//!
//! [`handle_key`] demonstrates the two-level dispatch producing a follow-on event:
//! a scrub key returns an [`AppEvent::ReplaySeek`] the render loop folds through
//! [`App::on_event`](crate::App::on_event), which emits a `SeekBundle` command to
//! the data layer — the widget itself never performs the seek I/O
//! (`docs/02-tui-architecture.md` §9).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::ReplayState;
use crate::event::{AppEvent, SeekTo};

/// Draw the replay screen for `state` into `area` — a pure render over the
/// borrowed bundle/timeline model (never recomputed here). Placeholder body until
/// v0.3.
pub fn draw(_state: &ReplayState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(
        frame,
        area,
        "Replay",
        "equity curve, P&L attribution, and drill-down land in v0.3",
    );
}

/// Handle a replay-local key, returning any follow-on [`AppEvent`]
/// (`docs/05-views-and-ux.md` §3). Pure — no I/O; a scrub key returns an
/// [`AppEvent::ReplaySeek`] rather than seeking inline.
///
/// The step-relative scrubs (`←`/`→` step back/forward, `Home` to the start) are
/// expressed against the one integer replay clock (`docs/04-replay-mode.md` §4).
/// `End` needs the last step, which the timeline model (v0.3) owns, so it is a
/// no-op placeholder until then. The remaining replay keys (`Space` play/pause,
/// `,`/`.` fill drill-down, speed) land with the replay screen (v0.3).
#[must_use]
pub fn handle_key(_state: &mut ReplayState, key: KeyEvent) -> Option<AppEvent> {
    match key.code {
        KeyCode::Left => Some(AppEvent::ReplaySeek(SeekTo::StepBy(-1))),
        KeyCode::Right => Some(AppEvent::ReplaySeek(SeekTo::StepBy(1))),
        KeyCode::Home => Some(AppEvent::ReplaySeek(SeekTo::Step(0))),
        _ => None,
    }
}
