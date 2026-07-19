//! The Deribit adapter — the zero-config, public-data poll leg
//! (`docs/03-data-providers.md` §7.1, [ADR-0003]).
//!
//! Deribit has **no chain endpoint**, so this adapter *assembles* an
//! [`OptionChain`] from the upstream [`deribit-http`](DeribitHttpClient) client:
//! `get_instruments(currency, "option")` supplies the structure and
//! `get_ticker(instrument)` carries the mark price, implied volatility, and
//! Greeks per contract ([`ChainCapability::Assemble`]). Public market data needs
//! **no credentials**, so the adapter is the zero-config default and drives
//! [`HttpConfig::production`] — it never requires or logs a credential; the
//! public endpoints send none.
//!
//! # Normalization happens at this seam
//!
//! Every raw `deribit-http` DTO (`Instrument`, `TickerData`, `OptionInstrument`)
//! is translated into the ChainView domain model **inside this module** and never
//! escapes it (`CLAUDE.md` "Module Boundaries"): prices/IV/sizes become
//! [`Positive`], Greeks become [`Decimal`], and each contract's provider-agnostic
//! [`InstrumentKey`] plus its native symbol lands in the [`AliasCatalog`]. The
//! numeric conversion is **checked at the `f64` seam** (`CLAUDE.md` "Governance
//! precedence" item 2): `NaN`/`Inf`/negative is rejected before a value becomes a
//! `Positive`, a rejected price field is dropped, a crossed quote is refused, and
//! only a payload that cannot yield a valid strike/style/expiry rejects the whole
//! row as a [`ProviderError::Normalize`].
//!
//! Deribit IV is **percentage-form** (`49.22` == 49.22%), so it is divided by 100
//! to a decimal fraction; expiry is a **direct UTC instant** (08:00 UTC
//! settlement), taken from the instrument's `expiration_timestamp` (or parsed
//! from the `instrument_name` date code as a fallback), never a relative offset.
//!
//! # Streaming overlay + reconnect (issue #16)
//!
//! [`subscribe`](Provider::subscribe) opens the live overlay over
//! [`deribit-websocket`](DeribitWebSocketClient): `ticker.{instrument}`
//! (mark/IV/Greeks → [`QuoteUpdate`] + [`GreeksRow`]) and `book.{instrument}`
//! depth (snapshots + deltas → [`DepthLadder`] with the upstream `change_id`
//! captured for later gap-detect/resync). The **`trades.` tape is not
//! subscribed** (deferred). Streamed theta/vega/rho are **deliberately
//! discarded** — [`OptionData`](optionstratlib::chains::OptionData) cannot store
//! them and the local sidecar owns them
//! (`docs/01-domain-model.md` §7); the adapter forwards only the venue
//! delta/gamma/IV.
//!
//! `deribit-websocket` (0.3.1) ships **no** auto-reconnect, so the
//! reconnect/resubscribe loop is **ChainView's** (`docs/03-data-providers.md`
//! §5): on a dropped stream the adapter emits `Health(Reconnecting{attempt})`,
//! backs off with [jittered exponential backoff](backoff_delay), re-fetches the
//! chain to reconcile drift, and resubscribes off the **fresh
//! [`AliasCatalog`]** — the backfill is current state, never a replayed tape.
//!
//! # One sender, two update classes
//!
//! The port hands `subscribe` a **single** bounded [`mpsc::Sender`]. Over it the
//! adapter emits two classes: **control-class** updates (`Health` / the reconnect
//! backfill `Chain`) are **await-sent** — never coalesced or dropped — and
//! **coalesced-class** updates (`Quote` / `Greeks` / `Depth`) go through a
//! per-[`InstrumentKey`] **producer-side staging map** ([`ProducerStaging`]) that
//! **overwrites the staged slot on a full channel** so the freshest value
//! survives under sustained saturation (the producer mirror of the #10 consumer
//! conflater, `docs/02-tui-architecture.md` §5). This one sender cannot
//! physically *separate* a control channel; the true two-class priority (a
//! separate control channel drained first) is the **consumer bridge's** concern,
//! and the port→bridge two-sender routing is reconciled at the composition seam
//! (#22, per ADR-0009). The rustls crypto provider is installed once via
//! [`install_default_crypto_provider`] before the first WS TLS handshake.
//!
//! [ADR-0003]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0003-zero-config-first-run.md

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use deribit_http::model::instrument::{
    Instrument as DeribitInstrument, OptionType as DeribitOptionType,
};
use deribit_http::model::other::OptionInstrument;
use deribit_http::{DeribitHttpClient, HttpConfig, HttpError};
use deribit_websocket::install_default_crypto_provider;
use deribit_websocket::prelude::{
    DeribitWebSocketClient, NotificationHandler, SubscriptionChannel, WebSocketConfig,
};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
    Provider, ProviderCapabilities, SubscriptionHandle, SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSnapshot, ChainSource, ContractSpecFingerprint, DepthLadder,
    DepthLevel, ExerciseStyle, ExpirySource, GreeksOrigin, GreeksRow, Instrument, InstrumentKey,
    MarketUpdate, ProviderId, QuoteUpdate, SettlementStyle, StreamHealth,
};
use crate::error::{NormalizeKind, ProviderError, TransportDetail, TransportKind};

/// The reserved provider id this adapter registers under
/// ([`RESERVED_PROVIDER_IDS`](crate::chain::RESERVED_PROVIDER_IDS)).
const DERIBIT_ID: &str = "deribit";

/// The suggested chain-refresh cadence, in seconds — a hint only; the effective
/// interval is `config.refresh_interval` (`docs/03-data-providers.md` §2).
const REFRESH_HINT_SECS: u32 = 2;

/// The quote currency used when the instrument does not name one — Deribit option
/// premiums settle in the venue's stable quote currency.
const DEFAULT_QUOTE_CURRENCY: &str = "USD";

/// The largest `f64` that still round-trips through `u32` — the ceiling for a
/// checked `contract_size` -> `contract_multiplier` conversion (`u32::MAX`).
const MULTIPLIER_MAX_F64: f64 = 4_294_967_295.0;

/// The largest integer an `f64` represents exactly (2^53) — the ceiling for a
/// checked `open_interest` -> `u64` conversion, well above any real book.
const OI_MAX_F64: f64 = 9_007_199_254_740_992.0;

/// The cap on in-flight `get_ticker` requests during chain hydration — bounded
/// concurrency that keeps startup fast without hammering the venue's rate
/// limiter (`docs/06-performance.md`).
const MAX_CONCURRENT_TICKERS: usize = 16;

// --- Reconnect backoff (docs/03-data-providers.md §5) ------------------------

/// The reconnect backoff base, in milliseconds (`BASE = 250 ms`,
/// `docs/03-data-providers.md` §5). Used by the pure [`backoff_delay`] kernel.
const BACKOFF_BASE_MS: f64 = 250.0;

/// The reconnect backoff ceiling, in milliseconds (`MAX = 30 s`,
/// `docs/03-data-providers.md` §5).
const BACKOFF_MAX_MS: f64 = 30_000.0;

/// The reconnect jitter magnitude — the delay is scaled by `1 + jitter` with
/// `jitter ∈ [-0.2, 0.2]` (`docs/03-data-providers.md` §5).
const JITTER_MAGNITUDE: f64 = 0.2;

/// The largest exponent applied to `2^attempt` before the [`BACKOFF_MAX_MS`] cap
/// takes over — `250 ms * 2^7` already exceeds `30 s`, so a ceiling of `20` both
/// keeps `attempt` growth harmless and avoids the `powi` overflow/wrap a very
/// large `attempt` would otherwise reach.
const BACKOFF_MAX_SHIFT: u32 = 20;

/// How often the streaming loop retries a producer-staged flush while the feed
/// is quiet. A value coalesced onto a full channel would otherwise wait for the
/// next `publish` to flush it — and after a burst subsides that notification may
/// never come, stranding the freshest quote/greeks/depth exactly when the user
/// is watching a now-stale "latest". A bounded tick delivers it instead. The
/// cadence sits well within the render loop's 16 ms/60 fps frame budget, so a
/// staged value reaches the consumer by the next frame; the tick only arms while
/// a value is staged, so an idle stream never wakes
/// (`docs/02-tui-architecture.md` §5).
const STAGING_FLUSH_INTERVAL: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// The adapter.
// ---------------------------------------------------------------------------

/// The Deribit `Provider` adapter (crate-internal; registered through
/// [`ChainViewAppBuilder::with_builtins`](crate::ChainViewAppBuilder)).
///
/// Holds the upstream REST client (built for the production venue, no
/// credentials) and its reserved [`ProviderId`]. Raw upstream types stay inside
/// this module — nothing on the public surface names a `deribit-http` DTO.
///
/// `Clone` is cheap: [`DeribitHttpClient`] is `Arc`-backed and [`ProviderId`]
/// owns a short string. A clone is moved into the spawned reconnect loop so it
/// can re-`fetch_chain` (over REST) to reconcile drift on reconnect without
/// borrowing `&self` across the task boundary.
#[derive(Clone)]
pub(crate) struct DeribitAdapter {
    client: DeribitHttpClient,
    id: ProviderId,
}

impl DeribitAdapter {
    /// Build the adapter for the public production venue. Public market data
    /// needs no credentials, so none is required or sent on this path (ADR-0003);
    /// the client is constructed once and shared read-only across cold-path calls.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            client: DeribitHttpClient::with_config(HttpConfig::production()),
            id: deribit_provider_id(),
        }
    }

    /// Hydrate the selected instruments into normalized legs, fetching their
    /// per-contract tickers with **bounded concurrency** ([`MAX_CONCURRENT_TICKERS`]).
    ///
    /// Each task clones the cheap `Arc`-backed client, fetches one ticker, and
    /// normalizes it. The task's [`LegOutcome`] keeps three cases distinct: a
    /// normalized leg, a leg the venue ANSWERED but that would not normalize (a
    /// dropped leg), and a ticker FETCH that failed at the transport level. Only
    /// the last is an outage signal — it is counted, never erased, so
    /// [`fetch_chain`](Self::fetch_chain) can tell a total ticker outage apart
    /// from a genuinely empty expiry. A single dropped/failed leg degrades that
    /// leg only, never the whole chain. The completion order does not matter:
    /// [`collect_outcomes`] and [`assemble_chain`] are order-independent.
    async fn hydrate_legs(&self, selected: Vec<DeribitInstrument>) -> Hydration {
        let mut pending = selected.into_iter();
        let mut join_set: JoinSet<LegOutcome> = JoinSet::new();

        // Prime up to the concurrency cap.
        for _ in 0..MAX_CONCURRENT_TICKERS {
            let Some(instrument) = pending.next() else {
                break;
            };
            self.spawn_ticker(&mut join_set, instrument);
        }

        let mut outcomes = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            let outcome = match joined {
                Ok(outcome) => outcome,
                // A task panic/cancel is a local bug, not a venue outage: fold it
                // to a dropped leg, never a transport failure that fakes an outage.
                Err(_) => LegOutcome::Dropped,
            };
            outcomes.push(outcome);
            if let Some(instrument) = pending.next() {
                self.spawn_ticker(&mut join_set, instrument);
            }
        }
        collect_outcomes(outcomes)
    }

    /// Spawn one bounded ticker-hydration task onto `join_set`. The task owns a
    /// cloned client and the instrument, so it is `'static`. It resolves to a
    /// [`LegOutcome`]: a hydrated leg; a [`Dropped`](LegOutcome::Dropped) leg the
    /// venue answered but that would not normalize; or a
    /// [`TransportFailed`](LegOutcome::TransportFailed) ticker fetch. The fetch
    /// failure is REPORTED, not swallowed into `None`, so a full outage stays
    /// visible.
    fn spawn_ticker(&self, join_set: &mut JoinSet<LegOutcome>, instrument: DeribitInstrument) {
        let client = self.client.clone();
        let _ = join_set.spawn(async move {
            let ticker = match client.get_ticker(&instrument.instrument_name).await {
                Ok(ticker) => ticker,
                Err(_) => return LegOutcome::TransportFailed,
            };
            let option = OptionInstrument { instrument, ticker };
            match normalize_leg(&option) {
                Ok(leg) => LegOutcome::Hydrated(Box::new(leg)),
                Err(_) => LegOutcome::Dropped,
            }
        });
    }
}

#[async_trait]
impl Provider for DeribitAdapter {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        deribit_capabilities()
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        // The venue's tradeable currencies ARE the underlyings Deribit offers
        // (BTC/ETH/...); expirations are resolved per underlying via
        // `fetch_chain`, so they are not surfaced here.
        let currencies = self
            .client
            .get_currencies()
            .await
            .map_err(|err| transport_error(&err))?;
        Ok(currencies
            .into_iter()
            .map(|currency| UnderlyingRef::new(currency.currency))
            .collect())
    }

    async fn fetch_chain(
        &self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        let currency = underlying.to_ascii_uppercase();

        // Resolve the requested expiry to an absolute-UTC target day; a relative
        // `Days` offset never reaches an `InstrumentKey` — it is resolved here.
        let target = expiration
            .get_date()
            .map_err(|_| ProviderError::Normalize {
                kind: NormalizeKind::UnparseableExpiry,
            })?;
        let target_day = target.date_naive();

        // Discover the currency's option instruments over REST (Deribit has no
        // chain endpoint), then keep the requested expiry.
        let instruments = self
            .client
            .get_instruments(&currency, Some("option"), Some(false))
            .await
            .map_err(|err| transport_error(&err))?;

        // Select the requested expiry's option instruments (no I/O), then hydrate
        // their tickers CONCURRENTLY with a bounded `JoinSet` so a large expiry
        // (~80-200 contracts) meets the startup-to-first-chain budget without a
        // sequential round-trip per instrument and without hammering the venue
        // (ADR-0007, `docs/06-performance.md`).
        let selected: Vec<DeribitInstrument> = instruments
            .into_iter()
            .filter(|instrument| {
                instrument.is_option()
                    && instrument
                        .expiration_timestamp
                        .and_then(DateTime::<Utc>::from_timestamp_millis)
                        .is_some_and(|expiry| expiry.date_naive() == target_day)
            })
            .collect();
        let selected_count = selected.len();

        let Hydration {
            legs,
            transport_failures,
        } = self.hydrate_legs(selected).await;

        if legs.is_empty() {
            // Zero legs is ambiguous: a genuinely empty/delisted expiry, or a
            // total ticker OUTAGE whose fetch failures were counted rather than
            // erased. `empty_expiry_outcome` distinguishes them so an outage
            // surfaces as a transport error (a reconnecting/error state + the
            // mode-correct retry), never a NoChain that reads as "no options
            // here" (docs 03 §6; the Codex review of PR #73).
            return Err(empty_expiry_outcome(
                selected_count,
                transport_failures,
                &currency,
                target,
            ));
        }

        // The assembled chain is grouped by strike in a `BTreeMap`, so it is
        // independent of the order tickers complete in; the expiry is shared by
        // every leg of one expiry, so any leg's resolved instant is authoritative.
        let expiration_utc = legs.first().map_or(target, |leg| leg.key.expiration_utc);
        let spot =
            legs.iter()
                .find_map(|leg| leg.underlying_price)
                .ok_or(ProviderError::Normalize {
                    kind: NormalizeKind::MissingField("underlying_price"),
                })?;

        Ok(assemble_chain(
            &currency,
            spot,
            expiration_utc,
            &legs,
            &self.id,
        ))
    }

    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        tx: mpsc::Sender<MarketUpdate>,
    ) -> Result<SubscriptionHandle, ProviderError> {
        // The adapter OWNS the reconnect/resubscribe loop — `deribit-websocket`
        // (0.3.1) ships no auto-reconnect (`docs/03-data-providers.md` §5). The
        // loop runs behind the returned handle; dropping (or aborting) the handle
        // cancels the token — a clean cooperative stop — and aborts the task as a
        // hard backstop, so there is no fire-and-forget spawn.
        //
        // The loop is generic over the [`DeribitTransport`] seam (issue #17): the
        // production [`LiveTransport`] wraps the upstream WebSocket client plus the
        // REST backfill, while tests inject a mock so the reconnect lifecycle runs
        // with no socket and no wall clock. Cold-path config assembly (venue URL
        // from env or the production default); no credential is read or required —
        // public data only.
        let cancel = CancellationToken::new();
        let loop_cancel = cancel.clone();
        let transport = LiveTransport::new(self.clone(), WebSocketConfig::default());
        let id = self.id.clone();
        let SubscriptionRequest {
            underlying,
            expiration_utc,
            instruments,
        } = req;
        let handle = tokio::spawn(run_reconnect_loop(
            transport,
            id,
            underlying,
            expiration_utc,
            instruments,
            tx,
            loop_cancel,
        ));
        Ok(SubscriptionHandle::new(move || {
            cancel.cancel();
            handle.abort();
        }))
    }
}

// ---------------------------------------------------------------------------
// Ticker hydration outcomes: a fetch outage vs. an empty expiry.
// ---------------------------------------------------------------------------

/// The outcome of hydrating one instrument's ticker into a leg.
///
/// It keeps a ticker-FETCH failure distinct from a normalize DROP. Erasing the
/// two together (as a bare `Option`) let a total ticker outage — every
/// `get_ticker` failing — masquerade as an empty expiry, indistinguishable from
/// a genuine delisting (the Codex review of PR #73). The `NormalizedLeg` is boxed
/// so the hydrated variant does not bloat the whole enum.
enum LegOutcome {
    /// The ticker fetched and its payload normalized into a leg.
    Hydrated(Box<NormalizedLeg>),
    /// The ticker fetched, but its payload would not normalize (a bad
    /// strike/style/expiry). The leg is skipped — the venue still ANSWERED, so it
    /// is NOT an outage.
    Dropped,
    /// The ticker FETCH failed at the transport level. Counted, not erased, so a
    /// total outage is distinguishable from a genuinely empty expiry.
    TransportFailed,
}

/// The result of hydrating an expiry's instruments: the normalized legs plus the
/// number of ticker fetches that failed at the transport level.
struct Hydration {
    /// The successfully normalized legs — possibly a partial set (some tickers
    /// may have failed or been dropped).
    legs: Vec<NormalizedLeg>,
    /// How many `get_ticker` fetches failed at the transport level — the outage
    /// signal [`empty_expiry_outcome`] reads when zero legs hydrate.
    transport_failures: usize,
}

/// Fold per-ticker [`LegOutcome`]s into a [`Hydration`]: collect the hydrated
/// legs and count the transport-level fetch failures (a dropped leg is neither).
/// Order-independent, matching the bounded-concurrency hydration's arbitrary
/// completion order.
fn collect_outcomes(outcomes: impl IntoIterator<Item = LegOutcome>) -> Hydration {
    let mut legs = Vec::new();
    let mut transport_failures = 0usize;
    for outcome in outcomes {
        match outcome {
            LegOutcome::Hydrated(leg) => legs.push(*leg),
            LegOutcome::TransportFailed => transport_failures += 1,
            LegOutcome::Dropped => {}
        }
    }
    Hydration {
        legs,
        transport_failures,
    }
}

