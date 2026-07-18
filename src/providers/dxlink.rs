//! The standalone DXLink adapter — the **overlay-only** quote/Greeks provider,
//! behind a DISABLED build feature (`docs/03-data-providers.md` §7.3,
//! `docs/SECURITY.md` §2.4).
//!
//! DXLink is WebSocket-only (the DXLink protocol) and has **no chain discovery**,
//! so a standalone DXLink provider cannot assemble a chain on its own. Its
//! [`discover`](Provider::discover) and [`fetch_chain`](Provider::fetch_chain)
//! return [`ProviderError::Unsupported`] (`chain: None`,
//! `option_stream: SymbolOnly`); it is usable only as a symbol-level quote/Greeks
//! **overlay** onto an **external chain source** the user supplies. An external
//! chain source is therefore **required** to select `dxlink`: with one configured
//! the standard chain screen renders (quotes/Greeks overlaid on the external
//! structure); without one, selecting `dxlink` is a
//! [`ConfigError::InvalidValue`](crate::error::ConfigError::InvalidValue) at
//! startup (the capability-driven composite-source guard in `src/app/registry.rs`
//! reads `chain == None`), never a separate raw-symbol screen.
//!
//! # The overlay join and its economic-equivalence gate
//!
//! Because a DXLink event and a differently-sourced chain row share one
//! [`InstrumentKey`](crate::chain::InstrumentKey) (the 4-tuple has no
//! `ProviderId`), an overlay leg is only a **candidate** on a key match. The merge
//! proceeds **iff** the overlay feed's
//! [`ContractSpecFingerprint`](crate::chain::ContractSpecFingerprint) (multiplier,
//! settlement, exercise, quote currency, venue product code) equals the source
//! leg's. That gate is the store's job: the adapter emits normalized
//! [`QuoteUpdate`]/[`GreeksRow`] tagged `provider: dxlink` carrying the DXLink
//! leg's fingerprint, and [`ChainStore::gate_overlay`](crate::chain::ChainStore)
//! compares the two per-leg fingerprints read from the alias catalog. On a match
//! the overlay (live stream) wins the quote/Greek fields and the source keeps
//! structure; on a mismatch the merge is a **per-leg, non-fatal**
//! [`OverlayError::SpecMismatch`](crate::error::OverlayError) — the overlay is
//! **refused**, the source leg is kept unchanged, and the leg is badged
//! overlay-refused — so two economically distinct contracts that merely share
//! `(underlying, expiry, strike, style)` are never silently merged
//! (`docs/01-domain-model.md` §4).
//!
//! # The gate — credential logging upstream (`docs/SECURITY.md` §2.4)
//!
//! The whole adapter sits behind the DISABLED-by-default `dxlink` Cargo feature
//! and is **excluded from `with_builtins()`**; it is reachable only via the
//! explicit `with_gated_builtin`, which returns a typed startup error while the
//! gate holds. So a stock binary can **never** execute the upstream's logging —
//! the credential guarantee holds **by construction**, not author discipline
//! (`docs/SECURITY.md` §3). Historically the `dxlink` client logged its serialized
//! `AuthMessage` token at `debug!` (the provenance the lifter needs:
//! `dxlink/src/client.rs` / `src/connection.rs`, commit `1c57a36`, upstream issue
//! against `joaquinbejar/DXlink`). The pinned `dxlink 0.2.0` this crate resolves
//! already **redacts** that token (`redact_sensitive` in `src/connection.rs`, and
//! its `Debug` exposes only `has_token`), but — exactly like the Alpaca gate — the
//! gate stays in place until `docs/SECURITY.md` records the captured-log proof and
//! flips the matrix cell; this adapter does not lift it unilaterally. It also
//! inherits the tastytrade gate whenever the DXLink token is minted there
//! (`quote_streamer_tokens()`).
//!
//! # Auth is injected programmatically (no dotenv, no foreign env namespace)
//!
//! `DXLinkClient::new(url, token)` takes the URL and token as plain arguments, so
//! [`from_env`](DxlinkAdapter::from_env) reads ChainView-namespaced
//! `CHAINVIEW_DXLINK_TOKEN` / `CHAINVIEW_DXLINK_URL` env vars and builds the client
//! directly. There is **no** dotenv load, **no** foreign env namespace, and **no**
//! global tracing subscriber installed on construction. The token is wrapped in
//! [`Secret`] and read only through [`Secret::expose`](crate::config::Secret::expose)
//! at the single client hand-off site — never logged or echoed in a
//! [`ProviderError`] (`docs/03-data-providers.md` §11.3).
//!
//! # Normalization happens at this seam (shares the tastytrade dxfeed decode)
//!
//! Every raw `dxlink` DTO stops here (`CLAUDE.md` "Module Boundaries"). The typed
//! `MarketEvent::{Quote, Greeks}` (sizes are `f64`, and there is **no** venue time
//! field) is mapped onto the neutral [`dxfeed_decode`](super::dxfeed_decode) views
//! (`event_time: None`; the `received_time` is stamped at the boundary) and decoded
//! by the shared [`decode_quote`](super::dxfeed_decode::decode_quote) /
//! [`decode_greeks`](super::dxfeed_decode::decode_greeks) helpers (#38) — the
//! **same** decode the tastytrade adapter feeds its bundled dxfeed events through,
//! with **no** adapter-to-adapter edge (both depend on that neutral module, neither
//! on the other, `docs/03-data-providers.md` §12). A crossed-tick decode error is a
//! **benign per-tick drop** (keep the prior, never a reconnect/health input), and an
//! event for an unknown symbol is dropped by the symbol clamp guard.
//!
//! # Reconnect + two update classes (`docs/03-data-providers.md` §5, [ADR-0009])
//!
//! The reconnect/resubscribe loop is **ChainView's**, driven behind the
//! [`SubscriptionHandle`]; on a dropped stream it emits `Health(Reconnecting)`,
//! backs off with jittered exponential backoff, and re-subscribes the **same** leg
//! set (there is no chain to re-fetch — the source provider owns the structure and
//! its own backfill). Every [`MarketUpdate`] is handed to the two-class
//! [`MarketUpdateSink`], which routes `Health` to the control channel and coalesces
//! `Quote`/`Greeks`.
//!
//! [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use optionstratlib::ExpirationDate;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use dxlink::{DXLinkClient, EventType, FeedSubscription, MarketEvent};

use super::dxfeed_decode::{DxGreeksEvent, DxQuoteEvent, decode_greeks, decode_quote};
use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, MarketUpdateSink,
    OptionStreamCapability, Provider, ProviderCapabilities, SinkSend, SubscriptionHandle,
    SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{ChainFetch, Instrument, MarketUpdate, ProviderId, StreamHealth};
use crate::config::{EnvSource, Secret, require_credentials};
use crate::error::ProviderError;

/// The reserved provider id this adapter registers under
/// ([`RESERVED_PROVIDER_IDS`](crate::chain::RESERVED_PROVIDER_IDS)).
const DXLINK_ID: &str = "dxlink";

