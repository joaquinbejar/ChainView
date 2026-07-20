//! The PUBLIC, semver-governed provider **port** (`docs/03-data-providers.md`
//! §2, §11.1, [ADR-0006], [SEMVER.md]).
//!
//! This module defines the seam between the heterogeneous upstream market-data
//! clients and the uniform ChainView domain model: the [`Provider`] trait
//! (`id` / `capabilities` / `discover` / `fetch_chain` / `subscribe`), the
//! capability self-declaration [`ProviderCapabilities`] with its builder and the
//! [`ChainCapability`] / [`GreeksCapability`] / [`OptionStreamCapability`] /
//! [`ChainPollCapability`] / [`AuthKind`] dimensions, and the port helper types
//! [`UnderlyingRef`] / [`SubscriptionRequest`] / [`SubscriptionHandle`].
//!
//! The normalized domain types the trait *emits* — [`ChainFetch`] and its
//! [`ExpirySource`] / [`AliasCatalog`], plus [`MarketUpdate`], [`ProviderError`]
//! and [`ProviderId`] — are DOMAIN/boundary types defined in the layers the trait
//! and the domain both depend on (`src/chain/*`, `src/error.rs`) and re-exported
//! at the crate root as part of the port surface an external adapter compiles
//! against (`docs/03-data-providers.md` §11.1). Defining them below the port
//! keeps the module graph acyclic — port → domain, never domain → port
//! (`docs/03-data-providers.md` §12).
//!
//! # The delivered external-integration surface
//!
//! This port is the **complete, semver-governed surface** an external developer
//! builds a broker integration against (`docs/03-data-providers.md` §11,
//! [ADR-0006], [SEMVER.md]). Every type an external `impl Provider` names — this
//! module's trait, capabilities, and helper types PLUS the emitted domain types
//! ([`ChainFetch`], [`QuoteUpdate`](crate::chain::QuoteUpdate),
//! [`GreeksRow`](crate::chain::GreeksRow),
//! [`DepthLadder`](crate::chain::DepthLadder),
//! [`MarketUpdate`](crate::chain::MarketUpdate), the identity types, and the
//! `optionstratlib` chain-model vocabulary) — is re-exported from the crate root,
//! so an external adapter compiles against `chainview::` alone. The registration
//! flow (`ChainViewApp::builder().with_builtins().register(..).run()`), the
//! reserved-id rule, and the `CHAINVIEW_<ID>_*` config namespacing are documented
//! in the crate-root docs (`src/lib.rs`) and `docs/03-data-providers.md` §11.
//!
//! # `#[non_exhaustive]` + builder = source-compatible extension
//!
//! [`ProviderCapabilities`] and every capability enum are `#[non_exhaustive]`,
//! and an out-of-crate adapter builds capabilities only through
//! [`ProviderCapabilities::builder`], never a struct literal. That representation
//! is what makes "add a new optional capability dimension" a **minor**,
//! source-compatible bump ([SEMVER.md]). In-crate UI code still matches the enums
//! exhaustively, so a new variant forces the gate to decide how it degrades.
//!
//! # `async_trait` allocation
//!
//! [`Provider`] uses [`async_trait`](macro@async_trait), which boxes and
//! allocates once per call.
//! Provider methods are **cold-path** (`discover`/`fetch_chain`/`subscribe`), not
//! per-tick — the hot render loop holds no `dyn Provider` — so the per-call
//! allocation is accepted (`docs/03-data-providers.md` §2).
//!
//! [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md
//! [SEMVER.md]: https://github.com/joaquinbejar/ChainView/blob/main/docs/SEMVER.md

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use optionstratlib::ExpirationDate;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::chain::{
    ChainFetch, DepthLadder, GreeksRow, Instrument, InstrumentKey, MarketUpdate, ProviderId,
    QuoteUpdate,
};
use crate::error::ProviderError;

/// The Deribit adapter — the zero-config, public-data poll leg (issue #15,
/// `docs/03-data-providers.md` §7.1). Crate-internal: it is registered through
/// [`ChainViewAppBuilder::with_builtins`](crate::ChainViewAppBuilder), never
/// named on the public surface, so no raw `deribit-http` DTO crosses the port.
pub(crate) mod deribit;

/// The **neutral**, shared dxfeed event-decode helpers (issue #38,
/// `docs/03-data-providers.md` §3, §12). Both the tastytrade adapter (#40) and
/// the standalone dxlink overlay (#42) map their upstream `dxfeed::Event` /
/// `MarketEvent` onto this module's neutral input views and call its
/// `decode_quote` / `decode_greeks`, so the DXLink adapter reuses the tastytrade
/// decode path **without** an adapter-to-adapter edge — both depend on THIS
/// module, neither on the other (the module-map hard rule). It is a neutral
/// provider-layer helper: no `Provider` impl, no upstream crate dependency, and
/// it never `use`s a sibling adapter, `src/app.rs`, or `src/ui/*`.
///
/// The consumers wired in are the tastytrade adapter (#40) and the standalone
/// dxlink overlay (#42): both decode their dxfeed `Quote`/`Greeks` events through
/// `decode_quote` / `decode_greeks`. Each sits behind its own DISABLED-by-default
/// feature (`tastytrade` / `dxlink`), so the shared decode's entry points have a
/// production caller when EITHER feature is on. The `dead_code` allow is therefore
/// **narrowed** (not removed) to `not(any(feature = "tastytrade", feature =
/// "dxlink"))`: with either feature on, `decode_quote` / `decode_greeks` and the
/// neutral view fields they read have real callers (the #38 time-boxed blanket
/// allow is gone); with both off — the default build, where every consumer is
/// compiled out — the helpers are exercised solely by this module's own fixture +
/// property tests, so the allow keeps `-D warnings` clean. The residual
/// `clamp_symbol` + `symbol` echo still awaits the deferred tracing sink
/// (governance deviation 3) in BOTH adapters, so those two carry their own
/// narrowed per-item allow (see each site) rather than a real caller.
#[cfg_attr(not(any(feature = "tastytrade", feature = "dxlink")), allow(dead_code))]
pub(crate) mod dxfeed_decode;

