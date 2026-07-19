//! The single-source keybinding map — pure data + resolution, no rendering
//! (`docs/05-views-and-ux.md` §3).
//!
//! # One map, read by both dispatch and the overlay
//!
//! [`KEYMAP`] is the **single source of truth** for every key ChainView acts on.
//! It lives in the **application** layer (this module) so both the key dispatch
//! ([`resolve_global`], read by
//! [`App::dispatch_key_global`](crate::App::dispatch_key_global), and
//! [`resolve_replay`], read by the replay screen) and the help overlay
//! (`src/ui/theme.rs`, which imports [`help_sections`]) read one table — a bound
//! key and its documentation **cannot drift** (proved by the cross-check test
//! below). The overlay renderer depends on this module (`ui → application`), never
//! the reverse: this module holds no `ratatui` type. Global keys work in every
//! screen; screen-local keys are additive.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{LiveScreen, Mode, ReplayScreen};

// ---------------------------------------------------------------------------
// Key chords + contexts.
// ---------------------------------------------------------------------------

/// A normalized, comparable, displayable key chord (`docs/05-views-and-ux.md`
/// §3). The keymap matches on these rather than raw crossterm events, so a binding
/// is provider- and platform-independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyChord {
    /// A plain character key (space is `Char(' ')`, labelled `Space`).
    Char(char),
    /// A `Ctrl`-modified character key, lower-cased (`Ctrl-c`).
    Ctrl(char),
    /// The up arrow.
    Up,
    /// The down arrow.
    Down,
    /// The left arrow.
    Left,
    /// The right arrow.
    Right,
    /// The Enter/Return key.
    Enter,
    /// The Escape key.
    Esc,
    /// The Tab key.
    Tab,
    /// Shift-Tab (crossterm's `BackTab`), labelled `S-Tab`.
    BackTab,
    /// The Home key.
    Home,
    /// The End key.
    End,
}

impl KeyChord {
    /// Normalize a crossterm [`KeyEvent`] into a [`KeyChord`], or `None` for a key
    /// outside the keymap vocabulary.
    ///
    /// [`KeyCode`] is crossterm's **open, `#[non_exhaustive]`** vocabulary, so the
    /// catch-all is required and correct here — an unmapped key normalizes to
    /// `None` and the dispatch forwards it (or ignores it).
    #[must_use]
    pub fn from_event(key: KeyEvent) -> Option<Self> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = key.code
        {
            return Some(Self::Ctrl(c.to_ascii_lowercase()));
        }
        match key.code {
            KeyCode::Char(c) => Some(Self::Char(c)),
            KeyCode::Up => Some(Self::Up),
            KeyCode::Down => Some(Self::Down),
            KeyCode::Left => Some(Self::Left),
            KeyCode::Right => Some(Self::Right),
            KeyCode::Enter => Some(Self::Enter),
            KeyCode::Esc => Some(Self::Esc),
            KeyCode::Tab => Some(Self::Tab),
            KeyCode::BackTab => Some(Self::BackTab),
            KeyCode::Home => Some(Self::Home),
            KeyCode::End => Some(Self::End),
            _ => None,
        }
    }
}

/// The context a binding is active in (`docs/05-views-and-ux.md` §3). `Global`
/// keys work everywhere; `Any` documents keys (like `Esc`) that mean the same in
/// every screen; the mode-scoped variants are screen-local and additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// Works in every screen, resolved at the global dispatch level.
    Global,
    /// Documented as active in any screen (e.g. `Esc` — ascend / close overlay).
    Any,
    /// Active only on the given live screen.
    Live(LiveScreen),
    /// Active only on the given replay screen.
    Replay(ReplayScreen),
}

// ---------------------------------------------------------------------------
// Actions.
// ---------------------------------------------------------------------------

/// The semantic action a binding performs, grouped by scope so each scope's
/// resolution stays exhaustive and wildcard-free (`docs/05-views-and-ux.md` §3).
///
/// Actions whose bodies land in a later issue (chain nav #18, surface/payoff v0.2,
/// depth v0.5, most replay controls v0.3) are declared here so the map documents
/// them; the map is the source of truth and a later issue wires the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A global action (quit / help / screen switch / cycle / reconnect / reload).
    Global(GlobalAction),
    /// A chain-screen action.
    Chain(ChainAction),
    /// A depth-screen action.
    Depth(DepthAction),
    /// A surface-screen action.
    Surface(SurfaceAction),
    /// A payoff-screen action.
    Payoff(PayoffAction),
    /// A replay-screen action.
    Replay(ReplayAction),
    /// Ascend focus / close overlay (`Esc`, context [`Context::Any`]).
    Ascend,
}

