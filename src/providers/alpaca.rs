//! The Alpaca adapter — the composed, completeness-provable poll->stream provider,
//! behind a DISABLED build feature (`docs/03-data-providers.md` §7.5,
//! `docs/SECURITY.md` §2.4).
//!
//! Alpaca has a **native** options-data REST surface but its `get_option_chain`
//! endpoint takes only an underlying and returns an `OptionSnapshotsResponse` map
//! with **no** next-page token and **no** expiry filter, so it cannot be proven
//! complete — a large or multi-expiry response could silently truncate. This
//! adapter therefore assembles one expiry via the paginated, filtered path:
//!
//! 1. **discover** every contract for `(underlying, expiration)` with
//!    [`get_option_contracts`](alpaca_http::AlpacaHttpClient::get_option_contracts),
//!    looping `next_page_token` until it is absent (provably exhausted);
//! 2. **hydrate** those OCC symbols with
//!    [`get_option_snapshots`](alpaca_http::AlpacaHttpClient::get_option_snapshots)
//!    in bounded batches (Greeks / IV / quote per strike);
//! 3. **publish atomically** — the chain is emitted only once every discovered
//!    contract is hydrated, so the UI never sees a half-filled chain as
//!    authoritative.
//!
//! # The gate — credential logging upstream (`docs/SECURITY.md` §2.4)
//!
//! Historically `alpaca-websocket` logged the API key and secret in its auth
//! `debug!`, so the whole adapter sits behind the DISABLED-by-default `alpaca`
//! Cargo feature and is **excluded from `with_builtins()`**; it is reachable only
//! via the explicit `with_gated_builtin`, which returns a typed startup error while
//! the gate holds. So a stock binary can **never** execute the upstream's logging —
//! the credential guarantee holds **by construction**, not author discipline
//! (`docs/SECURITY.md` §3). Lifting the gate (the redaction release + a captured-log
//! test + the matrix flip) is tracked in `docs/SECURITY.md` §2.4 and is **out of
//! scope** for this issue.
//!
//! Provenance note for the lifter: the auth-send site is
//! `alpaca-websocket/src/client.rs` `send_auth` (the historical leak was commit
//! `e33eb8f`). The pinned `alpaca-websocket 0.6.0` this crate resolves already
//! masks the key there (`redact_key`) and never logs the secret; the gate stays in
//! place until `docs/SECURITY.md` records the captured-log proof and flips the
//! matrix cell — this adapter does not lift it unilaterally.
//!
//! # Auth is injected programmatically (no dotenv, no foreign env namespace)
//!
//! Both upstream clients construct from `alpaca_base::Credentials::new(key, secret)`
//! plus `Environment::{Paper,Live}`, so [`from_env`](AlpacaAdapter::from_env) reads
//! ChainView-namespaced `CHAINVIEW_ALPACA_*` env vars and builds the clients
//! directly. It never calls `Credentials::from_env` / the clients' `from_env`
//! (which would read the foreign `ALPACA_*` namespace and load a `.env` via
//! `dotenv`), and the crate installs **no** global tracing subscriber on
//! construction (`init_logger` is opt-in and untouched). The credential is read
//! only through [`Secret::expose`](crate::config::Secret::expose) at the single
//! client hand-off site and is never logged or echoed in a `ProviderError`.
//!
//! # Normalization happens at this seam
//!
//! Every raw `alpaca-http` / `alpaca-websocket` DTO stops here
//! (`CLAUDE.md` "Module Boundaries"). The REST strike is a `String` -> checked into
//! [`Positive`] via [`Positive::new_decimal`]; prices/IV/sizes are checked `f64` ->
//! `Positive` and Greeks `f64` -> `Decimal`, rejecting `NaN`/`Inf`/negative before a
//! value enters the chain. Alpaca IV is **already a decimal fraction** (no `/100`).
//! Expiry is the US-equity **`16:00 America/New_York` -> UTC** rule, resolved
//! DST-aware here (`docs/03-data-providers.md` §3).
//!
//! # Venue Greeks flow as `GreeksOrigin::Provider` (the #24/#25 seam)
//!
//! Alpaca snapshots carry venue Greeks/IV. The poll leg folds venue `delta`/`gamma`/
//! `iv` into the `OptionChain` row, and the subscribe/backfill overlay emits per-leg
//! [`GreeksRow`]s tagged [`GreeksOrigin::Provider`] carrying the full venue
//! `delta`/`gamma`/`theta`/`vega`/`rho`/`iv`, so the analytics sidecar records them
//! as venue-supplied (not locally computed). ChainView never hand-rolls
//! Black-Scholes for Alpaca (`greeks: Provided`).
//!
//! # Streaming: underlying only, over a ChainView-owned bounded bridge
//!
//! Alpaca's WebSocket carries **no** option-contract stream — only underlying
//! Trade/Quote/Bar — so [`capabilities`](Provider::capabilities) declares
//! `option_stream: None`, `underlying_stream: true`, `chain_poll: Poll`. The chain
//! is **always polled**; the WS overlays the spot ticker only. The upstream
//! `MarketDataStream` is drained into the ChainView-owned **bounded** two-class
//! [`MarketUpdateSink`] (never handed raw to the app), and the adapter re-runs
//! `subscribe_market_data` on reconnect (`docs/03-data-providers.md` §5). On an
//! upstream `Lagged` signal the adapter **re-syncs** by re-polling the chain rather
//! than rendering a torn view. Because `MarketUpdate` has no underlying-spot
//! variant, the underlying quote is delivered as a [`QuoteUpdate`] for a spot
//! pseudo-instrument (the underlying ticker, a spot-sentinel strike); folding it
//! into `underlying_price` is the app/store seam's concern.
//!
//! # Reconnect + two update classes (`docs/03-data-providers.md` §5, [ADR-0009])
//!
//! The reconnect/resubscribe loop is **ChainView's**, driven behind the
//! [`SubscriptionHandle`]; on a dropped stream it emits `Health(Reconnecting)`,
//! backs off with jittered exponential backoff, re-polls the chain to reconcile
//! drift (the backfill), re-emits the venue overlays, and re-subscribes the
//! underlying. Every [`MarketUpdate`] is handed to the two-class
//! [`MarketUpdateSink`], which routes `Chain`/`Health` to the control channel and
//! coalesces `Quote`/`Greeks`.
//!
//! [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, Utc, Weekday};
use futures_util::StreamExt;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle};
use tokio_util::sync::CancellationToken;

use alpaca_http::{
    AlpacaError, AlpacaHttpClient, Credentials, Environment, OptionContractParams, OptionType,
};
use alpaca_websocket::messages::SubscribeMessage;
use alpaca_websocket::{AlpacaWebSocketClient, DataFeed, MarketDataEvent, MarketDataUpdate};

use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, MarketUpdateSink,
    OptionStreamCapability, Provider, ProviderCapabilities, SinkSend, SubscriptionHandle,
    SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSnapshot, ChainSource, ContractSpecFingerprint, ExerciseStyle,
    ExpirySource, GreeksOrigin, GreeksRow, Instrument, InstrumentKey, MarketUpdate, ProviderId,
    QuoteUpdate, SettlementStyle, StreamHealth,
};
use crate::config::{EnvSource, Secret, require_credentials};
use crate::error::{NormalizeKind, ProviderError, TransportDetail, TransportKind};

/// The reserved provider id this adapter registers under
/// ([`RESERVED_PROVIDER_IDS`](crate::chain::RESERVED_PROVIDER_IDS)).
const ALPACA_ID: &str = "alpaca";

/// The credential field names read from the environment for `KeySecret` auth
/// (`CHAINVIEW_ALPACA_API_KEY` / `CHAINVIEW_ALPACA_API_SECRET`,
/// `docs/03-data-providers.md` §11.3).
const CREDENTIAL_KEYS: [&str; 2] = ["api_key", "api_secret"];

/// The optional environment-selector variable (`paper` | `live`); absent or
/// unrecognized defaults to the safe [`AlpacaEnvironment::Paper`].
const ENVIRONMENT_VAR: &str = "CHAINVIEW_ALPACA_ENVIRONMENT";

/// The suggested chain-refresh cadence, in seconds — a hint only; the effective
/// interval is `config.refresh_interval` (`docs/03-data-providers.md` §2). Option
/// snapshots are polled, so a slightly longer hint than a crypto venue is honest.
const REFRESH_HINT_SECS: u32 = 5;

/// The quote currency US-equity option premiums settle in.
const QUOTE_CURRENCY: &str = "USD";

/// The default US-equity contract multiplier (shares per contract) when the venue
/// `size` is absent or will not parse.
const DEFAULT_SHARES_PER_CONTRACT: u32 = 100;

/// The maximum OCC symbols hydrated per [`get_option_snapshots`] call — the
/// endpoint's per-request ceiling. Discovery is chunked into batches of this size
/// so a large expiry is hydrated in bounded requests.
const MAX_SYMBOLS_PER_BATCH: usize = 100;

/// The hard cap on discovery pages walked, a runaway guard on the `next_page_token`
/// loop. A well-formed venue exhausts far below this; hitting it stops discovery
/// (bounded), never an unbounded loop.
const MAX_DISCOVERY_PAGES: usize = 64;

/// The hard cap on discovered contracts accumulated, a bounded-memory guard. A
/// single US-equity expiry is a few hundred contracts; this ceiling is a safety
/// valve, never a normal limit.
const MAX_CONTRACTS: usize = 8_192;

/// The upstream WebSocket connect retry budget handed to
/// [`connect_with_reconnect`](alpaca_websocket::AlpacaWebSocketClient::connect_with_reconnect).
/// ChainView owns the outer reconnect loop, so this only bounds one connect burst.
const WS_CONNECT_RETRIES: u32 = 3;

/// The `f64` size envelope that is exact for a `u64`/`u32` cast (`2^53`), a bound no
/// real contract quantity approaches.
const SIZE_EXACT_ENVELOPE: u64 = 1 << 53;

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

/// The trading environment the adapter targets — ChainView's own mirror of the
/// upstream `Environment`, so [`from_env`](AlpacaAdapter::from_env) can be tested
/// without constructing an upstream client. Mapped to the upstream type only at the
/// single client-construction site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AlpacaEnvironment {
    /// Paper trading — the safe default (data access unaffected).
    #[default]
    Paper,
    /// Live trading.
    Live,
}

impl AlpacaEnvironment {
    /// Parse the optional `CHAINVIEW_ALPACA_ENVIRONMENT` value; anything other than
    /// a case-insensitive `live` is [`Paper`](Self::Paper).
    fn from_value(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("live") {
            Self::Live
        } else {
            Self::Paper
        }
    }

    /// Map to the upstream `Environment`.
    fn to_upstream(self) -> Environment {
        match self {
            Self::Paper => Environment::Paper,
            Self::Live => Environment::Live,
        }
    }
}

/// The Alpaca `Provider` adapter (crate-internal; behind the disabled `alpaca`
/// feature and reachable only via `with_gated_builtin`).
///
/// Holds the reserved [`ProviderId`], the env-resolved credentials (wrapped in
/// [`Secret`], never logged), and the selected environment. `Clone` is cheap — a
/// clone is moved into the spawned reconnect loop so it can re-poll and reconnect
/// without borrowing `&self` across the task boundary.
#[derive(Clone)]
pub(crate) struct AlpacaAdapter {
    id: ProviderId,
    api_key: Secret,
    api_secret: Secret,
    environment: AlpacaEnvironment,
}

