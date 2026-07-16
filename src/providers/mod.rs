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

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use optionstratlib::ExpirationDate;
use tokio::sync::mpsc;

use crate::chain::{ChainFetch, Instrument, MarketUpdate, ProviderId};
use crate::error::ProviderError;

/// The Deribit adapter — the zero-config, public-data poll leg (issue #15,
/// `docs/03-data-providers.md` §7.1). Crate-internal: it is registered through
/// [`ChainViewAppBuilder::with_builtins`](crate::ChainViewAppBuilder), never
/// named on the public surface, so no raw `deribit-http` DTO crosses the port.
pub(crate) mod deribit;

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

    /// Open a streaming subscription; normalized [`MarketUpdate`]s are pushed to
    /// `tx` until the returned [`SubscriptionHandle`] is dropped. The adapter
    /// owns the reconnect/resubscribe loop behind the handle and always
    /// interposes this **bounded** sender (`docs/03-data-providers.md` §5). A
    /// poll-only provider returns [`ProviderError::Unsupported`].
    ///
    /// # Errors
    ///
    /// [`ProviderError::Unsupported`] for a poll-only provider, or a
    /// transport/auth failure.
    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        tx: mpsc::Sender<MarketUpdate>,
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
}

impl SubscriptionRequest {
    /// Construct a subscription request scoped to one `(underlying, expiry)` over
    /// the given legs.
    #[must_use]
    pub fn new(
        underlying: impl Into<String>,
        expiration_utc: DateTime<Utc>,
        instruments: Vec<Instrument>,
    ) -> Self {
        Self {
            underlying: underlying.into(),
            expiration_utc,
            instruments,
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
    /// `None` after it has run, so the action fires at most once.
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

impl SubscriptionHandle {
    /// Build a handle from the adapter's cancellation action, which runs once
    /// when the handle is dropped (or [`abort`](Self::abort)ed).
    #[must_use = "dropping the handle immediately cancels the subscription"]
    pub fn new<F>(on_cancel: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            cancel: Some(Box::new(on_cancel)),
        }
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
            _tx: mpsc::Sender<MarketUpdate>,
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
        let request = SubscriptionRequest::new("BTC", utc(1_700_000_000), Vec::new());
        match block_on(provider.subscribe(request, tx)) {
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
        let request = SubscriptionRequest::new(
            "BTC",
            utc(1_700_000_000),
            vec![sample_instrument("deribit", "native", None)],
        );
        assert_eq!(request.underlying, "BTC");
        assert_eq!(request.expiration_utc, utc(1_700_000_000));
        assert_eq!(request.instruments.len(), 1);
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
