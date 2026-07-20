//! # ChainView
//!
//! A [`ratatui`](https://docs.rs/ratatui) terminal UI for options traders:
//! real-time option chains, Greeks, and volatility surfaces (**Live** mode) and
//! IronCondor backtest result-bundle rendering (**Replay** mode). The market-data
//! clients and all the options math live upstream; this crate is the terminal
//! around them: provider adapters, normalization, and the render loop.
//!
//! `chainview` ships as **both a binary and a library**. The binary is the stock
//! terminal (`cargo install chainview`); the library exposes the
//! **semver-governed provider port**, so any developer can plug their own
//! market-data venue or broker into ChainView with no fork ([ADR-0006]).
//!
//! # The provider port: the external-integration surface
//!
//! The port an external adapter compiles against is the [`Provider`] trait, the
//! [`ProviderCapabilities`] self-declaration (built through its
//! [`builder`](ProviderCapabilities::builder)) with its dimension enums
//! ([`ChainCapability`] / [`GreeksCapability`] / [`OptionStreamCapability`] /
//! [`ChainPollCapability`] / [`AuthKind`]), and every normalized domain type the
//! trait emits: [`ChainFetch`] (with [`ExpirySource`] / [`AliasCatalog`]),
//! [`OptionChain`] / [`ExpirationDate`] (`optionstratlib`), [`UnderlyingRef`],
//! [`QuoteUpdate`], [`GreeksRow`], [`DepthLadder`], [`MarketUpdate`],
//! [`Instrument`] / [`InstrumentKey`] / [`ContractSpecFingerprint`],
//! [`SubscriptionRequest`], [`SubscriptionHandle`], [`MarketUpdateSink`],
//! [`ProviderError`], and [`ProviderId`]. Every one is re-exported from this crate
//! root — including the scalar field types the emitted values carry
//! ([`Positive`], [`Decimal`], [`OptionStyle`], [`ExpirationDate`], and the
//! [`DateTime`]`<`[`Utc`]`>` timestamps) — so an external adapter names each
//! port type through `chainview::` (`docs/03-data-providers.md` §11.1). Two
//! companion dependencies remain the adapter's own: `async_trait` (the trait
//! is `#[async_trait]`, so implementing it needs the macro) and
//! `optionstratlib` when the adapter *builds* an [`OptionChain`] itself.
//!
//! An external developer writes a thin binary that depends on `chainview` and
//! registers their adapter through the app builder:
//!
//! ```no_run
//! use async_trait::async_trait;
//! use chainview::{
//!     ChainFetch, ChainViewApp, ChainViewError, ExpirationDate, MarketUpdateSink,
//!     Provider, ProviderCapabilities, ProviderError, ProviderId, SubscriptionHandle,
//!     SubscriptionRequest, UnderlyingRef,
//! };
//!
//! struct MyBroker {
//!     id: ProviderId,
//! }
//!
//! #[async_trait]
//! impl Provider for MyBroker {
//!     fn id(&self) -> ProviderId {
//!         self.id.clone()
//!     }
//!
//!     fn capabilities(&self) -> ProviderCapabilities {
//!         // Declare EXACTLY what the upstream backs: the UI gates screens off
//!         // this, never off the id. Every dimension defaults to its least-capable
//!         // value, so adding a future optional dimension is a source-compatible
//!         // minor bump.
//!         ProviderCapabilities::builder().build()
//!     }
//!
//!     async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
//!         Ok(vec![UnderlyingRef::new("BTC")])
//!     }
//!
//!     async fn fetch_chain(
//!         &self,
//!         _underlying: &str,
//!         _expiration: &ExpirationDate,
//!     ) -> Result<ChainFetch, ProviderError> {
//!         // A chain-producing adapter assembles a normalized `ChainFetch` here;
//!         // an overlay-only feed returns `Unsupported`.
//!         Err(ProviderError::Unsupported("overlay-only: no chain discovery"))
//!     }
//!
//!     async fn subscribe(
//!         &self,
//!         _req: SubscriptionRequest,
//!         _sink: MarketUpdateSink,
//!     ) -> Result<SubscriptionHandle, ProviderError> {
//!         // Drive an adapter-owned reconnect loop that pushes normalized
//!         // `MarketUpdate`s into `_sink`; return a handle that cancels it.
//!         Ok(SubscriptionHandle::new(|| { /* cancel the upstream stream */ }))
//!     }
//! }
//!
//! fn main() -> Result<(), ChainViewError> {
//!     let broker = MyBroker { id: ProviderId::new("mybroker")? };
//!     ChainViewApp::builder()
//!         .with_builtins()   // the gate-clear bundled venues (Deribit)
//!         .register(broker)  // your own venue; the id is read from `provider.id()`
//!         .run() // a reserved/duplicate id is a typed startup error, never a panic
//! }
//! ```
//!
//! # What is semver-governed
//!
//! The port is a **public, semver-governed surface** (`docs/SEMVER.md`): a change
//! to the [`Provider`] trait signature or any port type is a **major** bump;
//! adding a new *optional* capability dimension is **minor**. That minor is
//! source-compatible only because [`ProviderCapabilities`] and its enums are
//! `#[non_exhaustive]` and an adapter builds them through
//! [`ProviderCapabilities::builder`], never a struct literal. An external adapter
//! pins a `chainview` major and compiles against a stable port for that major's
//! lifetime.
//!
//! # Reserved ids and configuration namespacing
//!
//! The six built-in ids in [`RESERVED_PROVIDER_IDS`]
//! (`deribit`/`tastytrade`/`dxlink`/`ig`/`alpaca`/`ibkr`) are reserved: an external
//! registration that reuses one is [`RegistryError::ReservedId`], and a duplicate
//! id is [`RegistryError::DuplicateId`] — both typed startup errors, never a
//! panic. Growing the reserved set later is a **major** bump (it can invalidate a
//! working external id) and is announced one minor ahead. Every provider —
//! built-in or external — reads its non-secret settings from `providers.<id>.*`
//! and its credentials from `CHAINVIEW_<ID>_*` (the id transliterated to a
//! shell-safe segment through a total bijection, `docs/07-configuration.md` §5.1);
//! the reserved-id rule guarantees an external provider can never shadow a
//! built-in's namespace.
//!
//! # Security boundary and scope
//!
//! An externally registered provider is **outside ChainView's credential audit
//! boundary** — its author owns its credential hygiene ([ADR-0006] §7,
//! `docs/SECURITY.md` §5). What ChainView still guarantees by construction is that
//! **its own code never logs what crosses the port**: the port carries only
//! normalized domain types (no credentials), and [`ProviderError`] is
//! structurally redaction-safe. Dynamic/plugin loading (`dlopen`) is **out of
//! scope for v1** — an adapter is a compile-time Rust dependency, not a loaded
//! object (Rust has no stable ABI).
//!
//! # Status
//!
//! Pre-1.0 and in active development: the public API — including the provider
//! port — may change until `v1.0.0`, after which the SemVer rules above are
//! binding. Follow progress at <https://github.com/joaquinbejar/ChainView>.
//!
//! [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md