/// A global-level action declared in the map (`docs/05-views-and-ux.md` §3). The
/// concrete screen-switch slot is derived from the pressed digit at resolution
/// time (see [`resolve_global`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GlobalAction {
    /// Quit (terminal restored on the way out).
    Quit,
    /// Toggle the modal help overlay.
    ToggleHelp,
    /// Switch to the screen the pressed number key selects (reachable only).
    SwitchScreen,
    /// Cycle to the next reachable screen.
    NextScreen,
    /// Cycle to the previous reachable screen.
    PrevScreen,
    /// Refresh / reconnect the active provider (Live).
    Reconnect,
    /// Mode reload — re-open the bundle (Replay) / re-discover the provider (Live).
    Rediscover,
}

/// The resolved global command, with the screen-switch slot bound
/// (`docs/05-views-and-ux.md` §3). `App::dispatch_key_global` matches this
/// exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalCommand {
    /// Quit.
    Quit,
    /// Toggle the help overlay.
    ToggleHelp,
    /// Switch to the screen bound to slot `1..=4` (reachable only).
    SwitchScreen(u8),
    /// Cycle to the next reachable screen.
    NextScreen,
    /// Cycle to the previous reachable screen.
    PrevScreen,
    /// Reconnect the active provider.
    Reconnect,
    /// Reload the mode (re-discover / re-open the bundle).
    Rediscover,
}

/// A chain-screen action (`docs/05-views-and-ux.md` §3); bodies land in #18.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChainAction {
    /// Move the strike selection (`↑↓` / `kj`).
    MoveStrike,
    /// Switch expiration (`←→` / `hl`).
    SwitchExpiry,
    /// Previous / next underlying (`[` / `]`).
    SwitchUnderlying,
    /// Focus the call / put leg of the cursor strike (`c` / `p`).
    FocusLeg,
    /// Drill into the selected strike (`Enter`).
    Drill,
    /// Add the focused leg to the payoff builder (`a`).
    AddLeg,
}

/// A depth-screen action (`docs/05-views-and-ux.md` §3); body lands in v0.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DepthAction {
    /// Scroll the depth ladder (`↑↓`).
    Scroll,
}

/// A surface-screen action (`docs/05-views-and-ux.md` §3); bodies land in v0.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SurfaceAction {
    /// Cycle the Greek axis (`g` / `G`).
    CycleGreek,
    /// Toggle smile / single-expiry surface (`x`).
    ToggleView,
}

/// A payoff-screen action (`docs/05-views-and-ux.md` §3); bodies land in v0.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PayoffAction {
    /// Add a leg at the cursor strike/side (`a`).
    AddLeg,
    /// Remove the leg under the cursor (`x`).
    RemoveLeg,
    /// Increase / decrease the cursor leg's quantity (`+` / `-`).
    Quantity,
    /// Toggle the cursor leg buy / sell (`s`).
    ToggleSide,
    /// Commit the built strategy (`Enter`).
    Commit,
    /// Cancel the builder (`Esc`).
    Cancel,
    /// Toggle expiration / t+0 curve (`t`).
    ToggleCurve,
}

/// A replay-screen action (`docs/05-views-and-ux.md` §3). The scrub, end-jump,
/// play/pause, and speed actions have bodies now (via `AppEvent::ReplaySeek` /
/// `AppEvent::ReplayControl`, #34); the fill drill-down (`,` / `.`) lands with the
/// drill-down render (#35+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReplayAction {
    /// Play / pause the timeline (`Space`).
    PlayPause,
    /// Playback speed slower (`-`).
    SpeedSlower,
    /// Playback speed faster (`+`).
    SpeedFaster,
    /// Step the scrub head back one step (`←` / `h`).
    StepBack,
    /// Step the scrub head forward one step (`→` / `l`).
    StepForward,
    /// Previous fill (drill-down, `,`).
    PrevFill,
    /// Next fill (drill-down, `.`).
    NextFill,
    /// Jump to the start of the run (`Home`).
    JumpStart,
    /// Jump to the end of the run (`End`).
    JumpEnd,
}

// ---------------------------------------------------------------------------
// The map.
// ---------------------------------------------------------------------------