/// The credential field name read from the environment for `Token` auth
/// (`CHAINVIEW_DXLINK_TOKEN`, `docs/03-data-providers.md` §11.3).
const CREDENTIAL_KEYS: [&str; 1] = ["token"];

/// The optional DXLink WebSocket URL variable (`CHAINVIEW_DXLINK_URL`); absent
/// falls back to [`DEFAULT_DXLINK_URL`].
const URL_VAR: &str = "CHAINVIEW_DXLINK_URL";

/// The production DXLink WebSocket endpoint (dxfeed realtime) used when
/// `CHAINVIEW_DXLINK_URL` is unset — a public, non-secret venue endpoint.
const DEFAULT_DXLINK_URL: &str = "wss://tasty-openapi-ws.dxfeed.com/realtime";

/// The `AUTO` feed-channel contract type DXLink resolves per-symbol.
const FEED_CONTRACT: &str = "AUTO";

/// The dxfeed `Quote` event type string in a [`FeedSubscription`].
const QUOTE_EVENT: &str = "Quote";

/// The dxfeed `Greeks` event type string in a [`FeedSubscription`].
const GREEKS_EVENT: &str = "Greeks";

// --- Reconnect backoff (docs/03-data-providers.md §5) ------------------------

/// The reconnect backoff base, in milliseconds (`BASE = 250 ms`).
const BACKOFF_BASE_MS: f64 = 250.0;
/// The reconnect backoff ceiling, in milliseconds (`MAX = 30 s`).
const BACKOFF_MAX_MS: f64 = 30_000.0;
/// The reconnect jitter magnitude — the delay is scaled by `1 + jitter`,
/// `jitter in [-0.2, 0.2]`.
const JITTER_MAGNITUDE: f64 = 0.2;
/// The largest exponent applied to `2^attempt` before the [`BACKOFF_MAX_MS`] cap
/// takes over — a ceiling that keeps `attempt` growth harmless.
const BACKOFF_MAX_SHIFT: u32 = 20;

// ---------------------------------------------------------------------------
// The adapter.
// ---------------------------------------------------------------------------

/// The standalone DXLink `Provider` adapter (crate-internal; behind the disabled
/// `dxlink` feature and reachable only via `with_gated_builtin`).
///
/// Holds the reserved [`ProviderId`], the env-resolved token (wrapped in
/// [`Secret`], never logged), and the DXLink WebSocket URL. `Clone` is cheap — a
/// clone is moved into the spawned reconnect loop so it can reconnect without
/// borrowing `&self` across the task boundary.
#[derive(Clone)]
pub(crate) struct DxlinkAdapter {
    id: ProviderId,
    token: Secret,
    url: String,
}

impl DxlinkAdapter {
    /// Build the adapter from the ChainView-namespaced environment
    /// (`CHAINVIEW_DXLINK_TOKEN`, and the optional `CHAINVIEW_DXLINK_URL`). The
    /// token is read **only** here (env-only policy) and wrapped in [`Secret`]; it
    /// is never logged or echoed in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::MissingCredential`](crate::error::ConfigError::MissingCredential)
    /// (naming the provider, never the key) when the token is unset/empty.
    pub(crate) fn from_env(env: &dyn EnvSource) -> Result<Self, crate::error::ConfigError> {
        let id = dxlink_provider_id();
        let creds = require_credentials(env, &id, &CREDENTIAL_KEYS)?;
        let token = creds
            .get("TOKEN")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        let url = env
            .get(URL_VAR)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_DXLINK_URL.to_owned());
        Ok(Self { id, token, url })
    }

    /// Build a fresh upstream `DXLinkClient` from the injected URL + token. The
    /// token is exposed only at this single hand-off site and never logged.
    fn client(&self) -> DXLinkClient {
        DXLinkClient::new(&self.url, self.token.expose())
    }
}

#[async_trait]
impl Provider for DxlinkAdapter {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        dxlink_capabilities()
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        // Overlay-only: DXLink has no chain discovery of its own (§7.3).
        Err(ProviderError::Unsupported("chain discovery"))
    }

    async fn fetch_chain(
        &self,
        _underlying: &str,
        _expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        // Overlay-only: DXLink cannot assemble a chain — it joins another
        // provider's chain as a quote/Greeks overlay (§7.3).
        Err(ProviderError::Unsupported("chain assembly"))
    }

    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        sink: MarketUpdateSink,
    ) -> Result<SubscriptionHandle, ProviderError> {
        // The adapter OWNS the reconnect/resubscribe loop. It selects on the
        // SUPERVISOR's child token (`req.cancel`, ADR-0009) so the #11 ordered
        // bounded-join can await it, and the returned `SubscriptionHandle::spawned`
        // surfaces the loop's `JoinHandle` for registration.
        let transport = LiveTransport::new(self.clone());
        let id = self.id.clone();
        let SubscriptionRequest {
            underlying: _underlying,
            expiration_utc: _expiration_utc,
            instruments,
            cancel,
        } = req;
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            transport,
            id,
            instruments,
            sink,
            loop_cancel,
        ));
        Ok(SubscriptionHandle::spawned(cancel, handle))
    }
}

// ---------------------------------------------------------------------------
// Identity + capabilities.
// ---------------------------------------------------------------------------

/// The adapter's reserved [`ProviderId`]. `"dxlink"` is a compile-time literal
/// that satisfies the grammar (proven by `test_dxlink_id_is_valid_and_reserved`),
/// so construction cannot fail; the fallback arm is unreachable.
fn dxlink_provider_id() -> ProviderId {
    match ProviderId::new(DXLINK_ID) {
        Ok(id) => id,
        Err(_) => unreachable!("`dxlink` is a valid, reserved provider id literal"),
    }
}

/// DXLink's honest capability self-declaration — the `docs/03-data-providers.md`
/// §7.3/§8 row: **no** chain (`chain: None` — overlay-only), no depth,
/// venue-provided Greeks (the dxfeed `Greeks` event), an (unverified)
/// symbol-level quote/Greek stream, **no** underlying stream, **no** chain
/// polling, no trades tape, and `Token` auth.
///
/// The `chain: None` is what makes DXLink unsuitable as a live SOURCE: the
/// capability-driven composite-source guard in `src/app/registry.rs` rejects a
/// chain-less provider selected as the source with
/// [`ConfigError::InvalidValue`](crate::error::ConfigError::InvalidValue).
#[must_use]
pub(crate) fn dxlink_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::None)
        .depth(false)
        .greeks(GreeksCapability::Provided)
        .option_stream(OptionStreamCapability::SymbolOnly { verified: false })
        .underlying_stream(false)
        .chain_poll(ChainPollCapability::None)
        .trades_tape(false)
        .auth(AuthKind::Token)
        .build()
}