/// The tastytrade adapter — the poll->stream merge provider (issue #40,
/// `docs/03-data-providers.md` §7.2). Behind the DISABLED-by-default `tastytrade`
/// Cargo feature and **excluded from `with_builtins()`**: the published
/// `tastytrade` 0.3.0 (the checksum-pinned artifact ChainView resolves) logs
/// credential material at `DEBUG` (`docs/SECURITY.md` §2.1), so it is reachable only through
/// the explicit `with_gated_builtin(id)` opt-in, which fails with a typed startup
/// error while the gate holds — a stock binary can never execute that logging
/// (`docs/SECURITY.md` §2/§3). Crate-internal: no raw `tastytrade` DTO crosses the
/// port. It maps its bundled dxfeed events onto the neutral [`dxfeed_decode`]
/// views (never an adapter-to-adapter edge).
#[cfg(feature = "tastytrade")]
pub(crate) mod tastytrade;

/// The Alpaca adapter — the composed, completeness-provable poll->stream provider
/// (issues #41, #99, `docs/03-data-providers.md` §7.5). Its upstream
/// credential-logging security gate is **lifted** (`docs/SECURITY.md` §2.4): the
/// pinned `alpaca-websocket 0.6.0` masks the key and never logs the secret, proven by
/// a captured-log test at the ChainView boundary
/// (`alpaca::tests::test_auth_subscribe_cycle_never_logs_credentials`). So under this
/// feature it is a **real built-in** — `with_builtins()` registers it when its
/// `CHAINVIEW_ALPACA_*` credentials are configured (omitting it, never erroring, when
/// absent). It stays behind the DISABLED-by-default `alpaca` Cargo feature only to
/// keep the heavy upstream deps out of a default build. Crate-internal: no raw
/// `alpaca-http` / `alpaca-websocket` DTO crosses the port — every upstream struct is
/// normalized to the domain model inside this module.
#[cfg(feature = "alpaca")]
pub(crate) mod alpaca;

/// The standalone DXLink adapter — the **overlay-only** quote/Greeks provider
/// (issue #42, `docs/03-data-providers.md` §7.3). Behind the DISABLED-by-default
/// `dxlink` Cargo feature and **excluded from `with_builtins()`**: like tastytrade
/// and Alpaca it is reachable only through the explicit `with_gated_builtin(id)`
/// opt-in, which fails with a typed startup error while the gate holds
/// (`docs/SECURITY.md` §2.4). It has **no chain discovery** (`chain: None`) — its
/// `discover`/`fetch_chain` return [`ProviderError::Unsupported`], so it is usable
/// only as a symbol-level overlay onto **another** provider's chain. It maps the
/// `dxlink` crate's typed `MarketEvent::{Quote,Greeks}` onto the neutral
/// [`dxfeed_decode`] views (never an adapter-to-adapter edge — it depends on that
/// shared module, never on the tastytrade adapter). Crate-internal: no raw `dxlink`
/// DTO crosses the port.
#[cfg(feature = "dxlink")]
pub(crate) mod dxlink;

/// The seam every adapter implements: one trait, one adapter per provider id
/// (`docs/03-data-providers.md` §2).
///
/// Together with [`ProviderCapabilities`] (and its dimension enums) and every
/// normalized domain type it emits, this trait is the **public, semver-governed
/// provider port** — the surface an external developer compiles against to plug
/// in their own venue ([ADR-0006], [SEMVER.md]). A trait-signature change is a
/// major bump; adding an optional capability dimension is minor.
///
/// The trait object [`Box<dyn Provider>`] is `Send + Sync` (registry-ready). The
/// [`async_trait`](macro@async_trait) per-call allocation is accepted — every
/// method is cold-path.
///
/// [ADR-0006]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md
/// [SEMVER.md]: https://github.com/joaquinbejar/ChainView/blob/main/docs/SEMVER.md
#[async_trait]
pub trait Provider: Send + Sync {
    /// The provider's own id — the registry key, config-namespace segment, and
    /// log label (`docs/01-domain-model.md` §4). An open, validated newtype; the
    /// registry (issue #12) rejects a reserved built-in id from an external
    /// adapter.
    fn id(&self) -> ProviderId;

    /// What this provider can and cannot do — the honest, static capability
    /// self-declaration the UI gates screens on (`docs/03-data-providers.md` §2,
    /// §8). It declares exactly what the adapter delivers and never claims a
    /// capability the upstream client cannot back with real data.
    fn capabilities(&self) -> ProviderCapabilities;

    /// List the underlyings this provider offers (and, where cheap, their
    /// expirations).
    ///
    /// # Errors
    ///
    /// [`ProviderError::Unsupported`] for a chain-less provider (standalone
    /// dxlink), or a transport/auth failure. The error is redaction-safe by
    /// construction — never a credential or a raw upstream string
    /// (`docs/03-data-providers.md` §6).
    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError>;

    /// Fetch a full chain snapshot for one `(underlying, expiration)` as the
    /// NAMED [`ChainFetch`] artifact — **never** a bare `OptionChain` — so the
    /// poll leg preserves the absolute-UTC expiry/source identity and the per-leg
    /// [`AliasCatalog`](crate::chain::AliasCatalog) the merge, subscription,
    /// resubscription, and DXLink overlay joins need
    /// (`docs/03-data-providers.md` §2, §4). This is the poll leg — always
    /// available where a chain exists.
    ///
    /// # Errors
    ///
    /// [`ProviderError::NoChain`] when no chain exists for the pair,
    /// [`ProviderError::Unsupported`] for a chain-less provider, or a
    /// transport/normalize/auth failure — all redaction-safe.
    async fn fetch_chain(
        &self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError>;

    /// Open a streaming subscription; normalized [`MarketUpdate`]s are pushed into
    /// `sink` until the returned [`SubscriptionHandle`] is dropped (or the loop's
    /// [`SubscriptionRequest::cancel`] token is cancelled). The adapter owns the
    /// reconnect/resubscribe loop behind the handle and routes every update through
    /// the two-class [`MarketUpdateSink`] — control-class (`Chain`/`Health`)
    /// await-sent, coalesced-class (`Quote`/`Greeks`/`Depth`) producer-staged
    /// last-value-wins ([ADR-0009], `docs/03-data-providers.md` §5). A poll-only
    /// provider returns [`ProviderError::Unsupported`].
    ///
    /// # Errors
    ///
    /// [`ProviderError::Unsupported`] for a poll-only provider, or a
    /// transport/auth failure.
    ///
    /// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        sink: MarketUpdateSink,
    ) -> Result<SubscriptionHandle, ProviderError>;
}

