//! The order-book depth-ladder screen (`docs/05-views-and-ux.md` §2.1).
//!
//! Depth is instrument-scoped and capability-gated (Deribit option instruments;
//! Alpaca crypto-spot only), so the depth screen is only reachable when the active
//! provider declares `depth` — the render dispatch never reaches [`draw`]
//! otherwise. This issue (#13) lands the pure `draw`/`handle_key` seam with a
//! placeholder body; the real ladder and its empty/stale/resync states land in a
//! later issue (v0.5). Both functions are pure — no I/O, no `.await`.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::LiveState;
use crate::event::AppEvent;

/// Draw the depth ladder for the live `state` into `area` — a pure render.
/// Placeholder body until the depth screen lands (v0.5).
pub fn draw(_state: &LiveState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(
        frame,
        area,
        "Depth",
        "order-book depth ladder lands in v0.5",
    );
}

/// Handle a depth-local key, returning any follow-on [`AppEvent`]. Pure — no I/O.
/// Ladder scroll lands with the depth screen (v0.5).
#[must_use]
pub fn handle_key(_state: &mut LiveState, _key: KeyEvent) -> Option<AppEvent> {
    None
}
