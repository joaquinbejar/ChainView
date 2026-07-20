//! The application-owned provider registry and the `ChainViewApp` builder
//! (`docs/02-tui-architecture.md` §11, `docs/03-data-providers.md` §9,
//! [ADR-0006]).
//!
//! `chainview` is published as a **binary + library**: the stock binary fills
//! the [`ProviderRegistry`] with the **gate-clear** built-ins via
//! [`ChainViewAppBuilder::with_builtins`], and an **external** developer depends
//! on the `chainview` library from their own thin binary and adds their own
//! `Box<dyn Provider>` before startup — no fork, no central enum to edit
//! ([ADR-0006] §2–§3). The registry lives in the **application** layer; the UI
//! never receives it and gates screens off declared
//! [`ProviderCapabilities`](crate::providers::ProviderCapabilities)
//! (`docs/02-tui-architecture.md` §3), never off a [`ProviderId`] match, so a
//! built-in and an external provider are treated identically.
//!
//! # Collision is a typed startup error, never a panic
//!
//! [`register`](ChainViewAppBuilder::register) reads `provider.id()`. A
//! **duplicate** id is [`RegistryError::DuplicateId`], a **reserved** built-in id
//! used through the external `register` path is [`RegistryError::ReservedId`], a
//! **gated** built-in requested via
//! [`with_gated_builtin`](ChainViewAppBuilder::with_gated_builtin) while its gate
//! holds is
//! [`RegistryError::Gated`], and an **empty** registry at
//! [`run`](ChainViewAppBuilder::run) is [`RegistryError::Empty`] — all surface as
//! [`ChainViewError::Registry`], never a panic or a silent last-writer-wins
//! (`docs/01-domain-model.md` §11). The build-phase errors are accumulated
//! first-error-wins and reported by [`run`](ChainViewAppBuilder::run), keeping
//! every builder method infallible-by-signature (`-> Self`) as the design fixes
//! (`docs/02-tui-architecture.md` §11).
//!
//! # `Arc<dyn Provider>`, immutable after validation
//!
//! The registry stores each adapter behind an [`Arc`] rather than a bare `Box`:
//! the same adapter is shared **read-only** across the independent poll and
//! stream tasks the render loop spawns (#13/#16) without re-fetching it, and the
//! registry is immutable once [`run`](ChainViewAppBuilder::run) has validated it
//! (`docs/02-tui-architecture.md` §11 implementation note). [`Provider`] is
//! `Send + Sync`, so `Arc<dyn Provider>` is registry- and task-ready.
//!
//! # Validation + resolution here; the render-loop composition is in the binary
//!
//! This module owns the registry, the builder, startup validation, `--provider`
//! resolution, and the capability-driven composite-source guard.
//! [`with_builtins`](ChainViewAppBuilder::with_builtins) registers the real Deribit
//! adapter — the zero-config poll leg. [`run`](ChainViewAppBuilder::run) validates
//! and resolves and returns `Ok(())`; a binary that drives the TUI calls
//! [`resolve`](ChainViewAppBuilder::resolve) instead to obtain the drivable
//! [`Resolved`] pieces (the provider, the [`SourceBinding`], the [`Config`]) and
//! composes the loop over them. The render-loop composition — the tokio runtime,
//! the bounded [`EventBridge`](super::EventBridge), the
//! [`Supervisor`](super::Supervisor), the provider stream task registered via
//! [`spawn_supervised_subscription`], and the render loop — lives in the **binary**
//! (`main.rs`), because the render loop is in the `ui` layer and the application
//! layer must not import `crate::ui` (the arch fence, `tests/arch.rs`). The seeded
//! [`ChainStore`](crate::chain::ChainStore) comes from the provider's first
//! `fetch_chain`; the streaming overlay is routed through the two-class
//! [`MarketUpdateSink`](crate::MarketUpdateSink) (ADR-0009).
//!
//! [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::chain::{Instrument, ProviderId, StreamHealth};
use crate::config::{CliOverrides, Config, EnvSource, ModeSelect, ProcessEnv};
use crate::error::{ChainViewError, ConfigError, ProviderError, RegistryError};
#[cfg(feature = "alpaca")]
use crate::providers::alpaca::AlpacaAdapter;
use crate::providers::deribit::DeribitAdapter;
#[cfg(feature = "dxlink")]
use crate::providers::dxlink::DxlinkAdapter;
#[cfg(feature = "ig")]
use crate::providers::ig::IgAdapter;
#[cfg(feature = "tastytrade")]
use crate::providers::tastytrade::TastytradeAdapter;
use crate::providers::{Provider, SubscriptionRequest};

use super::{BridgeSenders, SourceBinding, Supervisor, chain_present};

// ---------------------------------------------------------------------------
// ProviderRegistry: the application-owned set of adapters, keyed by ProviderId.
// ---------------------------------------------------------------------------

/// The set of providers available to the process, keyed by the open
/// [`ProviderId`] (`docs/02-tui-architecture.md` §11, [ADR-0006]).
///
/// Owned by the application layer and **opaque**: it is assembled only through
/// [`ChainViewAppBuilder`], its entries are stored behind an [`Arc`] so a single
/// adapter can be shared read-only across the poll and stream tasks, and it is
/// immutable once [`run`](ChainViewAppBuilder::run) has validated it. The UI
/// never receives it — screen gating reads declared [`ProviderCapabilities`]
/// (`crate::providers`), never the id.
///
/// [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md
/// [`ProviderCapabilities`]: crate::providers::ProviderCapabilities
pub struct ProviderRegistry {
    by_id: HashMap<ProviderId, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// An empty registry.
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    /// Whether no provider is registered.
    fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The registered adapter for `id`, or `None` when it is absent.
    fn get(&self, id: &ProviderId) -> Option<&Arc<dyn Provider>> {
        self.by_id.get(id)
    }

    /// Whether an adapter is registered under `id`.
    fn contains(&self, id: &ProviderId) -> bool {
        self.by_id.contains_key(id)
    }

    /// Insert `provider` under `id`. The caller has already rejected a reserved
    /// id and checked for a collision, so this always inserts a fresh entry.
    fn insert(&mut self, id: ProviderId, provider: Arc<dyn Provider>) {
        let _ = self.by_id.insert(id, provider);
    }
}

impl fmt::Debug for ProviderRegistry {
    /// Renders the registered ids only (sorted, for determinism). [`Provider`] is
    /// not `Debug`, and the ids are public, non-secret, so this stays loggable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ids: Vec<&ProviderId> = self.by_id.keys().collect();
        ids.sort();
        f.debug_struct("ProviderRegistry")
            .field("ids", &ids)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ChainViewApp: the entry point for the stock and external binaries.
// ---------------------------------------------------------------------------

/// The assembled ChainView application — the entry point every binary starts
/// from (`docs/02-tui-architecture.md` §11, [ADR-0006] §3).
///
/// A thin handle whose sole role is to expose [`builder`](Self::builder): the
/// stock binary and any external binary register their adapters through the
/// [`ChainViewAppBuilder`], then either [`run`](ChainViewAppBuilder::run) to
/// validate + resolve, or [`resolve`](ChainViewAppBuilder::resolve) to obtain the
/// drivable [`Resolved`] pieces the binary composes the render loop over
/// (`main.rs`).
///
/// ```no_run
/// use chainview::ChainViewApp;
///
/// # fn main() -> Result<(), chainview::ChainViewError> {
/// // Stock binary: register the gate-clear built-ins and validate + resolve.
/// ChainViewApp::builder().with_builtins().run()?;
/// # Ok(())
/// # }
/// ```
///
/// [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md
#[derive(Debug, Clone, Copy)]
pub struct ChainViewApp;

impl ChainViewApp {
    /// Start assembling the app. The returned [`ChainViewAppBuilder`] registers
    /// the built-in and/or external adapters, then [`run`](ChainViewAppBuilder::run)
    /// validates the registry and composes the app.
    #[must_use]
    pub fn builder() -> ChainViewAppBuilder {
        ChainViewAppBuilder::new()
    }
}

// ---------------------------------------------------------------------------
// ChainViewAppBuilder: builder-style registration + startup validation.
// ---------------------------------------------------------------------------

/// The builder that registers providers and validates the registry at startup
/// (`docs/02-tui-architecture.md` §11, [ADR-0006] §3).
///
/// Every registration method returns `Self` for fluent chaining and defers its
/// typed failure to [`run`](Self::run): a build-phase collision (reserved /
/// duplicate id) or a gated-built-in request records a **first-error-wins**
/// pending [`ChainViewError`] that [`run`](Self::run) surfaces, so a builder
/// method never has to return a `Result` mid-chain.
///
/// [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md
#[derive(Debug)]
pub struct ChainViewAppBuilder {
    registry: ProviderRegistry,
    /// The assembled startup config injected by `main.rs` (the CLI-derived
    /// `--provider`/mode source); `None` falls back to the zero-config default at
    /// [`run`](Self::run).
    config: Option<Config>,
    /// The first build-phase error (reserved / duplicate id, gated built-in),
    /// recorded first-wins and surfaced by [`run`](Self::run).
    pending: Option<ChainViewError>,
}

impl Default for ChainViewAppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChainViewAppBuilder {
    /// A builder with an empty registry and no injected config.
    #[must_use]
    fn new() -> Self {
        Self {
            registry: ProviderRegistry::new(),
            config: None,
            pending: None,
        }
    }