impl AlpacaAdapter {
    /// Build the adapter from the ChainView-namespaced environment
    /// (`CHAINVIEW_ALPACA_API_KEY` / `CHAINVIEW_ALPACA_API_SECRET`, and the optional
    /// `CHAINVIEW_ALPACA_ENVIRONMENT`). The credentials are read **only** here
    /// (env-only policy) and wrapped in [`Secret`]; they are never logged or echoed
    /// in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::MissingCredential`](crate::error::ConfigError::MissingCredential)
    /// (naming the provider, never the key) when either credential is unset/empty.
    pub(crate) fn from_env(env: &dyn EnvSource) -> Result<Self, crate::error::ConfigError> {
        let id = alpaca_provider_id();
        let creds = require_credentials(env, &id, &CREDENTIAL_KEYS)?;
        let api_key = creds
            .get("API_KEY")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        let api_secret = creds
            .get("API_SECRET")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        let environment = env
            .get(ENVIRONMENT_VAR)
            .map(|value| AlpacaEnvironment::from_value(&value))
            .unwrap_or_default();
        Ok(Self {
            id,
            api_key,
            api_secret,
            environment,
        })
    }

    /// Build the upstream `Credentials` from the injected secrets. The secret is
    /// exposed only at this single hand-off site and never logged.
    fn credentials(&self) -> Credentials {
        Credentials::new(
            self.api_key.expose().to_owned(),
            self.api_secret.expose().to_owned(),
        )
    }

    /// Build the REST client, injecting credentials programmatically (never the
    /// crate's `from_env`).
    ///
    /// # Errors
    ///
    /// A redaction-safe [`ProviderError`] when the upstream client rejects the
    /// configuration (never carrying the credential).
    fn http_client(&self) -> Result<AlpacaHttpClient, ProviderError> {
        AlpacaHttpClient::new(self.credentials(), self.environment.to_upstream())
            .map_err(alpaca_error)
    }

    /// Build the WebSocket client for the underlying data feed (IEX — the free feed
    /// that works on paper and live), injecting credentials programmatically.
    fn ws_client(&self) -> AlpacaWebSocketClient {
        AlpacaWebSocketClient::with_feed(
            self.credentials(),
            self.environment.to_upstream(),
            DataFeed::Iex,
        )
    }
}

#[async_trait]
impl Provider for AlpacaAdapter {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        alpaca_capabilities()
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        // Alpaca exposes no cheap "list every optionable underlying" endpoint; the
        // caller names the underlying and `fetch_chain` resolves its chain.
        Err(ProviderError::Unsupported("underlying discovery"))
    }

    async fn fetch_chain(
        &self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        let source = LiveDataSource {
            client: self.http_client()?,
        };
        let composed = compose_chain(&source, underlying, expiration, &self.id, now_utc()).await?;
        Ok(composed.fetch)
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
            underlying,
            expiration_utc,
            instruments: _instruments,
            cancel,
        } = req;
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            transport,
            id,
            underlying,
            expiration_utc,
            sink,
            loop_cancel,
        ));
        Ok(SubscriptionHandle::spawned(cancel, handle))
    }
}

// ---------------------------------------------------------------------------
// Identity + capabilities.
// ---------------------------------------------------------------------------

/// The adapter's reserved [`ProviderId`]. `"alpaca"` is a compile-time literal that
/// satisfies the grammar (proven by `test_alpaca_id_is_valid_and_reserved`), so
/// construction cannot fail; the fallback arm is unreachable.
fn alpaca_provider_id() -> ProviderId {
    match ProviderId::new(ALPACA_ID) {
        Ok(id) => id,
        Err(_) => unreachable!("`alpaca` is a valid, reserved provider id literal"),
    }
}

/// Alpaca's honest capability self-declaration — the `docs/03-data-providers.md`
/// §8 row: a native REST chain (composed + completeness-provable), **no** option
/// depth (crypto order books only, outside the v1 option product), venue-provided
/// Greeks, **no** option-contract stream, an underlying stream, REST chain polling,
/// no trades tape, and `KeySecret` auth.
///
/// The `option_stream: None` + `underlying_stream: true` + `chain_poll: Poll` split
/// is the whole point of the three-dimensional streaming model: it makes it
/// **impossible** to mis-badge Alpaca's polled option chain as a real-time stream.
#[must_use]
pub(crate) fn alpaca_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Native)
        .depth(false)
        .greeks(GreeksCapability::Provided)
        .option_stream(OptionStreamCapability::None)
        .underlying_stream(true)
        .chain_poll(ChainPollCapability::Poll {
            interval_hint_secs: REFRESH_HINT_SECS,
        })
        .trades_tape(false)
        .auth(AuthKind::KeySecret)
        .build()
}

// ---------------------------------------------------------------------------
// Expiry resolution: 16:00 America/New_York -> UTC, DST-aware.
// ---------------------------------------------------------------------------

/// Resolve a US-equity `YYYY-MM-DD` expiry to an absolute UTC instant at the venue's
/// **`16:00 America/New_York`** close, DST-aware (`docs/03-data-providers.md` §3).
///
/// The Eastern offset at a 16:00 wall-clock time is unambiguous: both DST
/// transitions occur at `02:00`, well before the close, so a same-day 16:00 is EDT
/// (`UTC-4` -> `20:00 UTC`) inside the DST window and EST (`UTC-5` -> `21:00 UTC`)
/// outside it.
///
/// # Errors
///
/// [`NormalizeKind::UnparseableExpiry`] for a malformed or calendar-invalid date.
fn expiry_to_utc(date_str: &str) -> Result<DateTime<Utc>, NormalizeKind> {
    let date = parse_ymd(date_str)?;
    let offset_hours = if is_us_eastern_dst(date) { 4 } else { 5 };
    let local_close = date
        .and_hms_opt(16, 0, 0)
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    let utc_naive = local_close
        .checked_add_signed(TimeDelta::hours(offset_hours))
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(utc_naive, Utc))
}

/// Parse a strict `YYYY-MM-DD` date, rejecting any other shape or a
/// calendar-invalid day.
fn parse_ymd(s: &str) -> Result<NaiveDate, NormalizeKind> {
    let mut parts = s.split('-');
    let year = parts
        .next()
        .and_then(|value| value.parse::<i32>().ok())
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    let month = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    let day = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    if parts.next().is_some() {
        return Err(NormalizeKind::UnparseableExpiry);
    }
    NaiveDate::from_ymd_opt(year, month, day).ok_or(NormalizeKind::UnparseableExpiry)
}

/// Render a [`NaiveDate`] as a strict `YYYY-MM-DD` string — the grammar the
/// contracts filter expects. Built manually (no locale-dependent formatting).
fn format_ymd(date: NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// Whether US Eastern DST (EDT) is in effect on `date` at the 16:00 close: from the
/// **second Sunday of March** through the day **before** the first Sunday of
/// November.
fn is_us_eastern_dst(date: NaiveDate) -> bool {
    let year = date.year();
    match (
        nth_weekday_of_month(year, 3, Weekday::Sun, 2),
        nth_weekday_of_month(year, 11, Weekday::Sun, 1),
    ) {
        (Some(start), Some(end)) => date >= start && date < end,
        _ => false,
    }
}

/// The `n`-th (1-based) `weekday` in `month` of `year`, or `None` when the month has
/// fewer than `n` of them.
fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let first_dow = first.weekday().num_days_from_sunday();
    let target_dow = weekday.num_days_from_sunday();
    let offset = (target_dow + 7 - first_dow) % 7;
    let day = 1u32
        .checked_add(offset)?
        .checked_add(n.checked_sub(1)?.checked_mul(7)?)?;
    NaiveDate::from_ymd_opt(year, month, day)
}

// ---------------------------------------------------------------------------
// Numeric normalization at the f64 seam.
// ---------------------------------------------------------------------------

/// A checked price/size field: `NaN`/`Inf`/negative is **dropped** (returns `None`).
/// Zero is a valid value and is kept.
fn positive_or_drop(value: f64) -> Option<Positive> {
    Positive::new(value).ok()
}

/// A checked implied-volatility figure. Alpaca IV is **already a decimal fraction**
/// (e.g. `0.35`), so there is no `/100` step. `NaN`/`Inf`/negative is dropped.
fn iv_or_drop(value: f64) -> Option<Positive> {
    if !value.is_finite() {
        return None;
    }
    Positive::new(value).ok()
}

/// A checked Greek: `NaN`/`Inf`/out-of-range is dropped (Greeks may legitimately be
/// negative, so there is no sign check). Uses the std `TryFrom<f64>` conversion.
fn greek_or_drop(value: Option<f64>) -> Option<Decimal> {
    let raw = value?;
    if !raw.is_finite() {
        return None;
    }
    Decimal::try_from(raw).ok()
}

/// A checked venue `u64` size -> [`Positive`]: dropped when it exceeds the exact
/// `f64` envelope (`2^53`), a bound no real quantity approaches.
#[allow(clippy::cast_precision_loss)]
fn size_to_positive(size: u64) -> Option<Positive> {
    if size >= SIZE_EXACT_ENVELOPE {
        return None;
    }
    Positive::new(size as f64).ok()
}

/// A checked strike: the venue `String` is parsed to a [`Decimal`] then into a
/// [`Positive`] via the checked [`Positive::new_decimal`] (rejecting a negative),
/// and a zero strike is refused (not a real contract).
fn strike_positive(value: &str) -> Result<Positive, NormalizeKind> {
    let decimal = value
        .trim()
        .parse::<Decimal>()
        .map_err(|_| NormalizeKind::OutOfRange("strike"))?;
    let strike = Positive::new_decimal(decimal).map_err(|_| NormalizeKind::OutOfRange("strike"))?;
    if strike == Positive::ZERO {
        return Err(NormalizeKind::OutOfRange("strike"));
    }
    Ok(strike)
}

/// The venue `size` string as a checked contract multiplier, defaulting to
/// [`DEFAULT_SHARES_PER_CONTRACT`] when absent, non-numeric, or out of range.
fn multiplier_of(size: Option<&str>) -> u32 {
    size.and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 1)
        .unwrap_or(DEFAULT_SHARES_PER_CONTRACT)
}

/// The venue `open_interest` string as a checked `u64`, or `None` when absent or
/// non-numeric.
fn open_interest_of(value: Option<&str>) -> Option<u64> {
    value.and_then(|raw| raw.trim().parse::<u64>().ok())
}

/// A normalized best-bid/best-ask pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NormalizedQuote {
    bid: Option<Positive>,
    ask: Option<Positive>,
}

/// Normalize a best-bid/best-ask pair with the field-specific rules
/// (`docs/03-data-providers.md` §3 table): a per-field `NaN`/`Inf`/negative is
/// dropped to `None`; a **zero bid is valid**; a **zero ask on a non-zero bid**, or
/// any `ask < bid`, is **crossed** and the whole update is rejected.
fn normalize_quote(bid: Option<f64>, ask: Option<f64>) -> Result<NormalizedQuote, NormalizeKind> {
    let bid = bid.and_then(positive_or_drop);
    let ask = ask.and_then(positive_or_drop);
    if let (Some(bid_value), Some(ask_value)) = (bid, ask)
        && ask_value < bid_value
    {
        return Err(NormalizeKind::OutOfRange("ask"));
    }
    Ok(NormalizedQuote { bid, ask })
}

/// The Alpaca economic-equivalence fingerprint: standard US-equity options are
/// **physically-settled**, quoted in USD, keyed by the chain's root symbol; the
/// exercise style and multiplier come from the contract.
fn alpaca_fingerprint(
    root_symbol: &str,
    multiplier: u32,
    exercise: ExerciseStyle,
) -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: multiplier,
        settlement: SettlementStyle::Physical,
        exercise,
        quote_currency: QUOTE_CURRENCY.to_owned(),
        venue_product_code: root_symbol.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Raw DTO views — mapped from the upstream types, never escaping this module.
