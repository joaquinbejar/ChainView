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

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

use crate::chain::{
    ChainFetch, ChainSnapshot, ChainStore, ExpirySource, MarketUpdate, MergeOutcome, ProviderId,
    StreamHealth,
};
use crate::config::ThemeChoice;
use crate::event::{AppEvent, Command, SeekTo};
use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};

mod bridge;
mod supervisor;

pub use bridge::{BridgeSenders, COMMAND_CHANNEL_CAPACITY, CONTROL_CHANNEL_CAPACITY, EventBridge};
pub use supervisor::{
    DEFAULT_JOIN_BUDGET, ExitCause, ExitReporter, FinalTeardown, GuardTeardown, SupervisedTask,
    Supervisor, TaskExit, TokioTask,
};

// ---------------------------------------------------------------------------
// App: the top-level state the render loop reads (§3).
// ---------------------------------------------------------------------------

/// All state the render loop reads, as a `Live | Replay` [`Mode`] state machine
/// (`docs/02-tui-architecture.md` §3).
///
/// Owned single-threaded by the synchronous render loop; every mutation happens
/// on an [`AppEvent`] through [`on_event`](App::on_event), never in `draw`.
#[derive(Debug)]
pub struct App {
    /// The active mode, which owns the mode-scoped active screen.
    pub mode: Mode,
    /// The color-theme selection. The **resolved** palette (auto-detection,
    /// `NO_COLOR` fallback) is computed by the theme layer (#14); this stores the
    /// user's selection so that resolution has an input.
    pub theme: ThemeChoice,
    /// The status-bar model (provider health, clock, mode). Its fields are
    /// populated by the render loop / status line (#13/#14); a documented stub
    /// here so [`App`] is constructible now.
    pub status: StatusLine,
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
            status: StatusLine::default(),
            help_open: false,
            should_quit: false,
            dirty: true,
            tx_command,
            commands_dropped: 0,
        }
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
        // On some platforms crossterm reports `Release`/`Repeat` in addition to
        // `Press`; act on the press only so a key never fires twice.
        if key.kind != KeyEventKind::Press {
            return;
        }
        // `Ctrl-C` is a hard quit regardless of the character key mapping.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        // `KeyCode` is crossterm's OPEN key vocabulary, not a ChainView closed
        // set, so a catch-all for keys this skeleton does not bind is correct
        // here — the full keybinding map and per-screen key forwarding land in
        // #14. The ChainView closed sets (`AppEvent` / `Mode` / `LiveScreen` /
        // `ReplayScreen`) stay wildcard-free.
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.toggle_help(),
            KeyCode::Char('r') => self.request_reconnect(),
            KeyCode::Char('R') => self.request_rediscover(),
            _ => {}
        }
    }

    fn on_resize(&mut self, _width: u16, _height: u16) {
        // A resize forces a redraw; the new geometry is read from `frame.area()`
        // in the render loop (#13), so nothing is stored here.
        self.dirty = true;
    }

    fn on_tick(&mut self) {
        // A tick sets `dirty` only when something time-dependent changed — a
        // fading `stale` badge, the status clock, a replay play-head advancing.
        // Those animated surfaces land with the render loop / status line
        // (#13/#14); until then a tick is a no-op and does not force a redraw
        // (§8), so an idle app parks on `rx_events.recv()`.
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
        Self {
            source,
            overlay: None,
            screen: LiveScreen::Chain,
            store,
            selection: Selection::default(),
            payoff_builder: PayoffBuilder::new(),
            load: ScreenLoad::Loading,
        }
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
        match update {
            MarketUpdate::Quote(quote) => merged(self.store.apply_quote(&quote)),
            MarketUpdate::Greeks(greeks) => merged(self.store.apply_greeks(&greeks)),
            MarketUpdate::Depth(_) => false,
            MarketUpdate::Chain(snapshot) => self.apply_chain_snapshot(snapshot),
            MarketUpdate::Health(provider, health) => self.apply_health(&provider, health),
        }
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
        if let Some(overlay) = &mut self.overlay {
            if *provider == overlay.provider {
                overlay.health = health;
                changed = true;
            }
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

/// The focused underlying/expiry/strike selection (`docs/02-tui-architecture.md`
/// §3). A minimal, typed anchor for #9; row/expiry navigation and scroll land
/// with the chain screen (#018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    /// The focused strike-row index within the visible chain, or `None` before
    /// any row is focused. Draw reads it with `.get()` + a fallback — never an
    /// unchecked index.
    pub focused_row: Option<usize>,
}

/// The multi-leg payoff-builder state (`docs/05-views-and-ux.md` §3). A
/// documented stub whose internals land in v0.2; `#[non_exhaustive]` so those
/// fields are a source-compatible addition.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PayoffBuilder;

impl PayoffBuilder {
    /// An empty payoff builder.
    #[must_use]
    pub fn new() -> Self {
        Self
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::OptionStyle;
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use tokio::sync::mpsc;

    use super::{
        App, BundleLoad, LiveScreen, LiveState, Mode, OverlayBinding, Playback, ReplayScreen,
        ReplayState, ScreenLoad, SourceBinding, is_screen_reachable,
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
        app.on_event(AppEvent::Tick);
        assert!(!app.dirty, "an idle tick must not force a redraw");
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