/// One row of the keybinding map: its context, its semantic action, the chords
/// that trigger it, the label the overlay/keybar show, and its help text
/// (`docs/05-views-and-ux.md` §3).
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    /// The context the binding is active in.
    pub context: Context,
    /// The semantic action.
    pub action: Action,
    /// The chords that trigger the action (for dispatch + cross-check).
    pub chords: &'static [KeyChord],
    /// The human label shown in the overlay/keybar (e.g. `↑↓ / kj`).
    pub keys_label: &'static str,
    /// The one-line description shown in the overlay.
    pub help: &'static str,
    /// `Some(version)` when the key **resolves and is documented** but its body is
    /// **not yet wired** (its `handle_key` is a no-op pending later I/O plumbing) —
    /// the help overlay renders a dim `(<version>)` suffix so `?` honestly advertises
    /// which advertised keys are not live yet, instead of presenting a dead key as a
    /// working feature (`docs/05-views-and-ux.md` §3, the honesty ethos). `None` for
    /// a live, wired key. This marks + documents only; it never changes resolution.
    pub deferred: Option<&'static str>,
}

/// The single source of truth for every key ChainView acts on
/// (`docs/05-views-and-ux.md` §3). Both the dispatch ([`resolve_global`],
/// [`resolve_replay`]) and the help overlay (`src/ui/theme.rs`, via
/// `help_sections`) read this one table, so a bound key and its documentation
/// cannot drift.
pub static KEYMAP: &[Binding] = &[
    // -- Global (all wired) ----------------------------------------------------
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::Quit),
        chords: &[KeyChord::Char('q'), KeyChord::Ctrl('c')],
        keys_label: "q / Ctrl-C",
        help: "Quit",
        deferred: None,
    },
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::ToggleHelp),
        chords: &[KeyChord::Char('?')],
        keys_label: "?",
        help: "Toggle help",
        deferred: None,
    },
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::SwitchScreen),
        chords: &[
            KeyChord::Char('1'),
            KeyChord::Char('2'),
            KeyChord::Char('3'),
            KeyChord::Char('4'),
        ],
        keys_label: "1-4",
        help: "Switch screen",
        deferred: None,
    },
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::NextScreen),
        chords: &[KeyChord::Tab],
        keys_label: "Tab",
        help: "Next screen",
        deferred: None,
    },
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::PrevScreen),
        chords: &[KeyChord::BackTab],
        keys_label: "S-Tab",
        help: "Previous screen",
        deferred: None,
    },
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::Reconnect),
        chords: &[KeyChord::Char('r')],
        keys_label: "r",
        help: "Reconnect provider",
        deferred: None,
    },
    Binding {
        context: Context::Global,
        action: Action::Global(GlobalAction::Rediscover),
        chords: &[KeyChord::Char('R')],
        keys_label: "R",
        help: "Reload / re-open",
        deferred: None,
    },
    // -- Any -------------------------------------------------------------------
    Binding {
        context: Context::Any,
        action: Action::Ascend,
        chords: &[KeyChord::Esc],
        keys_label: "Esc",
        help: "Ascend / close",
        deferred: None,
    },
    // -- Chain (MoveStrike/FocusLeg #18 + AddLeg #26 wired; the rest defer I/O) --
    Binding {
        context: Context::Live(LiveScreen::Chain),
        action: Action::Chain(ChainAction::MoveStrike),
        chords: &[
            KeyChord::Up,
            KeyChord::Down,
            KeyChord::Char('k'),
            KeyChord::Char('j'),
        ],
        keys_label: "↑↓ / kj",
        help: "Move strike",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Chain),
        action: Action::Chain(ChainAction::SwitchExpiry),
        chords: &[
            KeyChord::Left,
            KeyChord::Right,
            KeyChord::Char('h'),
            KeyChord::Char('l'),
        ],
        keys_label: "←→ / hl",
        help: "Switch expiration",
        deferred: Some("soon"),
    },
    Binding {
        context: Context::Live(LiveScreen::Chain),
        action: Action::Chain(ChainAction::SwitchUnderlying),
        chords: &[KeyChord::Char('['), KeyChord::Char(']')],
        keys_label: "[ ]",
        help: "Prev / next underlying",
        deferred: Some("soon"),
    },
    Binding {
        context: Context::Live(LiveScreen::Chain),
        action: Action::Chain(ChainAction::FocusLeg),
        chords: &[KeyChord::Char('c'), KeyChord::Char('p')],
        keys_label: "c / p",
        help: "Focus call / put leg",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Chain),
        action: Action::Chain(ChainAction::Drill),
        chords: &[KeyChord::Enter],
        keys_label: "Enter",
        help: "Drill into strike",
        deferred: Some("soon"),
    },
    Binding {
        context: Context::Live(LiveScreen::Chain),
        action: Action::Chain(ChainAction::AddLeg),
        chords: &[KeyChord::Char('a')],
        keys_label: "a",
        help: "Add leg to builder",
        deferred: None,
    },
    // -- Depth (body v0.5) -----------------------------------------------------
    Binding {
        context: Context::Live(LiveScreen::Depth),
        action: Action::Depth(DepthAction::Scroll),
        chords: &[KeyChord::Up, KeyChord::Down],
        keys_label: "↑↓",
        help: "Scroll ladder",
        deferred: Some("v0.5"),
    },
    // -- Surface (body v0.2) ---------------------------------------------------
    Binding {
        context: Context::Live(LiveScreen::Surface),
        action: Action::Surface(SurfaceAction::CycleGreek),
        chords: &[KeyChord::Char('g'), KeyChord::Char('G')],
        keys_label: "g / G",
        help: "Cycle Greek axis",
        deferred: Some("v0.2"),
    },
    Binding {
        context: Context::Live(LiveScreen::Surface),
        action: Action::Surface(SurfaceAction::ToggleView),
        chords: &[KeyChord::Char('x')],
        keys_label: "x",
        help: "Smile / surface",
        deferred: Some("v0.2"),
    },
    // -- Payoff (live, builder wired #26; the t+0 curve renders in #27) ---------
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::AddLeg),
        chords: &[KeyChord::Char('a')],
        keys_label: "a",
        help: "Add leg at cursor",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::RemoveLeg),
        chords: &[KeyChord::Char('x')],
        keys_label: "x",
        help: "Remove cursor leg",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::Quantity),
        chords: &[KeyChord::Char('+'), KeyChord::Char('-')],
        keys_label: "+ / -",
        help: "Quantity + / -",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::ToggleSide),
        chords: &[KeyChord::Char('s')],
        keys_label: "s",
        help: "Toggle buy / sell",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::Commit),
        chords: &[KeyChord::Enter],
        keys_label: "Enter",
        help: "Commit strategy",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::Cancel),
        chords: &[KeyChord::Esc],
        keys_label: "Esc",
        help: "Cancel builder",
        deferred: None,
    },
    Binding {
        context: Context::Live(LiveScreen::Payoff),
        action: Action::Payoff(PayoffAction::ToggleCurve),
        chords: &[KeyChord::Char('t')],
        keys_label: "t",
        help: "Expiration / t+0",
        deferred: None,
    },
    // -- Replay (scrub/end-jump/playback/speed wired #34; fill drill-down v0.3+) --
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::PlayPause),
        chords: &[KeyChord::Char(' ')],
        keys_label: "Space",
        help: "Play / pause",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::SpeedSlower),
        chords: &[KeyChord::Char('-')],
        keys_label: "-",
        help: "Slower (while playing)",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::SpeedFaster),
        chords: &[KeyChord::Char('+')],
        keys_label: "+",
        help: "Faster (while playing)",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::StepBack),
        chords: &[KeyChord::Left, KeyChord::Char('h')],
        keys_label: "← / h",
        help: "Step back",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::StepForward),
        chords: &[KeyChord::Right, KeyChord::Char('l')],
        keys_label: "→ / l",
        help: "Step forward",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::PrevFill),
        chords: &[KeyChord::Char(',')],
        keys_label: ",",
        help: "Previous fill",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::NextFill),
        chords: &[KeyChord::Char('.')],
        keys_label: ".",
        help: "Next fill",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::JumpStart),
        chords: &[KeyChord::Home],
        keys_label: "Home",
        help: "Jump to start",
        deferred: None,
    },
    Binding {
        context: Context::Replay(ReplayScreen::Replay),
        action: Action::Replay(ReplayAction::JumpEnd),
        chords: &[KeyChord::End],
        keys_label: "End",
        help: "Jump to end",
        deferred: None,
    },
];

