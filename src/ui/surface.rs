//! The volatility smile / surface screen (`docs/05-views-and-ux.md` §4).
//!
//! The real screen renders `optionstratlib` `Curve` / `Surface` / `GraphData`
//! (`Series2D` → line chart, `Surface3D` → shaded grid) that the domain layer
//! builds and caches — a `GraphData` is **never** built inside `draw`
//! (`docs/02-tui-architecture.md` §7). This issue (#13) lands the pure
//! `draw`/`handle_key` seam with a placeholder body; the real smile/surface lands
//! in v0.2. Both functions are pure — no I/O, no `.await`, no `GraphData` build.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::LiveState;
use crate::event::AppEvent;

/// Draw the vol smile / surface for the live `state` into `area` — a pure render
/// over borrowed, pre-built `GraphData` (never recomputed here). Placeholder body
/// until v0.2.
pub fn draw(_state: &LiveState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(frame, area, "Surface", "vol smile / surface lands in v0.2");
}

/// Handle a surface-local key, returning any follow-on [`AppEvent`]. Pure — no
/// I/O. Greek-axis cycling / smile-surface toggle land with the surface screen
/// (v0.2).
#[must_use]
pub fn handle_key(_state: &mut LiveState, _key: KeyEvent) -> Option<AppEvent> {
    None
}
