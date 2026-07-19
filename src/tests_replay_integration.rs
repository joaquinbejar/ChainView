//! The end-to-end **replay-path** integration tests and the committed replay
//! render goldens (issue #37 — the v0.3 acceptance gate; `docs/TESTING.md` §7/§4,
//! `docs/ROADMAP.md` v0.3, `docs/05-views-and-ux.md` §2.1/§5).
//!
//! # Why these live in-crate (the golden harness is `pub(crate)`)
//!
//! A file under `tests/` is a **separate crate** that links `chainview` and sees
//! only its PUBLIC API. The replay goldens need the `#[cfg(test)]` golden helper
//! ([`assert_golden`](crate::ui::golden::assert_golden) /
//! [`buffer_to_text`](crate::ui::golden::buffer_to_text)) and the `pub(crate)`
//! two-level key dispatch ([`App::dispatch_key_global`](crate::App::dispatch_key_global)
//! / [`KeyRoute`](crate::app::KeyRoute)), none of which is on the semver-governed
//! surface — for the **same reason the #19/#28 render goldens live in-crate**:
//! promoting them would widen the public API. So the replay end-to-end golden
//! render, the scrub-consistency proof, the bad-schema error render, and the
//! two-reachable-screens assertion live here, `#[cfg(test)]`, mirroring the
//! live-path harness in `src/tests_integration.rs`.
//!
//! # The production composition under test
//!
//! Each test drives the replay path as close to production as the harness allows:
//! open + decode the committed conformance fixture through the **real**
//! [`BundleReader`] (`open` + `load`, the same reader the off-thread worker runs),
//! fold the outcome through the **real** [`App::on_event`](crate::App::on_event)
//! (`AppEvent::BundleLoaded`, exactly what `spawn_bundle_load` delivers), sync the
//! ui view cache off the draw path ([`ViewState::sync`](crate::ViewState)), and
//! render the whole frame through the **real** [`render`](crate::ui::render) — the
//! only step skipped is the `spawn_blocking` worker thread (its outcome is folded
//! synchronously so the test is deterministic and network-free).
//!
//! Every test is deterministic (committed fixture bytes + a fixed `tick_count == 0`,
//! no socket, no wall-clock read) and finishes far under the 10 s integration bound
//! (`docs/TESTING.md` §7).

#![cfg(test)]

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

use crate::app::tests_support::loaded_bundle;
use crate::app::{
    App, BundleLoad, KeyRoute, Mode, ReplayScreen, ReplayState, is_replay_screen_reachable,
};
use crate::config::ThemeChoice;
use crate::error::BundleError;
use crate::event::{AppEvent, BundleLoadResult, Command, ReplayControl, SeekTo};
use crate::replay::{BundleReader, LoadedBundle};
use crate::ui::golden::{GOLDEN_HEIGHT, GOLDEN_WIDTH, assert_golden, buffer_to_text};
use crate::ui::render;
use crate::ui::view::ViewState;

// --- Fixture + harness helpers (no unwrap/expect/indexing per the ruleset) ----

/// The committed conformance fixture used as the natural end-to-end replay input
/// (`tests/fixtures/bundle/valid/`, the #36 corpus). Its highest step (its
/// `end_step`) — the run has `VALID_STEPS` (4) steps, `0..=3`.
const VALID_END_STEP: u32 = 3;

/// The path of one committed bundle fixture directory, anchored at the crate root
/// via `CARGO_MANIFEST_DIR` so it resolves the same from any working directory.
fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("bundle")
        .join(name)
}

/// Open + load a committed bundle fixture through the **real** [`BundleReader`]
/// (the same open/decode/validate path the off-thread load worker runs), asserting
/// success — the production reader seam, network-free.
#[track_caller]
fn load_fixture_bundle(name: &str) -> LoadedBundle {
    let reader = match BundleReader::open(fixture_dir(name)) {
        Ok(reader) => reader,
        Err(e) => panic!("the `{name}` fixture must open through the real reader: {e}"),
    };
    match reader.load() {
        Ok(bundle) => bundle,
        Err(e) => panic!("the `{name}` fixture must load through the real reader: {e}"),
    }
}

