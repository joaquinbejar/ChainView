//! The synchronous render loop, the two-level key dispatch, and the async
//! tick/input task seams (`docs/02-tui-architecture.md` §7, §8, §9, §12).
//!
//! # Two loops, one render thread
//!
//! ChainView runs an **async tokio data layer** feeding a **synchronous** render
//! loop, joined by channels ([ADR-0005], §2). This module owns the synchronous
//! side. The render loop runs on a dedicated (blocking) thread and **parks** on an
//! [`mpsc::Receiver<AppEvent>`](tokio::sync::mpsc::Receiver) via
//! [`blocking_recv`](tokio::sync::mpsc::Receiver::blocking_recv) — it is
//! **event-driven, never busy-poll**: between ticks and inputs the render thread
//! sleeps, and it redraws **only when [`App::dirty`]** (§8). It never `.await`s and
//! never performs network I/O; the draw itself is [`render`], a pure function of
//! `&App` (§7).
//!
//! # The provider bridge is pumped between frames
//!
//! Provider [`Market`](AppEvent::Market) updates do **not** ride the plain
//! `AppEvent` channel — they ride the two-class bounded, coalescing
//! [`EventBridge`] (#10, §5). The render loop **pumps the bridge between frames**:
//! on every parked-then-woken `AppEvent` it folds that event, then drains the
//! bridge (coalesced quotes/Greeks/depth + the priority control channel) into the
//! app, then redraws if dirty. Because the tick task wakes the loop at least every
//! `tick_interval` (§8), the bridge is flushed at least that often — the documented
//! flush cadence (§5) — with zero busy-polling.
//!
//! # Two-level key dispatch
//!
//! A key is dispatched in two levels (§9): [`App::dispatch_key_global`] handles the
//! globals and the modal-help intercept and reports a [`KeyRoute`]; an unbound
//! ([`KeyRoute::ToScreen`]) key is forwarded to the **active** screen's
//! `handle_key`, whose returned [`AppEvent`] the loop folds back through
//! [`App::on_event`] — so a screen never performs I/O, it emits an event. The
//! forwarding match is total and wildcard-free (mode, then that mode's screens),
//! mirroring [`render`].
//!
//! # The tick/input tasks are supervisor-owned seams (§12)
//!
//! The composition (in the app builder's `run`, #12/#15) assembles the pieces and
//! hands their lifecycles to the single [`Supervisor`](crate::Supervisor):
//!
//! 1. Build the [`EventBridge`] (#10) and the `AppEvent` channel
//!    ([`event_channel`]).
//! 2. Spawn the tick task ([`spawn_tick_task`]) and the input reader
//!    ([`spawn_input_reader`]); register each with the supervisor as an
//!    **ancillary** [`SupervisedTask`](crate::SupervisedTask) under a
//!    [`child_token`](crate::Supervisor::child_token) it selects on.
//! 3. Spawn the render loop ([`run_render_loop`]) on
//!    [`spawn_blocking`](tokio::task::spawn_blocking) — a blocking thread, so its
//!    `blocking_recv` is legal — holding a clone of the supervisor's
//!    [`root_token`](crate::Supervisor::root_token) to cancel on `App::should_quit`;
//!    register it as the **render** task ([`set_render`](crate::Supervisor::set_render)).
//! 4. `supervisor.run()` supervises until the first shutdown trigger, then joins
//!    provider → ancillary → render and restores the terminal **last** (§12).
//!
//! On teardown the supervisor cancels each task's child token; the tick task
//! observes it at its `select!`, and the input reader observes it at its next poll
//! timeout (both return well inside the join budget). The render thread exits its
//! loop when the `AppEvent` channel's producers are dropped (`blocking_recv`
//! returns `None`) or on `App::should_quit`.
//!
//! [ADR-0005]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0005-async-data-sync-render-split.md

use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::render;
use crate::app::{App, EventBridge, KeyRoute, LiveScreen, Mode, ReplayScreen, Selection};
use crate::error::ChainViewError;
use crate::event::{AppEvent, Command};
use crate::ui::view::ViewState;
use crate::ui::{chain, depth, payoff, replay, surface};