/// The honest, static capability self-declaration a [`Provider`] returns
/// (`docs/03-data-providers.md` §2). Streaming is **three independent
/// dimensions** — [`option_stream`](Self::option_stream),
/// [`underlying_stream`](Self::underlying_stream), and
/// [`chain_poll`](Self::chain_poll) — so a real-time underlying is never mistaken
/// for a real-time option chain.
///
/// `#[non_exhaustive]`: an external adapter never uses a struct literal to build
/// this — it constructs through [`ProviderCapabilities::builder`], so ChainView
/// can add a new optional dimension (a field with a safe default) as a **minor**
/// bump without breaking a downstream adapter's construction ([SEMVER.md]).
///
/// [SEMVER.md]: https://github.com/joaquinbejar/ChainView/blob/main/docs/SEMVER.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProviderCapabilities {
    /// How (or whether) the provider produces a chain.
    pub chain: ChainCapability,
    /// Whether the provider delivers order-book depth for the selected
    /// instrument class (option chains in v1).
    pub depth: bool,
    /// Whether Greeks/IV are venue-provided, computed locally, or unavailable.
    pub greeks: GreeksCapability,
    /// The contract-level quote/Greek stream that overlays onto a chain.
    pub option_stream: OptionStreamCapability,
    /// Whether the provider streams the underlying/index price.
    pub underlying_stream: bool,
    /// How the chain structure is kept current (REST polling cadence).
    pub chain_poll: ChainPollCapability,
    /// Whether the provider exposes a normalized public trades tape.
    pub trades_tape: bool,
    /// The authentication the provider requires.
    pub auth: AuthKind,
}

impl ProviderCapabilities {
    /// The **only** cross-crate construction path (`docs/03-data-providers.md`
    /// §2). Every dimension starts at its safe, least-capable default, so a
    /// future new field lands with a default that keeps existing external
    /// adapters compiling and honest.
    ///
    /// # Examples
    ///
    /// ```
    /// use chainview::{ChainCapability, ChainPollCapability, ProviderCapabilities};
    ///
    /// let caps = ProviderCapabilities::builder()
    ///     .chain(ChainCapability::Assemble)
    ///     .depth(true)
    ///     .chain_poll(ChainPollCapability::Poll { interval_hint_secs: 2 })
    ///     .build();
    /// assert!(caps.depth);
    /// assert_eq!(caps.chain, ChainCapability::Assemble);
    /// ```
    #[must_use]
    pub fn builder() -> ProviderCapabilitiesBuilder {
        ProviderCapabilitiesBuilder::default()
    }
}

/// The builder for [`ProviderCapabilities`] — the only cross-crate construction
/// path (`docs/03-data-providers.md` §2). Every field is settable; any left
/// unset keeps its safe, least-capable default.
#[derive(Debug, Clone, Default)]
pub struct ProviderCapabilitiesBuilder {
    chain: ChainCapability,
    depth: bool,
    greeks: GreeksCapability,
    option_stream: OptionStreamCapability,
    underlying_stream: bool,
    chain_poll: ChainPollCapability,
    trades_tape: bool,
    auth: AuthKind,
}

impl ProviderCapabilitiesBuilder {
    /// Set how the provider produces a chain.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn chain(mut self, chain: ChainCapability) -> Self {
        self.chain = chain;
        self
    }

    /// Set whether the provider delivers order-book depth.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn depth(mut self, depth: bool) -> Self {
        self.depth = depth;
        self
    }

    /// Set whether Greeks/IV are venue-provided, computed locally, or absent.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn greeks(mut self, greeks: GreeksCapability) -> Self {
        self.greeks = greeks;
        self
    }

    /// Set the contract-level option quote/Greek stream capability.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn option_stream(mut self, option_stream: OptionStreamCapability) -> Self {
        self.option_stream = option_stream;
        self
    }

    /// Set whether the provider streams the underlying/index price.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn underlying_stream(mut self, underlying_stream: bool) -> Self {
        self.underlying_stream = underlying_stream;
        self
    }

    /// Set how the chain structure is kept current by polling.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn chain_poll(mut self, chain_poll: ChainPollCapability) -> Self {
        self.chain_poll = chain_poll;
        self
    }

    /// Set whether the provider exposes a normalized public trades tape.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn trades_tape(mut self, trades_tape: bool) -> Self {
        self.trades_tape = trades_tape;
        self
    }

    /// Set the authentication the provider requires.
    #[must_use = "builders do nothing unless .build() is called"]
    pub fn auth(mut self, auth: AuthKind) -> Self {
        self.auth = auth;
        self
    }

    /// Finalize the capability set. Every dimension is populated — either
    /// explicitly set above or left at its safe default — so the result is always
    /// complete.
    #[must_use]
    pub fn build(self) -> ProviderCapabilities {
        ProviderCapabilities {
            chain: self.chain,
            depth: self.depth,
            greeks: self.greeks,
            option_stream: self.option_stream,
            underlying_stream: self.underlying_stream,
            chain_poll: self.chain_poll,
            trades_tape: self.trades_tape,
            auth: self.auth,
        }
    }
}

/// How a provider produces a chain (`docs/03-data-providers.md` §2, §8).
///
/// `#[non_exhaustive]`: an adapter *constructs* a variant (always compiles),
/// while only in-crate UI code *matches* on it (still exhaustiveness-checked), so
/// a new variant is a minor, source-compatible addition. The default is the
/// least-capable [`None`](Self::None).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum ChainCapability {
    /// A real chain endpoint (tastytrade, Alpaca).
    Native,
    /// Built from an instrument list (Deribit).
    Assemble,
    /// Built from a navigation tree, no native strike/expiry model (IG).
    Partial,
    /// No chain discovery at all — overlay-only (standalone dxlink):
    /// `discover`/`fetch_chain` return [`ProviderError::Unsupported`].
    #[default]
    None,
}

/// Whether a provider supplies Greeks/IV, computes them locally, or has none
/// (`docs/03-data-providers.md` §2, §8).
///
/// `#[non_exhaustive]`; default [`None`](Self::None).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum GreeksCapability {
    /// Venue-supplied Greeks/IV (Deribit ticker, dxfeed Greeks, Alpaca snapshot).
    Provided,
    /// Computed locally by ChainView via `optionstratlib` (IG) — badged in the
    /// UI as locally-computed analytics.
    ComputedLocally,
    /// No Greeks/IV available.
    #[default]
    None,
}