#![forbid(unsafe_code)]

pub(crate) mod app;
pub(crate) mod chain;
pub mod config;
pub(crate) mod error;
pub(crate) mod event;
pub(crate) mod providers;
pub(crate) mod replay;
pub(crate) mod terminal;
pub(crate) mod ui;

// In-crate Part B integration tests (issue #22) that require `pub(crate)`
// internals a `tests/*.rs` (a separate crate seeing only the public API) cannot
// reach — the live-path golden render (assembled `ChainStore` merge + the
// `pub(crate)` chain-matrix `draw` + the recorded-fixture assembler + the
// `#[cfg(test)]` golden helper), the id-agnostic render-parity proof, and the
// draw-path no-I/O assertion. The public-surface faux-provider conformance and
// the layering arch test live under `tests/` (`docs/TESTING.md` §7).
#[cfg(test)]
mod tests_integration;

// In-crate replay-path integration tests + the committed replay render goldens
// (issue #37, the v0.3 acceptance gate). Like `tests_integration`, these live
// in-crate because they use the `pub(crate)` render-golden harness
// (`assert_golden`/`buffer_to_text`) and the `pub(crate)` two-level key dispatch —
// none on the semver-governed surface (`docs/TESTING.md` §7/§4).
#[cfg(test)]
mod tests_replay_integration;

