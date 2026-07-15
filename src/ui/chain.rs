//! The chain-matrix screen (strikes × call/put: bid/ask/mark/IV/Greeks)
//! (`docs/05-views-and-ux.md` §4).
//!
//! This issue (#13) lands the pure `draw`/`handle_key` seam with an honest
//! placeholder body; the real chain matrix — the responsive column layout, the
//! moneyness shading, the empty/loading/stale/error states, and the strike
//! navigation — lands in #18. [`draw`] is pure (no I/O, no `.await`, no heavy
//! compute); [`handle_key`] is pure and returns any follow-on [`AppEvent`] the
//! render loop folds back.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::LiveState;
use crate::event::AppEvent;

/// Draw the chain matrix for the live `state` into `area` — a pure render
/// (`docs/02-tui-architecture.md` §7). Placeholder body until #18; the store is
/// borrowed, never recomputed.
pub fn draw(_state: &LiveState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(
        frame,
        area,
        "Chain",
        "chain matrix (strikes x call/put) lands in #18",
    );
}

/// Handle a chain-local key, returning any follow-on [`AppEvent`] for the render
/// loop to fold (`docs/02-tui-architecture.md` §9). Pure — no I/O. The strike /
/// expiry / underlying navigation lands with the chain matrix (#18), so this
/// placeholder consumes nothing.
#[must_use]
pub fn handle_key(_state: &mut LiveState, _key: KeyEvent) -> Option<AppEvent> {
    None
}