/// Classify an expiry that hydrated ZERO legs: a transport OUTAGE versus a
/// genuinely empty or delisted expiry ([`ProviderError::NoChain`]).
///
/// A non-empty instrument list that produced no legs BECAUSE a ticker fetch
/// failed at the transport level is an outage — surfaced as a transport error so
/// the UI shows a reconnecting/error state and the mode-correct retry, never a
/// "no options here" that hides a venue/network failure. An empty instrument list
/// (a real delisting), or one whose tickers all ANSWERED but yielded no
/// normalizable leg (`transport_failures == 0`), is a genuine empty expiry. Only
/// called when the hydrated leg set is empty; a single hydrated leg is a
/// partial-success chain, never routed here.
fn empty_expiry_outcome(
    selected_count: usize,
    transport_failures: usize,
    underlying: &str,
    expiration: DateTime<Utc>,
) -> ProviderError {
    if selected_count > 0 && transport_failures > 0 {
        return transport(TransportKind::Closed);
    }
    ProviderError::NoChain {
        underlying: underlying.to_owned(),
        expiration: expiration.to_rfc3339(),
    }
}

// ---------------------------------------------------------------------------
// Identity + capabilities.
// ---------------------------------------------------------------------------

/// The adapter's reserved [`ProviderId`].
///
/// `"deribit"` is a compile-time literal that satisfies the `ProviderId` grammar
/// (proven by `test_deribit_id_is_valid_and_reserved`), so construction cannot
/// fail; the fallback arm is unreachable and never taken — the documented
/// infallible-for-this-literal pattern (no `unwrap`/`expect` in production).
fn deribit_provider_id() -> ProviderId {
    match ProviderId::new(DERIBIT_ID) {
        Ok(id) => id,
        Err(_) => unreachable!("`deribit` is a valid, reserved provider id literal"),
    }
}

/// Deribit's honest capability self-declaration — the exact
/// `docs/03-data-providers.md` §8 row: an assembled chain with option depth,
/// venue-provided Greeks/IV, an (unverified) contract quote stream, an underlying
/// stream, REST chain polling, no trades tape, and **no auth** (public data).
#[must_use]
pub(crate) fn deribit_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        .depth(true)
        .greeks(GreeksCapability::Provided)
        .option_stream(OptionStreamCapability::ChainQuotes { verified: false })
        .underlying_stream(true)
        .chain_poll(ChainPollCapability::Poll {
            interval_hint_secs: REFRESH_HINT_SECS,
        })
        .trades_tape(false)
        .auth(AuthKind::None)
        .build()
}

// ---------------------------------------------------------------------------
// Field-specific numeric normalization at the f64 seam.
// ---------------------------------------------------------------------------

/// A checked price/size field: `NaN`/`Inf`/negative is **dropped** (returns
/// `None`), so a bad tick never becomes a fabricated `Positive`. Zero is a valid
/// value and is kept.
fn positive_or_drop(value: f64) -> Option<Positive> {
    Positive::new(value).ok()
}

/// A strike is **row-fatal** when invalid: `NaN`/`Inf` is
/// [`NonFinite`](NormalizeKind::NonFinite) and a non-positive strike is
/// [`OutOfRange`](NormalizeKind::OutOfRange) (a zero/negative strike is not a
/// real contract).
fn strike_positive(value: f64) -> Result<Positive, NormalizeKind> {
    if !value.is_finite() {
        return Err(NormalizeKind::NonFinite("strike"));
    }
    if value <= 0.0 {
        return Err(NormalizeKind::OutOfRange("strike"));
    }
    let strike = Positive::new(value).map_err(|_| NormalizeKind::OutOfRange("strike"))?;
    // A sub-`Decimal`-precision strike (e.g. an underflowing subnormal) rounds to
    // zero, which is not a real contract — reject it as out of range.
    if strike == Positive::ZERO {
        return Err(NormalizeKind::OutOfRange("strike"));
    }
    Ok(strike)
}

/// Normalize a Deribit implied-volatility figure from **percentage-form**
/// (`49.22` == 49.22%) to a decimal fraction (`0.4922`).
///
/// `NaN`/`Inf` is [`NonFinite`](NormalizeKind::NonFinite); a negative IV is
/// [`OutOfRange`](NormalizeKind::OutOfRange); zero is valid.
fn normalize_iv(mark_iv: f64) -> Result<Positive, NormalizeKind> {
    if !mark_iv.is_finite() {
        return Err(NormalizeKind::NonFinite("iv"));
    }
    let fraction = mark_iv / 100.0;
    Positive::new(fraction).map_err(|_| NormalizeKind::OutOfRange("iv"))
}

/// A checked Greek: `NaN`/`Inf`/out-of-range is dropped (Greeks may legitimately
/// be negative, so there is no sign check). Uses the std `TryFrom<f64>`
/// conversion (rounding to a clean decimal), so no `rust_decimal` trait import is
/// needed.
fn greek_or_drop(value: Option<f64>) -> Option<Decimal> {
    let raw = value?;
    if !raw.is_finite() {
        return None;
    }
    Decimal::try_from(raw).ok()
}

/// A normalized best-bid/best-ask pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NormalizedQuote {
    bid: Option<Positive>,
    ask: Option<Positive>,
}

/// Normalize a best-bid/best-ask pair with Deribit's field-specific rules
/// (`docs/03-data-providers.md` §3 table):
///
/// - a per-field `NaN`/`Inf`/negative is **dropped** to `None` (keeps the rest of
///   the quote);
/// - a **zero bid is valid** (kept — the midpoint is still derivable);
/// - a **zero ask on a non-zero bid**, or any `ask < bid`, is **crossed** — the
///   whole update is rejected ([`OutOfRange`](NormalizeKind::OutOfRange) on
///   `ask`) so a torn quote never overwrites a good one.
fn normalize_quote(bid: Option<f64>, ask: Option<f64>) -> Result<NormalizedQuote, NormalizeKind> {
    let bid = bid.and_then(positive_or_drop);
    let ask = ask.and_then(positive_or_drop);
    if let (Some(bid_value), Some(ask_value)) = (bid, ask) {
        // A zero ask on a non-zero bid satisfies `ask < bid`, so both crossed
        // cases collapse to this single check.
        if ask_value < bid_value {
            return Err(NormalizeKind::OutOfRange("ask"));
        }
    }
    Ok(NormalizedQuote { bid, ask })
}

// ---------------------------------------------------------------------------
// Symbol + expiry mapping.
// ---------------------------------------------------------------------------

/// The components parsed out of a Deribit `instrument_name`
/// (`SYMBOL-DDMMMYY-STRIKE-STYLE`, e.g. `BTC-27JUN25-60000-C`).
#[derive(Debug, Clone)]
struct ParsedName {
    underlying: String,
    expiry_code: String,
    strike: f64,
    style: OptionStyle,
}

/// Parse a Deribit option `instrument_name` into its four components
/// (`docs/03-data-providers.md` §3, responsibility 5). The name is uppercased and
/// split on `-`; anything that is not exactly `SYMBOL-DDMMMYY-STRIKE-{C|P}` is a
/// typed [`NormalizeKind`] naming the offending component, never a panic.
fn parse_instrument_name(name: &str) -> Result<ParsedName, NormalizeKind> {
    let mut parts = name.split('-');
    let underlying = parts
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(NormalizeKind::MissingField("instrument_name"))?;
    let expiry_code = parts
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    let strike_segment = parts
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(NormalizeKind::MissingField("strike"))?;
    let style_segment = parts
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(NormalizeKind::UnknownStyle)?;
    // A fifth segment means a compound/combo name, not a plain option.
    if parts.next().is_some() {
        return Err(NormalizeKind::MissingField("instrument_name"));
    }

    let strike = strike_segment
        .parse::<f64>()
        .map_err(|_| NormalizeKind::OutOfRange("strike"))?;
    let style = match style_segment.to_ascii_uppercase().as_str() {
        "C" => OptionStyle::Call,
        "P" => OptionStyle::Put,
        _ => return Err(NormalizeKind::UnknownStyle),
    };

    Ok(ParsedName {
        underlying: underlying.to_ascii_uppercase(),
        expiry_code: expiry_code.to_ascii_uppercase(),
        strike,
        style,
    })
}

/// Resolve a Deribit `DDMMMYY` expiry code (e.g. `27JUN25`) to an absolute UTC
/// instant at the venue's **08:00 UTC settlement** (`docs/03-data-providers.md`
/// §3). A non-ASCII, malformed, or calendar-invalid code is
/// [`UnparseableExpiry`](NormalizeKind::UnparseableExpiry) — never a silently
/// keyed row.
fn expiry_code_to_utc(code: &str) -> Result<DateTime<Utc>, NormalizeKind> {
    // ASCII guarantees `split_at` lands on char boundaries.
    if !code.is_ascii() || code.len() < 6 {
        return Err(NormalizeKind::UnparseableExpiry);
    }
    let year_at = code
        .len()
        .checked_sub(2)
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    let (head, year_str) = code.split_at(year_at);
    let month_at = head
        .len()
        .checked_sub(3)
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    let (day_str, month_str) = head.split_at(month_at);

    if day_str.is_empty() {
        return Err(NormalizeKind::UnparseableExpiry);
    }
    let day = day_str
        .parse::<u32>()
        .map_err(|_| NormalizeKind::UnparseableExpiry)?;
    let year_two = year_str
        .parse::<i32>()
        .map_err(|_| NormalizeKind::UnparseableExpiry)?;
    // Deribit codes are 21st-century two-digit years.
    let year = 2000 + year_two;
    let month = month_from_code(month_str)?;

    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or(NormalizeKind::UnparseableExpiry)?;
    let naive = date
        .and_hms_opt(8, 0, 0)
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Map a three-letter Deribit month code (`JAN`..`DEC`) to a 1-based month.
fn month_from_code(code: &str) -> Result<u32, NormalizeKind> {
    let month = match code {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return Err(NormalizeKind::UnparseableExpiry),
    };
    Ok(month)
}

/// Resolve a Deribit millisecond epoch to an absolute UTC instant — the DIRECT
/// expiry the venue publishes (`docs/03-data-providers.md` §3, "use it
/// directly"). An out-of-range value is
/// [`UnparseableExpiry`](NormalizeKind::UnparseableExpiry).
fn utc_from_millis(millis: i64) -> Result<DateTime<Utc>, NormalizeKind> {
    DateTime::<Utc>::from_timestamp_millis(millis).ok_or(NormalizeKind::UnparseableExpiry)
}

/// Build a provider-agnostic [`InstrumentKey`] purely from a Deribit
/// `instrument_name` — the symbol mapping (`docs/03-data-providers.md` §3): the
/// underlying, the 08:00-UTC expiry parsed from the date code, the checked
/// strike, and the style.
fn instrument_key_from_name(name: &str) -> Result<InstrumentKey, NormalizeKind> {
    let parsed = parse_instrument_name(name)?;
    let expiration_utc = expiry_code_to_utc(&parsed.expiry_code)?;
    let strike = strike_positive(parsed.strike)?;
    Ok(InstrumentKey {
        underlying: parsed.underlying,
        expiration_utc,
        strike,
        style: parsed.style,
    })
}

/// Translate a Deribit option type into the domain [`OptionStyle`].
fn style_of(option_type: DeribitOptionType) -> OptionStyle {
    match option_type {
        DeribitOptionType::Call => OptionStyle::Call,
        DeribitOptionType::Put => OptionStyle::Put,
    }
}

// ---------------------------------------------------------------------------
// Leg normalization: one OptionInstrument -> one NormalizedLeg.
// ---------------------------------------------------------------------------

/// One normalized contract leg — the domain values assembled into an
/// [`OptionChain`] row and its [`AliasCatalog`] entry. Numbers are already
/// checked at the `f64` seam, so every field is a valid domain numeric.
#[derive(Debug, Clone)]
struct NormalizedLeg {
    key: InstrumentKey,
    native_symbol: String,
    spec: ContractSpecFingerprint,
    bid: Option<Positive>,
    ask: Option<Positive>,
    iv: Option<Positive>,
    delta: Option<Decimal>,
    gamma: Option<Decimal>,
    volume: Option<Positive>,
    open_interest: Option<u64>,
    underlying_price: Option<Positive>,
    style: OptionStyle,
}

/// Normalize one upstream `OptionInstrument` (instrument + ticker) into a
/// [`NormalizedLeg`].
///
/// The join key is derived from the native `instrument_name` and then refined by
/// the instrument's **typed** fields when present (strike, option type, and the
/// direct-UTC `expiration_timestamp` — the authoritative expiry). A payload that
/// cannot yield a valid strike/style/expiry rejects the ROW with a typed
/// [`NormalizeKind`]; a crossed quote drops only the quote (the row is kept); a
/// bad price/IV/Greek field is dropped to `None`. `NaN`/`Inf` never becomes a
/// `Positive`.
fn normalize_leg(option: &OptionInstrument) -> Result<NormalizedLeg, NormalizeKind> {
    let instrument = &option.instrument;
    let ticker = &option.ticker;

    let mut key = instrument_key_from_name(&instrument.instrument_name)?;
    if let Some(strike) = instrument.strike {
        key.strike = strike_positive(strike)?;
    }
    if let Some(option_type) = instrument.option_type.clone() {
        key.style = style_of(option_type);
    }
    if let Some(millis) = instrument.expiration_timestamp {
        key.expiration_utc = utc_from_millis(millis)?;
    }

    // A crossed quote is refused (whole quote dropped); the row still carries its
    // strike/style/expiry. Once the tracing sink lands this is where a WARN goes.
    let quote = normalize_quote(ticker.best_bid_price, ticker.best_ask_price).unwrap_or_default();
    let iv = ticker.mark_iv.and_then(|value| normalize_iv(value).ok());
    let (delta, gamma) = match &ticker.greeks {
        Some(greeks) => (greek_or_drop(greeks.delta), greek_or_drop(greeks.gamma)),
        None => (None, None),
    };
    let volume = positive_or_drop(ticker.stats.volume);
    let open_interest = ticker.open_interest.and_then(oi_to_u64);
    let underlying_price = ticker
        .underlying_price
        .or(ticker.index_price)
        .and_then(positive_or_drop);
    let spec = deribit_fingerprint(instrument, &key.underlying);
    let style = key.style;

    Ok(NormalizedLeg {
        key,
        native_symbol: instrument.instrument_name.clone(),
        spec,
        bid: quote.bid,
        ask: quote.ask,
        iv,
        delta,
        gamma,
        volume,
        open_interest,
        underlying_price,
        style,
    })
}

/// The Deribit economic-equivalence fingerprint: options are **cash-settled,
/// European-exercise**, quoted in the instrument's quote currency (default
/// `USD`), with the base currency as the venue product code and the contract size
/// as the multiplier.
fn deribit_fingerprint(
    instrument: &DeribitInstrument,
    underlying: &str,
) -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: contract_multiplier_of(instrument),
        settlement: SettlementStyle::Cash,
        exercise: ExerciseStyle::European,
        quote_currency: instrument
            .quote_currency
            .clone()
            .unwrap_or_else(|| DEFAULT_QUOTE_CURRENCY.to_owned()),
        venue_product_code: underlying.to_owned(),
    }
}

/// A checked `contract_size` -> `contract_multiplier` conversion: only an
/// in-range value `>= 1` is used, else the multiplier defaults to `1` (Deribit
/// crypto options are single-contract). The cast is range-guarded, so it never
/// saturates or wraps.
fn contract_multiplier_of(instrument: &DeribitInstrument) -> u32 {
    match instrument.contract_size {
        Some(size) if size.is_finite() && (1.0..=MULTIPLIER_MAX_F64).contains(&size) => {
            size.trunc() as u32
        }
        _ => 1,
    }
}