// ---------------------------------------------------------------------------
// Dispatch resolution — both read the one map.
// ---------------------------------------------------------------------------

/// Resolve a chord against the **global** bindings into the [`GlobalCommand`] the
/// dispatch executes (`docs/05-views-and-ux.md` §3), or `None` when no global
/// binding matches (the dispatch then forwards the key to the active screen).
///
/// This is the read side `App::dispatch_key_global` uses, so the running dispatch
/// and the help overlay share the one map.
#[must_use]
pub fn resolve_global(chord: KeyChord) -> Option<GlobalCommand> {
    let binding = KEYMAP
        .iter()
        .find(|b| b.context == Context::Global && b.chords.contains(&chord))?;
    match binding.action {
        Action::Global(action) => resolve_global_command(action, chord),
        Action::Chain(_)
        | Action::Depth(_)
        | Action::Surface(_)
        | Action::Payoff(_)
        | Action::Replay(_)
        | Action::Ascend => None,
    }
}

/// Bind the concrete screen-switch slot (from the pressed digit) to a resolved
/// [`GlobalCommand`].
#[must_use]
fn resolve_global_command(action: GlobalAction, chord: KeyChord) -> Option<GlobalCommand> {
    match action {
        GlobalAction::Quit => Some(GlobalCommand::Quit),
        GlobalAction::ToggleHelp => Some(GlobalCommand::ToggleHelp),
        GlobalAction::NextScreen => Some(GlobalCommand::NextScreen),
        GlobalAction::PrevScreen => Some(GlobalCommand::PrevScreen),
        GlobalAction::Reconnect => Some(GlobalCommand::Reconnect),
        GlobalAction::Rediscover => Some(GlobalCommand::Rediscover),
        GlobalAction::SwitchScreen => slot_from_chord(chord).map(GlobalCommand::SwitchScreen),
    }
}

