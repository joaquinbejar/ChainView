//! # ChainView
//!
//! Terminal UI for option chains, Greeks and volatility — real-time market
//! data and backtest replay, rendered in your terminal.
//!
//! **Status:** early development — the crate skeleton is in place. The first
//! runtime surface to land is the boundary error type [`ChainViewError`]; the
//! remaining modules are planned surfaces and carry no runtime behavior yet.
//! Follow progress at <https://github.com/joaquinbejar/ChainView>.

#![forbid(unsafe_code)]

pub(crate) mod app;
pub(crate) mod chain;
pub mod config;
pub(crate) mod error;
pub(crate) mod event;
pub(crate) mod providers;
pub(crate) mod terminal;
pub(crate) mod ui;

// The application state machine + fan-in (`docs/02-tui-architecture.md` §3, §4):
// the `App`, the `Live | Replay` `Mode`, the mode-scoped `LiveScreen`/
// `ReplayScreen`, the composite source/overlay bindings and per-screen state, and
// the capability-read reachability helper. Plus the open-provider entry points
// (`docs/02-tui-architecture.md` §11, ADR-0006): `ChainViewApp::builder()` and
// its `ChainViewAppBuilder`, so an external thin binary can register its own
// `Box<dyn Provider>` and `run()`. The application-owned `ProviderRegistry` is
// deliberately NOT re-exported — external code composes through the builder, and
// the UI never receives the registry. Public so the app builder (#12) and the
// render loop (#13) can name and drive them.
pub use app::{
    App, BridgeSenders, BundleLoad, COMMAND_CHANNEL_CAPACITY, CONTROL_CHANNEL_CAPACITY,
    ChainViewApp, ChainViewAppBuilder, DEFAULT_JOIN_BUDGET, EventBridge, ExitCause, ExitReporter,
    FinalTeardown, GuardTeardown, LiveScreen, LiveState, LoadedReplay, Mode, OverlayBinding,
    PayoffBuilder, Playback, ReplayScreen, ReplayState, ScreenLoad, Selection, SourceBinding,
    StatusLine, SupervisedTask, Supervisor, TaskExit, TokioTask, is_screen_reachable,
};
// The closed event set folded by the state machine and the render -> data
// command channel (`docs/02-tui-architecture.md` §4).
pub use event::{AppEvent, Command, SeekTo};

pub use chain::{
    AliasCatalog, CHAIN_STALE_SLACK, ChainFetch, ChainSnapshot, ChainSource, ChainStore,
    ContractSpecFingerprint, DIRECTION_DECAY, DepthLadder, DepthLevel, ExerciseStyle, ExpirySource,
    FEED_DELAY_WARN, Freshness, GREEKS_STALE_AFTER, GreeksOrigin, GreeksRow, Instrument,
    InstrumentKey, MAX_PENDING, MarketUpdate, MergeOutcome, ProviderId, QUOTE_STALE_AFTER,
    QuoteUpdate, RESERVED_PROVIDER_IDS, SettlementStyle, StreamHealth, TickDir, chain_stale_after,
    pending_ttl,
};
pub use config::{CliOverrides, Config, ModeSelect, ProviderSettings, ThemeChoice};
pub use error::{
    BundleError, ChainViewError, ConfigError, NormalizeKind, OverlayError, ProviderError, Redacted,
    RegistryError, TransportDetail, TransportKind,
};
// The PUBLIC, semver-governed provider port surface (`docs/03-data-providers.md`
// §2, §11.1): the trait, the capability self-declaration + its builder + every
// dimension enum, and the port helper types. The emitted domain types
// (`ChainFetch`/`ExpirySource`/`AliasCatalog`, `MarketUpdate`, `ProviderError`,
// `ProviderId`) are re-exported above from their home layers.
pub use providers::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
    Provider, ProviderCapabilities, ProviderCapabilitiesBuilder, SubscriptionHandle,
    SubscriptionRequest, UnderlyingRef,
};
// The terminal lifecycle surface (`docs/02-tui-architecture.md` §6, ADR-0001):
// the RAII restore guard and the panic-hook restore installer. Public so an
// external thin binary (ADR-0006) can drive the same guaranteed restore. These
// stay the stable restore entrypoints for hand-rolled external binaries and are
// intentionally NOT narrowed to `pub(crate)` once `ChainViewApp::builder().run()`
// (issue #11) owns the guard internally.
pub use terminal::{TerminalGuard, install_panic_hook};
// The domain speaks `optionstratlib`'s numeric vocabulary
// (`docs/01-domain-model.md` §3–§4); re-export the two types that appear on the
// public identity surface so downstream callers can name them without depending
// on `optionstratlib` directly.
pub use optionstratlib::OptionStyle;
pub use optionstratlib::prelude::Positive;
