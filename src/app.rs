//! The application state machine and the synchronous event fan-in
//! (`docs/02-tui-architecture.md` §3, §4).
//!
//! [`App`] owns every piece of state the render loop reads and is a
//! `Live | Replay` [`Mode`] state machine — the two modes never run concurrently
//! in one process (§3). Screen identity is **mode-scoped** ([`LiveScreen`] /
//! [`ReplayScreen`]) so an out-of-mode pair (`Replay` + `Chain`) is
//! **unrepresentable** by the type system, not prevented by a runtime fallback,
//! and the render dispatch (#13) stays a total, wildcard-free match (§7).
//!
//! # The fan-in is synchronous and does no I/O
//!
//! [`App::on_event`] folds each [`AppEvent`] into state in **one exhaustive
//! match with no wildcard arm** and sets [`dirty`](App::dirty) on any mutation.
//! Handlers are pure and fast: one that needs I/O (reconnect, re-discover, seek
//! the bundle, subscribe a new expiry) emits a [`Command`] on the app's bounded
//! command channel via [`try_send`](tokio::sync::mpsc::Sender::try_send) — it
//! never performs the I/O inline and never `.await`s. `on_event` is **not**
//! `async` ([ADR-0005](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0005-async-data-sync-render-split.md)).
//!
//! # Capability-driven reachability, never a `ProviderId` match
//!
//! A live screen is only ever set to a **reachable** value, and reachability is
//! read from the active provider's declared [`ProviderCapabilities`] — there is
//! **no `match` on [`ProviderId`]** anywhere here, so a built-in and an
//! externally registered provider are gated by identical code
//! (`docs/03-data-providers.md` §11.4,
//! [ADR-0006](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md)).
//! The `Tab`/`S-Tab` skip and the number-key status hint that build on the
//! [`is_screen_reachable`] helper land with the keybinding map and render loop
//! (#13/#14).
//!
//! # Scope of this issue (#9)
//!
//! This lands the state + fan-in skeleton. The render loop and draw dispatch
//! (#13), the keybinding map and help overlay (#14), the bounded coalescing
//! channel that carries `Market` (#10), the task supervisor (#11), and the
//! registry / app builder (#12) are separate issues. The [`ReplayState`]
//! internals (v0.3) and the [`PayoffBuilder`] internals (v0.2) are documented
//! stubs whose enum/struct **shapes** are fixed here so later work fills them in
//! without a breaking change.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::model::Position;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::visualization::GraphData;
use optionstratlib::{ExpirationDate, OptionStyle};
use tokio::sync::mpsc;

use crate::app::keymap::{GlobalCommand, KeyChord, resolve_global};
use crate::chain::quote_is_stale;
use crate::chain::{
    ChainFetch, ChainSnapshot, ChainStore, ExpirySource, InstrumentKey, MarketUpdate, MergeOutcome,
    ProviderId, QuoteClocks, StreamHealth,
};
use crate::config::ThemeChoice;
use crate::event::{AppEvent, Command, SeekTo};
use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};

mod bridge;
pub(crate) mod keymap;
mod payoff_build;
mod registry;
mod supervisor;

pub use bridge::{BridgeSenders, COMMAND_CHANNEL_CAPACITY, CONTROL_CHANNEL_CAPACITY, EventBridge};
// `ProviderRegistry` stays crate-internal to `registry` (the UI never receives
// it, and external code composes through the builder), so only the builder entry
// points, the drivable resolution, and the supervised-subscription composition
// seam are re-exported here.
pub use registry::{
    ChainViewApp, ChainViewAppBuilder, ProviderSubscription, Resolved,
    spawn_supervised_subscription,
};
pub use supervisor::{
    DEFAULT_JOIN_BUDGET, ExitCause, ExitReporter, FinalTeardown, GuardTeardown, SupervisedTask,
    Supervisor, TaskExit, TokioTask,
};

// ---------------------------------------------------------------------------
// App: the top-level state the render loop reads (§3).
// ---------------------------------------------------------------------------

/// How many [`AppEvent::Tick`]s a transient keybar hint stays visible before it
/// auto-decays (`docs/05-views-and-ux.md` §2). At the default 250 ms tick this is
/// ~2 s — a readable minimum so a hint never flashes for near-zero time; a key
/// press still clears it sooner.
pub const HINT_TICKS: u8 = 8;

/// All state the render loop reads, as a `Live | Replay` [`Mode`] state machine
/// (`docs/02-tui-architecture.md` §3).
///
/// Owned single-threaded by the synchronous render loop; every mutation happens
/// on an [`AppEvent`] through [`on_event`](App::on_event), never in `draw`.
#[derive(Debug)]
pub struct App {
    /// The active mode, which owns the mode-scoped active screen.
    pub mode: Mode,
    /// The color-theme selection. The **resolved** palette (variant + `NO_COLOR`
    /// fallback) is computed by the theme layer
    /// ([`Theme::resolve`](crate::ui::theme::Theme::resolve)) each frame from this
    /// selection and [`no_color`](App::no_color).
    pub theme: ThemeChoice,
    /// Whether color output is disabled (`NO_COLOR` / `--no-color` / config). Read
    /// by the theme layer so every color-encoded state falls back to markers +
    /// intensity only (`docs/05-views-and-ux.md` §7). Set by startup from
    /// [`Config::no_color`](crate::Config); defaults `false`.
    pub no_color: bool,
    /// The status-bar model (provider health, clock, mode). Its fields are
    /// populated by the render loop / status line (#13/#14); a documented stub
    /// here so [`App`] is constructible now.
    pub status: StatusLine,
    /// A monotonic tick counter, advanced on every [`AppEvent::Tick`], that drives
    /// the status-bar spinner/clock animation (`docs/05-views-and-ux.md` §7). Read
    /// purely in `draw` so the animation never reads a wall clock there; an idle
    /// tick advances it but does **not** set [`dirty`](App::dirty) (§8).
    pub tick_count: u64,
    /// The wall-clock instant of the most recent tick, stamped off-draw by the
    /// tick handler from the `std` clock (chrono's `clock` feature is off). It is
    /// the **decay reference** the chain matrix reads at draw time, so
    /// the bid-up/ask-down markers decay on wall-time rather than being pinned to
    /// the last poll (`docs/01-domain-model.md` §6, `docs/02-tui-architecture.md`
    /// §7). Advanced every tick regardless of [`dirty`](App::dirty); the next
    /// redraw reads the fresh instant, so `draw` itself never touches a wall clock.
    pub now: DateTime<Utc>,
    /// A transient one-line keybar hint (e.g. "Depth not available on deribit"),
    /// flashed when an unavailable number key is pressed (`docs/05-views-and-ux.md`
    /// §2). It decays either on the next key or after `HINT_TICKS` ticks
    /// ([`hint_ttl`](App::hint_ttl)), so it is visible for a readable minimum
    /// duration. `None` when nothing is flashed.
    pub status_hint: Option<String>,
    /// Ticks remaining before [`status_hint`](App::status_hint) auto-decays. `0`
    /// when no hint is showing; set to `HINT_TICKS` when a hint is flashed and
    /// counted down each [`AppEvent::Tick`].
    pub hint_ttl: u8,
    /// Whether the modal help overlay is open (§9).
    pub help_open: bool,
    /// Whether the loop should exit after this event.
    pub should_quit: bool,
    /// Set by any handler that mutates state; gates the redraw and is cleared
    /// after a draw by [`mark_drawn`](App::mark_drawn). An idle app with no data
    /// and no input leaves it clear and does not redraw (§3, §8).
    pub dirty: bool,
    /// The render-loop -> data-layer command channel. A handler that needs I/O
    /// sends a [`Command`] here; it never performs the I/O inline. Private so the
    /// only way to enqueue work is a typed [`Command`].
    tx_command: mpsc::Sender<Command>,
    /// How many control commands (reconnect / rediscover / reload / seek) were
    /// dropped because the command channel was full or closed. A recovery
    /// keypress must never be a **silent** no-op: instead of swallowing the send
    /// failure, `send_command` records it here so the status line can surface it.
    /// Monotonic, stopping at the ceiling (a checked increment, never wrapping);
    /// `0` is the healthy steady state (commands are user-driven, so the bounded
    /// channel is never expected to fill — a non-zero value means the data layer
    /// is gone or wedged).
    pub commands_dropped: u64,
}

impl App {
    /// Assemble an app in the given mode. The command `Sender` is the render ->
    /// data half the supervisor (#11) / builder (#12) wire to the data layer.
    ///
    /// [`dirty`](App::dirty) starts `true` so the first frame draws.
    #[must_use]
    pub fn new(mode: Mode, theme: ThemeChoice, tx_command: mpsc::Sender<Command>) -> Self {
        Self {
            mode,
            theme,
            no_color: false,
            status: StatusLine::default(),
            tick_count: 0,
            now: now_utc(),
            status_hint: None,
            hint_ttl: 0,
            help_open: false,
            should_quit: false,
            dirty: true,
            tx_command,
            commands_dropped: 0,
        }
    }

    /// Set whether color output is disabled, builder-style, at startup
    /// (`docs/05-views-and-ux.md` §7). Wired from
    /// [`Config::no_color`](crate::Config) by the app builder / `main`.
    #[must_use]
    pub fn with_no_color(mut self, no_color: bool) -> Self {
        self.no_color = no_color;
        self
    }