    /// Register the bundled adapters whose security gate is **clear** at the
    /// pinned upstream revisions (`docs/SECURITY.md` §2, `docs/03-data-providers.md`
    /// §9). Still-gated adapters (tastytrade / standalone dxlink — credential-logging
    /// upstreams, `docs/SECURITY.md` §2.3–§2.4) are **never** registered implicitly;
    /// they require the explicit [`with_gated_builtin`](Self::with_gated_builtin)
    /// opt-in.
    ///
    /// # The gate-clear built-ins
    ///
    /// - **Deribit** (public, no auth) — the zero-config default (ADR-0003), always
    ///   registered under the reserved `deribit` id, so `builder().with_builtins()`
    ///   always resolves a live source.
    /// - **Alpaca** — its credential-logging gate is **lifted** (#99,
    ///   `docs/SECURITY.md` §2.4, captured-log proof in `src/providers/alpaca.rs`).
    ///   It is a **credentialed** built-in, so it is registered **only when its
    ///   `CHAINVIEW_ALPACA_*` credentials are configured** in the environment, and is
    ///   silently omitted (never a startup error) when they are absent — so the
    ///   zero-config Deribit default is unaffected. It is compiled in only under the
    ///   opt-in `alpaca` Cargo feature (the heavy upstream deps stay out of a default
    ///   build).
    ///
    /// The credential probe reads the real process environment; the env-injectable
    /// `with_builtins_from_env` (a private method) is the same path used
    /// deterministically in tests.
    #[must_use]
    pub fn with_builtins(self) -> Self {
        self.with_builtins_from_env(&ProcessEnv)
    }

    /// The env-injectable core of [`with_builtins`](Self::with_builtins): register the
    /// gate-clear built-ins, reading any **credentialed** built-in's credentials from
    /// `env` (env-only, never logged). Production passes the real [`ProcessEnv`];
    /// tests inject a map so registration is deterministic and never touches the
    /// process environment.
    ///
    /// A credentialed built-in whose credentials are absent is **omitted**, not an
    /// error, so a stock startup with no Alpaca credentials keeps the zero-config
    /// Deribit default and a later `--provider alpaca` is a clean
    /// [`ConfigError::UnknownProvider`], never a `MissingCredential` startup failure.
    #[must_use]
    fn with_builtins_from_env(self, env: &dyn EnvSource) -> Self {
        let builder = self.register_builtin(DeribitAdapter::new());
        #[cfg(feature = "alpaca")]
        let builder = builder.register_credentialed_builtin(env, alpaca_builtin_factory);
        // IG is a REAL built-in under its `ig` dependency-weight feature (ADR-0013):
        // registered when `CHAINVIEW_IG_*` is configured, omitted (never an error)
        // when absent, so zero-config Deribit is unaffected (the alpaca #99 pattern).
        #[cfg(feature = "ig")]
        let builder = builder.register_credentialed_builtin(env, ig_builtin_factory);
        #[cfg(not(any(feature = "alpaca", feature = "ig")))]
        let _ = env;
        builder
    }

    /// Opt in to a security-**gated** bundled adapter explicitly. Fails at
    /// startup with [`RegistryError::Gated`] while the adapter's upstream
    /// credential-logging gate still holds, so a gated adapter can **never** be
    /// enabled silently (`docs/SECURITY.md` §2.3–§2.4,
    /// `docs/02-tui-architecture.md` §11).
    ///
    /// # v0.1: the gate always holds
    ///
    /// No gated adapter ships in v0.1 and none has cleared its gate, so this
    /// records a pending [`RegistryError::Gated`] that [`run`](Self::run)
    /// surfaces. This is the **mechanism**; it is exercised in v0.4 when a gated
    /// adapter's pinned upstream clears its gate (`docs/ROADMAP.md`).
    #[must_use]
    pub fn with_gated_builtin(mut self, id: ProviderId) -> Self {
        // A still-gated built-in (tastytrade / standalone dxlink) is NEVER enabled
        // while its upstream credential-logging gate holds (`docs/SECURITY.md` §2):
        // record the typed startup error and never construct the adapter.
        // `note_gated_builtins` names each still-gated adapter's factory (never
        // invoked here) purely so the deliberately unregistered adapter stays
        // compiled + linted in a plain `--features <gated>` library build. (Alpaca is
        // no longer here — its gate is lifted (#99), so it is a real built-in
        // registered by `with_builtins`, not reached through this opt-in.)
        #[cfg(any(feature = "tastytrade", feature = "dxlink"))]
        note_gated_builtins();
        self.record(RegistryError::Gated(id).into());
        self
    }

    /// Register an external provider (`docs/02-tui-architecture.md` §11,
    /// [ADR-0006](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md) §3).
    /// The id is read from `provider.id()`: a **reserved** built-in id used here
    /// records [`RegistryError::ReservedId`] (so a downstream adapter can never
    /// masquerade as a built-in or shadow its config namespace), and a
    /// **duplicate** id records [`RegistryError::DuplicateId`] (never a silent
    /// last-writer-wins). Both are surfaced by [`run`](Self::run).
    #[must_use]
    pub fn register(mut self, provider: impl Provider + 'static) -> Self {
        self.register_arc(Arc::new(provider));
        self
    }

    /// Inject the assembled startup [`Config`] — the source of the `--provider`
    /// selection and the run [`ModeSelect`]. `main.rs` passes the CLI-derived
    /// config here; when omitted, [`run`](Self::run) loads the zero-config default
    /// (ADR-0003 Deribit BTC).
    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Validate the registry, resolve the selected mode/source, and — for a live
    /// TUI — hand the resolved pieces to the caller to drive
    /// (`docs/02-tui-architecture.md` §11).
    ///
    /// [`run`](Self::run) is the library **validation + resolution** entry: it
    /// returns `Ok(())` once resolution succeeds. It does **not** spin the render
    /// loop, because the loop lives in the `ui` layer and the composition that
    /// drives it (the tokio runtime + terminal + bounded
    /// [`EventBridge`](super::EventBridge) + [`Supervisor`](super::Supervisor) +
    /// provider [`spawn_supervised_subscription`] + render loop) is assembled in the
    /// **binary** (`main.rs`) — the application layer must not import `crate::ui`
    /// (the arch fence, `tests/arch.rs`). A binary drives the TUI by calling
    /// [`resolve`](Self::resolve) instead and composing the loop
    /// over the returned [`Resolved`] pieces.
    ///
    /// # Errors
    ///
    /// [`ChainViewError::Registry`] for a reserved / duplicate id, a gated
    /// built-in, or an empty registry; [`ChainViewError::Config`] for an unknown
    /// selected provider or a chain-less source without an overlay chain; and a
    /// [`Config`]-load failure from the zero-config fallback.
    pub fn run(self) -> Result<(), ChainViewError> {
        // Resolution is the validation: an empty registry / unknown provider /
        // chain-less source / build-phase collision all surface here. The binary
        // (`main.rs`) uses `resolve()` directly to obtain the drivable pieces.
        let _resolved = self.resolve()?;
        Ok(())
    }