// ---------------------------------------------------------------------------

/// A neutral view of one discovered option contract, mapped from the upstream
/// `OptionContract` inside [`LiveDataSource`] so no raw DTO escapes.
#[derive(Debug, Clone)]
struct RawContract {
    /// The OCC symbol — the native id and the hydration/subscription key.
    symbol: String,
    /// The canonical (upper-cased) underlying ticker.
    underlying: String,
    /// The expiry `YYYY-MM-DD`.
    expiration_date: String,
    /// The strike price, as the venue's decimal string.
    strike_price: String,
    /// Call or put (from the upstream `type`).
    style: OptionStyle,
    /// American or European exercise (from the upstream `style`).
    exercise: ExerciseStyle,
    /// The chain's root symbol (venue product code).
    root_symbol: String,
    /// The venue contract size string (shares per contract), when present.
    size: Option<String>,
    /// The venue open-interest string, when present.
    open_interest: Option<String>,
}

/// A neutral view of one option snapshot, mapped from the upstream `OptionSnapshot`.
/// Every numeric is a raw `f64`/`u64` here and is checked at the seam before it
/// enters a domain type.
#[derive(Debug, Clone, Default)]
struct RawSnapshot {
    bid: Option<f64>,
    ask: Option<f64>,
    last: Option<f64>,
    bid_size: Option<u64>,
    ask_size: Option<u64>,
    quote_time: Option<DateTime<Utc>>,
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
    rho: Option<f64>,
    iv: Option<f64>,
}

/// Map an upstream `OptionType` to the domain call/put style.
fn style_of(option_type: OptionType) -> OptionStyle {
    match option_type {
        OptionType::Call => OptionStyle::Call,
        OptionType::Put => OptionStyle::Put,
    }
}

/// Map an upstream option `style` (american/european) to the domain exercise style.
fn exercise_of(style: alpaca_http::OptionStyle) -> ExerciseStyle {
    match style {
        alpaca_http::OptionStyle::American => ExerciseStyle::American,
        alpaca_http::OptionStyle::European => ExerciseStyle::European,
    }
}

/// Map one upstream `OptionContract` onto the neutral [`RawContract`].
fn map_contract(contract: alpaca_http::OptionContract) -> RawContract {
    RawContract {
        symbol: contract.symbol,
        underlying: contract.underlying_symbol.to_ascii_uppercase(),
        expiration_date: contract.expiration_date,
        strike_price: contract.strike_price,
        style: style_of(contract.option_type),
        exercise: exercise_of(contract.style),
        root_symbol: contract.root_symbol,
        size: contract.size,
        open_interest: contract.open_interest,
    }
}

/// Map one upstream `OptionSnapshot` onto the neutral [`RawSnapshot`].
fn map_snapshot(snapshot: alpaca_http::OptionSnapshot) -> RawSnapshot {
    let (bid, ask, bid_size, ask_size, quote_time) = match snapshot.latest_quote {
        Some(quote) => (
            Some(quote.bid_price),
            Some(quote.ask_price),
            Some(quote.bid_size),
            Some(quote.ask_size),
            Some(quote.timestamp),
        ),
        None => (None, None, None, None, None),
    };
    let last = snapshot.latest_trade.map(|trade| trade.price);
    let (delta, gamma, theta, vega, rho) = match snapshot.greeks {
        Some(greeks) => (
            greeks.delta,
            greeks.gamma,
            greeks.theta,
            greeks.vega,
            greeks.rho,
        ),
        None => (None, None, None, None, None),
    };
    RawSnapshot {
        bid,
        ask,
        last,
        bid_size,
        ask_size,
        quote_time,
        delta,
        gamma,
        theta,
        vega,
        rho,
        iv: snapshot.implied_volatility,
    }
}

// ---------------------------------------------------------------------------
// The composed, completeness-provable chain path.
// ---------------------------------------------------------------------------

/// One page of contract discovery — the contracts plus the token for the next page
/// (`None` when discovery is exhausted).
#[derive(Debug, Clone, Default)]
struct ContractPage {
    contracts: Vec<RawContract>,
    next_page_token: Option<String>,
}

/// The REST data seam the composed chain path drives, so paging/hydration/atomic
/// publish run deterministically against a **mock** with no network. The production
/// [`LiveDataSource`] wraps the upstream `AlpacaHttpClient`; a raw DTO is mapped to
/// the neutral views inside it and never crosses this seam.
#[async_trait]
trait ChainDataSource: Send + Sync {
    /// Fetch one page of contract discovery for `(underlying, expiration_date)`.
    async fn discover_page(
        &self,
        underlying: &str,
        expiration_date: &str,
        page_token: Option<String>,
    ) -> Result<ContractPage, ProviderError>;

    /// Hydrate one bounded batch of OCC symbols into their snapshots, keyed by OCC
    /// symbol. A symbol the venue omits simply has no entry.
    async fn hydrate_batch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, RawSnapshot>, ProviderError>;
}

/// The production [`ChainDataSource`]: the upstream `AlpacaHttpClient`. Raw upstream
/// types stay inside it.
struct LiveDataSource {
    client: AlpacaHttpClient,
}

#[async_trait]
impl ChainDataSource for LiveDataSource {
    async fn discover_page(
        &self,
        underlying: &str,
        expiration_date: &str,
        page_token: Option<String>,
    ) -> Result<ContractPage, ProviderError> {
        let params = OptionContractParams {
            underlying_symbol: Some(underlying.to_owned()),
            expiration_date: Some(expiration_date.to_owned()),
            page_token,
            ..OptionContractParams::default()
        };
        let response = self
            .client
            .get_option_contracts(&params)
            .await
            .map_err(alpaca_error)?;
        let contracts = response
            .option_contracts
            .into_iter()
            .map(map_contract)
            .collect();
        Ok(ContractPage {
            contracts,
            next_page_token: response.next_page_token,
        })
    }

    async fn hydrate_batch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, RawSnapshot>, ProviderError> {
        if symbols.is_empty() {
            return Ok(HashMap::new());
        }
        let joined = symbols.join(",");
        let response = self
            .client
            .get_option_snapshots(&joined)
            .await
            .map_err(alpaca_error)?;
        Ok(response
            .snapshots
            .into_iter()
            .map(|(symbol, snapshot)| (symbol, map_snapshot(snapshot)))
            .collect())
    }
}

/// The composed result: the poll-leg [`ChainFetch`] plus the per-leg venue overlays
/// (`Quote`/`Greeks`, tagged [`GreeksOrigin::Provider`]) the subscribe loop replays
/// so the venue Greeks reach the sidecar (the #24/#25 seam).
#[derive(Debug, Clone)]
struct ComposedChain {
    fetch: ChainFetch,
    overlays: Vec<MarketUpdate>,
}

/// One normalized contract leg — the domain values assembled into an [`OptionChain`]
/// row, its [`AliasCatalog`] entry, and its venue overlay.
#[derive(Debug, Clone)]
struct NormalizedLeg {
    key: InstrumentKey,
    native_symbol: String,
    spec: ContractSpecFingerprint,
    style: OptionStyle,
    bid: Option<Positive>,
    ask: Option<Positive>,
    last: Option<Positive>,
    bid_size: Option<Positive>,
    ask_size: Option<Positive>,
    quote_time: Option<DateTime<Utc>>,
    delta: Option<Decimal>,
    gamma: Option<Decimal>,
    theta: Option<Decimal>,
    vega: Option<Decimal>,
    rho: Option<Decimal>,
    iv: Option<Positive>,
    open_interest: Option<u64>,
}

/// Normalize one discovered contract joined with its (optional) snapshot into a
/// [`NormalizedLeg`]. A contract whose strike will not normalize contributes **no**
/// leg (returns `None`); every price/IV/Greek field is checked at the `f64` seam and
/// a crossed quote drops only the quote.
fn normalize_leg(
    contract: &RawContract,
    expiration_utc: DateTime<Utc>,
    snapshot: Option<&RawSnapshot>,
) -> Option<NormalizedLeg> {
    let strike = strike_positive(&contract.strike_price).ok()?;
    let multiplier = multiplier_of(contract.size.as_deref());
    let spec = alpaca_fingerprint(&contract.root_symbol, multiplier, contract.exercise);
    let key = InstrumentKey {
        underlying: contract.underlying.clone(),
        expiration_utc,
        strike,
        style: contract.style,
    };

    let snapshot = snapshot.cloned().unwrap_or_default();
    let quote = normalize_quote(snapshot.bid, snapshot.ask).unwrap_or_default();

    Some(NormalizedLeg {
        key,
        native_symbol: contract.symbol.clone(),
        spec,
        style: contract.style,
        bid: quote.bid,
        ask: quote.ask,
        last: snapshot.last.and_then(positive_or_drop),
        bid_size: snapshot.bid_size.and_then(size_to_positive),
        ask_size: snapshot.ask_size.and_then(size_to_positive),
        quote_time: snapshot.quote_time,
        delta: greek_or_drop(snapshot.delta),
        gamma: greek_or_drop(snapshot.gamma),
        theta: greek_or_drop(snapshot.theta),
        vega: greek_or_drop(snapshot.vega),
        rho: greek_or_drop(snapshot.rho),
        iv: snapshot.iv.and_then(iv_or_drop),
        open_interest: open_interest_of(contract.open_interest.as_deref()),
    })
}

/// The venue [`GreeksRow`] for one leg, tagged [`GreeksOrigin::Provider`] and
/// carrying the full venue `delta`/`gamma`/`theta`/`vega`/`rho`/`iv` — the #24/#25
/// seam that lands venue Greeks in the analytics sidecar.
fn snapshot_greeks_row(
    instrument: &Instrument,
    leg: &NormalizedLeg,
    received: DateTime<Utc>,
) -> GreeksRow {
    GreeksRow {
        instrument: instrument.clone(),
        iv: leg.iv,
        delta: leg.delta,
        gamma: leg.gamma,
        theta: leg.theta,
        vega: leg.vega,
        rho: leg.rho,
        origin: GreeksOrigin::Provider,
        event_time: leg.quote_time,
        received_time: received,
    }
}

/// The venue [`QuoteUpdate`] for one leg (bid/ask/last/sizes from the snapshot).
fn snapshot_quote(
    instrument: &Instrument,
    leg: &NormalizedLeg,
    received: DateTime<Utc>,
) -> QuoteUpdate {
    QuoteUpdate {
        instrument: instrument.clone(),
        bid: leg.bid,
        ask: leg.ask,
        last: leg.last,
        bid_size: leg.bid_size,
        ask_size: leg.ask_size,
        event_time: leg.quote_time,
        received_time: received,
    }
}

/// Whether a leg carries any venue quote field worth publishing.
fn has_quote(leg: &NormalizedLeg) -> bool {
    leg.bid.is_some() || leg.ask.is_some() || leg.last.is_some()
}

/// Whether a leg carries any venue Greek/IV worth publishing.
fn has_greeks(leg: &NormalizedLeg) -> bool {
    leg.iv.is_some()
        || leg.delta.is_some()
        || leg.gamma.is_some()
        || leg.theta.is_some()
        || leg.vega.is_some()
        || leg.rho.is_some()
}

/// The call/put legs sharing one strike.
#[derive(Debug, Default)]
struct StrikePair<'a> {
    call: Option<&'a NormalizedLeg>,
    put: Option<&'a NormalizedLeg>,
}