    /// Fold one event into the app in a **single exhaustive match with no
    /// wildcard arm** (`docs/02-tui-architecture.md` §4). Every mutating handler
    /// sets [`dirty`](App::dirty); a handler that needs I/O emits a [`Command`]
    /// rather than blocking or awaiting. Not `async` and performs no I/O.
    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.on_key(key),
            AppEvent::Resize(width, height) => self.on_resize(width, height),
            AppEvent::Tick => self.on_tick(),
            AppEvent::Market(update) => self.on_market(update),
            AppEvent::ReplaySeek(seek) => self.on_replay_seek(seek),
        }
    }

    /// Clear [`dirty`](App::dirty) after the render loop has drawn a frame (§3).
    pub fn mark_drawn(&mut self) {
        self.dirty = false;
    }

    // --- Event handlers (each sets `dirty` only when it mutates state) --------

    fn on_key(&mut self, key: KeyEvent) {
        // Fold a key through the GLOBAL level only. The render-loop dispatch (#13,
        // `src/ui`) forwards a [`KeyRoute::ToScreen`] key on to the active screen's
        // `handle_key`; a non-loop caller (this fan-in entry, and #9's tests) folds
        // only the globals and discards the route, so an unbound key is a no-op —
        // exactly the prior behavior.
        let _ = self.dispatch_key_global(key);
    }

    /// The **global** half of the two-level key dispatch
    /// (`docs/02-tui-architecture.md` §9): handle the quit / help / reconnect /
    /// rediscover globals and the modal-help intercept, and report whether the key
    /// was [`Consumed`](KeyRoute::Consumed) or should be forwarded
    /// [`ToScreen`](KeyRoute::ToScreen). The render loop (#13) forwards a
    /// `ToScreen` key to the active screen's `handle_key`; a `Consumed` key stops
    /// here.
    ///
    /// **Modal help precedence (`docs/05-views-and-ux.md` §3).** While the help
    /// overlay is open it is modal: it intercepts **every** key, honoring only `?`
    /// and `Esc` (both close it) and swallowing the rest — no background screen
    /// action fires behind the overlay, so a keystroke can never mutate hidden
    /// state.
    ///
    /// The globals resolve **through the single keybinding map**
    /// ([`resolve_global`], `src/ui/theme.rs`), so the running dispatch and the
    /// help overlay read one table and cannot drift (§3). `Ctrl-C` is the terminal
    /// interrupt and hard-quits even behind the overlay; every other global (quit,
    /// help, screen switch `1`–`4`, `Tab`/`S-Tab` cycle, reconnect, reload) is a
    /// map [`GlobalCommand`]. An unmapped key is forwarded to the active screen (the
    /// two-level dispatch).
    #[must_use = "the route decides whether the active screen also handles the key"]
    pub(crate) fn dispatch_key_global(&mut self, key: KeyEvent) -> KeyRoute {
        // On some platforms crossterm reports `Release`/`Repeat` in addition to
        // `Press`; act on the press only so a key never fires twice, and never
        // forward a non-press to a screen.
        if key.kind != KeyEventKind::Press {
            return KeyRoute::Consumed;
        }
        // `Ctrl-C` is a hard quit regardless of the character key mapping or the
        // overlay state (the terminal-interrupt convention): it is the ONE key that
        // acts behind the modal help overlay. The map lists `q`/`Ctrl-C` as one
        // Quit action, but `q` is swallowed while the overlay is modal and `Ctrl-C`
        // is not — an intentional carve-out for the terminal interrupt.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return KeyRoute::Consumed;
        }
        // Modal help: intercept EVERY key — before the keymap-vocabulary check — so
        // an out-of-vocabulary key (F-keys, PageUp/Down, Insert/Delete, media) can
        // never reach the hidden screen behind the overlay. Only `?`/`Esc` act (both
        // close it); everything else is swallowed.
        if self.help_open {
            if matches!(
                KeyChord::from_event(key),
                Some(KeyChord::Char('?') | KeyChord::Esc)
            ) {
                self.toggle_help();
            }
            return KeyRoute::Consumed;
        }
        let Some(chord) = KeyChord::from_event(key) else {
            // Outside the keymap vocabulary — forward to the active screen.
            return KeyRoute::ToScreen;
        };
        // Any live key decays a stale transient hint (`docs/05-views-and-ux.md`
        // §2); a command below may set a fresh one.
        self.clear_status_hint();
        // Resolve the global command from the single map. An unmapped key routes to
        // the active screen.
        match resolve_global(chord) {
            Some(command) => {
                self.apply_global_command(command);
                KeyRoute::Consumed
            }
            None => KeyRoute::ToScreen,
        }
    }

    /// Execute a resolved global command from the keymap
    /// (`docs/05-views-and-ux.md` §3). Matched exhaustively over the
    /// [`GlobalCommand`] closed set with no wildcard arm.
    fn apply_global_command(&mut self, command: GlobalCommand) {
        match command {
            GlobalCommand::Quit => self.request_quit(),
            GlobalCommand::ToggleHelp => self.toggle_help(),
            GlobalCommand::Reconnect => self.request_reconnect(),
            GlobalCommand::Rediscover => self.request_rediscover(),
            GlobalCommand::SwitchScreen(slot) => self.request_switch_screen(slot),
            GlobalCommand::NextScreen => self.cycle_screen(true),
            GlobalCommand::PrevScreen => self.cycle_screen(false),
        }
    }

    fn on_resize(&mut self, _width: u16, _height: u16) {
        // A resize forces a redraw; the new geometry is read from `frame.area()`
        // in the render loop (#13), so nothing is stored here.
        self.dirty = true;
    }

    fn on_tick(&mut self) {
        // Advance the wall-clock reference the chain matrix reads for tick-direction
        // decay, so a bid-up/ask-down marker decays on wall-time instead of persisting
        // until the next poll (`docs/01-domain-model.md` §6). Stamped off-draw here;
        // `draw` reads the cached instant and never touches a wall clock (§7). This
        // sets no `dirty` on its own — under streaming quotes each `Market` update
        // already redraws and reads this fresh instant.
        self.now = now_utc();
        // Advance the spinner/clock counter the status bar reads purely in `draw`
        // (`docs/05-views-and-ux.md` §7). `checked_add` avoids the banned
        // `wrapping_add`; the counter resets on the (practically unreachable) u64
        // overflow, harmless for a spinner index.
        self.tick_count = self.tick_count.checked_add(1).unwrap_or(0);
        let mut needs_redraw = false;
        // Decay a transient keybar hint after its readable minimum lifetime (§2).
        // Counting down does not redraw; only the final clear does, so an idle app
        // does not spin on the intermediate ticks.
        if self.tick_hint() {
            needs_redraw = true;
        }
        // Advance the loading/reconnecting/playback spinner: redraw ONLY in a motion
        // state, so the spinner animates during the initial connect / reconnect /
        // playback (§7) while a truly idle, non-motion app still parks and never
        // redraws on a tick (§8).
        if self.is_in_motion() {
            needs_redraw = true;
        }
        if needs_redraw {
            self.dirty = true;
        }
    }

    /// Count down a live transient hint, clearing it when its lifetime expires.
    /// Returns whether a redraw is needed (only when the hint is actually cleared,
    /// so the intermediate countdown ticks set no `dirty`).
    fn tick_hint(&mut self) -> bool {
        if self.status_hint.is_none() {
            return false;
        }
        // `saturating_*`/`wrapping_*` are banned, so decrement with a checked step
        // and treat "reached zero" as "clear the hint".
        match self.hint_ttl.checked_sub(1) {
            Some(remaining) if remaining > 0 => {
                self.hint_ttl = remaining;
                false
            }
            _ => {
                self.status_hint = None;
                self.hint_ttl = 0;
                true
            }
        }
    }

    /// Whether the app is in a **motion** state whose spinner/play-head advances on
    /// each tick (`docs/05-views-and-ux.md` §7): a live view still loading or
    /// reconnecting, or a replay bundle still loading or playing. A non-motion app
    /// (a ready chain on a live feed, a paused replay) parks and does not redraw on
    /// an idle tick (§8). Mirrors the theme's status-bar motion predicate.
    #[must_use]
    fn is_in_motion(&self) -> bool {
        match &self.mode {
            Mode::Live(live) => {
                matches!(live.load, ScreenLoad::Loading)
                    || matches!(live.source.health, StreamHealth::Reconnecting { .. })
            }
            Mode::Replay(replay) => {
                matches!(replay.bundle, BundleLoad::Loading)
                    || matches!(replay.play, Playback::Playing { .. })
            }
        }
    }

    fn on_market(&mut self, update: MarketUpdate) {
        let changed = match &mut self.mode {
            Mode::Live(live) => live.apply_market(update),
            // Live market updates are meaningless in replay mode (there is no
            // live store); replay data arrives via `ReplaySeek`/`SeekBundle`.
            Mode::Replay(_) => false,
        };
        if changed {
            self.dirty = true;
        }
    }

    fn on_replay_seek(&mut self, seek: SeekTo) {
        match &self.mode {
            // A scrub is meaningless in live mode; ignored.
            Mode::Live(_) => {}
            // Seeking re-decodes table indices — I/O — so emit a `Command`; the
            // play-head advances when the seek worker (v0.3) responds.
            Mode::Replay(_) => self.send_command(Command::SeekBundle(seek)),
        }
    }

    // --- Global key actions ---------------------------------------------------

    fn request_quit(&mut self) {
        self.should_quit = true;
        self.dirty = true;
    }

    fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
        self.dirty = true;
    }

    fn request_reconnect(&mut self) {
        match &self.mode {
            Mode::Live(_) => self.send_command(Command::Reconnect),
            // `r` has no replay binding — `R` reloads the bundle instead (§6).
            Mode::Replay(_) => {}
        }
    }

    fn request_rediscover(&mut self) {
        match &self.mode {
            Mode::Live(_) => self.send_command(Command::Rediscover),
            Mode::Replay(replay) => {
                let command = Command::ReloadBundle(replay.dir.clone());
                self.send_command(command);
            }
        }
    }

    /// Enqueue a command for the data layer, non-blocking. Never blocks or awaits
    /// the render loop. A full or closed command channel does **not** silently
    /// swallow a recovery keypress: the failure is counted in
    /// [`commands_dropped`](App::commands_dropped) so the status line can surface
    /// it. A command storm is impossible (commands are user-driven, `docs/02`
    /// §5), so a full channel is not expected; a closed channel means the data
    /// layer is gone, which the surfaced count now makes visible rather than
    /// hidden.
    fn send_command(&mut self, command: Command) {
        if self.tx_command.try_send(command).is_err() {
            // Checked, not `saturating_add` (a banned method): increment only
            // when it does not overflow, so the counter stops at the ceiling
            // rather than wrapping. Reaching `u64::MAX` dropped commands is not a
            // reachable state; the check is defensive.
            if let Some(next) = self.commands_dropped.checked_add(1) {
                self.commands_dropped = next;
            }
        }
    }

    // --- Screen navigation (reachable-only, capability-driven) ----------------

    /// Switch to the screen a number key selects (`docs/05-views-and-ux.md` §2).
    /// A **reachable** target switches; an **unavailable** one flashes a one-line
    /// keybar hint and leaves the current screen in place — so `screen` is only
    /// ever set to a reachable value and the render dispatch (#13) stays total.
    /// Reachability reads declared [`ProviderCapabilities`], **never** a
    /// [`ProviderId`] match.
    fn request_switch_screen(&mut self, slot: u8) {
        let outcome = match &mut self.mode {
            Mode::Live(live) => match live_screen_for_slot(slot) {
                Some(screen) if live.screen == screen => SwitchOutcome::NoChange,
                Some(screen) if live.screen_reachable(screen) => {
                    live.screen = screen;
                    SwitchOutcome::Switched
                }
                Some(screen) => SwitchOutcome::Unavailable(format!(
                    "{} not available on {}",
                    live_screen_name(screen),
                    live.source.provider.as_str()
                )),
                None => SwitchOutcome::Unavailable(format!("no screen bound to {slot}")),
            },
            Mode::Replay(replay) => match replay_screen_for_slot(slot) {
                Some(screen) if replay.screen == screen => SwitchOutcome::NoChange,
                Some(screen) if is_replay_screen_reachable(screen) => {
                    replay.screen = screen;
                    SwitchOutcome::Switched
                }
                Some(screen) => SwitchOutcome::Unavailable(replay_unavailable_hint(screen)),
                None => SwitchOutcome::Unavailable(format!("no screen bound to {slot}")),
            },
        };
        match outcome {
            SwitchOutcome::Switched => self.dirty = true,
            SwitchOutcome::NoChange => {}
            SwitchOutcome::Unavailable(hint) => self.set_status_hint(hint),
        }
    }

    /// Cycle to the next (`forward`) or previous **reachable** screen for the
    /// active mode+provider (`docs/05-views-and-ux.md` §2), skipping unavailable
    /// screens so `Tab`/`S-Tab` never land on a not-supported body.
    fn cycle_screen(&mut self, forward: bool) {
        let changed = match &mut self.mode {
            Mode::Live(live) => {
                let reachable: Vec<LiveScreen> = LIVE_SCREEN_ORDER
                    .into_iter()
                    .filter(|screen| live.screen_reachable(*screen))
                    .collect();
                match next_in_cycle(&reachable, live.screen, forward) {
                    Some(next) if next != live.screen => {
                        live.screen = next;
                        true
                    }
                    _ => false,
                }
            }
            Mode::Replay(replay) => {
                let reachable: Vec<ReplayScreen> = REPLAY_SCREEN_ORDER
                    .into_iter()
                    .filter(|screen| is_replay_screen_reachable(*screen))
                    .collect();
                match next_in_cycle(&reachable, replay.screen, forward) {
                    Some(next) if next != replay.screen => {
                        replay.screen = next;
                        true
                    }
                    _ => false,
                }
            }
        };
        if changed {
            self.dirty = true;
        }
    }

    /// Flash a transient one-line keybar hint (`docs/05-views-and-ux.md` §2). It
    /// decays on the next key or after [`HINT_TICKS`] ticks, so it stays visible for
    /// a readable minimum duration.
    fn set_status_hint(&mut self, hint: String) {
        self.status_hint = Some(hint);
        self.hint_ttl = HINT_TICKS;
        self.dirty = true;
    }

    /// Clear a transient keybar hint if one is showing, marking the frame dirty
    /// only when a hint was actually cleared (so an idle key with no hint sets no
    /// dirty).
    fn clear_status_hint(&mut self) {
        if self.status_hint.take().is_some() {
            self.hint_ttl = 0;
            self.dirty = true;
        }
    }
}

/// The outcome of a number-key screen switch (`docs/05-views-and-ux.md` §2),
/// computed under the mode borrow and applied to [`App`] afterwards to avoid an
/// overlapping mutable borrow.
enum SwitchOutcome {
    /// The switch was applied — the screen changed.
    Switched,
    /// The requested screen is already active — nothing to do.
    NoChange,
    /// The requested screen is unavailable — flash this keybar hint, no switch.
    Unavailable(String),
}

/// The outcome of the **global** key-dispatch level
/// (`docs/02-tui-architecture.md` §9): whether [`App::dispatch_key_global`]
/// consumed the key, or the render loop (#13) should forward it to the active
/// screen's `handle_key`.
///
/// A ChainView closed set matched exhaustively with no wildcard arm, so the
/// render-loop forwarding site is revisited by the compiler if a routing outcome
/// is ever added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum KeyRoute {
    /// The global level handled the key (quit / help / reconnect / rediscover, or
    /// a modal-help intercept); the active screen must **not** also see it.
    Consumed,
    /// The global level did not bind the key; forward it to the active screen.
    ToScreen,
}

// ---------------------------------------------------------------------------
// Mode + mode-scoped screens (§3).
// ---------------------------------------------------------------------------

/// The `Live | Replay` mode, each owning its own state and active screen (§3).
///
/// A ChainView closed set matched exhaustively with no wildcard arm, so a new
/// mode forces every match site to be revisited by the compiler.
///
/// The `Live` variant is far larger than `Replay` (it owns the live
/// [`ChainStore`]), but exactly **one** `Mode` exists per process — it is
/// `App.mode`, never collected — so the size asymmetry wastes no memory, while
/// boxing the hot `LiveState` would add indirection to every event fold. The
/// documented shape (§3) is intentionally unboxed.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Mode {
    /// Live mode: a streamed option chain from the selected provider.
    Live(LiveState),
    /// Replay mode: an IronCondor result bundle rendered read-only.
    Replay(ReplayState),
}

/// The active **Live**-mode screen (`docs/02-tui-architecture.md` §3, §7).
///
/// Owned by [`LiveState`], so a live screen can never pair with a replay mode —
/// the out-of-mode pair is unrepresentable. Fieldless, so `#[repr(u8)]` per the
/// ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum LiveScreen {
    /// The chain matrix (strikes × call/put), the default screen.
    #[default]
    Chain,
    /// The order-book depth ladder (depth-capable providers only).
    Depth,
    /// The volatility smile / surface.
    Surface,
    /// The multi-leg payoff diagram.
    Payoff,
}

/// The active **Replay**-mode screen (`docs/02-tui-architecture.md` §3, §7).
///
/// Owned by [`ReplayState`]. Fieldless, so `#[repr(u8)]` per the ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ReplayScreen {
    /// The equity curve, P&L attribution, and per-trade drill-down.
    #[default]
    Replay,
    /// The payoff diagram for the selected trade (v0.5, `docs/ROADMAP.md`).
    Payoff,
}

// ---------------------------------------------------------------------------
// LiveState: the composite source + overlay binding, the store, and screen.
// ---------------------------------------------------------------------------