/// Contract-level streaming: does the provider stream **option-contract**
/// quotes/Greeks that overlay onto a chain (`docs/03-data-providers.md` §2, §8)?
///
/// `verified: false` means "the stream exists upstream but ChainView has no
/// recorded fixture yet, so snapshot polling is the supported live path" — a cell
/// flips to `verified: true` only in the PR that lands its fixture (§8 gate).
///
/// `#[non_exhaustive]`; default [`None`](Self::None).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OptionStreamCapability {
    /// No contract-level stream (Alpaca — its WebSocket carries only the
    /// underlying).
    #[default]
    None,
    /// Individual symbols only, with no chain of its own — usable purely as an
    /// overlay onto an external chain source (standalone dxlink).
    SymbolOnly {
        /// Whether a recorded fixture proves the decode (§8 gate).
        verified: bool,
    },
    /// Contract quotes/Greeks that overlay onto an assembled chain.
    ChainQuotes {
        /// Whether a recorded fixture proves the decode (§8 gate).
        verified: bool,
    },
}

/// How the chain structure is kept current — orthogonal to the streams
/// (`docs/03-data-providers.md` §2, §8). Alpaca, for instance, always polls the
/// option chain even though its WebSocket streams the underlying.
///
/// `#[non_exhaustive]`; default [`None`](Self::None).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ChainPollCapability {
    /// The chain is never polled (overlay-only providers).
    #[default]
    None,
    /// The chain is refreshed by REST on this cadence.
    Poll {
        /// The suggested refresh cadence, in **seconds**. A hint only — the
        /// effective interval is `config.refresh_interval`.
        interval_hint_secs: u32,
    },
}

/// The authentication a provider requires (`docs/03-data-providers.md` §2, §8).
///
/// `#[non_exhaustive]`; default [`None`](Self::None) — the zero-config Deribit
/// public-data path needs no credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum AuthKind {
    /// No authentication — public data (Deribit public endpoints).
    #[default]
    None,
    /// A bearer token (dxlink).
    Token,
    /// An API key + secret pair (Alpaca).
    KeySecret,
    /// A username + password (IG, tastytrade).
    UserPass,
}

/// One underlying a provider offers, with its expirations where the provider
/// surfaces them cheaply (`docs/03-data-providers.md` §2). [`Provider::discover`]
/// returns a list of these.
#[derive(Debug, Clone)]
pub struct UnderlyingRef {
    /// The canonical upper-case underlying ticker (`"BTC"`, `"SPY"`).
    pub underlying: String,
    /// The expirations known at discovery time, or empty when the provider does
    /// not surface them cheaply (the caller then resolves one via `fetch_chain`).
    pub expirations: Vec<ExpirationDate>,
}

impl UnderlyingRef {
    /// An underlying with no pre-listed expirations.
    #[must_use]
    pub fn new(underlying: impl Into<String>) -> Self {
        Self {
            underlying: underlying.into(),
            expirations: Vec::new(),
        }
    }

    /// An underlying carrying the expirations discovered alongside it.
    #[must_use]
    pub fn with_expirations(
        underlying: impl Into<String>,
        expirations: Vec<ExpirationDate>,
    ) -> Self {
        Self {
            underlying: underlying.into(),
            expirations,
        }
    }
}

/// The request to open a streaming subscription for one chain
/// (`docs/03-data-providers.md` §2). Scoped to one `(underlying, expiry)`; the
/// `instruments` are the legs to (re)subscribe, taken from the
/// [`ChainFetch::aliases`](crate::chain::ChainFetch) the poll leg returned so the
/// adapter never re-derives symbols (§4).
#[derive(Debug, Clone)]
pub struct SubscriptionRequest {
    /// The canonical upper-case underlying ticker.
    pub underlying: String,
    /// The absolute-UTC expiry the subscription is scoped to.
    pub expiration_utc: DateTime<Utc>,
    /// The legs to (re)subscribe, each carrying its native/stream symbols from
    /// the alias catalog.
    pub instruments: Vec<Instrument>,
    /// The per-provider cancellation token the reconnect loop selects on
    /// ([ADR-0009]). It is the supervisor's [`child_token`] for this provider:
    /// the root cancel cascades to it (clean shutdown), and a per-provider
    /// `Unsubscribe`/`Rediscover` cancels **only** this subtree. The composition
    /// seam ([`spawn_supervised_subscription`](crate::spawn_supervised_subscription))
    /// mints it and keeps a clone for the mid-run per-provider cancel.
    ///
    /// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
    /// [`child_token`]: crate::Supervisor
    pub cancel: CancellationToken,
}

impl SubscriptionRequest {
    /// Construct a subscription request scoped to one `(underlying, expiry)` over
    /// the given legs, carrying the supervisor-owned per-provider `cancel` token
    /// the reconnect loop selects on.
    #[must_use]
    pub fn new(
        underlying: impl Into<String>,
        expiration_utc: DateTime<Utc>,
        instruments: Vec<Instrument>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            underlying: underlying.into(),
            expiration_utc,
            instruments,
            cancel,
        }
    }
}

/// A handle to a live streaming subscription (`docs/03-data-providers.md` §2,
/// §5).
///
/// Dropping it cancels the subscription: the adapter's reconnect/resubscribe loop
/// observes the cancellation and tears the stream down. The cancellation
/// mechanism is adapter-internal — the handle carries a `Send` cancel closure, so
/// the port stays agnostic to whether the adapter aborts a task, drops a oneshot,
/// or flips a watch flag.
#[must_use = "dropping the handle immediately cancels the subscription"]
pub struct SubscriptionHandle {
    /// The cancellation action, run once on drop or [`abort`](Self::abort).
    /// `None` after it has run (or after [`take_join_handle`](Self::take_join_handle)
    /// detaches it, since the supervisor then owns the lifecycle), so the action
    /// fires at most once.
    cancel: Option<Box<dyn FnOnce() + Send>>,
    /// The spawned reconnect-loop join handle, present only for a
    /// [`spawned`](Self::spawned) handle. The composition seam takes it via
    /// [`take_join_handle`](Self::take_join_handle) and hands it to the supervisor,
    /// which then owns the join (an ordered, bounded join, not just a drop-abort).
    join: Option<JoinHandle<()>>,
}