// ---------------------------------------------------------------------------
// The transport seam: the venue I/O the reconnect loop drives (mockable).
// ---------------------------------------------------------------------------

/// The transport is gone — a connect/subscribe step failed or the stream
/// dropped/errored. A zero-size marker: it carries no upstream text (so no DXLink
/// error string can leak, `docs/03-data-providers.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransportGone;

/// A neutral, adapter-internal view of one raw DXLink `MarketEvent`, so the
/// reconnect loop is testable against a mock with **no** upstream type. The raw
/// `MarketEvent` is mapped onto this inside [`LiveTransport`] and never escapes.
///
/// DXLink sizes are natively `f64` and the events carry **no** venue timestamp —
/// the structural difference from tastytrade's `i64` sizes + `time` field, bridged
/// here before the shared decode (`docs/03-data-providers.md` §3, §7.3).
#[derive(Debug, Clone)]
enum RawDxEvent {
    /// A quote event: `bid`/`ask` prices and `bid_size`/`ask_size` (all `f64`).
    Quote {
        symbol: String,
        bid: f64,
        ask: f64,
        bid_size: f64,
        ask_size: f64,
    },
    /// A Greeks event: the five Greeks + `volatility` (all `f64`).
    Greeks {
        symbol: String,
        delta: f64,
        gamma: f64,
        theta: f64,
        vega: f64,
        rho: f64,
        volatility: f64,
    },
    /// A trade or other event the adapter does not overlay — ignored.
    Ignored,
}

/// The venue-I/O seam the reconnect loop drives, so the loop runs deterministically
/// against a **mock** — no socket, no wall clock. The production [`LiveTransport`]
/// wraps the upstream `DXLinkClient` + its `MarketEvent` receiver; a raw
/// `MarketEvent` is decoded to [`RawDxEvent`] inside it and never crosses this seam.
///
/// There is **no** `refetch` (unlike the poll->stream adapters): DXLink is
/// overlay-only and has no chain to reconcile — the source provider owns the
/// structure and its own backfill (§7.3).
#[async_trait]
trait DxlinkTransport: Send {
    /// Connect and subscribe `symbols` (dxfeed streamer symbols) for Quote+Greeks:
    /// `connect()` -> `create_feed_channel("AUTO")` -> `setup_feed(&[Quote,Greeks])`
    /// -> `subscribe(...)`.
    async fn connect_and_subscribe(&mut self, symbols: Vec<String>) -> Result<(), TransportGone>;

    /// Await the next event. `Err(_)` means the stream ended — the loop reconnects.
    async fn receive(&mut self) -> Result<RawDxEvent, TransportGone>;
}

/// The production [`DxlinkTransport`]: the upstream `DXLinkClient` for the DXLink
/// protocol handshake + subscription and its `MarketEvent` receiver for live
/// events. The raw upstream types stay private and never escape.
struct LiveTransport {
    adapter: DxlinkAdapter,
    /// The connected client, held so its keepalive / message-processing tasks and
    /// the WebSocket stay up for the subscription's lifetime. Taken and
    /// `disconnect`ed on the next reconnect so a prior connection never leaks.
    client: Option<DXLinkClient>,
    /// The `MarketEvent` receiver `connect()` returned; drained by [`receive`].
    events: Option<Receiver<MarketEvent>>,
}

impl LiveTransport {
    fn new(adapter: DxlinkAdapter) -> Self {
        Self {
            adapter,
            client: None,
            events: None,
        }
    }
}

#[async_trait]
impl DxlinkTransport for LiveTransport {
    async fn connect_and_subscribe(&mut self, symbols: Vec<String>) -> Result<(), TransportGone> {
        // Tear down any prior connection before opening a fresh one on reconnect,
        // so a stale client's keepalive/message tasks and WebSocket never leak.
        if let Some(mut old) = self.client.take() {
            let _ = old.disconnect().await;
        }
        self.events = None;

        let mut client = self.adapter.client();
        let events = client.connect().await.map_err(|_| TransportGone)?;
        let channel_id = client
            .create_feed_channel(FEED_CONTRACT)
            .await
            .map_err(|_| TransportGone)?;
        client
            .setup_feed(channel_id, &[EventType::Quote, EventType::Greeks])
            .await
            .map_err(|_| TransportGone)?;
        client
            .subscribe(channel_id, feed_subscriptions(&symbols))
            .await
            .map_err(|_| TransportGone)?;
        self.events = Some(events);
        self.client = Some(client);
        Ok(())
    }

    async fn receive(&mut self) -> Result<RawDxEvent, TransportGone> {
        match self.events.as_mut() {
            Some(events) => match events.recv().await {
                Some(event) => Ok(map_market_event(event)),
                // The receiver closed (the client's message task ended): reconnect.
                None => Err(TransportGone),
            },
            None => Err(TransportGone),
        }
    }
}

/// Map a raw DXLink `MarketEvent` onto the neutral [`RawDxEvent`] — the one place a
/// raw upstream event is touched (it never escapes [`LiveTransport`]). DXLink sizes
/// are `f64` and there is no venue time field.
fn map_market_event(event: MarketEvent) -> RawDxEvent {
    match event {
        MarketEvent::Quote(quote) => RawDxEvent::Quote {
            symbol: quote.event_symbol,
            bid: quote.bid_price,
            ask: quote.ask_price,
            bid_size: quote.bid_size,
            ask_size: quote.ask_size,
        },
        MarketEvent::Greeks(greeks) => RawDxEvent::Greeks {
            symbol: greeks.event_symbol,
            delta: greeks.delta,
            gamma: greeks.gamma,
            theta: greeks.theta,
            vega: greeks.vega,
            rho: greeks.rho,
            volatility: greeks.volatility,
        },
        MarketEvent::Trade(_) => RawDxEvent::Ignored,
    }
}

/// Build the `Quote` + `Greeks` [`FeedSubscription`]s for the given dxfeed streamer
/// symbols — the subscription DXLink's `subscribe(channel_id, ...)` takes.
fn feed_subscriptions(symbols: &[String]) -> Vec<FeedSubscription> {
    let cap = symbols.len().checked_mul(2).unwrap_or(symbols.len());
    let mut subs = Vec::with_capacity(cap);
    for symbol in symbols {
        subs.push(FeedSubscription {
            event_type: QUOTE_EVENT.to_owned(),
            symbol: symbol.clone(),
            from_time: None,
            source: None,
        });
        subs.push(FeedSubscription {
            event_type: GREEKS_EVENT.to_owned(),
            symbol: symbol.clone(),
            from_time: None,
            source: None,
        });
    }
    subs
}

// ---------------------------------------------------------------------------
// The adapter-owned reconnect / resubscribe loop.
// ---------------------------------------------------------------------------

/// Why one connection attempt ended.
enum StreamExit {
    /// The stream dropped or a (re)connect step failed — back off and retry.
    Reconnect,
    /// The subscription is cancelled or the consumer is gone — stop the loop.
    Shutdown,
}