/// Live-mode state: the chain source binding, an optional overlay binding, the
/// active screen, the live [`ChainStore`], and the per-screen support state
/// (`docs/02-tui-architecture.md` §3).
#[derive(Debug)]
pub struct LiveState {
    /// The provider that supplies the chain structure (and possibly its own
    /// quotes/Greeks), with its declared capabilities and this side's health.
    pub source: SourceBinding,
    /// An optional quote/Greek overlay provider (standalone DXLink over another
    /// provider's chain), with its own capabilities and **independent** health.
    pub overlay: Option<OverlayBinding>,
    /// The active Live screen — only ever set to a reachable value
    /// ([`set_screen`](LiveState::set_screen)).
    pub screen: LiveScreen,
    /// The normalized, streaming-current chain (`src/chain`), the source of truth
    /// draw reads.
    pub store: ChainStore,
    /// The focused underlying/expiry/strike selection.
    pub selection: Selection,
    /// The multi-leg payoff-builder state (v0.2, `docs/05-views-and-ux.md` §3).
    pub payoff_builder: PayoffBuilder,
    /// The screen's load lifecycle (loading / ready / error).
    pub load: ScreenLoad,
    /// The cached ATM strike-row index (ascending strike order) — the chain
    /// screen's `◀ATM` marker and default scroll anchor. Recomputed **off-draw**
    /// only when a poll changes the strike ladder or spot ([`new`](LiveState::new)
    /// and [`apply_chain_snapshot`](LiveState::apply_chain_snapshot)); a quote
    /// patches a row but never moves the ATM, so it is not recomputed on a quote.
    /// `draw` reads it via [`atm_index`](LiveState::atm_index) in O(1) instead of
    /// rescanning the full ladder each frame (the frame-budget rule, `CLAUDE.md`).
    atm_index: Option<usize>,
}

impl LiveState {
    /// A live state bound to a chain source, seeded with its store, on the
    /// default [`LiveScreen::Chain`] screen in the [`ScreenLoad::Loading`] state.
    ///
    /// The default [`LiveScreen::Chain`] is only reachable because a live source
    /// always assembles a chain (`ChainCapability != None`); a chain-less feed
    /// (standalone dxlink) is rejected as a *source* at registry startup (#12),
    /// never reaching here. The `debug_assert` makes that non-local invariant
    /// local so a future construction path that violated it would fail fast in
    /// development rather than open on an unreachable screen.
    #[must_use]
    pub fn new(source: SourceBinding, store: ChainStore) -> Self {
        debug_assert!(
            chain_present(source.capabilities.chain),
            "a live source must assemble a chain so the default Chain screen is reachable",
        );
        // Seed the cached ATM index off-draw from the initial chain, so the first
        // frame reads it without rescanning the ladder.
        let atm_index = atm_index_of(store.chain());
        Self {
            source,
            overlay: None,
            screen: LiveScreen::Chain,
            store,
            selection: Selection::default(),
            payoff_builder: PayoffBuilder::new(),
            load: ScreenLoad::Loading,
            atm_index,
        }
    }

    /// The cached ATM strike-row index (ascending strike order), or `None` for an
    /// empty chain — the chain screen's `◀ATM` marker and default scroll anchor.
    /// Recomputed off-draw on a poll, so `draw` reads it in O(1) rather than
    /// rescanning the strike ladder each frame.
    #[must_use]
    pub fn atm_index(&self) -> Option<usize> {
        self.atm_index
    }

    /// Bind an overlay provider (its health is tracked independently of the
    /// source's).
    #[must_use]
    pub fn with_overlay(mut self, overlay: OverlayBinding) -> Self {
        self.overlay = Some(overlay);
        self
    }

    /// Switch to `screen` **iff** it is reachable for the effective capabilities,
    /// returning whether the switch was applied. A screen is never set to an
    /// unreachable value, so the render dispatch (#13) stays total without an
    /// "unavailable" arm. The caller sets [`App::dirty`] on a `true` result; the
    /// `Tab` skip and number-key hint build on this in #14.
    #[must_use = "the returned bool reports whether the screen switch was applied"]
    pub fn set_screen(&mut self, screen: LiveScreen) -> bool {
        if self.screen_reachable(screen) {
            self.screen = screen;
            true
        } else {
            false
        }
    }

    /// Whether `screen` is reachable for this state's **effective** capabilities
    /// (source ∪ overlay). Reads capabilities only — never the [`ProviderId`].
    #[must_use]
    pub fn screen_reachable(&self, screen: LiveScreen) -> bool {
        is_screen_reachable(screen, &self.effective_capabilities())
    }

    /// The capabilities the UI gates on: the source's, with the overlay's live
    /// quote/Greek and depth reach unioned in when an overlay is bound
    /// (`docs/02-tui-architecture.md` §3, the capabilities union). The structure
    /// dimensions (`chain`/`chain_poll`) always come from the source, since the
    /// overlay has no chain of its own. The remaining stream/tape union
    /// refinements are finalized with the navigation layer (#13/#14).
    #[must_use]
    pub fn effective_capabilities(&self) -> ProviderCapabilities {
        let source = self.source.capabilities;
        let Some(overlay) = self.overlay.as_ref() else {
            return source;
        };
        let overlay = overlay.capabilities;
        ProviderCapabilities::builder()
            .chain(source.chain)
            .depth(source.depth || overlay.depth)
            .greeks(max_greeks(source.greeks, overlay.greeks))
            .option_stream(source.option_stream)
            .underlying_stream(source.underlying_stream || overlay.underlying_stream)
            .chain_poll(source.chain_poll)
            .trades_tape(source.trades_tape || overlay.trades_tape)
            .auth(source.auth)
            .build()
    }

    /// Fold one market update into the store, returning whether the fold changed
    /// anything the screen would render (so the caller can set `dirty`).
    ///
    /// Matched exhaustively over the [`MarketUpdate`] closed set with no wildcard
    /// arm. `Depth` has no store path yet (the depth-ladder store lands with the
    /// depth screen, v0.5) — folding it is a documented no-op here.
    fn apply_market(&mut self, update: MarketUpdate) -> bool {
        let changed = match update {
            MarketUpdate::Quote(quote) => merged(self.store.apply_quote(&quote)),
            MarketUpdate::Greeks(greeks) => merged(self.store.apply_greeks(&greeks)),
            MarketUpdate::Depth(_) => false,
            MarketUpdate::Chain(snapshot) => self.apply_chain_snapshot(snapshot),
            MarketUpdate::Health(provider, health) => self.apply_health(&provider, health),
        };
        // A data-changing fold may move the committed payoff's t+0 curve (its
        // per-leg IV is re-read from the just-updated sidecar). The refresh runs
        // **only** while the t+0 curve is the shown one — the hot quote path does
        // nothing when the (IV-independent) expiration curve is displayed — and
        // re-prices the committed legs directly, never reconstructing a
        // `CustomStrategy` (#27, off the draw path).
        if changed {
            self.refresh_committed_tplus0();
        }
        changed
    }

    /// Re-price the committed strategy's t+0 curve against the current store
    /// snapshot when it is the shown curve. A cheap no-op otherwise (two field
    /// reads), so a streaming quote never rebuilds a hidden curve.
    fn refresh_committed_tplus0(&mut self) {
        if self.payoff_builder.curve() != CurveMode::TPlus0
            || self.payoff_builder.committed().is_none()
        {
            return;
        }
        let LiveState {
            store,
            payoff_builder,
            ..
        } = self;
        payoff_builder.refresh_tplus0(store);
    }

    /// Reconcile the store's structure against a fresh full re-poll delivered as
    /// a [`ChainSnapshot`]. The snapshot is lowered back into a [`ChainFetch`]
    /// and handed to [`ChainStore::apply_poll`] — the store's own tombstone /
    /// pending-drain reconciliation — using the snapshot's own poll timestamp, so
    /// the fold is deterministic and reads no wall clock. A snapshot with no poll
    /// timestamp is a pre-seed artifact and carries no structure to reconcile (a
    /// defensive no-op).
    fn apply_chain_snapshot(&mut self, snapshot: ChainSnapshot) -> bool {
        let ChainSnapshot {
            chain_key,
            chain,
            aliases,
            last_full_poll,
            source: _,
            health: _,
        } = snapshot;
        match last_full_poll {
            Some(polled) => {
                let (provider, underlying, expiration_utc) = chain_key;
                let fetch = ChainFetch::new(
                    chain,
                    ExpirySource::new(underlying, expiration_utc, provider),
                    aliases,
                );
                self.store.apply_poll(fetch, polled);
                // A poll can move spot or the strike ladder, so refresh the cached
                // ATM index off-draw here — draw never rescans the ladder.
                self.atm_index = atm_index_of(self.store.chain());
                true
            }
            None => false,
        }
    }

    /// Route a stream-health transition to the correct side by comparing the
    /// event's provider to each binding's — an equality check, never a `match` on
    /// [`ProviderId`]. Either side failing degrades **only** that side (§3): a
    /// source drop badges the structure stale and clears the store's price
    /// direction indicators; an overlay drop badges quotes/Greeks stale while the
    /// structure keeps rendering. Returns whether a side changed.
    fn apply_health(&mut self, provider: &ProviderId, health: StreamHealth) -> bool {
        let mut changed = false;
        if *provider == self.source.provider {
            self.source.health = health.clone();
            // Forward the SOURCE feed's health to the store so its price-direction
            // indicators clear on a stale/reconnecting chain feed (`docs/01` §6).
            self.store.apply_health(health.clone());
            changed = true;
        }
        if let Some(overlay) = &mut self.overlay
            && *provider == overlay.provider
        {
            overlay.health = health;
            changed = true;
        }
        changed
    }
}

/// The provider that supplies the chain structure (and possibly its own
/// quotes/Greeks) plus the capabilities the UI gates on and this side's health
/// (`docs/02-tui-architecture.md` §3).
#[derive(Debug, Clone)]
pub struct SourceBinding {
    /// The source provider's id.
    pub provider: ProviderId,
    /// Its declared capabilities — gates which screens are reachable.
    pub capabilities: ProviderCapabilities,
    /// This side's connection health/freshness.
    pub health: StreamHealth,
}

impl SourceBinding {
    /// Bind a chain source from its id, capabilities, and initial health.
    #[must_use]
    pub fn new(
        provider: ProviderId,
        capabilities: ProviderCapabilities,
        health: StreamHealth,
    ) -> Self {
        Self {
            provider,
            capabilities,
            health,
        }
    }
}

/// An optional quote/Greek overlay provider that streams contract updates but
/// has no chain of its own (standalone DXLink over another provider's chain,
/// `docs/02-tui-architecture.md` §3). Its health is tracked **per side**,
/// independent of the source's — the composite degrades one side at a time.
#[derive(Debug, Clone)]
pub struct OverlayBinding {
    /// The overlay provider's id (e.g. `dxlink`).
    pub provider: ProviderId,
    /// Its declared capabilities — unioned into the gating set.
    pub capabilities: ProviderCapabilities,
    /// This side's connection health/freshness, independent of the source's.
    pub health: StreamHealth,
}

impl OverlayBinding {
    /// Bind an overlay from its id, capabilities, and initial health.
    #[must_use]
    pub fn new(
        provider: ProviderId,
        capabilities: ProviderCapabilities,
        health: StreamHealth,
    ) -> Self {
        Self {
            provider,
            capabilities,
            health,
        }
    }
}

/// Which option leg (call or put) is focused on the selected strike row
/// (`docs/05-views-and-ux.md` §3), toggled by `c` / `p` on the chain matrix
/// (#18).
///
/// A ChainView UI-selection enum, fieldless, so `#[repr(u8)]` per the ruleset;
/// it defaults to [`Call`](LegFocus::Call). The chain matrix emphasizes the
/// focused leg on the selected row; a future leg-detail view derives that leg's
/// own ITM/OTM from its style, never off the shared row relation
/// (`docs/01-domain-model.md` §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum LegFocus {
    /// The call leg is focused.
    #[default]
    Call,
    /// The put leg is focused.
    Put,
}

/// The focused underlying/expiry/strike selection (`docs/02-tui-architecture.md`
/// §3). The typed cursor the chain matrix (#18) drives: the focused strike row
/// and the focused leg on it. Expiry/underlying navigation emits a [`Command`]
/// rather than living here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    /// The focused strike-row index within the visible chain, or `None` before
    /// any row is focused. Draw reads it with `.get()` + a fallback — never an
    /// unchecked index — and clamps it to the current strike count, so a poll
    /// that shrank the chain never yields an out-of-range cursor.
    pub focused_row: Option<usize>,
    /// The focused option leg on the selected strike row, toggled by `c` / `p`
    /// (`docs/05-views-and-ux.md` §3). Defaults to the call leg.
    pub focused_leg: LegFocus,
}

/// Whether a payoff-builder leg is **bought** (long) or **sold** (short)
/// (`docs/05-views-and-ux.md` §3). A ChainView UI closed set, fieldless, so
/// `#[repr(u8)]` per the ruleset; it defaults to [`Buy`](Side::Buy) — a freshly
/// appended leg is long until `s` toggles it. The `Selection` carries no side, so
/// [`PayoffBuilder`](PayoffBuilder)'s append seeds a leg long and `s` flips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Side {
    /// A bought (long) leg.
    #[default]
    Buy,
    /// A sold (short) leg.
    Sell,
}

impl Side {
    /// The opposite side — `Buy ⇄ Sell` (`s` on the payoff screen). Exhaustive
    /// over the closed set with no wildcard arm.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }

    /// The short, color-independent label for the side (`BUY` / `SELL`), so the
    /// side reads on a monochrome terminal (color is never the only signal).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

/// One leg of the multi-leg payoff builder (`docs/05-views-and-ux.md` §3): a
/// contract at a `strike`/`style`, a `side` (buy/sell), and an integer `qty`
/// (contracts). Appended from the chain's focused leg (`a`) and edited in place by
/// the cursor keys. A `qty` of `0` is an invalid state validation rejects, so it is
/// a plain `u32`, not a `NonZero` — the zero-qty check exists precisely to catch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuilderLeg {
    /// The strike price of the contract.
    pub strike: Positive,
    /// The option style (call or put).
    pub style: OptionStyle,
    /// Whether the leg is bought or sold.
    pub side: Side,
    /// The number of contracts; a `0` qty fails validation.
    pub qty: u32,
}