/// The screen-switch slot `1..=9` a digit chord selects, or `None` for a non-digit.
#[must_use]
fn slot_from_chord(chord: KeyChord) -> Option<u8> {
    let KeyChord::Char(c) = chord else {
        return None;
    };
    c.to_digit(10).and_then(|d| u8::try_from(d).ok())
}

/// Resolve a chord against the **chain** screen's bindings
/// (`docs/05-views-and-ux.md` §3), or `None` when no binding matches (the
/// dispatch then ignores the key, e.g. `Esc`, which is a [`Context::Any`] binding,
/// not a chain one).
///
/// This is the read side the chain screen's `handle_key` (`src/ui/chain.rs`) uses,
/// so the chain dispatch and the help overlay share the one map — a bound key and
/// its documentation cannot drift (the cross-check test below proves it). The concrete
/// direction (`↑` vs `↓`, `←` vs `→`, `[` vs `]`, `c` vs `p`) is read from the
/// resolved chord at dispatch time, the same way [`resolve_global`] derives the
/// screen-switch slot from the pressed digit.
#[must_use]
pub fn resolve_chain(chord: KeyChord) -> Option<ChainAction> {
    let binding = KEYMAP
        .iter()
        .find(|b| b.context == Context::Live(LiveScreen::Chain) && b.chords.contains(&chord))?;
    match binding.action {
        Action::Chain(action) => Some(action),
        Action::Global(_)
        | Action::Depth(_)
        | Action::Surface(_)
        | Action::Payoff(_)
        | Action::Replay(_)
        | Action::Ascend => None,
    }
}

/// Resolve a chord against the **payoff** screen's bindings
/// (`docs/05-views-and-ux.md` §3), or `None` when no binding matches (the dispatch
/// then ignores the key).
///
/// This is the read side the payoff screen's `handle_key` (`src/ui/payoff.rs`) uses,
/// so the builder dispatch and the help overlay share the one map — a bound key and
/// its documentation cannot drift. The concrete direction of the shared
/// [`PayoffAction::Quantity`] chord (`+` increment vs `-` decrement) is read from the
/// resolved chord at dispatch time, the same way [`resolve_global`] derives the
/// screen-switch slot from the pressed digit.
#[must_use]
pub fn resolve_payoff(chord: KeyChord) -> Option<PayoffAction> {
    let binding = KEYMAP
        .iter()
        .find(|b| b.context == Context::Live(LiveScreen::Payoff) && b.chords.contains(&chord))?;
    match binding.action {
        Action::Payoff(action) => Some(action),
        Action::Global(_)
        | Action::Chain(_)
        | Action::Depth(_)
        | Action::Surface(_)
        | Action::Replay(_)
        | Action::Ascend => None,
    }
}

/// Resolve a chord against a **replay** screen's bindings
/// (`docs/05-views-and-ux.md` §3), or `None` when no binding matches.
#[must_use]
pub fn resolve_replay(chord: KeyChord, screen: ReplayScreen) -> Option<ReplayAction> {
    let binding = KEYMAP
        .iter()
        .find(|b| b.context == Context::Replay(screen) && b.chords.contains(&chord))?;
    match binding.action {
        Action::Replay(action) => Some(action),
        Action::Global(_)
        | Action::Chain(_)
        | Action::Depth(_)
        | Action::Surface(_)
        | Action::Payoff(_)
        | Action::Ascend => None,
    }
}

// ---------------------------------------------------------------------------
// Overlay grouping — pure, read by the ui help-overlay renderer.
// ---------------------------------------------------------------------------

/// The bindings in a context, in map order.
#[must_use]
fn bindings_in(context: Context) -> Vec<&'static Binding> {
    KEYMAP.iter().filter(|b| b.context == context).collect()
}