    /// Validate the registry, resolve the startup [`Config`], and produce the
    /// drivable [`Resolved`] pieces the binary composes the runtime + render loop
    /// over (`docs/02-tui-architecture.md` §11, ADR-0009).
    ///
    /// The order is: (1) a build-phase collision / gated-builtin error (recorded
    /// first-wins) is surfaced; (2) the startup [`Config`] is resolved (injected,
    /// or the zero-config default loaded here); (3) in [`ModeSelect::Live`] the
    /// registry is validated non-empty ([`RegistryError::Empty`]), `--provider` is
    /// resolved against it ([`ConfigError::UnknownProvider`] when absent), the
    /// selected provider's capabilities are read **once**, the capability-driven
    /// composite-source guard runs, and the resolved provider + [`SourceBinding`]
    /// are returned. [`ModeSelect::Replay`] needs no live provider.
    ///
    /// # Errors
    ///
    /// The same set as [`run`](Self::run).
    pub fn resolve(self) -> Result<Resolved, ChainViewError> {
        let Self {
            registry,
            config,
            pending,
        } = self;

        // (1) A build-phase collision / gated-builtin is the first failure.
        if let Some(error) = pending {
            return Err(error);
        }

        // (2) Resolve the startup config: injected by main.rs, or the zero-config
        // default loaded here (ADR-0003). Config assembly (issue #3/#4) has
        // already validated the `--provider` GRAMMAR, so `config.provider` is a
        // well-formed id; a syntactically invalid `--provider` is
        // `ConfigError::InvalidValue` there, before ever reaching this seam.
        let config = match config {
            Some(config) => config,
            None => Config::load(CliOverrides::default())?,
        };

        // (3) Resolve per mode.
        match &config.mode {
            ModeSelect::Live => {
                let (provider, source) = resolve_source(&registry, &config)?;
                Ok(Resolved::Live {
                    provider,
                    source,
                    config,
                })
            }
            ModeSelect::Replay(dir) => {
                // Replay renders an IronCondor bundle read-only and needs NO live
                // provider, so the registry emptiness / `--provider` resolution do
                // not apply. SEAM: the binary's replay composition builds the
                // `App` with `Mode::Replay(ReplayState::new(dir))`
                // (BundleLoad::Loading) and starts the off-thread load with
                // [`spawn_bundle_load`](super::spawn_bundle_load) under a
                // supervisor child token, whose `AppEvent::BundleLoaded` folds
                // the bundle in (#34).
                let dir = dir.clone();
                Ok(Resolved::Replay { dir, config })
            }
        }
    }

    // --- Internal helpers -----------------------------------------------------

    /// Register a bundled built-in adapter under its reserved id. Unlike the
    /// external [`register`](Self::register) path, a reserved id is **expected**
    /// here (built-ins own the reserved namespace), so only a duplicate is an
    /// error (recorded first-error-wins) — a built-in registered twice.
    fn register_builtin(mut self, provider: impl Provider + 'static) -> Self {
        let arc: Arc<dyn Provider> = Arc::new(provider);
        let id = arc.id();
        if self.registry.contains(&id) {
            self.record(RegistryError::DuplicateId(id).into());
        } else {
            self.registry.insert(id, arc);
        }
        self
    }

    /// Register a **credentialed** gate-clear built-in from its env factory
    /// (`docs/SECURITY.md` §2.4). On a successful build register it under its reserved
    /// id (a duplicate is recorded first-error-wins); on a **missing credential**
    /// silently omit it (the built-in is simply unconfigured, so the zero-config
    /// Deribit default is preserved); on any other typed error record it. The reserved
    /// id is EXPECTED here (built-ins own the reserved namespace), so only a duplicate
    /// — never the reserved id itself — is an error.
    #[cfg(any(feature = "alpaca", feature = "ig"))]
    fn register_credentialed_builtin(
        mut self,
        env: &dyn EnvSource,
        factory: CredentialedBuiltinFactory,
    ) -> Self {
        match factory(env) {
            Ok(arc) => {
                let id = arc.id();
                if self.registry.contains(&id) {
                    self.record(RegistryError::DuplicateId(id).into());
                } else {
                    self.registry.insert(id, arc);
                }
            }
            Err(ConfigError::MissingCredential(_)) => {}
            Err(other) => self.record(other.into()),
        }
        self
    }

    /// Register an already-boxed adapter, rejecting a reserved id and a
    /// collision (both recorded first-error-wins).
    fn register_arc(&mut self, provider: Arc<dyn Provider>) {
        let id = provider.id();
        // A reserved built-in id used through the EXTERNAL register path is
        // rejected (ADR-0006 §1); built-ins are added through `with_builtins`,
        // not here.
        if id.is_reserved() {
            self.record(RegistryError::ReservedId(id).into());
            return;
        }
        if self.registry.contains(&id) {
            self.record(RegistryError::DuplicateId(id).into());
            return;
        }
        self.registry.insert(id, provider);
    }