// In-crate v0.4 acceptance-gate capability-matrix reconcile (issue #46): the
// single executable table asserting every bundled adapter's live
// `ProviderCapabilities` equals its documented `docs/03-data-providers.md` §8 row
// (cell by cell), with the gated adapters checked under their feature and the
// IG-deferred row marked N/A. In-crate because it reads each adapter's
// crate-private `<id>_capabilities()`, none on the public surface
// (`docs/TESTING.md` §5, `docs/03-data-providers.md` §8).
#[cfg(test)]
mod tests_capability_matrix;

// Bench-only support surface (issue #21), compiled ONLY under the `bench` Cargo
// feature. It exposes the constructors the `benches/*` targets need — a
// populated render `App`, a seeded `ChainStore`, a scripted `MarketUpdate`
// burst, and the Deribit `ticker.`/`book.` → coalescing-merge harness — through
// the crate's own public types, so a `benches/*.rs` (a separate crate that sees
// only the public API) can reach the three hot paths WITHOUT the pure-render
// `chain::draw` or the `ChainStore` being promoted to the default public
// surface. It is an INTERNAL, UNSTABLE harness with NO SemVer guarantee: even
// under `--features bench` it is EXCLUDED from the semver-governed public API and
// may change or be removed in any release without notice (see the module docs and
// `docs/SEMVER.md`). Because it is `#[cfg(feature = "bench")]`, a normal build
// never compiles it and the default public surface is unchanged
// (`docs/06-performance.md` §4, `docs/TESTING.md` §11).
#[cfg(feature = "bench")]
pub mod bench_support;

