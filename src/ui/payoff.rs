//! The payoff-diagram screen — expiration and t+0 (`docs/05-views-and-ux.md` §4).
//!
//! The real screen renders `optionstratlib` `Profit` / `GraphData` (a `Series2D`
//! of price → P&L) that the domain layer builds and caches — never inside `draw`
//! (`docs/02-tui-architecture.md` §7). Payoff is reachable in **both** modes: from
//! the multi-leg builder in live mode ([`draw`]) and from the open position at the
//! replay head ([`draw_replay`], v0.5). This issue (#13) lands the pure
//! `draw`/`handle_key` seams with placeholder bodies; the real payoff lands in
//! v0.2 (live) / v0.5 (replay). All functions are pure — no I/O, no `.await`.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{LiveState, ReplayState};
use crate::event::AppEvent;

/// Draw the live multi-leg payoff for `state` into `area` — a pure render over the
/// borrowed, pre-built `GraphData` (never recomputed here). Placeholder body until
/// v0.2.
pub fn draw(_state: &LiveState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(
        frame,
        area,
        "Payoff",
        "multi-leg payoff diagram lands in v0.2",
    );
}

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

/// Handle a live-payoff-local key, returning any follow-on [`AppEvent`]. Pure — no
/// I/O. Leg add/remove/quantity/side and the t+0 toggle land with the builder
/// (v0.2).
#[must_use]
pub fn handle_key(_state: &mut LiveState, _key: KeyEvent) -> Option<AppEvent> {
    None
}

/// Handle a replay-payoff-local key, returning any follow-on [`AppEvent`]. Pure —
/// no I/O. Lands with the replay payoff (v0.5).
#[must_use]
pub fn handle_key_replay(_state: &mut ReplayState, _key: KeyEvent) -> Option<AppEvent> {
    None
}