/// A replay [`App`] with the `name` fixture folded to [`BundleLoad::Ready`] through
/// the **real** event path, plus its command-channel receiver (so a test can assert
/// on any emitted [`Command`]). The bundle is delivered as a single
/// [`AppEvent::BundleLoaded`] — exactly what the off-thread worker sends — and
/// folded by [`App::on_event`], which builds the [`crate::TimelineCursor`] and the
/// cached equity geometry (`LoadedReplay::new`).
#[track_caller]
fn replay_app_from_fixture(name: &str) -> (App, mpsc::Receiver<Command>) {
    let loaded = load_fixture_bundle(name);
    let (tx, rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(
        Mode::Replay(ReplayState::new(fixture_dir(name))),
        ThemeChoice::Auto,
        tx,
    );
    app.on_event(AppEvent::BundleLoaded(BundleLoadResult::Loaded(Box::new(
        loaded,
    ))));
    app.mark_drawn();
    (app, rx)
}

/// Sync the ui view cache off the draw path and render the **whole frame** through
/// the real [`render`] into a fixed [`GOLDEN_WIDTH`]×[`GOLDEN_HEIGHT`] `TestBackend`
/// at `tick_count == 0` (so the loading spinner frame is fixed), returning the
/// buffer as golden text — the exact #19 golden harness, so the bytes are stable.
#[track_caller]
fn render_replay_frame(app: &App) -> String {
    let mut view = ViewState::new();
    view.sync(app);
    let mut term = match Terminal::new(TestBackend::new(GOLDEN_WIDTH, GOLDEN_HEIGHT)) {
        Ok(term) => term,
        Err(e) => panic!("TestBackend construction failed: {e}"),
    };
    match term.draw(|frame| render(app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("replay frame render failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

/// A `Char` key press (kind `Press`, no modifiers) — `KeyChord::from_event` maps the
/// resolved char directly, so `press('R')` normalizes to `KeyChord::Char('R')`.
fn press(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// A `Tab` key press (the `NextScreen` cycle global).
fn tab() -> KeyEvent {
    KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
}

/// Drive `key` through the **two-level dispatch** exactly as the render loop does
/// (`src/ui/driver.rs`): the globals / modal-help first, then — for an unbound
/// (`ToScreen`) key — the active replay screen's `handle_key`, folding any follow-on
/// event. The replay drill-down (`,` / `.`) mutates the in-memory selection directly
/// and returns no event, so this is how a test moves the selection production-faithfully.
fn dispatch_key(app: &mut App, key: KeyEvent) {
    match app.dispatch_key_global(key) {
        KeyRoute::Consumed => {}
        KeyRoute::ToScreen => {
            let follow = match &mut app.mode {
                Mode::Replay(replay) => crate::ui::replay::handle_key(replay, key),
                Mode::Live(_) => panic!("expected replay mode"),
            };
            if let Some(follow) = follow {
                app.on_event(follow);
            }
        }
    }
}

/// The active replay screen, panicking if the app is not in replay mode.
#[track_caller]
fn replay_screen(app: &App) -> ReplayScreen {
    match &app.mode {
        Mode::Replay(replay) => replay.screen,
        Mode::Live(_) => panic!("expected replay mode"),
    }
}

/// The replay bundle-load state, panicking if the app is not in replay mode.
#[track_caller]
fn replay_bundle(app: &App) -> &BundleLoad {
    match &app.mode {
        Mode::Replay(replay) => &replay.bundle,
        Mode::Live(_) => panic!("expected replay mode"),
    }
}

/// Whether replay playback is currently running, panicking if not in replay mode.
#[track_caller]
fn is_playing(app: &App) -> bool {
    match &app.mode {
        Mode::Replay(replay) => replay.is_playing(),
        Mode::Live(_) => panic!("expected replay mode"),
    }
}

// =============================================================================
// 1. The end-to-end replay path: fixture -> real reader -> real fold -> real
//    render, network-free and deterministic. Asserts the rendered content
//    (equity chart populated, the as-authored attribution values, a fixture fill,
//    and the status line) matches the conformance fixture's known data.
// =============================================================================

#[test]
fn test_replay_path_valid_fixture_end_to_end_renders_populated_screen() {
    // The four Parquet tables round-trip through the REAL reader against the shared
    // conformance fixture (ROADMAP v0.3): one row per step across all four tables.
    let decoded = load_fixture_bundle("valid");
    let n = usize::try_from(VALID_END_STEP).unwrap_or(usize::MAX) + 1;
    assert_eq!(
        decoded.fills.len(),
        n,
        "the reader decoded one fill per step"
    );
    assert_eq!(decoded.equity.len(), n, "one equity row per step");
    assert_eq!(decoded.positions.len(), n, "one position row per step");
    assert_eq!(decoded.greeks.len(), n, "one greeks row per step");

    // Fold it through the REAL App event path (the production composition minus the
    // spawn_blocking worker) and confirm it lands Ready.
    let (mut app, _rx) = replay_app_from_fixture("valid");
    assert!(
        matches!(replay_bundle(&app), BundleLoad::Ready(_)),
        "the valid bundle folds Loading -> Ready through on_event",
    );

    // Head at the last step so every panel is fully populated.
    app.on_event(AppEvent::ReplaySeek(SeekTo::Step(u32::MAX)));
    let text = render_replay_frame(&app);

    // The status line names the mode + the run (the bundle dir's final component).
    assert!(
        text.contains("replay"),
        "the status line names replay mode: {text:?}"
    );
    assert!(
        text.contains("valid"),
        "the status line names the run/dir: {text:?}"
    );

    // Every populated panel renders.
    for panel in ["equity", "drawdown", "P&L attribution", "fills"] {
        assert!(
            text.contains(panel),
            "the `{panel}` panel renders: {text:?}"
        );
    }
    // The equity chart is populated (not the deliberate empty state).
    assert!(
        !text.contains("no equity rows"),
        "the equity chart is populated at the head, not the empty state: {text:?}",
    );

    // The attribution panel renders the fixture's known, as-authored figures
    // (integer cents -> `$` at the render edge, never recomputed): theta 40c ->
    // $0.40, delta -120c -> −$1.20.
    assert!(
        text.contains("theta"),
        "a by-Greek attribution term is labelled: {text:?}"
    );
    assert!(
        text.contains("$0.40"),
        "theta 40c renders as $0.40: {text:?}"
    );
    assert!(
        text.contains("$1.20"),
        "delta 120c magnitude renders as $1.20: {text:?}"
    );
    assert!(
        text.contains('−'),
        "a negative term carries the − sign glyph: {text:?}"
    );

    // The trade drill-down shows a fixture fill from its own columns (LONG 1×
    // BTC C @ $125.00 — price 12500c).
    assert!(text.contains("LONG"), "a fill side renders: {text:?}");
    assert!(
        text.contains("BTC"),
        "the fill underlying renders: {text:?}"
    );
    assert!(
        text.contains("$125.00"),
        "the fill price 12500c renders as $125.00: {text:?}"
    );

    // Render-level two-screens evidence: the Payoff key is shown dimmed/parenthesized
    // (unavailable), never a live body — no dead key.
    assert!(
        text.contains("(Payoff)"),
        "the keybar shows the Payoff slot as unavailable (no dead key): {text:?}",
    );
}

// =============================================================================
// 2. Scrub consistency: seeking to several steps updates the equity curve, the
//    attribution breakdown, and the fills drill-down to the SAME post-fill head
//    each time (the single-`position` invariant, observed at the render seam).
// =============================================================================

#[test]
fn test_replay_scrub_updates_all_panels_to_one_coherent_head() {
    let (mut app, _rx) = replay_app_from_fixture("valid");
    for step in 0..=VALID_END_STEP {
        // Drive the scrub through the real ReplaySeek fold.
        app.on_event(AppEvent::ReplaySeek(SeekTo::Step(step)));
        let text = render_replay_frame(&app);

        // The framed title and the attribution header agree on the SAME head step —
        // both panels read one coherent as-of slice, never a stale index.
        assert!(
            text.contains(&format!("step {step}/{VALID_END_STEP}")),
            "the title shows head step {step}: {text:?}",
        );
        assert!(
            text.contains(&format!("P&L attribution @ step {step}")),
            "the attribution panel is @ head step {step}: {text:?}",
        );

        // The loaded cursor's as-of slices all reflect that same head: the conformance
        // fixture has one fill / equity / greeks row per step, so `step + 1` fills are
        // visible up to and including the head, and the head rows are the step-`step`
        // rows.
        match &app.mode {
            Mode::Replay(replay) => match replay.loaded() {
                Some(loaded) => {
                    assert_eq!(
                        loaded.cursor.position(),
                        step,
                        "the cursor sits at head {step}"
                    );
                    let visible = loaded.cursor.visible_fills(&loaded.bundle);
                    let expected = usize::try_from(step).unwrap_or(usize::MAX) + 1;
                    assert_eq!(
                        visible.len(),
                        expected,
                        "exactly {expected} fills are visible up to head {step}",
                    );
                    match loaded.cursor.head_greeks(&loaded.bundle) {
                        Some(g) => assert_eq!(g.step, step, "head greeks is the step-{step} row"),
                        None => panic!("the head greeks row must exist at step {step}"),
                    }
                    match loaded.cursor.head_equity(&loaded.bundle) {
                        Some(e) => assert_eq!(e.step, step, "head equity is the step-{step} row"),
                        None => panic!("the head equity row must exist at step {step}"),
                    }
                }
                None => panic!("the bundle must be Ready while scrubbing"),
            },
            Mode::Live(_) => panic!("expected replay mode"),
        }
    }
}

// =============================================================================
// 3. An unknown / unsupported schema is rejected with a clear, actionable error
//    state — NOT a partial render of the tables. The REAL reader rejects the #36
//    `bad_schema` fixture at open() with UnsupportedSchema; the app folds it to a
//    retryable Error the screen renders deliberately.
// =============================================================================

#[test]
fn test_replay_bad_schema_renders_actionable_error_not_partial() {
    let dir = fixture_dir("bad_schema");
    // The real reader rejects the major-incompatible schema at open().
    let err = match BundleReader::open(fixture_dir("bad_schema")) {
        Ok(_) => panic!("the bad_schema fixture must be rejected at open()"),
        Err(BundleError::UnsupportedSchema(tag)) => {
            assert_eq!(tag, "ironcondor.bundle.v2", "the offending tag is echoed");
            BundleError::UnsupportedSchema(tag)
        }
        Err(other) => {
            panic!("a major-incompatible schema must be UnsupportedSchema, got {other:?}")
        }
    };

    // Fold it exactly as the off-thread worker does — the non-secret message text.
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Replay(ReplayState::new(dir)), ThemeChoice::Auto, tx);
    app.on_event(AppEvent::BundleLoaded(BundleLoadResult::Failed(
        err.to_string(),
    )));
    assert!(
        matches!(replay_bundle(&app), BundleLoad::Error { .. }),
        "the rejected bundle folds to a retryable Error",
    );

    let text = render_replay_frame(&app);
    // The actionable error + retry affordance render.
    assert!(
        text.contains("unsupported schema"),
        "the schema error is shown: {text:?}"
    );
    assert!(
        text.contains("ironcondor.bundle.v2"),
        "the offending schema tag is shown: {text:?}",
    );
    assert!(
        text.contains("press R to retry"),
        "the retry affordance is shown: {text:?}"
    );
    // NOT a partial render: none of the populated panels leak through the error body.
    for panel in ["P&L attribution", "drawdown", "fills"] {
        assert!(
            !text.contains(panel),
            "a rejected bundle must not partial-render the `{panel}` panel: {text:?}",
        );
    }
}

// =============================================================================
// 4. Replay reaches EXACTLY its two documented screens — the Replay screen is
//    reachable; the Payoff screen stays unreachable until v0.5, with NO dead key.
//    Asserted at both the reachability seam and the render seam.
// =============================================================================

#[test]
fn test_replay_reaches_exactly_two_documented_screens_no_dead_payoff_key() {
    // The reachability gate: v0.3 replay reaches only the Replay screen; the payoff
    // panel is deferred to v0.5. This is a build/version gate, not a ProviderId or
    // capability match (replay has no live provider).
    assert!(
        is_replay_screen_reachable(ReplayScreen::Replay),
        "the equity/attribution/drill-down screen is reachable",
    );
    assert!(
        !is_replay_screen_reachable(ReplayScreen::Payoff),
        "the payoff screen is NOT reachable until v0.5",
    );

    let (mut app, _rx) = replay_app_from_fixture("valid");

    // The Payoff number-key slot (2) can NEVER switch to it — it is a consumed
    // global that flashes the "v0.5" hint and leaves the Replay screen in place.
    match app.dispatch_key_global(press('2')) {
        KeyRoute::Consumed => {}
        KeyRoute::ToScreen => panic!("a screen-switch number key is a consumed global"),
    }
    assert_eq!(
        replay_screen(&app),
        ReplayScreen::Replay,
        "the Payoff slot never switches to the unreachable screen",
    );

    // Tab / S-Tab cycle only reachable screens, so they can never land on Payoff.
    for _ in 0..4 {
        let _ = app.dispatch_key_global(tab());
        assert_eq!(
            replay_screen(&app),
            ReplayScreen::Replay,
            "Tab never cycles onto the unreachable Payoff screen",
        );
    }

    // The render seam: with no transient hint showing, the keybar lists the Replay
    // slot live and the Payoff slot dimmed/parenthesized (the no-dead-key visual),
    // and the rendered BODY is the reachable Replay screen — never a payoff body.
    let (fresh, _rx2) = replay_app_from_fixture("valid");
    let text = render_replay_frame(&fresh);
    assert!(
        text.contains("1 Replay"),
        "the Replay slot is a live number key: {text:?}"
    );
    assert!(
        text.contains("(Payoff)"),
        "the Payoff slot renders dimmed/parenthesized (no dead key): {text:?}",
    );
    assert!(
        text.contains("P&L attribution"),
        "the reachable Replay body renders: {text:?}",
    );
}

// =============================================================================
// 5. The load-error retry round-trip: a rejected bundle -> `R` -> Loading + a
//    ReloadBundle command -> a fresh valid load -> Ready (the operator's retry).
// =============================================================================

#[test]
fn test_replay_load_error_retry_round_trips_to_ready() {
    let dir = fixture_dir("bad_schema");
    let err = match BundleReader::open(fixture_dir("bad_schema")) {
        Ok(_) => panic!("the bad_schema fixture must reject at open()"),
        Err(e) => e,
    };
    let (tx, mut rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(
        Mode::Replay(ReplayState::new(dir.clone())),
        ThemeChoice::Auto,
        tx,
    );
    app.on_event(AppEvent::BundleLoaded(BundleLoadResult::Failed(
        err.to_string(),
    )));
    assert!(
        matches!(replay_bundle(&app), BundleLoad::Error { .. }),
        "the rejected bundle is a retryable Error",
    );

    // `R` (Rediscover) begins a reload: back to Loading, and a ReloadBundle command
    // to the data layer targeting the same dir.
    match app.dispatch_key_global(press('R')) {
        KeyRoute::Consumed => {}
        KeyRoute::ToScreen => panic!("R is a consumed global"),
    }
    assert!(
        matches!(replay_bundle(&app), BundleLoad::Loading),
        "R returns the screen to the Loading state",
    );
    match rx.try_recv() {
        Ok(Command::ReloadBundle(reload_dir)) => {
            assert_eq!(reload_dir, dir, "the reload targets the same bundle dir");
        }
        other => panic!("R must emit ReloadBundle for the same dir, got {other:?}"),
    }

    // A fresh valid load completes the retry -> Ready, and the populated screen renders.
    let loaded = load_fixture_bundle("valid");
    app.on_event(AppEvent::BundleLoaded(BundleLoadResult::Loaded(Box::new(
        loaded,
    ))));
    assert!(
        matches!(replay_bundle(&app), BundleLoad::Ready(_)),
        "a fresh valid load completes the retry to Ready",
    );
    let text = render_replay_frame(&app);
    assert!(
        text.contains("P&L attribution"),
        "the reloaded bundle renders the populated screen: {text:?}",
    );
}

// =============================================================================
// 6. Playback tick end-to-end: `Space` starts playback, ticks advance the head
//    by the speed quantum, and it auto-pauses at the tape end without wrapping.
// =============================================================================

#[test]
fn test_replay_playback_ticks_advance_head_and_auto_pause_at_end() {
    let (mut app, _rx) = replay_app_from_fixture("valid");
    // Space toggles playback on (Paused -> Playing at the default ×1 speed).
    app.on_event(AppEvent::ReplayControl(ReplayControl::PlayPause));
    assert!(is_playing(&app), "Space starts playback");

    // Ticks advance the play-head; the cursor clamps at end_step, so playback stops
    // at the tape end and auto-pauses (so the render loop parks, never spins/wraps).
    for _ in 0..16 {
        app.on_event(AppEvent::Tick);
    }
    assert!(!is_playing(&app), "playback auto-pauses at the tape end");
    match &app.mode {
        Mode::Replay(replay) => match replay.loaded() {
            Some(loaded) => assert_eq!(
                loaded.cursor.position(),
                VALID_END_STEP,
                "the play-head lands on the last step and never wraps",
            ),
            None => panic!("the bundle must be Ready during playback"),
        },
        Mode::Live(_) => panic!("expected replay mode"),
    }

    // The final frame renders the last head consistently.
    let text = render_replay_frame(&app);
    assert!(
        text.contains(&format!("step {VALID_END_STEP}/{VALID_END_STEP}")),
        "the play-head reached the tape end: {text:?}",
    );
}

// =============================================================================
// 7. The committed replay render goldens at the fixed 120×40 (docs/TESTING.md §4).
//    Every one is produced through the REAL render path (state -> ViewState::sync
//    -> render), never a hand-drawn buffer. Populated (three scrub/selection
//    variants of the single replay screen) + the loading / error / empty states.
// =============================================================================

#[test]
fn test_replay_equity_curve_golden() {
    // The populated screen with the head at the last step — the full equity curve
    // from the run start to the head, attribution + fills at the tape end.
    let (mut app, _rx) = replay_app_from_fixture("valid");
    app.on_event(AppEvent::ReplaySeek(SeekTo::Step(u32::MAX)));
    assert_golden("replay", "equity_curve.txt", &render_replay_frame(&app));
}

#[test]
fn test_replay_greek_attribution_golden() {
    // The same screen at a DIFFERENT scrub head (mid-run) — the attribution panel
    // and the equity curve follow the head, the render-level scrub-consistency proof.
    let (mut app, _rx) = replay_app_from_fixture("valid");
    app.on_event(AppEvent::ReplaySeek(SeekTo::Step(2)));
    assert_golden(
        "replay",
        "greek_attribution.txt",
        &render_replay_frame(&app),
    );
}

#[test]
fn test_replay_trade_drilldown_golden() {
    // The populated screen at the last step with a fill drilled into (`.` selects the
    // most recent visible fill), so the selected-fill detail panel renders.
    let (mut app, _rx) = replay_app_from_fixture("valid");
    app.on_event(AppEvent::ReplaySeek(SeekTo::Step(u32::MAX)));
    dispatch_key(&mut app, press('.'));
    // The selection landed on a fill (the detail panel is populated below the list).
    match &app.mode {
        Mode::Replay(replay) => match replay.loaded() {
            Some(loaded) => assert!(
                loaded.selection.is_some(),
                "`.` drilled into the most recent visible fill",
            ),
            None => panic!("the bundle must be Ready"),
        },
        Mode::Live(_) => panic!("expected replay mode"),
    }
    assert_golden("replay", "trade_drilldown.txt", &render_replay_frame(&app));
}

#[test]
fn test_replay_loading_golden() {
    // The bundle-load lifecycle's Loading state: the centered spinner + "loading
    // bundle …" body, with the status-bar spinner in lock-step (tick 0, so fixed).
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let app = App::new(
        Mode::Replay(ReplayState::new(fixture_dir("valid"))),
        ThemeChoice::Auto,
        tx,
    );
    assert!(
        matches!(replay_bundle(&app), BundleLoad::Loading),
        "a fresh replay state is Loading until the bundle arrives",
    );
    assert_golden("replay", "loading.txt", &render_replay_frame(&app));
}

#[test]
fn test_replay_error_golden() {
    // The bundle-load lifecycle's Error state, driven by the REAL reader's
    // UnsupportedSchema rejection of the bad_schema fixture — the actionable message
    // + the "press R to retry" affordance, never a partial render.
    let dir = fixture_dir("bad_schema");
    let err = match BundleReader::open(fixture_dir("bad_schema")) {
        Ok(_) => panic!("the bad_schema fixture must reject at open()"),
        Err(e) => e,
    };
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Replay(ReplayState::new(dir)), ThemeChoice::Auto, tx);
    app.on_event(AppEvent::BundleLoaded(BundleLoadResult::Failed(
        err.to_string(),
    )));
    assert_golden("replay", "error.txt", &render_replay_frame(&app));
}

#[test]
fn test_replay_empty_golden() {
    // A valid but zero-row (degenerate) run: each panel renders its deliberate empty
    // state (no equity rows / no fills / `—` attribution), never a blank or a panic.
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(
        Mode::Replay(ReplayState::new(PathBuf::from("empty-run"))),
        ThemeChoice::Auto,
        tx,
    );
    app.on_event(AppEvent::BundleLoaded(BundleLoadResult::Loaded(Box::new(
        loaded_bundle(0),
    ))));
    assert_golden("replay", "empty.txt", &render_replay_frame(&app));
}

// ---------------------------------------------------------------------------
// Section: the REAL supervised startup seam (#37 review) — the load arrives
// through `spawn_bundle_load` -> the event channel -> the fold, never a
// hand-injected `BundleLoaded`, so a regression in the real worker/channel
// path is caught here.
// ---------------------------------------------------------------------------

/// Drive the real off-thread load for `dir` and fold its posted event into a
/// fresh replay `App`, returning the app (and asserting an event ARRIVED through
/// the channel rather than being injected).
#[track_caller]
fn replay_app_via_real_load(dir: PathBuf) -> App {
    use crate::app::spawn_bundle_load;
    use crate::replay::ResourceCeilings;
    use tokio_util::sync::CancellationToken;

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => panic!("tokio runtime for the load worker failed: {e}"),
    };
    let (tx_events, mut rx_events) = mpsc::channel::<AppEvent>(16);
    let cancel = CancellationToken::new();
    let event = runtime.block_on(async {
        let handle = spawn_bundle_load(
            dir.clone(),
            ResourceCeilings::default(),
            tx_events,
            cancel.child_token(),
        );
        let event = rx_events.recv().await;
        let _ = handle.await;
        event
    });
    let Some(event) = event else {
        panic!("the real load worker posted no BundleLoaded event");
    };
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Replay(ReplayState::new(dir)), ThemeChoice::Auto, tx);
    app.on_event(event);
    app
}

#[test]
fn test_real_spawned_load_reaches_ready_through_the_event_channel() {
    let app = replay_app_via_real_load(fixture_dir("valid"));
    match &app.mode {
        Mode::Replay(replay) => match &replay.bundle {
            BundleLoad::Ready(loaded) => {
                assert_eq!(loaded.cursor.end_step(), VALID_END_STEP);
            }
            other => panic!("the real load must reach Ready, got {other:?}"),
        },
        Mode::Live(_) => panic!("a replay app"),
    }
    // The frame renders the real-loaded bundle, not a blank.
    let frame = render_replay_frame(&app);
    assert!(
        frame.contains("Replay"),
        "the Ready frame renders the replay screen"
    );
}

#[test]
fn test_real_spawned_load_surfaces_a_typed_error_through_the_event_channel() {
    let app = replay_app_via_real_load(fixture_dir("bad_schema"));
    match &app.mode {
        Mode::Replay(replay) => match &replay.bundle {
            BundleLoad::Error { .. } => {}
            other => panic!("a bad-schema bundle must surface Error, got {other:?}"),
        },
        Mode::Live(_) => panic!("a replay app"),
    }
    // The error frame offers the replay retry key, never a blank or a panic.
    let frame = render_replay_frame(&app);
    assert!(
        frame.contains('R'),
        "the Error frame offers the R retry affordance"
    );
}