impl BuilderLeg {
    /// The leg's mark (mid) price in `chain`, or `None` when the strike is absent
    /// from the chain, the relevant side has no mid this frame, **or** the mid is the
    /// non-finite `Positive::INFINITY` sentinel — the `—`-not-`0` honesty rule
    /// (`docs/01-domain-model.md` §8). A pure lookup: validation and the builder
    /// widget both read the mark through this one helper so they cannot disagree on
    /// what "known" means.
    #[must_use]
    pub fn mark_in(&self, chain: &OptionChain) -> Option<Positive> {
        let od = chain
            .options
            .iter()
            .find(|o| o.strike_price == self.strike)?;
        let mid = match self.style {
            OptionStyle::Call => od.call_middle,
            OptionStyle::Put => od.put_middle,
        };
        // Honesty symmetry with the display: `fmt_price` renders the `Positive::INFINITY`
        // sentinel as `—` (unknown), so an infinite mid is "no mark" here too — it fails
        // validation with `NoMark`, never passing a value the widget would paint as `—`.
        mid.filter(|m| *m != Positive::INFINITY)
    }
}

/// Which payoff curve the screen draws (`docs/05-views-and-ux.md` §4): the
/// **expiration** payoff or the **t+0** (mark-based) curve, toggled by `t`. The
/// state lives here because the toggle is a view preference on the builder; the
/// curve itself is rendered by the payoff screen (#27). A closed set, fieldless, so
/// `#[repr(u8)]` per the ruleset; defaults to [`Expiration`](CurveMode::Expiration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CurveMode {
    /// The payoff at expiration.
    #[default]
    Expiration,
    /// The t+0 (current-mark) curve.
    TPlus0,
}

impl CurveMode {
    /// The other curve — `Expiration ⇄ t+0` (`t`). Exhaustive with no wildcard arm.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Expiration => Self::TPlus0,
            Self::TPlus0 => Self::Expiration,
        }
    }

    /// The short, color-independent label for the curve mode.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Expiration => "expiration",
            Self::TPlus0 => "t+0",
        }
    }
}

/// A single validation failure of the payoff builder, projected inline in the
/// builder panel (`docs/05-views-and-ux.md` §3, §6). A ChainView closed set matched
/// exhaustively with no wildcard arm. Leg indices are **1-based** in the message so
/// the text matches the ladder the user sees (e.g. `leg 2: no mark`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegError {
    /// The builder has no legs — the empty state, not a per-leg failure.
    Empty,
    /// The leg at zero-based index `idx` has a zero quantity.
    ZeroQty {
        /// The zero-based index of the offending leg.
        idx: usize,
    },
    /// The leg at zero-based index `idx` has no known/fresh mark in the current
    /// chain snapshot (its strike is absent, or its side has no mid this frame).
    NoMark {
        /// The zero-based index of the offending leg.
        idx: usize,
    },
    /// The leg at zero-based index `idx` has a mark, but its stream quote went
    /// stale beyond the documented threshold (`QUOTE_STALE_AFTER`) — a cached
    /// midpoint from a feed that stopped updating must not be committed as an
    /// entry price. Mirrors the #24 kernel gate: a leg with no stream clock
    /// (poll-seeded) is not gated.
    StaleMark {
        /// The zero-based index of the offending leg.
        idx: usize,
    },
}

impl std::fmt::Display for LegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "add a leg with `a`"),
            Self::ZeroQty { idx } => write!(f, "leg {}: zero quantity", idx + 1),
            Self::NoMark { idx } => write!(f, "leg {}: no mark", idx + 1),
            Self::StaleMark { idx } => write!(f, "leg {}: mark is stale", idx + 1),
        }
    }
}

/// A validated, committed multi-leg strategy (`docs/05-views-and-ux.md` §3, §4).
/// Built by [`PayoffBuilder::validate`] from a strategy that passed every check,
/// then enriched on commit with the payoff **geometry** — the expiration and t+0
/// curves, the shared price grid, and the break-even points — all sampled from
/// `optionstratlib` **off** the draw path, so the payoff screen (#27) draws the
/// cached series **without re-validating or re-pricing**.
/// `#[non_exhaustive]` so those cached fields stay a source-compatible addition; no
/// `Eq` because the cached [`GraphData`] carries display `f64` line widths.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CommittedStrategy {
    /// The validated legs, in the order they were built.
    legs: Vec<BuilderLeg>,
    /// The shared underlying-price x-grid both curves and the t+0 refresh reuse
    /// (empty when the geometry could not be priced).
    grid: Vec<Positive>,
    /// The **frozen** commit-time positions (entry premium P0 per leg). The t+0
    /// tick refresh reprices these — mutating only the sampled underlying and the
    /// current IV — so the entry premium never re-reads the live mark and both
    /// curves share one cost basis (#27 SF-1). Empty when the geometry could not be
    /// priced.
    entry_positions: Vec<Position>,
    /// The expiration payoff as a single `GraphData::Series` (price → P&L).
    expiration: GraphData,
    /// The t+0 (mark-based) curve as a single `GraphData::Series` (price → P&L), or
    /// an empty series when a leg lacks a plausible IV (the "t+0 unavailable" state).
    tplus0: GraphData,
    /// The break-even underlying prices, read off the expiration series (#27).
    break_evens: Vec<Positive>,
}

impl CommittedStrategy {
    /// The validated legs, in build order.
    #[must_use]
    pub fn legs(&self) -> &[BuilderLeg] {
        &self.legs
    }

    /// The cached break-even underlying prices (empty when the curve could not be
    /// priced), overlaid as markers by the payoff screen (#27).
    #[must_use]
    pub fn break_even_points(&self) -> &[Positive] {
        &self.break_evens
    }

    /// Whether the cached, IV-independent **expiration** series carries renderable
    /// points — so the payoff screen can tell a t+0 curve that is unavailable purely
    /// for lack of a plausible IV (expiration still renders) from a strategy that
    /// cannot be priced at all (#27 SF-2). Exhaustive over [`GraphData`], no wildcard.
    #[must_use]
    fn has_expiration_curve(&self) -> bool {
        match &self.expiration {
            GraphData::Series(series) => !series.x.is_empty(),
            GraphData::MultiSeries(_) | GraphData::GraphSurface(_) => false,
        }
    }

    /// The cached payoff series for `curve` — the expiration or t+0
    /// `GraphData::Series` — exhaustive over [`CurveMode`] with no wildcard.
    #[must_use]
    fn active_graph(&self, curve: CurveMode) -> &GraphData {
        match curve {
            CurveMode::Expiration => &self.expiration,
            CurveMode::TPlus0 => &self.tplus0,
        }
    }

    /// Enrich a freshly-validated strategy with the payoff geometry sampled from
    /// the `store` snapshot, **off** the draw path. Leaves the geometry empty when
    /// the legs cannot be priced (a missing IV or a non-future expiry) — the
    /// screen then renders a deliberate "curve unavailable" state rather than a
    /// fabricated line.
    fn populate_geometry(&mut self, store: &ChainStore) {
        if let Some(geometry) = payoff_build::build_geometry(&self.legs, store) {
            self.grid = geometry.grid;
            self.entry_positions = geometry.entry_positions;
            self.expiration = geometry.expiration;
            self.tplus0 = geometry.tplus0;
            self.break_evens = geometry.break_evens;
        }
    }

    /// Re-price the t+0 curve against the current `store` snapshot on the committed
    /// grid by repricing the **frozen** entry positions (the entry premium stays P0;
    /// only the sampled underlying and the current per-leg IV move), returning
    /// whether the series changed. A leg that loses its plausible IV flips the t+0
    /// curve to the empty "unavailable" series. Constructs **no** `CustomStrategy`
    /// and never touches the expiration curve or break-evens — the tick-path refresh
    /// (#27 SF-1/SF-2).
    fn refresh_tplus0(&mut self, store: &ChainStore) -> bool {
        if self.grid.is_empty() {
            return false;
        }
        let series =
            payoff_build::rebuild_tplus0(&self.legs, store, &self.grid, &self.entry_positions);
        if series != self.tplus0 {
            self.tplus0 = series;
            true
        } else {
            false
        }
    }
}

/// The multi-leg payoff-builder state machine (`docs/05-views-and-ux.md` §3): an
/// ordered [`BuilderLeg`] list with a cursor, the current [`CurveMode`], the last
/// validation errors, and the committed strategy. It lives in the **application**
/// layer so the payoff screen (`src/ui/payoff.rs`) drives it through these methods
/// and reads it through the accessors — the UI never reaches into the fields
/// directly (`rules/global_rules.md`, inner fields private).
///
/// Every mutation that changes visible state bumps a monotonic
/// [`revision`](Self::revision) the render loop diffs to schedule a redraw, exactly
/// as it diffs the chain [`Selection`] (`docs/02-tui-architecture.md` §8): a builder
/// edit emits **no** [`AppEvent`], so the revision is how the loop learns the frame
/// changed.
#[derive(Debug, Clone)]
pub struct PayoffBuilder {
    /// The ordered legs being built.
    legs: Vec<BuilderLeg>,
    /// The cursor index into [`legs`](Self::legs); every in-place edit targets this
    /// leg. Kept in bounds by every mutator (or `0` when empty).
    cursor: usize,
    /// Which payoff curve the screen draws (`t` toggles); a view preference kept
    /// across a discard.
    curve: CurveMode,
    /// The validation errors from the last [`commit`](Self::commit) attempt, cleared
    /// on any edit. Rendered inline in the builder.
    errors: Vec<LegError>,
    /// The committed strategy (set by a successful [`commit`](Self::commit), cleared
    /// on any edit). `None` while the strategy is still being built.
    committed: Option<CommittedStrategy>,
    /// A monotonic edit counter the render loop diffs to schedule a redraw.
    revision: u64,
    /// A monotonic counter, **distinct** from [`revision`](Self::revision), bumped
    /// only when the **active** payoff `GraphData` changes (a commit, a curve
    /// toggle, a t+0 refresh that moved the series, or a clear) — never on a
    /// cursor-only edit that would over-invalidate the projection cache (#27).
    graph_revision: u64,
    /// The empty payoff series returned by [`active_graph_data`](Self::active_graph_data)
    /// while nothing is committed — the #23 adapter renders it as the deliberate
    /// "add a leg" empty projection.
    empty_graph: GraphData,
}

impl Default for PayoffBuilder {
    fn default() -> Self {
        Self {
            legs: Vec::new(),
            cursor: 0,
            curve: CurveMode::default(),
            errors: Vec::new(),
            committed: None,
            revision: 0,
            graph_revision: 0,
            empty_graph: payoff_build::empty_series(),
        }
    }
}

/// The default quantity of a freshly appended leg (one contract).
const DEFAULT_LEG_QTY: u32 = 1;

/// The maximum quantity a single leg's `+` can reach, so an increment can never
/// overflow `u32` (`rules/global_rules.md`, no `saturating_*`/`wrapping_*`). A
/// four-digit cap is far beyond any realistic hand-built strategy.
const MAX_LEG_QTY: u32 = 9_999;

impl PayoffBuilder {
    /// An empty payoff builder (no legs, cursor at `0`, expiration curve).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // --- Accessors (the UI reads state; it never mutates the fields) ----------

    /// The ordered legs being built, borrowed for rendering.
    #[must_use]
    pub fn legs(&self) -> &[BuilderLeg] {
        &self.legs
    }