/// A checked `open_interest` -> `u64` conversion: only a finite, non-negative,
/// in-range value is kept; anything else is dropped to `None`. The cast is
/// range-guarded, so it never saturates or wraps.
fn oi_to_u64(value: f64) -> Option<u64> {
    if value.is_finite() && (0.0..=OI_MAX_F64).contains(&value) {
        Some(value.trunc() as u64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Chain assembly.
// ---------------------------------------------------------------------------

/// The call/put legs sharing one strike.
#[derive(Debug, Default)]
struct StrikePair<'a> {
    call: Option<&'a NormalizedLeg>,
    put: Option<&'a NormalizedLeg>,
}

/// Assemble the normalized legs into a single `optionstratlib` [`OptionChain`]
/// plus its [`AliasCatalog`] and [`ExpirySource`], grouping the call and put at
/// each strike into one `OptionData` row (`docs/03-data-providers.md` §7.1). The
/// chain's expiry string is the RFC 3339 absolute instant, so no relative offset
/// reaches the stored chain.
fn assemble_chain(
    underlying: &str,
    spot: Positive,
    expiration_utc: DateTime<Utc>,
    legs: &[NormalizedLeg],
    provider: &ProviderId,
) -> ChainFetch {
    let mut aliases = AliasCatalog::new();
    for leg in legs {
        aliases.insert(Instrument {
            key: leg.key.clone(),
            provider: provider.clone(),
            native_symbol: leg.native_symbol.clone(),
            stream_symbol: None,
            spec: leg.spec.clone(),
        });
    }

    let mut by_strike: BTreeMap<Positive, StrikePair<'_>> = BTreeMap::new();
    for leg in legs {
        let entry = by_strike.entry(leg.key.strike).or_default();
        match leg.style {
            OptionStyle::Call => entry.call = Some(leg),
            OptionStyle::Put => entry.put = Some(leg),
        }
    }

    let mut chain = OptionChain::new(underlying, spot, expiration_utc.to_rfc3339(), None, None);
    for (strike, pair) in by_strike {
        // `add_option` requires a single IV per strike; prefer the call's, fall
        // back to the put's, and default a fabricated-free zero when neither feed
        // supplied one (a valid zero IV per the normalization table).
        let iv = pair
            .call
            .and_then(|leg| leg.iv)
            .or_else(|| pair.put.and_then(|leg| leg.iv))
            .unwrap_or(Positive::ZERO);
        chain.add_option(
            strike,
            pair.call.and_then(|leg| leg.bid),
            pair.call.and_then(|leg| leg.ask),
            pair.put.and_then(|leg| leg.bid),
            pair.put.and_then(|leg| leg.ask),
            iv,
            pair.call.and_then(|leg| leg.delta),
            pair.put.and_then(|leg| leg.delta),
            pair.call
                .and_then(|leg| leg.gamma)
                .or_else(|| pair.put.and_then(|leg| leg.gamma)),
            pair.call
                .and_then(|leg| leg.volume)
                .or_else(|| pair.put.and_then(|leg| leg.volume)),
            pair.call
                .and_then(|leg| leg.open_interest)
                .or_else(|| pair.put.and_then(|leg| leg.open_interest)),
            None,
        );
    }

    ChainFetch::new(
        chain,
        ExpirySource::new(underlying, expiration_utc, provider.clone()),
        aliases,
    )
}

// ---------------------------------------------------------------------------
// Render-golden data source (#19) — test-only.
// ---------------------------------------------------------------------------

/// Assemble the recorded BTC option chain from the committed #17 fixtures through
/// the **real** adapter seam ([`normalize_leg`] + [`assemble_chain`]), under the
/// caller-supplied `underlying` display symbol — the source for the chain render
/// goldens (#19, `docs/TESTING.md` §4) and the escape-hygiene seam probe.
///
/// Driving the golden through the actual normalize/assemble path (not a hand-built
/// chain) proves the rendered matrix reflects the adapter's output. Passing a
/// **hostile** `underlying` also proves the seam keeps venue bytes verbatim — the
/// domain never mangles a venue string — so it is the render edge, not the domain,
/// that neutralizes the escape sequence (`docs/SECURITY.md` §6.4). `#[cfg(test)]`,
/// so it never rides in the release binary; the fixture bytes are baked in with
/// `include_str!`, so the golden is byte-stable across machines (no I/O, no socket).
#[cfg(test)]
pub(crate) fn fixture_btc_chain_fetch_named(underlying: &str) -> ChainFetch {
    use deribit_http::model::ticker::TickerData;
    use deribit_websocket::prelude::Value;

    const INSTRUMENTS_JSON: &str =
        include_str!("../../tests/fixtures/deribit/instruments/instruments_btc.json");
    // A DISTINCT recorded ticker per instrument, so the assembled chain depicts a
    // believable call/put asymmetry (a call delta near +0.55, a put delta near
    // -0.45, the 61000 call its own distinct quote) — not one cloned ticker. This
    // also removes a test blind spot: with distinct per-leg Greeks the golden would
    // catch a projection that read the wrong leg's delta.
    const TICKER_60000_CALL_JSON: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_normal.json");
    const TICKER_60000_PUT_JSON: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_put.json");
    const TICKER_61000_CALL_JSON: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_61000_call.json");

    fn ticker_json_for(instrument_name: &str) -> &'static str {
        match instrument_name {
            "BTC-27JUN25-60000-P" => TICKER_60000_PUT_JSON,
            "BTC-27JUN25-61000-C" => TICKER_61000_CALL_JSON,
            _ => TICKER_60000_CALL_JSON,
        }
    }

    fn deserialize_ticker(json: &str) -> TickerData {
        let value: Value = match json.parse() {
            Ok(value) => value,
            Err(e) => panic!("ticker fixture must parse: {e}"),
        };
        match TickerData::deserialize(&value) {
            Ok(ticker) => ticker,
            Err(e) => panic!("ticker fixture must deserialize: {e}"),
        }
    }

    let instruments_value: Value = match INSTRUMENTS_JSON.parse() {
        Ok(value) => value,
        Err(e) => panic!("instruments fixture must parse: {e}"),
    };
    let instruments = match Vec::<DeribitInstrument>::deserialize(&instruments_value) {
        Ok(list) => list,
        Err(e) => panic!("instruments fixture must deserialize: {e}"),
    };

    let legs: Vec<NormalizedLeg> = instruments
        .into_iter()
        .filter(|instrument| instrument.is_option())
        .filter_map(|instrument| {
            let ticker = deserialize_ticker(ticker_json_for(&instrument.instrument_name));
            normalize_leg(&OptionInstrument { instrument, ticker }).ok()
        })
        .collect();

    let spot = match legs.iter().find_map(|leg| leg.underlying_price) {
        Some(spot) => spot,
        None => panic!("the recorded tickers carry an underlying price"),
    };
    let expiration_utc = match legs.first() {
        Some(leg) => leg.key.expiration_utc,
        None => panic!("the recorded fixture yields at least one normalized leg"),
    };

    assemble_chain(
        underlying,
        spot,
        expiration_utc,
        &legs,
        &deribit_provider_id(),
    )
}

// ---------------------------------------------------------------------------
// Redaction-safe transport error mapping.
// ---------------------------------------------------------------------------

/// Map an upstream [`HttpError`] to a redaction-safe [`ProviderError`] by
/// **category only** — the inner message (which may hold a URL or body) is never
/// interpolated (`docs/03-data-providers.md` §6).
fn transport_error(err: &HttpError) -> ProviderError {
    match err {
        HttpError::AuthenticationFailed(_) => ProviderError::Auth,
        HttpError::RateLimitExceeded => ProviderError::RateLimited(None),
        HttpError::NetworkError(_) => transport(TransportKind::Closed),
        HttpError::RequestFailed(_) | HttpError::ConfigError(_) => transport(TransportKind::Http),
        HttpError::InvalidResponse(_) | HttpError::ParseError(_) => {
            transport(TransportKind::Decode)
        }
    }
}

/// A [`ProviderError::Transport`] carrying only a category (no status, no
/// upstream text).
fn transport(kind: TransportKind) -> ProviderError {
    ProviderError::Transport(Box::new(TransportDetail::new(kind, None)))
}

// ---------------------------------------------------------------------------
// Reconnect backoff — the pure, injectable-jitter kernel.
// ---------------------------------------------------------------------------

/// The jittered exponential backoff delay for reconnect attempt `attempt`
/// (`docs/03-data-providers.md` §5):
///
/// ```text
/// delay = min(MAX, BASE * 2^attempt) * (1 + jitter)
/// ```
///
/// with `BASE = 250 ms`, `MAX = 30 s`, and `jitter ∈ [-0.2, 0.2]` (values
/// outside the range are clamped). This is a **pure** kernel: `jitter` is
/// **injected**, not sampled — the loop calls it with [`sample_jitter`], while
/// tests pass a fixed value so the mapping is deterministic (no wall clock, no
/// unseeded RNG). `attempt = 0` maps to exactly `BASE`, so a reset-to-zero would
/// restart the ramp at `BASE`.
///
/// Note the loop passes a **1-based** `attempt` (matching the 1-based
/// `Reconnecting { attempt }` badge): it increments to `1` before the *first*
/// backoff, so the first retry delay is `BASE * 2^1 ≈ 500 ms` (with jitter), not
/// `BASE`. `attempt = 0` is the kernel's identity point, not a delay the loop
/// ever waits.
///
/// The result never exceeds `MAX * (1 + 0.2) = 36 s` and never drops below
/// `BASE * (1 - 0.2) = 200 ms`.
#[must_use]
fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let exponent = attempt.min(BACKOFF_MAX_SHIFT);
    let uncapped = BACKOFF_BASE_MS * 2.0_f64.powi(exponent as i32);
    let capped = uncapped.min(BACKOFF_MAX_MS);
    let jitter = jitter.clamp(-JITTER_MAGNITUDE, JITTER_MAGNITUDE);
    let millis = capped * (1.0 + jitter);
    Duration::from_secs_f64(millis / 1000.0)
}

/// Sample a reconnect jitter in `[-0.2, 0.2)` from the process clock's
/// sub-second nanoseconds — enough entropy to spread simultaneous reconnects,
/// with no RNG dependency. It is deliberately **outside** the [`backoff_delay`]
/// kernel (which takes the jitter as a parameter) so the kernel stays pure and
/// deterministic under test.
fn sample_jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let unit = f64::from(nanos) / 1_000_000_000.0; // [0, 1)
    (unit * 2.0 - 1.0) * JITTER_MAGNITUDE // [-0.2, 0.2)
}

/// The current wall-clock instant as a normalization `received_time`
/// (`docs/01-domain-model.md` §5.1). Uses `std`'s clock (chrono's `clock`
/// feature is off), clamps a pathological system time to the representable
/// range, and never `unwrap`s. Called only in the impure loop; the pure
/// normalization functions take `received` as a parameter.
fn now_utc() -> DateTime<Utc> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp(secs, since.subsec_nanos()).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Resolve a Deribit millisecond epoch to an optional venue `event_time` — an
/// out-of-range value yields `None` (the stream is not rejected; the event
/// simply carries no venue clock, `docs/01-domain-model.md` §5.1).
fn millis_to_event_time(millis: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(millis)
}

// ---------------------------------------------------------------------------
// Raw streaming payload DTOs — deserialized from the subscription JSON, and
// never escaping this module.
// ---------------------------------------------------------------------------

/// The `ticker.{instrument}` notification payload (`docs/03-data-providers.md`
/// §7.1). Only the fields the overlay reads are named; every one is optional so
/// a partial or unfamiliar payload deserializes rather than rejecting the frame.
#[derive(Debug, Clone, Deserialize)]
struct TickerPayload {
    #[serde(default)]
    best_bid_price: Option<f64>,
    #[serde(default)]
    best_ask_price: Option<f64>,
    #[serde(default)]
    best_bid_amount: Option<f64>,
    #[serde(default)]
    best_ask_amount: Option<f64>,
    #[serde(default)]
    last_price: Option<f64>,
    #[serde(default)]
    mark_iv: Option<f64>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    greeks: Option<GreeksPayload>,
}

/// The `greeks` object inside a ticker payload. Only `delta` and `gamma` are
/// read; the venue's `theta`/`vega`/`rho` are **deliberately discarded**
/// (`docs/01-domain-model.md` §7) — `OptionData` cannot store them and the local
/// sidecar owns them — so they are not even deserialized here (serde ignores the
/// unmodeled JSON fields), and [`normalize_ticker`] always emits `None` for them.
#[derive(Debug, Clone, Deserialize)]
struct GreeksPayload {
    #[serde(default)]
    delta: Option<f64>,
    #[serde(default)]
    gamma: Option<f64>,
}

/// The `book.{instrument}.{group}` notification payload
/// (`docs/03-data-providers.md` §7.1): best-first `bids`/`asks`, the upstream
/// `change_id` for gap-detect/resync, and a venue `timestamp`. A snapshot and a
/// delta frame share this shape; delta application is the depth screen's job
/// (v0.5) — the adapter normalizes each frame's levels into a [`DepthLadder`].
#[derive(Debug, Clone, Deserialize)]
struct BookPayload {
    #[serde(default)]
    change_id: Option<u64>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
}

/// One order-book level, in either Deribit encoding: an aggregated
/// `[price, amount]` pair, or a raw-book `[action, price, amount]` triple whose
/// leading action string (`"new"`/`"change"`/`"delete"`) is ignored here — only
/// the price and size are normalized (`docs/03-data-providers.md` §7.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum BookLevel {
    /// The aggregated-book encoding: `[price, amount]`.
    Priced([f64; 2]),
    /// The raw-book encoding: `[action, price, amount]`.
    Actioned(String, f64, f64),
}

impl BookLevel {
    /// The `(price, amount)` this level carries, dropping the raw-book action
    /// tag when present.
    fn price_size(&self) -> (f64, f64) {
        match self {
            BookLevel::Priced([price, amount]) => (*price, *amount),
            BookLevel::Actioned(_action, price, amount) => (*price, *amount),
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming normalization — ticker -> QuoteUpdate + GreeksRow, book -> ladder.
// ---------------------------------------------------------------------------

/// Normalize a `ticker.{instrument}` payload into a [`QuoteUpdate`] **and** a
/// [`GreeksRow`] for the resolved [`Instrument`]
/// (`docs/03-data-providers.md` §3 table). The same field-specific rules as the
/// poll leg apply at the `f64` seam: a crossed/torn quote drops the bid/ask to
/// `None` (the store keeps the prior values), a `NaN`/`Inf`/negative field is
/// dropped, IV is percentage-form and divided by 100, and venue delta/gamma are
/// forwarded as `Decimal`. **Streamed theta/vega/rho are discarded**
/// (`docs/01-domain-model.md` §7) — the returned `GreeksRow` always carries
/// `None` for them.
fn normalize_ticker(
    instrument: &Instrument,
    payload: &TickerPayload,
    received: DateTime<Utc>,
) -> (QuoteUpdate, GreeksRow) {
    let quote = normalize_quote(payload.best_bid_price, payload.best_ask_price).unwrap_or_default();
    let event_time = payload.timestamp.and_then(millis_to_event_time);

    let quote_update = QuoteUpdate {
        instrument: instrument.clone(),
        bid: quote.bid,
        ask: quote.ask,
        last: payload.last_price.and_then(positive_or_drop),
        bid_size: payload.best_bid_amount.and_then(positive_or_drop),
        ask_size: payload.best_ask_amount.and_then(positive_or_drop),
        event_time,
        received_time: received,
    };

    let iv = payload.mark_iv.and_then(|value| normalize_iv(value).ok());
    let (delta, gamma) = match &payload.greeks {
        Some(greeks) => (greek_or_drop(greeks.delta), greek_or_drop(greeks.gamma)),
        None => (None, None),
    };
    let greeks_row = GreeksRow {
        instrument: instrument.clone(),
        iv,
        delta,
        gamma,
        // Streamed theta/vega/rho are deliberately discarded (docs/01 §7).
        theta: None,
        vega: None,
        rho: None,
        origin: GreeksOrigin::Provider,
        event_time,
        received_time: received,
    };

    (quote_update, greeks_row)
}

/// Normalize a `book.{instrument}.{group}` payload into a [`DepthLadder`] for the
/// resolved [`Instrument`], capturing the upstream `change_id` for later
/// gap-detect/resync (`docs/01-domain-model.md` §5, `docs/03-data-providers.md`
/// §7.1). A level whose price or size is `NaN`/`Inf`/negative is dropped (the
/// rest of the ladder survives); the venue `timestamp` becomes the optional
/// `event_time`. Levels are forwarded best-first, as Deribit sends them.
fn normalize_book(
    instrument: &Instrument,
    payload: &BookPayload,
    received: DateTime<Utc>,
) -> DepthLadder {
    DepthLadder {
        instrument: instrument.clone(),
        bids: payload.bids.iter().filter_map(depth_level).collect(),
        asks: payload.asks.iter().filter_map(depth_level).collect(),
        event_time: payload.timestamp.and_then(millis_to_event_time),
        received_time: received,
        change_id: payload.change_id,
    }
}

/// A checked one-level conversion: a level whose price or size is
/// `NaN`/`Inf`/negative is dropped to `None` (never a fabricated [`Positive`]).
fn depth_level(level: &BookLevel) -> Option<DepthLevel> {
    let (price, size) = level.price_size();
    Some(DepthLevel {
        price: positive_or_drop(price)?,
        size: positive_or_drop(size)?,
    })
}

// ---------------------------------------------------------------------------
// Producer-side overwrite-on-full staging (docs/02-tui-architecture.md §5).
// ---------------------------------------------------------------------------

/// Whether the bounded fan-in channel is still open. A closed channel means the
/// consumer (the app) is gone, so the reconnect loop shuts down rather than
/// reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendState {
    /// The channel accepted the update (directly or into the staging slot).
    Open,
    /// The channel is closed — the consumer dropped its receiver.
    Closed,
}

/// One producer-staged slot for one instrument: the latest of each coalesced
/// update **kind** independently, so a Greeks refresh never clobbers a pending
/// quote — the producer mirror of the #10 consumer `StagedInstrument`.
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
/// (`docs/02-tui-architecture.md` §5). This completes the NFR-15
/// latest-value-wins guarantee under sustained saturation — the mirror of the
/// #10 consumer-side staging. It is O(N subscribed) and **reuses its
/// allocation** across bursts (`HashMap::retain` on flush retains the buckets;
/// a repeat update for an already-staged instrument clones no key).
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
    /// channel slot. Gates the streaming loop's flush tick so an idle stream
    /// (nothing staged) never wakes, while a burst residue is retried until it
    /// drains.
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

    /// Overwrite the staged slot for `update`'s instrument, last-value-wins per
    /// kind. A control-class update never reaches here (the loop sends `Health`
    /// / `Chain` directly), but the match stays total over the closed
    /// [`MarketUpdate`] set with no wildcard arm.
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
    /// cloned **only** when the slot is vacant (the HP-3 discipline), mirroring
    /// the consumer bridge; the `None` arm is treated as a no-op, never an
    /// `expect` (the lint policy forbids `expect`).
    fn slot_mut(&mut self, key: &InstrumentKey) -> Option<&mut StagedInstrument> {
        if !self.slots.contains_key(key) {
            let _ = self.slots.insert(key.clone(), StagedInstrument::default());
        }
        self.slots.get_mut(key)
    }
}

// ---------------------------------------------------------------------------
// The transport seam (issue #17): the venue I/O the reconnect loop drives.
// ---------------------------------------------------------------------------

/// The transport is gone — a connect/subscribe step failed or the socket
/// dropped/errored mid-stream. The reconnect loop maps it to
/// [`StreamExit::Reconnect`]. A zero-size marker: it carries no upstream text,
/// so no venue string can ride along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransportGone;

/// The venue-I/O seam the reconnect loop drives so the loop is deterministically
/// testable against a **mock** transport — no real socket, no wall clock (issue
/// #17, `docs/TESTING.md` §5). `deribit-websocket` (0.3.1) ships no
/// auto-reconnect and connects through a concrete client, so #16's loop reached
/// straight for that client; this trait lifts the three impure operations the
/// loop performs (connect+subscribe, receive a frame, re-fetch the chain for the
/// backfill) behind one seam. The production [`LiveTransport`] wraps the upstream
/// WebSocket client plus the adapter's REST `fetch_chain`; a test mock yields
/// scripted frames/errors. The public [`Provider`] surface is unchanged — this
/// seam is a crate-internal implementation detail and no raw upstream DTO crosses
/// it.
#[async_trait]
trait DeribitTransport: Send {
    /// Open one connection and (re)subscribe `channels`. `Ok(())` means the
    /// stream is live and ready to [`receive`](Self::receive); `Err(_)` is a
    /// recoverable connect/subscribe failure — the loop backs off and retries.
    async fn connect_and_subscribe(&mut self, channels: Vec<String>) -> Result<(), TransportGone>;

    /// Await the next raw notification frame. `Err(_)` means the socket dropped
    /// or errored — the loop surfaces `Reconnecting` and reconnects.
    async fn receive(&mut self) -> Result<String, TransportGone>;

    /// Re-fetch the chain to reconcile drift on reconnect (backfill = current
    /// state, `docs/03-data-providers.md` §5). `None` on a failed/cancelled fetch
    /// — the caller keeps the prior aliases.
    async fn refetch(
        &mut self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Option<ChainFetch>;
}

/// The production [`DeribitTransport`]: the upstream `deribit-websocket` client
/// for live frames and the adapter's REST `fetch_chain` for the reconnect
/// backfill — the #16 behaviour, now behind the #17 seam. Raw upstream types stay
/// inside it: the `DeribitWebSocketClient` session is private and never escapes.
struct LiveTransport {
    /// The REST adapter, cloned in for the reconnect backfill (`refetch`).
    adapter: DeribitAdapter,
    /// The WebSocket config (venue URL); no credential — public data only.
    ws_config: WebSocketConfig,
    /// The current live session, replaced on each (re)connect. `None` before the
    /// first connect and between attempts.
    session: Option<DeribitWebSocketClient>,
}

impl LiveTransport {
    /// Build the live transport from the REST adapter and the WS config.
    fn new(adapter: DeribitAdapter, ws_config: WebSocketConfig) -> Self {
        Self {
            adapter,
            ws_config,
            session: None,
        }
    }
}

#[async_trait]
impl DeribitTransport for LiveTransport {
    async fn connect_and_subscribe(&mut self, channels: Vec<String>) -> Result<(), TransportGone> {
        // The rustls crypto provider must be installed before the TLS handshake;
        // it is process-global and idempotent (a repeat call is `AlreadyInstalled`).
        let _ = install_default_crypto_provider();
        let client = DeribitWebSocketClient::new(&self.ws_config).map_err(|_| TransportGone)?;
        client.connect().await.map_err(|_| TransportGone)?;
        client
            .subscribe(channels)
            .await
            .map_err(|_| TransportGone)?;
        self.session = Some(client);
        Ok(())
    }

    async fn receive(&mut self) -> Result<String, TransportGone> {
        match self.session.as_ref() {
            Some(client) => client.receive_message().await.map_err(|_| TransportGone),
            None => Err(TransportGone),
        }
    }

