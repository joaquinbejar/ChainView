//! The render-loop-owned **view cache** — the off-draw home of every screen's
//! projected geometry (`docs/02-tui-architecture.md` §7, `docs/05-views-and-ux.md`
//! §4).
//!
//! # Why the projection cache lives here, not on app state
//!
//! Projecting an `optionstratlib` [`GraphData`] into a ratatui chart shape is
//! [`GraphCache`] work that must run **off** the draw path (#23,
//! `docs/02-tui-architecture.md` §7); but the application layer must not import
//! `crate::ui` (the layering arch test, `tests/arch.rs`). The reconciliation is
//! this [`ViewState`]: a **ui-owned** cache the render loop threads alongside the
//! [`App`], synced between frames from borrowed app state and read (a borrow) in
//! `draw`. The application layer holds only `optionstratlib` `GraphData` (built off
//! the draw path in [`PayoffBuilder`](crate::app::PayoffBuilder)); the ui projects
//! it here.
//!
//! # The sync is off-draw; the read in `draw` is pure
//!
//! [`sync`](ViewState::sync) mutates the cache **before** the draw, diffing a cheap
//! revision so it re-projects only when the source geometry actually changed. In
//! `draw`, a screen reads [`payoff`](ViewState::payoff) — an immutable borrow of the
//! cached [`GraphProjection`] — so the paint builds no `GraphData` and does no
//! projection work.

use crate::app::{App, Mode};
use crate::ui::graph::{GraphCache, GraphProjection};
use optionstratlib::visualization::{GraphData, Series2D};

/// The render-loop-owned cache of every screen's projected geometry, threaded
/// alongside [`App`] through the render loop and synced off the draw path.
#[derive(Debug, Clone)]
pub struct ViewState {
    /// The payoff screen's projection cache (#27).
    payoff: PayoffView,
}

impl Default for ViewState {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewState {
    /// A fresh view cache: the payoff projection seeded from an empty series, so
    /// the first frame renders the deliberate "add a leg" empty state rather than a
    /// blank.
    #[must_use]
    pub fn new() -> Self {
        Self {
            payoff: PayoffView::new(),
        }
    }

    /// Re-project any screen geometry whose source changed, **off** the draw path.
    /// Called by the render loop between the event fold and the draw (gated on
    /// [`App::dirty`](crate::App::dirty)); it diffs the payoff builder's
    /// [`graph_revision`](crate::app::PayoffBuilder::graph_revision) and re-projects
    /// only on a change. A no-op in replay mode (no live payoff builder).
    pub fn sync(&mut self, app: &App) {
        match &app.mode {
            Mode::Live(live) => self.payoff.sync(live.payoff_builder.graph_revision(), || {
                live.payoff_builder.active_graph_data().clone()
            }),
            // Replay mode has no live payoff builder; its own view geometry (#35)
            // lands with the replay screens.
            Mode::Replay(_) => {}
        }
    }

    /// The cached payoff [`GraphProjection`] — the only thing the payoff screen's
    /// `draw` reads (a borrow), so the paint builds no `GraphData` (#27).
    #[must_use]
    pub fn payoff(&self) -> &GraphProjection {
        self.payoff.cache.projection()
    }
}

/// The payoff screen's projection cache: the [`GraphCache`] plus the
/// [`graph_revision`](crate::app::PayoffBuilder::graph_revision) it was last
/// projected for, so a re-project fires only on a real geometry change.
#[derive(Debug, Clone)]
struct PayoffView {
    /// The projected payoff series (the #23 cache, projected off the draw path).
    cache: GraphCache,
    /// The builder graph revision the cache was projected for, or `None` before
    /// the first sync.
    projected_for: Option<u64>,
}

impl PayoffView {
    /// A payoff view seeded with an empty series → `Empty(NoData)` → the "add a
    /// leg" empty state.
    fn new() -> Self {
        Self {
            cache: GraphCache::new(GraphData::Series(Series2D::default())),
            projected_for: None,
        }
    }

    /// Re-project the payoff series when `revision` differs from the last projected
    /// one, pulling the fresh `GraphData` from `source` (a closure so the clone
    /// happens only on a change). Off the draw path.
    fn sync(&mut self, revision: u64, source: impl FnOnce() -> GraphData) {
        if self.projected_for != Some(revision) {
            self.cache.update(source());
            self.projected_for = Some(revision);
        }
    }
}