    /// Whether the builder has no legs (the empty state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.legs.is_empty()
    }

    /// The cursor index into [`legs`](Self::legs) — always in bounds, or `0` when
    /// empty. The UI reads it with [`legs`](Self::legs)`.get(cursor)`, never an
    /// unchecked index.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The active payoff [`CurveMode`] (`t` toggles it).
    #[must_use]
    pub fn curve(&self) -> CurveMode {
        self.curve
    }

    /// The validation errors from the last commit attempt (empty when none),
    /// borrowed for the inline error render.
    #[must_use]
    pub fn errors(&self) -> &[LegError] {
        &self.errors
    }

    /// The committed strategy, or `None` while still building.
    #[must_use]
    pub fn committed(&self) -> Option<&CommittedStrategy> {
        self.committed.as_ref()
    }

    /// The monotonic edit counter the render loop diffs to schedule a redraw
    /// (`docs/02-tui-architecture.md` §8).
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The monotonic **payoff-curve** revision, bumped only when the active payoff
    /// `GraphData` changes (`docs/05-views-and-ux.md` §4). Distinct from
    /// [`revision`](Self::revision): the ui view-cache (`src/ui/view.rs`) diffs
    /// this to decide when to re-project the payoff series **off** the draw path, so
    /// a cursor-only edit (which bumps `revision` but not this) never re-projects
    /// (#27).
    #[must_use]
    pub fn graph_revision(&self) -> u64 {
        self.graph_revision
    }

    /// The active payoff [`GraphData`] for the current [`CurveMode`] — the
    /// committed expiration or t+0 series, or a stored empty series while nothing
    /// is committed. The ui view-cache clones this off the draw path and projects
    /// it through `src/ui/graph.rs`; `draw` never builds it (#27).
    #[must_use]
    pub fn active_graph_data(&self) -> &GraphData {
        match self.committed.as_ref() {
            Some(committed) => committed.active_graph(self.curve),
            None => &self.empty_graph,
        }
    }

    /// The committed strategy's break-even underlying prices (empty while nothing
    /// is committed or the curve could not be priced), overlaid as markers by the
    /// payoff screen (#27).
    #[must_use]
    pub fn break_even_points(&self) -> &[Positive] {
        match self.committed.as_ref() {
            Some(committed) => committed.break_even_points(),
            None => &[],
        }
    }

    /// Whether the committed strategy's (IV-independent) **expiration** curve has
    /// renderable data. `false` while nothing is committed. The payoff screen reads
    /// this to render the honest "t+0 unavailable — no reliable IV" state when only
    /// the t+0 curve is missing while the expiration curve still renders (#27 SF-2).
    #[must_use]
    pub fn has_expiration_curve(&self) -> bool {
        match self.committed.as_ref() {
            Some(committed) => committed.has_expiration_curve(),
            None => false,
        }
    }

    // --- Mutations (each edit clears the stale commit + errors and bumps the
    //     revision when it changes visible state) ------------------------------
    //
    // These are `pub(crate)`: the builder is hand-driven only by the in-crate UI
    // (`payoff::handle_key`, the chain-side `a`, the driver), never by an external
    // lib consumer — so the semver-governed public surface stays the types, the read
    // accessors, and `validate`, not these mutators.

    /// Append a leg from the chain's focused `strike`/`style`, long by default, with
    /// the cursor tracking the newly added leg (`a`). Any prior commit/errors are
    /// stale once the strategy changes, so they are cleared.
    pub(crate) fn append(&mut self, strike: Positive, style: OptionStyle) {
        self.legs.push(BuilderLeg {
            strike,
            style,
            side: Side::Buy,
            qty: DEFAULT_LEG_QTY,
        });
        // The cursor tracks the leg just added; `len` is ≥ 1 here.
        self.cursor = self.legs.len().max(1) - 1;
        self.on_edit();
    }

    /// Remove the cursor leg (`x`), clamping the cursor to the new last leg (or `0`
    /// when the list becomes empty). A no-op when the cursor is out of bounds.
    pub(crate) fn remove_cursor(&mut self) {
        if self.cursor >= self.legs.len() {
            return;
        }
        let _ = self.legs.remove(self.cursor);
        if self.cursor >= self.legs.len() {
            // Removed the last leg: step the cursor back to the new last (or 0).
            self.cursor = self.legs.len().max(1) - 1;
        }
        self.on_edit();
    }

    /// Increase the cursor leg's quantity by one, capped at `MAX_LEG_QTY` so the
    /// add can never overflow (`+`). A no-op with no cursor leg or already at the cap.
    pub(crate) fn increment_qty(&mut self) {
        if let Some(leg) = self.legs.get_mut(self.cursor)
            && leg.qty < MAX_LEG_QTY
        {
            // `leg.qty < MAX_LEG_QTY < u32::MAX`, so `+ 1` cannot overflow.
            leg.qty += 1;
            self.on_edit();
        }
    }

    /// Decrease the cursor leg's quantity by one, floored at `0` (`-`). A `0`-qty leg
    /// is left in place (validation reports it) so the user can bump it back up or
    /// remove it. A no-op with no cursor leg or already at `0`.
    pub(crate) fn decrement_qty(&mut self) {
        if let Some(leg) = self.legs.get_mut(self.cursor)
            && leg.qty > 0
        {
            // `leg.qty > 0`, so `- 1` cannot underflow.
            leg.qty -= 1;
            self.on_edit();
        }
    }

    /// Toggle the cursor leg buy ⇄ sell (`s`). A no-op with no cursor leg.
    pub(crate) fn toggle_cursor_side(&mut self) {
        if let Some(leg) = self.legs.get_mut(self.cursor) {
            leg.side = leg.side.toggled();
            self.on_edit();
        }
    }

    /// Toggle the payoff curve expiration ⇄ t+0 (`t`). Selects which cached series
    /// the screen shows (#27); does not touch the built legs, so it keeps any
    /// commit/errors. Bumps [`graph_revision`](Self::graph_revision) **only when a
    /// strategy is committed** — the toggle switches the **active** payoff `GraphData`
    /// only then (with nothing committed both curves resolve to the same stored empty
    /// series), so it is symmetric with `clear`/`on_edit` and never triggers a
    /// redundant empty reprojection.
    pub(crate) fn toggle_curve(&mut self) {
        self.curve = self.curve.toggled();
        self.bump();
        if self.committed.is_some() {
            self.bump_graph();
        }
    }

    /// Re-price the committed strategy's t+0 curve against the current `store`
    /// snapshot, bumping [`graph_revision`](Self::graph_revision) only when the
    /// series changed. Called from the market-tick fold (never `draw`) and never
    /// reconstructs a `CustomStrategy` — a no-op with no committed strategy (#27).
    pub(crate) fn refresh_tplus0(&mut self, store: &ChainStore) {
        let Some(committed) = self.committed.as_mut() else {
            return;
        };
        if committed.refresh_tplus0(store) {
            self.bump_graph();
        }
    }

    /// Discard the uncommitted strategy and return to the empty state (`Esc`): clear
    /// the legs, cursor, errors, and any commit. The [`CurveMode`] is a view
    /// preference and is **kept**. A no-op (no redraw) when already empty.
    pub(crate) fn discard(&mut self) {
        if self.legs.is_empty() && self.errors.is_empty() && self.committed.is_none() {
            return;
        }
        // Clearing a committed strategy returns the active payoff `GraphData` to the
        // empty series, so the projection cache must re-project.
        let had_committed = self.committed.is_some();
        self.legs.clear();
        self.cursor = 0;
        self.errors.clear();
        self.committed = None;
        self.bump();
        if had_committed {
            self.bump_graph();
        }
    }

    /// Validate the built strategy against the store's chain and, when valid,
    /// enrich it with the payoff geometry and store it (`Enter`); on failure store
    /// the per-leg errors and commit nothing. Returns whether the strategy
    /// committed.
    ///
    /// The whole `store` is borrowed immutably while `self` is borrowed mutably —
    /// the caller passes disjoint field borrows (`store` vs `payoff_builder`) — so
    /// the geometry build (the payoff series + break-evens, sampled from
    /// `optionstratlib`) runs here, **off** the draw path, and a successful commit
    /// bumps [`graph_revision`](Self::graph_revision) so the projection cache
    /// re-projects. Freshness reaches validation as data (#26): the store's
    /// stream-quote receipt clocks + the analytics reference instant, so a leg
    /// whose feed died is rejected with `StaleMark` instead of committing its
    /// cached midpoint. No I/O and no wall-clock read (#27).
    pub(crate) fn commit(&mut self, store: &ChainStore) -> bool {
        let clocks = store.quote_clocks();
        let as_of = store.analytics_as_of();
        match self.validate(store.chain(), &clocks, as_of) {
            Ok(mut strategy) => {
                strategy.populate_geometry(store);
                self.committed = Some(strategy);
                self.errors.clear();
                self.bump();
                self.bump_graph();
                true
            }
            Err(errors) => {
                self.committed = None;
                self.errors = errors;
                self.bump();
                false
            }
        }
    }

    /// Validate the built strategy against the current `chain` snapshot — a **pure**
    /// function of the builder and the chain, inventing and pricing nothing. The
    /// checks are: **≥ 1 leg**, **no zero-qty leg**, and **every leg has a
    /// known/fresh mark** ([`BuilderLeg::mark_in`], the `—`-not-`0` rule). The
    /// result is what the widget renders inline, so #27 draws the committed strategy
    /// without re-validating.
    ///
    /// # Errors
    ///
    /// Returns a [`Vec`] of [`LegError`] describing every failure: a lone
    /// [`LegError::Empty`] when there are no legs, else a [`LegError::ZeroQty`] /
    /// [`LegError::NoMark`] / [`LegError::StaleMark`] per offending leg (a leg
    /// can report more than one). Freshness arrives as **data** (`clocks`,
    /// `as_of` — the store's per-instrument stream-quote receipt snapshots and
    /// the pricing reference instant), so validation stays a pure read: a leg
    /// whose recorded clock is stale beyond `QUOTE_STALE_AFTER` is rejected with
    /// `StaleMark` rather than committing a cached midpoint from a dead feed; a
    /// leg with no recorded clock (poll-seeded) is not gated, mirroring the #24
    /// kernel convention.
    pub fn validate(
        &self,
        chain: &OptionChain,
        clocks: &QuoteClocks,
        as_of: DateTime<Utc>,
    ) -> Result<CommittedStrategy, Vec<LegError>> {
        if self.legs.is_empty() {
            return Err(vec![LegError::Empty]);
        }
        // The clock lookup keys on the chain's absolute-UTC expiry; a chain whose
        // expiry is not absolute (an adapter-seam invariant violation) simply
        // yields no key match, so freshness degrades to ungated rather than
        // panicking or blocking the commit.
        let expiration_utc = match chain.get_expiration() {
            Some(ExpirationDate::DateTime(dt)) => Some(dt),
            _ => None,
        };
        let mut errors = Vec::new();
        for (idx, leg) in self.legs.iter().enumerate() {
            if leg.qty == 0 {
                errors.push(LegError::ZeroQty { idx });
            }
            if leg.mark_in(chain).is_none() {
                errors.push(LegError::NoMark { idx });
            } else if let Some(expiration_utc) = expiration_utc {
                let key = InstrumentKey {
                    underlying: chain.symbol.clone(),
                    expiration_utc,
                    strike: leg.strike,
                    style: leg.style,
                };
                if let Some(received) = clocks.received(&key)
                    && quote_is_stale(received, as_of)
                {
                    errors.push(LegError::StaleMark { idx });
                }
            }
        }
        if errors.is_empty() {
            // The geometry is priced separately by [`commit`] (off the draw path);
            // `validate` stays a pure leg/mark check, so the returned strategy
            // carries empty geometry until enriched.
            Ok(CommittedStrategy {
                legs: self.legs.clone(),
                grid: Vec::new(),
                entry_positions: Vec::new(),
                expiration: payoff_build::empty_series(),
                tplus0: payoff_build::empty_series(),
                break_evens: Vec::new(),
            })
        } else {
            Err(errors)
        }
    }

    /// Shared post-edit bookkeeping: a strategy edit invalidates the last commit and
    /// its errors, then bumps the revision so the loop redraws. Clearing a committed
    /// strategy also returns the active payoff `GraphData` to the empty series, so
    /// the projection cache is invalidated (only then, never on a cursor-only edit).
    fn on_edit(&mut self) {
        let had_committed = self.committed.is_some();
        self.committed = None;
        self.errors.clear();
        self.bump();
        if had_committed {
            self.bump_graph();
        }
    }

    /// Advance the redraw revision, wrapping to `0` on the (practically unreachable)
    /// `u64` overflow — `checked_add` avoids the banned `wrapping_add`, matching the
    /// tick counter (`docs/02-tui-architecture.md` §8).
    fn bump(&mut self) {
        self.revision = self.revision.checked_add(1).unwrap_or(0);
    }

    /// Advance the payoff-curve revision (the projection-cache invalidation signal),
    /// wrapping to `0` on the (practically unreachable) `u64` overflow.
    fn bump_graph(&mut self) {
        self.graph_revision = self.graph_revision.checked_add(1).unwrap_or(0);
    }
}

/// A screen's load lifecycle (`docs/02-tui-architecture.md` §3,
/// `docs/05-views-and-ux.md` §6).
///
/// The `Empty`, `Stale`, `Not-supported`, and `Computed-Greeks` render states of
/// `docs/05` §6 are **derived** at draw time (from the store rows, the binding
/// health, and the declared capabilities), so they are not variants here; this
/// enum is the load/failure axis only. The retry **key** an [`Error`] shows
/// (`r` reconnect vs `R` re-discover) is chosen by the render/keymap layer
/// (#13/#14) from the mode, not stored here.
///
/// [`Error`]: ScreenLoad::Error
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScreenLoad {
    /// First fetch / stream connecting — the screen renders its connecting state.
    #[default]
    Loading,
    /// The screen renders its body from the store (which may itself be empty).
    ///
    /// Producer note: the `Loading → Ready` transition is driven when the first
    /// chain lands — wired by the chain-matrix screen work (#18) off the store's
    /// populated state; this skeleton folds the store but does not yet flip
    /// `load`.
    Ready,
    /// A provider/fetch error — the screen renders the actionable message plus
    /// the mode's retry-key affordance (#13/#14).
    ///
    /// Producer note: no event in the current closed `AppEvent`/`MarketUpdate`
    /// set carries a provider-error message, so this state is not yet reachable
    /// by the fan-in. The error carrier that drives it lands with the live
    /// reconnect loop (#16), which will extend the closed set — a compile-fenced,
    /// source-compatible addition (every fold site is revisited by the compiler).
    Error {
        /// The actionable, non-secret error message to display.
        message: String,
    },
}

/// The status-bar model: provider health, clock, and mode
/// (`docs/02-tui-architecture.md` §3). A documented stub whose fields are
/// populated by the status line (#13/#14); `#[non_exhaustive]` so those fields
/// are a source-compatible addition.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct StatusLine;

// ---------------------------------------------------------------------------
// ReplayState: the bundle load state machine (v0.3 fills the internals).
// ---------------------------------------------------------------------------

/// Replay-mode state: the bundle directory, its load state machine, the active
/// screen, and the playback state (`docs/02-tui-architecture.md` §3). The
/// internals are filled by the v0.3 replay work; the shapes here are stable.
#[derive(Debug)]
pub struct ReplayState {
    /// The bundle directory (kept for reload via `R`).
    pub dir: PathBuf,
    /// The bundle load state machine.
    pub bundle: BundleLoad,
    /// The active Replay screen.
    pub screen: ReplayScreen,
    /// The playback state (paused / playing).
    pub play: Playback,
}

impl ReplayState {
    /// A replay state for a bundle directory: [`BundleLoad::Loading`], the
    /// default [`ReplayScreen::Replay`] screen, and paused playback.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            bundle: BundleLoad::Loading,
            screen: ReplayScreen::Replay,
            play: Playback::Paused,
        }
    }
}