    /// Record a build-phase error, first-error-wins (a later error never
    /// overwrites the first, so `run` reports the first problem encountered).
    fn record(&mut self, error: ChainViewError) {
        if self.pending.is_none() {
            self.pending = Some(error);
        }
    }
}

/// A gate-clear **credentialed** built-in factory: read the provider's credentials
/// from the environment (env-only, never logged) and yield a registry-ready
/// `Arc<dyn Provider>`, or [`ConfigError::MissingCredential`] when unconfigured (so
/// the built-in is simply omitted). Invoked by
/// [`register_credentialed_builtin`](ChainViewAppBuilder::register_credentialed_builtin).
#[cfg(any(feature = "alpaca", feature = "ig"))]
type CredentialedBuiltinFactory = fn(&dyn EnvSource) -> Result<Arc<dyn Provider>, ConfigError>;

/// The Alpaca credentialed-built-in factory (#99): read its `CHAINVIEW_ALPACA_*`
/// credentials from `env` and yield a registry-ready `Arc<dyn Provider>`, or
/// [`ConfigError::MissingCredential`] when unconfigured. Its credential-logging gate
/// is **lifted** (`docs/SECURITY.md` §2.4, captured-log proof in
/// `src/providers/alpaca.rs`), so unlike the still-gated factories in
/// [`note_gated_builtins`] this one IS invoked — by
/// [`register_credentialed_builtin`](ChainViewAppBuilder::register_credentialed_builtin)
/// from [`with_builtins`](ChainViewAppBuilder::with_builtins).
#[cfg(feature = "alpaca")]
fn alpaca_builtin_factory(env: &dyn EnvSource) -> Result<Arc<dyn Provider>, ConfigError> {
    Ok(Arc::new(AlpacaAdapter::from_env(env)?) as Arc<dyn Provider>)
}

/// The IG credentialed-built-in factory (#39): read its `CHAINVIEW_IG_*` credentials
/// from `env` and yield a registry-ready `Arc<dyn Provider>`, or
/// [`ConfigError::MissingCredential`] when unconfigured (so the built-in is simply
/// omitted). IG's `ig` feature is a **dependency-weight** gate (ADR-0013), not a
/// credential-logging security gate, so — like [`alpaca_builtin_factory`] — it IS
/// invoked by [`register_credentialed_builtin`](ChainViewAppBuilder::register_credentialed_builtin)
/// from [`with_builtins`](ChainViewAppBuilder::with_builtins).
#[cfg(feature = "ig")]
fn ig_builtin_factory(env: &dyn EnvSource) -> Result<Arc<dyn Provider>, ConfigError> {
    Ok(Arc::new(IgAdapter::from_env(env)?) as Arc<dyn Provider>)
}

/// Keep every still-security-gated built-in adapter compiled and linted without
/// enabling it (`docs/SECURITY.md` §2, `docs/03-data-providers.md` §9). A gated
/// adapter is deliberately **not** registered — [`with_gated_builtin`](ChainViewAppBuilder::with_gated_builtin)
/// records [`RegistryError::Gated`] and never constructs it — so in a plain
/// `--features <gated>` library build (no `#[cfg(test)]`) nothing references the
/// adapter and the dead-code lint would flag its real, tested code. Naming each
/// gated adapter's factory here (behind its own feature) makes the full `Provider`
/// call graph reachable — the `Arc<dyn Provider>` coercion builds the vtable — WITHOUT
/// invoking the factory. The factory is invoked only once the gate lifts and the
/// adapter is registered for real (as Alpaca now is — see [`alpaca_builtin_factory`],
/// which is no longer named here).
/// A gated built-in adapter factory: it reads the provider's credentials from the
/// environment and yields a registry-ready `Arc<dyn Provider>`. Invoked only once
/// the adapter's security gate lifts; while the gate holds it is merely *named*
/// (see [`note_gated_builtins`]).
#[cfg(any(feature = "tastytrade", feature = "dxlink"))]
type GatedBuiltinFactory =
    fn(&dyn crate::config::EnvSource) -> Result<Arc<dyn Provider>, ConfigError>;

#[cfg(any(feature = "tastytrade", feature = "dxlink"))]
fn note_gated_builtins() {
    #[cfg(feature = "tastytrade")]
    {
        let _tastytrade: GatedBuiltinFactory =
            |env| Ok(Arc::new(TastytradeAdapter::from_env(env)?) as Arc<dyn Provider>);
        let _ = _tastytrade;
    }
    #[cfg(feature = "dxlink")]
    {
        let _dxlink: GatedBuiltinFactory =
            |env| Ok(Arc::new(DxlinkAdapter::from_env(env)?) as Arc<dyn Provider>);
        let _ = _dxlink;
    }
}

// ---------------------------------------------------------------------------
// Source resolution: capability-driven, never a ProviderId match.
// ---------------------------------------------------------------------------

/// Resolve the `--provider` selection against the registry and wire it into a
/// [`SourceBinding`], reading the selected provider's declared capabilities
/// **once** (`docs/02-tui-architecture.md` §11, §3).
///
/// Gating is **capability-driven**: the composite-source guard reads
/// [`ProviderCapabilities::chain`](crate::providers::ProviderCapabilities), never
/// matches a [`ProviderId`], so a chain-less provider (standalone dxlink) is
/// rejected as a live *source* identically whether it is a built-in or an
/// external adapter.
///
/// # Errors
///
/// [`RegistryError::Empty`] when nothing is registered, [`ConfigError::UnknownProvider`]
/// when `--provider` names no registered id, and [`ConfigError::InvalidValue`]
/// when the selected provider produces no chain of its own (the composite-source
/// rule — a chain-less feed can only overlay an external chain source; no
/// composite-source configuration ships in v0.1, so this is the guard exercised
/// in v0.4).
fn resolve_source(
    registry: &ProviderRegistry,
    config: &Config,
) -> Result<(Arc<dyn Provider>, SourceBinding), ChainViewError> {
    // An empty registry has no live provider to select.
    if registry.is_empty() {
        return Err(RegistryError::Empty.into());
    }

    // `--provider` resolution: the grammar is already validated (issue #4), so we
    // only check the id is REGISTERED. An absent id is `UnknownProvider`.
    let provider = registry.get(&config.provider).ok_or_else(|| {
        ChainViewError::from(ConfigError::UnknownProvider(
            config.provider.as_str().to_owned(),
        ))
    })?;

    // Read the declared capabilities ONCE — the single startup query the UI gates
    // every screen on (§3). Never re-queried per frame.
    let capabilities = provider.capabilities();

    // Composite-source guard (capability-driven, NEVER a `ProviderId` match): a
    // chain-less provider cannot be a live SOURCE on its own — it can only overlay
    // an external chain source (v0.4 composite source). No composite-source
    // configuration exists in v0.1, so selecting a chain-less provider as the
    // source is rejected here.
    if !chain_present(capabilities.chain) {
        return Err(ChainViewError::from(ConfigError::InvalidValue {
            field: "provider".to_owned(),
            reason: format!(
                "provider `{}` produces no option chain, so it cannot be a live \
                 chain source; a chain-less feed can only overlay an external \
                 chain source",
                config.provider
            ),
        }));
    }

    // Wire the selected provider into the source binding. The initial health is
    // the pre-connection `Reconnecting { attempt: 1 }` (first connect in
    // progress); the reconnect loop (#16) drives it to `Live` on the first data
    // and to a later `Reconnecting`/`Stale` on a drop.
    let binding = SourceBinding::new(
        config.provider.clone(),
        capabilities,
        StreamHealth::Reconnecting { attempt: 1 },
    );
    Ok((Arc::clone(provider), binding))
}

// ---------------------------------------------------------------------------
// The drivable resolution + the supervised subscription composition seam.
// ---------------------------------------------------------------------------

/// The resolved startup pieces a binary composes the runtime + render loop over
/// (`docs/02-tui-architecture.md` §11). Returned by
/// [`ChainViewAppBuilder::resolve`]; the TUI composition lives in the binary
/// (`main.rs`) because the render loop is in the `ui` layer and the application
/// layer must not import `crate::ui` (the arch fence).
#[allow(clippy::large_enum_variant)]
pub enum Resolved {
    /// A live TUI: the resolved provider (the poll + stream source), the UI-facing
    /// [`SourceBinding`] (capabilities + id + health), and the assembled [`Config`].
    Live {
        /// The resolved chain source — shared read-only across the poll seed and
        /// the supervised stream task.
        provider: Arc<dyn Provider>,
        /// The UI-facing binding the screens gate on (capabilities, never the id).
        source: SourceBinding,
        /// The assembled startup config (refresh cadence, channel capacity, …).
        config: Config,
    },
    /// A replay session: the bundle directory (read-only) and the config. Needs no
    /// live provider.
    Replay {
        /// The IronCondor result-bundle directory to open, read-only.
        dir: PathBuf,
        /// The assembled startup config.
        config: Config,
    },
}

/// A live provider subscription registered under the [`Supervisor`] — the caller
/// keeps this so a per-provider `Unsubscribe`/`Rediscover` can cancel **only**
/// this provider's subtree without tripping the root ([ADR-0009]).
///
/// It retains the provider's [`child_token`](Supervisor::child_token) (a
/// [`cancel`](Self::cancel) cancels exactly this provider) and the watched task's
/// [`AbortHandle`] (a last-resort [`abort`](Self::abort) after cancellation), so
/// the "no per-provider cancel handle" gap is closed.
///
/// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
#[derive(Debug, Clone)]
pub struct ProviderSubscription {
    id: ProviderId,
    cancel: CancellationToken,
    abort: AbortHandle,
}

impl ProviderSubscription {
    /// The provider this subscription belongs to.
    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        &self.id
    }

    /// Cancel **only** this provider's reconnect loop (a cooperative stop on its
    /// child token), without touching the root or the other providers.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Force-abort the watched task — the last resort after [`cancel`](Self::cancel)
    /// if the loop ignores cooperative cancellation.
    pub fn abort(&self) {
        self.abort.abort();
    }
}