impl SubscriptionHandle {
    /// Build a handle from the adapter's cancellation action, which runs once
    /// when the handle is dropped (or [`abort`](Self::abort)ed). Used by an
    /// adapter that manages its own task lifecycle without exposing a join handle.
    #[must_use = "dropping the handle immediately cancels the subscription"]
    pub fn new<F>(on_cancel: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            cancel: Some(Box::new(on_cancel)),
            join: None,
        }
    }

    /// Build a handle over a spawned reconnect-loop task ([ADR-0009]): the
    /// `cancel` token stops the loop cooperatively (a drop/`abort` cancels it),
    /// and `join` is carried so the supervised composition seam can take it via
    /// [`take_join_handle`](Self::take_join_handle) and register it for an ordered,
    /// bounded join. Dropping the handle **before** the join is taken cancels the
    /// token (the RAII backstop); once the join is taken, the supervisor owns the
    /// lifecycle.
    ///
    /// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
    #[must_use = "dropping the handle immediately cancels the subscription"]
    pub fn spawned(cancel: CancellationToken, join: JoinHandle<()>) -> Self {
        Self {
            cancel: Some(Box::new(move || cancel.cancel())),
            join: Some(join),
        }
    }

    /// Take the spawned loop's [`JoinHandle`] out of the handle for the supervisor
    /// to own, **detaching** the RAII cancel action at the same time — the
    /// supervisor's watched join + the request's [`cancel`](SubscriptionRequest::cancel)
    /// token now own the lifecycle, so a subsequent drop of this handle must not
    /// also cancel the loop out from under the supervisor ([ADR-0009]). Returns
    /// `None` for a [`new`](Self::new)-style handle (no join to transfer).
    ///
    /// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
    pub fn take_join_handle(&mut self) -> Option<JoinHandle<()>> {
        let join = self.join.take();
        if join.is_some() {
            // The supervisor owns the lifecycle now: detach the RAII cancel so a
            // later drop of this husk does not cancel the supervised loop.
            self.cancel = None;
        }
        join
    }

    /// Cancel the subscription now, without waiting for the handle to drop. The
    /// cancellation action runs at most once, whether via `abort` or `drop`.
    pub fn abort(mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

impl fmt::Debug for SubscriptionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionHandle")
            .field("active", &self.cancel.is_some())
            .field("supervised", &self.join.is_some())
            .finish()
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// MarketUpdateSink: the two-class adapter-side facade (ADR-0009).
// ---------------------------------------------------------------------------

/// Whether the bounded fan-in channel is still open. A closed channel means the
/// consumer (the app) is gone, so the adapter's reconnect loop shuts down rather
/// than reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SendState {
    /// The update was accepted (delivered, or staged into a producer slot).
    Open,
    /// The channel is closed — the consumer dropped its receiver.
    Closed,
}

/// The two-class sender an adapter's [`subscribe`](Provider::subscribe) sends
/// every [`MarketUpdate`] into ([ADR-0009], `docs/02-tui-architecture.md` §5).
///
/// It is the missing **adapter-side facade** that reconciles the one-sender port
/// with the two physical bridge channels: it routes **internally by class** so the
/// consumer bridge's priority + coalescing guarantees are honest end to end.
///
/// - **Control class** — [`Chain`](MarketUpdate::Chain) /
///   [`Health`](MarketUpdate::Health): **await-sent** onto the small priority
///   control channel, never coalesced or dropped, so a `Health(Reconnecting)` or a
///   reconnect backfill `Chain` is delivered promptly even under a quote burst.
/// - **Coalesced class** — [`Quote`](MarketUpdate::Quote) /
///   [`Greeks`](MarketUpdate::Greeks) / [`Depth`](MarketUpdate::Depth): routed
///   through a per-[`InstrumentKey`] **producer-side overwrite-on-full staging
///   map** onto the coalesced channel, so the freshest value per instrument
///   survives sustained saturation (the NFR-15 latest-value-wins completion). The
///   staging is O(N subscribed) and reuses its allocation across bursts.
///
/// # Epoch on reconnect ([ADR-0009], the sink-staging fix)
///
/// The sink is created **once per subscription** and outlives individual socket
/// drops, so a value staged before an outage would otherwise flush over the fresh
/// REST reconnect backfill `Chain` (a stale quote overwriting the current state,
/// since a poll does not advance the store's per-instrument event watermark). The
/// adapter's reconnect loop clears the coalesced staging on a disconnect/refetch
/// (the crate-internal `epoch` seam), so pre-outage staged values never flush over
/// the backfill.
#[derive(Debug)]
pub struct MarketUpdateSink {
    /// The control channel (`Chain` / `Health`), drained first by the bridge.
    tx_control: mpsc::Sender<MarketUpdate>,
    /// The coalesced channel (`Quote` / `Greeks` / `Depth`).
    tx_coalesced: mpsc::Sender<MarketUpdate>,
    /// The producer-side overwrite-on-full conflater for the coalesced class.
    staging: ProducerStaging,
}

impl MarketUpdateSink {
    /// Build a sink over the bridge's two producer senders. Constructed by
    /// [`BridgeSenders::market_update_sink`](crate::BridgeSenders::market_update_sink).
    #[must_use]
    pub fn new(
        tx_control: mpsc::Sender<MarketUpdate>,
        tx_coalesced: mpsc::Sender<MarketUpdate>,
    ) -> Self {
        Self {
            tx_control,
            tx_coalesced,
            staging: ProducerStaging::new(),
        }
    }

    /// Send one update, routing **internally by class** ([ADR-0009]). Control-class
    /// (`Chain`/`Health`) is await-sent on the priority channel; coalesced-class
    /// (`Quote`/`Greeks`/`Depth`) is producer-staged onto the coalesced channel.
    /// The match is total over the closed [`MarketUpdate`] set with no wildcard arm.
    ///
    /// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
    pub async fn send(&mut self, update: MarketUpdate) -> SendState {
        match update {
            control @ (MarketUpdate::Chain(_) | MarketUpdate::Health(_, _)) => {
                self.send_control(control).await
            }
            coalesced @ (MarketUpdate::Quote(_)
            | MarketUpdate::Greeks(_)
            | MarketUpdate::Depth(_)) => self.publish_coalesced(coalesced),
        }
    }