/// Capacity of the bounded `AppEvent` channel the render loop parks on
/// (`docs/02-tui-architecture.md` §5). The channel carries only the
/// **low-frequency** input/tick events (`Key` / `Resize` / `Tick`) — the
/// high-frequency `Market` stream rides the coalescing [`EventBridge`] instead —
/// so a small bound is ample and a transient tick drop under a busy render is
/// harmless.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// How long the input reader blocks in one [`poll`](event::poll) before
/// re-checking its cancellation token, so a cancelled reader returns within this
/// window even when the terminal is idle (`docs/02-tui-architecture.md` §12).
const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(100);

/// Create the bounded `AppEvent` channel the render loop parks on and the input /
/// tick tasks feed (`docs/02-tui-architecture.md` §4, §5). The composition (#12)
/// clones the sender to each producer and hands the receiver to
/// [`run_render_loop`].
#[must_use]
pub fn event_channel() -> (mpsc::Sender<AppEvent>, mpsc::Receiver<AppEvent>) {
    mpsc::channel(EVENT_CHANNEL_CAPACITY)
}

// ---------------------------------------------------------------------------
// The synchronous, event-driven render loop.
// ---------------------------------------------------------------------------

/// Run the synchronous render loop until quit or channel close
/// (`docs/02-tui-architecture.md` §7, §8).
///
/// The loop draws the first frame (`App::dirty` starts `true`), then **parks** on
/// `rx_events` via [`blocking_recv`](mpsc::Receiver::blocking_recv) — event-driven,
/// no busy-poll. On each event it folds it (the two-level key dispatch), pumps the
/// provider [`EventBridge`] between frames, and redraws **only when
/// dirty**, clearing dirty after the draw. It breaks on
/// [`App::should_quit`](crate::App::should_quit) and returns when the channel
/// closes (all producers dropped).
///
/// Runs on a dedicated blocking thread (spawned by the supervisor via
/// [`spawn_blocking`](tokio::task::spawn_blocking)), so `blocking_recv` is legal —
/// it must **not** be called from an async context.
///
/// `route` receives every [`Command`] the fold produces (via the bridge) so the
/// data layer (#11/#16) can act on it; in this issue's scope it may be a no-op.
///
/// `view` is the ui view-cache (`src/ui/view.rs`): the loop [`sync`](ViewState::sync)s
/// it between the event fold and the draw so the payoff projection (#27) is computed
/// **off** the draw path, and the draw reads it as a borrow.
///
/// # Errors
///
/// [`ChainViewError::Terminal`] if the backend rejects a draw.
pub fn run_render_loop<B, R>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    bridge: &mut EventBridge,
    view: &mut ViewState,
    rx_events: &mut mpsc::Receiver<AppEvent>,
    mut route: R,
) -> Result<(), ChainViewError>
where
    B: Backend,
    R: FnMut(Command),
{
    // The first frame: `App::dirty` starts true, so the initial state paints
    // before the loop parks (§7). Sync the view cache off the draw path first.
    if app.dirty {
        view.sync(app);
        draw_frame(terminal, app, view)?;
    }
    // Event-driven: park on the async channel from this dedicated blocking thread.
    // `None` means every producer half was dropped (shutdown) — end the loop.
    while let Some(event) = rx_events.blocking_recv() {
        let outcome = step(terminal, app, bridge, view, event, &mut route)?;
        if outcome.quit {
            break;
        }
    }
    Ok(())
}

/// The outcome of one [`step`]: whether a frame was drawn (for the dirty-gated
/// redraw assertion) and whether the loop should stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepOutcome {
    /// Whether this step drew a frame (only when the fold left `App::dirty`).
    redrawn: bool,
    /// Whether the loop should stop (`App::should_quit`).
    quit: bool,
}