/// Spawn a provider's streaming subscription and register it under the
/// [`Supervisor`] as a **watched** task (ADR-0009, the composition seam).
///
/// This is the single public seam a binary (built-in or external, ADR-0006) uses
/// to plug a resolved provider into the live loop with **no built-in
/// special-casing**:
///
/// 1. mint the provider's [`child_token`](Supervisor::child_token) (the issue-11
///    supervision seam) — the root cancel cascades to it, and the returned
///    [`ProviderSubscription`] retains a clone for a per-provider cancel;
/// 2. build the two-class [`MarketUpdateSink`](crate::MarketUpdateSink) from
///    `senders` and call [`Provider::subscribe`];
/// 3. take the loop's [`JoinHandle`](tokio::task::JoinHandle) out of the returned
///    handle and [`watch`](Supervisor::watch) it, so a panic/return **mid-run**
///    wakes the supervisor (rather than the loop dying unnoticed) and the task is
///    reaped at the ordered teardown.
///
/// Returns `Ok(Some(_))` for a streaming provider (the retained per-provider
/// cancel handle), and `Ok(None)` for a **poll-only** provider whose `subscribe`
/// returns [`ProviderError::Unsupported`] (there is no stream task to supervise).
/// Must be called from within a tokio runtime.
///
/// # Errors
///
/// Any non-`Unsupported` [`ProviderError`] from `subscribe` (a transport/auth
/// failure opening the stream).
pub async fn spawn_supervised_subscription(
    provider: &Arc<dyn Provider>,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    instruments: Vec<Instrument>,
    senders: &BridgeSenders,
    supervisor: &mut Supervisor,
) -> Result<Option<ProviderSubscription>, ProviderError> {
    // (1) The per-provider child token (issue-11 seam): the root cancel cascades
    // to it; the caller keeps the returned clone for a mid-run per-provider cancel.
    let cancel = supervisor.child_token();
    // (2) The two-class sink over the bridge's control + coalesced senders.
    let sink = senders.market_update_sink();
    let request = SubscriptionRequest::new(underlying, expiration_utc, instruments, cancel.clone());
    match provider.subscribe(request, sink).await {
        Ok(mut handle) => match handle.take_join_handle() {
            // (3) Watch the spawned loop so a mid-run panic/return wakes the
            // supervisor; the ordered teardown reaps it (bounded, then aborted).
            Some(join) => {
                let abort = supervisor.watch(join);
                Ok(Some(ProviderSubscription {
                    id: provider.id(),
                    cancel,
                    abort,
                }))
            }
            // A streaming provider is expected to return a spawned handle; a handle
            // with no join carries its own RAII lifecycle, so there is nothing to
            // supervise here (the handle's drop cancels it). Treat as no stream task.
            None => Ok(None),
        },
        // A poll-only provider has no stream to supervise.
        Err(ProviderError::Unsupported(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use optionstratlib::ExpirationDate;
    use proptest::prelude::*;

    use super::{ChainViewApp, ProviderRegistry, resolve_source, spawn_supervised_subscription};
    use crate::app::{
        BridgeSenders, EventBridge, FinalTeardown, LiveScreen, SourceBinding, Supervisor,
        is_screen_reachable,
    };
    use crate::chain::{
        ChainFetch, Instrument, MarketUpdate, ProviderId, RESERVED_PROVIDER_IDS, StreamHealth,
    };
    use crate::config::{Config, EnvSource, ModeSelect, ThemeChoice};
    use crate::error::{ChainViewError, ConfigError, ProviderError, RegistryError};
    use crate::providers::{
        ChainCapability, GreeksCapability, MarketUpdateSink, Provider, ProviderCapabilities,
        SubscriptionHandle, SubscriptionRequest, UnderlyingRef,
    };

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    /// A map-backed [`EnvSource`] so the credentialed-built-in registration is tested
    /// deterministically, never touching the process environment.
    struct TestEnv(HashMap<String, String>);

    impl EnvSource for TestEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    /// An empty environment — no provider credentials configured.
    fn empty_env() -> TestEnv {
        TestEnv(HashMap::new())
    }

    /// An environment carrying valid `CHAINVIEW_ALPACA_*` credentials.
    fn alpaca_creds_env() -> TestEnv {
        let mut env = HashMap::new();
        let _ = env.insert(
            "CHAINVIEW_ALPACA_API_KEY".to_owned(),
            "PKTESTKEY0001".to_owned(),
        );
        let _ = env.insert(
            "CHAINVIEW_ALPACA_API_SECRET".to_owned(),
            "test-secret-value".to_owned(),
        );
        TestEnv(env)
    }

    fn chainful_caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .greeks(GreeksCapability::Provided)
            .build()
    }

    fn chainless_caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::None)
            .build()
    }

    /// A live config selecting `provider`, with the zero-config defaults for the
    /// remaining fields (constructed directly — every `Config` field is public —
    /// so the resolution seam is tested without touching the environment/file).
    fn live_config(provider: &str) -> Config {
        Config {
            provider: pid(provider),
            underlying: "BTC".to_owned(),
            refresh_interval: Duration::from_secs(2),
            tick_interval: Duration::from_millis(250),
            channel_capacity: 1024,
            log_file: None,
            theme: ThemeChoice::Auto,
            no_color: false,
            providers: BTreeMap::new(),
            mode: ModeSelect::Live,
        }
    }

    fn replay_config(dir: &str) -> Config {
        Config {
            mode: ModeSelect::Replay(PathBuf::from(dir)),
            ..live_config("deribit")
        }
    }

    /// A minimal in-test provider proving [`register`] works against the public
    /// port without a real adapter — it prefigures the #22 faux provider. Its
    /// async methods return `Unsupported` (the port shape) and are never awaited
    /// by these tests, which exercise only the sync `id`/`capabilities` seam.
    struct FakeProvider {
        id: ProviderId,
        capabilities: ProviderCapabilities,
        capability_calls: Arc<AtomicUsize>,
    }

    impl FakeProvider {
        fn new(id: ProviderId, capabilities: ProviderCapabilities) -> Self {
            Self {
                id,
                capabilities,
                capability_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn chainful(id: ProviderId) -> Self {
            Self::new(id, chainful_caps())
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            let _ = self.capability_calls.fetch_add(1, Ordering::SeqCst);
            self.capabilities
        }

        async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
            Ok(Vec::new())
        }

        async fn fetch_chain(
            &self,
            _underlying: &str,
            _expiration: &ExpirationDate,
        ) -> Result<ChainFetch, ProviderError> {
            Err(ProviderError::Unsupported("fake provider has no chain"))
        }

        async fn subscribe(
            &self,
            _req: SubscriptionRequest,
            _sink: MarketUpdateSink,
        ) -> Result<SubscriptionHandle, ProviderError> {
            Err(ProviderError::Unsupported("fake provider has no stream"))
        }
    }

    /// Build a single-entry registry with one chain-capable provider under `id`.
    fn registry_with(id: &str) -> ProviderRegistry {
        let mut registry = ProviderRegistry::new();
        registry.insert(pid(id), Arc::new(FakeProvider::chainful(pid(id))));
        registry
    }

    // === register: collision paths ============================================

    #[test]
    fn test_register_duplicate_id_is_duplicate_error() {
        let result = ChainViewApp::builder()
            .register(FakeProvider::chainful(pid("mybroker")))
            .register(FakeProvider::chainful(pid("mybroker")))
            .with_config(live_config("mybroker"))
            .run();
        match result {
            Err(ChainViewError::Registry(RegistryError::DuplicateId(id))) => {
                assert_eq!(id.as_str(), "mybroker");
            }
            other => panic!("expected DuplicateId(mybroker), got {other:?}"),
        }
    }

    #[test]
    fn test_register_reserved_id_is_reserved_error() {
        // An EXTERNAL registration using a reserved built-in id is rejected, so a
        // downstream adapter can never masquerade as `deribit`.
        let result = ChainViewApp::builder()
            .register(FakeProvider::chainful(pid("deribit")))
            .with_config(live_config("deribit"))
            .run();
        match result {
            Err(ChainViewError::Registry(RegistryError::ReservedId(id))) => {
                assert_eq!(id.as_str(), "deribit");
            }
            other => panic!("expected ReservedId(deribit), got {other:?}"),
        }
    }

    #[test]
    fn test_register_duplicate_wins_over_later_reserved_first_error_wins() {
        // The FIRST build-phase error is the one reported: a duplicate recorded
        // before a later reserved-id registration surfaces as DuplicateId.
        let result = ChainViewApp::builder()
            .register(FakeProvider::chainful(pid("mybroker")))
            .register(FakeProvider::chainful(pid("mybroker")))
            .register(FakeProvider::chainful(pid("alpaca")))
            .with_config(live_config("mybroker"))
            .run();
        assert!(matches!(
            result,
            Err(ChainViewError::Registry(RegistryError::DuplicateId(_)))
        ));
    }

    // === empty registry =======================================================

    #[test]
    fn test_run_empty_registry_is_empty_error() {
        // A live startup with NO providers registered (no `with_builtins`, no
        // external `register`) finds an empty registry.
        let result = ChainViewApp::builder()
            .with_config(live_config("deribit"))
            .run();
        assert!(matches!(
            result,
            Err(ChainViewError::Registry(RegistryError::Empty))
        ));
    }

    #[test]
    fn test_with_builtins_registers_deribit_and_resolves_source() {
        // `with_builtins` now registers the real Deribit adapter (issue #15), so a
        // stock live startup selecting `deribit` resolves its chain source. `run`
        // stops after resolving the live source (the loop lands later), so a
        // successful resolution returns `Ok(())` without any network I/O.
        let result = ChainViewApp::builder()
            .with_builtins()
            .with_config(live_config("deribit"))
            .run();
        assert!(
            result.is_ok(),
            "expected deribit to resolve, got {result:?}"
        );
    }

    // === --provider resolution ================================================

    #[test]
    fn test_run_unknown_provider_is_unknown_provider_error() {
        // The selected id is grammar-valid but not registered.
        let result = ChainViewApp::builder()
            .register(FakeProvider::chainful(pid("mybroker")))
            .with_config(live_config("othervendor"))
            .run();
        match result {
            Err(ChainViewError::Config(ConfigError::UnknownProvider(id))) => {
                assert_eq!(id, "othervendor");
            }
            other => panic!("expected UnknownProvider(othervendor), got {other:?}"),
        }
    }

    #[test]
    fn test_provider_selection_invalid_grammar_is_invalid_value() {
        // A syntactically invalid `--provider` is `ConfigError::InvalidValue` at
        // the `ProviderId::new` grammar gate (issue #4) — BEFORE it can reach the
        // registry — so it can never construct a `config.provider` that reaches
        // `run`. This is the same gate config assembly reuses.
        match ProviderId::new("Bad-Upper") {
            Err(ConfigError::InvalidValue { field, .. }) => assert_eq!(field, "provider id"),
            other => panic!("expected InvalidValue on provider id, got {other:?}"),
        }
    }

    // === gated built-in =======================================================

    #[test]
    fn test_with_gated_builtin_fails_while_gate_holds() {
        // The gated request records the first (pending) error, surfaced by run()
        // even though a valid external provider is also registered.
        let result = ChainViewApp::builder()
            .with_gated_builtin(pid("tastytrade"))
            .register(FakeProvider::chainful(pid("mybroker")))
            .with_config(live_config("mybroker"))
            .run();
        match result {
            Err(ChainViewError::Registry(RegistryError::Gated(id))) => {
                assert_eq!(id.as_str(), "tastytrade");
            }
            other => panic!("expected Gated(tastytrade), got {other:?}"),
        }
    }

    #[test]
    fn test_with_builtins_enables_alpaca_when_configured() {
        // #99 gate lift (docs/SECURITY.md §2.4): pre-lift, with_builtins NEVER enabled
        // alpaca (it was reachable only through the gated opt-in). With the
        // credential-logging gate cleared by the captured-log proof
        // (src/providers/alpaca.rs `test_auth_subscribe_cycle_never_logs_credentials`,
        // corroborated by upstream alpaca-websocket/tests/log_redaction.rs),
        // with_builtins now registers alpaca as a REAL built-in WHEN its
        // CHAINVIEW_ALPACA_* credentials are configured — the inversion of the old
        // "never enables alpaca" gate assertion. The `alpaca` Cargo feature still
        // gates the heavy upstream deps, so the built-in exists only in an
        // `--features alpaca` build; a default build has no alpaca code to register.
        let env = alpaca_creds_env();
        let result = ChainViewApp::builder()
            .with_builtins_from_env(&env)
            .with_config(live_config("alpaca"))
            .run();
        #[cfg(feature = "alpaca")]
        assert!(
            result.is_ok(),
            "with the gate lifted, a configured alpaca resolves as a registered built-in: {result:?}"
        );
        #[cfg(not(feature = "alpaca"))]
        match result {
            // Without the feature the adapter is not compiled in, so it cannot be
            // registered; selecting it is a clean UnknownProvider.
            Err(ChainViewError::Config(ConfigError::UnknownProvider(id))) => {
                assert_eq!(id, "alpaca");
            }
            other => panic!("without the alpaca feature the adapter is not compiled in: {other:?}"),
        }
    }

    #[test]
    fn test_with_builtins_skips_unconfigured_alpaca_preserving_zero_config() {
        // Zero-config is preserved: with NO alpaca credentials, with_builtins does not
        // register alpaca (never a startup error), so Deribit stays the zero-config
        // default and selecting alpaca is a clean UnknownProvider. This holds in every
        // feature configuration (a missing credential simply omits the built-in).
        let result = ChainViewApp::builder()
            .with_builtins_from_env(&empty_env())
            .with_config(live_config("alpaca"))
            .run();
        match result {
            Err(ChainViewError::Config(ConfigError::UnknownProvider(id))) => {
                assert_eq!(id, "alpaca");
            }
            other => panic!("expected UnknownProvider(alpaca) when unconfigured, got {other:?}"),
        }
        // Deribit still resolves (the zero-config default is unaffected).
        let deribit = ChainViewApp::builder()
            .with_builtins_from_env(&empty_env())
            .with_config(live_config("deribit"))
            .run();
        assert!(
            deribit.is_ok(),
            "the zero-config deribit default is unaffected: {deribit:?}"
        );
    }

    /// An environment carrying valid `CHAINVIEW_IG_*` credentials.
    fn ig_creds_env() -> TestEnv {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_IG_USERNAME".to_owned(), "alice".to_owned());
        let _ = env.insert("CHAINVIEW_IG_PASSWORD".to_owned(), "test-pw".to_owned());
        let _ = env.insert("CHAINVIEW_IG_API_KEY".to_owned(), "test-key".to_owned());
        TestEnv(env)
    }

    #[test]
    fn test_with_builtins_enables_ig_when_configured() {
        // Issue #39: IG's `ig` feature is a DEPENDENCY-WEIGHT gate (ADR-0013), NOT a
        // security gate, so with_builtins registers IG as a REAL built-in WHEN its
        // CHAINVIEW_IG_* credentials are configured — the flip of the old
        // "ig is never built / deferred" assertion. IG has a Partial (navigation)
        // chain, so it resolves as a live source. Under `--features ig` a configured
        // IG resolves; without the feature it is not compiled in (clean
        // UnknownProvider). Zero-config Deribit is unaffected either way.
        let env = ig_creds_env();
        let result = ChainViewApp::builder()
            .with_builtins_from_env(&env)
            .with_config(live_config("ig"))
            .run();
        #[cfg(feature = "ig")]
        assert!(
            result.is_ok(),
            "a configured IG resolves as a registered built-in under --features ig: {result:?}"
        );
        #[cfg(not(feature = "ig"))]
        match result {
            Err(ChainViewError::Config(ConfigError::UnknownProvider(id))) => {
                assert_eq!(id, "ig");
            }
            other => panic!("without the ig feature the adapter is not compiled in: {other:?}"),
        }
    }

    #[test]
    fn test_with_builtins_skips_unconfigured_ig_preserving_zero_config() {
        // With NO IG credentials, with_builtins omits IG (never a startup error), so
        // selecting `ig` is a clean UnknownProvider and Deribit stays the zero-config
        // default (the alpaca #99 pattern). Holds in every feature configuration.
        let result = ChainViewApp::builder()
            .with_builtins_from_env(&empty_env())
            .with_config(live_config("ig"))
            .run();
        match result {
            Err(ChainViewError::Config(ConfigError::UnknownProvider(id))) => {
                assert_eq!(id, "ig");
            }
            other => panic!("expected UnknownProvider(ig) when unconfigured, got {other:?}"),
        }
        let deribit = ChainViewApp::builder()
            .with_builtins_from_env(&empty_env())
            .with_config(live_config("deribit"))
            .run();
        assert!(
            deribit.is_ok(),
            "the zero-config deribit default is unaffected: {deribit:?}"
        );
    }

    #[test]
    fn test_with_gated_builtin_dxlink_fails_while_gate_holds() {
        // The standalone DXLink overlay (issue #42) is reachable only through the
        // gated opt-in, which fails with a typed startup error while its gate holds —
        // a stock binary can never enable it (docs/SECURITY.md §2.4).
        let result = ChainViewApp::builder()
            .with_gated_builtin(pid("dxlink"))
            .register(FakeProvider::chainful(pid("mybroker")))
            .with_config(live_config("mybroker"))
            .run();
        match result {
            Err(ChainViewError::Registry(RegistryError::Gated(id))) => {
                assert_eq!(id.as_str(), "dxlink");
            }
            other => panic!("expected Gated(dxlink), got {other:?}"),
        }
    }

    #[test]
    fn test_with_builtins_never_enables_dxlink() {
        // The stock builder registers only the gate-clear built-ins (Deribit), so
        // selecting the gated `dxlink` id resolves to UnknownProvider — proving the
        // overlay-only adapter is never enabled by `with_builtins` (docs/SECURITY.md
        // §2.4). (A chain-less source would separately be rejected by the
        // composite-source guard; here it is simply unregistered.)
        let result = ChainViewApp::builder()
            .with_builtins()
            .with_config(live_config("dxlink"))
            .run();
        match result {
            Err(ChainViewError::Config(ConfigError::UnknownProvider(id))) => {
                assert_eq!(id, "dxlink");
            }
            other => panic!("expected UnknownProvider(dxlink), got {other:?}"),
        }
    }

    // === source resolution: capabilities read once, wired into SourceBinding ==

    #[test]
    fn test_run_resolves_live_source_reading_capabilities_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = FakeProvider {
            id: pid("mybroker"),
            capabilities: chainful_caps(),
            capability_calls: Arc::clone(&calls),
        };
        let result = ChainViewApp::builder()
            .register(provider)
            .with_config(live_config("mybroker"))
            .run();
        assert!(
            result.is_ok(),
            "a registered chain source resolves: {result:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "capabilities are read exactly once at startup"
        );
    }

    #[test]
    fn test_resolve_source_wires_declared_capabilities_into_source_binding() {
        let registry = registry_with("mybroker");
        let config = live_config("mybroker");
        match resolve_source(&registry, &config) {
            Ok((provider, binding)) => {
                assert_eq!(provider.id().as_str(), "mybroker");
                assert_eq!(binding.provider.as_str(), "mybroker");
                // The UI-facing seam carries the DECLARED capabilities, never the
                // `dyn Provider`/registry.
                assert_eq!(binding.capabilities, chainful_caps());
                assert!(matches!(
                    binding.health,
                    StreamHealth::Reconnecting { attempt: 1 }
                ));
            }
            Err(e) => panic!("expected a resolved SourceBinding, got {e}"),
        }
    }

    #[test]
    fn test_resolve_chainless_source_without_overlay_is_invalid_value() {
        // The composite-source guard reads CAPABILITIES (chain == None), never the
        // id: a chain-less provider cannot be a live source on its own.
        let mut registry = ProviderRegistry::new();
        registry.insert(
            pid("myoverlay"),
            Arc::new(FakeProvider::new(pid("myoverlay"), chainless_caps())),
        );
        match resolve_source(&registry, &live_config("myoverlay")) {
            Err(ChainViewError::Config(ConfigError::InvalidValue { field, .. })) => {
                assert_eq!(field, "provider");
            }
            // `dyn Provider` is not `Debug`, so report the error text (or "Ok")
            // rather than debug-printing the resolved tuple.
            Err(other) => panic!("expected InvalidValue on provider, got {other}"),
            Ok(_) => panic!("expected InvalidValue on provider, got a resolved source"),
        }
    }

    // === Replay mode needs no live provider ===================================

    #[test]
    fn test_run_replay_mode_ignores_empty_registry() {
        // Replay reads a bundle read-only; an empty registry is fine.
        let result = ChainViewApp::builder()
            .with_config(replay_config("/bundle"))
            .run();
        assert!(result.is_ok(), "replay needs no live provider: {result:?}");
    }

    // === The registry is application-owned; the UI-facing seam is caps + id ===

    #[test]
    fn test_ui_facing_seam_is_capabilities_not_the_registry() {
        // `run` consumes the builder (and the registry) by value; the value the UI
        // reads for gating is a `SourceBinding` carrying a `ProviderCapabilities`
        // and a `ProviderId` — never a `ProviderRegistry` or a `dyn Provider`. So
        // the registry is structurally unreachable from the UI (the arch test that
        // enforces no `src/ui/*` import lands in #22).
        let binding = SourceBinding::new(pid("mybroker"), chainful_caps(), StreamHealth::Live);
        let _caps: ProviderCapabilities = binding.capabilities;
        let _id: ProviderId = binding.provider;
        // Gating over the binding's capabilities never consults the id.
        assert!(is_screen_reachable(LiveScreen::Chain, &chainful_caps()));
    }

    #[test]
    fn test_registry_debug_lists_sorted_ids_only() {
        let mut registry = ProviderRegistry::new();
        registry.insert(pid("zeta"), Arc::new(FakeProvider::chainful(pid("zeta"))));
        registry.insert(
            pid("mybroker"),
            Arc::new(FakeProvider::chainful(pid("mybroker"))),
        );
        let rendered = format!("{registry:?}");
        // Sorted, ids only (no secret, no provider internals).
        assert!(rendered.contains("mybroker"));
        assert!(rendered.contains("zeta"));
        let mybroker = rendered.find("mybroker");
        let zeta = rendered.find("zeta");
        assert!(mybroker < zeta, "ids are rendered sorted: {rendered}");
    }

    // === spawn_supervised_subscription: watched + per-provider cancel ==========

    /// A no-op final teardown (no real TTY) for the supervisor in these tests.
    struct NoopTeardown;

    impl FinalTeardown for NoopTeardown {
        fn run(self: Box<Self>) {}
    }

    #[track_caller]
    fn expiry() -> DateTime<Utc> {
        match DateTime::<Utc>::from_timestamp(1_751_011_200, 0) {
            Some(t) => t,
            None => panic!("valid fixed expiry"),
        }
    }

    /// A streaming faux provider whose `subscribe` sends a control-class
    /// `Health(Live)` through the real two-class [`MarketUpdateSink`] and then
    /// spawns a loop. `panic_mid_run` makes the loop panic immediately (with no
    /// external trigger) so the supervisor's `watch` seam is the only thing
    /// observing it; otherwise the loop is cooperative and stops on its child token.
    struct StreamingFake {
        id: ProviderId,
        panic_mid_run: bool,
    }

    #[async_trait]
    impl Provider for StreamingFake {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            chainful_caps()
        }

        async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
            Ok(vec![UnderlyingRef::new("BTC")])
        }

        async fn fetch_chain(
            &self,
            _underlying: &str,
            _expiration: &ExpirationDate,
        ) -> Result<ChainFetch, ProviderError> {
            Err(ProviderError::Unsupported("no chain in this test"))
        }

        async fn subscribe(
            &self,
            req: SubscriptionRequest,
            mut sink: MarketUpdateSink,
        ) -> Result<SubscriptionHandle, ProviderError> {
            let _ = sink
                .send(MarketUpdate::Health(self.id.clone(), StreamHealth::Live))
                .await;
            let cancel = req.cancel;
            let panic_mid_run = self.panic_mid_run;
            let loop_cancel = cancel.clone();
            let join = tokio::spawn(async move {
                if panic_mid_run {
                    panic!("mid-run provider panic");
                }
                loop_cancel.cancelled().await;
            });
            Ok(SubscriptionHandle::spawned(cancel, join))
        }
    }

    #[tokio::test]
    async fn test_spawn_supervised_subscription_returns_cancel_handle_and_joins_clean() {
        let provider: Arc<dyn Provider> = Arc::new(StreamingFake {
            id: pid("faux"),
            panic_mid_run: false,
        });
        let (_bridge, senders) = EventBridge::new(64);
        let mut supervisor = Supervisor::new(Box::new(NoopTeardown));
        let sub = spawn_supervised_subscription(
            &provider,
            "BTC",
            expiry(),
            Vec::<Instrument>::new(),
            &senders,
            &mut supervisor,
        )
        .await;
        let handle = match sub {
            Ok(Some(handle)) => handle,
            other => panic!("expected a supervised subscription handle, got {other:?}"),
        };
        assert_eq!(handle.provider().as_str(), "faux");
        // The per-provider cancel handle stops only this provider's subtree.
        handle.cancel();
        // A clean quit joins the watched loop in the ordered teardown.
        supervisor.request_quit();
        assert!(
            supervisor.run().await.is_clean(),
            "the supervised, cooperatively-cancelled loop joins clean"
        );
    }

    #[tokio::test]
    async fn test_spawn_supervised_provider_death_mid_run_wakes_supervisor() {
        // Closes Codex finding #2: a watched provider task that panics MID-RUN
        // (with NO external trigger) must wake the supervisor as fatal, not run
        // stale forever.
        let provider: Arc<dyn Provider> = Arc::new(StreamingFake {
            id: pid("faux"),
            panic_mid_run: true,
        });
        let (_bridge, senders) = EventBridge::new(64);
        let mut supervisor = Supervisor::new(Box::new(NoopTeardown));
        let sub = spawn_supervised_subscription(
            &provider,
            "BTC",
            expiry(),
            Vec::<Instrument>::new(),
            &senders,
            &mut supervisor,
        )
        .await;
        assert!(
            matches!(sub, Ok(Some(_))),
            "the streaming provider task is watched under the supervisor"
        );
        // No external trigger: only `watch` observes the mid-run panic.
        let cause = supervisor.run().await;
        assert_eq!(
            cause.exit_code(),
            1,
            "a watched provider panicking mid-run wakes the supervisor as fatal"
        );
    }

    #[tokio::test]
    async fn test_spawn_supervised_subscription_poll_only_is_none() {
        // A poll-only provider whose `subscribe` returns `Unsupported` has no
        // stream task to supervise.
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::chainful(pid("mybroker")));
        let (_bridge, senders): (EventBridge, BridgeSenders) = EventBridge::new(64);
        let mut supervisor = Supervisor::new(Box::new(NoopTeardown));
        let sub = spawn_supervised_subscription(
            &provider,
            "BTC",
            expiry(),
            Vec::<Instrument>::new(),
            &senders,
            &mut supervisor,
        )
        .await;
        assert!(
            matches!(sub, Ok(None)),
            "a poll-only provider has no supervised stream task"
        );
        supervisor.request_quit();
        assert!(supervisor.run().await.is_clean());
    }

    // === Property tests =======================================================

    /// A strategy over grammar-valid, NON-reserved provider ids.
    fn valid_custom_id() -> impl Strategy<Value = ProviderId> {
        "[a-z][a-z0-9]{1,10}"
            .prop_map(ProviderId::new)
            .prop_filter("valid, non-reserved id", |r| {
                r.as_ref().is_ok_and(|p| !p.is_reserved())
            })
            .prop_map(|r| match r {
                Ok(p) => p,
                // Filtered above; unreachable, but no unwrap per the ruleset.
                Err(e) => panic!("filtered id was invalid: {e}"),
            })
    }

    fn chain_capability(idx: u8) -> ChainCapability {
        match idx % 4 {
            0 => ChainCapability::Native,
            1 => ChainCapability::Assemble,
            2 => ChainCapability::Partial,
            _ => ChainCapability::None,
        }
    }

    fn greeks_capability(idx: u8) -> GreeksCapability {
        match idx % 3 {
            0 => GreeksCapability::Provided,
            1 => GreeksCapability::ComputedLocally,
            _ => GreeksCapability::None,
        }
    }

    proptest! {
        /// An external registration using ANY reserved built-in id is rejected.
        #[test]
        fn prop_registry_rejects_reserved_id(idx in 0usize..RESERVED_PROVIDER_IDS.len()) {
            let id_str = match RESERVED_PROVIDER_IDS.get(idx) {
                Some(s) => *s,
                None => return Ok(()),
            };
            let id = match ProviderId::new(id_str) {
                Ok(p) => p,
                Err(e) => panic!("reserved id `{id_str}` must be grammar-valid: {e}"),
            };
            let result = ChainViewApp::builder()
                .register(FakeProvider::chainful(id))
                .run();
            prop_assert!(matches!(
                result,
                Err(ChainViewError::Registry(RegistryError::ReservedId(_)))
            ));
        }

        /// Registering the same id twice is always a DuplicateId, never a silent
        /// last-writer-wins — the pending error is reported before config resolves.
        #[test]
        fn prop_registry_rejects_duplicate_id(id in valid_custom_id()) {
            let result = ChainViewApp::builder()
                .register(FakeProvider::chainful(id.clone()))
                .register(FakeProvider::chainful(id))
                .run();
            prop_assert!(matches!(
                result,
                Err(ChainViewError::Registry(RegistryError::DuplicateId(_)))
            ));
        }

        /// Every provider returns a COMPLETE `ProviderCapabilities` (the builder
        /// always fills every dimension), and screen gating is a TOTAL function of
        /// the declared capabilities — never the id. Building the caps and reading
        /// them back is lossless, and `is_screen_reachable` matches the
        /// capability-derived truth for all four screens.
        #[test]
        fn prop_capabilities_total(
            id in valid_custom_id(),
            depth in any::<bool>(),
            chain_idx in any::<u8>(),
            greeks_idx in any::<u8>(),
        ) {
            let chain = chain_capability(chain_idx);
            let greeks = greeks_capability(greeks_idx);
            let caps = ProviderCapabilities::builder()
                .chain(chain)
                .depth(depth)
                .greeks(greeks)
                .build();
            let provider = FakeProvider::new(id, caps);
            // A provider reports EXACTLY its declared, complete capabilities.
            prop_assert_eq!(provider.capabilities(), caps);
            // Gating is derived from capabilities alone.
            let chain_ok = !matches!(chain, ChainCapability::None);
            let greeks_ok = !matches!(greeks, GreeksCapability::None);
            prop_assert_eq!(is_screen_reachable(LiveScreen::Chain, &caps), chain_ok);
            prop_assert_eq!(is_screen_reachable(LiveScreen::Payoff, &caps), chain_ok);
            prop_assert_eq!(is_screen_reachable(LiveScreen::Depth, &caps), depth);
            prop_assert_eq!(is_screen_reachable(LiveScreen::Surface, &caps), greeks_ok);
        }
    }
}