    /// Await-send a control-class update onto the priority channel — never
    /// coalesced, never dropped. The caller races this against its cancel token so
    /// a full channel cannot defer cancellation.
    pub(crate) async fn send_control(&mut self, update: MarketUpdate) -> SendState {
        match self.tx_control.send(update).await {
            Ok(()) => SendState::Open,
            Err(_) => SendState::Closed,
        }
    }

    /// Publish a coalesced-class update through the producer overwrite-on-full
    /// staging onto the coalesced channel (synchronous — no `.await`). A full
    /// channel **stages** the freshest value per instrument rather than dropping it.
    pub(crate) fn publish_coalesced(&mut self, update: MarketUpdate) -> SendState {
        self.staging.publish(&self.tx_coalesced, update)
    }

    /// Retry the staged coalesced residue onto the coalesced channel as it drains
    /// — the producer flush the streaming loop performs on a tick when the feed is
    /// quiet, so the freshest staged value is not stranded.
    pub(crate) fn flush(&mut self) -> SendState {
        self.staging.flush(&self.tx_coalesced)
    }

    /// True while any instrument still holds a staged coalesced value awaiting a
    /// free channel slot — gates the streaming loop's flush tick.
    pub(crate) fn has_pending(&self) -> bool {
        self.staging.has_pending()
    }

    /// Clear the coalesced staging (an **epoch**), so pre-outage staged values are
    /// dropped on a disconnect/refetch and never flush over the fresh reconnect
    /// backfill `Chain` ([ADR-0009], the sink-staging fix). The allocation is
    /// retained for reuse.
    ///
    /// [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
    pub(crate) fn epoch(&mut self) {
        self.staging.clear();
    }

    /// Whether either bounded channel has closed (the consumer is gone), so the
    /// adapter's reconnect loop can stop instead of reconnecting.
    pub(crate) fn is_closed(&self) -> bool {
        self.tx_control.is_closed() || self.tx_coalesced.is_closed()
    }
}

// Test-only inspection of the producer staging bound (NFR-15): the O(N)
// slots-per-instrument view the Deribit staging tests assert on. `pub(crate)`, so
// the default semver surface is unchanged.
#[cfg(test)]
impl MarketUpdateSink {
    /// The number of staged instruments — a view of the O(N) producer bound.
    pub(crate) fn staged_len(&self) -> usize {
        self.staging.slots.len()
    }
}

/// One producer-staged slot for one instrument: the latest of each coalesced
/// update **kind** independently, so a Greeks refresh never clobbers a pending
/// quote — the producer mirror of the consumer `StagedInstrument` (#10).
#[derive(Debug, Default)]
struct StagedInstrument {
    quote: Option<QuoteUpdate>,
    greeks: Option<GreeksRow>,
    depth: Option<DepthLadder>,
}

impl StagedInstrument {
    /// True while any kind is still staged.
    fn has_any(&self) -> bool {
        self.quote.is_some() || self.greeks.is_some() || self.depth.is_some()
    }

    /// Flush this slot's present kinds onto `tx`, reserving a channel slot
    /// **before** taking the value so a full channel never drops the staged
    /// update. Stops at the first full/closed reservation, leaving the remaining
    /// kinds staged.
    fn flush_into(&mut self, tx: &mpsc::Sender<MarketUpdate>) -> FlushStep {
        if self.quote.is_some() {
            match reserve_send(tx, &mut self.quote, MarketUpdate::Quote) {
                FlushStep::Drained => {}
                blocked => return blocked,
            }
        }
        if self.greeks.is_some() {
            match reserve_send(tx, &mut self.greeks, MarketUpdate::Greeks) {
                FlushStep::Drained => {}
                blocked => return blocked,
            }
        }
        if self.depth.is_some() {
            match reserve_send(tx, &mut self.depth, MarketUpdate::Depth) {
                FlushStep::Drained => {}
                blocked => return blocked,
            }
        }
        FlushStep::Drained
    }
}

/// The outcome of trying to flush one staged kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushStep {
    /// The kind was sent (or was absent).
    Drained,
    /// The channel had no capacity — the value stays staged.
    Full,
    /// The channel is closed.
    Closed,
}

/// Reserve a channel slot, then move the staged value out of `slot` into it — so
/// a full channel leaves the value **in place** (it is only `take`n on a
/// successful reservation, never lost). The caller only calls this for a
/// non-empty `slot`.
fn reserve_send<T>(
    tx: &mpsc::Sender<MarketUpdate>,
    slot: &mut Option<T>,
    wrap: fn(T) -> MarketUpdate,
) -> FlushStep {
    match tx.try_reserve() {
        Ok(permit) => {
            if let Some(value) = slot.take() {
                permit.send(wrap(value));
            }
            FlushStep::Drained
        }
        Err(TrySendError::Full(())) => FlushStep::Full,
        Err(TrySendError::Closed(())) => FlushStep::Closed,
    }
}

/// The producer-side conflater: one slot per [`InstrumentKey`], overwritten
/// last-value-wins per kind when the bounded channel is full
/// (`docs/02-tui-architecture.md` §5). Completes the NFR-15 latest-value-wins
/// guarantee under sustained saturation — the mirror of the #10 consumer-side
/// staging. It is O(N subscribed) and **reuses its allocation** across bursts.
#[derive(Debug, Default)]
struct ProducerStaging {
    slots: HashMap<InstrumentKey, StagedInstrument>,
}

impl ProducerStaging {
    /// An empty staging map.
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// Publish `update` on the bounded `tx`, preserving the freshest value under
    /// saturation: first opportunistically flush anything already staged (the
    /// channel may now have space), then try to send `update`; on a **full**
    /// channel the update is **staged** (overwriting its kind's slot) rather than
    /// dropped. Returns [`SendState::Closed`] once the channel is closed.
    fn publish(&mut self, tx: &mpsc::Sender<MarketUpdate>, update: MarketUpdate) -> SendState {
        if self.flush(tx) == SendState::Closed {
            return SendState::Closed;
        }
        match tx.try_send(update) {
            Ok(()) => SendState::Open,
            Err(TrySendError::Full(update)) => {
                self.stage(update);
                SendState::Open
            }
            Err(TrySendError::Closed(_)) => SendState::Closed,
        }
    }

    /// True while any instrument still holds a staged value awaiting a free
    /// channel slot.
    fn has_pending(&self) -> bool {
        self.slots.values().any(StagedInstrument::has_any)
    }