/// Process one event: fold it (two-level key dispatch), pump the bridge between
/// frames, sync the view cache off the draw path, and redraw **iff** dirty —
/// clearing dirty after the draw (`docs/02-tui-architecture.md` §7, §8).
fn step<B, R>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    bridge: &mut EventBridge,
    view: &mut ViewState,
    event: AppEvent,
    route: &mut R,
) -> Result<StepOutcome, ChainViewError>
where
    B: Backend,
    R: FnMut(Command),
{
    fold_event(app, event);
    // Pump the coalescing provider bridge between frames (drains the priority
    // control channel + the coalesced quotes/Greeks/depth, and routes commands).
    bridge.pump(app, &mut *route);
    let redrawn = if app.dirty {
        // Re-project any changed screen geometry (the payoff series, #27) off the
        // draw path — mirrors where `mark_drawn` runs — so the paint stays pure.
        view.sync(app);
        draw_frame(terminal, app, view)?;
        true
    } else {
        false
    };
    Ok(StepOutcome {
        redrawn,
        quit: app.should_quit,
    })
}

/// Draw one frame from `app` + the synced view cache and clear [`App::dirty`]
/// (`docs/02-tui-architecture.md` §7, §8).
///
/// The draw closure borrows `app` and `view` **immutably**, so [`render`] stays a
/// pure function of `&App` + `&ViewState`; [`mark_drawn`](crate::App::mark_drawn)
/// runs after the borrow ends. The view was already synced (off the draw path) by
/// the caller.
fn draw_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    view: &ViewState,
) -> Result<(), ChainViewError> {
    let view_app: &App = app;
    terminal
        .draw(|frame| render(view_app, view, frame))
        .map_err(|e| ChainViewError::Terminal(e.to_string()))?;
    app.mark_drawn();
    Ok(())
}

// ---------------------------------------------------------------------------
// The two-level key dispatch (§9), total and wildcard-free.
// ---------------------------------------------------------------------------

/// Fold one [`AppEvent`] into `app`. A key goes through the two-level dispatch
/// ([`dispatch_key`]); every other event is folded directly by
/// [`App::on_event`](crate::App::on_event). The match is exhaustive and
/// **wildcard-free** over the `AppEvent` closed set, so a new variant forces this
/// site to be revisited by the compiler.
fn fold_event(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::Key(key) => dispatch_key(app, key),
        AppEvent::Resize(width, height) => app.on_event(AppEvent::Resize(width, height)),
        AppEvent::Tick => app.on_event(AppEvent::Tick),
        AppEvent::Market(update) => app.on_event(AppEvent::Market(update)),
        AppEvent::ReplaySeek(seek) => app.on_event(AppEvent::ReplaySeek(seek)),
    }
}

/// The two-level key dispatch (`docs/02-tui-architecture.md` §9): the globals /
/// modal-help are handled by [`App::dispatch_key_global`], and an unbound
/// ([`KeyRoute::ToScreen`]) key is forwarded to the active screen's `handle_key`,
/// whose follow-on [`AppEvent`] is folded back. The [`KeyRoute`] match is
/// exhaustive and wildcard-free.
fn dispatch_key(app: &mut App, key: KeyEvent) {
    match app.dispatch_key_global(key) {
        KeyRoute::Consumed => {}
        KeyRoute::ToScreen => {
            // A screen-local change — the chain strike cursor / focused leg (#18) or a
            // payoff-builder edit (#26) — mutates `LiveState` directly and produces
            // **no** `AppEvent`, so it would not otherwise mark the app dirty. Detect
            // it by diffing a `Copy` live-view signature (the `Selection` plus the
            // builder's edit revision) across the forward and request a redraw when it
            // changed, so the edit actually paints while a truly-unbound key the screen
            // ignores leaves the frame clean (the idle-redraw property of
            // `docs/05-views-and-ux.md` §8 holds).
            let before = live_view_sig(app);
            let follow = screen_handle_key(app, key);
            if live_view_sig(app) != before {
                app.dirty = true;
            }
            if let Some(follow) = follow {
                app.on_event(follow);
            }
        }
    }
}

/// A `Copy` snapshot of the live screen's local mutable state that a screen key can
/// change without emitting an `AppEvent` — the chain [`Selection`] plus the
/// payoff-builder edit revision (`docs/02-tui-architecture.md` §8) — or `None` in
/// replay mode (which has no such cursor). The diff basis that turns a screen-local
/// change into a redraw request.
#[must_use]
fn live_view_sig(app: &App) -> Option<(Selection, u64)> {
    match &app.mode {
        Mode::Live(live) => Some((live.selection, live.payoff_builder.revision())),
        Mode::Replay(_) => None,
    }
}