/// The adapter-owned reconnect/resubscribe loop (`docs/03-data-providers.md` §5).
/// Connect + subscribe the leg set's dxfeed symbols, drain quote/Greeks events; on
/// a drop emit `Health(Reconnecting{attempt})`, back off with jitter, and reconnect
/// (re-subscribing the SAME legs — there is no chain to re-fetch). Cancellation is
/// observed at every `.await`, so the loop never opens a stream after cancellation
/// and never hot-loops.
async fn run_reconnect_loop<T: DxlinkTransport>(
    mut transport: T,
    id: ProviderId,
    instruments: Vec<Instrument>,
    mut sink: MarketUpdateSink,
    cancel: CancellationToken,
) {
    let mut attempt: u32 = 0;
    loop {
        if cancel.is_cancelled() || sink.is_closed() {
            return;
        }
        let exit = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            exit = connect_stream_once(&mut transport, &id, &instruments, &mut sink, &cancel, &mut attempt) => exit,
        };
        if matches!(exit, StreamExit::Shutdown) || cancel.is_cancelled() {
            return;
        }
        // The stream dropped: surface the reconnect honestly, then back off.
        attempt = attempt.checked_add(1).unwrap_or(attempt);
        let health = MarketUpdate::Health(id.clone(), StreamHealth::Reconnecting { attempt });
        let health_sent = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            outcome = sink.send(health) => outcome,
        };
        if health_sent == SinkSend::Closed {
            return;
        }
        let delay = backoff_delay(attempt, sample_jitter());
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        // Overlay-only: there is NO chain to re-fetch — the source provider owns
        // the chain structure and its own backfill (§7.3). The loop re-subscribes
        // the SAME leg set on the next iteration.
    }
}

/// One connection attempt: connect + subscribe the leg set's dxfeed symbols, then
/// drain events until the stream drops or the subscription is cancelled. `attempt`
/// resets to 0 on a successful (re)subscribe.
async fn connect_stream_once<T: DxlinkTransport>(
    transport: &mut T,
    id: &ProviderId,
    instruments: &[Instrument],
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
    attempt: &mut u32,
) -> StreamExit {
    let symbols = subscription_symbols(instruments);
    let subscribed = tokio::select! {
        biased;
        () = cancel.cancelled() => return StreamExit::Shutdown,
        result = transport.connect_and_subscribe(symbols) => result,
    };
    if subscribed.is_err() {
        return StreamExit::Reconnect;
    }

    *attempt = 0;
    let live = MarketUpdate::Health(id.clone(), StreamHealth::Live);
    if sink.send(live).await == SinkSend::Closed {
        return StreamExit::Shutdown;
    }

    let lookup = stream_lookup(instruments);
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => return StreamExit::Shutdown,
            event = transport.receive() => event,
        };
        let event = match event {
            Ok(event) => event,
            Err(_) => return StreamExit::Reconnect,
        };
        if route_event(&event, &lookup, sink).await == SinkSend::Closed {
            return StreamExit::Shutdown;
        }
    }
}

/// The dxfeed streamer symbols to subscribe for these legs — each leg's
/// `stream_symbol` (the dxfeed id) or, when it has none, its `native_symbol`.
fn subscription_symbols(instruments: &[Instrument]) -> Vec<String> {
    instruments
        .iter()
        .map(|instrument| {
            instrument
                .stream_symbol
                .clone()
                .unwrap_or_else(|| instrument.native_symbol.clone())
        })
        .collect()
}

/// Index the subscribed legs by the dxfeed symbol an incoming event can carry
/// (both the `stream_symbol` and the `native_symbol`), so an event resolves back to
/// the normalized [`Instrument`] — the alias-catalog reverse join (§4). An event for
/// a symbol not in this map is dropped (the unknown-symbol guard).
fn stream_lookup(instruments: &[Instrument]) -> HashMap<String, Instrument> {
    let mut map = HashMap::new();
    for instrument in instruments {
        if let Some(stream) = &instrument.stream_symbol {
            let _ = map
                .entry(stream.clone())
                .or_insert_with(|| instrument.clone());
        }
        let _ = map
            .entry(instrument.native_symbol.clone())
            .or_insert_with(|| instrument.clone());
    }
    map
}

/// Decode one raw DXLink event and publish the normalized update through the
/// neutral [`dxfeed_decode`](super::dxfeed_decode) helpers.
///
/// DXLink events carry **no** venue timestamp, so the neutral view's `event_time`
/// is `None` and ordering falls to `received_time` (stamped here at the boundary).
/// An unknown streamer symbol is a **benign drop** (keep prior; the deferred
/// `clamp_symbol` echo lands with the tracing sink), and a **crossed** quote is
/// likewise a benign per-tick drop — neither feeds reconnect/health/error-rate
/// logic. Returns [`SinkSend::Closed`] once the consumer is gone.
async fn route_event(
    event: &RawDxEvent,
    lookup: &HashMap<String, Instrument>,
    sink: &mut MarketUpdateSink,
) -> SinkSend {
    let received = now_utc();
    match event {
        RawDxEvent::Quote {
            symbol,
            bid,
            ask,
            bid_size,
            ask_size,
        } => {
            let Some(instrument) = lookup.get(symbol) else {
                // Unknown-symbol guard: an event for a symbol not in the subscribed
                // set is dropped (never resurrects a strike). Once the tracing sink
                // lands (governance deviation 3) a bounded `clamp_symbol(symbol)`
                // echo goes here at TRACE — the deribit-adapter house pattern.
                return SinkSend::Delivered;
            };
            let view = DxQuoteEvent {
                symbol: symbol.clone(),
                bid: *bid,
                ask: *ask,
                bid_size: *bid_size,
                ask_size: *ask_size,
                // A dxfeed Quote event carries no last (it rides a Trade event).
                last: None,
                // DXLink events carry no venue timestamp (§7.3).
                event_time: None,
                received_time: received,
            };
            match decode_quote(&view, instrument) {
                Ok(quote) => sink.send(MarketUpdate::Quote(quote)).await,
                // A momentarily-crossed tick is a benign microstructure event on a
                // fast feed: keep the prior quote, do NOT feed reconnect/health/error
                // rate. Once the tracing sink lands a `clamp_symbol` TRACE goes here.
                Err(_) => SinkSend::Delivered,
            }
        }
        RawDxEvent::Greeks {
            symbol,
            delta,
            gamma,
            theta,
            vega,
            rho,
            volatility,
        } => {
            let Some(instrument) = lookup.get(symbol) else {
                // Unknown-symbol guard (see the quote arm): dropped, prior kept. A
                // bounded `clamp_symbol(symbol)` TRACE goes here once the tracing
                // sink lands (governance deviation 3).
                return SinkSend::Delivered;
            };
            let view = DxGreeksEvent {
                symbol: symbol.clone(),
                delta: *delta,
                gamma: *gamma,
                theta: *theta,
                vega: *vega,
                rho: *rho,
                volatility: *volatility,
                event_time: None,
                received_time: received,
            };
            match decode_greeks(&view, instrument) {
                // The dxfeed Greeks event is a venue analytics source; `decode_greeks`
                // carries the IV as-is (§3) and tags the row `GreeksOrigin::Provider`.
                Ok(greeks) => sink.send(MarketUpdate::Greeks(greeks)).await,
                // decode_greeks is total for a well-formed event; a defensive drop.
                Err(_) => SinkSend::Delivered,
            }
        }
        RawDxEvent::Ignored => SinkSend::Delivered,
    }
}