/// The overlay sections for the active mode: the global keys first, then the
/// mode's screens — every screen and its keys, generated from the map so the
/// overlay cannot drift from the dispatch (`docs/05-views-and-ux.md` §3). Read by
/// the ui help-overlay renderer (`src/ui/theme.rs`).
#[must_use]
pub(crate) fn help_sections(mode: &Mode) -> Vec<(&'static str, Vec<&'static Binding>)> {
    let mut global = bindings_in(Context::Global);
    global.extend(bindings_in(Context::Any));
    match mode {
        Mode::Live(_) => {
            let mut sections = vec![("Global", global)];
            sections.push(("Chain", bindings_in(Context::Live(LiveScreen::Chain))));
            sections.push(("Depth", bindings_in(Context::Live(LiveScreen::Depth))));
            sections.push(("Surface", bindings_in(Context::Live(LiveScreen::Surface))));
            sections.push(("Payoff", bindings_in(Context::Live(LiveScreen::Payoff))));
            sections
        }
        Mode::Replay(_) => {
            // `r` (Reconnect) is a Live-only affordance — in replay it is a deliberate
            // no-op (there is no live provider; `R` reloads the bundle instead), so it
            // must NOT appear in the replay overlay or `?` would advertise a dead key
            // (fix SF-04). The binding stays in `KEYMAP` (dispatch reads one table);
            // this scopes only the DOCUMENTATION, keeping the overlay truthful in both
            // modes without a second source of truth.
            global.retain(|b| b.action != Action::Global(GlobalAction::Reconnect));
            let mut sections = vec![("Global", global)];
            sections.push(("Replay", bindings_in(Context::Replay(ReplayScreen::Replay))));
            sections.push((
                "Payoff (v0.5)",
                bindings_in(Context::Replay(ReplayScreen::Payoff)),
            ));
            sections
        }
    }
}