/// Forward `key` to the **active** screen's `handle_key`, returning any follow-on
/// [`AppEvent`] (`docs/02-tui-architecture.md` §9). The dispatch is **total and
/// wildcard-free** — the mode first, then an exhaustive match over that mode's
/// screens — mirroring [`render`], so a new screen forces this site to be revisited
/// by the compiler.
#[must_use]
fn screen_handle_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match &mut app.mode {
        Mode::Live(state) => match state.screen {
            LiveScreen::Chain => chain::handle_key(state, key),
            LiveScreen::Depth => depth::handle_key(state, key),
            LiveScreen::Surface => surface::handle_key(state, key),
            LiveScreen::Payoff => payoff::handle_key(state, key),
        },
        Mode::Replay(state) => match state.screen {
            ReplayScreen::Replay => replay::handle_key(state, key),
            ReplayScreen::Payoff => payoff::handle_key_replay(state, key),
        },
    }
}

// ---------------------------------------------------------------------------
// The async tick + input tasks — supervisor-owned seams (§8, §9, §12).
// ---------------------------------------------------------------------------

/// Spawn the fixed-interval tick task, emitting [`AppEvent::Tick`] every
/// `tick_interval` until cancelled (`docs/02-tui-architecture.md` §8, §12).
///
/// The task `select!`s on its `cancel` [`CancellationToken`] (from the
/// supervisor's [`child_token`](crate::Supervisor::child_token)) and the interval,
/// so it observes cancellation at the next await point and returns well inside the
/// join budget. A tick send is **non-blocking**: a full channel (the render loop
/// is busy) drops the tick — harmless, since the next fires in `tick_interval` and
/// an idle tick sets no `dirty` — and a closed channel (the render loop is gone)
/// ends the task.
///
/// The returned [`JoinHandle`] is wrapped in a
/// [`TokioTask`](crate::TokioTask) and registered as an ancillary
/// [`SupervisedTask`](crate::SupervisedTask).
#[must_use = "register the returned JoinHandle with the Supervisor so it has a shutdown path"]
pub fn spawn_tick_task(
    tick_interval: Duration,
    tx_events: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tick_interval);
        // A slow render must not make ticks pile up: skip missed ticks rather than
        // firing a catch-up burst.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => match tx_events.try_send(AppEvent::Tick) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                },
            }
        }
    })
}

/// Spawn the dedicated terminal input reader, emitting [`AppEvent::Key`] /
/// [`AppEvent::Resize`] until cancelled (`docs/02-tui-architecture.md` §9, §12).
///
/// A **dedicated** reader on a blocking thread ([`spawn_blocking`](tokio::task::spawn_blocking))
/// means a slow render never drops a keystroke: the reader `blocking_send`s, so it
/// respects backpressure rather than discarding input. It polls with a short
/// `INPUT_POLL_TIMEOUT` so it re-checks its `cancel` token even on an idle
/// terminal, returning well inside the join budget; a closed channel (the render
/// loop is gone) or a read error ends the task.
///
/// The returned [`JoinHandle`] is wrapped in a
/// [`TokioTask`](crate::TokioTask) and registered as an ancillary
/// [`SupervisedTask`](crate::SupervisedTask).
#[must_use = "register the returned JoinHandle with the Supervisor so it has a shutdown path"]
pub fn spawn_input_reader(
    tx_events: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || read_input_loop(&tx_events, &cancel))
}

