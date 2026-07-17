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
    /// The replay equity-curve projection cache (#35).
    replay: ReplayView,
}

impl Default for ViewState {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewState {
    /// A fresh view cache: the payoff and replay-equity projections seeded from an
    /// empty series, so the first frame renders the deliberate empty state ("add a
    /// leg" / "no equity rows") rather than a blank.
    #[must_use]
    pub fn new() -> Self {
        Self {
            payoff: PayoffView::new(),
            replay: ReplayView::new(),
        }
    }

    /// Re-project any screen geometry whose source changed, **off** the draw path.
    /// Called by the render loop between the event fold and the draw (gated on
    /// [`App::dirty`](crate::App::dirty)).
    ///
    /// In **live** mode it diffs the payoff builder's
    /// [`graph_revision`](crate::app::PayoffBuilder::graph_revision) and re-projects
    /// only on a change. In **replay** mode it diffs the loaded bundle's
    /// [`equity_revision`](crate::app::LoadedReplay::equity_revision) and re-projects
    /// the equity series only when the cursor moved (#35); a loading/failed bundle
    /// resets the cache to the empty projection, so a fresh bundle always re-projects.
    pub fn sync(&mut self, app: &App) {
        match &app.mode {
            Mode::Live(live) => self.payoff.sync(live.payoff_builder.graph_revision(), || {
                live.payoff_builder.active_graph_data().clone()
            }),
            Mode::Replay(replay) => match replay.loaded() {
                Some(loaded) => self
                    .replay
                    .sync(loaded.equity_revision(), || loaded.equity_graph().clone()),
                // Loading / failed: no equity to show — reset so the next `Ready`
                // (a fresh bundle whose revision restarts at 0) always re-projects.
                None => self.replay.reset(),
            },
        }
    }

    /// The cached payoff [`GraphProjection`] — the only thing the payoff screen's
    /// `draw` reads (a borrow), so the paint builds no `GraphData` (#27).
    #[must_use]
    pub fn payoff(&self) -> &GraphProjection {
        self.payoff.cache.projection()
    }

    /// The cached replay equity [`GraphProjection`] — the only geometry the replay
    /// screen's `draw` reads (a borrow), so the paint builds no `GraphData` (#35).
    #[must_use]
    pub fn replay(&self) -> &GraphProjection {
        self.replay.cache.projection()
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

/// The replay screen's equity-curve projection cache: the [`GraphCache`] plus the
/// [`equity_revision`](crate::app::LoadedReplay::equity_revision) it was last
/// projected for, so a re-project fires only when the cursor actually moved the
/// as-of equity slice (#35).
#[derive(Debug, Clone)]
struct ReplayView {
    /// The projected equity series (the #23 cache, projected off the draw path).
    cache: GraphCache,
    /// The bundle's equity revision the cache was projected for, or `None` before
    /// the first sync / after a reset to a loading/failed bundle.
    projected_for: Option<u64>,
}

impl ReplayView {
    /// A replay view seeded with an empty series → `Empty(NoData)` → the "no equity
    /// rows" empty state.
    fn new() -> Self {
        Self {
            cache: GraphCache::new(GraphData::Series(Series2D::default())),
            projected_for: None,
        }
    }

    /// Re-project the equity series when `revision` differs from the last projected
    /// one, pulling the fresh `GraphData` from `source` (a closure so the clone
    /// happens only on a change). Off the draw path.
    fn sync(&mut self, revision: u64, source: impl FnOnce() -> GraphData) {
        if self.projected_for != Some(revision) {
            self.cache.update(source());
            self.projected_for = Some(revision);
        }
    }

    /// Reset to the empty projection when there is no loaded bundle (loading /
    /// failed), so a fresh bundle whose revision restarts at `0` always re-projects.
    /// Idempotent — only touches the cache when it was projecting something.
    fn reset(&mut self) {
        if self.projected_for.is_some() {
            self.cache.update(GraphData::Series(Series2D::default()));
            self.projected_for = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ViewState;
    use crate::app::tests_support::{live_app_on, ready_replay_app, ready_replay_app_with_fills};
    use crate::app::{LiveScreen, ScreenLoad};
    use crate::event::SeekTo;

    #[test]
    fn test_sync_projects_replay_equity_for_a_ready_bundle() {
        // A Ready bundle's equity series projects to a renderable chart off the draw
        // path; a live app leaves the replay cache at its empty seed.
        let (replay, _rx) = ready_replay_app_with_fills(6);
        let mut view = ViewState::new();
        view.sync(&replay);
        assert!(
            view.replay().ready().is_some(),
            "a Ready bundle projects a renderable equity series",
        );
        let live = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, false);
        let mut live_view = ViewState::new();
        live_view.sync(&live);
        assert!(
            live_view.replay().empty_reason().is_some(),
            "a live app leaves the replay cache empty",
        );
    }

    #[test]
    fn test_sync_reprojects_only_when_the_equity_revision_changes() {
        // The revision-diff cache re-projects on a cursor move and no-ops on an
        // unrelated re-sync (the #27 pattern applied to replay).
        let (mut replay, _rx) = ready_replay_app_with_fills(6);
        let mut view = ViewState::new();
        view.sync(&replay);
        let first = view.replay().clone();
        // Re-syncing with no change reuses the cached projection (same value).
        view.sync(&replay);
        assert_eq!(
            &first,
            view.replay(),
            "an unchanged bundle does not re-project"
        );
        // A seek bumps the equity revision → the projection changes (fewer points).
        replay.on_event(crate::event::AppEvent::ReplaySeek(SeekTo::Step(2)));
        view.sync(&replay);
        assert_ne!(
            &first,
            view.replay(),
            "a cursor move re-projects the equity series to the new head",
        );
    }

    #[test]
    fn test_sync_resets_replay_cache_when_the_bundle_is_not_ready() {
        // A loading bundle (no `loaded()`) resets the cache to the empty projection,
        // so a later fresh bundle always re-projects even at revision 0.
        let (_ready, _rx) = ready_replay_app(6);
        let loading =
            crate::app::tests_support::replay_app_on(crate::app::ReplayScreen::Replay, false);
        let mut view = ViewState::new();
        view.sync(&loading);
        assert!(
            view.replay().empty_reason().is_some(),
            "a loading bundle leaves the replay cache empty",
        );
    }
}