/// Assemble the composed chain from a completeness-provable discovery + hydration:
/// (1) discover every contract for `(underlying, expiration)`, looping
/// `next_page_token` until absent (bounded by [`MAX_DISCOVERY_PAGES`] /
/// [`MAX_CONTRACTS`]); (2) hydrate the discovered OCC symbols in
/// [`MAX_SYMBOLS_PER_BATCH`]-sized batches; (3) assemble the [`OptionChain`],
/// [`AliasCatalog`], and venue overlays **atomically** — the result is built once,
/// after every discovered contract is hydrated, so no half-filled chain is ever
/// returned.
///
/// # Errors
///
/// [`ProviderError::Normalize`] for an unparseable expiry; [`ProviderError::NoChain`]
/// when discovery yields no normalizable contract; a transport failure from
/// discovery or hydration.
async fn compose_chain<S: ChainDataSource + ?Sized>(
    source: &S,
    underlying: &str,
    expiration: &ExpirationDate,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Result<ComposedChain, ProviderError> {
    let symbol = underlying.to_ascii_uppercase();
    let target = expiration
        .get_date()
        .map_err(|_| ProviderError::Normalize {
            kind: NormalizeKind::UnparseableExpiry,
        })?;
    let expiration_date = format_ymd(target.date_naive());
    let expiration_utc =
        expiry_to_utc(&expiration_date).map_err(|kind| ProviderError::Normalize { kind })?;

    // (1) Discover every contract, provably exhausting the pages.
    let contracts = discover_contracts(source, &symbol, &expiration_date).await?;
    if contracts.is_empty() {
        return Err(ProviderError::NoChain {
            underlying: symbol,
            expiration: expiration_utc.to_rfc3339(),
        });
    }

    // (2) Hydrate the discovered symbols in bounded batches.
    let occ_symbols: Vec<String> = contracts.iter().map(|c| c.symbol.clone()).collect();
    let snapshots = hydrate_symbols(source, &occ_symbols).await?;

    // (3) Assemble atomically — only after every contract is hydrated.
    assemble_composed(
        &symbol,
        expiration_utc,
        &contracts,
        &snapshots,
        provider,
        received,
    )
}

/// Loop `next_page_token` until it is absent (discovery provably exhausted),
/// accumulating contracts filtered to the requested expiry. Bounded by
/// [`MAX_DISCOVERY_PAGES`] pages and [`MAX_CONTRACTS`] contracts (runaway guards).
async fn discover_contracts<S: ChainDataSource + ?Sized>(
    source: &S,
    underlying: &str,
    expiration_date: &str,
) -> Result<Vec<RawContract>, ProviderError> {
    let mut all = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..MAX_DISCOVERY_PAGES {
        let page = source
            .discover_page(underlying, expiration_date, page_token.clone())
            .await?;
        for contract in page.contracts {
            // Belt-and-suspenders: keep only the requested expiry (the API already
            // filters), and stop accumulating at the bounded ceiling.
            if contract.expiration_date == expiration_date && all.len() < MAX_CONTRACTS {
                all.push(contract);
            }
        }
        match page.next_page_token {
            Some(token) if !token.is_empty() => page_token = Some(token),
            // No further pages (or an empty token) -> discovery is exhausted.
            _ => break,
        }
    }
    Ok(all)
}

/// Hydrate every discovered OCC symbol into snapshots, chunked into
/// [`MAX_SYMBOLS_PER_BATCH`]-sized batches. A batch REQUEST failure propagates (the
/// chain cannot be proven complete); a symbol the venue omits simply has no entry.
async fn hydrate_symbols<S: ChainDataSource + ?Sized>(
    source: &S,
    symbols: &[String],
) -> Result<HashMap<String, RawSnapshot>, ProviderError> {
    let mut merged: HashMap<String, RawSnapshot> = HashMap::with_capacity(symbols.len());
    for batch in symbols.chunks(MAX_SYMBOLS_PER_BATCH) {
        let snapshots = source.hydrate_batch(batch).await?;
        for (symbol, snapshot) in snapshots {
            let _ = merged.insert(symbol, snapshot);
        }
    }
    Ok(merged)
}

/// Assemble the discovered + hydrated legs into a single [`OptionChain`] plus its
/// [`AliasCatalog`] and per-leg venue overlays. The published strike set equals the
/// normalizable discovered set.
///
/// # Errors
///
/// [`ProviderError::NoChain`] when no discovered contract yields a normalizable leg.
fn assemble_composed(
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    contracts: &[RawContract],
    snapshots: &HashMap<String, RawSnapshot>,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Result<ComposedChain, ProviderError> {
    let legs: Vec<NormalizedLeg> = contracts
        .iter()
        .filter_map(|contract| {
            normalize_leg(contract, expiration_utc, snapshots.get(&contract.symbol))
        })
        .collect();

    if legs.is_empty() {
        return Err(ProviderError::NoChain {
            underlying: underlying.to_owned(),
            expiration: expiration_utc.to_rfc3339(),
        });
    }

    // The alias catalog carries the native OCC symbol per leg for (re)subscription.
    let mut aliases = AliasCatalog::new();
    let mut overlays: Vec<MarketUpdate> = Vec::new();
    for leg in &legs {
        let instrument = Instrument {
            key: leg.key.clone(),
            provider: provider.clone(),
            native_symbol: leg.native_symbol.clone(),
            stream_symbol: None,
            spec: leg.spec.clone(),
        };
        if has_quote(leg) {
            overlays.push(MarketUpdate::Quote(snapshot_quote(
                &instrument,
                leg,
                received,
            )));
        }
        if has_greeks(leg) {
            overlays.push(MarketUpdate::Greeks(snapshot_greeks_row(
                &instrument,
                leg,
                received,
            )));
        }
        aliases.insert(instrument);
    }

    // Group call/put per strike into one OptionData row (deterministic, order-free).
    let mut by_strike: BTreeMap<Positive, StrikePair<'_>> = BTreeMap::new();
    for leg in &legs {
        let entry = by_strike.entry(leg.key.strike).or_default();
        match leg.style {
            OptionStyle::Call => entry.call = Some(leg),
            OptionStyle::Put => entry.put = Some(leg),
        }
    }

    // The composed snapshot carries no underlying price, so seed the chain center to
    // the MEDIAN strike (a real value from the strike ladder, refreshed by the
    // underlying stream / next poll), never a fabricated quote.
    let spot = median_strike(&by_strike);
    let mut chain = OptionChain::new(underlying, spot, expiration_utc.to_rfc3339(), None, None);
    for (strike, pair) in &by_strike {
        let iv = pair
            .call
            .and_then(|leg| leg.iv)
            .or_else(|| pair.put.and_then(|leg| leg.iv))
            .unwrap_or(Positive::ZERO);
        chain.add_option(
            *strike,
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
            None,
            pair.call
                .and_then(|leg| leg.open_interest)
                .or_else(|| pair.put.and_then(|leg| leg.open_interest)),
            None,
        );
    }

    let fetch = ChainFetch::new(
        chain,
        ExpirySource::new(underlying, expiration_utc, provider.clone()),
        aliases,
    );
    Ok(ComposedChain { fetch, overlays })
}

/// The median strike of a sorted strike map — the provisional chain center used when
/// the composed payload carries no spot. The map is non-empty at every call site
/// (the caller returns `NoChain` first), so the fallback is never taken.
fn median_strike(by_strike: &BTreeMap<Positive, StrikePair<'_>>) -> Positive {
    let strikes: Vec<Positive> = by_strike.keys().copied().collect();
    let mid = strikes.len() / 2;
    strikes.get(mid).copied().unwrap_or(Positive::ONE)
}

// ---------------------------------------------------------------------------
// Redaction-safe transport / auth error mapping.
// ---------------------------------------------------------------------------

/// Map an upstream [`AlpacaError`] to a redaction-safe [`ProviderError`] by
/// **category only** — the inner message (which may hold a URL, body, or the
/// credential the upstream may interpolate) is never carried
/// (`docs/03-data-providers.md` §6, `docs/SECURITY.md` §1). Only a non-secret HTTP
/// status rides along.
fn alpaca_error(err: AlpacaError) -> ProviderError {
    match err {
        AlpacaError::Auth(_) => ProviderError::Auth,
        AlpacaError::RateLimit {
            retry_after_secs, ..
        } => ProviderError::RateLimited(Some(Duration::from_secs(retry_after_secs))),
        AlpacaError::Api { status, .. } => ProviderError::Transport(Box::new(
            TransportDetail::new(TransportKind::Http, Some(status)),
        )),
        AlpacaError::Timeout(_) => transport(TransportKind::Closed),
        AlpacaError::Network(_) | AlpacaError::WebSocket(_) => transport(TransportKind::Closed),
        AlpacaError::Json(_) | AlpacaError::InvalidData(_) => transport(TransportKind::Decode),
        AlpacaError::Http(_)
        | AlpacaError::Config(_)
        | AlpacaError::Validation(_)
        | AlpacaError::ValidationErrors(_) => transport(TransportKind::Http),
    }
}

/// A [`ProviderError::Transport`] carrying only a category (no status, no upstream
/// text).
fn transport(kind: TransportKind) -> ProviderError {
    ProviderError::Transport(Box::new(TransportDetail::new(kind, None)))
}

// ---------------------------------------------------------------------------
// The stream transport seam: the venue I/O the reconnect loop drives (mockable).
// ---------------------------------------------------------------------------

/// The transport is gone — a connect/subscribe step failed or the stream
/// dropped/errored. A zero-size marker: it carries no upstream text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransportGone;

/// A neutral, adapter-internal view of one raw Alpaca WS event, so the reconnect
/// loop is testable against a mock with **no** upstream type. The raw
/// `MarketDataEvent` is mapped onto this inside [`LiveTransport`] and never escapes.
#[derive(Debug, Clone)]
enum RawStreamEvent {
    /// An underlying quote: `bid`/`ask` prices, `bid_size`/`ask_size`, and a venue
    /// timestamp.
    UnderlyingQuote {
        symbol: String,
        bid: f64,
        ask: f64,
        bid_size: u64,
        ask_size: u64,
        time: DateTime<Utc>,
    },
    /// An underlying trade — carries a `last` price and a venue timestamp.
    UnderlyingTrade {
        symbol: String,
        price: f64,
        time: DateTime<Utc>,
    },
    /// The consumer fell behind and the upstream dropped `missed` updates — the
    /// signal to re-sync (re-poll) per the bounded-bridge lag policy.
    Lagged,
    /// A bar or lifecycle event the adapter does not overlay — ignored.
    Ignored,
}

/// The venue-I/O seam the reconnect loop drives so the loop runs deterministically
/// against a **mock** — no socket, no wall clock. The production [`LiveTransport`]
/// wraps the upstream `AlpacaWebSocketClient` / `MarketDataStream` plus the REST
/// re-poll; a raw `MarketDataEvent` is decoded to [`RawStreamEvent`] inside it and
/// never crosses this seam.
#[async_trait]
trait AlpacaTransport: Send {
    /// Open the underlying data stream and subscribe the `underlying` quote/trade.
    async fn connect_and_subscribe(&mut self, underlying: &str) -> Result<(), TransportGone>;

    /// Await the next underlying event. `Err(_)` means the stream ended — the loop
    /// reconnects.
    async fn receive(&mut self) -> Result<RawStreamEvent, TransportGone>;

    /// Re-poll the composed chain to reconcile drift on (re)connect and on a lag
    /// re-sync (backfill = current state, §5). `None` on a failed/cancelled poll —
    /// the caller keeps prior.
    async fn poll(
        &mut self,
        underlying: &str,
        expiration: &ExpirationDate,
        received: DateTime<Utc>,
    ) -> Option<ComposedChain>;
}

/// The production [`AlpacaTransport`]: the upstream `AlpacaWebSocketClient` /
/// `MarketDataStream` for live underlying events and the adapter's REST re-poll for
/// the backfill. The raw upstream types stay private and never escape.
struct LiveTransport {
    adapter: AlpacaAdapter,
    stream: Option<alpaca_websocket::MarketDataStream>,
}

impl LiveTransport {
    fn new(adapter: AlpacaAdapter) -> Self {
        Self {
            adapter,
            stream: None,
        }
    }
}

#[async_trait]
impl AlpacaTransport for LiveTransport {
    async fn connect_and_subscribe(&mut self, underlying: &str) -> Result<(), TransportGone> {
        // `subscribe_market_data` connects, authenticates, and subscribes, returning
        // a bounded `MarketDataStream`. ChainView owns the OUTER reconnect loop, so a
        // single connect burst is bounded by `WS_CONNECT_RETRIES` (upstream retries
        // only the initial connect).
        let client = self.adapter.ws_client();
        let _connect_probe = client
            .connect_with_reconnect(WS_CONNECT_RETRIES)
            .await
            .map_err(|_| TransportGone)?;
        let subscription = SubscribeMessage {
            trades: Some(vec![underlying.to_owned()]),
            quotes: Some(vec![underlying.to_owned()]),
            bars: None,
            trade_updates: None,
        };
        let stream = client
            .subscribe_market_data(subscription)
            .await
            .map_err(|_| TransportGone)?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn receive(&mut self) -> Result<RawStreamEvent, TransportGone> {
        match self.stream.as_mut() {
            Some(stream) => match stream.next().await {
                Some(event) => Ok(map_stream_event(event)),
                // The stream ended (Disconnected or closed): reconnect.
                None => Err(TransportGone),
            },
            None => Err(TransportGone),
        }
    }

    async fn poll(
        &mut self,
        underlying: &str,
        expiration: &ExpirationDate,
        received: DateTime<Utc>,
    ) -> Option<ComposedChain> {
        let source = LiveDataSource {
            client: self.adapter.http_client().ok()?,
        };
        compose_chain(&source, underlying, expiration, &self.adapter.id, received)
            .await
            .ok()
    }
}

/// Map a raw Alpaca `MarketDataEvent` onto the neutral [`RawStreamEvent`] — the one
/// place a raw upstream event is touched (it never escapes [`LiveTransport`]).
fn map_stream_event(event: MarketDataEvent) -> RawStreamEvent {
    match event {
        MarketDataEvent::Update(MarketDataUpdate::Quote { symbol, quote }) => {
            RawStreamEvent::UnderlyingQuote {
                symbol,
                bid: quote.bid_price,
                ask: quote.ask_price,
                bid_size: u64::from(quote.bid_size),
                ask_size: u64::from(quote.ask_size),
                time: quote.timestamp,
            }
        }
        MarketDataEvent::Update(MarketDataUpdate::Trade { symbol, trade }) => {
            RawStreamEvent::UnderlyingTrade {
                symbol,
                price: trade.price,
                time: trade.timestamp,
            }
        }
        MarketDataEvent::Lagged { .. } => RawStreamEvent::Lagged,
        MarketDataEvent::Update(MarketDataUpdate::Bar { .. })
        | MarketDataEvent::Reconnecting { .. }
        | MarketDataEvent::Reconnected
        | MarketDataEvent::Disconnected { .. } => RawStreamEvent::Ignored,
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

/// The spot pseudo-instrument for the underlying ticker. Because `MarketUpdate` has
/// no underlying-spot variant, the streamed underlying quote is delivered as a
/// [`QuoteUpdate`] for this instrument (native symbol = the underlying ticker, a
/// spot-sentinel [`Positive::ONE`] strike, `Call` style). Folding it into
/// `underlying_price` is the app/store seam's concern; keying it here gives the
/// coalescing bridge one stable per-instrument slot.
///
/// **COLLISION HAZARD (binding on the #46 fold):** `Positive::ONE` is NOT
/// provably disjoint from a real strike — a sub-$5 underlying can list a
/// genuine $1.00 call whose `InstrumentKey` EQUALS this pseudo-instrument's.
/// Today that is harmless only because the update buffers as pending and
/// TTL-expires (no chain row matches on liquid underlyings, and the adapter is
/// gated). The #46 store fold MUST route the underlying spot by an explicit
/// marker (or a new closed-set `MarketUpdate::UnderlyingQuote` variant), and
/// must NEVER fold a spot update into a chain row by strike match — folding by
/// this key would write the underlying's bid/ask onto a real $1 call row.
fn spot_instrument(
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    provider: &ProviderId,
) -> Instrument {
    Instrument {
        key: InstrumentKey {
            underlying: underlying.to_ascii_uppercase(),
            expiration_utc,
            strike: Positive::ONE,
            style: OptionStyle::Call,
        },
        provider: provider.clone(),
        native_symbol: underlying.to_ascii_uppercase(),
        stream_symbol: None,
        spec: ContractSpecFingerprint {
            contract_multiplier: 1,
            settlement: SettlementStyle::Physical,
            exercise: ExerciseStyle::American,
            quote_currency: QUOTE_CURRENCY.to_owned(),
            venue_product_code: underlying.to_ascii_uppercase(),
        },
    }
}

/// The adapter-owned reconnect/resubscribe loop (`docs/03-data-providers.md` §5).
/// Connect + subscribe the underlying, re-poll the composed chain (backfill + venue
/// overlays), drain underlying quotes; on a drop emit `Health(Reconnecting{attempt})`,
/// back off with jitter, and reconnect (which re-polls and resubscribes).
/// Cancellation is observed at every `.await`, so the loop never opens a stream after
/// cancellation and never hot-loops.
async fn run_reconnect_loop<T: AlpacaTransport>(
    mut transport: T,
    id: ProviderId,
    underlying: String,
    expiration_utc: DateTime<Utc>,
    mut sink: MarketUpdateSink,
    cancel: CancellationToken,
) {
    let spot = spot_instrument(&underlying, expiration_utc, &id);
    let mut attempt: u32 = 0;
    loop {
        if cancel.is_cancelled() || sink.is_closed() {
            return;
        }
        let exit = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            exit = connect_stream_once(&mut transport, &id, &underlying, expiration_utc, &spot, &mut sink, &cancel, &mut attempt) => exit,
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
    }
}

/// One connection attempt: connect + subscribe the underlying, emit the composed
/// backfill (Chain + venue overlays), then drain underlying quotes until the stream
/// drops or the subscription is cancelled. `attempt` resets to 0 on a successful
/// (re)connect.
#[allow(clippy::too_many_arguments)]
async fn connect_stream_once<T: AlpacaTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    spot: &Instrument,
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
    attempt: &mut u32,
) -> StreamExit {
    let subscribed = tokio::select! {
        biased;
        () = cancel.cancelled() => return StreamExit::Shutdown,
        result = transport.connect_and_subscribe(underlying) => result,
    };
    if subscribed.is_err() {
        return StreamExit::Reconnect;
    }

    *attempt = 0;
    let live = MarketUpdate::Health(id.clone(), StreamHealth::Live);
    if sink.send(live).await == SinkSend::Closed {
        return StreamExit::Shutdown;
    }

    // Backfill = CURRENT STATE: re-poll the chain and emit the Chain snapshot plus
    // the venue Quote/Greeks overlays (so venue Greeks reach the sidecar, #24/#25).
    if backfill(transport, underlying, expiration_utc, sink, cancel).await == SinkSend::Closed {
        return StreamExit::Shutdown;
    }

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
        // A lag signal re-syncs by re-polling (the bounded-bridge lag policy), never
        // renders a torn view.
        if matches!(event, RawStreamEvent::Lagged) {
            if backfill(transport, underlying, expiration_utc, sink, cancel).await
                == SinkSend::Closed
            {
                return StreamExit::Shutdown;
            }
            continue;
        }
        if route_underlying_event(&event, spot, sink).await == SinkSend::Closed {
            return StreamExit::Shutdown;
        }
    }
}

/// Re-poll the composed chain and emit the reconciled structure as a control-class
/// `Chain` plus the per-leg venue `Quote`/`Greeks` overlays. Cancellation
/// short-circuits; a failed poll keeps prior. Returns [`SinkSend::Closed`] once the
/// consumer is gone.
async fn backfill<T: AlpacaTransport>(
    transport: &mut T,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
) -> SinkSend {
    let expiration = ExpirationDate::DateTime(expiration_utc);
    let composed = tokio::select! {
        biased;
        () = cancel.cancelled() => return SinkSend::Delivered,
        result = transport.poll(underlying, &expiration, now_utc()) => result,
    };
    let Some(composed) = composed else {
        return SinkSend::Delivered;
    };

    let snapshot = MarketUpdate::Chain(chain_snapshot(&composed.fetch, now_utc()));
    let snapshot_sent = tokio::select! {
        biased;
        () = cancel.cancelled() => return SinkSend::Delivered,
        outcome = sink.send(snapshot) => outcome,
    };
    if snapshot_sent == SinkSend::Closed {
        return SinkSend::Closed;
    }
    for overlay in composed.overlays {
        let sent = tokio::select! {
            biased;
            () = cancel.cancelled() => return SinkSend::Delivered,
            outcome = sink.send(overlay) => outcome,
        };
        if sent == SinkSend::Closed {
            return SinkSend::Closed;
        }
    }
    SinkSend::Delivered
}

/// Assemble a streaming-current [`ChainSnapshot`] from a re-polled [`ChainFetch`] —
/// the same `AliasCatalog` carried forward with no re-derivation. The source is
/// [`ChainSource::Merged`] (REST seeds structure + venue Greeks, the underlying
/// stream overlays spot).
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

/// Route one underlying stream event onto the spot pseudo-instrument's
/// [`QuoteUpdate`]. An event whose symbol is not this underlying is a **benign drop**
/// (the unknown-symbol guard). A crossed quote drops the update (keep prior).
async fn route_underlying_event(
    event: &RawStreamEvent,
    spot: &Instrument,
    sink: &mut MarketUpdateSink,
) -> SinkSend {
    let received = now_utc();
    match event {
        RawStreamEvent::UnderlyingQuote {
            symbol,
            bid,
            ask,
            bid_size,
            ask_size,
            time,
        } => {
            if !symbol.eq_ignore_ascii_case(&spot.native_symbol) {
                return SinkSend::Delivered; // unknown-symbol guard
            }
            let Ok(quote) = normalize_quote(Some(*bid), Some(*ask)) else {
                return SinkSend::Delivered; // crossed -> keep prior
            };
            let update = QuoteUpdate {
                instrument: spot.clone(),
                bid: quote.bid,
                ask: quote.ask,
                last: None,
                bid_size: size_to_positive(*bid_size),
                ask_size: size_to_positive(*ask_size),
                event_time: Some(*time),
                received_time: received,
            };
            sink.send(MarketUpdate::Quote(update)).await
        }
        RawStreamEvent::UnderlyingTrade {
            symbol,
            price,
            time,
        } => {
            if !symbol.eq_ignore_ascii_case(&spot.native_symbol) {
                return SinkSend::Delivered;
            }
            let update = QuoteUpdate {
                instrument: spot.clone(),
                bid: None,
                ask: None,
                last: positive_or_drop(*price),
                bid_size: None,
                ask_size: None,
                event_time: Some(*time),
                received_time: received,
            };
            sink.send(MarketUpdate::Quote(update)).await
        }
        RawStreamEvent::Lagged | RawStreamEvent::Ignored => SinkSend::Delivered,
    }
}

// ---------------------------------------------------------------------------
// Reconnect backoff kernel + clocks (pure, injectable jitter).
// ---------------------------------------------------------------------------

/// The jittered exponential backoff delay for reconnect attempt `attempt`
/// (`docs/03-data-providers.md` §5): `delay = min(MAX, BASE * 2^attempt) *
/// (1 + jitter)`, with `BASE = 250 ms`, `MAX = 30 s`, `jitter in [-0.2, 0.2]`. A
/// **pure** kernel: `jitter` is injected, so the mapping is deterministic under test.
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
/// nanoseconds — enough entropy to spread simultaneous reconnects, no RNG dep. It is
/// deliberately outside [`backoff_delay`] so the kernel stays pure under test.
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
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use chrono::Timelike;
    use proptest::prelude::*;
    use tokio::sync::mpsc;

    use super::*;
    use crate::chain::GreeksOrigin;

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
    fn date(s: &str) -> NaiveDate {
        match parse_ymd(s) {
            Ok(d) => d,
            Err(e) => panic!("invalid test date `{s}`: {e}"),
        }
    }

    #[track_caller]
    fn utc_rfc3339(s: &str) -> DateTime<Utc> {
        match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => panic!("invalid rfc3339 `{s}`: {e}"),
        }
    }

    /// A map-backed [`EnvSource`] — the process environment is never mutated.
    struct MapEnv(HashMap<String, String>);

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn creds_env() -> MapEnv {
        let mut env = HashMap::new();
        let _ = env.insert(
            "CHAINVIEW_ALPACA_API_KEY".to_owned(),
            "PKTESTKEY0001".to_owned(),
        );
        let _ = env.insert(
            "CHAINVIEW_ALPACA_API_SECRET".to_owned(),
            "do-not-log-this-secret".to_owned(),
        );
        MapEnv(env)
    }

    #[track_caller]
    fn sample_adapter() -> AlpacaAdapter {
        match AlpacaAdapter::from_env(&creds_env()) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed with both creds present: {e}"),
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

    // === Fixture: recorded contract + snapshot payloads =======================

    const CONTRACTS_PAGE_1: &str =
        include_str!("../../tests/fixtures/alpaca/option_contracts_spy_page1.json");
    const CONTRACTS_PAGE_2: &str =
        include_str!("../../tests/fixtures/alpaca/option_contracts_spy_page2.json");
    const SNAPSHOTS: &str = include_str!("../../tests/fixtures/alpaca/option_snapshots_spy.json");

    #[track_caller]
    fn contracts_from(json: &str) -> ContractPage {
        let response: alpaca_http::endpoints::OptionContractsResponse =
            match serde_json::from_str(json) {
                Ok(r) => r,
                Err(e) => panic!("contracts fixture must deserialize: {e}"),
            };
        ContractPage {
            contracts: response
                .option_contracts
                .into_iter()
                .map(map_contract)
                .collect(),
            next_page_token: response.next_page_token,
        }
    }

    #[track_caller]
    fn snapshots_from(json: &str) -> HashMap<String, RawSnapshot> {
        let response: alpaca_http::endpoints::OptionSnapshotsResponse =
            match serde_json::from_str(json) {
                Ok(r) => r,
                Err(e) => panic!("snapshots fixture must deserialize: {e}"),
            };
        response
            .snapshots
            .into_iter()
            .map(|(symbol, snapshot)| (symbol, map_snapshot(snapshot)))
            .collect()
    }

    /// A scripted [`ChainDataSource`] serving recorded pages + snapshots, recording
    /// each discovery page token and each hydration batch it is asked for.
    struct MockDataSource {
        pages: Vec<ContractPage>,
        snapshots: HashMap<String, RawSnapshot>,
        page_tokens: Arc<StdMutex<Vec<Option<String>>>>,
        batches: Arc<StdMutex<Vec<Vec<String>>>>,
    }

    impl MockDataSource {
        fn from_fixtures() -> Self {
            Self {
                pages: vec![
                    contracts_from(CONTRACTS_PAGE_1),
                    contracts_from(CONTRACTS_PAGE_2),
                ],
                snapshots: snapshots_from(SNAPSHOTS),
                page_tokens: Arc::new(StdMutex::new(Vec::new())),
                batches: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl ChainDataSource for MockDataSource {
        async fn discover_page(
            &self,
            _underlying: &str,
            _expiration_date: &str,
            page_token: Option<String>,
        ) -> Result<ContractPage, ProviderError> {
            if let Ok(mut log) = self.page_tokens.lock() {
                log.push(page_token.clone());
            }
            // Page 0 when no token; otherwise resolve by the token the previous page
            // handed out ("page2" -> index 1).
            let index = match page_token.as_deref() {
                None => 0,
                Some("page2") => 1,
                Some(_) => return Ok(ContractPage::default()),
            };
            Ok(self.pages.get(index).cloned().unwrap_or_default())
        }

        async fn hydrate_batch(
            &self,
            symbols: &[String],
        ) -> Result<HashMap<String, RawSnapshot>, ProviderError> {
            if let Ok(mut log) = self.batches.lock() {
                log.push(symbols.to_vec());
            }
            let mut out = HashMap::new();
            for symbol in symbols {
                if let Some(snapshot) = self.snapshots.get(symbol) {
                    let _ = out.insert(symbol.clone(), snapshot.clone());
                }
            }
            Ok(out)
        }
    }

    #[track_caller]
    fn expiry() -> ExpirationDate {
        // The fixture's sole expiration is 2026-03-20 (a Friday inside EDT).
        ExpirationDate::DateTime(utc_rfc3339("2026-03-20T20:00:00+00:00"))
    }

    #[track_caller]
    fn compose_fixture() -> ComposedChain {
        let source = MockDataSource::from_fixtures();
        let received = utc_rfc3339("2026-03-19T15:00:00+00:00");
        match block(compose_chain(
            &source,
            "spy",
            &expiry(),
            &pid("alpaca"),
            received,
        )) {
            Ok(composed) => composed,
            Err(e) => panic!("compose_chain should succeed for the fixtures, got: {e}"),
        }
    }

    /// The composed fixture, awaited directly — for use inside an async test where a
    /// nested `block_on` runtime is illegal.
    async fn compose_fixture_async() -> ComposedChain {
        let source = MockDataSource::from_fixtures();
        let received = utc_rfc3339("2026-03-19T15:00:00+00:00");
        match compose_chain(&source, "spy", &expiry(), &pid("alpaca"), received).await {
            Ok(composed) => composed,
            Err(e) => panic!("compose_chain should succeed for the fixtures, got: {e}"),
        }
    }

    // === Identity + capabilities ==============================================

    #[test]
    fn test_alpaca_id_is_valid_and_reserved() {
        let id = alpaca_provider_id();
        assert_eq!(id.as_str(), "alpaca");
        assert!(id.is_reserved());
        assert!(ProviderId::new(ALPACA_ID).is_ok());
    }

    #[test]
    fn test_alpaca_capabilities_match_section_8_row() {
        let caps = alpaca_capabilities();
        assert_eq!(caps.chain, ChainCapability::Native);
        assert!(
            !caps.depth,
            "option depth is crypto-only, out of the v1 product"
        );
        assert_eq!(caps.greeks, GreeksCapability::Provided);
        // The whole point of the three-dimensional split: no option stream, but an
        // underlying stream and a polled chain.
        assert_eq!(caps.option_stream, OptionStreamCapability::None);
        assert!(caps.underlying_stream);
        assert_eq!(
            caps.chain_poll,
            ChainPollCapability::Poll {
                interval_hint_secs: REFRESH_HINT_SECS
            }
        );
        assert!(!caps.trades_tape);
        assert_eq!(caps.auth, AuthKind::KeySecret);
    }

    #[test]
    fn test_adapter_reports_capabilities_and_id_via_trait() {
        let adapter: Box<dyn Provider> = Box::new(sample_adapter());
        assert_eq!(adapter.id().as_str(), "alpaca");
        assert_eq!(adapter.capabilities().chain, ChainCapability::Native);
        assert_eq!(
            adapter.capabilities().option_stream,
            OptionStreamCapability::None
        );
    }

    // === Credentials: env-only, never logged ==================================

    #[test]
    fn test_credentials_never_appear_in_debug_of_adapter_secrets() {
        let adapter = sample_adapter();
        let rendered = format!("{:?}", adapter.api_secret);
        assert!(!rendered.contains("do-not-log-this-secret"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn test_from_env_reads_chainview_namespace_only() {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_ALPACA_API_KEY".to_owned(), "key-a".to_owned());
        let _ = env.insert(
            "CHAINVIEW_ALPACA_API_SECRET".to_owned(),
            "secret-b".to_owned(),
        );
        // A foreign-namespace value must be ignored.
        let _ = env.insert("ALPACA_API_KEY".to_owned(), "foreign".to_owned());
        let adapter = match AlpacaAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed: {e}"),
        };
        assert_eq!(adapter.api_key.expose(), "key-a");
        assert_eq!(adapter.api_secret.expose(), "secret-b");
        assert_eq!(adapter.environment, AlpacaEnvironment::Paper);
    }

    #[test]
    fn test_from_env_environment_selector_defaults_paper_and_parses_live() {
        // Absent -> Paper.
        assert_eq!(sample_adapter().environment, AlpacaEnvironment::Paper);
        // Explicit live.
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_ALPACA_API_KEY".to_owned(), "k".to_owned());
        let _ = env.insert("CHAINVIEW_ALPACA_API_SECRET".to_owned(), "s".to_owned());
        let _ = env.insert("CHAINVIEW_ALPACA_ENVIRONMENT".to_owned(), "LIVE".to_owned());
        match AlpacaAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => assert_eq!(adapter.environment, AlpacaEnvironment::Live),
            Err(e) => panic!("from_env should succeed: {e}"),
        }
    }

    #[test]
    fn test_from_env_missing_credential_is_error() {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_ALPACA_API_KEY".to_owned(), "key-a".to_owned());
        // Secret absent.
        match AlpacaAdapter::from_env(&MapEnv(env)) {
            Err(crate::error::ConfigError::MissingCredential(id)) => {
                assert_eq!(id.as_str(), "alpaca");
            }
            Err(other) => panic!("expected MissingCredential, got: {other}"),
            Ok(_) => panic!("expected MissingCredential, got Ok"),
        }
    }

    // === Expiry: 16:00 America/New_York -> UTC, DST-aware =====================

    #[test]
    fn test_expiry_edt_resolves_to_2000_utc() {
        match expiry_to_utc("2026-03-20") {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-03-20T20:00:00+00:00"),
            Err(e) => panic!("EDT expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_est_resolves_to_2100_utc() {
        match expiry_to_utc("2026-11-20") {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-11-20T21:00:00+00:00"),
            Err(e) => panic!("EST expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_dst_boundaries() {
        // Second Sunday of March 2026 = 2026-03-08 (EDT at 16:00).
        assert!(is_us_eastern_dst(date("2026-03-08")));
        assert!(!is_us_eastern_dst(date("2026-03-07")));
        // First Sunday of November 2026 = 2026-11-01 (EST at 16:00).
        assert!(!is_us_eastern_dst(date("2026-11-01")));
        assert!(is_us_eastern_dst(date("2026-10-31")));
        assert_eq!(
            nth_weekday_of_month(2026, 3, Weekday::Sun, 2),
            Some(date("2026-03-08"))
        );
    }

    #[test]
    fn test_expiry_summer_is_not_fixed_2100() {
        match expiry_to_utc("2026-07-17") {
            Ok(utc) => {
                assert_eq!(utc.to_rfc3339(), "2026-07-17T20:00:00+00:00");
                assert_ne!(utc.to_rfc3339(), "2026-07-17T21:00:00+00:00");
            }
            Err(e) => panic!("summer expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_unparseable_is_rejected() {
        assert_eq!(
            expiry_to_utc("not-a-date"),
            Err(NormalizeKind::UnparseableExpiry)
        );
        assert_eq!(
            expiry_to_utc("2026-13-01"),
            Err(NormalizeKind::UnparseableExpiry)
        );
        assert_eq!(
            expiry_to_utc("2026-03"),
            Err(NormalizeKind::UnparseableExpiry)
        );
    }

    #[test]
    fn test_format_ymd_round_trips() {
        assert_eq!(format_ymd(date("2026-03-20")), "2026-03-20");
        assert_eq!(format_ymd(date("2026-11-01")), "2026-11-01");
    }

    // === Numeric normalization at the seam ====================================

    #[test]
    fn test_strike_positive_parses_and_rejects() {
        match strike_positive("500") {
            Ok(strike) => assert_eq!(strike, pos(500.0)),
            Err(e) => panic!("500 should parse, got: {e}"),
        }
        match strike_positive("192.5") {
            Ok(strike) => assert_eq!(strike, pos(192.5)),
            Err(e) => panic!("192.5 should parse, got: {e}"),
        }
        assert_eq!(
            strike_positive("0"),
            Err(NormalizeKind::OutOfRange("strike"))
        );
        assert_eq!(
            strike_positive("-5"),
            Err(NormalizeKind::OutOfRange("strike"))
        );
        assert_eq!(
            strike_positive("abc"),
            Err(NormalizeKind::OutOfRange("strike"))
        );
    }

    #[test]
    fn test_iv_is_decimal_no_division() {
        // Alpaca IV is already a decimal fraction, unlike Deribit's percentage form.
        assert_eq!(iv_or_drop(0.35), Some(pos(0.35)));
        assert_eq!(iv_or_drop(0.0), Some(Positive::ZERO));
        assert_eq!(iv_or_drop(f64::NAN), None);
        assert_eq!(iv_or_drop(-0.1), None);
    }

    #[test]
    fn test_greek_keeps_negative_drops_non_finite() {
        match greek_or_drop(Some(-0.45)) {
            Some(delta) => assert_eq!(delta, Decimal::new(-45, 2)),
            None => panic!("a negative Greek must be kept"),
        }
        assert_eq!(greek_or_drop(Some(f64::INFINITY)), None);
        assert_eq!(greek_or_drop(None), None);
    }

    #[test]
    fn test_normalize_quote_rules() {
        // Zero bid valid.
        match normalize_quote(Some(0.0), Some(1.0)) {
            Ok(q) => {
                assert_eq!(q.bid, Some(Positive::ZERO));
                assert_eq!(q.ask, Some(pos(1.0)));
            }
            Err(e) => panic!("zero bid valid, got: {e}"),
        }
        // Crossed rejects the whole update.
        assert_eq!(
            normalize_quote(Some(5.0), Some(3.0)),
            Err(NormalizeKind::OutOfRange("ask"))
        );
        assert_eq!(
            normalize_quote(Some(5.0), Some(0.0)),
            Err(NormalizeKind::OutOfRange("ask"))
        );
        // A negative field drops only that field.
        match normalize_quote(Some(-1.0), Some(2.0)) {
            Ok(q) => {
                assert_eq!(q.bid, None);
                assert_eq!(q.ask, Some(pos(2.0)));
            }
            Err(e) => panic!("negative bid drops only that field, got: {e}"),
        }
    }

    #[test]
    fn test_multiplier_and_open_interest_of() {
        assert_eq!(multiplier_of(Some("100")), 100);
        assert_eq!(multiplier_of(Some("0")), DEFAULT_SHARES_PER_CONTRACT);
        assert_eq!(multiplier_of(None), DEFAULT_SHARES_PER_CONTRACT);
        assert_eq!(multiplier_of(Some("junk")), DEFAULT_SHARES_PER_CONTRACT);
        assert_eq!(open_interest_of(Some("1234")), Some(1234));
        assert_eq!(open_interest_of(Some("")), None);
        assert_eq!(open_interest_of(None), None);
    }

    // === Composed chain: multi-page discovery + hydration + atomic publish =====

    #[test]
    fn test_compose_walks_all_pages_and_publishes_discovered_set() {
        let source = MockDataSource::from_fixtures();
        let received = utc_rfc3339("2026-03-19T15:00:00+00:00");
        let composed = match block(compose_chain(
            &source,
            "spy",
            &expiry(),
            &pid("alpaca"),
            received,
        )) {
            Ok(c) => c,
            Err(e) => panic!("compose should succeed, got: {e}"),
        };
        // Both pages walked: the second discovery used the "page2" token.
        match source.page_tokens.lock() {
            Ok(tokens) => {
                assert_eq!(tokens.len(), 2, "two discovery pages walked");
                assert_eq!(tokens.first(), Some(&None));
                assert_eq!(tokens.get(1), Some(&Some("page2".to_owned())));
            }
            Err(_) => panic!("page-token log poisoned"),
        }
        // The published strike set equals the discovered set: page1 has 500/510,
        // page2 has 520 -> three strikes.
        let strikes: Vec<Positive> = composed
            .fetch
            .chain
            .options
            .iter()
            .map(|o| o.strike_price)
            .collect();
        assert_eq!(strikes, vec![pos(500.0), pos(510.0), pos(520.0)]);
        // Four discovered contracts on page1 (2 strikes x call/put) + 2 on page2
        // (1 strike x call/put) = 6 alias legs.
        assert_eq!(composed.fetch.aliases.len(), 6);
    }

    #[test]
    fn test_compose_hydrates_in_bounded_batches() {
        let source = MockDataSource::from_fixtures();
        let received = utc_rfc3339("2026-03-19T15:00:00+00:00");
        let _ = block(compose_chain(
            &source,
            "spy",
            &expiry(),
            &pid("alpaca"),
            received,
        ));
        match source.batches.lock() {
            Ok(batches) => {
                assert!(!batches.is_empty(), "at least one hydration batch");
                for batch in batches.iter() {
                    assert!(
                        batch.len() <= MAX_SYMBOLS_PER_BATCH,
                        "each batch is bounded by MAX_SYMBOLS_PER_BATCH"
                    );
                }
                let total: usize = batches.iter().map(Vec::len).sum();
                assert_eq!(total, 6, "every discovered OCC symbol is hydrated");
            }
            Err(_) => panic!("batch log poisoned"),
        }
    }

    #[test]
    fn test_compose_publishes_full_strike_set_even_with_missing_snapshots() {
        // A source that discovers every contract but hydrates NONE: the published
        // structure still equals the discovered set (atomic, no partial), with the
        // rows simply carrying no quotes/Greeks.
        struct NoHydrate(MockDataSource);
        #[async_trait]
        impl ChainDataSource for NoHydrate {
            async fn discover_page(
                &self,
                u: &str,
                e: &str,
                t: Option<String>,
            ) -> Result<ContractPage, ProviderError> {
                self.0.discover_page(u, e, t).await
            }
            async fn hydrate_batch(
                &self,
                _symbols: &[String],
            ) -> Result<HashMap<String, RawSnapshot>, ProviderError> {
                Ok(HashMap::new())
            }
        }
        let source = NoHydrate(MockDataSource::from_fixtures());
        let received = utc_rfc3339("2026-03-19T15:00:00+00:00");
        let composed = match block(compose_chain(
            &source,
            "spy",
            &expiry(),
            &pid("alpaca"),
            received,
        )) {
            Ok(c) => c,
            Err(e) => panic!("compose should still succeed, got: {e}"),
        };
        let strikes: Vec<Positive> = composed
            .fetch
            .chain
            .options
            .iter()
            .map(|o| o.strike_price)
            .collect();
        assert_eq!(strikes, vec![pos(500.0), pos(510.0), pos(520.0)]);
        // No snapshot -> no venue overlays emitted.
        assert!(composed.overlays.is_empty(), "no snapshots -> no overlays");
    }

    #[test]
    fn test_compose_empty_discovery_is_no_chain() {
        struct Empty;
        #[async_trait]
        impl ChainDataSource for Empty {
            async fn discover_page(
                &self,
                _u: &str,
                _e: &str,
                _t: Option<String>,
            ) -> Result<ContractPage, ProviderError> {
                Ok(ContractPage::default())
            }
            async fn hydrate_batch(
                &self,
                _symbols: &[String],
            ) -> Result<HashMap<String, RawSnapshot>, ProviderError> {
                Ok(HashMap::new())
            }
        }
        let received = utc_rfc3339("2026-03-19T15:00:00+00:00");
        match block(compose_chain(
            &Empty,
            "spy",
            &expiry(),
            &pid("alpaca"),
            received,
        )) {
            Err(ProviderError::NoChain { underlying, .. }) => assert_eq!(underlying, "SPY"),
            other => panic!("expected NoChain, got {other:?}"),
        }
    }

    #[test]
    fn test_composed_chain_carries_venue_greeks_and_iv() {
        let composed = compose_fixture();
        // The EDT expiry -> 20:00 UTC.
        assert_eq!(
            composed.fetch.expiry_source.expiration_utc.to_rfc3339(),
            "2026-03-20T20:00:00+00:00"
        );
        // The 500 call row carries venue delta/gamma/iv from the snapshot.
        let row = composed
            .fetch
            .chain
            .options
            .iter()
            .find(|o| o.strike_price == pos(500.0));
        match row {
            Some(row) => {
                assert_eq!(row.implied_volatility, pos(0.2841));
                assert_eq!(row.delta_call, Some(Decimal::new(55, 2)));
            }
            None => panic!("the 500 strike row is missing"),
        }
    }

    // === Greeks/IV venue tagging (the #24/#25 seam) ===========================

    #[test]
    fn test_snapshot_overlays_are_tagged_provider_origin() {
        let composed = compose_fixture();
        // Every emitted Greeks overlay is venue-sourced, never computed-locally, and
        // preserves the venue IV as-is (Alpaca IV is already decimal, no /100).
        let mut saw_greeks = false;
        for update in &composed.overlays {
            if let MarketUpdate::Greeks(row) = update {
                assert_eq!(
                    row.origin,
                    GreeksOrigin::Provider,
                    "snapshot Greeks are venue-supplied, never ComputedLocally"
                );
                saw_greeks = true;
            }
        }
        assert!(
            saw_greeks,
            "the fixture snapshots produce venue Greeks overlays"
        );
        // The 500 call's overlay carries the venue IV as-is.
        let iv = composed.overlays.iter().find_map(|u| match u {
            MarketUpdate::Greeks(row)
                if row.instrument.key.strike == pos(500.0)
                    && row.instrument.key.style == OptionStyle::Call =>
            {
                row.iv
            }
            _ => None,
        });
        assert_eq!(iv, Some(pos(0.2841)), "venue IV survives as-is, no /100");
    }

    // === Underlying stream: bounded-bridge burst coalescing ===================

    #[tokio::test]
    async fn test_underlying_burst_coalesces_last_value_wins_per_instrument() {
        // A cap-1 coalesced channel, never drained beyond the first: a burst of
        // underlying quotes for one spot instrument coalesces last-value-wins without
        // unbounded growth (the Alpaca unbounded upstream path specifically).
        let spot = spot_instrument(
            "SPY",
            utc_rfc3339("2026-03-20T20:00:00+00:00"),
            &pid("alpaca"),
        );
        let (tx_control, _rx_control) = mpsc::channel::<MarketUpdate>(8);
        let (tx_coalesced, mut rx_coalesced) = mpsc::channel::<MarketUpdate>(1);
        let mut sink = MarketUpdateSink::new(tx_control, tx_coalesced);

        for round in 0..500u32 {
            let bid = 100.0 + f64::from(round) * 0.01;
            let event = RawStreamEvent::UnderlyingQuote {
                symbol: "SPY".to_owned(),
                bid,
                ask: bid + 0.02,
                bid_size: 3,
                ask_size: 4,
                time: utc_rfc3339("2026-03-20T15:00:00+00:00"),
            };
            assert_eq!(
                route_underlying_event(&event, &spot, &mut sink).await,
                SinkSend::Delivered
            );
        }
        // Only a bounded number of updates ever materialized on the channel; the
        // freshest is preserved by the producer staging on the next flush.
        let first = rx_coalesced.try_recv();
        assert!(first.is_ok(), "at least the first quote is on the channel");
        // A final send flushes the freshest staged value.
        let last_bid = 100.0 + f64::from(500u32) * 0.01;
        let final_event = RawStreamEvent::UnderlyingQuote {
            symbol: "SPY".to_owned(),
            bid: last_bid,
            ask: last_bid + 0.02,
            bid_size: 3,
            ask_size: 4,
            time: utc_rfc3339("2026-03-20T15:00:00+00:00"),
        };
        let _ = route_underlying_event(&final_event, &spot, &mut sink).await;
        // Whatever remains is a bounded handful, never the full 500-burst.
        let remaining = drain(&mut rx_coalesced);
        assert!(
            remaining.len() <= 2,
            "the burst coalesced, never grew unbounded"
        );
    }

    #[tokio::test]
    async fn test_underlying_quote_routes_to_spot_instrument() {
        let spot = spot_instrument(
            "SPY",
            utc_rfc3339("2026-03-20T20:00:00+00:00"),
            &pid("alpaca"),
        );
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);
        let event = RawStreamEvent::UnderlyingQuote {
            symbol: "spy".to_owned(), // case-insensitive match
            bid: 500.10,
            ask: 500.20,
            bid_size: 10,
            ask_size: 20,
            time: utc_rfc3339("2026-03-20T15:30:00+00:00"),
        };
        assert_eq!(
            route_underlying_event(&event, &spot, &mut sink).await,
            SinkSend::Delivered
        );
        match rx_coalesced.try_recv() {
            Ok(MarketUpdate::Quote(q)) => {
                assert_eq!(q.bid, Some(pos(500.10)));
                assert_eq!(q.ask, Some(pos(500.20)));
                assert_eq!(q.instrument.native_symbol, "SPY");
                assert!(q.event_time.is_some());
            }
            other => panic!("expected a routed spot Quote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_underlying_unknown_symbol_is_benign_drop() {
        let spot = spot_instrument(
            "SPY",
            utc_rfc3339("2026-03-20T20:00:00+00:00"),
            &pid("alpaca"),
        );
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);
        let event = RawStreamEvent::UnderlyingQuote {
            symbol: "QQQ".to_owned(),
            bid: 1.0,
            ask: 1.1,
            bid_size: 1,
            ask_size: 1,
            time: utc_rfc3339("2026-03-20T15:30:00+00:00"),
        };
        assert_eq!(
            route_underlying_event(&event, &spot, &mut sink).await,
            SinkSend::Delivered
        );
        assert!(
            rx_coalesced.try_recv().is_err(),
            "an unknown symbol publishes nothing"
        );
    }

    #[tokio::test]
    async fn test_underlying_crossed_quote_is_benign_drop() {
        let spot = spot_instrument(
            "SPY",
            utc_rfc3339("2026-03-20T20:00:00+00:00"),
            &pid("alpaca"),
        );
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);
        let event = RawStreamEvent::UnderlyingQuote {
            symbol: "SPY".to_owned(),
            bid: 2.0,
            ask: 1.0, // crossed
            bid_size: 1,
            ask_size: 1,
            time: utc_rfc3339("2026-03-20T15:30:00+00:00"),
        };
        assert_eq!(
            route_underlying_event(&event, &spot, &mut sink).await,
            SinkSend::Delivered
        );
        assert!(
            rx_coalesced.try_recv().is_err(),
            "a crossed quote publishes nothing"
        );
    }

    // === Reconnect loop over a MOCK transport (no socket, no wall clock) =======

    /// A scripted mock stream transport: `attempts[n]` is the event list for
    /// connection attempt `n`, drained then a drop; once every attempt is exhausted
    /// it cancels the token so the loop stops. It records each connect and can serve
    /// a composed backfill on `poll`.
    struct MockTransport {
        attempts: Vec<Vec<RawStreamEvent>>,
        attempt_idx: usize,
        cursor: usize,
        backfill: Option<ComposedChain>,
        connects: Arc<StdMutex<u32>>,
        polls: Arc<StdMutex<u32>>,
        cancel: CancellationToken,
    }

    #[async_trait]
    impl AlpacaTransport for MockTransport {
        async fn connect_and_subscribe(&mut self, _underlying: &str) -> Result<(), TransportGone> {
            if let Ok(mut count) = self.connects.lock() {
                *count += 1;
            }
            self.cursor = 0;
            Ok(())
        }

        async fn receive(&mut self) -> Result<RawStreamEvent, TransportGone> {
            let Some(events) = self.attempts.get(self.attempt_idx) else {
                self.cancel.cancel();
                return Err(TransportGone);
            };
            if let Some(event) = events.get(self.cursor) {
                self.cursor = self.cursor.saturating_add(1);
                return Ok(event.clone());
            }
            self.attempt_idx = self.attempt_idx.saturating_add(1);
            self.cursor = 0;
            if self.attempt_idx >= self.attempts.len() {
                self.cancel.cancel();
            }
            Err(TransportGone)
        }

        async fn poll(
            &mut self,
            _underlying: &str,
            _expiration: &ExpirationDate,
            _received: DateTime<Utc>,
        ) -> Option<ComposedChain> {
            if let Ok(mut count) = self.polls.lock() {
                *count += 1;
            }
            self.backfill.as_ref().map(|composed| ComposedChain {
                fetch: composed.fetch.clone(),
                overlays: composed.overlays.clone(),
            })
        }
    }

    /// A transport whose `receive` never resolves, so only cancellation stops the
    /// loop — the shutdown test.
    struct PendingTransport;

    #[async_trait]
    impl AlpacaTransport for PendingTransport {
        async fn connect_and_subscribe(&mut self, _underlying: &str) -> Result<(), TransportGone> {
            Ok(())
        }
        async fn receive(&mut self) -> Result<RawStreamEvent, TransportGone> {
            std::future::pending::<()>().await;
            Err(TransportGone)
        }
        async fn poll(
            &mut self,
            _underlying: &str,
            _expiration: &ExpirationDate,
            _received: DateTime<Utc>,
        ) -> Option<ComposedChain> {
            None
        }
    }

    fn quote_event(bid: f64) -> RawStreamEvent {
        RawStreamEvent::UnderlyingQuote {
            symbol: "SPY".to_owned(),
            bid,
            ask: bid + 0.02,
            bid_size: 5,
            ask_size: 5,
            time: utc_rfc3339("2026-03-20T15:00:00+00:00"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_reconnect_loop_resubscribes_and_backfills() {
        // Attempt 0 emits a spot quote then drops; attempt 1 emits another then the
        // loop stops. Each connect re-polls the chain (backfill) and re-subscribes.
        let cancel = CancellationToken::new();
        let connects = Arc::new(StdMutex::new(0));
        let polls = Arc::new(StdMutex::new(0));
        let transport = MockTransport {
            attempts: vec![vec![quote_event(500.0)], vec![quote_event(501.0)]],
            attempt_idx: 0,
            cursor: 0,
            backfill: Some(compose_fixture_async().await),
            connects: Arc::clone(&connects),
            polls: Arc::clone(&polls),
            cancel: cancel.clone(),
        };
        let (sink, mut rx_control, mut rx_coalesced) = test_sink(64);

        run_reconnect_loop(
            transport,
            pid("alpaca"),
            "SPY".to_owned(),
            utc_rfc3339("2026-03-20T20:00:00+00:00"),
            sink,
            cancel,
        )
        .await;

        assert_eq!(
            *connects.lock().unwrap_or_else(|e| e.into_inner()),
            2,
            "connected twice"
        );
        assert!(
            *polls.lock().unwrap_or_else(|e| e.into_inner()) >= 2,
            "each connect re-polled the chain (backfill)"
        );

        // The control channel carried a Chain backfill and the reconnect health.
        let control = drain(&mut rx_control);
        assert!(
            control.iter().any(|u| matches!(u, MarketUpdate::Chain(_))),
            "a Chain backfill was emitted"
        );
        assert!(
            control.iter().any(|u| matches!(
                u,
                MarketUpdate::Health(_, StreamHealth::Reconnecting { .. })
            )),
            "a Reconnecting health was emitted"
        );
        assert!(
            control
                .iter()
                .any(|u| matches!(u, MarketUpdate::Health(_, StreamHealth::Live))),
            "a Live health was emitted"
        );

        // The coalesced channel carried the spot quotes + venue overlays.
        let coalesced = drain(&mut rx_coalesced);
        assert!(
            coalesced
                .iter()
                .any(|u| matches!(u, MarketUpdate::Greeks(_))),
            "venue Greeks overlays reached the coalesced channel"
        );
    }

    #[tokio::test]
    async fn test_reconnect_loop_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let (sink, _rx_control, _rx_coalesced) = test_sink(8);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            PendingTransport,
            pid("alpaca"),
            "SPY".to_owned(),
            utc_rfc3339("2026-03-20T20:00:00+00:00"),
            sink,
            loop_cancel,
        ));
        tokio::task::yield_now().await;
        cancel.cancel();
        match handle.await {
            Ok(()) => {}
            Err(e) => panic!("the loop task should join cleanly on cancel, got: {e}"),
        }
    }

    // === Property: normalization is total (never a panic) =====================

    proptest! {
        /// `expiry_to_utc` is TOTAL: any string yields `Ok` or `UnparseableExpiry`,
        /// never a panic; every valid US-equity date resolves to exactly the 20:00
        /// (EDT) or 21:00 (EST) UTC close (contributes to `normalize_total`).
        #[test]
        fn prop_expiry_to_utc_total_and_dst_shape(
            year in 2000i32..2100,
            month in 1u32..=12,
            day in 1u32..=28,
            junk in "\\PC{0,16}",
        ) {
            let _ = expiry_to_utc(&junk);
            let date_str = format!("{year:04}-{month:02}-{day:02}");
            match expiry_to_utc(&date_str) {
                Ok(utc) => {
                    let hour = utc.hour();
                    prop_assert!(hour == 20 || hour == 21, "unexpected UTC hour {hour} for {date_str}");
                }
                Err(kind) => prop_assert_eq!(kind, NormalizeKind::UnparseableExpiry),
            }
        }

        /// `strike_positive` is TOTAL over any string: a positive numeric yields a
        /// non-zero `Positive`, everything else yields `OutOfRange`, never a panic
        /// (contributes to `normalize_total`).
        #[test]
        fn prop_strike_positive_total(raw in "\\PC{0,12}") {
            match strike_positive(&raw) {
                Ok(strike) => prop_assert!(strike > Positive::ZERO),
                Err(kind) => prop_assert_eq!(kind, NormalizeKind::OutOfRange("strike")),
            }
        }

        /// `normalize_quote` is TOTAL over any bid/ask pair: it is `Ok` (a per-field
        /// checked pair) or a crossed `OutOfRange("ask")`, never a panic
        /// (contributes to `normalize_total` / `normalize_rejects_unknown`).
        #[test]
        fn prop_normalize_quote_total(bid in -1.0e6f64..1.0e6, ask in -1.0e6f64..1.0e6) {
            match normalize_quote(Some(bid), Some(ask)) {
                Ok(quote) => {
                    if let (Some(b), Some(a)) = (quote.bid, quote.ask) {
                        prop_assert!(a >= b, "an accepted quote is never crossed");
                    }
                }
                Err(kind) => prop_assert_eq!(kind, NormalizeKind::OutOfRange("ask")),
            }
        }
    }
}