/// The bundle load state machine (`docs/02-tui-architecture.md` §3): loading,
/// loaded, or a retryable error, so startup / failure / retry / success are all
/// representable. `R` re-issues a [`Command::ReloadBundle`]. The enum shape is
/// stable for v0.3; only [`LoadedReplay`]'s internals are filled later.
///
/// Producer note: no event in the current closed `AppEvent` set carries a bundle
/// load result, so `Ready`/`Error` are not yet reachable by the fan-in and the
/// state cannot leave `Loading` (`ReplaySeek` is an input, not a result). The
/// bundle-load-result carrier that drives these transitions lands with the v0.3
/// replay subcommand + bundle reader (#34), which will extend the closed set —
/// a compile-fenced, source-compatible addition.
#[derive(Debug)]
pub enum BundleLoad {
    /// The bundle is being opened and validated.
    Loading,
    /// The bundle loaded successfully. Boxed so the large loaded payload does not
    /// bloat the small `Loading`/`Error` variants.
    Ready(Box<LoadedReplay>),
    /// The bundle failed to load — an actionable [`crate::BundleError`] message;
    /// `R` retries.
    Error {
        /// The actionable, non-secret bundle-error message to display.
        message: String,
    },
}

/// The loaded replay payload — the bundle, its timeline cursor, and the selected
/// fill (`docs/02-tui-architecture.md` §3). A documented stub for #9;
/// `#[non_exhaustive]` so the v0.3 fields are a source-compatible addition.
#[derive(Debug)]
#[non_exhaustive]
pub struct LoadedReplay;

/// The replay playback state (`docs/04-replay-mode.md` §4). A documented stub;
/// v0.3 refines the speed model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Playback {
    /// Playback is paused (the default on load).
    #[default]
    Paused,
    /// Playback is running at the given speed multiplier.
    Playing {
        /// The playback speed multiplier (steps advanced per tick).
        speed: u8,
    },
}

// ---------------------------------------------------------------------------
// Capability-driven screen reachability (reads capabilities, never ProviderId).
// ---------------------------------------------------------------------------

/// Whether a [`LiveScreen`] is reachable for a declared [`ProviderCapabilities`]
/// set (`docs/02-tui-architecture.md` §3, `docs/03-data-providers.md` §11.4).
///
/// Reads capability dimensions only — there is **no `match` on [`ProviderId`]**,
/// so a built-in and an externally registered provider are gated identically.
/// The `Tab`/`S-Tab` skip and number-key hint that consume this land in #14.
#[must_use]
pub fn is_screen_reachable(screen: LiveScreen, caps: &ProviderCapabilities) -> bool {
    match screen {
        // The chain and its payoff need a chain to exist.
        LiveScreen::Chain | LiveScreen::Payoff => chain_present(caps.chain),
        // The depth ladder needs an order book.
        LiveScreen::Depth => caps.depth,
        // The vol smile/surface needs IV, i.e. some Greeks source.
        LiveScreen::Surface => greeks_available(caps.greeks),
    }
}

/// Whether the provider produces a chain at all. Exhaustive over
/// [`ChainCapability`] (not `matches!`) so a new variant forces this gate to
/// decide how it degrades (`CLAUDE.md` "Key Decisions").
fn chain_present(chain: ChainCapability) -> bool {
    match chain {
        ChainCapability::Native | ChainCapability::Assemble | ChainCapability::Partial => true,
        ChainCapability::None => false,
    }
}

/// Whether IV/Greeks are available (provided or computed locally). Exhaustive
/// over [`GreeksCapability`] so a new variant forces this gate to decide.
fn greeks_available(greeks: GreeksCapability) -> bool {
    match greeks {
        GreeksCapability::Provided | GreeksCapability::ComputedLocally => true,
        GreeksCapability::None => false,
    }
}

/// The more-capable of two Greeks sources for the capability union:
/// `Provided > ComputedLocally > None`. Exhaustive so a new variant forces a
/// decision.
fn max_greeks(a: GreeksCapability, b: GreeksCapability) -> GreeksCapability {
    match (a, b) {
        (GreeksCapability::Provided, _) | (_, GreeksCapability::Provided) => {
            GreeksCapability::Provided
        }
        (GreeksCapability::ComputedLocally, _) | (_, GreeksCapability::ComputedLocally) => {
            GreeksCapability::ComputedLocally
        }
        (GreeksCapability::None, GreeksCapability::None) => GreeksCapability::None,
    }
}

/// Whether a merge outcome changed what the screen would render (an applied patch
/// or a newly-refused overlay leg both do; a buffered or dropped update does
/// not).
fn merged(outcome: MergeOutcome) -> bool {
    match outcome {
        MergeOutcome::Applied | MergeOutcome::OverlayRefused => true,
        MergeOutcome::Buffered
        | MergeOutcome::DroppedOutOfOrder
        | MergeOutcome::DroppedTombstoned
        | MergeOutcome::DroppedCrossed => false,
    }
}

// ---------------------------------------------------------------------------
// Screen navigation policy (number-key slots, cycle order, reachability, names).
// ---------------------------------------------------------------------------

/// The live screens in number-key / cycle order (`docs/05-views-and-ux.md` §2).
const LIVE_SCREEN_ORDER: [LiveScreen; 4] = [
    LiveScreen::Chain,
    LiveScreen::Depth,
    LiveScreen::Surface,
    LiveScreen::Payoff,
];

/// The replay screens in number-key / cycle order (`docs/05-views-and-ux.md` §2).
const REPLAY_SCREEN_ORDER: [ReplayScreen; 2] = [ReplayScreen::Replay, ReplayScreen::Payoff];

/// The live screen a number-key slot selects (`docs/05-views-and-ux.md` §2):
/// `1`=Chain, `2`=Depth, `3`=Surface, `4`=Payoff; any other slot is unbound.
#[must_use]
fn live_screen_for_slot(slot: u8) -> Option<LiveScreen> {
    match slot {
        1 => Some(LiveScreen::Chain),
        2 => Some(LiveScreen::Depth),
        3 => Some(LiveScreen::Surface),
        4 => Some(LiveScreen::Payoff),
        _ => None,
    }
}

/// The replay screen a number-key slot selects (`docs/05-views-and-ux.md` §2):
/// `1`=Replay, `2`=Payoff (v0.5); any other slot is unbound.
#[must_use]
fn replay_screen_for_slot(slot: u8) -> Option<ReplayScreen> {
    match slot {
        1 => Some(ReplayScreen::Replay),
        2 => Some(ReplayScreen::Payoff),
        _ => None,
    }
}

/// Whether a [`ReplayScreen`] is reachable in the current build
/// (`docs/05-views-and-ux.md` §2.1). Replay is always reachable; the replay Payoff
/// screen is a **v0.5** feature and is not reachable yet — its number key flashes
/// the "Payoff is v0.5" hint rather than switching. This is a build/version gate,
/// not a capability or [`ProviderId`] gate (replay has no live provider).
#[must_use]
pub fn is_replay_screen_reachable(screen: ReplayScreen) -> bool {
    match screen {
        ReplayScreen::Replay => true,
        ReplayScreen::Payoff => false,
    }
}

/// The display name of a live screen for the keybar and hints
/// (`docs/05-views-and-ux.md` §8).
#[must_use]
pub(crate) fn live_screen_name(screen: LiveScreen) -> &'static str {
    match screen {
        LiveScreen::Chain => "Chain",
        LiveScreen::Depth => "Depth",
        LiveScreen::Surface => "Surface",
        LiveScreen::Payoff => "Payoff",
    }
}

/// The display name of a replay screen for the keybar and hints
/// (`docs/05-views-and-ux.md` §8).
#[must_use]
pub(crate) fn replay_screen_name(screen: ReplayScreen) -> &'static str {
    match screen {
        ReplayScreen::Replay => "Replay",
        ReplayScreen::Payoff => "Payoff",
    }
}

/// The keybar hint for an unavailable replay screen (`docs/05-views-and-ux.md`
/// §2.1). The replay Payoff screen is deferred to v0.5.
#[must_use]
fn replay_unavailable_hint(screen: ReplayScreen) -> String {
    match screen {
        ReplayScreen::Payoff => "Payoff is v0.5".to_owned(),
        // Replay is always reachable, so this arm is defensive; keep the exhaustive
        // match wildcard-free.
        ReplayScreen::Replay => "screen not available".to_owned(),
    }
}

/// The next element after `current` in `items`, wrapping, moving `forward` or
/// backward (`docs/05-views-and-ux.md` §2). Returns `None` when `items` is empty
/// or `current` is absent. Uses `.get()` + modular indexing — no unchecked index
/// and no `saturating_*`/`wrapping_*` arithmetic.
#[must_use]
fn next_in_cycle<T: PartialEq + Copy>(items: &[T], current: T, forward: bool) -> Option<T> {
    let idx = items.iter().position(|item| *item == current)?;
    let len = items.len();
    if len == 0 {
        return None;
    }
    let next = if forward {
        (idx + 1) % len
    } else {
        (idx + len - 1) % len
    };
    items.get(next).copied()
}

/// The current wall-clock instant from `std`'s clock (chrono's `clock` feature is
/// off, so `Utc::now` is unavailable) — the decay reference the tick handler stamps
/// onto [`App::now`] so `draw` reads a cached instant and never touches a wall clock
/// itself (`docs/02-tui-architecture.md` §7). Clamps a pathological system time to
/// the representable range and never `unwrap`s.
#[must_use]
fn now_utc() -> DateTime<Utc> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp(secs, since.subsec_nanos()).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The index (ascending strike order) of the strike nearest spot — the chain
/// screen's `◀ATM` row and default scroll anchor, cached on [`LiveState`] and
/// recomputed off-draw on a poll so `draw` never rescans the ladder. `None` for an
/// empty chain. Uses only `optionstratlib` chain types, so it is a domain-facing
/// projection the application layer may compute (no dependency on `src/ui`).
#[must_use]
pub(crate) fn atm_index_of(chain: &OptionChain) -> Option<usize> {
    let spot = chain.underlying_price.to_dec();
    let mut best: Option<(usize, Decimal)> = None;
    for (idx, od) in chain.options.iter().enumerate() {
        let diff = (od.strike_price.to_dec() - spot).abs();
        let better = match best {
            Some((_, best_diff)) => diff < best_diff,
            None => true,
        };
        if better {
            best = Some((idx, diff));
        }
    }
    best.map(|(idx, _)| idx)
}