    async fn refetch(
        &mut self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Option<ChainFetch> {
        self.adapter.fetch_chain(underlying, expiration).await.ok()
    }
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

/// The adapter-owned reconnect/resubscribe loop (`docs/03-data-providers.md`
/// §5). `deribit-websocket` ships no auto-reconnect, so ChainView drives it:
/// connect, resubscribe, drain updates; on a drop emit
/// `Health(Reconnecting{attempt})`, back off with jitter, **re-`fetch_chain`**
/// to reconcile drift, then resubscribe off the **fresh** aliases. `attempt`
/// resets to 0 on a successful (re)subscribe. Cancellation (handle drop) is
/// observed at every `.await` via a `biased` `select!`, so the loop never opens
/// a socket after cancellation and never hot-loops.
async fn run_reconnect_loop<T: DeribitTransport>(
    mut transport: T,
    id: ProviderId,
    underlying: String,
    expiration_utc: DateTime<Utc>,
    mut instruments: Vec<Instrument>,
    tx: mpsc::Sender<MarketUpdate>,
    cancel: CancellationToken,
) {
    let mut attempt: u32 = 0;
    loop {
        // Stop before opening any socket if cancelled or the consumer is gone —
        // so a closed channel is noticed at the top of the loop, never after a
        // wasted connect cycle.
        if cancel.is_cancelled() || tx.is_closed() {
            return;
        }
        let exit = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            exit = connect_stream_once(&mut transport, &id, &instruments, &tx, &cancel, &mut attempt) => exit,
        };
        if matches!(exit, StreamExit::Shutdown) || cancel.is_cancelled() {
            return;
        }
        // The stream dropped: surface the reconnect honestly, then back off.
        // `attempt` is 1-based here and MUST NOT wrap back to 0 (that would reset
        // the ramp), so it is held at the ceiling rather than saturated.
        attempt = attempt.checked_add(1).unwrap_or(attempt);
        let health = MarketUpdate::Health(id.clone(), StreamHealth::Reconnecting { attempt });
        // Cancel-wrapped await-send: on a full shared channel this must still
        // observe cancellation promptly (and stop cleanly if the consumer is gone).
        let health_sent = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            result = tx.send(health) => result,
        };
        if health_sent.is_err() {
            return; // consumer gone
        }
        let delay = backoff_delay(attempt, sample_jitter());
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        // Backfill = CURRENT STATE: re-fetch the chain to reconcile any drift
        // during the outage, then resubscribe off the fresh aliases next loop.
        if let Some(fresh) = refetch(
            &mut transport,
            &id,
            &underlying,
            expiration_utc,
            &tx,
            &cancel,
        )
        .await
        {
            if !fresh.is_empty() {
                instruments = fresh;
            }
        }
    }
}

/// One connection attempt over the [`DeribitTransport`] seam: connect + subscribe
/// the `ticker.`/`book.` channels, then drain updates until the socket drops or
/// the subscription is cancelled. `attempt` is reset to 0 on a successful
/// (re)subscribe (the loop-level reset-on-success guarantee, asserted through the
/// mock transport in #17). Returns [`StreamExit::Reconnect`] on a recoverable
/// drop and [`StreamExit::Shutdown`] on cancellation or a closed consumer channel.
async fn connect_stream_once<T: DeribitTransport>(
    transport: &mut T,
    id: &ProviderId,
    instruments: &[Instrument],
    tx: &mpsc::Sender<MarketUpdate>,
    cancel: &CancellationToken,
    attempt: &mut u32,
) -> StreamExit {
    // Resubscribe the exact `ticker.`/`book.` set for these legs. Cancellation is
    // observed by racing the connect+subscribe against the token, so the loop
    // never opens a socket after cancellation.
    let channels = subscription_channels(instruments);
    let subscribed = tokio::select! {
        biased;
        () = cancel.cancelled() => return StreamExit::Shutdown,
        result = transport.connect_and_subscribe(channels) => result,
    };
    if subscribed.is_err() {
        return StreamExit::Reconnect;
    }

    // A successful (re)subscribe resets the backoff ramp and surfaces `Live`.
    *attempt = 0;
    let live = MarketUpdate::Health(id.clone(), StreamHealth::Live);
    if tx.send(live).await.is_err() {
        return StreamExit::Shutdown;
    }

    let lookup = instrument_lookup(instruments);
    let mut staging = ProducerStaging::new();
    // The producer flushes staged values at the start of the next `publish`, but
    // once a burst subsides and the feed goes quiet no further `publish` arrives
    // to drain the freshest staged value. A bounded tick flushes it instead, so
    // the latest quote/greeks/depth is delivered promptly after capacity frees
    // rather than stranded. The tick is gated on `has_pending`, so an idle stream
    // never wakes; `Delay` keeps a post-idle re-arm from firing a catch-up burst.
    let mut flush_tick = tokio::time::interval(STAGING_FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let message = tokio::select! {
            biased;
            () = cancel.cancelled() => return StreamExit::Shutdown,
            // The bounded staging flush (#16) fires through the same loop, gated
            // on `has_pending`, so the freshest coalesced value is delivered after
            // a burst subsides even when no further message arrives.
            _ = flush_tick.tick(), if staging.has_pending() => {
                if staging.flush(tx) == SendState::Closed {
                    return StreamExit::Shutdown; // consumer gone
                }
                continue;
            }
            // The message source is the `DeribitTransport` seam (#17) so the
            // stream lifecycle is exercised under a mock with no real socket.
            message = transport.receive() => message,
        };
        let text = match message {
            Ok(text) => text,
            Err(_) => return StreamExit::Reconnect, // socket closed / errored
        };
        if route_message(&text, &lookup, &mut staging, tx) == SendState::Closed {
            return StreamExit::Shutdown; // consumer gone
        }
    }
}

/// Re-fetch the chain to reconcile drift and emit the fresh `Chain` snapshot,
/// returning the fresh Deribit legs for the next resubscribe (backfill = current
/// state, `docs/03-data-providers.md` §5). Cancellation short-circuits to `None`;
/// a failed fetch keeps the prior aliases (the caller does not overwrite). The
/// fetch itself goes through the [`DeribitTransport`] seam, so the reconnect
/// backfill is exercised with no REST call under a mock transport (#17).
async fn refetch<T: DeribitTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    tx: &mpsc::Sender<MarketUpdate>,
    cancel: &CancellationToken,
) -> Option<Vec<Instrument>> {
    let expiration = ExpirationDate::DateTime(expiration_utc);
    let fetched = tokio::select! {
        biased;
        () = cancel.cancelled() => return None,
        result = transport.refetch(underlying, &expiration) => result,
    };
    let fetch = fetched?;

    // Emit the reconciled structure as a control-class `Chain` (await-send,
    // never coalesced/dropped) so the store reconciles drift. Cancel-wrapped so a
    // full shared channel cannot defer cancellation during the backfill send.
    let snapshot = MarketUpdate::Chain(chain_snapshot(&fetch, now_utc()));
    let snapshot_sent = tokio::select! {
        biased;
        () = cancel.cancelled() => return None,
        result = tx.send(snapshot) => result,
    };
    if snapshot_sent.is_err() {
        return None;
    }

    let instruments: Vec<Instrument> = fetch
        .aliases
        .instruments()
        .filter(|instrument| instrument.provider == *id)
        .cloned()
        .collect();
    Some(instruments)
}

/// Assemble a streaming-current [`ChainSnapshot`] from a re-fetched
/// [`ChainFetch`] — the same `AliasCatalog` carried forward with no
/// re-derivation. The source is [`ChainSource::Merged`] (a REST poll seeds
/// structure, the stream overlays quotes) and the health [`StreamHealth::Live`]
/// (the resubscribe follows).
fn chain_snapshot(fetch: &ChainFetch, last_poll: DateTime<Utc>) -> ChainSnapshot {
    ChainSnapshot {
        chain_key: (
            fetch.expiry_source.provider.clone(),
            fetch.expiry_source.underlying.clone(),
            fetch.expiry_source.expiration_utc,
        ),
        chain: fetch.chain.clone(),
        aliases: fetch.aliases.clone(),
        source: ChainSource::Merged,
        health: StreamHealth::Live,
        last_full_poll: Some(last_poll),
    }
}

/// The `ticker.{instrument}` and `book.{instrument}.{group}` channels to
/// subscribe for these legs, built through the upstream [`SubscriptionChannel`]
/// helper (never hand-formatted). **`trades.{instrument}` is intentionally not
/// subscribed** — the trades tape is deferred (`docs/03-data-providers.md` §8).
fn subscription_channels(instruments: &[Instrument]) -> Vec<String> {
    // Two channels (ticker + book) per leg; fall back to the leg count if the
    // doubled hint would overflow (a purely defensive capacity hint).
    let hint = instruments
        .len()
        .checked_mul(2)
        .unwrap_or(instruments.len());
    let mut channels = Vec::with_capacity(hint);
    for instrument in instruments {
        let native = instrument.native_symbol.clone();
        channels.push(SubscriptionChannel::Ticker(native.clone()).channel_name());
        channels.push(SubscriptionChannel::OrderBook(native).channel_name());
    }
    channels
}

/// Index the subscribed legs by their native `instrument_name`, so an incoming
/// notification's channel resolves back to the normalized [`Instrument`]. A
/// stream update for a symbol not in this map is dropped (an unknown-symbol
/// guard, `docs/03-data-providers.md` §4).
fn instrument_lookup(instruments: &[Instrument]) -> HashMap<String, Instrument> {
    instruments
        .iter()
        .map(|instrument| (instrument.native_symbol.clone(), instrument.clone()))
        .collect()
}

/// Decode one raw notification frame and publish the normalized updates.
///
/// A frame that is not a subscription notification, carries no channel/data,
/// names an unfamiliar channel, resolves to an **unknown symbol**, or fails to
/// deserialize is **skipped** (never a panic). A `ticker.` frame yields a
/// [`QuoteUpdate`] and a [`GreeksRow`]; a `book.` frame a [`DepthLadder`]; both
/// go through the producer staging. Returns [`SendState::Closed`] once the fan-in
/// channel is closed.
fn route_message(
    text: &str,
    lookup: &HashMap<String, Instrument>,
    staging: &mut ProducerStaging,
    tx: &mpsc::Sender<MarketUpdate>,
) -> SendState {
    let handler = NotificationHandler::new();
    let Ok(notification) = handler.parse_notification(text) else {
        return SendState::Open;
    };
    if !handler.is_subscription_notification(&notification) {
        return SendState::Open;
    }
    let (Some(channel), Some(data)) = (
        handler.extract_channel(&notification),
        handler.extract_data(&notification),
    ) else {
        return SendState::Open;
    };
    let received = now_utc();

    // The instrument is always the SECOND dotted segment for both families —
    // `ticker.{instrument}[.{interval}]` and `book.{instrument}.{group}`. Deribit
    // can echo a trailing interval on the ticker channel, so we must take the
    // instrument segment and ignore any suffix (a plain `strip_prefix("ticker.")`
    // would leave `{instrument}.{interval}` and silently drop every quote).
    // Instrument names carry `-`, never `.`, so the segment split is safe.
    let instrument_segment = channel.split('.').nth(1);

    if channel.starts_with("ticker.") {
        let Some(symbol) = instrument_segment else {
            return SendState::Open;
        };
        let Some(instrument) = lookup.get(symbol) else {
            return SendState::Open; // unknown-symbol guard
        };
        let Ok(payload) = TickerPayload::deserialize(&data) else {
            return SendState::Open;
        };
        let (quote, greeks) = normalize_ticker(instrument, &payload, received);
        if staging.publish(tx, MarketUpdate::Quote(quote)) == SendState::Closed {
            return SendState::Closed;
        }
        return staging.publish(tx, MarketUpdate::Greeks(greeks));
    }

    if channel.starts_with("book.") {
        let Some(symbol) = instrument_segment else {
            return SendState::Open;
        };
        let Some(instrument) = lookup.get(symbol) else {
            return SendState::Open; // unknown-symbol guard
        };
        let Ok(payload) = BookPayload::deserialize(&data) else {
            return SendState::Open;
        };
        let ladder = normalize_book(instrument, &payload, received);
        return staging.publish(tx, MarketUpdate::Depth(ladder));
    }

    SendState::Open // an unsubscribed channel family (e.g. we never open `trades.`)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use deribit_http::model::instrument::InstrumentKind;
    use deribit_http::model::other::Greeks;
    use deribit_http::model::ticker::{TickerData, TickerStats};
    use deribit_websocket::prelude::Value;
    use proptest::prelude::*;

    use super::*;

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    #[track_caller]
    fn utc_millis(millis: i64) -> DateTime<Utc> {
        match DateTime::<Utc>::from_timestamp_millis(millis) {
            Some(t) => t,
            None => panic!("invalid test millis: {millis}"),
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// A `deribit-http` instrument with the fields the adapter reads; the rest
    /// default. `expiration_timestamp` is a Deribit millisecond epoch.
    fn deribit_instrument(
        name: &str,
        strike: Option<f64>,
        option_type: Option<DeribitOptionType>,
        expiration_ms: Option<i64>,
    ) -> DeribitInstrument {
        DeribitInstrument {
            instrument_name: name.to_owned(),
            kind: Some(InstrumentKind::Option),
            currency: Some("BTC".to_owned()),
            is_active: Some(true),
            expiration_timestamp: expiration_ms,
            strike,
            option_type,
            contract_size: Some(1.0),
            quote_currency: Some("USD".to_owned()),
            ..DeribitInstrument::default()
        }
    }

    /// A ticker carrying only the fields the adapter reads (bid/ask/mark/IV/
    /// Greeks/volume/underlying); `TickerData` has no `Default`, so the rest are
    /// filled with inert values.
    fn ticker(
        name: &str,
        best_bid_price: Option<f64>,
        best_ask_price: Option<f64>,
        mark_iv: Option<f64>,
        greeks: Option<Greeks>,
    ) -> TickerData {
        TickerData {
            instrument_name: name.to_owned(),
            last_price: None,
            mark_price: 0.05,
            best_bid_price,
            best_ask_price,
            best_bid_amount: 0.0,
            best_ask_amount: 0.0,
            volume: Some(12.0),
            volume_usd: None,
            open_interest: Some(34.0),
            high: None,
            low: None,
            price_change: None,
            price_change_percentage: None,
            bid_iv: None,
            ask_iv: None,
            mark_iv,
            timestamp: 0,
            state: "open".to_owned(),
            settlement_price: None,
            stats: TickerStats {
                volume: 12.0,
                volume_usd: None,
                price_change: None,
                high: None,
                low: None,
            },
            greeks,
            index_price: Some(61_000.0),
            min_price: None,
            max_price: None,
            interest_rate: None,
            underlying_price: Some(60_500.0),
            underlying_index: None,
            estimated_delivery_price: None,
        }
    }

    fn greeks(delta: Option<f64>, gamma: Option<f64>) -> Greeks {
        Greeks {
            delta,
            gamma,
            vega: None,
            theta: None,
            rho: None,
        }
    }

    /// A fully-populated call `OptionInstrument` for `BTC-27JUN25-60000-C`.
    fn sample_option() -> OptionInstrument {
        OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-60000-C",
                Some(60_000.0),
                Some(DeribitOptionType::Call),
                Some(1_751_011_200_000), // 2025-06-27T08:00:00Z
            ),
            ticker: ticker(
                "BTC-27JUN25-60000-C",
                Some(0.05),
                Some(0.06),
                Some(49.22),
                Some(greeks(Some(0.55), Some(0.0001))),
            ),
        }
    }

    // --- capabilities() matches the docs 03 §8 Deribit row exactly -----------

    #[test]
    fn test_deribit_capabilities_match_section_8_row() {
        let caps = deribit_capabilities();
        assert_eq!(caps.chain, ChainCapability::Assemble);
        assert!(caps.depth);
        assert_eq!(caps.greeks, GreeksCapability::Provided);
        assert_eq!(
            caps.option_stream,
            OptionStreamCapability::ChainQuotes { verified: false }
        );
        assert!(caps.underlying_stream);
        assert_eq!(
            caps.chain_poll,
            ChainPollCapability::Poll {
                interval_hint_secs: REFRESH_HINT_SECS
            }
        );
        assert!(!caps.trades_tape);
        assert_eq!(caps.auth, AuthKind::None);
    }

    #[test]
    fn test_deribit_id_is_valid_and_reserved() {
        let id = deribit_provider_id();
        assert_eq!(id.as_str(), "deribit");
        assert!(id.is_reserved());
        // Proves the `deribit_provider_id` fallback arm is genuinely unreachable.
        assert!(ProviderId::new(DERIBIT_ID).is_ok());
    }

    // --- IV: percentage-form -> decimal --------------------------------------

    #[test]
    fn test_deribit_normalize_iv_divides_by_100() {
        match normalize_iv(49.22) {
            Ok(iv) => assert_eq!(iv, pos(0.4922)),
            Err(e) => panic!("expected 49.22% -> 0.4922, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_iv_zero_is_valid() {
        match normalize_iv(0.0) {
            Ok(iv) => assert_eq!(iv, Positive::ZERO),
            Err(e) => panic!("expected zero IV to be valid, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_iv_rejects_non_finite() {
        assert_eq!(normalize_iv(f64::NAN), Err(NormalizeKind::NonFinite("iv")));
        assert_eq!(
            normalize_iv(f64::INFINITY),
            Err(NormalizeKind::NonFinite("iv"))
        );
    }

    #[test]
    fn test_deribit_normalize_iv_rejects_negative() {
        assert_eq!(normalize_iv(-1.0), Err(NormalizeKind::OutOfRange("iv")));
    }

    // --- Quote field rules ----------------------------------------------------

    #[test]
    fn test_deribit_normalize_quote_keeps_zero_bid() {
        match normalize_quote(Some(0.0), Some(1.0)) {
            Ok(quote) => {
                assert_eq!(quote.bid, Some(Positive::ZERO));
                assert_eq!(quote.ask, Some(pos(1.0)));
            }
            Err(e) => panic!("a zero bid is valid, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_quote_rejects_zero_ask_on_nonzero_bid() {
        assert_eq!(
            normalize_quote(Some(5.0), Some(0.0)),
            Err(NormalizeKind::OutOfRange("ask"))
        );
    }

    #[test]
    fn test_deribit_normalize_quote_rejects_crossed() {
        assert_eq!(
            normalize_quote(Some(5.0), Some(3.0)),
            Err(NormalizeKind::OutOfRange("ask"))
        );
    }

    #[test]
    fn test_deribit_normalize_quote_drops_negative_price_field() {
        match normalize_quote(Some(-1.0), Some(2.0)) {
            Ok(quote) => {
                assert_eq!(quote.bid, None);
                assert_eq!(quote.ask, Some(pos(2.0)));
            }
            Err(e) => panic!("a negative bid drops only that field, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_quote_drops_non_finite_price_field() {
        match normalize_quote(Some(f64::NAN), Some(2.0)) {
            Ok(quote) => {
                assert_eq!(quote.bid, None);
                assert_eq!(quote.ask, Some(pos(2.0)));
            }
            Err(e) => panic!("a NaN bid drops only that field, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_quote_both_zero_is_valid() {
        match normalize_quote(Some(0.0), Some(0.0)) {
            Ok(quote) => {
                assert_eq!(quote.bid, Some(Positive::ZERO));
                assert_eq!(quote.ask, Some(Positive::ZERO));
            }
            Err(e) => panic!("a zero bid AND zero ask is valid, got {e}"),
        }
    }

    // --- Greeks ---------------------------------------------------------------

    #[test]
    fn test_deribit_greek_decimal_keeps_negative() {
        // A put delta is negative and must be preserved as a signed Decimal.
        match greek_or_drop(Some(-0.45)) {
            Some(delta) => assert_eq!(delta, Decimal::new(-45, 2)),
            None => panic!("a negative Greek must be kept"),
        }
    }

    #[test]
    fn test_deribit_greek_decimal_drops_non_finite() {
        assert_eq!(greek_or_drop(Some(f64::NAN)), None);
        assert_eq!(greek_or_drop(Some(f64::INFINITY)), None);
        assert_eq!(greek_or_drop(None), None);
    }

    // --- Symbol -> InstrumentKey mapping + the name parse --------------------

    #[test]
    fn test_deribit_parse_instrument_name_maps_fields() {
        match parse_instrument_name("BTC-27JUN25-60000-C") {
            Ok(parsed) => {
                assert_eq!(parsed.underlying, "BTC");
                assert_eq!(parsed.expiry_code, "27JUN25");
                assert_eq!(parsed.strike, 60_000.0);
                assert_eq!(parsed.style, OptionStyle::Call);
            }
            Err(e) => panic!("expected a clean parse, got {e}"),
        }
    }

    #[test]
    fn test_deribit_instrument_name_maps_to_instrument_key() {
        match instrument_key_from_name("BTC-27JUN25-60000-P") {
            Ok(key) => {
                assert_eq!(key.underlying, "BTC");
                assert_eq!(key.strike, pos(60_000.0));
                assert_eq!(key.style, OptionStyle::Put);
                // 08:00 UTC settlement on 2025-06-27.
                assert_eq!(key.expiration_utc, utc_millis(1_751_011_200_000));
            }
            Err(e) => panic!("expected a clean key mapping, got {e}"),
        }
    }

    #[test]
    fn test_deribit_expiry_code_resolves_to_0800_utc() {
        match expiry_code_to_utc("27JUN25") {
            Ok(instant) => {
                assert_eq!(instant.to_rfc3339(), "2025-06-27T08:00:00+00:00");
            }
            Err(e) => panic!("expected 08:00 UTC settlement, got {e}"),
        }
    }

    #[test]
    fn test_deribit_expiry_code_single_digit_day() {
        match expiry_code_to_utc("3JAN25") {
            Ok(instant) => assert_eq!(instant.to_rfc3339(), "2025-01-03T08:00:00+00:00"),
            Err(e) => panic!("expected a single-digit day to parse, got {e}"),
        }
    }

    // --- Expiry: direct UTC vs. rejected -------------------------------------

    #[test]
    fn test_deribit_normalize_uses_direct_utc_expiry() {
        // The instrument's millisecond timestamp is the authoritative direct UTC
        // instant and overrides any date-code inference.
        match normalize_leg(&sample_option()) {
            Ok(leg) => assert_eq!(leg.key.expiration_utc, utc_millis(1_751_011_200_000)),
            Err(e) => panic!("expected a normalized leg, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_rejects_unparseable_expiry() {
        // A wildly out-of-range millisecond epoch cannot resolve to an instant.
        let option = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-60000-C",
                Some(60_000.0),
                Some(DeribitOptionType::Call),
                Some(i64::MAX),
            ),
            ticker: ticker("BTC-27JUN25-60000-C", Some(0.05), Some(0.06), None, None),
        };
        match normalize_leg(&option) {
            Err(kind) => assert_eq!(kind, NormalizeKind::UnparseableExpiry),
            Ok(_) => panic!("an out-of-range expiry must reject the row"),
        }
    }

    #[test]
    fn test_deribit_parse_instrument_name_rejects_unparseable_expiry() {
        assert_eq!(
            instrument_key_from_name("BTC-99XYZ25-60000-C"),
            Err(NormalizeKind::UnparseableExpiry)
        );
    }

    // --- Row-fatal rejections -------------------------------------------------

    #[test]
    fn test_deribit_normalize_rejects_missing_strike() {
        // No typed strike and a name whose strike segment is empty -> the row
        // cannot yield a strike.
        let option = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25--C",
                None,
                Some(DeribitOptionType::Call),
                Some(1_751_011_200_000),
            ),
            ticker: ticker("BTC-27JUN25--C", Some(0.05), Some(0.06), None, None),
        };
        match normalize_leg(&option) {
            Err(kind) => assert_eq!(kind, NormalizeKind::MissingField("strike")),
            Ok(_) => panic!("a row with no strike must be rejected"),
        }
    }

    #[test]
    fn test_deribit_normalize_rejects_zero_strike() {
        let option = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-0-C",
                Some(0.0),
                Some(DeribitOptionType::Call),
                Some(1_751_011_200_000),
            ),
            ticker: ticker("BTC-27JUN25-0-C", Some(0.05), Some(0.06), None, None),
        };
        match normalize_leg(&option) {
            Err(kind) => assert_eq!(kind, NormalizeKind::OutOfRange("strike")),
            Ok(_) => panic!("a zero strike must reject the row"),
        }
    }

    #[test]
    fn test_deribit_normalize_rejects_unknown_style() {
        assert_eq!(
            instrument_key_from_name("BTC-27JUN25-60000-X"),
            Err(NormalizeKind::UnknownStyle)
        );
    }

    // --- Leg end-to-end: IV /100 reaches the leg -----------------------------

    #[test]
    fn test_deribit_normalize_leg_iv_is_decimal_fraction() {
        match normalize_leg(&sample_option()) {
            Ok(leg) => {
                assert_eq!(leg.iv, Some(pos(0.4922)));
                assert_eq!(leg.bid, Some(pos(0.05)));
                assert_eq!(leg.ask, Some(pos(0.06)));
                assert_eq!(leg.delta, Some(Decimal::new(55, 2)));
                assert_eq!(leg.key.underlying, "BTC");
                assert_eq!(leg.style, OptionStyle::Call);
            }
            Err(e) => panic!("expected a normalized leg, got {e}"),
        }
    }

    #[test]
    fn test_deribit_normalize_leg_drops_crossed_quote_keeps_row() {
        let option = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-60000-C",
                Some(60_000.0),
                Some(DeribitOptionType::Call),
                Some(1_751_011_200_000),
            ),
            // Crossed: ask (0.03) < bid (0.06).
            ticker: ticker("BTC-27JUN25-60000-C", Some(0.06), Some(0.03), None, None),
        };
        match normalize_leg(&option) {
            Ok(leg) => {
                // The crossed quote is dropped, but the row (strike/style) is kept.
                assert_eq!(leg.bid, None);
                assert_eq!(leg.ask, None);
                assert_eq!(leg.key.strike, pos(60_000.0));
            }
            Err(e) => panic!("a crossed quote drops only the quote, got {e}"),
        }
    }

    // --- Chain assembly: call + put collapse into one strike row -------------

    #[test]
    fn test_deribit_assemble_chain_merges_call_and_put_into_row() {
        let call = match normalize_leg(&sample_option()) {
            Ok(leg) => leg,
            Err(e) => panic!("call leg should normalize, got {e}"),
        };
        let put_option = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-60000-P",
                Some(60_000.0),
                Some(DeribitOptionType::Put),
                Some(1_751_011_200_000),
            ),
            ticker: ticker(
                "BTC-27JUN25-60000-P",
                Some(0.04),
                Some(0.05),
                Some(50.0),
                Some(greeks(Some(-0.45), Some(0.0001))),
            ),
        };
        let put = match normalize_leg(&put_option) {
            Ok(leg) => leg,
            Err(e) => panic!("put leg should normalize, got {e}"),
        };

        let provider = deribit_provider_id();
        let fetch = assemble_chain(
            "BTC",
            pos(60_500.0),
            utc_millis(1_751_011_200_000),
            &[call, put],
            &provider,
        );

        // One strike row carrying both sides; the alias catalog resolves both
        // native symbols back to their distinct keys.
        assert_eq!(fetch.chain.options.len(), 1);
        assert_eq!(fetch.chain.symbol, "BTC");
        assert_eq!(fetch.aliases.len(), 2);
        assert!(
            fetch
                .aliases
                .resolve_symbol("BTC-27JUN25-60000-C")
                .is_some()
        );
        assert!(
            fetch
                .aliases
                .resolve_symbol("BTC-27JUN25-60000-P")
                .is_some()
        );
        assert_eq!(fetch.expiry_source.underlying, "BTC");
        assert_eq!(
            fetch.expiry_source.expiration_utc,
            utc_millis(1_751_011_200_000)
        );

        match fetch.chain.options.iter().next() {
            Some(row) => {
                assert_eq!(row.strike_price, pos(60_000.0));
                assert_eq!(row.call_bid, Some(pos(0.05)));
                assert_eq!(row.put_bid, Some(pos(0.04)));
            }
            None => panic!("expected exactly one strike row"),
        }
    }

    #[test]
    fn test_deribit_assemble_chain_is_order_independent() {
        // The bounded-concurrency hydration delivers legs in an arbitrary order;
        // assembly groups by strike, so the assembled chain must be identical
        // regardless of the order the legs arrive in.
        let call = match normalize_leg(&sample_option()) {
            Ok(leg) => leg,
            Err(e) => panic!("call leg should normalize, got {e}"),
        };
        let put_option = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-60000-P",
                Some(60_000.0),
                Some(DeribitOptionType::Put),
                Some(1_751_011_200_000),
            ),
            ticker: ticker(
                "BTC-27JUN25-60000-P",
                Some(0.04),
                Some(0.05),
                Some(50.0),
                Some(greeks(Some(-0.45), Some(0.0001))),
            ),
        };
        let put = match normalize_leg(&put_option) {
            Ok(leg) => leg,
            Err(e) => panic!("put leg should normalize, got {e}"),
        };

        let provider = deribit_provider_id();
        let expiry = utc_millis(1_751_011_200_000);
        let forward = assemble_chain(
            "BTC",
            pos(60_500.0),
            expiry,
            &[call.clone(), put.clone()],
            &provider,
        );
        let reversed = assemble_chain("BTC", pos(60_500.0), expiry, &[put, call], &provider);

        // Same strike set, same per-strike call/put quotes, same alias catalog.
        assert_eq!(forward.chain.options.len(), reversed.chain.options.len());
        assert_eq!(forward.aliases.len(), reversed.aliases.len());
        let forward_row = forward.chain.options.iter().next();
        let reversed_row = reversed.chain.options.iter().next();
        match (forward_row, reversed_row) {
            (Some(a), Some(b)) => {
                assert_eq!(a.strike_price, b.strike_price);
                assert_eq!(a.call_bid, b.call_bid);
                assert_eq!(a.call_ask, b.call_ask);
                assert_eq!(a.put_bid, b.put_bid);
                assert_eq!(a.put_ask, b.put_ask);
            }
            _ => panic!("both orderings must yield exactly one strike row"),
        }
    }

    // --- Outage vs. empty expiry: a total ticker failure is not "no options" --

    /// A normalized call leg (via `sample_option`) for the outcome tests.
    #[track_caller]
    fn sample_leg() -> NormalizedLeg {
        match normalize_leg(&sample_option()) {
            Ok(leg) => leg,
            Err(e) => panic!("sample option should normalize, got {e}"),
        }
    }

    #[test]
    fn test_deribit_collect_outcomes_partial_keeps_hydrated_legs() {
        // Some tickers hydrate, some fail transport, some drop -> the partial set
        // is kept and the transport failures are counted, never erased.
        let hydration = collect_outcomes(vec![
            LegOutcome::Hydrated(Box::new(sample_leg())),
            LegOutcome::TransportFailed,
            LegOutcome::Dropped,
        ]);
        assert_eq!(hydration.legs.len(), 1);
        assert_eq!(hydration.transport_failures, 1);
    }

    #[test]
    fn test_deribit_collect_outcomes_counts_all_transport_failures() {
        // A total outage: every ticker fetch failed -> zero legs, all counted.
        let hydration = collect_outcomes(vec![
            LegOutcome::TransportFailed,
            LegOutcome::TransportFailed,
            LegOutcome::TransportFailed,
        ]);
        assert!(hydration.legs.is_empty());
        assert_eq!(hydration.transport_failures, 3);
    }

    #[test]
    fn test_deribit_all_tickers_fail_surfaces_transport_outage() {
        // Non-empty instrument list, zero legs, every ticker fetch failed at the
        // transport level -> an OUTAGE surfaced as a transport error (a
        // reconnecting/error state), never a NoChain that reads as "no options".
        let err = empty_expiry_outcome(8, 8, "BTC", utc_millis(1_751_011_200_000));
        assert!(matches!(err, ProviderError::Transport(_)));
        assert_eq!(err.to_string(), "upstream transport: transport: closed");
    }

    #[test]
    fn test_deribit_partial_hydration_is_not_an_outage() {
        // At least one leg hydrated: `empty_expiry_outcome` is never reached, but
        // even if the fetch had some transport failures, the presence of legs is a
        // partial-success chain. Assert the decision rule directly: with zero
        // failures and a non-empty list it is a genuine (non-outage) empty case.
        let leg_count = collect_outcomes(vec![
            LegOutcome::Hydrated(Box::new(sample_leg())),
            LegOutcome::TransportFailed,
        ])
        .legs
        .len();
        assert_eq!(
            leg_count, 1,
            "a partial hydration keeps its successful legs"
        );
    }

    #[test]
    fn test_deribit_empty_instrument_list_is_no_chain() {
        // No option instruments for the expiry (a real delisting / empty expiry):
        // zero selected, zero failures -> NoChain, not an outage.
        let err = empty_expiry_outcome(0, 0, "BTC", utc_millis(1_751_011_200_000));
        match err {
            ProviderError::NoChain {
                underlying,
                expiration,
            } => {
                assert_eq!(underlying, "BTC");
                assert_eq!(expiration, utc_millis(1_751_011_200_000).to_rfc3339());
            }
            other => panic!("expected NoChain for an empty instrument list, got {other:?}"),
        }
    }

    #[test]
    fn test_deribit_tickers_answered_but_unnormalizable_is_no_chain() {
        // The venue answered every ticker (no transport failure) but nothing
        // normalized -> a genuinely empty expiry, not an outage.
        let err = empty_expiry_outcome(5, 0, "ETH", utc_millis(1_751_011_200_000));
        assert!(matches!(err, ProviderError::NoChain { .. }));
    }

    // --- Transport error mapping is redaction-safe ---------------------------

    #[test]
    fn test_deribit_transport_error_maps_by_category() {
        assert!(matches!(
            transport_error(&HttpError::AuthenticationFailed(
                "secret-bearing".to_owned()
            )),
            ProviderError::Auth
        ));
        assert!(matches!(
            transport_error(&HttpError::RateLimitExceeded),
            ProviderError::RateLimited(None)
        ));
        // The redaction-safe detail never carries the upstream message.
        let rendered = transport_error(&HttpError::RequestFailed(
            "https://user:pass@example/secret".to_owned(),
        ))
        .to_string();
        assert!(!rendered.contains("example"));
        assert!(!rendered.contains("pass"));
        assert_eq!(rendered, "upstream transport: transport: http");
    }

    // --- subscribe spawns the adapter-owned reconnect loop -------------------

    #[tokio::test]
    async fn test_deribit_subscribe_spawns_cancellable_loop() {
        // `subscribe` returns a handle immediately after spawning the loop; the
        // loop task is queued but not polled on this current-thread runtime, so
        // NO socket is ever opened. Dropping the handle cancels + aborts it.
        let adapter = DeribitAdapter::new();
        let (tx, _rx) = mpsc::channel::<MarketUpdate>(4);
        let request = SubscriptionRequest::new("BTC", utc_millis(1_751_011_200_000), Vec::new());
        match adapter.subscribe(request, tx).await {
            Ok(handle) => drop(handle),
            Err(e) => panic!("subscribe should spawn the reconnect loop, got {e:?}"),
        }
    }

    #[test]
    fn test_deribit_adapter_reports_id_and_capabilities() {
        let adapter = DeribitAdapter::new();
        assert_eq!(adapter.id().as_str(), "deribit");
        assert_eq!(adapter.capabilities().chain, ChainCapability::Assemble);
    }

    #[test]
    fn test_deribit_adapter_is_send_sync() {
        assert_send_sync::<DeribitAdapter>();
    }

    // --- Property tests: normalization is total, never panics ----------------

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

        /// `normalize_quote` is total over any pair of `f64` (including NaN/Inf):
        /// it returns without panic, never yields a crossed Ok, and every present
        /// price is a valid `Positive` (so NaN/Inf never becomes one).
        #[test]
        fn prop_normalize_quote_is_total(
            bid in proptest::num::f64::ANY,
            ask in proptest::num::f64::ANY,
        ) {
            match normalize_quote(Some(bid), Some(ask)) {
                Ok(quote) => {
                    if let (Some(b), Some(a)) = (quote.bid, quote.ask) {
                        prop_assert!(a >= b);
                    }
                }
                Err(kind) => prop_assert_eq!(kind, NormalizeKind::OutOfRange("ask")),
            }
        }

        /// `normalize_iv` is total: NaN/Inf is `NonFinite`, a negative is
        /// `OutOfRange`, and any Ok is a valid (finite, non-negative) `Positive`.
        #[test]
        fn prop_normalize_iv_is_total(raw in proptest::num::f64::ANY) {
            match normalize_iv(raw) {
                Ok(iv) => prop_assert!(iv >= Positive::ZERO),
                Err(kind) => prop_assert!(
                    kind == NormalizeKind::NonFinite("iv")
                        || kind == NormalizeKind::OutOfRange("iv")
                ),
            }
        }

        /// `parse_instrument_name` never panics over arbitrary strings — it maps
        /// to a `ParsedName` or a typed `NormalizeKind`.
        #[test]
        fn prop_parse_instrument_name_is_total(name in ".{0,24}") {
            let _ = parse_instrument_name(&name);
        }

        /// `normalize_leg` is total over generated payload shapes: it returns a
        /// leg or a typed error, never a panic; a normalized strike is always
        /// positive and a present IV is a valid `Positive`.
        #[test]
        fn prop_normalize_leg_is_total(
            name in prop_oneof![
                Just("BTC-27JUN25-60000-C".to_owned()),
                Just("ETH-3JAN25-2000-P".to_owned()),
                Just("BTC-27JUN25--C".to_owned()),
                Just("garbage".to_owned()),
                ".{0,16}",
            ],
            strike in prop_oneof![Just(None), proptest::num::f64::ANY.prop_map(Some)],
            option_type in prop_oneof![
                Just(None),
                Just(Some(DeribitOptionType::Call)),
                Just(Some(DeribitOptionType::Put)),
            ],
            expiry_ms in prop_oneof![Just(None), any::<i64>().prop_map(Some)],
            bid in prop_oneof![Just(None), proptest::num::f64::ANY.prop_map(Some)],
            ask in prop_oneof![Just(None), proptest::num::f64::ANY.prop_map(Some)],
            iv in prop_oneof![Just(None), proptest::num::f64::ANY.prop_map(Some)],
        ) {
            let option = OptionInstrument {
                instrument: deribit_instrument(&name, strike, option_type, expiry_ms),
                ticker: ticker(&name, bid, ask, iv, Some(greeks(Some(0.5), Some(0.01)))),
            };
            if let Ok(leg) = normalize_leg(&option) {
                prop_assert!(leg.key.strike > Positive::ZERO);
                if let Some(value) = leg.iv {
                    prop_assert!(value >= Positive::ZERO);
                }
            }
        }
    }