/// The blocking input-read loop: poll (bounded, so cancellation is observed),
/// read, normalize into an [`AppEvent`], and `blocking_send` it (never dropping a
/// keystroke).
fn read_input_loop(tx_events: &mpsc::Sender<AppEvent>, cancel: &CancellationToken) {
    while !cancel.is_cancelled() {
        match event::poll(INPUT_POLL_TIMEOUT) {
            Ok(true) => match event::read() {
                Ok(raw) => {
                    if let Some(app_event) = to_app_event(raw) {
                        // Blocking send from a dedicated blocking thread: respects
                        // backpressure without dropping input. A closed channel
                        // (render gone) ends the task.
                        if tx_events.blocking_send(app_event).is_err() {
                            break;
                        }
                    }
                }
                // The input source is gone — stop and let the supervisor tear down.
                Err(_) => break,
            },
            // Timed out with no event: re-check the cancel token and poll again.
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

/// Normalize a crossterm [`Event`] into an [`AppEvent`], or `None` for an event
/// outside ChainView's v1 keyboard-only input model
/// (`docs/05-views-and-ux.md` §2).
///
/// [`Event`] is crossterm's **open, `#[non_exhaustive]`** vocabulary — not a
/// ChainView closed set — so a catch-all is required and correct here; mouse /
/// focus / paste events are intentionally ignored (no mouse-only actions in v1).
#[must_use]
fn to_app_event(event: Event) -> Option<AppEvent> {
    match event {
        Event::Key(key) => Some(AppEvent::Key(key)),
        Event::Resize(cols, rows) => Some(AppEvent::Resize(cols, rows)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{
        StepOutcome, event_channel, fold_event, run_render_loop, spawn_tick_task, step,
        to_app_event,
    };
    use crate::app::tests_support::{live_app, replay_app};
    use crate::app::{EventBridge, LiveScreen, Mode, ReplayScreen, ScreenLoad};
    use crate::event::{AppEvent, Command, SeekTo};
    use crate::ui::view::ViewState;

    #[track_caller]
    fn test_terminal() -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(80, 24)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend terminal construction failed: {e}"),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn noop_route() -> impl FnMut(Command) {
        |_command: Command| {}
    }

    // --- dirty-gated redraw (a redraw happens ONLY when dirty; dirty clears) ---

    #[test]
    fn test_step_idle_tick_does_not_redraw() {
        // A `Ready` chain on a live feed is a non-motion (idle) state, so a tick sets
        // no dirty and the loop does not redraw (a motion state animates the spinner
        // instead, covered in `src/app.rs`).
        let (mut app, _rx) = live_app(LiveScreen::Chain, ScreenLoad::Ready, false);
        assert!(!app.dirty, "the app is clean after the initial frame");
        let (mut bridge, _senders) = EventBridge::new(64);
        let mut view = ViewState::new();
        let mut terminal = test_terminal();
        let mut route = noop_route();
        let outcome = match step(
            &mut terminal,
            &mut app,
            &mut bridge,
            &mut view,
            AppEvent::Tick,
            &mut route,
        ) {
            Ok(o) => o,
            Err(e) => panic!("step failed: {e}"),
        };
        assert_eq!(
            outcome,
            StepOutcome {
                redrawn: false,
                quit: false
            },
            "an idle non-motion tick sets no dirty, so no redraw and no quit"
        );
        assert!(!app.dirty);
    }

    #[test]
    fn test_step_resize_redraws_and_clears_dirty() {
        let (mut app, _rx) = live_app(LiveScreen::Chain, ScreenLoad::Loading, false);
        let (mut bridge, _senders) = EventBridge::new(64);
        let mut view = ViewState::new();
        let mut terminal = test_terminal();
        let mut route = noop_route();
        let outcome = match step(
            &mut terminal,
            &mut app,
            &mut bridge,
            &mut view,
            AppEvent::Resize(100, 30),
            &mut route,
        ) {
            Ok(o) => o,
            Err(e) => panic!("step failed: {e}"),
        };
        assert!(outcome.redrawn, "a resize sets dirty, so the loop redraws");
        assert!(!app.dirty, "dirty is cleared after the draw");
        assert!(!outcome.quit);
    }

    #[test]
    fn test_step_quit_key_redraws_final_frame_and_signals_stop() {
        let (mut app, _rx) = live_app(LiveScreen::Chain, ScreenLoad::Loading, false);
        let (mut bridge, _senders) = EventBridge::new(64);
        let mut view = ViewState::new();
        let mut terminal = test_terminal();
        let mut route = noop_route();
        let outcome = match step(
            &mut terminal,
            &mut app,
            &mut bridge,
            &mut view,
            AppEvent::Key(key(KeyCode::Char('q'))),
            &mut route,
        ) {
            Ok(o) => o,
            Err(e) => panic!("step failed: {e}"),
        };
        assert!(app.should_quit);
        assert!(outcome.quit, "the quit key signals the loop to stop");
        assert!(outcome.redrawn, "quit set dirty, so a final frame is drawn");
    }

    // --- the parked loop: drains until close, breaks on quit -------------------

    #[test]
    fn test_run_render_loop_drains_until_channel_closes() {
        // A plain (non-async) test: `blocking_recv` parks the loop on the channel.
        let (mut app, _cmd_rx) = live_app(LiveScreen::Chain, ScreenLoad::Loading, false);
        let (mut bridge, _senders) = EventBridge::new(64);
        let mut terminal = test_terminal();
        let (tx, mut rx) = event_channel();
        // Preload two events, then drop the sender so `blocking_recv` yields `None`
        // and the loop ends without hanging.
        let _ = tx.try_send(AppEvent::Resize(100, 30));
        let _ = tx.try_send(AppEvent::Tick);
        drop(tx);
        let mut view = ViewState::new();
        match run_render_loop(
            &mut terminal,
            &mut app,
            &mut bridge,
            &mut view,
            &mut rx,
            noop_route(),
        ) {
            Ok(()) => {}
            Err(e) => panic!("render loop failed: {e}"),
        }
        assert!(!app.should_quit, "no quit event was sent");
        assert!(!app.dirty, "the last processed frame cleared dirty");
    }

    #[test]
    fn test_run_render_loop_breaks_on_quit_even_with_open_sender() {
        let (mut app, _cmd_rx) = live_app(LiveScreen::Chain, ScreenLoad::Loading, false);
        let (mut bridge, _senders) = EventBridge::new(64);
        let mut terminal = test_terminal();
        let (tx, mut rx) = event_channel();
        let _ = tx.try_send(AppEvent::Key(key(KeyCode::Char('q'))));
        // Keep the sender OPEN: the loop must break on quit, not wait for close.
        let _keep_open = tx.clone();
        drop(tx);
        let mut view = ViewState::new();
        match run_render_loop(
            &mut terminal,
            &mut app,
            &mut bridge,
            &mut view,
            &mut rx,
            noop_route(),
        ) {
            Ok(()) => {}
            Err(e) => panic!("render loop failed: {e}"),
        }
        assert!(app.should_quit, "the loop broke on the quit key");
    }

    // --- two-level key dispatch ------------------------------------------------

    #[test]
    fn test_fold_event_global_quit_key_is_consumed_no_command() {
        let (mut app, mut rx) = live_app(LiveScreen::Chain, ScreenLoad::Loading, false);
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Char('q'))));
        assert!(app.should_quit);
        assert!(rx.try_recv().is_err(), "a global quit emits no command");
    }

    #[test]
    fn test_fold_event_unbound_key_forwarded_to_replay_screen_emits_seek() {
        // An unbound key (`Left`) is forwarded to the active replay screen, which
        // returns `ReplaySeek(StepBy(-1))`; `App::on_event` folds it into a
        // `SeekBundle` command — the full two-level dispatch end to end.
        let (mut app, mut rx) = replay_app(ReplayScreen::Replay, false);
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Left)));
        match rx.try_recv() {
            Ok(Command::SeekBundle(SeekTo::StepBy(-1))) => {}
            other => panic!("expected SeekBundle(StepBy(-1)), got {other:?}"),
        }
    }

    #[test]
    fn test_fold_event_modal_help_swallows_forwarded_key() {
        // With help open, the modal intercept swallows the scrub key — it never
        // reaches the screen, so no command is emitted.
        let (mut app, mut rx) = replay_app(ReplayScreen::Replay, true);
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Left)));
        assert!(app.help_open, "help stays open");
        assert!(rx.try_recv().is_err(), "the modal overlay swallows the key");
    }

    #[test]
    fn test_fold_event_chain_move_strike_selects_and_marks_dirty() {
        // A chain strike-nav key (`j`) is forwarded to the chain screen (#18),
        // which places the strike cursor — a screen-local change that emits no
        // command, so the loop detects the `Selection` change and requests a
        // redraw.
        let (mut app, mut rx) = live_app(LiveScreen::Chain, ScreenLoad::Ready, false);
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Char('j'))));
        assert!(!app.should_quit);
        assert!(app.dirty, "moving the strike cursor requests a redraw");
        assert!(rx.try_recv().is_err(), "local nav emits no command");
        match &app.mode {
            Mode::Live(live) => assert!(
                live.selection.focused_row.is_some(),
                "the strike cursor is placed",
            ),
            Mode::Replay(_) => panic!("expected a live app"),
        }
    }

    #[test]
    fn test_fold_event_payoff_add_leg_marks_dirty() {
        // A payoff builder key (`a`) is forwarded to the payoff screen (#26), which
        // appends a leg — a screen-local edit that emits no command, so the loop
        // detects the builder's revision change and requests a redraw.
        let (mut app, mut rx) = live_app(LiveScreen::Payoff, ScreenLoad::Ready, false);
        assert!(!app.dirty, "the app is clean after its initial frame");
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Char('a'))));
        assert!(app.dirty, "appending a builder leg requests a redraw");
        assert!(rx.try_recv().is_err(), "a builder edit emits no command");
        match &app.mode {
            Mode::Live(live) => assert_eq!(
                live.payoff_builder.legs().len(),
                1,
                "the focused leg is appended",
            ),
            Mode::Replay(_) => panic!("expected a live app"),
        }
    }

    #[test]
    fn test_fold_event_payoff_unbound_key_makes_no_change_no_dirty() {
        // A key bound to nothing on the payoff screen (`z`) changes no builder state,
        // so the revision is unchanged and no redundant redraw fires (§8).
        let (mut app, _rx) = live_app(LiveScreen::Payoff, ScreenLoad::Ready, false);
        assert!(!app.dirty, "the app is clean after its initial frame");
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Char('z'))));
        assert!(
            !app.dirty,
            "an unbound key changes nothing and requests no redraw"
        );
    }

    #[test]
    fn test_fold_event_chain_unbound_key_makes_no_change_no_dirty() {
        // A key bound to nothing (`z`) is forwarded to the chain screen, which
        // ignores it — no `Selection` change, so no redundant redraw (§8).
        let (mut app, _rx) = live_app(LiveScreen::Chain, ScreenLoad::Ready, false);
        assert!(!app.dirty, "the app is clean after its initial frame");
        fold_event(&mut app, AppEvent::Key(key(KeyCode::Char('z'))));
        assert!(
            !app.dirty,
            "an unbound key changes nothing and requests no redraw"
        );
        assert!(!app.should_quit);
    }

    // --- crossterm Event normalization -----------------------------------------

    #[test]
    fn test_to_app_event_maps_key_and_resize_ignores_others() {
        let mapped_key = to_app_event(Event::Key(key(KeyCode::Char('j'))));
        assert!(matches!(mapped_key, Some(AppEvent::Key(_))));
        assert!(matches!(
            to_app_event(Event::Resize(120, 40)),
            Some(AppEvent::Resize(120, 40))
        ));
        assert!(to_app_event(Event::FocusGained).is_none());
        assert!(to_app_event(Event::FocusLost).is_none());
        assert!(to_app_event(Event::Paste("x".to_owned())).is_none());
    }

    // --- the tick task seam (paused virtual clock, zero real wait) --------------

    #[tokio::test(start_paused = true)]
    async fn test_spawn_tick_task_emits_ticks_and_stops_on_cancel() {
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let cancel = CancellationToken::new();
        let handle = spawn_tick_task(Duration::from_millis(250), tx, cancel.clone());
        // `interval` fires its first tick immediately, so a tick arrives with no
        // clock advance.
        assert!(matches!(rx.recv().await, Some(AppEvent::Tick)));
        cancel.cancel();
        match handle.await {
            Ok(()) => {}
            Err(e) => panic!("tick task join failed: {e}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_spawn_tick_task_stops_when_event_channel_closes() {
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let cancel = CancellationToken::new();
        let handle = spawn_tick_task(Duration::from_millis(250), tx, cancel.clone());
        assert!(matches!(rx.recv().await, Some(AppEvent::Tick)));
        // Drop the consumer: the next tick's `try_send` sees a closed channel and
        // the task stops on its own (no cancel needed).
        drop(rx);
        tokio::time::advance(Duration::from_millis(300)).await;
        match handle.await {
            Ok(()) => {}
            Err(e) => panic!("tick task join failed: {e}"),
        }
        let _ = cancel;
    }
}