// Fuzz-only harness surface (issue #53, docs/TESTING.md §13.4), compiled ONLY
// under the `fuzz` Cargo feature. It exposes the byte-in entry point the
// separate `fuzz/` cargo-fuzz crate's `fuzz_provider_normalize` target needs to
// reach the `pub(crate)` Deribit normalize seam (raw upstream DTOs stop at
// `src/providers/*`). Like `bench_support`, it is `#[cfg(feature = "fuzz")]` and
// OFF by default, so a normal build never compiles it and the semver-governed
// public API is unchanged. The replay decode surface it complements is already
// public (`BundleReader`), so the `fuzz_replay_decode` target needs nothing here
// (docs/SECURITY.md §7).
#[cfg(feature = "fuzz")]
pub mod fuzz_support;

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
    App, BridgeSenders, BuilderLeg, BundleLoad, COMMAND_CHANNEL_CAPACITY, CONTROL_CHANNEL_CAPACITY,
    ChainViewApp, ChainViewAppBuilder, CommittedStrategy, CurveMode, DEFAULT_JOIN_BUDGET,
    EventBridge, ExitCause, ExitReporter, FinalTeardown, GuardTeardown, LegError, LegFocus,
    LiveScreen, LiveState, LoadedReplay, Mode, OverlayBinding, PayoffBuilder, ProviderSubscription,
    ReplayPayoffHead, ReplayScreen, ReplayState, Resolved, ScreenLoad, Selection, Side,
    SourceBinding, StatusLine, SupervisedTask, Supervisor, SurfaceAxis, SurfacePanel, SurfaceView,
    TaskExit, TokioTask, is_replay_screen_reachable, is_screen_reachable, spawn_bundle_load,
    spawn_supervised_subscription,
};
// The closed event set folded by the state machine and the render -> data
// command channel (`docs/02-tui-architecture.md` §4).
pub use event::{AppEvent, BundleLoadResult, Command, ReplayControl, SeekTo};
// The pure draw dispatch and the synchronous, event-driven render loop
// (`docs/02-tui-architecture.md` §7, §8, §9): `render` (pure over `&App`), the root
// layout, the loop driver, the bounded `AppEvent` channel, and the tick/input task
// seams the supervisor (#11) owns. These are the render-loop **composition
// internals**; they are exposed provisionally so the loop is reachable while it has
// no runtime caller yet (its `registry::run` composition seam is wired in #15).
// NOTE: this is NOT the ADR-0006 external-extension surface — ADR-0006's external
// model is `ChainViewApp::builder()…run()`, not a hand-rolled loop. Nothing here is
// semver-frozen pre-v1.0; #15's `run()` becomes the canonical driver and revisits
// whether these stay public or narrow to `pub(crate)`.
pub use ui::driver::{
    EVENT_CHANNEL_CAPACITY, event_channel, run_render_loop, spawn_input_reader, spawn_tick_task,
};
pub use ui::view::ViewState;
pub use ui::{RootLayout, layout_root, render};
// The chain-matrix view models (`src/ui/chain.rs`, issue #18,
// `docs/01-domain-model.md` §8): `ChainRow`/`LegView`, projected from the domain
// `OptionChain` at draw time and borrowed, never owned. Public so the render
// goldens (#19) and downstream screens (#25) can name the projected shapes.
pub use ui::chain::{ChainRow, LegView};
// The `GraphData` → ratatui dataset adapter (`src/ui/graph.rs`, issue #23,
// `docs/05-views-and-ux.md` §4): the fallible projection of an `optionstratlib`
// `GraphData` into a ratatui chart shape (borrowed `(f64, f64)` points + `[f64; 2]`
// axis bounds + precomputed labels), and the cache handle that keeps `GraphData`
// construction off the draw path. Public so the payoff screen (#27), the replay
// screens (#35), and the vol surface (#47) can hold the cache on their state and
// name the projected shapes.
pub use ui::graph::{
    AxisBounds, EmptyReason, GraphCache, GraphProjection, ProjectedSeries, ProjectedSurface,
    project,
};
// The theme + render surface: the `NO_COLOR`-aware `Theme`, the
// help-overlay/status/keybar renderers, the `StrikeRelation` K/S bucket and its
// markers/spans, the responsive chain column-drop policy, and the too-small guard
// (`docs/05-views-and-ux.md` §7, §8, issue #14). Public so the chain matrix (#18)
// and the render goldens (#19) can name and reuse the markers, styles, and column
// policy.
pub use ui::theme::{
    AT_SPOT_MARKER, ATM_BAND_PERMILLE, GREEK_DROP_ORDER, GreekColumn, GreekColumns, MIN_HEIGHT,
    MIN_WIDTH, StrikeRelation, Theme, ThemeVariant, greek_columns_for_slots, health_span,
    is_too_small, pnl_sign_char, pnl_sign_span, strike_relation_marker,
    strike_relation_marker_span, tick_dir_glyph, tick_dir_span,
};
// The single-source keybinding map — pure data + resolution in the **application**
// layer (`src/app/keymap.rs`, `docs/05-views-and-ux.md` §3, issue #14). Both
// `App::dispatch_key_global` and the help overlay (`src/ui/theme.rs`) read this one
// table, so dispatch and documentation cannot drift. Public so the chain matrix
// (#18) and the render goldens (#19) can name and reuse the keymap.
pub use app::keymap::{
    Action, Binding, ChainAction, Context, DepthAction, GlobalAction, GlobalCommand, KEYMAP,
    KeyChord, PayoffAction, ReplayAction, SurfaceAction, help_bindings, resolve_chain,
    resolve_depth, resolve_global, resolve_payoff, resolve_replay, resolve_surface,
};