/// Shared, crate-internal test constructors for a fully-formed [`App`] in any
/// reachable state, used by both this module's tests and the `src/ui` render-loop
/// tests (#13). Kept minimal and self-contained so a render test can enumerate
/// every reachable mode × screen × load state without reaching into a provider.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::PathBuf;
    use std::time::Duration;

    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use tokio::sync::mpsc;

    use super::{
        App, LiveScreen, LiveState, Mode, ReplayScreen, ReplayState, ScreenLoad, SourceBinding,
    };
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, ProviderId, StreamHealth,
    };
    use crate::config::ThemeChoice;
    use crate::event::Command;
    use crate::providers::{
        ChainCapability, ChainPollCapability, GreeksCapability, ProviderCapabilities,
    };

    const EXP: i64 = 1_700_000_000;

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> chrono::DateTime<chrono::Utc> {
        match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
            Some(t) => t,
            None => panic!("invalid test timestamp: {secs}"),
        }
    }

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    fn row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn chain_with(strikes: &[f64]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for strike in strikes {
            let _ = chain.options.insert(row(*strike));
        }
        chain
    }

    fn store() -> ChainStore {
        ChainStore::seed(
            ChainFetch::new(
                chain_with(&[60_000.0]),
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        )
    }

    /// A Deribit-like capability set: assembled chain, depth, provided Greeks — so
    /// every live screen is reachable and every screen body can be exercised.
    fn full_caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(true)
            .greeks(GreeksCapability::Provided)
            .chain_poll(ChainPollCapability::Poll {
                interval_hint_secs: 2,
            })
            .build()
    }

    /// A live [`App`] forced onto `screen` in `load` with `help_open == help`,
    /// plus the receiver half of its command channel so a test can assert on
    /// emitted [`Command`]s. The screen is set on the `pub` field directly (the
    /// reachability gate is proven separately, #9) so any screen can be rendered.
    #[must_use]
    pub(crate) fn live_app(
        screen: LiveScreen,
        load: ScreenLoad,
        help: bool,
    ) -> (App, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel::<Command>(8);
        let mut live = LiveState::new(
            SourceBinding::new(pid("deribit"), full_caps(), StreamHealth::Live),
            store(),
        );
        live.screen = screen;
        live.load = load;
        let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
        app.help_open = help;
        app.mark_drawn();
        (app, rx)
    }

    /// A replay [`App`] forced onto `screen` with `help_open == help`, plus its
    /// command-channel receiver.
    #[must_use]
    pub(crate) fn replay_app(screen: ReplayScreen, help: bool) -> (App, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel::<Command>(8);
        let mut replay = ReplayState::new(PathBuf::from("/bundle"));
        replay.screen = screen;
        let mut app = App::new(Mode::Replay(replay), ThemeChoice::Auto, tx);
        app.help_open = help;
        app.mark_drawn();
        (app, rx)
    }

    /// A live [`App`] in a given state, dropping the command receiver — for render
    /// tests that never assert on commands.
    #[must_use]
    pub(crate) fn live_app_on(screen: LiveScreen, load: ScreenLoad, help: bool) -> App {
        live_app(screen, load, help).0
    }

    /// A live [`App`] on the default [`LiveScreen::Chain`] screen bound to a source
    /// with the given `capabilities`, dropping the command receiver — for the
    /// reachability-skip / number-key-hint tests, which need a provider that lacks
    /// a capability (e.g. depth).
    #[must_use]
    pub(crate) fn live_app_caps(capabilities: ProviderCapabilities) -> App {
        let (tx, _rx) = mpsc::channel::<Command>(8);
        let live = LiveState::new(
            SourceBinding::new(pid("deribit"), capabilities, StreamHealth::Live),
            store(),
        );
        let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
        app.mark_drawn();
        app
    }

    /// A replay [`App`] in a given state, dropping the command receiver.
    #[must_use]
    pub(crate) fn replay_app_on(screen: ReplayScreen, help: bool) -> App {
        replay_app(screen, help).0
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use optionstratlib::OptionStyle;
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use tokio::sync::mpsc;

    use super::{
        App, BundleLoad, HINT_TICKS, KeyRoute, LiveScreen, LiveState, Mode, OverlayBinding,
        Playback, ReplayScreen, ReplayState, ScreenLoad, SourceBinding, is_screen_reachable,
    };
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSnapshot, ChainSource, ChainStore, ExpirySource, Instrument,
        MarketUpdate, ProviderId, QuoteUpdate, StreamHealth,
    };
    use crate::chain::{ContractSpecFingerprint, ExerciseStyle, InstrumentKey, SettlementStyle};
    use crate::config::ThemeChoice;
    use crate::event::{AppEvent, Command, SeekTo};
    use crate::providers::{
        ChainCapability, ChainPollCapability, GreeksCapability, ProviderCapabilities,
    };

    const EXP: i64 = 1_700_000_000;

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> chrono::DateTime<chrono::Utc> {
        match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
            Some(t) => t,
            None => panic!("invalid test timestamp: {secs}"),
        }
    }

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    fn refresh() -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn spec() -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: 1,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn ikey(strike: f64, style: OptionStyle) -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(EXP),
            strike: pos(strike),
            style,
        }
    }

    fn instrument(provider: &str, strike: f64, style: OptionStyle) -> Instrument {
        Instrument {
            key: ikey(strike, style),
            provider: pid(provider),
            native_symbol: format!("BTC-{strike}-{}", style.as_str()),
            stream_symbol: None,
            spec: spec(),
        }
    }

    fn quote(
        provider: &str,
        strike: f64,
        style: OptionStyle,
        bid: Option<f64>,
        ask: Option<f64>,
        received: i64,
    ) -> QuoteUpdate {
        QuoteUpdate {
            instrument: instrument(provider, strike, style),
            bid: bid.map(pos),
            ask: ask.map(pos),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: None,
            received_time: utc(received),
        }
    }

    fn row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn chain_with(strikes: &[f64]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for strike in strikes {
            let _ = chain.options.insert(row(*strike));
        }
        chain
    }

    fn fetch(strikes: &[f64], provider: &str) -> ChainFetch {
        ChainFetch::new(
            chain_with(strikes),
            ExpirySource::new("BTC", utc(EXP), pid(provider)),
            AliasCatalog::new(),
        )
    }

    fn store(strikes: &[f64], provider: &str) -> ChainStore {
        ChainStore::seed(
            fetch(strikes, provider),
            ChainSource::Merged,
            refresh(),
            utc(EXP),
        )
    }

    /// Capabilities for a Deribit-like source: assembled chain, depth, provided
    /// Greeks.
    fn full_caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(true)
            .greeks(GreeksCapability::Provided)
            .chain_poll(ChainPollCapability::Poll {
                interval_hint_secs: 2,
            })
            .build()
    }

    fn source_binding(provider: &str, caps: ProviderCapabilities) -> SourceBinding {
        SourceBinding::new(pid(provider), caps, StreamHealth::Live)
    }

    /// A live app over a seeded store, plus the receiver half of its command
    /// channel so tests can assert on emitted [`Command`]s.
    fn live_app(caps: ProviderCapabilities) -> (App, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel::<Command>(8);
        let live = LiveState::new(
            source_binding("deribit", caps),
            store(&[60_000.0], "deribit"),
        );
        let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
        app.mark_drawn();
        (app, rx)
    }

    fn replay_app(dir: &str) -> (App, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel::<Command>(8);
        let mut app = App::new(
            Mode::Replay(ReplayState::new(PathBuf::from(dir))),
            ThemeChoice::Auto,
            tx,
        );
        app.mark_drawn();
        (app, rx)
    }

    #[track_caller]
    fn live(app: &App) -> &LiveState {
        match &app.mode {
            Mode::Live(live) => live,
            Mode::Replay(_) => panic!("expected a live app"),
        }
    }

    #[track_caller]
    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn test_send_command_on_a_closed_channel_is_surfaced_not_silently_dropped() {
        let (app, rx) = live_app(full_caps());
        let mut app = app;
        assert_eq!(app.commands_dropped, 0, "healthy steady state");
        // Close the channel: the data layer is gone.
        drop(rx);
        app.send_command(Command::Reconnect);
        assert_eq!(
            app.commands_dropped, 1,
            "a dropped recovery command must be counted, never silently swallowed"
        );
        app.send_command(Command::Rediscover);
        assert_eq!(app.commands_dropped, 2);
    }

    // --- dirty: mutating vs idle ---------------------------------------------

    #[test]
    fn test_app_new_starts_dirty_so_first_frame_draws() {
        let (tx, _rx) = mpsc::channel::<Command>(8);
        let live = LiveState::new(
            source_binding("deribit", full_caps()),
            store(&[60_000.0], "deribit"),
        );
        let app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
        assert!(app.dirty);
    }

    #[test]
    fn test_on_event_tick_idle_does_not_set_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        // A ready chain on a live feed is a non-motion (idle) state; a tick must not
        // force a redraw so the render loop parks.
        match &mut app.mode {
            Mode::Live(live) => live.load = ScreenLoad::Ready,
            Mode::Replay(_) => panic!("expected a live app"),
        }
        app.on_event(AppEvent::Tick);
        assert!(
            !app.dirty,
            "an idle non-motion tick must not force a redraw"
        );
    }

    #[test]
    fn test_on_event_tick_in_motion_sets_dirty_to_animate_spinner() {
        // The default live state is `Loading` (a motion state, the initial connect),
        // so a tick must set `dirty` to advance the spinner — otherwise it freezes
        // and reads as a hang (`docs/05-views-and-ux.md` §7).
        let (mut app, _rx) = live_app(full_caps());
        assert!(matches!(
            &app.mode,
            Mode::Live(live) if matches!(live.load, ScreenLoad::Loading)
        ));
        app.on_event(AppEvent::Tick);
        assert!(app.dirty, "a motion-state tick must redraw the spinner");
    }

    #[test]
    fn test_status_hint_decays_after_n_ticks_not_immediately() {
        // A flashed hint stays visible for a readable minimum: it survives the next
        // tick (no near-zero flash) and clears only once its lifetime elapses.
        let (mut app, _rx) = live_app(full_caps());
        // Put the app in a non-motion state so only the hint drives redraws here.
        match &mut app.mode {
            Mode::Live(live) => live.load = ScreenLoad::Ready,
            Mode::Replay(_) => panic!("expected a live app"),
        }
        app.set_status_hint("Depth not available on deribit".to_owned());
        // One tick: the hint is still showing and an intermediate countdown tick does
        // not force a redraw.
        app.mark_drawn();
        app.on_event(AppEvent::Tick);
        assert!(
            app.status_hint.is_some(),
            "the hint must survive at least one tick, not flash near-zero"
        );
        assert!(!app.dirty, "an intermediate countdown tick does not redraw");
        // Keep ticking (clearing dirty each time) until the hint decays; the tick that
        // clears it dirties the frame so the keybar redraws without the hint.
        let mut cleared = false;
        for _ in 0..u32::from(HINT_TICKS) + 2 {
            app.mark_drawn();
            app.on_event(AppEvent::Tick);
            if app.status_hint.is_none() {
                assert!(app.dirty, "the decaying tick that clears the hint redraws");
                cleared = true;
                break;
            }
        }
        assert!(cleared, "the hint decays within its lifetime");
    }

    #[test]
    fn test_on_event_resize_sets_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        app.on_event(AppEvent::Resize(100, 30));
        assert!(app.dirty);
    }

    #[test]
    fn test_mark_drawn_clears_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        app.on_event(AppEvent::Resize(100, 30));
        assert!(app.dirty);
        app.mark_drawn();
        assert!(!app.dirty);
    }

    // --- Market fold: quote / chain / health ---------------------------------

    #[test]
    fn test_on_event_market_quote_applied_folds_into_store_and_sets_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        let update = MarketUpdate::Quote(quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.4),
            Some(1.6),
            EXP + 100,
        ));
        app.on_event(AppEvent::Market(update));
        assert!(app.dirty);
        // The store row patched to the new quote.
        let patched = live(&app)
            .store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(60_000.0))
            .map(|o| o.call_bid);
        assert_eq!(patched, Some(Some(pos(1.4))));
    }

    #[test]
    fn test_on_event_market_quote_dropped_crossed_does_not_set_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        // ask < bid on the seeded strike -> DroppedCrossed -> no visible change.
        let update = MarketUpdate::Quote(quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(2.0),
            Some(1.5),
            EXP + 100,
        ));
        app.on_event(AppEvent::Market(update));
        assert!(!app.dirty);
    }

    #[test]
    fn test_on_event_market_quote_unknown_strike_buffered_does_not_set_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        // A strike absent from the chain is buffered pending a poll, not rendered.
        let update = MarketUpdate::Quote(quote(
            "deribit",
            99_000.0,
            OptionStyle::Call,
            Some(1.0),
            Some(1.2),
            EXP + 100,
        ));
        app.on_event(AppEvent::Market(update));
        assert!(!app.dirty);
    }

    #[test]
    fn test_on_event_market_chain_snapshot_applies_poll_and_sets_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        // A re-poll introducing a new strike (61000) and dropping the old (60000).
        let snapshot = ChainSnapshot {
            chain_key: (pid("deribit"), "BTC".to_owned(), utc(EXP)),
            chain: chain_with(&[61_000.0]),
            aliases: AliasCatalog::new(),
            source: ChainSource::Merged,
            health: StreamHealth::Live,
            last_full_poll: Some(utc(EXP + 200)),
        };
        app.on_event(AppEvent::Market(MarketUpdate::Chain(snapshot)));
        assert!(app.dirty);
        assert!(live(&app).store.contains_strike(pos(61_000.0)));
        assert!(!live(&app).store.contains_strike(pos(60_000.0)));
    }

    #[test]
    fn test_on_event_market_chain_snapshot_without_poll_time_is_noop() {
        let (mut app, _rx) = live_app(full_caps());
        let snapshot = ChainSnapshot {
            chain_key: (pid("deribit"), "BTC".to_owned(), utc(EXP)),
            chain: chain_with(&[61_000.0]),
            aliases: AliasCatalog::new(),
            source: ChainSource::Merged,
            health: StreamHealth::Live,
            last_full_poll: None,
        };
        app.on_event(AppEvent::Market(MarketUpdate::Chain(snapshot)));
        assert!(!app.dirty);
        // Structure unchanged: the original strike remains, the new one absent.
        assert!(live(&app).store.contains_strike(pos(60_000.0)));
        assert!(!live(&app).store.contains_strike(pos(61_000.0)));
    }

    // --- ATM cache: computed off-draw at construction and refreshed on a poll --

    #[test]
    fn test_atm_index_of_finds_nearest_strike() {
        let chain = chain_with(&[50_000.0, 59_000.0, 70_000.0]);
        // spot 60000 -> nearest is 59000 at index 1.
        assert_eq!(super::atm_index_of(&chain), Some(1));
        assert_eq!(
            super::atm_index_of(&OptionChain::new(
                "BTC",
                pos(1.0),
                "x".to_owned(),
                None,
                None
            )),
            None
        );
    }

    #[test]
    fn test_live_state_caches_atm_index_and_refreshes_on_poll() {
        // The cache is seeded off-draw at construction and only recomputed when a
        // poll changes the ladder — draw reads it without rescanning the strikes.
        let (tx, _rx) = mpsc::channel::<Command>(8);
        let state = LiveState::new(
            source_binding("deribit", full_caps()),
            store(&[58_000.0, 60_000.0, 62_000.0], "deribit"),
        );
        // spot 60000 -> nearest is 60000 at index 1.
        assert_eq!(state.atm_index(), Some(1), "seeded from the initial chain");
        let mut app = App::new(Mode::Live(state), ThemeChoice::Auto, tx);
        // A quote patches a row but never moves the ATM, so the cache is unchanged.
        app.on_event(AppEvent::Market(MarketUpdate::Quote(quote(
            "deribit",
            60_000.0,
            OptionStyle::Call,
            Some(1.4),
            Some(1.6),
            EXP + 100,
        ))));
        assert_eq!(
            live(&app).atm_index(),
            Some(1),
            "a quote does not move the ATM"
        );
        // A poll that re-lists strikes around a new spot refreshes the cache.
        let snapshot = ChainSnapshot {
            chain_key: (pid("deribit"), "BTC".to_owned(), utc(EXP)),
            chain: chain_with(&[59_000.0, 60_000.0]),
            aliases: AliasCatalog::new(),
            source: ChainSource::Merged,
            health: StreamHealth::Live,
            last_full_poll: Some(utc(EXP + 200)),
        };
        app.on_event(AppEvent::Market(MarketUpdate::Chain(snapshot)));
        // The new ladder [59000, 60000] with spot 60000 -> nearest is index 1.
        assert_eq!(
            live(&app).atm_index(),
            Some(1),
            "the poll refreshed the cached ATM index"
        );
    }

    #[test]
    fn test_on_event_market_depth_is_noop_without_a_store_path() {
        let (mut app, _rx) = live_app(full_caps());
        let ladder = crate::chain::DepthLadder {
            instrument: instrument("deribit", 60_000.0, OptionStyle::Call),
            bids: Vec::new(),
            asks: Vec::new(),
            event_time: None,
            received_time: utc(EXP + 100),
            change_id: None,
        };
        app.on_event(AppEvent::Market(MarketUpdate::Depth(ladder)));
        assert!(!app.dirty);
    }

    // --- Per-side health: source and overlay degrade independently -----------

    fn overlay_caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .greeks(GreeksCapability::Provided)
            .build()
    }

    fn live_app_with_overlay() -> (App, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel::<Command>(8);
        let live = LiveState::new(
            source_binding("deribit", full_caps()),
            store(&[60_000.0], "deribit"),
        )
        .with_overlay(OverlayBinding::new(
            pid("dxlink"),
            overlay_caps(),
            StreamHealth::Live,
        ));
        let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
        app.mark_drawn();
        (app, rx)
    }

    #[test]
    fn test_on_event_market_health_degrades_source_side_only() {
        let (mut app, _rx) = live_app_with_overlay();
        app.on_event(AppEvent::Market(MarketUpdate::Health(
            pid("deribit"),
            StreamHealth::Reconnecting { attempt: 2 },
        )));
        assert!(app.dirty);
        let live = live(&app);
        assert!(matches!(
            live.source.health,
            StreamHealth::Reconnecting { attempt: 2 }
        ));
        // The overlay side is untouched.
        match &live.overlay {
            Some(overlay) => assert!(matches!(overlay.health, StreamHealth::Live)),
            None => panic!("expected an overlay binding"),
        }
    }

    #[test]
    fn test_on_event_market_health_degrades_overlay_side_only() {
        let (mut app, _rx) = live_app_with_overlay();
        app.on_event(AppEvent::Market(MarketUpdate::Health(
            pid("dxlink"),
            StreamHealth::Stale { since: utc(EXP) },
        )));
        assert!(app.dirty);
        let live = live(&app);
        // The source side stays live; only the overlay degrades.
        assert!(matches!(live.source.health, StreamHealth::Live));
        match &live.overlay {
            Some(overlay) => assert!(matches!(overlay.health, StreamHealth::Stale { .. })),
            None => panic!("expected an overlay binding"),
        }
    }

    #[test]
    fn test_on_event_market_health_for_unknown_provider_is_noop() {
        let (mut app, _rx) = live_app(full_caps());
        app.on_event(AppEvent::Market(MarketUpdate::Health(
            pid("alpaca"),
            StreamHealth::Reconnecting { attempt: 1 },
        )));
        assert!(!app.dirty);
    }

    // --- I/O-needing keys emit the right Command -----------------------------

    #[test]
    fn test_on_key_r_emits_reconnect_command_in_live() {
        let (mut app, mut rx) = live_app(full_caps());
        app.on_event(key(KeyCode::Char('r')));
        match rx.try_recv() {
            Ok(Command::Reconnect) => {}
            other => panic!("expected Reconnect, got {other:?}"),
        }
    }

    #[test]
    fn test_on_key_shift_r_emits_rediscover_command_in_live() {
        let (mut app, mut rx) = live_app(full_caps());
        app.on_event(key(KeyCode::Char('R')));
        match rx.try_recv() {
            Ok(Command::Rediscover) => {}
            other => panic!("expected Rediscover, got {other:?}"),
        }
    }

    #[test]
    fn test_on_key_shift_r_emits_reload_bundle_in_replay() {
        let (mut app, mut rx) = replay_app("/bundle");
        app.on_event(key(KeyCode::Char('R')));
        match rx.try_recv() {
            Ok(Command::ReloadBundle(dir)) => assert_eq!(dir, PathBuf::from("/bundle")),
            other => panic!("expected ReloadBundle, got {other:?}"),
        }
    }

    #[test]
    fn test_on_key_r_in_replay_emits_no_command() {
        let (mut app, mut rx) = replay_app("/bundle");
        app.on_event(key(KeyCode::Char('r')));
        assert!(rx.try_recv().is_err(), "`r` has no replay binding");
    }

    #[test]
    fn test_on_replay_seek_emits_seek_bundle_command_in_replay() {
        let (mut app, mut rx) = replay_app("/bundle");
        app.on_event(AppEvent::ReplaySeek(SeekTo::Step(3)));
        match rx.try_recv() {
            Ok(Command::SeekBundle(SeekTo::Step(3))) => {}
            other => panic!("expected SeekBundle(Step(3)), got {other:?}"),
        }
    }

    #[test]
    fn test_on_replay_seek_in_live_is_noop() {
        let (mut app, mut rx) = live_app(full_caps());
        app.on_event(AppEvent::ReplaySeek(SeekTo::StepBy(1)));
        assert!(!app.dirty);
        assert!(
            rx.try_recv().is_err(),
            "a scrub is meaningless in live mode"
        );
    }

    // --- Quit / help ----------------------------------------------------------

    #[test]
    fn test_on_key_q_requests_quit_and_sets_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        app.on_event(key(KeyCode::Char('q')));
        assert!(app.should_quit);
        assert!(app.dirty);
    }

    #[test]
    fn test_on_key_ctrl_c_requests_quit() {
        let (mut app, _rx) = live_app(full_caps());
        app.on_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.should_quit);
    }

    #[test]
    fn test_on_key_question_toggles_help_and_sets_dirty() {
        let (mut app, _rx) = live_app(full_caps());
        assert!(!app.help_open);
        app.on_event(key(KeyCode::Char('?')));
        assert!(app.help_open);
        assert!(app.dirty);
        app.mark_drawn();
        app.on_event(key(KeyCode::Char('?')));
        assert!(!app.help_open);
        assert!(app.dirty);
    }

    // --- Two-level key dispatch: global route vs. forward-to-screen ----------

    #[track_caller]
    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_dispatch_key_global_bound_global_is_consumed() {
        let (mut app, _rx) = live_app(full_caps());
        // A bound global (`q`) is consumed at the global level, never forwarded.
        assert_eq!(
            app.dispatch_key_global(key_event(KeyCode::Char('q'))),
            KeyRoute::Consumed
        );
        assert!(app.should_quit);
    }

    #[test]
    fn test_dispatch_key_global_unbound_key_routes_to_screen() {
        let (mut app, _rx) = live_app(full_caps());
        // An unbound key (`j`, a chain-nav key owned by the screen) is forwarded.
        assert_eq!(
            app.dispatch_key_global(key_event(KeyCode::Char('j'))),
            KeyRoute::ToScreen
        );
        assert!(!app.dirty, "forwarding a key mutates no global state");
    }

    #[test]
    fn test_dispatch_key_global_modal_help_swallows_and_never_forwards() {
        let (mut app, _rx) = live_app(full_caps());
        assert_eq!(
            app.dispatch_key_global(key_event(KeyCode::Char('?'))),
            KeyRoute::Consumed
        );
        assert!(app.help_open);
        app.mark_drawn();
        // With help open, an unbound key is swallowed (Consumed), NOT forwarded to
        // the screen behind the overlay — the modal intercept.
        assert_eq!(
            app.dispatch_key_global(key_event(KeyCode::Char('j'))),
            KeyRoute::Consumed
        );
        assert!(app.help_open, "a swallowed key leaves the overlay open");
        assert!(!app.dirty, "a swallowed key mutates no state");
        // `Esc` closes the modal overlay.
        assert_eq!(
            app.dispatch_key_global(key_event(KeyCode::Esc)),
            KeyRoute::Consumed
        );
        assert!(!app.help_open);
        assert!(app.dirty);
    }

    #[test]
    fn test_dispatch_key_global_non_press_is_consumed_not_forwarded() {
        let (mut app, _rx) = live_app(full_caps());
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(app.dispatch_key_global(release), KeyRoute::Consumed);
    }

    #[test]
    fn test_dispatch_key_global_modal_swallows_out_of_vocab_key() {
        // A key OUTSIDE the keymap vocabulary (an F-key, PageUp, …) must be swallowed
        // while the overlay is modal — it must never reach the hidden screen behind
        // it (the intercept runs before the vocabulary check).
        let (mut app, _rx) = live_app(full_caps());
        assert_eq!(
            app.dispatch_key_global(key_event(KeyCode::Char('?'))),
            KeyRoute::Consumed
        );
        assert!(app.help_open);
        app.mark_drawn();
        for code in [
            KeyCode::F(5),
            KeyCode::PageUp,
            KeyCode::Insert,
            KeyCode::Delete,
        ] {
            assert_eq!(
                app.dispatch_key_global(key_event(code)),
                KeyRoute::Consumed,
                "an out-of-vocab key must be swallowed while modal, not forwarded"
            );
            assert!(app.help_open, "the overlay stays open");
            assert!(!app.dirty, "a swallowed key mutates no state");
        }
    }

    // --- Capability-driven reachability (no ProviderId match) ----------------

    #[test]
    fn test_is_screen_reachable_depth_gated_on_capability() {
        let with_depth = full_caps();
        assert!(is_screen_reachable(LiveScreen::Depth, &with_depth));
        let without_depth = ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .greeks(GreeksCapability::Provided)
            .build();
        assert!(!is_screen_reachable(LiveScreen::Depth, &without_depth));
        // Chain/Payoff still reachable; Surface still reachable (Greeks present).
        assert!(is_screen_reachable(LiveScreen::Chain, &without_depth));
        assert!(is_screen_reachable(LiveScreen::Payoff, &without_depth));
        assert!(is_screen_reachable(LiveScreen::Surface, &without_depth));
    }

    #[test]
    fn test_is_screen_reachable_surface_gated_on_greeks() {
        let no_greeks = ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .greeks(GreeksCapability::None)
            .build();
        assert!(!is_screen_reachable(LiveScreen::Surface, &no_greeks));
        assert!(is_screen_reachable(LiveScreen::Chain, &no_greeks));
    }

    #[test]
    fn test_set_screen_refuses_unreachable_and_keeps_prior() {
        let no_depth = ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .greeks(GreeksCapability::Provided)
            .depth(false)
            .build();
        let (mut app, _rx) = live_app(no_depth);
        let switched = match &mut app.mode {
            Mode::Live(live) => (live.set_screen(LiveScreen::Depth), live.screen),
            Mode::Replay(_) => panic!("expected live"),
        };
        assert!(!switched.0, "an unreachable screen switch is refused");
        assert_eq!(switched.1, LiveScreen::Chain, "the prior screen is kept");
    }

    #[test]
    fn test_set_screen_switches_to_reachable() {
        let (mut app, _rx) = live_app(full_caps());
        let (switched, screen) = match &mut app.mode {
            Mode::Live(live) => (live.set_screen(LiveScreen::Surface), live.screen),
            Mode::Replay(_) => panic!("expected live"),
        };
        assert!(switched);
        assert_eq!(screen, LiveScreen::Surface);
    }

    #[test]
    fn test_effective_capabilities_unions_overlay_depth_and_greeks() {
        // A source with neither depth nor Greeks, plus an overlay that adds
        // Greeks — the union makes the Surface screen reachable.
        let bare_source = ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(false)
            .greeks(GreeksCapability::None)
            .build();
        let overlay_with_greeks = ProviderCapabilities::builder()
            .depth(true)
            .greeks(GreeksCapability::ComputedLocally)
            .build();
        let live = LiveState::new(source_binding("ig", bare_source), store(&[60_000.0], "ig"))
            .with_overlay(OverlayBinding::new(
                pid("dxlink"),
                overlay_with_greeks,
                StreamHealth::Live,
            ));
        assert!(live.screen_reachable(LiveScreen::Surface));
        assert!(live.screen_reachable(LiveScreen::Depth));
        let effective = live.effective_capabilities();
        assert!(effective.depth);
        assert_eq!(effective.greeks, GreeksCapability::ComputedLocally);
    }

    // --- Compile fences: closed sets are matched wildcard-free ---------------

    #[test]
    fn test_mode_and_screen_matches_are_wildcard_free() {
        // These exhaustive, wildcard-free matches mirror the render/fan-in
        // discipline: adding a `Mode`, `LiveScreen`, or `ReplayScreen` variant
        // breaks THESE matches at compile time.
        fn mode_label(mode: &Mode) -> &'static str {
            match mode {
                Mode::Live(_) => "live",
                Mode::Replay(_) => "replay",
            }
        }
        fn live_label(screen: LiveScreen) -> &'static str {
            match screen {
                LiveScreen::Chain => "chain",
                LiveScreen::Depth => "depth",
                LiveScreen::Surface => "surface",
                LiveScreen::Payoff => "payoff",
            }
        }
        fn replay_label(screen: ReplayScreen) -> &'static str {
            match screen {
                ReplayScreen::Replay => "replay",
                ReplayScreen::Payoff => "payoff",
            }
        }
        let (app, _rx) = live_app(full_caps());
        assert_eq!(mode_label(&app.mode), "live");
        assert_eq!(live_label(LiveScreen::Chain), "chain");
        assert_eq!(replay_label(ReplayScreen::Replay), "replay");
    }

    #[test]
    fn test_out_of_mode_screen_pair_is_unrepresentable() {
        // A `Mode::Replay` owns a `ReplayScreen`, never a `LiveScreen`, so
        // `Replay` + `Chain` cannot be constructed — the type system, not a
        // runtime fallback, prevents it. This asserts the mode-scoped defaults;
        // the impossibility is proven by the fact that assigning a `LiveScreen`
        // to `ReplayState::screen` would not compile.
        let replay = ReplayState::new(PathBuf::from("/bundle"));
        assert_eq!(replay.screen, ReplayScreen::Replay);
        assert!(matches!(replay.bundle, BundleLoad::Loading));
        assert_eq!(replay.play, Playback::Paused);
    }

    #[test]
    fn test_screen_load_default_is_loading() {
        assert_eq!(ScreenLoad::default(), ScreenLoad::Loading);
        let error = ScreenLoad::Error {
            message: "no chain for BTC".to_owned(),
        };
        assert_ne!(error, ScreenLoad::Loading);
    }
}