    /// Flush the staged current values onto `tx`, retaining the map allocation.
    /// Stops sending once the channel is full (leaving the rest staged) and
    /// reports a closed channel.
    fn flush(&mut self, tx: &mpsc::Sender<MarketUpdate>) -> SendState {
        let mut closed = false;
        let mut full = false;
        self.slots.retain(|_key, slot| {
            if !closed && !full {
                match slot.flush_into(tx) {
                    FlushStep::Drained => {}
                    FlushStep::Full => full = true,
                    FlushStep::Closed => closed = true,
                }
            }
            slot.has_any()
        });
        if closed {
            SendState::Closed
        } else {
            SendState::Open
        }
    }

    /// Drop every staged value (an epoch), retaining the allocation.
    fn clear(&mut self) {
        self.slots.clear();
    }

    /// Overwrite the staged slot for `update`'s instrument, last-value-wins per
    /// kind. A control-class update never reaches here (the sink await-sends it
    /// directly), but the match stays total over the closed [`MarketUpdate`] set.
    fn stage(&mut self, update: MarketUpdate) {
        match update {
            MarketUpdate::Quote(quote) => {
                if let Some(slot) = self.slot_mut(&quote.instrument.key) {
                    slot.quote = Some(quote);
                }
            }
            MarketUpdate::Greeks(greeks) => {
                if let Some(slot) = self.slot_mut(&greeks.instrument.key) {
                    slot.greeks = Some(greeks);
                }
            }
            MarketUpdate::Depth(depth) => {
                if let Some(slot) = self.slot_mut(&depth.instrument.key) {
                    slot.depth = Some(depth);
                }
            }
            MarketUpdate::Chain(_) | MarketUpdate::Health(_, _) => {}
        }
    }

    /// A mutable reference to `key`'s slot, creating it on first use. The key is
    /// cloned **only** when the slot is vacant (the HP-3 discipline); the `None`
    /// arm is treated as a no-op, never an `expect`.
    fn slot_mut(&mut self, key: &InstrumentKey) -> Option<&mut StagedInstrument> {
        if !self.slots.contains_key(key) {
            let _ = self.slots.insert(key.clone(), StagedInstrument::default());
        }
        self.slots.get_mut(key)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};

    use optionstratlib::OptionStyle;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;

    use super::*;
    use crate::chain::{
        AliasCatalog, ContractSpecFingerprint, ExerciseStyle, ExpirySource, InstrumentKey,
        SettlementStyle,
    };

    // --- Test constructors + a minimal executor (no unwrap/expect per ruleset) -

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> DateTime<Utc> {
        match DateTime::<Utc>::from_timestamp(secs, 0) {
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

    fn sample_key() -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(1_700_000_000),
            strike: pos(60_000.0),
            style: OptionStyle::Call,
        }
    }

    fn sample_instrument(provider: &str, native: &str, stream: Option<&str>) -> Instrument {
        Instrument {
            key: sample_key(),
            provider: pid(provider),
            native_symbol: native.to_owned(),
            stream_symbol: stream.map(str::to_owned),
            spec: ContractSpecFingerprint {
                contract_multiplier: 1,
                settlement: SettlementStyle::Cash,
                exercise: ExerciseStyle::European,
                quote_currency: "USD".to_owned(),
                venue_product_code: "BTC".to_owned(),
            },
        }
    }

    /// Drive a future to completion on the current thread with a no-op waker.
    /// The fake provider's futures resolve on the first poll (no real await
    /// points), so a single poll suffices — this avoids pulling a tokio runtime
    /// into a port-only unit test. `Waker::noop` is stable since Rust 1.85,
    /// below the crate's 1.88 MSRV, so no `unsafe` waker construction is needed.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future parked; port futures must resolve on first poll"),
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// A minimal in-test poll-only provider proving the trait is object-safe and
    /// `Send + Sync`. Its `subscribe` returns `Unsupported` (the poll-only shape).
    struct FakeProvider {
        id: ProviderId,
        capabilities: ProviderCapabilities,
        chain: ChainFetch,
        underlyings: Vec<UnderlyingRef>,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            self.capabilities
        }