pub use chain::{
    AliasCatalog, CHAIN_STALE_SLACK, ChainFetch, ChainSnapshot, ChainSource, ChainStore,
    ContractSpecFingerprint, DEFAULT_DIVIDEND_YIELD, DEFAULT_RISK_FREE_RATE, DIRECTION_DECAY,
    DepthBook, DepthLadder, DepthLevel, DepthStatus, DepthStore, ExerciseStyle, ExpirySource,
    FEED_DELAY_WARN, Freshness, GREEKS_STALE_AFTER, GreeksOrigin, GreeksRow, GreeksSidecar,
    Instrument, InstrumentKey, LegGreeks, LegStatus, MAX_DEPTH_BOOKS, MAX_PENDING, MarketUpdate,
    MergeOutcome, PremiumNumeraire, PricingInputs, PricingModel, ProviderId, QUOTE_STALE_AFTER,
    QuoteClocks, QuoteSelect, QuoteUpdate, RESERVED_PROVIDER_IDS, SettlementStyle, StreamHealth,
    TickDir, chain_stale_after, compute_leg_greeks, depth_continues, pending_ttl,
};
pub use config::{CliOverrides, Config, ModeSelect, ProviderSettings, ThemeChoice};
pub use error::{
    BundleError, ChainViewError, ConfigError, NormalizeKind, OverlayError, ProviderError, Redacted,
    RegistryError, TransportDetail, TransportKind,
};
// The replay-mode domain types (`src/replay/mod.rs`, issues #29/#30,
// `docs/01-domain-model.md` §9, `docs/04-replay-mode.md` §2/§3): ChainView's
// typed, read-only views of the IronCondor result bundle — the permissive
// `BundleManifest`, the narrow `CapitalConfig` projection, the four strict
// Parquet-backed rows (`Fill`/`EquityPoint`/`PositionRow`/`GreeksAttribution`),
// the closed `PositionSide`/`ExecMode` enums, the `contract_id` grammar
// constants, and the `BundleReader`/`LoadedBundle` surface. Money is integer
// cents; the only `f64` is `EquityPoint::drawdown`. `OptionStyle` is re-exported
// from `optionstratlib` below.
//
// Issue #30 adds the reader body and the untrusted-input hardening spine:
// `BundleReader::{open, open_with_ceilings, load, load_cancellable}`, the
// `ResourceCeilings` config knobs and their documented `MAX_*` /
// `DECODED_OVERHEAD_PERMILLE` defaults, and the `SUPPORTED_SCHEMA` gate. Issue #31
// wires the typed per-column decode (`src/replay/tables.rs`) into that batched,
// budget-measured loop, so `load` returns the four tables populated and sorted;
// the cross-table validation chain (#32) is still written against this surface.
// The public surface gains no new item beyond the `BundleError::Schema` decode
// variant — the decoders themselves are `pub(crate)`.
//
// Issue #32 lands the post-decode validation chain (wired inside `load`, so the
// reader surface is unchanged) plus the cross-repo equivalence oracle:
// `compare_bundles` returns `Ok(())` or the first typed `BundleDivergence`, and
// `ORACLE_ABS_TOL`/`ORACLE_REL_TOL` are the combined-tolerance constants that must
// match IronCondor's copy exactly (`docs/04-replay-mode.md` §5). The validation
// checks and the `contract_id` parser stay module-private — only the oracle is
// public, for the cross-repo agreement check.
//
// Issue #33 adds the timeline scrub model (`src/replay/timeline.rs`,
// `docs/04-replay-mode.md` §4, `docs/01-domain-model.md` §10): `TimelineCursor`
// (O(1) `StepBy` / O(log n) `Step` seeks over the integer `step` clock, the
// post-fill open-position set, and the as-of slices), plus the domain `Playback` /
// `PlaybackSpeed` playback model and `TimelineCursor::advance_playback`. The cursor
// consumes `event::SeekTo`.
//
// Issue #34 (the app-state wiring) collapsed the earlier `app::Playback` stub into
// this single domain `Playback` — there is now exactly one playback type, exported
// bare from the crate root (the earlier transitional re-export alias is gone), and
// the app-state field (`ReplayState::play`) and the tick fold reference this type.
pub use replay::{
    BundleDivergence, BundleManifest, BundleReader, CONTRACT_ID_FORMAT,
    CONTRACT_ID_UNDERLYING_PATTERN, CONTRACT_ID_VERSION_PREFIX, CapitalConfig,
    DECODED_OVERHEAD_PERMILLE, EquityPoint, ExecMode, Fill, GreeksAttribution, LoadedBundle,
    MAX_BATCH_BYTES, MAX_BATCH_ROWS, MAX_EXPANSION_RATIO, MAX_MANIFEST_BYTES, MAX_TABLE_BYTES,
    MAX_TABLE_ROWS, MAX_WORKING_SET, ORACLE_ABS_TOL, ORACLE_REL_TOL, Playback, PlaybackSpeed,
    PositionRow, PositionSide, ResourceCeilings, SUPPORTED_SCHEMA, TimelineCursor, compare_bundles,
};
// The PUBLIC, semver-governed provider port surface (`docs/03-data-providers.md`
// §2, §11.1): the trait, the capability self-declaration + its builder + every
// dimension enum, and the port helper types. The emitted domain types
// (`ChainFetch`/`ExpirySource`/`AliasCatalog`, `MarketUpdate`, `ProviderError`,
// `ProviderId`) are re-exported above from their home layers.
pub use providers::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, MarketUpdateSink,
    OptionStreamCapability, Provider, ProviderCapabilities, ProviderCapabilitiesBuilder, SendState,
    SubscriptionHandle, SubscriptionRequest, UnderlyingRef,
};
// The terminal lifecycle surface (`docs/02-tui-architecture.md` §6, ADR-0001):
// the RAII restore guard and the panic-hook restore installer. Public so an
// external thin binary (ADR-0006) can drive the same guaranteed restore. These
// stay the stable restore entrypoints for hand-rolled external binaries and are
// intentionally NOT narrowed to `pub(crate)` once `ChainViewApp::builder().run()`
// (issue #11) owns the guard internally.
pub use terminal::{TerminalGuard, install_panic_hook};
// The provider port and the domain speak `optionstratlib`'s chain-model and
// numeric vocabulary (`docs/01-domain-model.md` §3–§4,
// `docs/03-data-providers.md` §11.1, ADR-0006 §5): `OptionChain` is the chain a
// `ChainFetch` wraps, `ExpirationDate` is the `Provider::fetch_chain` /
// `UnderlyingRef` expiry type, and `Positive` / `Decimal` / `OptionStyle` are the
// numeric/style types the emitted `QuoteUpdate` / `GreeksRow` / `InstrumentKey`
// carry. Re-export all five at the crate root so an external adapter can name
// every type in the port's signatures through `chainview::` alone, without a
// direct `optionstratlib` dependency (a chain-PRODUCING adapter still depends on
// it to BUILD an `OptionChain`). These are part of the semver-governed port
// surface (`docs/SEMVER.md`, provider-port versioning).
pub use optionstratlib::chains::chain::OptionChain;
pub use optionstratlib::prelude::{Decimal, Positive};
pub use optionstratlib::{ExpirationDate, OptionStyle};
// The timestamp scalar every emitted event/identity value carries
// (`QuoteUpdate`/`GreeksRow`/`DepthLadder` received/event times,
// `InstrumentKey::expiration_utc`, `ExpirySource::expiration_utc`). No exported
// fn produces one, so without this re-export a chain-producing or streaming
// external adapter would need a direct `chrono` dependency to construct the
// values the port emits (#43 review).
pub use chrono::{DateTime, Utc};