    // =====================================================================
    // Streaming overlay (#16): test constructors
    // =====================================================================

    /// The subscribed BTC-27JUN25-60000-C call leg (native symbol + deribit id).
    fn sample_instrument() -> Instrument {
        Instrument {
            key: InstrumentKey {
                underlying: "BTC".to_owned(),
                expiration_utc: utc_millis(1_751_011_200_000),
                strike: pos(60_000.0),
                style: OptionStyle::Call,
            },
            provider: deribit_provider_id(),
            native_symbol: "BTC-27JUN25-60000-C".to_owned(),
            stream_symbol: None,
            spec: ContractSpecFingerprint {
                contract_multiplier: 1,
                settlement: SettlementStyle::Cash,
                exercise: ExerciseStyle::European,
                quote_currency: "USD".to_owned(),
                venue_product_code: "BTC".to_owned(),
            },
        }
    }

    /// A distinct leg at `strike` (distinct `InstrumentKey` + native symbol), so
    /// the producer staging keys separate slots.
    fn instrument_at(strike: f64) -> Instrument {
        Instrument {
            key: InstrumentKey {
                strike: pos(strike),
                ..sample_instrument().key
            },
            native_symbol: format!("BTC-27JUN25-{strike}-C"),
            ..sample_instrument()
        }
    }