// ---------------------------------------------------------------------------
// Reconnect backoff kernel + clocks (pure, injectable jitter).
// ---------------------------------------------------------------------------

/// The jittered exponential backoff delay for reconnect attempt `attempt`
/// (`docs/03-data-providers.md` §5): `delay = min(MAX, BASE * 2^attempt) *
/// (1 + jitter)`, with `BASE = 250 ms`, `MAX = 30 s`, `jitter in [-0.2, 0.2]`. A
/// **pure** kernel: `jitter` is injected, so the mapping is deterministic under
/// test.
#[must_use]
fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let exponent = attempt.min(BACKOFF_MAX_SHIFT);
    let uncapped = BACKOFF_BASE_MS * 2.0_f64.powi(exponent as i32);
    let capped = uncapped.min(BACKOFF_MAX_MS);
    let jitter = jitter.clamp(-JITTER_MAGNITUDE, JITTER_MAGNITUDE);
    let millis = capped * (1.0 + jitter);
    Duration::from_secs_f64(millis / 1000.0)
}

/// Sample a reconnect jitter in `[-0.2, 0.2)` from the process clock's sub-second
/// nanoseconds — enough entropy to spread simultaneous reconnects, no RNG dep. It
/// is deliberately outside [`backoff_delay`] so the kernel stays pure under test.
fn sample_jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let unit = f64::from(nanos) / 1_000_000_000.0;
    (unit * 2.0 - 1.0) * JITTER_MAGNITUDE
}