        async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
            Ok(self.underlyings.clone())
        }

        async fn fetch_chain(
            &self,
            _underlying: &str,
            _expiration: &ExpirationDate,
        ) -> Result<ChainFetch, ProviderError> {
            Ok(self.chain.clone())
        }

        async fn subscribe(
            &self,
            _req: SubscriptionRequest,
            _sink: MarketUpdateSink,
        ) -> Result<SubscriptionHandle, ProviderError> {
            Err(ProviderError::Unsupported("streaming"))
        }
    }

    fn fake_provider() -> FakeProvider {
        let mut aliases = AliasCatalog::new();
        aliases.insert(sample_instrument(
            "fake",
            "BTC-27JUN25-60000-C",
            Some("dxfeed-sym"),
        ));
        FakeProvider {
            id: pid("fake"),
            capabilities: ProviderCapabilities::builder()
                .chain(ChainCapability::Assemble)
                .greeks(GreeksCapability::Provided)
                .chain_poll(ChainPollCapability::Poll {
                    interval_hint_secs: 2,
                })
                .build(),
            chain: ChainFetch::new(
                OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None),
                ExpirySource::new("BTC", utc(1_700_000_000), pid("fake")),
                aliases,
            ),
            underlyings: vec![UnderlyingRef::new("BTC")],
        }
    }

    // --- Builder yields a complete, correct capability set -------------------

    #[test]
    fn test_provider_capabilities_builder_sets_every_field() {
        let caps = ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(true)
            .greeks(GreeksCapability::ComputedLocally)
            .option_stream(OptionStreamCapability::ChainQuotes { verified: false })
            .underlying_stream(true)
            .chain_poll(ChainPollCapability::Poll {
                interval_hint_secs: 5,
            })
            .trades_tape(true)
            .auth(AuthKind::UserPass)
            .build();
        assert_eq!(caps.chain, ChainCapability::Assemble);
        assert!(caps.depth);
        assert_eq!(caps.greeks, GreeksCapability::ComputedLocally);
        assert_eq!(
            caps.option_stream,
            OptionStreamCapability::ChainQuotes { verified: false }
        );
        assert!(caps.underlying_stream);
        assert_eq!(
            caps.chain_poll,
            ChainPollCapability::Poll {
                interval_hint_secs: 5
            }
        );
        assert!(caps.trades_tape);
        assert_eq!(caps.auth, AuthKind::UserPass);
    }

    #[test]
    fn test_provider_capabilities_builder_defaults_are_least_capable() {
        let caps = ProviderCapabilities::builder().build();
        assert_eq!(caps.chain, ChainCapability::None);
        assert!(!caps.depth);
        assert_eq!(caps.greeks, GreeksCapability::None);
        assert_eq!(caps.option_stream, OptionStreamCapability::None);
        assert!(!caps.underlying_stream);
        assert_eq!(caps.chain_poll, ChainPollCapability::None);
        assert!(!caps.trades_tape);
        assert_eq!(caps.auth, AuthKind::None);
    }

    #[test]
    fn test_capability_enum_defaults_are_none() {
        assert_eq!(ChainCapability::default(), ChainCapability::None);
        assert_eq!(GreeksCapability::default(), GreeksCapability::None);
        assert_eq!(
            OptionStreamCapability::default(),
            OptionStreamCapability::None
        );
        assert_eq!(ChainPollCapability::default(), ChainPollCapability::None);
        assert_eq!(AuthKind::default(), AuthKind::None);
    }

    // --- AliasCatalog round-trips native AND stream symbols to the key -------

    #[test]
    fn test_alias_catalog_round_trips_native_and_stream_symbols() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(sample_instrument(
            "deribit",
            "BTC-27JUN25-60000-C",
            Some("dxfeed-sym"),
        ));
        assert_eq!(
            catalog.resolve_symbol("BTC-27JUN25-60000-C"),
            Some(&sample_key())
        );
        assert_eq!(catalog.resolve_symbol("dxfeed-sym"), Some(&sample_key()));
        match catalog.instrument(&sample_key(), &pid("deribit")) {
            Some(found) => assert_eq!(found.native_symbol, "BTC-27JUN25-60000-C"),
            None => panic!("expected the deribit alias for the leg"),
        }
    }

    #[test]
    fn test_chain_fetch_carries_alias_catalog_forward_unchanged() {
        let provider = fake_provider();
        let fetch = block_on(provider.fetch_chain("BTC", &ExpirationDate::Days(pos(30.0))));
        match fetch {
            Ok(chain_fetch) => {
                // The catalog the fake seeded rides forward on the artifact.
                assert_eq!(
                    chain_fetch.aliases.resolve_symbol("dxfeed-sym"),
                    Some(&sample_key())
                );
                assert!(
                    chain_fetch
                        .aliases
                        .instrument(&sample_key(), &pid("fake"))
                        .is_some()
                );
                assert_eq!(chain_fetch.expiry_source.underlying, "BTC");
            }
            Err(e) => panic!("fetch_chain should succeed for the fake provider, got: {e}"),
        }
    }

    // --- Object safety + Send + Sync (registry-ready) ------------------------

    #[test]
    fn test_provider_trait_object_is_send_sync() {
        assert_send_sync::<Box<dyn Provider>>();
    }

    #[test]
    fn test_fake_provider_is_object_safe_and_reports_capabilities() {
        let provider: Box<dyn Provider> = Box::new(fake_provider());
        assert_eq!(provider.id().as_str(), "fake");
        assert_eq!(provider.capabilities().chain, ChainCapability::Assemble);
    }

    #[test]
    fn test_fake_provider_discover_lists_underlyings() {
        let provider = fake_provider();
        match block_on(provider.discover()) {
            Ok(underlyings) => match underlyings.first() {
                Some(first) => assert_eq!(first.underlying, "BTC"),
                None => panic!("expected at least one underlying"),
            },
            Err(e) => panic!("discover should succeed for the fake provider, got: {e}"),
        }
    }

    #[test]
    fn test_fake_provider_subscribe_is_unsupported_for_poll_only_shape() {
        let provider = fake_provider();
        let (tx, _rx) = mpsc::channel::<MarketUpdate>(1);
        let sink = MarketUpdateSink::new(tx.clone(), tx);
        let request = SubscriptionRequest::new(
            "BTC",
            utc(1_700_000_000),
            Vec::new(),
            CancellationToken::new(),
        );
        match block_on(provider.subscribe(request, sink)) {
            Err(ProviderError::Unsupported(what)) => assert_eq!(what, "streaming"),
            other => panic!("expected Unsupported(\"streaming\"), got {other:?}"),
        }
    }

    // --- SubscriptionHandle cancellation -------------------------------------

    #[test]
    fn test_subscription_handle_drop_cancels_subscription() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let handle = SubscriptionHandle::new(move || flag.store(true, Ordering::SeqCst));
        assert!(!cancelled.load(Ordering::SeqCst));
        drop(handle);
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn test_subscription_handle_abort_cancels_once() {
        let count = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&count);
        let handle = SubscriptionHandle::new(move || flag.store(true, Ordering::SeqCst));
        handle.abort();
        // The handle dropped at the end of `abort`; the action still fires exactly
        // once (the flag is true and no second store is possible).
        assert!(count.load(Ordering::SeqCst));
    }

    // --- Port helper types ---------------------------------------------------

    #[test]
    fn test_subscription_request_new_sets_fields() {
        let cancel = CancellationToken::new();
        let request = SubscriptionRequest::new(
            "BTC",
            utc(1_700_000_000),
            vec![sample_instrument("deribit", "native", None)],
            cancel.clone(),
        );
        assert_eq!(request.underlying, "BTC");
        assert_eq!(request.expiration_utc, utc(1_700_000_000));
        assert_eq!(request.instruments.len(), 1);
        // The request carries the supervisor's per-provider cancel token verbatim.
        assert!(!request.cancel.is_cancelled());
        cancel.cancel();
        assert!(request.cancel.is_cancelled());
    }

    #[test]
    fn test_underlying_ref_new_has_no_expirations() {
        let underlying = UnderlyingRef::new("SPY");
        assert_eq!(underlying.underlying, "SPY");
        assert!(underlying.expirations.is_empty());
    }

    #[test]
    fn test_underlying_ref_with_expirations_carries_them() {
        let underlying =
            UnderlyingRef::with_expirations("BTC", vec![ExpirationDate::Days(pos(7.0))]);
        assert_eq!(underlying.expirations.len(), 1);
    }
}