    fn greeks_payload(delta: Option<f64>, gamma: Option<f64>) -> GreeksPayload {
        GreeksPayload { delta, gamma }
    }

    fn ticker_payload(
        bid: Option<f64>,
        ask: Option<f64>,
        mark_iv: Option<f64>,
        greeks: Option<GreeksPayload>,
    ) -> TickerPayload {
        TickerPayload {
            best_bid_price: bid,
            best_ask_price: ask,
            best_bid_amount: Some(5.0),
            best_ask_amount: Some(4.0),
            last_price: Some(0.055),
            mark_iv,
            timestamp: Some(1_751_011_200_000),
            greeks,
        }
    }

    /// A `Quote` `MarketUpdate` for `(strike, bid)`, normalized through the seam.
    fn quote_update(strike: f64, bid: f64) -> MarketUpdate {
        let payload = ticker_payload(Some(bid), Some(bid + 0.1), Some(50.0), None);
        let (quote, _greeks) = normalize_ticker(&instrument_at(strike), &payload, utc_millis(0));
        MarketUpdate::Quote(quote)
    }

    /// The `(strike, bid)` of a `Quote` update, or `None` for any other variant.
    fn quote_strike_bid(update: &MarketUpdate) -> Option<(Positive, Option<Positive>)> {
        match update {
            MarketUpdate::Quote(quote) => Some((quote.instrument.key.strike, quote.bid)),
            MarketUpdate::Greeks(_)
            | MarketUpdate::Depth(_)
            | MarketUpdate::Chain(_)
            | MarketUpdate::Health(_, _) => None,
        }
    }

    /// Drain every currently-buffered update from a channel (no runtime needed —
    /// `try_recv` is non-blocking).
    fn drain_channel(rx: &mut mpsc::Receiver<MarketUpdate>) -> Vec<MarketUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    #[track_caller]
    fn assert_delay_ms(delay: Duration, expected_ms: f64) {
        let got = delay.as_secs_f64() * 1000.0;
        assert!(
            (got - expected_ms).abs() < 1.0,
            "expected ~{expected_ms}ms, got {got}ms"
        );
    }

    fn opt_f64() -> impl Strategy<Value = Option<f64>> {
        prop_oneof![Just(None), proptest::num::f64::ANY.prop_map(Some)]
    }

    // =====================================================================
    // Backoff delay: the pure, injectable-jitter kernel
    // =====================================================================

    #[test]
    fn test_deribit_backoff_attempt_zero_is_base() {
        // attempt 0 → exactly BASE (250 ms), so the loop's reset-on-success
        // restarts the ramp at BASE.
        assert_delay_ms(backoff_delay(0, 0.0), 250.0);
    }

    #[test]
    fn test_deribit_backoff_doubles_per_attempt() {
        assert_delay_ms(backoff_delay(1, 0.0), 500.0);
        assert_delay_ms(backoff_delay(2, 0.0), 1000.0);
        assert_delay_ms(backoff_delay(3, 0.0), 2000.0);
    }

    #[test]
    fn test_deribit_backoff_caps_at_max() {
        // A large attempt caps at MAX (30 s), never a runaway or a `powi` wrap.
        assert_delay_ms(backoff_delay(100, 0.0), 30_000.0);
        assert_delay_ms(backoff_delay(u32::MAX, 0.0), 30_000.0);
    }

    #[test]
    fn test_deribit_backoff_jitter_widens_range() {
        // attempt 5 → 250 * 2^5 = 8000 ms, below the 30 s cap.
        assert_delay_ms(backoff_delay(5, -0.2), 8000.0 * 0.8);
        assert_delay_ms(backoff_delay(5, 0.2), 8000.0 * 1.2);
        assert!(backoff_delay(5, -0.2) < backoff_delay(5, 0.2));
    }

    #[test]
    fn test_deribit_backoff_clamps_out_of_range_jitter() {
        // Jitter beyond ±0.2 is clamped (a hostile jitter cannot widen the delay).
        assert_eq!(backoff_delay(0, 1.0), backoff_delay(0, 0.2));
        assert_eq!(backoff_delay(0, -1.0), backoff_delay(0, -0.2));
    }

    #[test]
    fn test_deribit_backoff_never_exceeds_max_plus_jitter() {
        for attempt in 0..40u32 {
            let delay = backoff_delay(attempt, 0.2).as_secs_f64();
            assert!(
                delay <= 36.0 + 1e-6,
                "attempt {attempt} exceeded 36 s: {delay}"
            );
            let low = backoff_delay(attempt, -0.2).as_secs_f64();
            assert!(low >= 0.2 - 1e-6, "attempt {attempt} below 200 ms: {low}");
        }
    }

    // =====================================================================
    // Ticker normalization -> QuoteUpdate + GreeksRow
    // =====================================================================

    #[test]
    fn test_deribit_normalize_ticker_maps_quote_and_greeks() {
        let payload = ticker_payload(
            Some(0.05),
            Some(0.06),
            Some(49.22),
            Some(greeks_payload(Some(0.55), Some(0.0001))),
        );
        let (quote, greeks) = normalize_ticker(
            &sample_instrument(),
            &payload,
            utc_millis(1_751_011_200_000),
        );
        assert_eq!(quote.bid, Some(pos(0.05)));
        assert_eq!(quote.ask, Some(pos(0.06)));
        assert_eq!(quote.last, Some(pos(0.055)));
        assert_eq!(quote.bid_size, Some(pos(5.0)));
        assert_eq!(quote.ask_size, Some(pos(4.0)));
        assert_eq!(quote.event_time, Some(utc_millis(1_751_011_200_000)));
        // IV is percentage-form -> decimal fraction; venue delta/gamma forwarded.
        assert_eq!(greeks.iv, Some(pos(0.4922)));
        assert_eq!(greeks.delta, Some(Decimal::new(55, 2)));
        assert!(greeks.gamma.is_some());
        assert_eq!(greeks.origin, GreeksOrigin::Provider);
        assert_eq!(greeks.event_time, Some(utc_millis(1_751_011_200_000)));
    }