/// The current wall-clock instant as a normalization `received_time`. Uses `std`'s
/// clock, clamps a pathological system time to the representable range, never
/// `unwrap`s.
fn now_utc() -> DateTime<Utc> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp(secs, since.subsec_nanos()).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use optionstratlib::OptionStyle;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use proptest::prelude::*;
    use tokio::sync::mpsc;

    use super::*;
    use crate::chain::{
        AliasCatalog, ChainSource, ChainStore, ContractSpecFingerprint, ExerciseStyle,
        ExpirySource, GreeksOrigin, GreeksRow, InstrumentKey, MergeOutcome, QuoteUpdate,
        SettlementStyle,
    };

    const EXP: i64 = 1_700_000_000;
    const STRIKE: f64 = 60_000.0;

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> DateTime<Utc> {
        match DateTime::<Utc>::from_timestamp(secs, 0) {
            Some(t) => t,
            None => panic!("invalid test timestamp: {secs}"),
        }
    }

    /// A map-backed [`EnvSource`] — the process environment is never mutated
    /// (which is `unsafe` on the 2024 edition).
    struct MapEnv(HashMap<String, String>);

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn token_env() -> MapEnv {
        let mut env = HashMap::new();
        let _ = env.insert(
            "CHAINVIEW_DXLINK_TOKEN".to_owned(),
            "do-not-log-this-token".to_owned(),
        );
        MapEnv(env)
    }

    #[track_caller]
    fn sample_adapter() -> DxlinkAdapter {
        match DxlinkAdapter::from_env(&token_env()) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed with the token present: {e}"),
        }
    }

    /// A two-class sink over bounded channels, with both receivers.
    fn test_sink(
        capacity: usize,
    ) -> (
        MarketUpdateSink,
        mpsc::Receiver<MarketUpdate>,
        mpsc::Receiver<MarketUpdate>,
    ) {
        let (tx_control, rx_control) = mpsc::channel::<MarketUpdate>(capacity);
        let (tx_coalesced, rx_coalesced) = mpsc::channel::<MarketUpdate>(capacity);
        (
            MarketUpdateSink::new(tx_control, tx_coalesced),
            rx_control,
            rx_coalesced,
        )
    }

    /// Drive a future to completion on a current-thread runtime (non-networked).
    #[track_caller]
    fn block<F: std::future::Future>(future: F) -> F::Output {
        match tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
        {
            Ok(rt) => rt.block_on(future),
            Err(e) => panic!("failed to build a test runtime: {e}"),
        }
    }

    fn drain(rx: &mut mpsc::Receiver<MarketUpdate>) -> Vec<MarketUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    /// The dxfeed streamer symbol used across the tests.
    const DX_SYMBOL: &str = ".BTC271700000000C60000";

    fn ikey(strike: f64, style: OptionStyle) -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(EXP),
            strike: pos(strike),
            style,
        }
    }

    fn spec(multiplier: u32) -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: multiplier,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    /// A DXLink overlay leg for one key: `provider: dxlink`, its dxfeed stream
    /// symbol, and the given fingerprint multiplier.
    fn dxlink_leg(strike: f64, style: OptionStyle, multiplier: u32) -> Instrument {
        Instrument {
            key: ikey(strike, style),
            provider: pid("dxlink"),
            native_symbol: DX_SYMBOL.to_owned(),
            stream_symbol: Some(DX_SYMBOL.to_owned()),
            spec: spec(multiplier),
        }
    }

    /// A source (deribit) leg for one key, with the given fingerprint multiplier.
    fn source_leg(strike: f64, style: OptionStyle, multiplier: u32) -> Instrument {
        Instrument {
            key: ikey(strike, style),
            provider: pid("deribit"),
            native_symbol: "BTC-27JUN25-60000-C".to_owned(),
            stream_symbol: None,
            spec: spec(multiplier),
        }
    }

    // === Identity + capabilities ==============================================

    #[test]
    fn test_dxlink_id_is_valid_and_reserved() {
        let id = dxlink_provider_id();
        assert_eq!(id.as_str(), "dxlink");
        assert!(id.is_reserved());
        assert!(ProviderId::new(DXLINK_ID).is_ok());
    }

    #[test]
    fn test_dxlink_capabilities_match_section_73_row() {
        let caps = dxlink_capabilities();
        // Overlay-only: no chain, never a live source.
        assert_eq!(caps.chain, ChainCapability::None);
        assert!(!caps.depth);
        assert_eq!(caps.greeks, GreeksCapability::Provided);
        assert_eq!(
            caps.option_stream,
            OptionStreamCapability::SymbolOnly { verified: false }
        );
        assert!(!caps.underlying_stream);
        assert_eq!(caps.chain_poll, ChainPollCapability::None);
        assert!(!caps.trades_tape);
        assert_eq!(caps.auth, AuthKind::Token);
    }

    #[test]
    fn test_adapter_reports_capabilities_and_id_via_trait() {
        let adapter: Box<dyn Provider> = Box::new(sample_adapter());
        assert_eq!(adapter.id().as_str(), "dxlink");
        assert_eq!(adapter.capabilities().chain, ChainCapability::None);
        assert_eq!(
            adapter.capabilities().option_stream,
            OptionStreamCapability::SymbolOnly { verified: false }
        );
    }

    #[test]
    fn test_capabilities_chain_none_makes_dxlink_unsuitable_as_source() {
        // The composite-source guard in `src/app/registry.rs` keys off
        // `chain == None`: a chain-less provider selected as the live source is
        // `ConfigError::InvalidValue` (proven generically by
        // `registry::tests::test_resolve_chainless_source_without_overlay_is_invalid_value`).
        // Asserting the capability here is the unit-level proof for DXLink.
        assert_eq!(dxlink_capabilities().chain, ChainCapability::None);
    }

    // === Credentials: env-only, never logged ==================================

    #[test]
    fn test_from_env_reads_chainview_namespace_only() {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_DXLINK_TOKEN".to_owned(), "tok-a".to_owned());
        // A foreign-namespace value must be ignored.
        let _ = env.insert("DXLINK_TOKEN".to_owned(), "foreign".to_owned());
        let adapter = match DxlinkAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed: {e}"),
        };
        assert_eq!(adapter.token.expose(), "tok-a");
        // URL defaults when unset.
        assert_eq!(adapter.url, DEFAULT_DXLINK_URL);
    }

    #[test]
    fn test_from_env_url_default_and_override() {
        assert_eq!(sample_adapter().url, DEFAULT_DXLINK_URL);
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_DXLINK_TOKEN".to_owned(), "tok".to_owned());
        let _ = env.insert(
            "CHAINVIEW_DXLINK_URL".to_owned(),
            "wss://custom.example/realtime".to_owned(),
        );
        match DxlinkAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => assert_eq!(adapter.url, "wss://custom.example/realtime"),
            Err(e) => panic!("from_env should succeed: {e}"),
        }
    }

    #[test]
    fn test_from_env_missing_token_is_error() {
        // No token in the environment.
        match DxlinkAdapter::from_env(&MapEnv(HashMap::new())) {
            Err(crate::error::ConfigError::MissingCredential(id)) => {
                assert_eq!(id.as_str(), "dxlink");
            }
            Err(other) => panic!("expected MissingCredential, got: {other}"),
            Ok(_) => panic!("expected MissingCredential, got Ok"),
        }
    }

    #[test]
    fn test_token_never_appears_in_debug_of_secret() {
        let adapter = sample_adapter();
        let rendered = format!("{:?}", adapter.token);
        assert!(!rendered.contains("do-not-log-this-token"));
        assert!(rendered.contains("redacted"));
    }

    // === Overlay-only: discover / fetch_chain -> Unsupported ==================

    #[test]
    fn test_discover_is_unsupported() {
        match block(sample_adapter().discover()) {
            Err(ProviderError::Unsupported(what)) => assert_eq!(what, "chain discovery"),
            other => panic!("expected Unsupported(chain discovery), got {other:?}"),
        }
    }

    #[test]
    fn test_fetch_chain_is_unsupported() {
        let adapter = sample_adapter();
        let result = block(adapter.fetch_chain("BTC", &ExpirationDate::Days(pos(30.0))));
        match result {
            Err(ProviderError::Unsupported(what)) => assert_eq!(what, "chain assembly"),
            other => panic!("expected Unsupported(chain assembly), got {other:?}"),
        }
    }

    // === The #38 view mapping: f64 sizes, absent time =========================

    #[test]
    fn test_map_market_event_quote_carries_f64_sizes() {
        let ev = map_market_event(MarketEvent::Quote(dxlink::events::QuoteEvent {
            event_type: "Quote".to_owned(),
            event_symbol: DX_SYMBOL.to_owned(),
            bid_price: 1.5,
            ask_price: 1.7,
            bid_size: 12.5,
            ask_size: 8.0,
        }));
        match ev {
            RawDxEvent::Quote {
                symbol,
                bid,
                ask,
                bid_size,
                ask_size,
            } => {
                assert_eq!(symbol, DX_SYMBOL);
                assert_eq!(bid, 1.5);
                assert_eq!(ask, 1.7);
                // Fractional f64 sizes survive intact (no i64 narrowing).
                assert_eq!(bid_size, 12.5);
                assert_eq!(ask_size, 8.0);
            }
            other => panic!("expected a Quote, got {other:?}"),
        }
    }

    #[test]
    fn test_map_market_event_trade_is_ignored() {
        let ev = map_market_event(MarketEvent::Trade(dxlink::events::TradeEvent {
            event_type: "Trade".to_owned(),
            event_symbol: DX_SYMBOL.to_owned(),
            price: 1.6,
            size: 3.0,
            day_volume: 100.0,
        }));
        assert!(matches!(ev, RawDxEvent::Ignored));
    }

    // === Symbol -> InstrumentKey resolution via the lookup (alias join) =======

    #[test]
    fn test_stream_lookup_resolves_stream_and_native_symbol() {
        let legs = vec![dxlink_leg(STRIKE, OptionStyle::Call, 1)];
        let lookup = stream_lookup(&legs);
        match lookup.get(DX_SYMBOL) {
            Some(instrument) => assert_eq!(instrument.key, ikey(STRIKE, OptionStyle::Call)),
            None => panic!("the dxfeed symbol should resolve to the leg"),
        }
        // Native symbol also resolves (native == stream here).
        assert!(lookup.contains_key(DX_SYMBOL));
    }

    #[test]
    fn test_subscription_symbols_uses_stream_symbol() {
        let legs = vec![dxlink_leg(STRIKE, OptionStyle::Call, 1)];
        assert_eq!(subscription_symbols(&legs), vec![DX_SYMBOL.to_owned()]);
    }

    // === route_event: decode a quote/greeks, benign drops =====================

    #[track_caller]
    fn route(event: &RawDxEvent) -> Vec<MarketUpdate> {
        let legs = vec![dxlink_leg(STRIKE, OptionStyle::Call, 1)];
        let lookup = stream_lookup(&legs);
        let (mut sink, mut rx_control, mut rx_coalesced) = test_sink(8);
        let sent = block(route_event(event, &lookup, &mut sink));
        assert_ne!(sent, SinkSend::Closed);
        let mut out = drain(&mut rx_control);
        out.extend(drain(&mut rx_coalesced));
        out
    }

    #[test]
    fn test_route_quote_decodes_to_quote_update_with_absent_event_time() {
        let updates = route(&RawDxEvent::Quote {
            symbol: DX_SYMBOL.to_owned(),
            bid: 1.5,
            ask: 1.7,
            bid_size: 10.0,
            ask_size: 20.0,
        });
        match updates.as_slice() {
            [MarketUpdate::Quote(quote)] => {
                assert_eq!(quote.instrument.key, ikey(STRIKE, OptionStyle::Call));
                assert_eq!(quote.instrument.provider.as_str(), "dxlink");
                assert_eq!(quote.bid, Some(pos(1.5)));
                assert_eq!(quote.ask, Some(pos(1.7)));
                assert_eq!(quote.bid_size, Some(pos(10.0)));
                // DXLink carries no venue time -> event_time is None.
                assert!(quote.event_time.is_none());
            }
            other => panic!("expected a single QuoteUpdate, got {other:?}"),
        }
    }

    #[test]
    fn test_route_greeks_decodes_to_provider_greeks_row_iv_as_is() {
        let updates = route(&RawDxEvent::Greeks {
            symbol: DX_SYMBOL.to_owned(),
            delta: 0.55,
            gamma: 0.01,
            theta: -0.05,
            vega: 0.2,
            rho: 0.03,
            volatility: 0.35,
        });
        match updates.as_slice() {
            [MarketUpdate::Greeks(greeks)] => {
                assert_eq!(greeks.origin, GreeksOrigin::Provider);
                // dxfeed IV carried as-is (no /100).
                assert_eq!(greeks.iv, Some(pos(0.35)));
                assert!(greeks.event_time.is_none());
            }
            other => panic!("expected a single GreeksRow, got {other:?}"),
        }
    }

    #[test]
    fn test_route_unknown_symbol_is_benign_drop() {
        let updates = route(&RawDxEvent::Quote {
            symbol: ".UNSUBSCRIBED".to_owned(),
            bid: 1.5,
            ask: 1.7,
            bid_size: 10.0,
            ask_size: 20.0,
        });
        assert!(
            updates.is_empty(),
            "an unknown-symbol event never emits an update: {updates:?}"
        );
    }

    #[test]
    fn test_route_crossed_quote_is_benign_drop() {
        // ask < bid is a crossed quote -> decode rejects it -> the adapter drops
        // the tick (keep prior), never emits, never signals reconnect/health.
        let updates = route(&RawDxEvent::Quote {
            symbol: DX_SYMBOL.to_owned(),
            bid: 2.0,
            ask: 1.0,
            bid_size: 10.0,
            ask_size: 20.0,
        });
        assert!(
            updates.is_empty(),
            "a crossed quote is a benign per-tick drop: {updates:?}"
        );
    }

    #[test]
    fn test_route_ignored_event_emits_nothing() {
        assert!(route(&RawDxEvent::Ignored).is_empty());
    }

    // === subscribe path through a mock transport ==============================

    /// A scripted [`DxlinkTransport`]: each `connect_and_subscribe` records the
    /// subscribed symbol set, then `receive` replays the scripted events for that
    /// connection until it is exhausted (which ends the connection -> reconnect).
    struct MockTransport {
        /// One `VecDeque` of events per connection; popped front-to-back.
        connections: VecDeque<VecDeque<RawDxEvent>>,
        current: Option<VecDeque<RawDxEvent>>,
        subscribed: Arc<StdMutex<Vec<Vec<String>>>>,
    }

    impl MockTransport {
        fn new(connections: Vec<Vec<RawDxEvent>>) -> Self {
            Self {
                connections: connections.into_iter().map(VecDeque::from).collect(),
                current: None,
                subscribed: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl DxlinkTransport for MockTransport {
        async fn connect_and_subscribe(
            &mut self,
            symbols: Vec<String>,
        ) -> Result<(), TransportGone> {
            if let Ok(mut log) = self.subscribed.lock() {
                log.push(symbols);
            }
            match self.connections.pop_front() {
                Some(events) => {
                    self.current = Some(events);
                    Ok(())
                }
                // No further scripted connections -> the connect fails, which the
                // loop treats as a drop (reconnect); the test cancels to stop it.
                None => Err(TransportGone),
            }
        }

        async fn receive(&mut self) -> Result<RawDxEvent, TransportGone> {
            match self.current.as_mut().and_then(VecDeque::pop_front) {
                Some(event) => Ok(event),
                // The scripted events are exhausted -> the stream ends (reconnect).
                None => {
                    self.current = None;
                    Err(TransportGone)
                }
            }
        }
    }

    #[test]
    fn test_subscribe_streams_health_and_decoded_updates() {
        block(async {
            let (sink, mut rx_control, mut rx_coalesced) = test_sink(32);
            let transport = MockTransport::new(vec![vec![
                RawDxEvent::Quote {
                    symbol: DX_SYMBOL.to_owned(),
                    bid: 1.5,
                    ask: 1.7,
                    bid_size: 10.0,
                    ask_size: 20.0,
                },
                RawDxEvent::Greeks {
                    symbol: DX_SYMBOL.to_owned(),
                    delta: 0.55,
                    gamma: 0.01,
                    theta: -0.05,
                    vega: 0.2,
                    rho: 0.03,
                    volatility: 0.35,
                },
            ]]);
            let subscribed = Arc::clone(&transport.subscribed);
            let cancel = CancellationToken::new();
            let handle = tokio::spawn(run_reconnect_loop(
                transport,
                pid("dxlink"),
                vec![dxlink_leg(STRIKE, OptionStyle::Call, 1)],
                sink,
                cancel.clone(),
            ));

            // Give the loop time to connect, emit Health(Live), and drain both
            // scripted events, then cancel to stop the reconnect.
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
            let _ = handle.await;

            let control = drain(&mut rx_control);
            let coalesced = drain(&mut rx_coalesced);

            // The subscribed symbol set was the leg's dxfeed symbol.
            match subscribed.lock() {
                Ok(log) => {
                    assert!(
                        log.iter().any(|set| set == &vec![DX_SYMBOL.to_owned()]),
                        "the leg's dxfeed symbol was subscribed: {log:?}"
                    );
                }
                Err(_) => panic!("subscribed log lock poisoned"),
            }

            // Health(Live) rides the control channel.
            assert!(
                control
                    .iter()
                    .any(|u| matches!(u, MarketUpdate::Health(_, StreamHealth::Live))),
                "Health(Live) is emitted on connect: {control:?}"
            );
            // The quote + greeks ride the coalesced channel.
            assert!(
                coalesced
                    .iter()
                    .any(|u| matches!(u, MarketUpdate::Quote(_))),
                "a QuoteUpdate is emitted: {coalesced:?}"
            );
            assert!(
                coalesced
                    .iter()
                    .any(|u| matches!(u, MarketUpdate::Greeks(_))),
                "a GreeksRow is emitted: {coalesced:?}"
            );
        });
    }

    #[test]
    fn test_subscribe_reconnect_lifecycle_emits_reconnecting() {
        block(async {
            let (sink, mut rx_control, _rx_coalesced) = test_sink(32);
            // First connection yields one event then ends (drop); the loop should
            // emit Health(Reconnecting) before the second connect.
            let transport = MockTransport::new(vec![
                vec![RawDxEvent::Quote {
                    symbol: DX_SYMBOL.to_owned(),
                    bid: 1.5,
                    ask: 1.7,
                    bid_size: 10.0,
                    ask_size: 20.0,
                }],
                vec![],
            ]);
            let cancel = CancellationToken::new();
            let handle = tokio::spawn(run_reconnect_loop(
                transport,
                pid("dxlink"),
                vec![dxlink_leg(STRIKE, OptionStyle::Call, 1)],
                sink,
                cancel.clone(),
            ));
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
            let _ = handle.await;

            let control = drain(&mut rx_control);
            assert!(
                control.iter().any(|u| matches!(
                    u,
                    MarketUpdate::Health(_, StreamHealth::Reconnecting { .. })
                )),
                "a dropped stream emits Health(Reconnecting): {control:?}"
            );
        });
    }

    #[test]
    fn test_subscribe_cancel_stops_loop_without_connecting_again() {
        block(async {
            let (sink, _rx_control, _rx_coalesced) = test_sink(8);
            let transport = MockTransport::new(vec![vec![]]);
            let subscribed = Arc::clone(&transport.subscribed);
            let cancel = CancellationToken::new();
            // Cancel BEFORE spawning drains: the loop must observe the token and
            // never open a stream after cancellation.
            cancel.cancel();
            let handle = tokio::spawn(run_reconnect_loop(
                transport,
                pid("dxlink"),
                vec![dxlink_leg(STRIKE, OptionStyle::Call, 1)],
                sink,
                cancel,
            ));
            let _ = handle.await;
            match subscribed.lock() {
                Ok(log) => assert!(
                    log.is_empty(),
                    "a pre-cancelled loop never connects: {log:?}"
                ),
                Err(_) => panic!("subscribed log lock poisoned"),
            }
        });
    }

    // === The overlay fingerprint gate (through the ChainStore) ================

    fn overlay_quote(bid: f64, ask: f64) -> QuoteUpdate {
        QuoteUpdate {
            instrument: dxlink_leg(STRIKE, OptionStyle::Call, 1),
            bid: Some(pos(bid)),
            ask: Some(pos(ask)),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: None,
            received_time: utc(EXP + 10),
        }
    }

    /// A store seeded from a `deribit` source chain whose alias catalog also
    /// carries the DXLink overlay leg — the cross-provider join. `overlay_mult` is
    /// the DXLink leg's fingerprint multiplier; the source leg's is always 1.
    fn overlay_store(overlay_mult: u32) -> ChainStore {
        let mut catalog = AliasCatalog::new();
        catalog.insert(source_leg(STRIKE, OptionStyle::Call, 1));
        catalog.insert(dxlink_leg(STRIKE, OptionStyle::Call, overlay_mult));
        // A second strike, source-only, to prove other legs are unaffected.
        catalog.insert(source_leg(STRIKE + 1_000.0, OptionStyle::Call, 1));

        let mut chain = OptionChain::new("BTC", pos(STRIKE), utc(EXP).to_rfc3339(), None, None);
        chain.add_option(
            pos(STRIKE),
            Some(pos(1.0)),
            Some(pos(1.2)),
            Some(pos(2.0)),
            Some(pos(2.4)),
            Positive::ZERO,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        chain.add_option(
            pos(STRIKE + 1_000.0),
            Some(pos(3.0)),
            Some(pos(3.2)),
            None,
            None,
            Positive::ZERO,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let fetch = ChainFetch::new(
            chain,
            ExpirySource::new("BTC", utc(EXP), pid("deribit")),
            catalog,
        );
        ChainStore::seed(fetch, ChainSource::Merged, Duration::from_secs(2), utc(EXP))
    }

    #[track_caller]
    fn call_bid(store: &ChainStore, strike: f64) -> Option<Positive> {
        store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(strike))
            .and_then(|o| o.call_bid)
    }

    #[test]
    fn test_matching_fingerprint_overlay_wins() {
        let mut store = overlay_store(1);
        let outcome = store.apply_quote(&overlay_quote(5.0, 5.2));
        assert_eq!(outcome, MergeOutcome::Applied);
        assert!(!store.is_overlay_refused(&ikey(STRIKE, OptionStyle::Call)));
        // The overlay (live stream) wins the quote field.
        assert_eq!(call_bid(&store, STRIKE), Some(pos(5.0)));
    }

    #[test]
    fn test_mismatched_fingerprint_refused_source_kept_others_unaffected() {
        let mut store = overlay_store(100);
        let outcome = store.apply_quote(&overlay_quote(5.0, 5.2));
        assert_eq!(outcome, MergeOutcome::OverlayRefused);
        assert!(store.is_overlay_refused(&ikey(STRIKE, OptionStyle::Call)));
        // The source leg is kept unchanged.
        assert_eq!(call_bid(&store, STRIKE), Some(pos(1.0)));
        // A different (source-only) leg is unaffected — refusal is per-leg.
        assert_eq!(call_bid(&store, STRIKE + 1_000.0), Some(pos(3.0)));
    }

    #[test]
    fn test_mismatched_greeks_overlay_also_refused() {
        let mut store = overlay_store(100);
        let row = GreeksRow {
            instrument: dxlink_leg(STRIKE, OptionStyle::Call, 1),
            iv: Some(pos(0.35)),
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(EXP + 10),
        };
        assert_eq!(store.apply_greeks(&row), MergeOutcome::OverlayRefused);
        assert!(store.is_overlay_refused(&ikey(STRIKE, OptionStyle::Call)));
    }

    // === Property: overlay_spec_gate ==========================================

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// A DXLink overlay merges a leg only when its `ContractSpecFingerprint`
        /// matches the source leg's; any mismatch is refused (OverlayError::
        /// SpecMismatch outcome) and the source leg kept. The source multiplier is
        /// 1, so the overlay merges iff `overlay_mult == 1`.
        #[test]
        fn prop_overlay_spec_gate(overlay_mult in 1u32..250) {
            let mut store = overlay_store(overlay_mult);
            let outcome = store.apply_quote(&overlay_quote(5.0, 5.2));
            let after = call_bid(&store, STRIKE);
            if overlay_mult == 1 {
                prop_assert_eq!(outcome, MergeOutcome::Applied);
                prop_assert_eq!(after, Some(pos(5.0)));
            } else {
                prop_assert_eq!(outcome, MergeOutcome::OverlayRefused);
                // Source leg kept.
                prop_assert_eq!(after, Some(pos(1.0)));
            }
        }
    }

    // === Backoff kernel (pure) ================================================

    #[test]
    fn test_backoff_delay_grows_and_caps() {
        let zero = backoff_delay(0, 0.0);
        assert_eq!(zero, Duration::from_millis(250));
        // Growth, then the 30 s ceiling.
        assert!(backoff_delay(4, 0.0) > backoff_delay(1, 0.0));
        assert_eq!(backoff_delay(30, 0.0), Duration::from_millis(30_000));
    }
}