/// Every binding the help overlay shows for `mode`, flattened — the cross-check
/// surface a test uses to prove every dispatched key is documented.
#[must_use]
pub fn help_bindings(mode: &Mode) -> Vec<&'static Binding> {
    help_sections(mode)
        .into_iter()
        .flat_map(|(_, bindings)| bindings)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests_support::{live_app_on, replay_app_on};
    use crate::app::{LiveScreen, ReplayScreen, ScreenLoad};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // --- The map drives dispatch AND the overlay (anti-drift) ----------------

    #[test]
    fn test_resolve_global_every_global_chord_resolves_and_is_documented() {
        // Every chord in a Global-context binding resolves to a command (dispatch
        // acts on it) AND appears in the help overlay (documented). A key the
        // dispatch acts on but the overlay omits would be a defect.
        let live = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, false);
        let documented = help_bindings(&live.mode);
        for binding in KEYMAP.iter().filter(|b| b.context == Context::Global) {
            for &chord in binding.chords {
                assert!(
                    resolve_global(chord).is_some(),
                    "global chord {chord:?} does not resolve",
                );
                assert!(
                    documented.iter().any(|b| b.chords.contains(&chord)),
                    "global chord {chord:?} is dispatched but not in the overlay",
                );
            }
        }
    }

    #[test]
    fn test_resolve_chain_every_chain_chord_resolves_and_is_documented() {
        // Every chord in a chain-context binding resolves to a `ChainAction`
        // (`ui::chain::handle_key` acts on it) AND appears in the help overlay
        // (documented), so the chain dispatch and its documentation cannot drift.
        let live = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, false);
        let documented = help_bindings(&live.mode);
        for binding in KEYMAP
            .iter()
            .filter(|b| b.context == Context::Live(LiveScreen::Chain))
        {
            for &chord in binding.chords {
                assert!(
                    resolve_chain(chord).is_some(),
                    "chain chord {chord:?} does not resolve",
                );
                assert!(
                    documented.iter().any(|b| b.chords.contains(&chord)),
                    "chain chord {chord:?} is dispatched but not in the overlay",
                );
            }
        }
    }

    #[test]
    fn test_resolve_chain_ignores_non_chain_chords() {
        // A global chord (`q`) and the `Context::Any` `Esc` are not chain bindings,
        // so the chain screen forwards nothing for them.
        assert_eq!(resolve_chain(KeyChord::Char('q')), None);
        assert_eq!(resolve_chain(KeyChord::Esc), None);
        assert_eq!(resolve_chain(KeyChord::Char('z')), None);
        // A chain chord resolves to its action.
        assert_eq!(
            resolve_chain(KeyChord::Char('j')),
            Some(ChainAction::MoveStrike),
        );
        assert_eq!(
            resolve_chain(KeyChord::Char('c')),
            Some(ChainAction::FocusLeg)
        );
    }

    #[test]
    fn test_keymap_marks_deferred_chain_keys_not_the_wired_ones() {
        // The wired chain keys (MoveStrike/FocusLeg #18, AddLeg #26) carry no deferred
        // marker; the three not-yet-wired ones do, so the overlay advertises honestly
        // which keys are not live.
        let deferral = |action: ChainAction| -> Option<&'static str> {
            KEYMAP
                .iter()
                .find(|b| b.action == Action::Chain(action))
                .and_then(|b| b.deferred)
        };
        assert_eq!(
            deferral(ChainAction::MoveStrike),
            None,
            "MoveStrike is wired"
        );
        assert_eq!(deferral(ChainAction::FocusLeg), None, "FocusLeg is wired");
        assert_eq!(
            deferral(ChainAction::AddLeg),
            None,
            "AddLeg is wired in #26 (chain→a→builder)"
        );
        assert!(deferral(ChainAction::SwitchExpiry).is_some());
        assert!(deferral(ChainAction::SwitchUnderlying).is_some());
        assert!(deferral(ChainAction::Drill).is_some());
    }

    #[test]
    fn test_keymap_marks_replay_keys_all_wired() {
        // #34 wired the scrub, end-jump, play/pause, and speed keys; #35 wired the
        // fill drill-down (`,` / `.`), so no replay key carries a deferred marker any
        // more — the overlay advertises every replay key as live.
        let deferral = |action: ReplayAction| -> Option<&'static str> {
            KEYMAP
                .iter()
                .find(|b| b.action == Action::Replay(action))
                .and_then(|b| b.deferred)
        };
        assert_eq!(deferral(ReplayAction::StepBack), None);
        assert_eq!(deferral(ReplayAction::StepForward), None);
        assert_eq!(deferral(ReplayAction::JumpStart), None);
        assert_eq!(deferral(ReplayAction::JumpEnd), None);
        assert_eq!(deferral(ReplayAction::PlayPause), None);
        assert_eq!(deferral(ReplayAction::SpeedFaster), None);
        assert_eq!(deferral(ReplayAction::SpeedSlower), None);
        // #35 wired the fill drill-down render, so `,` / `.` are live now.
        assert_eq!(deferral(ReplayAction::PrevFill), None);
        assert_eq!(deferral(ReplayAction::NextFill), None);
    }

    #[test]
    fn test_help_reconnect_scoped_to_live_overlay_only() {
        // SF-04: `r` (Reconnect) is Live-only; the replay overlay must NOT advertise
        // it (it is a no-op there), while the live overlay still does. The binding
        // stays in KEYMAP for dispatch — this only scopes the documentation, so the
        // overlay is truthful in BOTH modes from the one source of truth.
        let live = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, false);
        let replay = replay_app_on(ReplayScreen::Replay, false);
        let is_reconnect = |b: &&Binding| b.action == Action::Global(GlobalAction::Reconnect);
        assert!(
            help_bindings(&live.mode).iter().any(is_reconnect),
            "the live overlay lists Reconnect",
        );
        assert!(
            !help_bindings(&replay.mode).iter().any(is_reconnect),
            "the replay overlay omits Reconnect (no-op in replay)",
        );
        // Single source of truth intact: `r` still resolves globally in the map.
        assert_eq!(
            resolve_global(KeyChord::Char('r')),
            Some(GlobalCommand::Reconnect),
        );
    }

    #[test]
    fn test_help_speed_keys_note_while_playing() {
        // SF-03: the `+`/`-` speed help labels state the while-playing condition, so
        // the overlay does not advertise a silent no-op while paused.
        let help_of = |action: ReplayAction| -> &'static str {
            KEYMAP
                .iter()
                .find(|b| b.action == Action::Replay(action))
                .map_or("", |b| b.help)
        };
        assert!(
            help_of(ReplayAction::SpeedFaster).contains("playing"),
            "faster notes while-playing",
        );
        assert!(
            help_of(ReplayAction::SpeedSlower).contains("playing"),
            "slower notes while-playing",
        );
    }

    #[test]
    fn test_keymap_deferred_keys_still_resolve_and_are_documented() {
        // Deferral marks + documents only — it never changes resolution: a deferred
        // chain key still resolves to its action AND stays in the overlay.
        let live = live_app_on(LiveScreen::Chain, ScreenLoad::Ready, false);
        let documented = help_bindings(&live.mode);
        assert_eq!(
            resolve_chain(KeyChord::Enter),
            Some(ChainAction::Drill),
            "a deferred key still resolves",
        );
        assert!(
            documented
                .iter()
                .any(|b| b.chords.contains(&KeyChord::Enter)
                    && b.context == Context::Live(LiveScreen::Chain)),
            "a deferred key stays documented",
        );
    }

    #[test]
    fn test_resolve_replay_every_replay_chord_resolves_and_is_documented() {
        let replay = replay_app_on(ReplayScreen::Replay, false);
        let documented = help_bindings(&replay.mode);
        for binding in KEYMAP
            .iter()
            .filter(|b| b.context == Context::Replay(ReplayScreen::Replay))
        {
            for &chord in binding.chords {
                assert!(
                    resolve_replay(chord, ReplayScreen::Replay).is_some(),
                    "replay chord {chord:?} does not resolve",
                );
                assert!(
                    documented.iter().any(|b| b.chords.contains(&chord)),
                    "replay chord {chord:?} is dispatched but not in the overlay",
                );
            }
        }
    }

    #[test]
    fn test_resolve_payoff_every_payoff_chord_resolves_and_is_documented() {
        // Every chord in a payoff-context binding resolves to a `PayoffAction`
        // (`ui::payoff::handle_key` acts on it) AND appears in the help overlay, so
        // the builder dispatch and its documentation cannot drift.
        let live = live_app_on(LiveScreen::Payoff, ScreenLoad::Ready, false);
        let documented = help_bindings(&live.mode);
        for binding in KEYMAP
            .iter()
            .filter(|b| b.context == Context::Live(LiveScreen::Payoff))
        {
            for &chord in binding.chords {
                assert!(
                    resolve_payoff(chord).is_some(),
                    "payoff chord {chord:?} does not resolve",
                );
                assert!(
                    documented.iter().any(|b| b.chords.contains(&chord)),
                    "payoff chord {chord:?} is dispatched but not in the overlay",
                );
            }
        }
    }

    #[test]
    fn test_resolve_payoff_quantity_shares_plus_and_minus_chords() {
        // Both `+` and `-` resolve to the one `Quantity` action; the screen reads the
        // concrete chord to decide increment vs decrement.
        assert_eq!(
            resolve_payoff(KeyChord::Char('+')),
            Some(PayoffAction::Quantity),
        );
        assert_eq!(
            resolve_payoff(KeyChord::Char('-')),
            Some(PayoffAction::Quantity),
        );
        assert_eq!(resolve_payoff(KeyChord::Enter), Some(PayoffAction::Commit));
        assert_eq!(resolve_payoff(KeyChord::Esc), Some(PayoffAction::Cancel));
        // A non-payoff chord does not resolve here.
        assert_eq!(resolve_payoff(KeyChord::Char('q')), None);
    }

    #[test]
    fn test_keymap_payoff_builder_keys_are_wired_not_deferred() {
        // The builder bodies land in #26, so the payoff keys carry no deferred
        // marker; the help overlay advertises them as live.
        for action in [
            PayoffAction::AddLeg,
            PayoffAction::RemoveLeg,
            PayoffAction::Quantity,
            PayoffAction::ToggleSide,
            PayoffAction::Commit,
            PayoffAction::Cancel,
            PayoffAction::ToggleCurve,
        ] {
            let deferred = KEYMAP
                .iter()
                .find(|b| b.action == Action::Payoff(action))
                .and_then(|b| b.deferred);
            assert_eq!(deferred, None, "payoff {action:?} is wired in #26");
        }
    }

    #[test]
    fn test_resolve_global_switch_screen_binds_the_pressed_slot() {
        assert_eq!(
            resolve_global(KeyChord::Char('2')),
            Some(GlobalCommand::SwitchScreen(2)),
        );
        assert_eq!(
            resolve_global(KeyChord::Tab),
            Some(GlobalCommand::NextScreen)
        );
        assert_eq!(
            resolve_global(KeyChord::BackTab),
            Some(GlobalCommand::PrevScreen),
        );
        assert_eq!(
            resolve_global(KeyChord::Char('q')),
            Some(GlobalCommand::Quit)
        );
        assert_eq!(
            resolve_global(KeyChord::Ctrl('c')),
            Some(GlobalCommand::Quit)
        );
    }

    #[test]
    fn test_resolve_global_unmapped_key_returns_none() {
        // A chain-nav key (`j`) is not a global binding, so it forwards to the
        // screen; `Esc` (context Any) is not a global command either.
        assert_eq!(resolve_global(KeyChord::Char('j')), None);
        assert_eq!(resolve_global(KeyChord::Esc), None);
        assert_eq!(resolve_global(KeyChord::Char('z')), None);
    }

    #[test]
    fn test_key_chord_from_event_normalizes_ctrl_and_named_keys() {
        assert_eq!(
            KeyChord::from_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(KeyChord::Ctrl('c')),
        );
        assert_eq!(
            KeyChord::from_event(press(KeyCode::Char('?'))),
            Some(KeyChord::Char('?')),
        );
        assert_eq!(
            KeyChord::from_event(press(KeyCode::BackTab)),
            Some(KeyChord::BackTab),
        );
        assert_eq!(KeyChord::from_event(press(KeyCode::Up)), Some(KeyChord::Up));
        assert!(KeyChord::from_event(press(KeyCode::F(5))).is_none());
    }
}