    #[test]
    fn test_deribit_normalize_ticker_discards_theta_vega_rho() {
        // A wire ticker carrying theta/vega/rho: they are not even deserialized,
        // and normalize_ticker always emits None for them (docs/01 §7 — OptionData
        // cannot store them, the sidecar owns them). Venue delta/gamma survive.
        use deribit_websocket::prelude::json;
        let data = json!({
            "best_bid_price": 0.05,
            "best_ask_price": 0.06,
            "mark_iv": 50.0,
            "greeks": { "delta": 0.5, "gamma": 0.001, "theta": -9.9, "vega": 8.8, "rho": 7.7 }
        });
        let payload = match TickerPayload::deserialize(&data) {
            Ok(payload) => payload,
            Err(e) => panic!("ticker payload should deserialize: {e}"),
        };
        let (_quote, greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(
            greeks.delta,
            Some(Decimal::new(5, 1)),
            "venue delta forwarded"
        );
        assert!(greeks.gamma.is_some(), "venue gamma forwarded");
        assert!(greeks.theta.is_none(), "streamed theta must be discarded");
        assert!(greeks.vega.is_none(), "streamed vega must be discarded");
        assert!(greeks.rho.is_none(), "streamed rho must be discarded");
    }

    #[test]
    fn test_deribit_normalize_ticker_crossed_quote_drops_bid_ask() {
        // ask (0.03) < bid (0.06) is crossed -> bid/ask dropped (the store keeps
        // the prior quote); non-quote fields survive.
        let payload = ticker_payload(Some(0.06), Some(0.03), None, None);
        let (quote, _greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(quote.bid, None);
        assert_eq!(quote.ask, None);
        assert_eq!(quote.last, Some(pos(0.055)));
    }

    #[test]
    fn test_deribit_normalize_ticker_missing_greeks_are_none() {
        let payload = ticker_payload(Some(0.05), Some(0.06), None, None);
        let (_quote, greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
        assert!(greeks.iv.is_none());
        assert!(greeks.delta.is_none());
        assert!(greeks.gamma.is_none());
    }

    #[test]
    fn test_deribit_ticker_payload_deserializes_from_json() {
        use deribit_websocket::prelude::json;
        let data = json!({
            "best_bid_price": 0.05,
            "best_ask_price": 0.06,
            "mark_iv": 49.22,
            "timestamp": 1_751_011_200_000i64,
            "greeks": { "delta": 0.55, "gamma": 0.0001, "theta": -1.0, "vega": 2.0, "rho": 3.0 }
        });
        match TickerPayload::deserialize(&data) {
            Ok(payload) => {
                let (quote, greeks) =
                    normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
                assert_eq!(quote.bid, Some(pos(0.05)));
                assert_eq!(greeks.iv, Some(pos(0.4922)));
                assert!(greeks.theta.is_none());
            }
            Err(e) => panic!("ticker payload should deserialize: {e}"),
        }
    }

    // =====================================================================
    // Book normalization -> DepthLadder (change_id captured)
    // =====================================================================

    #[test]
    fn test_deribit_normalize_book_captures_change_id_and_levels() {
        let payload = BookPayload {
            change_id: Some(770),
            timestamp: Some(1_751_011_200_000),
            bids: vec![
                BookLevel::Priced([60_000.0, 2.0]),
                BookLevel::Priced([59_990.0, 5.0]),
            ],
            asks: vec![BookLevel::Priced([60_010.0, 1.5])],
        };
        let ladder = normalize_book(
            &sample_instrument(),
            &payload,
            utc_millis(1_751_011_200_000),
        );
        assert_eq!(ladder.change_id, Some(770));
        assert_eq!(ladder.event_time, Some(utc_millis(1_751_011_200_000)));
        assert_eq!(ladder.bids.len(), 2);
        assert_eq!(ladder.asks.len(), 1);
        match ladder.bids.first() {
            Some(level) => {
                assert_eq!(level.price, pos(60_000.0));
                assert_eq!(level.size, pos(2.0));
            }
            None => panic!("expected the best bid at index 0"),
        }
    }

    #[test]
    fn test_deribit_normalize_book_drops_invalid_levels() {
        let payload = BookPayload {
            change_id: Some(1),
            timestamp: None,
            bids: vec![
                BookLevel::Priced([f64::NAN, 1.0]),
                BookLevel::Priced([60_000.0, 2.0]),
                BookLevel::Priced([59_000.0, -1.0]),
            ],
            asks: Vec::new(),
        };
        let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(ladder.bids.len(), 1, "NaN price and negative size dropped");
        assert_eq!(ladder.event_time, None, "no timestamp -> no event_time");
    }

    #[test]
    fn test_deribit_normalize_book_decodes_raw_action_levels() {
        // The raw-book `[action, price, amount]` encoding decodes; the action tag
        // is ignored (delta application is v0.5).
        let payload = BookPayload {
            change_id: Some(2),
            timestamp: Some(1_751_011_200_000),
            bids: vec![BookLevel::Actioned("new".to_owned(), 60_000.0, 3.0)],
            asks: vec![BookLevel::Actioned("delete".to_owned(), 60_010.0, 0.0)],
        };
        let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
        match ladder.bids.first() {
            Some(level) => assert_eq!(level.price, pos(60_000.0)),
            None => panic!("the raw `new` bid should decode"),
        }
        assert_eq!(ladder.asks.len(), 1);
    }

    #[test]
    fn test_deribit_book_payload_deserializes_both_level_encodings() {
        use deribit_websocket::prelude::json;
        let aggregated =
            json!({ "change_id": 5, "bids": [[60_000.0, 2.0]], "asks": [[60_010.0, 1.0]] });
        match BookPayload::deserialize(&aggregated) {
            Ok(payload) => {
                let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
                assert_eq!(ladder.change_id, Some(5));
                assert_eq!(ladder.bids.len(), 1);
            }
            Err(e) => panic!("aggregated `[price, amount]` book should deserialize: {e}"),
        }
        let raw = json!({ "change_id": 6, "bids": [["new", 60_000.0, 2.0]], "asks": [] });
        match BookPayload::deserialize(&raw) {
            Ok(payload) => {
                let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
                assert_eq!(ladder.change_id, Some(6));
                assert_eq!(ladder.bids.len(), 1);
            }
            Err(e) => panic!("raw `[action, price, amount]` book should deserialize: {e}"),
        }
    }

    // =====================================================================
    // Producer-side overwrite-on-full staging (docs/02 §5)
    // =====================================================================

    #[test]
    fn test_deribit_producer_staging_overwrites_on_full_channel() {
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(1);
        let mut staging = ProducerStaging::new();
        // The first send fills the cap-1 channel.
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 1.0)),
            SendState::Open
        );
        // Channel full: these stage + overwrite in place (freshest wins per kind).
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 2.0)),
            SendState::Open
        );
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 3.0)),
            SendState::Open
        );
        assert_eq!(
            staging.slots.len(),
            1,
            "one slot per instrument, overwritten"
        );
        // The already-sent value (1.0) drains first.
        let sent = drain_channel(&mut rx);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent.first()
                .and_then(quote_strike_bid)
                .and_then(|(_, bid)| bid),
            Some(pos(1.0))
        );
        // A further publish flushes the FRESHEST staged value (3.0, not 2.0).
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 4.0)),
            SendState::Open
        );
        let after = drain_channel(&mut rx);
        assert_eq!(after.len(), 1, "flush-on-space delivers the staged value");
        assert_eq!(
            after
                .first()
                .and_then(quote_strike_bid)
                .and_then(|(_, bid)| bid),
            Some(pos(3.0)),
            "the freshest staged value survived, not the intermediate 2.0"
        );
    }

    #[test]
    fn test_deribit_producer_staging_flush_delivers_freshest_after_quiet() {
        // A burst saturates the cap-1 channel and coalesces two overwrites into
        // the staging slot, then the feed goes QUIET: no further `publish`
        // arrives to flush it. The streaming loop's flush tick (here `flush`,
        // the method the tick drives) must still deliver the FRESHEST staged
        // value once capacity frees, so the latest quote is never stranded when
        // the user is watching a now-stale "latest" (Codex review, PR #74).
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(1);
        let mut staging = ProducerStaging::new();
        // Fill the cap-1 channel (1.0 sent), then stage two overwrites: freshest
        // wins per kind, so 3.0 sits in staging, 2.0 is superseded.
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 1.0)),
            SendState::Open
        );
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 2.0)),
            SendState::Open
        );
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 3.0)),
            SendState::Open
        );
        assert!(staging.has_pending(), "the burst left a value staged");
        // The feed goes quiet: the consumer drains the already-sent 1.0, freeing
        // capacity. NO further `publish` will arrive.
        let sent = drain_channel(&mut rx);
        assert_eq!(sent.len(), 1);
        // The tick-driven flush (not a next publish) delivers the freshest value.
        assert_eq!(staging.flush(&tx), SendState::Open);
        let after = drain_channel(&mut rx);
        assert_eq!(
            after.len(),
            1,
            "flush-on-tick delivers the staged value with no further publish"
        );
        assert_eq!(
            after
                .first()
                .and_then(quote_strike_bid)
                .and_then(|(_, bid)| bid),
            Some(pos(3.0)),
            "the freshest staged value (3.0) reached the channel after the feed went quiet"
        );
        assert!(
            !staging.has_pending(),
            "nothing remains staged once the tick flush drains the slot"
        );
    }

    #[test]
    fn test_deribit_producer_staging_keeps_quote_and_greeks_independently() {
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(2);
        let mut staging = ProducerStaging::new();
        // Fill both channel slots with unrelated sends.
        let _ = staging.publish(&tx, quote_update(200.0, 9.0));
        let _ = staging.publish(&tx, quote_update(201.0, 9.0));
        // Same instrument, two kinds — both stage (channel full), one slot.
        let payload = ticker_payload(
            Some(0.05),
            Some(0.06),
            Some(50.0),
            Some(greeks_payload(Some(0.5), Some(0.01))),
        );
        let (quote, greeks) = normalize_ticker(&instrument_at(100.0), &payload, utc_millis(0));
        let _ = staging.publish(&tx, MarketUpdate::Quote(quote));
        let _ = staging.publish(&tx, MarketUpdate::Greeks(greeks));
        assert_eq!(staging.slots.len(), 1, "one slot holds both kinds");
        let _ = drain_channel(&mut rx);
        // A further publish flushes BOTH staged kinds (a Greeks refresh never
        // clobbered the pending quote).
        let _ = staging.publish(&tx, quote_update(300.0, 1.0));
        let flushed = drain_channel(&mut rx);
        assert!(
            flushed.iter().any(|update| matches!(
                update,
                MarketUpdate::Quote(quote) if quote.instrument.key.strike == pos(100.0)
            )),
            "the staged quote flushed"
        );
        assert!(
            flushed
                .iter()
                .any(|update| matches!(update, MarketUpdate::Greeks(_))),
            "the staged greeks flushed"
        );
    }

    #[test]
    fn test_deribit_producer_staging_is_bounded_by_instruments() {
        // The channel is never drained, so it stays full; a sustained burst over
        // three instruments keeps the staging map O(N=3), never O(burst).
        let (tx, _rx) = mpsc::channel::<MarketUpdate>(1);
        let mut staging = ProducerStaging::new();
        let _ = staging.publish(&tx, quote_update(1.0, 1.0));
        for round in 0..200u32 {
            for strike in [1.0, 2.0, 3.0] {
                assert_eq!(
                    staging.publish(&tx, quote_update(strike, f64::from(round) + 1.0)),
                    SendState::Open
                );
            }
            assert!(
                staging.slots.len() <= 3,
                "staging is O(N=3 instruments), not O(burst): round {round}"
            );
        }
    }

    #[test]
    fn test_deribit_producer_staging_reports_closed_channel() {
        let (tx, rx) = mpsc::channel::<MarketUpdate>(4);
        drop(rx);
        let mut staging = ProducerStaging::new();
        assert_eq!(
            staging.publish(&tx, quote_update(100.0, 1.0)),
            SendState::Closed,
            "a closed consumer channel stops the loop, never a silent buffer"
        );
    }

    // =====================================================================
    // Frame routing: channel -> normalized update, unknown-symbol guard
    // =====================================================================

    #[test]
    fn test_deribit_route_message_ticker_publishes_quote_and_greeks() {
        use deribit_websocket::prelude::json;
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(8);
        let lookup = instrument_lookup(&[sample_instrument()]);
        let mut staging = ProducerStaging::new();
        let text = json!({
            "jsonrpc": "2.0",
            "method": "subscription",
            "params": {
                "channel": "ticker.BTC-27JUN25-60000-C",
                "data": { "best_bid_price": 0.05, "best_ask_price": 0.06, "mark_iv": 49.22,
                          "greeks": { "delta": 0.55, "gamma": 0.0001, "theta": -1.0 } }
            }
        })
        .to_string();
        assert_eq!(
            route_message(&text, &lookup, &mut staging, &tx),
            SendState::Open
        );
        let out = drain_channel(&mut rx);
        assert!(out.iter().any(|u| matches!(u, MarketUpdate::Quote(_))));
        assert!(
            out.iter()
                .any(|u| matches!(u, MarketUpdate::Greeks(g) if g.theta.is_none())),
            "greeks published with theta discarded"
        );
    }

    #[test]
    fn test_deribit_route_message_ticker_with_interval_suffix_still_routes() {
        // Deribit can echo a trailing interval on the ticker channel
        // (`ticker.{instrument}.{interval}`). The instrument segment must still
        // resolve — a naive `strip_prefix("ticker.")` would leave
        // `BTC-...-C.100ms`, miss the lookup, and silently drop every quote.
        use deribit_websocket::prelude::json;
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(8);
        let lookup = instrument_lookup(&[sample_instrument()]);
        let mut staging = ProducerStaging::new();
        let text = json!({
            "jsonrpc": "2.0",
            "method": "subscription",
            "params": {
                "channel": "ticker.BTC-27JUN25-60000-C.100ms",
                "data": { "best_bid_price": 0.05, "best_ask_price": 0.06 }
            }
        })
        .to_string();
        assert_eq!(
            route_message(&text, &lookup, &mut staging, &tx),
            SendState::Open
        );
        let out = drain_channel(&mut rx);
        // The quote routed to the right InstrumentKey (60000 call), not dropped.
        match out.iter().find_map(quote_strike_bid) {
            Some((strike, bid)) => {
                assert_eq!(strike, pos(60_000.0));
                assert_eq!(bid, Some(pos(0.05)));
            }
            None => panic!("a ticker frame with a trailing interval must still route"),
        }
    }

    #[test]
    fn test_deribit_route_message_book_publishes_depth_with_change_id() {
        use deribit_websocket::prelude::json;
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(8);
        let lookup = instrument_lookup(&[sample_instrument()]);
        let mut staging = ProducerStaging::new();
        let text = json!({
            "jsonrpc": "2.0",
            "method": "subscription",
            "params": {
                "channel": "book.BTC-27JUN25-60000-C.raw",
                "data": { "change_id": 770, "bids": [[60_000.0, 2.0]], "asks": [[60_010.0, 1.0]] }
            }
        })
        .to_string();
        assert_eq!(
            route_message(&text, &lookup, &mut staging, &tx),
            SendState::Open
        );
        match drain_channel(&mut rx).first() {
            Some(MarketUpdate::Depth(ladder)) => assert_eq!(ladder.change_id, Some(770)),
            other => panic!("expected a Depth update with change_id, got {other:?}"),
        }
    }

    #[test]
    fn test_deribit_route_message_unknown_symbol_is_dropped() {
        use deribit_websocket::prelude::json;
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(8);
        // The lookup knows only 60000-C; a frame for 99999-C is dropped.
        let lookup = instrument_lookup(&[sample_instrument()]);
        let mut staging = ProducerStaging::new();
        let text = json!({
            "jsonrpc": "2.0",
            "method": "subscription",
            "params": {
                "channel": "ticker.BTC-27JUN25-99999-C",
                "data": { "best_bid_price": 0.05, "best_ask_price": 0.06 }
            }
        })
        .to_string();
        assert_eq!(
            route_message(&text, &lookup, &mut staging, &tx),
            SendState::Open
        );
        assert!(
            drain_channel(&mut rx).is_empty(),
            "an update for an unsubscribed symbol is dropped, never keyed blindly"
        );
    }

    #[test]
    fn test_deribit_route_message_ignores_non_subscription_and_malformed() {
        use deribit_websocket::prelude::json;
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(8);
        let lookup = instrument_lookup(&[sample_instrument()]);
        let mut staging = ProducerStaging::new();
        // A non-subscription notification is ignored.
        let heartbeat =
            json!({ "jsonrpc": "2.0", "method": "heartbeat", "params": {} }).to_string();
        assert_eq!(
            route_message(&heartbeat, &lookup, &mut staging, &tx),
            SendState::Open
        );
        // Malformed JSON never panics.
        assert_eq!(
            route_message("{ not json", &lookup, &mut staging, &tx),
            SendState::Open
        );
        assert!(drain_channel(&mut rx).is_empty());
    }

    // =====================================================================
    // Subscription channels + reconnect backfill snapshot
    // =====================================================================

    #[test]
    fn test_deribit_subscription_channels_ticker_and_book_never_trades() {
        let channels = subscription_channels(&[sample_instrument()]);
        assert!(channels.contains(&"ticker.BTC-27JUN25-60000-C".to_owned()));
        assert!(
            channels
                .iter()
                .any(|channel| channel.starts_with("book.BTC-27JUN25-60000-C")),
            "the book channel is subscribed"
        );
        assert!(
            !channels
                .iter()
                .any(|channel| channel.starts_with("trades.")),
            "the trades tape is deferred, never subscribed"
        );
        assert_eq!(channels.len(), 2, "exactly ticker + book per leg");
    }

    #[test]
    fn test_deribit_chain_snapshot_from_fetch_is_merged_live_and_carries_aliases() {
        let call = match normalize_leg(&sample_option()) {
            Ok(leg) => leg,
            Err(e) => panic!("call leg should normalize, got {e}"),
        };
        let fetch = assemble_chain(
            "BTC",
            pos(60_500.0),
            utc_millis(1_751_011_200_000),
            &[call],
            &deribit_provider_id(),
        );
        let snapshot = chain_snapshot(&fetch, utc_millis(1_751_011_200_001));
        assert_eq!(snapshot.source, ChainSource::Merged);
        assert!(matches!(snapshot.health, StreamHealth::Live));
        assert_eq!(snapshot.chain_key.1, "BTC");
        assert_eq!(snapshot.chain_key.2, utc_millis(1_751_011_200_000));
        assert_eq!(snapshot.last_full_poll, Some(utc_millis(1_751_011_200_001)));
        // The fresh alias catalog rides forward with no re-derivation.
        assert!(
            snapshot
                .aliases
                .resolve_symbol("BTC-27JUN25-60000-C")
                .is_some()
        );
    }

    // =====================================================================
    // Property: streaming normalization is total (never panics)
    // =====================================================================

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// The backoff kernel is total and bounded for ANY attempt/jitter — no
        /// panic (`from_secs_f64` never sees a NaN/negative), never above
        /// `MAX * 1.2` (36 s) nor below `BASE * 0.8` (200 ms).
        #[test]
        fn prop_deribit_backoff_is_bounded(attempt in 0u32..64, jitter in -5.0f64..5.0) {
            let delay = backoff_delay(attempt, jitter).as_secs_f64();
            prop_assert!(delay >= 0.2 - 1e-9);
            prop_assert!(delay <= 36.0 + 1e-9);
        }

        /// `normalize_ticker` is total over arbitrary numeric fields: no panic,
        /// any present quote is never crossed, a present IV is a valid `Positive`,
        /// and theta/vega/rho are always discarded.
        #[test]
        fn prop_deribit_normalize_ticker_is_total(
            bid in opt_f64(),
            ask in opt_f64(),
            last in opt_f64(),
            bid_amt in opt_f64(),
            ask_amt in opt_f64(),
            iv in opt_f64(),
            delta in opt_f64(),
            gamma in opt_f64(),
            ts in prop_oneof![Just(None), any::<i64>().prop_map(Some)],
        ) {
            let payload = TickerPayload {
                best_bid_price: bid,
                best_ask_price: ask,
                best_bid_amount: bid_amt,
                best_ask_amount: ask_amt,
                last_price: last,
                mark_iv: iv,
                timestamp: ts,
                greeks: Some(GreeksPayload { delta, gamma }),
            };
            let (quote, greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
            if let (Some(b), Some(a)) = (quote.bid, quote.ask) {
                prop_assert!(a >= b);
            }
            if let Some(value) = greeks.iv {
                prop_assert!(value >= Positive::ZERO);
            }
            // theta/vega/rho are never sourced from the stream.
            prop_assert!(greeks.theta.is_none());
            prop_assert!(greeks.vega.is_none());
            prop_assert!(greeks.rho.is_none());
        }

        /// `normalize_book` is total over arbitrary level shapes: no panic, every
        /// surviving level's price/size is a valid `Positive`, and the `change_id`
        /// is carried through verbatim.
        #[test]
        fn prop_deribit_normalize_book_is_total(
            levels in proptest::collection::vec(
                (proptest::num::f64::ANY, proptest::num::f64::ANY),
                0..8,
            ),
            change_id in prop_oneof![Just(None), any::<u64>().prop_map(Some)],
        ) {
            let payload = BookPayload {
                change_id,
                timestamp: None,
                bids: levels.iter().map(|(p, a)| BookLevel::Priced([*p, *a])).collect(),
                asks: Vec::new(),
            };
            let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
            prop_assert_eq!(ladder.change_id, change_id);
            for level in &ladder.bids {
                prop_assert!(level.price >= Positive::ZERO);
                prop_assert!(level.size >= Positive::ZERO);
            }
        }
    }

    // =====================================================================
    // Recorded fixtures (issue #17): constructed-to-wire-shape at the pinned
    // deribit-http 0.7.1 / deribit-websocket 0.3.1 DTOs. `include_str!` bakes
    // the bytes into the test binary, so the fixtures are byte-stable across
    // machines (docs/TESTING.md §5).
    // =====================================================================

    const FIXTURE_INSTRUMENTS_BTC: &str =
        include_str!("../../tests/fixtures/deribit/instruments/instruments_btc.json");
    const FIXTURE_INSTRUMENTS_MISSING_STRIKE: &str =
        include_str!("../../tests/fixtures/deribit/instruments/instruments_missing_strike.json");
    const FIXTURE_TICKER_NORMAL: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_normal.json");
    const FIXTURE_TICKER_ZERO_BID: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_zero_bid.json");
    const FIXTURE_TICKER_CROSSED: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_crossed.json");
    const FIXTURE_TICKER_NEGATIVE: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_negative.json");
    const FIXTURE_TICKER_NON_FINITE: &str =
        include_str!("../../tests/fixtures/deribit/ticker/ticker_non_finite.json");
    const FIXTURE_BOOK_SNAPSHOT: &str =
        include_str!("../../tests/fixtures/deribit/book/book_snapshot.json");
    const FIXTURE_BOOK_DELTA: &str =
        include_str!("../../tests/fixtures/deribit/book/book_delta.json");

    /// Parse a fixture string into a `serde_json::Value` (reached through the
    /// upstream prelude re-export, so no new dependency). Every downstream DTO is
    /// deserialized from this `Value`.
    #[track_caller]
    fn fixture_value(json: &str) -> Value {
        match json.parse::<Value>() {
            Ok(value) => value,
            Err(e) => panic!("fixture JSON should parse: {e}"),
        }
    }

    /// Deserialize a `get_instruments` fixture into the upstream instrument list.
    #[track_caller]
    fn instruments_fixture(json: &str) -> Vec<DeribitInstrument> {
        let value = fixture_value(json);
        match Vec::<DeribitInstrument>::deserialize(&value) {
            Ok(instruments) => instruments,
            Err(e) => panic!("instruments fixture should deserialize: {e}"),
        }
    }

    /// Deserialize a ticker fixture into the REST `TickerData` (the poll leg).
    #[track_caller]
    fn ticker_data_fixture(json: &str) -> TickerData {
        let value = fixture_value(json);
        match TickerData::deserialize(&value) {
            Ok(ticker) => ticker,
            Err(e) => panic!("ticker fixture should deserialize as TickerData: {e}"),
        }
    }

    /// Deserialize a ticker fixture into the WS `TickerPayload` (the overlay leg).
    #[track_caller]
    fn ticker_payload_fixture(json: &str) -> TickerPayload {
        let value = fixture_value(json);
        match TickerPayload::deserialize(&value) {
            Ok(payload) => payload,
            Err(e) => panic!("ticker fixture should deserialize as TickerPayload: {e}"),
        }
    }

    /// Deserialize a book fixture into the WS `BookPayload`.
    #[track_caller]
    fn book_payload_fixture(json: &str) -> BookPayload {
        let value = fixture_value(json);
        match BookPayload::deserialize(&value) {
            Ok(payload) => payload,
            Err(e) => panic!("book fixture should deserialize as BookPayload: {e}"),
        }
    }

    /// Wrap a raw `data` JSON in a `ticker.{symbol}` subscription envelope — the
    /// exact frame `route_message` (and the reconnect loop) decode.
    fn ticker_frame(symbol: &str, data_json: &str) -> String {
        subscription_frame(&format!("ticker.{symbol}"), data_json)
    }

    /// Wrap a raw `data` JSON in a JSON-RPC subscription notification for
    /// `channel`.
    fn subscription_frame(channel: &str, data_json: &str) -> String {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"subscription\",\
             \"params\":{{\"channel\":\"{channel}\",\"data\":{data_json}}}}}"
        )
    }

    // --- Fixture -> OptionChain (the REST assembly path) ---------------------

    #[test]
    fn test_deribit_fixture_instruments_assemble_to_option_chain() {
        let instruments = instruments_fixture(FIXTURE_INSTRUMENTS_BTC);
        // The perpetual future is filtered out; only the three options remain.
        let options: Vec<DeribitInstrument> = instruments
            .iter()
            .filter(|i| i.is_option())
            .cloned()
            .collect();
        assert_eq!(options.len(), 3, "the perpetual future is filtered out");

        // Pair each option with the recorded ticker and normalize into legs.
        let ticker = ticker_data_fixture(FIXTURE_TICKER_NORMAL);
        let legs: Vec<NormalizedLeg> = options
            .into_iter()
            .filter_map(|instrument| {
                normalize_leg(&OptionInstrument {
                    instrument,
                    ticker: ticker.clone(),
                })
                .ok()
            })
            .collect();
        assert_eq!(legs.len(), 3, "every option leg normalizes");

        let fetch = assemble_chain(
            "BTC",
            pos(60_500.0),
            utc_millis(1_751_011_200_000),
            &legs,
            &deribit_provider_id(),
        );

        // Two strikes: 60000 (call + put collapse into one row) and 61000 (call).
        assert_eq!(fetch.chain.symbol, "BTC");
        assert_eq!(fetch.chain.options.len(), 2);
        assert_eq!(
            fetch.aliases.len(),
            3,
            "three native aliases, one per option"
        );
        assert!(
            fetch
                .aliases
                .resolve_symbol("BTC-27JUN25-60000-P")
                .is_some()
        );

        match fetch
            .chain
            .options
            .iter()
            .find(|row| row.strike_price == pos(60_000.0))
        {
            Some(row) => {
                assert_eq!(row.call_bid, Some(pos(0.05)));
                assert_eq!(row.call_ask, Some(pos(0.06)));
                assert_eq!(row.put_bid, Some(pos(0.05)));
                assert_eq!(row.put_ask, Some(pos(0.06)));
            }
            None => panic!("expected the 60000 strike row"),
        }

        // The percentage-form IV reached the leg as a decimal fraction (49.22 %).
        match legs.iter().find(|leg| leg.style == OptionStyle::Call) {
            Some(leg) => {
                assert_eq!(leg.iv, Some(pos(0.4922)));
                assert_eq!(leg.delta, Some(Decimal::new(55, 2)));
                assert_eq!(leg.underlying_price, Some(pos(60_500.0)));
            }
            None => panic!("expected a normalized call leg"),
        }
    }

    #[test]
    fn test_deribit_fixture_missing_strike_and_style_reject_rows() {
        // A degraded instruments fixture: one name with no strike segment, one
        // with an unknown style. Each rejects the ROW as a typed Normalize error
        // — never a fabricated strike/style.
        let instruments = instruments_fixture(FIXTURE_INSTRUMENTS_MISSING_STRIKE);
        let ticker = ticker_data_fixture(FIXTURE_TICKER_NORMAL);
        let mut kinds = Vec::new();
        for instrument in instruments {
            let option = OptionInstrument {
                instrument,
                ticker: ticker.clone(),
            };
            match normalize_leg(&option) {
                Err(kind) => kinds.push(kind),
                Ok(_) => panic!("a missing-strike / unknown-style row must be rejected"),
            }
        }
        assert!(
            kinds.contains(&NormalizeKind::MissingField("strike")),
            "the empty strike segment is a missing-field reject"
        );
        assert!(
            kinds.contains(&NormalizeKind::UnknownStyle),
            "the `-X` style is an unknown-style reject"
        );
    }

    // --- Fixture -> QuoteUpdate + GreeksRow (the WS overlay path) -------------

    #[test]
    fn test_deribit_fixture_ticker_normal_normalizes_quote_and_greeks() {
        let payload = ticker_payload_fixture(FIXTURE_TICKER_NORMAL);
        let (quote, greeks) = normalize_ticker(
            &sample_instrument(),
            &payload,
            utc_millis(1_751_011_200_000),
        );
        assert_eq!(quote.bid, Some(pos(0.05)));
        assert_eq!(quote.ask, Some(pos(0.06)));
        assert_eq!(quote.last, Some(pos(0.055)));
        assert_eq!(quote.bid_size, Some(pos(5.0)));
        assert_eq!(quote.ask_size, Some(pos(4.0)));
        assert_eq!(quote.event_time, Some(utc_millis(1_751_011_200_000)));
        // IV is percentage-form -> decimal fraction; venue delta/gamma forwarded.
        assert_eq!(greeks.iv, Some(pos(0.4922)));
        assert_eq!(greeks.delta, Some(Decimal::new(55, 2)));
        assert!(greeks.gamma.is_some());
        // The fixture carries theta/vega/rho; they are deliberately discarded.
        assert!(greeks.theta.is_none());
        assert!(greeks.vega.is_none());
        assert!(greeks.rho.is_none());
    }

    #[test]
    fn test_deribit_fixture_ticker_zero_bid_keeps_quote() {
        // A zero bid is valid — the midpoint is still derivable, so it is kept.
        let payload = ticker_payload_fixture(FIXTURE_TICKER_ZERO_BID);
        let (quote, _greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(quote.bid, Some(Positive::ZERO));
        assert_eq!(quote.ask, Some(pos(0.06)));
    }

    #[test]
    fn test_deribit_fixture_ticker_crossed_drops_bid_ask() {
        // ask (0.03) < bid (0.06): crossed -> bid/ask dropped (store keeps prior),
        // non-quote fields survive.
        let payload = ticker_payload_fixture(FIXTURE_TICKER_CROSSED);
        let (quote, _greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(quote.bid, None);
        assert_eq!(quote.ask, None);
        assert_eq!(
            quote.last,
            Some(pos(0.055)),
            "last survives a crossed quote"
        );
    }

    #[test]
    fn test_deribit_fixture_ticker_negative_drops_bid_keeps_ask() {
        // A negative bid drops ONLY that field; the ask survives.
        let payload = ticker_payload_fixture(FIXTURE_TICKER_NEGATIVE);
        let (quote, _greeks) = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(quote.bid, None);
        assert_eq!(quote.ask, Some(pos(0.06)));
    }

    #[test]
    fn test_deribit_fixture_ticker_non_finite_refuses_frame() {
        // JSON has no NaN/Inf literal, so a real degraded frame delivers a
        // non-finite price as a non-numeric field. That fails the whole
        // `TickerPayload` deserialize, so the router drops the frame — no
        // fabricated value reaches the chain (the adapter reports the gap).
        let value = fixture_value(FIXTURE_TICKER_NON_FINITE);
        assert!(
            TickerPayload::deserialize(&value).is_err(),
            "a non-numeric price field refuses the whole frame"
        );
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(4);
        let lookup = instrument_lookup(&[sample_instrument()]);
        let mut staging = ProducerStaging::new();
        let frame = ticker_frame("BTC-27JUN25-60000-C", FIXTURE_TICKER_NON_FINITE);
        assert_eq!(
            route_message(&frame, &lookup, &mut staging, &tx),
            SendState::Open,
            "a degraded frame is skipped, never a panic"
        );
        assert!(
            drain_channel(&mut rx).is_empty(),
            "the degraded frame produces no update, never a fabricated value"
        );
    }

    // --- Fixture -> DepthLadder (the WS book path) ---------------------------

    #[test]
    fn test_deribit_fixture_book_snapshot_normalizes_ladder() {
        let payload = book_payload_fixture(FIXTURE_BOOK_SNAPSHOT);
        let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(ladder.change_id, Some(1_419_600_001));
        assert_eq!(ladder.event_time, Some(utc_millis(1_751_011_200_000)));
        assert_eq!(
            ladder.bids.len(),
            3,
            "raw-book `new` bids decode best-first"
        );
        assert_eq!(ladder.asks.len(), 2);
        match ladder.bids.first() {
            Some(level) => {
                assert_eq!(level.price, pos(0.05));
                assert_eq!(level.size, pos(5.0));
            }
            None => panic!("expected the best bid at index 0"),
        }
    }

    #[test]
    fn test_deribit_fixture_book_delta_captures_change_and_delete() {
        let payload = book_payload_fixture(FIXTURE_BOOK_DELTA);
        let ladder = normalize_book(&sample_instrument(), &payload, utc_millis(0));
        assert_eq!(ladder.change_id, Some(1_419_600_002));
        assert_eq!(ladder.bids.len(), 2, "both change and delete levels decode");
        // The delete level keeps its price with size 0 (delta application is v0.5).
        match ladder.bids.iter().find(|level| level.price == pos(0.049)) {
            Some(level) => assert_eq!(level.size, Positive::ZERO),
            None => panic!("the delete level should decode with size 0"),
        }
        assert_eq!(ladder.asks.len(), 1);
    }

    #[test]
    fn test_deribit_fixture_corpus_normalizes_without_panic() {
        // Every committed fixture is a normalize_total / normalize_rejects_unknown
        // corpus seed (docs/TESTING.md §3): each drives its normalizer to a valid
        // update or a typed reject, never a panic. This complements the property
        // tests above by pinning the recorded shapes as concrete inputs.
        for json in [
            FIXTURE_TICKER_NORMAL,
            FIXTURE_TICKER_ZERO_BID,
            FIXTURE_TICKER_CROSSED,
            FIXTURE_TICKER_NEGATIVE,
            FIXTURE_TICKER_NON_FINITE,
        ] {
            let value = fixture_value(json);
            // The non-finite fixture fails deserialization (by design); every
            // other normalizes without panic.
            if let Ok(payload) = TickerPayload::deserialize(&value) {
                let _ = normalize_ticker(&sample_instrument(), &payload, utc_millis(0));
            }
        }
        for json in [FIXTURE_BOOK_SNAPSHOT, FIXTURE_BOOK_DELTA] {
            let payload = book_payload_fixture(json);
            let _ = normalize_book(&sample_instrument(), &payload, utc_millis(0));
        }
    }

    // =====================================================================
    // Mock-transport lifecycle tests (issue #17): the reconnect loop driven
    // through the `DeribitTransport` seam with NO real socket and NO wall
    // clock. `start_paused` auto-advances the virtual timer whenever the
    // runtime is otherwise idle, so a jittered backoff `sleep` completes
    // instantly and every drain is bounded by a virtual `timeout`
    // (docs/TESTING.md §5, §7, §9).
    // =====================================================================

    /// A scripted frame the mock transport yields from [`DeribitTransport::receive`].
    enum MockFrame {
        /// A raw notification frame to route.
        Text(String),
        /// `receive` returns `Err(TransportGone)` — a socket close / stream error.
        Drop,
    }

    /// Test-observable state of the mock transport. The test holds an `Arc` clone
    /// to inspect the reconnect behaviour after the loop runs.
    #[derive(Default)]
    struct MockState {
        connect_calls: usize,
        refetch_calls: usize,
        channel_sets: Vec<Vec<String>>,
    }

    /// A mock [`DeribitTransport`]: `receive` yields scripted frames (or a drop)
    /// from a channel the test feeds, `connect_and_subscribe`/`refetch` record
    /// their calls so a resubscribe/backfill is observable, and `refetch` returns
    /// a fixed [`ChainFetch`] so the reconnect backfill runs with no REST. No
    /// socket, no clock.
    struct MockTransport {
        state: Arc<StdMutex<MockState>>,
        frames: mpsc::UnboundedReceiver<MockFrame>,
        canned_fetch: ChainFetch,
    }

    #[async_trait]
    impl DeribitTransport for MockTransport {
        async fn connect_and_subscribe(
            &mut self,
            channels: Vec<String>,
        ) -> Result<(), TransportGone> {
            if let Ok(mut state) = self.state.lock() {
                state.connect_calls += 1;
                state.channel_sets.push(channels);
            }
            Ok(())
        }

        async fn receive(&mut self) -> Result<String, TransportGone> {
            match self.frames.recv().await {
                Some(MockFrame::Text(text)) => Ok(text),
                Some(MockFrame::Drop) => Err(TransportGone),
                // The script is exhausted: park so the loop waits on cancellation
                // (a quiet-but-open stream), never busy-loops.
                None => std::future::pending().await,
            }
        }

        async fn refetch(
            &mut self,
            _underlying: &str,
            _expiration: &ExpirationDate,
        ) -> Option<ChainFetch> {
            if let Ok(mut state) = self.state.lock() {
                state.refetch_calls += 1;
            }
            Some(self.canned_fetch.clone())
        }
    }

    /// The canned reconnect backfill: a two-leg chain (60000-C + 61000-C) so a
    /// resubscribe off the FRESH aliases is observable (the initial subscription
    /// is only 60000-C).
    fn canned_reconnect_fetch() -> ChainFetch {
        let call_60k = match normalize_leg(&sample_option()) {
            Ok(leg) => leg,
            Err(e) => panic!("the 60000 call should normalize: {e}"),
        };
        let option_61k = OptionInstrument {
            instrument: deribit_instrument(
                "BTC-27JUN25-61000-C",
                Some(61_000.0),
                Some(DeribitOptionType::Call),
                Some(1_751_011_200_000),
            ),
            ticker: ticker(
                "BTC-27JUN25-61000-C",
                Some(0.03),
                Some(0.04),
                Some(45.0),
                Some(greeks(Some(0.4), Some(0.0001))),
            ),
        };
        let call_61k = match normalize_leg(&option_61k) {
            Ok(leg) => leg,
            Err(e) => panic!("the 61000 call should normalize: {e}"),
        };
        assemble_chain(
            "BTC",
            pos(60_500.0),
            utc_millis(1_751_011_200_000),
            &[call_60k, call_61k],
            &deribit_provider_id(),
        )
    }

    /// Build a mock transport and return it with a shared handle onto its state.
    fn mock_transport() -> (
        MockTransport,
        mpsc::UnboundedSender<MockFrame>,
        Arc<StdMutex<MockState>>,
    ) {
        let (script_tx, script_rx) = mpsc::unbounded_channel::<MockFrame>();
        let state = Arc::new(StdMutex::new(MockState::default()));
        let transport = MockTransport {
            state: Arc::clone(&state),
            frames: script_rx,
            canned_fetch: canned_reconnect_fetch(),
        };
        (transport, script_tx, state)
    }

    /// Spawn the reconnect loop over a mock transport, scoped to the single
    /// 60000-C leg. Returns the join handle and the loop's cancel token.
    fn spawn_loop(
        transport: MockTransport,
        tx: mpsc::Sender<MarketUpdate>,
    ) -> (tokio::task::JoinHandle<()>, CancellationToken) {
        let cancel = CancellationToken::new();
        let join = tokio::spawn(run_reconnect_loop(
            transport,
            deribit_provider_id(),
            "BTC".to_owned(),
            utc_millis(1_751_011_200_000),
            vec![sample_instrument()],
            tx,
            cancel.clone(),
        ));
        (join, cancel)
    }

    /// Drain updates until `stop` returns true or nothing more arrives within a
    /// virtual 5 s window. Under `start_paused` the window is instant unless a
    /// real update is pending, so a genuinely-absent update never hangs the test.
    async fn drain_until<F>(rx: &mut mpsc::Receiver<MarketUpdate>, mut stop: F) -> Vec<MarketUpdate>
    where
        F: FnMut(&MarketUpdate) -> bool,
    {
        let mut collected = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(update)) => {
                    let done = stop(&update);
                    collected.push(update);
                    if done {
                        return collected;
                    }
                }
                Ok(None) | Err(_) => return collected,
            }
        }
    }

    /// Await a loop's clean stop within a virtual window (never a wall-clock wait).
    async fn loop_stopped(join: tokio::task::JoinHandle<()>) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), join).await,
            Ok(Ok(()))
        )
    }

    fn is_live(update: &MarketUpdate) -> bool {
        matches!(update, MarketUpdate::Health(_, StreamHealth::Live))
    }

    fn is_reconnecting(update: &MarketUpdate) -> bool {
        matches!(
            update,
            MarketUpdate::Health(_, StreamHealth::Reconnecting { .. })
        )
    }

    fn is_chain(update: &MarketUpdate) -> bool {
        matches!(update, MarketUpdate::Chain(_))
    }

    fn reconnect_attempt(update: &MarketUpdate) -> Option<u32> {
        match update {
            MarketUpdate::Health(_, StreamHealth::Reconnecting { attempt }) => Some(*attempt),
            MarketUpdate::Quote(_)
            | MarketUpdate::Greeks(_)
            | MarketUpdate::Depth(_)
            | MarketUpdate::Chain(_)
            | MarketUpdate::Health(_, StreamHealth::Live | StreamHealth::Stale { .. }) => None,
        }
    }

    /// A varying ticker `data` JSON for a saturation/lag burst (a fresh bid each
    /// round), shaped as a `ticker.` `TickerPayload`.
    fn burst_ticker_json(round: u32) -> String {
        let bid = 0.01 + f64::from(round) * 0.001;
        let ask = bid + 0.01;
        format!(
            "{{\"best_bid_price\":{bid},\"best_ask_price\":{ask},\
             \"best_bid_amount\":5.0,\"best_ask_amount\":4.0,\"mark_iv\":50.0}}"
        )
    }

    // --- (a) socket close -> Health(Reconnecting) + backoff ------------------

    #[tokio::test(start_paused = true)]
    async fn test_deribit_lifecycle_socket_close_emits_reconnecting() {
        let (transport, script_tx, _state) = mock_transport();
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(16);
        let (join, cancel) = spawn_loop(transport, tx);

        // A live frame, then the socket closes.
        let _ = script_tx.send(MockFrame::Text(ticker_frame(
            "BTC-27JUN25-60000-C",
            FIXTURE_TICKER_NORMAL,
        )));
        let _ = script_tx.send(MockFrame::Drop);

        let updates = drain_until(&mut rx, is_reconnecting).await;
        assert!(
            updates.iter().any(is_live),
            "the stream first surfaced Live"
        );
        assert!(
            updates.iter().any(|u| matches!(u, MarketUpdate::Quote(_))),
            "the live frame produced a quote before the drop"
        );
        match updates.last().and_then(reconnect_attempt) {
            Some(attempt) => assert_eq!(attempt, 1, "the first drop surfaces Reconnecting{{1}}"),
            None => panic!("a socket close must surface Health(Reconnecting)"),
        }

        cancel.cancel();
        assert!(loop_stopped(join).await, "the cancelled loop stops");
    }

    // --- (b) stream error -> reconnects without panic ------------------------

    #[tokio::test(start_paused = true)]
    async fn test_deribit_lifecycle_stream_error_reconnects_without_panic() {
        let (transport, script_tx, state) = mock_transport();
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(16);
        let (join, cancel) = spawn_loop(transport, tx);

        // A stream error immediately after connect (no data flowed).
        let _ = script_tx.send(MockFrame::Drop);

        // Drain to the SECOND Live — proving the loop recovered after the error.
        let mut lives = 0;
        let updates = drain_until(&mut rx, |u| {
            if is_live(u) {
                lives += 1;
            }
            lives >= 2
        })
        .await;
        assert!(
            updates.iter().any(is_reconnecting),
            "the stream error surfaced Reconnecting, no panic"
        );
        assert_eq!(
            updates.iter().filter(|u| is_live(u)).count(),
            2,
            "the loop reconnected to Live after the error"
        );
        let refetches = state.lock().map(|s| s.refetch_calls).unwrap_or(0);
        assert!(
            refetches >= 1,
            "the reconnect re-fetched the chain (backfill)"
        );

        cancel.cancel();
        assert!(loop_stopped(join).await);
    }

    // --- (c) resubscribe -> re-fetch + fresh subscription set + reset --------

    #[tokio::test(start_paused = true)]
    async fn test_deribit_lifecycle_reconnect_refetches_and_resubscribes_fresh() {
        let (transport, script_tx, state) = mock_transport();
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(32);
        let (join, cancel) = spawn_loop(transport, tx);

        // Two drops (each after a Live), so we observe two reconnects.
        let _ = script_tx.send(MockFrame::Drop);
        let _ = script_tx.send(MockFrame::Drop);

        let mut reconnects = 0;
        let updates = drain_until(&mut rx, |u| {
            if is_reconnecting(u) {
                reconnects += 1;
            }
            reconnects >= 2
        })
        .await;

        // Both reconnects surface attempt == 1: the backoff ramp resets on each
        // successful resubscribe (the loop-level reset #16 deferred to #17).
        let attempts: Vec<u32> = updates.iter().filter_map(reconnect_attempt).collect();
        assert_eq!(
            attempts,
            vec![1, 1],
            "attempt resets to 1 after each successful resubscribe"
        );
        assert!(
            updates.iter().any(is_chain),
            "the reconnect backfill emitted a Chain snapshot"
        );

        let (connects, refetches, sets) = match state.lock() {
            Ok(guard) => (
                guard.connect_calls,
                guard.refetch_calls,
                guard.channel_sets.clone(),
            ),
            Err(_) => panic!("mock state lock poisoned"),
        };
        assert!(connects >= 2, "the loop reconnected");
        assert!(refetches >= 1, "the reconnect re-issued fetch_chain");

        // The first subscribe is the single initial leg (ticker + book); after the
        // re-fetch, a resubscribe picks up the FRESH 61000-C leg from the aliases.
        match sets.first() {
            Some(first) => assert_eq!(first.len(), 2, "initial subscribe = ticker + book"),
            None => panic!("expected an initial subscribe set"),
        }
        assert!(
            sets.iter()
                .skip(1)
                .any(|set| set.iter().any(|c| c == "ticker.BTC-27JUN25-61000-C")),
            "a resubscribe used the fresh 61000-C alias from the re-fetch"
        );

        cancel.cancel();
        assert!(loop_stopped(join).await);
    }

    // --- (d) bounded-bridge saturation -> flat memory ------------------------

    #[test]
    fn test_deribit_lifecycle_saturation_coalesces_with_flat_memory() {
        // The bounded bridge the reconnect loop runs per frame (route_message ->
        // ProducerStaging). A burst far beyond the cap-1 channel capacity, over
        // three instruments, with the consumer NEVER draining, keeps the staging
        // map O(N = 3 instruments) — never O(burst): last-value-wins per
        // instrument without unbounded growth (docs/02 §5, NFR-15).
        let (tx, _rx) = mpsc::channel::<MarketUpdate>(1);
        let instruments = [
            instrument_at(60_000.0),
            instrument_at(61_000.0),
            instrument_at(62_000.0),
        ];
        let lookup = instrument_lookup(&instruments);
        let mut staging = ProducerStaging::new();
        for round in 0..500u32 {
            for strike in [60_000.0, 61_000.0, 62_000.0] {
                let symbol = format!("BTC-27JUN25-{strike}-C");
                let frame = ticker_frame(&symbol, &burst_ticker_json(round));
                assert_eq!(
                    route_message(&frame, &lookup, &mut staging, &tx),
                    SendState::Open,
                    "a saturated bridge never drops the loop"
                );
            }
            assert!(
                staging.slots.len() <= 3,
                "staging stays O(N = 3 instruments), not O(burst): round {round}"
            );
        }
        // `_rx` stayed alive so the channel remained full throughout the burst.
        drop(_rx);
    }

    // --- (e) lag -> control (Chain/Health) not stalled -----------------------

    #[tokio::test(start_paused = true)]
    async fn test_deribit_lifecycle_lag_does_not_stall_control_updates() {
        // A slow consumer on a cap-1 channel saturated by coalesced quotes must
        // still receive the control-class Health/Chain (await-sent) in order.
        let (transport, script_tx, _state) = mock_transport();
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(1);
        let (join, cancel) = spawn_loop(transport, tx);

        // A burst of coalesced-class quote frames, then a socket drop forcing a
        // control-class Reconnecting + Chain backfill.
        for round in 0..50u32 {
            let _ = script_tx.send(MockFrame::Text(ticker_frame(
                "BTC-27JUN25-60000-C",
                &burst_ticker_json(round),
            )));
        }
        let _ = script_tx.send(MockFrame::Drop);

        // The slow consumer must still see the control updates: Live, then
        // Reconnecting, then the Chain backfill — never stalled by the quotes.
        let updates = drain_until(&mut rx, is_chain).await;
        assert!(
            updates.iter().any(is_live),
            "Health(Live) delivered despite the lagging consumer"
        );
        assert!(
            updates.iter().any(is_reconnecting),
            "Health(Reconnecting) not stalled by the quote backlog"
        );
        assert!(
            updates.iter().any(is_chain),
            "the Chain backfill reached the lagging consumer"
        );

        cancel.cancel();
        assert!(loop_stopped(join).await);
    }

    // --- (f) shutdown -> dropping the SubscriptionHandle stops the loop -------

    #[tokio::test(start_paused = true)]
    async fn test_deribit_lifecycle_shutdown_on_handle_drop_stops_loop() {
        let (transport, script_tx, _state) = mock_transport();
        let (tx, mut rx) = mpsc::channel::<MarketUpdate>(16);
        let (join, cancel) = spawn_loop(transport, tx);

        // Wrap the loop's cancel token in a real `SubscriptionHandle`, exactly as
        // `subscribe` does, so dropping the handle is what stops the loop.
        let handle = SubscriptionHandle::new(move || cancel.cancel());

        // Reach a live stream, then park on `receive` (the script stays open).
        let _ = script_tx.send(MockFrame::Text(ticker_frame(
            "BTC-27JUN25-60000-C",
            FIXTURE_TICKER_NORMAL,
        )));
        let updates = drain_until(&mut rx, is_live).await;
        assert!(
            updates.iter().any(is_live),
            "the stream is live before shutdown"
        );

        // Dropping the handle cancels the token; the parked loop stops promptly.
        drop(handle);
        assert!(
            loop_stopped(join).await,
            "dropping the SubscriptionHandle stops the reconnect loop"
        );
        drop(script_tx);
    }
}
