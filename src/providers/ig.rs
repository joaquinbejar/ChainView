//! The IG adapter — the navigation-assembled, local-Greeks poll->stream provider
//! (issue #39, `docs/03-data-providers.md` §7.4). Behind the DISABLED-by-default
//! `ig` Cargo feature (a **dependency-weight** gate, [ADR-0013], not a
//! credential-logging security gate), so under the feature it is a **real
//! built-in** registered by [`with_builtins`](crate::ChainViewAppBuilder::with_builtins)
//! when its `CHAINVIEW_IG_*` credentials are configured.
//!
//! IG lists dated options as **epics** discoverable through market navigation /
//! search — there is no strike/expiry chain model — so the adapter is universal
//! responsibilities **1 + 2** at once (`docs/03-data-providers.md` §7.4): it
//! **assembles** the chain from the navigation/search markets
//! ([`ChainCapability::Partial`]) and, because IG supplies **no Greeks/IV**, it
//! declares [`GreeksCapability::ComputedLocally`] — the store's local engine
//! (`src/chain/greeks.rs`, issue #24) fills delta/gamma/theta/vega/rho + IV from
//! the assembled two-sided quotes and tags every leg
//! [`GreeksOrigin::ComputedLocally`], so the trader sees ChainView's Black-Scholes,
//! never a venue number IG does not have (`docs/01-domain-model.md` §7). IG is the
//! forcing function for that v0.2 engine.
//!
//! # Auth is injected programmatically (env-only, no dotenv, no foreign namespace)
//!
//! [`from_env`](IgAdapter::from_env) reads the ChainView-namespaced
//! `CHAINVIEW_IG_USERNAME` / `_PASSWORD` / `_API_KEY` (and the optional non-secret
//! `CHAINVIEW_IG_ACCOUNT_ID` / `CHAINVIEW_IG_ENVIRONMENT`) and builds the upstream
//! client via `Config::from_credentials(Credentials)` + `Client::with_config`
//! (`docs/03-data-providers.md` §11.3). It NEVER calls `Config::new` /
//! `Config::default` / `Client::try_new` (the only upstream paths that load a
//! `.env` via `dotenv` and read the foreign `IG_*` namespace), and never
//! `std::env::set_var` (edition-2024 `unsafe`; `#![forbid(unsafe_code)]` holds).
//! The credential is exposed only at the single client hand-off site
//! ([`credentials`](IgAdapter::credentials)) and is never logged or echoed in a
//! [`ProviderError`]. IG requires auth for everything — there is no public path —
//! and session v2 (CST / X-SECURITY-TOKEN) or v3 (OAuth) auto-refresh is handled by
//! the upstream client transparently on the first API call.
//!
//! # Streaming is env-free too — `StreamerClient::with_client`, NOT `DynamicMarketStreamer`
//!
//! The Lightstreamer overlay is opened over
//! [`StreamerClient::with_client(&client)`](ig_client::application::client::StreamerClient::with_client),
//! which REUSES the injected client's session. It deliberately does **not** use
//! `DynamicMarketStreamer`, whose `start_internal` builds its own
//! `StreamerClient::new()` -> `Client::try_new()` -> `Config::default()` — the
//! `.env`/`IG_*` path — which would break the env-only credential rule. Both are
//! `ig-client` streaming types; ChainView never reimplements the Lightstreamer
//! protocol (`CLAUDE.md` "Key Decisions"). Every raw `PriceData` is normalized to a
//! per-epic [`QuoteUpdate`] here and no raw `ig-client` DTO crosses the port.
//!
//! # Expiry -> one absolute UTC instant at the seam
//!
//! IG hands a date-only `expiry` (`"18-JUL-26"`) on a navigation market and an
//! authoritative timestamped `lastDealingDate` on a market-details instrument. The
//! **timestamped field wins** when present; a date-only value resolves through the
//! market's session close in a supported IANA zone (a bounded, DST-aware set — no
//! timezone database is pulled); an ambiguous / unparseable / unknown-zone expiry
//! is a typed [`ProviderError::Normalize`] with
//! [`NormalizeKind::UnparseableExpiry`], never a silently-keyed row
//! (`docs/03-data-providers.md` §3, `docs/01-domain-model.md` §4).
//!
//! # Numerics at the f64 seam
//!
//! IG prices are `f64`. Every bid/offer/strike is checked at this seam into a
//! [`Positive`] (rejecting `NaN`/`Inf`/negative) before it enters the chain; a
//! crossed quote drops only the quote. `f64` never flows past `src/providers/*`
//! (`CLAUDE.md` governance deviation 2).
//!
//! # Reconnect + two update classes (`docs/03-data-providers.md` §5, [ADR-0009])
//!
//! The reconnect/resubscribe loop is **ChainView's**, driven behind the
//! [`SubscriptionHandle`]: on a dropped stream it emits `Health(Reconnecting)`,
//! backs off with jittered exponential backoff (respecting the client's governor —
//! never a hot loop), re-polls the chain (the backfill), and **resubscribes off the
//! fresh [`ChainFetch`] aliases**. Every [`MarketUpdate`] is handed to the two-class
//! [`MarketUpdateSink`], which routes `Chain`/`Health` to the control channel and
//! coalesces per-epic `Quote`s.
//!
//! [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
//! [ADR-0013]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0013-ig-builtin-packaging-and-0122-supply-chain-stop.md

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeDelta, Utc, Weekday};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::Positive;
use optionstratlib::{ExpirationDate, OptionStyle};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use ig_client::application::client::{Client, StreamerClient};
use ig_client::application::config::{Config, Credentials};
use ig_client::application::interfaces::market::MarketService;
use ig_client::error::AppError;
use ig_client::model::streaming::StreamingMarketField;
use ig_client::presentation::instrument::InstrumentType;
use ig_client::presentation::market::MarketData;
use ig_client::presentation::price::PriceData;
use ig_client::utils::parsing::parse_instrument_name;

use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, MarketUpdateSink,
    OptionStreamCapability, Provider, ProviderCapabilities, SendState, SubscriptionHandle,
    SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSnapshot, ChainSource, ContractSpecFingerprint, ExerciseStyle,
    ExpirySource, Instrument, InstrumentKey, MarketUpdate, ProviderId, QuoteUpdate,
    SettlementStyle, StreamHealth,
};
use crate::config::{EnvSource, Secret, require_credentials};
use crate::error::{NormalizeKind, ProviderError, TransportDetail, TransportKind};

/// The reserved provider id this adapter registers under
/// ([`RESERVED_PROVIDER_IDS`](crate::chain::RESERVED_PROVIDER_IDS)).
const IG_ID: &str = "ig";

/// The required credential field names read from the environment for `UserPass`
/// auth (`CHAINVIEW_IG_USERNAME` / `_PASSWORD` / `_API_KEY`,
/// `docs/03-data-providers.md` §11.3).
const CREDENTIAL_KEYS: [&str; 3] = ["username", "password", "api_key"];

/// The OPTIONAL, non-secret account selector. IG v3 (OAuth) pins the account after
/// login, so an absent value is safe (the session lands on the default account);
/// v2 only switches when a real account id is set.
const ACCOUNT_ID_VAR: &str = "CHAINVIEW_IG_ACCOUNT_ID";

/// The OPTIONAL environment selector (`demo` | `live`); absent or unrecognized
/// defaults to the safe [`IgEnvironment::Demo`] (the upstream client default).
const ENVIRONMENT_VAR: &str = "CHAINVIEW_IG_ENVIRONMENT";

/// The suggested chain-refresh cadence, in seconds — a hint only; the effective
/// interval is `config.refresh_interval`. IG's navigation-assembled structure is
/// re-polled while the Lightstreamer overlay keeps quotes current.
const REFRESH_HINT_SECS: u32 = 5;

/// IG option premiums quote in the instrument's currency; the assembled chain uses
/// a single quote currency label for the fingerprint (the venue product code
/// carries the epic root).
const DEFAULT_QUOTE_CURRENCY: &str = "USD";

/// The default contract multiplier for an IG dated option when the venue contract
/// size will not parse (IG options are `1x` on the epic's value-per-point).
const DEFAULT_CONTRACT_MULTIPLIER: u32 = 1;

/// The IG production REST gateway (`live`); the `demo` default lives in the
/// upstream `Config`. Public, non-secret endpoint.
const LIVE_REST_URL: &str = "https://api.ig.com/gateway/deal";
/// The IG production Lightstreamer endpoint (`live`). Public, non-secret.
const LIVE_WS_URL: &str = "wss://apd.marketdatasystems.com";

/// The bounded navigation/search market ceiling — a runaway guard on chain
/// assembly. A single IG option expiry is a few hundred legs; this ceiling is a
/// safety valve, never a normal limit.
const MAX_MARKETS: usize = 8_192;

/// The cap name carried by the typed limit error when assembly exhausts
/// [`MAX_MARKETS`] with markets still remaining (a compile-time `&'static str`).
const MARKET_CAP: &str = "ig market cap";

// --- Reconnect backoff (docs/03-data-providers.md §5) ------------------------

/// The reconnect backoff base, in milliseconds (`BASE = 250 ms`).
const BACKOFF_BASE_MS: f64 = 250.0;
/// The reconnect backoff ceiling, in milliseconds (`MAX = 30 s`).
const BACKOFF_MAX_MS: f64 = 30_000.0;
/// The reconnect jitter magnitude — the delay is scaled by `1 + jitter`,
/// `jitter in [-0.2, 0.2]`.
const JITTER_MAGNITUDE: f64 = 0.2;
/// The largest exponent applied to `2^attempt` before the [`BACKOFF_MAX_MS`] cap
/// takes over.
const BACKOFF_MAX_SHIFT: u32 = 20;

// ---------------------------------------------------------------------------
// The adapter.
// ---------------------------------------------------------------------------

/// The trading environment the adapter targets — ChainView's own mirror so
/// [`from_env`](IgAdapter::from_env) is testable without constructing an upstream
/// client. Mapped to the upstream base URLs only at the single config site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum IgEnvironment {
    /// IG demo (the upstream `Config` default) — the safe default.
    #[default]
    Demo,
    /// IG live/production.
    Live,
}

impl IgEnvironment {
    /// Parse the optional `CHAINVIEW_IG_ENVIRONMENT`; anything other than a
    /// case-insensitive `live` is [`Demo`](Self::Demo).
    fn from_value(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("live") {
            Self::Live
        } else {
            Self::Demo
        }
    }
}

/// The IG `Provider` adapter (crate-internal; behind the disabled `ig` feature).
///
/// Holds the reserved [`ProviderId`], the env-resolved credentials (wrapped in
/// [`Secret`], never logged), the optional non-secret account id, and the target
/// environment. `Clone` is cheap — a clone is moved into the spawned reconnect loop
/// so it can re-poll and reconnect without borrowing `&self` across the task
/// boundary.
#[derive(Clone)]
pub(crate) struct IgAdapter {
    id: ProviderId,
    username: Secret,
    password: Secret,
    api_key: Secret,
    account_id: String,
    environment: IgEnvironment,
}

impl IgAdapter {
    /// Build the adapter from the ChainView-namespaced environment
    /// (`CHAINVIEW_IG_USERNAME` / `_PASSWORD` / `_API_KEY`, and the optional
    /// `CHAINVIEW_IG_ACCOUNT_ID` / `CHAINVIEW_IG_ENVIRONMENT`). The credentials are
    /// read **only** here (env-only policy) and wrapped in [`Secret`]; they are
    /// never logged or echoed in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::MissingCredential`](crate::error::ConfigError::MissingCredential)
    /// (naming the provider, never the key) when any required credential is
    /// unset/empty.
    pub(crate) fn from_env(env: &dyn EnvSource) -> Result<Self, crate::error::ConfigError> {
        let id = ig_provider_id();
        let creds = require_credentials(env, &id, &CREDENTIAL_KEYS)?;
        let username = creds
            .get("USERNAME")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        let password = creds
            .get("PASSWORD")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        let api_key = creds
            .get("API_KEY")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        // Account id is a non-secret selector; absent -> empty (default account).
        let account_id = env.get(ACCOUNT_ID_VAR).unwrap_or_default();
        let environment = env
            .get(ENVIRONMENT_VAR)
            .map(|value| IgEnvironment::from_value(&value))
            .unwrap_or_default();
        Ok(Self {
            id,
            username,
            password,
            api_key,
            account_id,
            environment,
        })
    }

    /// Build the upstream `Credentials` from the injected secrets. The secret is
    /// exposed only at this single hand-off site and never logged.
    fn credentials(&self) -> Credentials {
        Credentials::new(
            self.username.expose().to_owned(),
            self.password.expose().to_owned(),
            self.account_id.clone(),
            self.api_key.expose().to_owned(),
        )
    }

    /// Build the env-free upstream `Config` from the injected credentials, pointing
    /// at the demo (default) or live REST/Lightstreamer endpoints. Never calls
    /// `Config::new`/`Config::default` (which would load a `.env`).
    fn upstream_config(&self) -> Config {
        let mut config = Config::from_credentials(self.credentials());
        if self.environment == IgEnvironment::Live {
            config.rest_api.base_url = LIVE_REST_URL.to_owned();
            config.websocket.url = LIVE_WS_URL.to_owned();
        }
        config
    }

    /// Build the REST client, injecting credentials programmatically (never the
    /// crate's `try_new`/`Config::default`).
    ///
    /// # Errors
    ///
    /// A redaction-safe [`ProviderError`] when the upstream client rejects the
    /// configuration (never carrying the credential).
    fn client(&self) -> Result<Client, ProviderError> {
        Client::with_config(self.upstream_config()).map_err(ig_error)
    }
}

#[async_trait]
impl Provider for IgAdapter {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ig_capabilities()
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        let source = LiveMarketSource {
            client: self.client()?,
        };
        discover_underlyings(&source).await
    }

    async fn fetch_chain(
        &self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        let source = LiveMarketSource {
            client: self.client()?,
        };
        let assembled = compose_chain(&source, underlying, expiration, &self.id, now_utc()).await?;
        Ok(assembled.fetch)
    }

    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        sink: MarketUpdateSink,
    ) -> Result<SubscriptionHandle, ProviderError> {
        // The adapter OWNS the reconnect/resubscribe loop. It selects on the
        // SUPERVISOR's child token (`req.cancel`, ADR-0009) so the ordered
        // bounded-join can await it, and the returned `SubscriptionHandle::spawned`
        // surfaces the loop's `JoinHandle` for registration.
        let transport = LiveStreamTransport::new(self.clone());
        let id = self.id.clone();
        let SubscriptionRequest {
            underlying,
            expiration_utc,
            instruments,
            cancel,
        } = req;
        // Seed the resubscribe epics + aliases from the poll leg's alias catalog so
        // the loop never re-derives symbols from strikes (docs/03 §4).
        let mut aliases = AliasCatalog::new();
        for instrument in &instruments {
            aliases.insert(instrument.clone());
        }
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            transport,
            id,
            underlying,
            expiration_utc,
            aliases,
            sink,
            loop_cancel,
        ));
        Ok(SubscriptionHandle::spawned(cancel, handle))
    }
}

// ---------------------------------------------------------------------------
// Identity + capabilities.
// ---------------------------------------------------------------------------

/// The adapter's reserved [`ProviderId`]. `"ig"` is a compile-time literal that
/// satisfies the grammar (proven by `test_ig_id_is_valid_and_reserved`), so
/// construction cannot fail; the fallback arm is unreachable.
fn ig_provider_id() -> ProviderId {
    match ProviderId::new(IG_ID) {
        Ok(id) => id,
        Err(_) => unreachable!("`ig` is a valid, reserved provider id literal"),
    }
}

/// IG's honest capability self-declaration — the `docs/03-data-providers.md` §8
/// row: a **Partial** (navigation-assembled) chain, **no** depth (a dated-option
/// epic does not populate the five-level ladder; the cell is `unverified` pending
/// the option-epic fixture, issue #50), Greeks **computed locally** (IG supplies
/// none), an unverified per-epic `ChainQuotes` overlay, **no** underlying stream
/// (the subscribe leg carries only the option epics from the alias catalog — IG
/// exposes no underlying spot on the option markets, so nothing folds an
/// underlying quote; the #40/#41 honesty rule), REST chain polling, **no** public
/// trades tape (IG's "trade" stream is the user's own deal confirmations), and
/// `UserPass` auth.
#[must_use]
pub(crate) fn ig_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Partial)
        .depth(false)
        .greeks(GreeksCapability::ComputedLocally)
        .option_stream(OptionStreamCapability::ChainQuotes { verified: false })
        // FALSE per the #40/#41 honesty rule: the subscribe leg carries only the
        // option epics from the alias catalog; IG exposes no underlying spot on the
        // option markets, so no update ever folds an underlying quote. Returns to
        // true only when an underlying epic is subscribed and folded.
        .underlying_stream(false)
        .chain_poll(ChainPollCapability::Poll {
            interval_hint_secs: REFRESH_HINT_SECS,
        })
        .trades_tape(false)
        .auth(AuthKind::UserPass)
        .build()
}

// ---------------------------------------------------------------------------
// Expiry resolution: authoritative timestamp wins, else date-only via session
// close, DST-aware in a bounded IANA-zone set (no timezone DB pulled).
// ---------------------------------------------------------------------------

/// A supported market session zone. Deliberately a **bounded** set — ChainView
/// pulls no timezone database (`docs/03-data-providers.md` §3) — so an expiry whose
/// market maps to no supported zone resolves to
/// [`NormalizeKind::UnparseableExpiry`] rather than a fabricated instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionZone {
    /// UK (Europe/London): GMT (`+0`) or BST (`+1`), DST last-Sun-Mar..last-Sun-Oct.
    UkLondon,
    /// US Eastern (America/New_York): EST (`-5`) or EDT (`-4`).
    UsEastern,
}

/// A market's session close — the zone plus the local close time-of-day used to
/// resolve a **date-only** expiry (`docs/03-data-providers.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionClose {
    /// The IANA session zone (bounded, DST-aware set).
    zone: SessionZone,
    /// The local close hour (0..=23).
    hour: u32,
    /// The local close minute (0..=59).
    minute: u32,
}

impl SessionClose {
    /// Construct a session close.
    const fn new(zone: SessionZone, hour: u32, minute: u32) -> Self {
        Self { zone, hour, minute }
    }
}

/// Resolve the session close for a market by its quote currency, defaulting to the
/// IG home session (Europe/London 16:30) — IG is UK-domiciled and most IG index
/// options settle on the exchange's local afternoon. A `USD`-quoted option (e.g. a
/// US index option) resolves through the US Eastern 16:00 close instead. This is a
/// documented, deterministic heuristic; the exact per-epic IANA zone would need a
/// timezone database (out of scope), and a market with no resolvable session yields
/// an [`UnparseableExpiry`](NormalizeKind::UnparseableExpiry) leg rather than a
/// fabricated instant.
fn session_for(currency: Option<&str>) -> SessionClose {
    match currency.map(str::to_ascii_uppercase).as_deref() {
        Some("USD") => SessionClose::new(SessionZone::UsEastern, 16, 0),
        _ => SessionClose::new(SessionZone::UkLondon, 16, 30),
    }
}

/// A parsed timestamp: either an absolute instant (an offset-carrying `lastDealingDate`)
/// or a naive local wall-clock time that still needs its session zone applied.
enum ParsedStamp {
    /// An offset-carrying (RFC3339) timestamp — already absolute.
    Absolute(DateTime<Utc>),
    /// A naive local datetime — needs the session zone applied.
    Local(NaiveDateTime),
}

/// Resolve an IG expiry to **one absolute UTC instant** (`docs/03-data-providers.md`
/// §3): the authoritative timestamped `last_dealing_date` **wins** when present; an
/// absent one falls to the date-only `expiry` resolved through the market's session
/// close. A present-but-unparseable timestamp, an unparseable date, or a
/// non-representable local time is [`NormalizeKind::UnparseableExpiry`] — never a
/// silently-keyed row.
///
/// # Errors
///
/// [`NormalizeKind::UnparseableExpiry`] for a malformed timestamp, a malformed
/// date, or an unresolvable local instant.
fn resolve_expiry(
    timestamped: Option<&str>,
    date_only: &str,
    session: &SessionClose,
) -> Result<DateTime<Utc>, NormalizeKind> {
    if let Some(stamp) = timestamped.map(str::trim).filter(|s| !s.is_empty()) {
        // The authoritative field is present: it MUST resolve (a garbage
        // authoritative value is an error, never a silent fall-through to the
        // date-only field).
        return match parse_timestamp(stamp) {
            Some(ParsedStamp::Absolute(dt)) => Ok(dt),
            Some(ParsedStamp::Local(naive)) => {
                local_to_utc(naive, session.zone).ok_or(NormalizeKind::UnparseableExpiry)
            }
            None => Err(NormalizeKind::UnparseableExpiry),
        };
    }
    let date = parse_ig_date(date_only).ok_or(NormalizeKind::UnparseableExpiry)?;
    let naive = date
        .and_hms_opt(session.hour, session.minute, 0)
        .ok_or(NormalizeKind::UnparseableExpiry)?;
    local_to_utc(naive, session.zone).ok_or(NormalizeKind::UnparseableExpiry)
}

/// Parse an IG `lastDealingDate`: an RFC3339 (offset-carrying) timestamp is
/// absolute; a naive `YYYY-MM-DDThh:mm[:ss]` is a local wall-clock time. Returns
/// `None` for anything else.
fn parse_timestamp(s: &str) -> Option<ParsedStamp> {
    // Offset-carrying forms end with `Z` or carry a `+`/`-` offset after the time.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(ParsedStamp::Absolute(dt.with_timezone(&Utc)));
    }
    parse_naive_datetime(s).map(ParsedStamp::Local)
}

/// Parse a strict `YYYY-MM-DDThh:mm` or `YYYY-MM-DDThh:mm:ss` naive datetime,
/// tolerating a space separator instead of `T`. No locale, no format string.
fn parse_naive_datetime(s: &str) -> Option<NaiveDateTime> {
    let (date_part, time_part) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let date = parse_iso_date(date_part)?;
    let mut time = time_part.split(':');
    let hour = time.next()?.parse::<u32>().ok()?;
    let minute = time.next()?.parse::<u32>().ok()?;
    let second = match time.next() {
        Some(sec) => sec.parse::<u32>().ok()?,
        None => 0,
    };
    if time.next().is_some() {
        return None;
    }
    date.and_hms_opt(hour, minute, second)
}

/// Parse a strict `YYYY-MM-DD` date.
fn parse_iso_date(s: &str) -> Option<NaiveDate> {
    let mut parts = s.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Parse an IG date-only expiry: either `DD-MON-YY` (`"18-JUL-26"`, the navigation
/// market form) or `YYYY-MM-DD`. Returns `None` for anything else.
fn parse_ig_date(s: &str) -> Option<NaiveDate> {
    let trimmed = s.trim();
    if let Some(date) = parse_iso_date(trimmed) {
        return Some(date);
    }
    let mut parts = trimmed.split('-');
    let day = parts.next()?.trim().parse::<u32>().ok()?;
    let month = month_from_abbrev(parts.next()?.trim())?;
    let year_two = parts.next()?.trim().parse::<i32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // A two-digit IG year is 2000-based (dated options are near-dated).
    let year = 2000 + year_two;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Map an uppercase three-letter month abbreviation to its 1-based month number.
fn month_from_abbrev(abbrev: &str) -> Option<u32> {
    match abbrev.to_ascii_uppercase().as_str() {
        "JAN" => Some(1),
        "FEB" => Some(2),
        "MAR" => Some(3),
        "APR" => Some(4),
        "MAY" => Some(5),
        "JUN" => Some(6),
        "JUL" => Some(7),
        "AUG" => Some(8),
        "SEP" => Some(9),
        "OCT" => Some(10),
        "NOV" => Some(11),
        "DEC" => Some(12),
        _ => None,
    }
}

/// Convert a naive local close time in `zone` to an absolute UTC instant,
/// DST-aware. The offset at a 16:00/16:30 close is unambiguous (both DST
/// transitions occur near 01:00-02:00 local, well before the close).
fn local_to_utc(naive: NaiveDateTime, zone: SessionZone) -> Option<DateTime<Utc>> {
    let date = naive.date();
    // Offset EAST of UTC, in hours (local = UTC + offset).
    let offset_hours: i64 = match zone {
        SessionZone::UkLondon => {
            if uk_dst(date) {
                1
            } else {
                0
            }
        }
        SessionZone::UsEastern => {
            if us_eastern_dst(date) {
                -4
            } else {
                -5
            }
        }
    };
    let utc_naive = naive.checked_sub_signed(TimeDelta::hours(offset_hours))?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(utc_naive, Utc))
}

/// Whether UK summer time (BST) is in effect on `date`: from the **last Sunday of
/// March** through the day **before** the last Sunday of October.
fn uk_dst(date: NaiveDate) -> bool {
    let year = date.year();
    match (
        last_weekday_of_month(year, 3, Weekday::Sun),
        last_weekday_of_month(year, 10, Weekday::Sun),
    ) {
        (Some(start), Some(end)) => date >= start && date < end,
        _ => false,
    }
}

/// Whether US Eastern DST (EDT) is in effect on `date`: from the **second Sunday of
/// March** through the day **before** the first Sunday of November.
fn us_eastern_dst(date: NaiveDate) -> bool {
    let year = date.year();
    match (
        nth_weekday_of_month(year, 3, Weekday::Sun, 2),
        nth_weekday_of_month(year, 11, Weekday::Sun, 1),
    ) {
        (Some(start), Some(end)) => date >= start && date < end,
        _ => false,
    }
}

/// The `n`-th (1-based) `weekday` in `month` of `year`, or `None` when the month
/// has fewer than `n` of them.
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

/// The **last** `weekday` in `month` of `year`.
fn last_weekday_of_month(year: i32, month: u32, weekday: Weekday) -> Option<NaiveDate> {
    // Start at the first of the next month, step back one day, then walk back to
    // the target weekday.
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month.checked_add(1)?)
    };
    let first_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    let mut cursor = first_next.pred_opt()?;
    let target = weekday.num_days_from_sunday();
    for _ in 0..7 {
        if cursor.weekday().num_days_from_sunday() == target {
            return Some(cursor);
        }
        cursor = cursor.pred_opt()?;
    }
    None
}

// ---------------------------------------------------------------------------
// Numeric normalization at the f64 seam.
// ---------------------------------------------------------------------------

/// A checked price field: `NaN`/`Inf`/negative is **dropped** (returns `None`).
/// Zero is a valid value and is kept.
fn positive_or_drop(value: f64) -> Option<Positive> {
    Positive::new(value).ok()
}

/// A normalized best-bid/best-ask pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NormalizedQuote {
    bid: Option<Positive>,
    ask: Option<Positive>,
}

/// Normalize a best-bid/best-ask pair (`docs/03-data-providers.md` §3): a per-field
/// `NaN`/`Inf`/negative is dropped to `None`; a **zero bid is valid**; a **zero ask
/// on a non-zero bid**, or any `ask < bid`, is **crossed** and the whole quote is
/// rejected.
fn normalize_quote(bid: Option<f64>, ask: Option<f64>) -> Result<NormalizedQuote, NormalizeKind> {
    let bid = bid.and_then(positive_or_drop);
    let ask = ask.and_then(positive_or_drop);
    if let (Some(bid_value), Some(ask_value)) = (bid, ask)
        && (ask_value < bid_value || (ask_value == Positive::ZERO && bid_value != Positive::ZERO))
    {
        return Err(NormalizeKind::OutOfRange("ask"));
    }
    Ok(NormalizedQuote { bid, ask })
}

/// Parse an IG strike (the `parse_instrument_name` string, e.g. `"7500"` or
/// `"10.5"`) into a non-zero [`Positive`].
fn strike_positive(value: &str) -> Result<Positive, NormalizeKind> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(positive_or_drop)
        .ok_or(NormalizeKind::OutOfRange("strike"))?;
    if parsed == Positive::ZERO {
        return Err(NormalizeKind::OutOfRange("strike"));
    }
    Ok(parsed)
}

/// The IG economic-equivalence fingerprint: a dated option is **cash-settled**
/// (index/CFD), European-style, quoted in its currency, keyed by the epic root.
fn ig_fingerprint(epic_root: &str, quote_currency: &str) -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: DEFAULT_CONTRACT_MULTIPLIER,
        settlement: SettlementStyle::Cash,
        exercise: ExerciseStyle::European,
        quote_currency: quote_currency.to_owned(),
        venue_product_code: epic_root.to_owned(),
    }
}

/// The epic root — the leading `OP.D.<ROOT>...` segment used as the venue product
/// code, or the whole epic when it does not split.
fn epic_root(epic: &str) -> String {
    epic.split('.').take(3).collect::<Vec<_>>().join(".")
}

// ---------------------------------------------------------------------------
// Raw DTO neutral views — mapped from the upstream types, never escaping here.
// ---------------------------------------------------------------------------

/// A neutral view of one navigation/search market, mapped from the upstream
/// `MarketData` inside [`LiveMarketSource`] so no raw DTO escapes. Every numeric is
/// a raw `f64` here and is checked at the seam before it enters a domain type.
#[derive(Debug, Clone)]
pub(crate) struct RawMarket {
    /// The epic — the native id and the subscription key.
    epic: String,
    /// The human instrument name (`"US 500 7500 CALL"`), parsed for strike + type.
    instrument_name: String,
    /// The date-only expiry (`"18-JUL-26"`), resolved via session close.
    expiry: String,
    /// The authoritative timestamped `lastDealingDate`, when a market-details fetch
    /// supplied it (wins over the date-only `expiry`).
    last_dealing_date: Option<String>,
    /// Whether the upstream instrument type is an option (`OPT_*`).
    is_option: bool,
    /// The venue best bid, when quoted.
    bid: Option<f64>,
    /// The venue best offer/ask, when quoted.
    offer: Option<f64>,
    /// The quote currency, when the market surfaces it (drives the session zone).
    currency: Option<String>,
}

/// A neutral view of one Lightstreamer price update, mapped from the upstream
/// `PriceData` inside [`LiveStreamTransport`] so no raw DTO escapes.
#[derive(Debug, Clone)]
struct RawPriceEvent {
    /// The epic the update is for (the `MARKET:` prefix stripped).
    epic: String,
    /// The streamed best bid.
    bid: Option<f64>,
    /// The streamed best offer/ask.
    offer: Option<f64>,
}

/// True when an upstream `InstrumentType` is a dated-option type (`OPT_*`).
fn is_option_type(instrument_type: &InstrumentType) -> bool {
    matches!(
        instrument_type,
        InstrumentType::OptCommodities
            | InstrumentType::OptCurrencies
            | InstrumentType::OptIndices
            | InstrumentType::OptRates
            | InstrumentType::OptShares
    )
}

/// Map one upstream `MarketData` onto the neutral [`RawMarket`]. A navigation
/// market carries no `lastDealingDate` (that lives on market-details), so the
/// timestamped field is `None` here — the date-only path resolves it.
pub(crate) fn map_market_data(market: &MarketData) -> RawMarket {
    RawMarket {
        epic: market.epic.clone(),
        instrument_name: market.instrument_name.clone(),
        expiry: market.expiry.clone(),
        last_dealing_date: None,
        is_option: is_option_type(&market.instrument_type),
        bid: market.bid,
        offer: market.offer,
        currency: None,
    }
}

/// Map one upstream `PriceData` onto the neutral [`RawPriceEvent`], stripping the
/// Lightstreamer `MARKET:` item prefix so the epic resolves against the alias
/// catalog.
fn map_price_data(price: &PriceData) -> RawPriceEvent {
    let epic = price
        .item_name
        .strip_prefix("MARKET:")
        .unwrap_or(&price.item_name)
        .to_owned();
    RawPriceEvent {
        epic,
        bid: price.fields.bid,
        offer: price.fields.offer,
    }
}

// ---------------------------------------------------------------------------
// The navigation/search market seam the chain path drives (mockable).
// ---------------------------------------------------------------------------

/// The REST market seam the chain path drives, so navigation walking / search /
/// assembly run deterministically against a **mock** with no network. The
/// production [`LiveMarketSource`] wraps the upstream `Client`; a raw DTO is mapped
/// to [`RawMarket`] inside it and never crosses this seam.
#[async_trait]
trait IgMarketSource: Send + Sync {
    /// Search markets by term (the fetch-chain discovery path).
    async fn search(&self, term: &str) -> Result<Vec<RawMarket>, ProviderError>;

    /// The top-level navigation node names (the `discover` candidate-underlying
    /// path).
    async fn navigation_roots(&self) -> Result<Vec<String>, ProviderError>;
}

/// The production [`IgMarketSource`]: the upstream `Client` (`MarketService`). Raw
/// upstream types stay inside it.
struct LiveMarketSource {
    client: Client,
}

#[async_trait]
impl IgMarketSource for LiveMarketSource {
    async fn search(&self, term: &str) -> Result<Vec<RawMarket>, ProviderError> {
        let response = self.client.search_markets(term).await.map_err(ig_error)?;
        Ok(response.markets.iter().map(map_market_data).collect())
    }

    async fn navigation_roots(&self) -> Result<Vec<String>, ProviderError> {
        let response = self
            .client
            .get_market_navigation()
            .await
            .map_err(ig_error)?;
        Ok(response.nodes.into_iter().map(|node| node.name).collect())
    }
}

// ---------------------------------------------------------------------------
// discover + the assembled chain path.
// ---------------------------------------------------------------------------

/// Enumerate candidate option underlyings via the navigation root
/// (`docs/03-data-providers.md` §7.4): the top-level node names are the broad
/// categories a trader drills into. Bounded to [`MAX_MARKETS`] refs.
async fn discover_underlyings<S: IgMarketSource + ?Sized>(
    source: &S,
) -> Result<Vec<UnderlyingRef>, ProviderError> {
    let roots = source.navigation_roots().await?;
    Ok(roots
        .into_iter()
        .take(MAX_MARKETS)
        .map(UnderlyingRef::new)
        .collect())
}

/// The assembled result: the poll-leg [`ChainFetch`]. IG supplies no venue Greeks,
/// so there are no venue overlays — the store computes local Greeks from the
/// two-sided quotes at seed/poll.
#[derive(Debug, Clone)]
struct AssembledChain {
    fetch: ChainFetch,
}

/// One normalized IG option leg — the domain values assembled into an
/// [`OptionChain`] row plus its [`AliasCatalog`] entry.
#[derive(Debug, Clone)]
struct NormalizedLeg {
    key: InstrumentKey,
    native_symbol: String,
    spec: ContractSpecFingerprint,
    style: OptionStyle,
    bid: Option<Positive>,
    ask: Option<Positive>,
}

/// Normalize one raw market into a [`NormalizedLeg`], or `None` when it is not a
/// parseable option leg for the target expiry. A market whose strike/type will not
/// parse, whose expiry does not resolve, or whose expiry differs from the target
/// contributes **no** leg; a crossed quote drops only the quote.
fn normalize_leg(
    market: &RawMarket,
    underlying: &str,
    target_expiry_utc: DateTime<Utc>,
) -> Option<NormalizedLeg> {
    if !market.is_option {
        return None;
    }
    let parsed = parse_instrument_name(&market.instrument_name);
    let style = match parsed.option_type.as_deref() {
        Some("CALL") => OptionStyle::Call,
        Some("PUT") => OptionStyle::Put,
        _ => return None,
    };
    let strike = strike_positive(parsed.strike.as_deref()?).ok()?;

    let session = session_for(market.currency.as_deref());
    let expiration_utc = resolve_expiry(
        market.last_dealing_date.as_deref(),
        &market.expiry,
        &session,
    )
    .ok()?;
    // Only legs of the requested expiry belong to this chain.
    if expiration_utc != target_expiry_utc {
        return None;
    }

    let quote = normalize_quote(market.bid, market.offer).unwrap_or_default();
    let spec = ig_fingerprint(&epic_root(&market.epic), DEFAULT_QUOTE_CURRENCY);
    let key = InstrumentKey {
        underlying: underlying.to_owned(),
        expiration_utc,
        strike,
        style,
    };
    Some(NormalizedLeg {
        key,
        native_symbol: market.epic.clone(),
        spec,
        style,
        bid: quote.bid,
        ask: quote.ask,
    })
}

/// The call/put legs sharing one strike.
#[derive(Debug, Default)]
struct StrikePair<'a> {
    call: Option<&'a NormalizedLeg>,
    put: Option<&'a NormalizedLeg>,
}

/// Resolve the requested `expiration` to the absolute-UTC instant the chain keys
/// on. A relative offset is resolved via the caller-supplied `received` reference
/// and the default session close; an absolute `DateTime` passes through. A relative
/// offset without a date is rejected.
fn target_expiry(
    expiration: &ExpirationDate,
    received: DateTime<Utc>,
) -> Result<DateTime<Utc>, ProviderError> {
    match expiration {
        ExpirationDate::DateTime(dt) => Ok(*dt),
        ExpirationDate::Days(days) => {
            // Resolve a relative offset deterministically off the received clock,
            // then snap to the default session close of that date.
            let seconds = (days.to_f64() * 86_400.0).round();
            if !seconds.is_finite() {
                return Err(ProviderError::Normalize {
                    kind: NormalizeKind::UnparseableExpiry,
                });
            }
            let seconds = seconds as i64;
            let instant = received
                .checked_add_signed(TimeDelta::seconds(seconds))
                .ok_or(ProviderError::Normalize {
                    kind: NormalizeKind::UnparseableExpiry,
                })?;
            let session = SessionClose::new(SessionZone::UkLondon, 16, 30);
            let naive = instant
                .date_naive()
                .and_hms_opt(session.hour, session.minute, 0)
                .ok_or(ProviderError::Normalize {
                    kind: NormalizeKind::UnparseableExpiry,
                })?;
            local_to_utc(naive, session.zone).ok_or(ProviderError::Normalize {
                kind: NormalizeKind::UnparseableExpiry,
            })
        }
    }
}

/// Assemble the chain for `(underlying, expiration)` by searching IG's markets and
/// normalizing every option leg of the requested expiry into an [`OptionChain`]
/// plus its [`AliasCatalog`] — atomically (the result is built once, so no
/// half-filled chain is returned).
///
/// # Errors
///
/// [`ProviderError::Normalize`] for an unparseable requested expiry or the market
/// ceiling exceeded; [`ProviderError::NoChain`] when no option leg of the requested
/// expiry is found; a transport failure from the search.
async fn compose_chain<S: IgMarketSource + ?Sized>(
    source: &S,
    underlying: &str,
    expiration: &ExpirationDate,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Result<AssembledChain, ProviderError> {
    let symbol = underlying.to_ascii_uppercase();
    let expiration_utc = target_expiry(expiration, received)?;
    let markets = source.search(underlying).await?;
    if markets.len() > MAX_MARKETS {
        return Err(ProviderError::Normalize {
            kind: NormalizeKind::LimitExceeded(MARKET_CAP),
        });
    }
    assemble_chain(&markets, &symbol, expiration_utc, provider)
}

/// Assemble the normalized legs into a single [`OptionChain`] plus its
/// [`AliasCatalog`]. The published strike set equals the normalizable option legs
/// of the requested expiry.
///
/// # Errors
///
/// [`ProviderError::NoChain`] when no market yields a normalizable option leg for
/// the requested expiry.
fn assemble_chain(
    markets: &[RawMarket],
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    provider: &ProviderId,
) -> Result<AssembledChain, ProviderError> {
    let legs: Vec<NormalizedLeg> = markets
        .iter()
        .filter_map(|market| normalize_leg(market, underlying, expiration_utc))
        .collect();

    if legs.is_empty() {
        return Err(ProviderError::NoChain {
            underlying: underlying.to_owned(),
            expiration: expiration_utc.to_rfc3339(),
        });
    }

    let mut aliases = AliasCatalog::new();
    for leg in &legs {
        aliases.insert(Instrument {
            key: leg.key.clone(),
            provider: provider.clone(),
            native_symbol: leg.native_symbol.clone(),
            stream_symbol: Some(format!("MARKET:{}", leg.native_symbol)),
            spec: leg.spec.clone(),
        });
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

    // IG carries no underlying spot on the option markets, so seed the chain center
    // to the MEDIAN strike (a real value from the strike ladder, refreshed by the
    // next poll), never a fabricated quote.
    let spot = median_strike(&by_strike);
    let mut chain = OptionChain::new(underlying, spot, expiration_utc.to_rfc3339(), None, None);
    for (strike, pair) in &by_strike {
        chain.add_option(
            *strike,
            pair.call.and_then(|leg| leg.bid),
            pair.call.and_then(|leg| leg.ask),
            pair.put.and_then(|leg| leg.bid),
            pair.put.and_then(|leg| leg.ask),
            // IG supplies no venue IV: seed the absent-IV sentinel (Positive::ZERO);
            // the store's local engine inverts IV from the two-sided quotes.
            Positive::ZERO,
            // No venue Greeks — the store computes them locally, ComputedLocally.
            None,
            None,
            None,
            None,
            None,
            None,
        );
    }

    let fetch = ChainFetch::new(
        chain,
        ExpirySource::new(underlying, expiration_utc, provider.clone()),
        aliases,
    );
    Ok(AssembledChain { fetch })
}

/// The median strike of a sorted strike map — the provisional chain center used
/// when IG carries no spot. The map is non-empty at every call site (the caller
/// returns `NoChain` first), so the fallback is never taken.
fn median_strike(by_strike: &BTreeMap<Positive, StrikePair<'_>>) -> Positive {
    let strikes: Vec<Positive> = by_strike.keys().copied().collect();
    let mid = strikes.len() / 2;
    strikes.get(mid).copied().unwrap_or(Positive::ONE)
}

// ---------------------------------------------------------------------------
// Redaction-safe transport / auth error mapping.
// ---------------------------------------------------------------------------

/// Map an upstream [`AppError`] to a redaction-safe [`ProviderError`] by **category
/// only** — the inner message (which may hold a URL, body, or a credential the
/// upstream may interpolate) is never carried (`docs/03-data-providers.md` §6,
/// `docs/SECURITY.md` §1). Only a non-secret HTTP status rides along.
fn ig_error(err: AppError) -> ProviderError {
    match err {
        AppError::Auth(_) | AppError::Unauthorized | AppError::OAuthTokenExpired => {
            ProviderError::Auth
        }
        AppError::Unexpected(status) => ProviderError::Transport(Box::new(TransportDetail::new(
            TransportKind::Http,
            Some(status.as_u16()),
        ))),
        AppError::NotFound => ProviderError::Transport(Box::new(TransportDetail::new(
            TransportKind::Http,
            Some(404),
        ))),
        AppError::RateLimitExceeded => ProviderError::RateLimited(None),
        AppError::HistoricalDataAllowanceExceeded { allowance_expiry } => {
            ProviderError::RateLimited(Some(Duration::from_secs(allowance_expiry)))
        }
        AppError::Network(_) | AppError::Io(_) | AppError::WebSocketError(_) => {
            transport(TransportKind::Closed)
        }
        AppError::Json(_) | AppError::Deserialization(_) | AppError::SerializationError(_) => {
            transport(TransportKind::Decode)
        }
        AppError::Db(_) | AppError::InvalidInput(_) | AppError::Generic(_) => {
            transport(TransportKind::Http)
        }
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

/// The venue-I/O seam the reconnect loop drives so the loop runs deterministically
/// against a **mock** — no socket, no wall clock. The production
/// [`LiveStreamTransport`] wraps the upstream `StreamerClient` (Lightstreamer) plus
/// the REST re-poll; a raw `PriceData` is decoded to [`RawPriceEvent`] inside it and
/// never crosses this seam.
#[async_trait]
trait IgStreamTransport: Send {
    /// Open the Lightstreamer stream and subscribe the `epics` (bid/offer).
    async fn connect_and_subscribe(&mut self, epics: &[String]) -> Result<(), TransportGone>;

    /// Await the next per-epic price update. `Err(_)` means the stream ended — the
    /// loop reconnects.
    async fn receive(&mut self) -> Result<RawPriceEvent, TransportGone>;

    /// Re-poll the assembled chain to reconcile drift on (re)connect (backfill =
    /// current state, §5). `None` on a failed/cancelled poll — the caller keeps
    /// prior.
    async fn poll(
        &mut self,
        underlying: &str,
        expiration: &ExpirationDate,
        received: DateTime<Utc>,
    ) -> Option<AssembledChain>;
}

/// The production [`IgStreamTransport`]: the upstream `StreamerClient`
/// (`with_client`, env-free) for live per-epic price updates and the adapter's REST
/// re-poll for the backfill. The raw upstream types stay private and never escape.
struct LiveStreamTransport {
    adapter: IgAdapter,
    /// The per-epic price receiver from the active market subscription.
    receiver: Option<UnboundedReceiver<PriceData>>,
    /// The shutdown signal for the spawned `connect` task (notified on teardown).
    shutdown: Option<Arc<Notify>>,
    /// The spawned `StreamerClient::connect` task handle, aborted on teardown.
    conn_task: Option<tokio::task::JoinHandle<()>>,
}

impl LiveStreamTransport {
    fn new(adapter: IgAdapter) -> Self {
        Self {
            adapter,
            receiver: None,
            shutdown: None,
            conn_task: None,
        }
    }

    /// Signal + abort the previous connection before opening a fresh one, so a
    /// reconnect never leaves a half-open Lightstreamer session behind.
    fn teardown(&mut self) {
        if let Some(signal) = self.shutdown.take() {
            signal.notify_one();
        }
        if let Some(task) = self.conn_task.take() {
            task.abort();
        }
        self.receiver = None;
    }
}

impl Drop for LiveStreamTransport {
    fn drop(&mut self) {
        self.teardown();
    }
}

#[async_trait]
impl IgStreamTransport for LiveStreamTransport {
    async fn connect_and_subscribe(&mut self, epics: &[String]) -> Result<(), TransportGone> {
        self.teardown();
        if epics.is_empty() {
            return Err(TransportGone);
        }
        // Build a fresh client + streamer from injected credentials (env-free,
        // reuses the client's session via `with_client`).
        let client = self.adapter.client().map_err(|_| TransportGone)?;
        let mut streamer = StreamerClient::with_client(&client)
            .await
            .map_err(|_| TransportGone)?;
        let fields = std::collections::HashSet::from([
            StreamingMarketField::Bid,
            StreamingMarketField::Offer,
            StreamingMarketField::UpdateTime,
        ]);
        let receiver = streamer
            .market_subscribe(epics.to_vec(), fields)
            .await
            .map_err(|_| TransportGone)?;
        // `connect` blocks until the shutdown signal, so it runs in its own task;
        // the receiver is already owned here for `receive`.
        let signal = Arc::new(Notify::new());
        let task_signal = Arc::clone(&signal);
        let task = tokio::spawn(async move {
            let mut streamer = streamer;
            let _ = streamer.connect(Some(task_signal)).await;
            let _ = streamer.disconnect().await;
        });
        self.receiver = Some(receiver);
        self.shutdown = Some(signal);
        self.conn_task = Some(task);
        Ok(())
    }

    async fn receive(&mut self) -> Result<RawPriceEvent, TransportGone> {
        match self.receiver.as_mut() {
            Some(receiver) => match receiver.recv().await {
                Some(price) => Ok(map_price_data(&price)),
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
    ) -> Option<AssembledChain> {
        let source = LiveMarketSource {
            client: self.adapter.client().ok()?,
        };
        compose_chain(&source, underlying, expiration, &self.adapter.id, received)
            .await
            .ok()
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

/// The adapter-owned reconnect/resubscribe loop (`docs/03-data-providers.md` §5).
/// Connect + subscribe the per-epic legs, emit the Live health + a re-polled
/// backfill (`Chain` snapshot — the store computes local Greeks), drain the stream
/// as per-epic `Quote`s; on a drop emit `Health(Reconnecting{attempt})`, back off
/// with jitter, and reconnect. The backfill refreshes the epics + aliases off the
/// FRESH [`ChainFetch`] so the resubscription is off current aliases. Cancellation
/// is observed at every `.await`, so the loop never opens a stream after
/// cancellation and never hot-loops.
async fn run_reconnect_loop<T: IgStreamTransport>(
    mut transport: T,
    id: ProviderId,
    underlying: String,
    expiration_utc: DateTime<Utc>,
    mut aliases: AliasCatalog,
    mut sink: MarketUpdateSink,
    cancel: CancellationToken,
) {
    let mut epics = epics_of(&aliases, &id);
    let mut attempt: u32 = 0;
    loop {
        if cancel.is_cancelled() || sink.is_closed() {
            return;
        }
        let exit = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            exit = connect_stream_once(
                &mut transport, &id, &underlying, expiration_utc,
                &mut epics, &mut aliases, &mut sink, &cancel, &mut attempt,
            ) => exit,
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
        if health_sent == SendState::Closed {
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

/// One connection attempt: connect + subscribe the epics, emit the Live backfill
/// (`Chain`), then drain the stream as per-epic `Quote`s until it drops or the
/// subscription is cancelled. `attempt` resets to 0 on a successful (re)connect.
#[allow(clippy::too_many_arguments)]
async fn connect_stream_once<T: IgStreamTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    epics: &mut Vec<String>,
    aliases: &mut AliasCatalog,
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
    attempt: &mut u32,
) -> StreamExit {
    let subscribed = tokio::select! {
        biased;
        () = cancel.cancelled() => return StreamExit::Shutdown,
        result = transport.connect_and_subscribe(epics) => result,
    };
    if subscribed.is_err() {
        return StreamExit::Reconnect;
    }

    *attempt = 0;
    // The (re)connect is live: emit Health(Live), then the CURRENT-STATE backfill
    // (a fresh Chain snapshot), refreshing the epics + aliases off the fresh fetch.
    if go_live_and_backfill(
        transport,
        id,
        underlying,
        expiration_utc,
        epics,
        aliases,
        sink,
        cancel,
    )
    .await
        == SendState::Closed
    {
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
        // Guard against a stream update for an unknown epic (hard rule 5): resolve
        // it against the current aliases; an unknown symbol is dropped, never keyed.
        let Some(update) = quote_update(&event, aliases, id, now_utc()) else {
            continue;
        };
        let sent = tokio::select! {
            biased;
            () = cancel.cancelled() => return StreamExit::Shutdown,
            outcome = sink.send(MarketUpdate::Quote(update)) => outcome,
        };
        if sent == SendState::Closed {
            return StreamExit::Shutdown;
        }
    }
}

/// Emit the Live health then the re-polled backfill (a fresh `Chain`) — the shared
/// "(re)connect established" step. Refreshes `epics` + `aliases` off the fresh
/// fetch so a subsequent reconnect resubscribes off current aliases. Cancellation
/// short-circuits; [`SendState::Closed`] once the consumer is gone.
#[allow(clippy::too_many_arguments)]
async fn go_live_and_backfill<T: IgStreamTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    epics: &mut Vec<String>,
    aliases: &mut AliasCatalog,
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
) -> SendState {
    let live = MarketUpdate::Health(id.clone(), StreamHealth::Live);
    let sent = tokio::select! {
        biased;
        () = cancel.cancelled() => return SendState::Open,
        outcome = sink.send(live) => outcome,
    };
    if sent == SendState::Closed {
        return SendState::Closed;
    }

    let expiration = ExpirationDate::DateTime(expiration_utc);
    let composed = tokio::select! {
        biased;
        () = cancel.cancelled() => return SendState::Open,
        result = transport.poll(underlying, &expiration, now_utc()) => result,
    };
    let Some(composed) = composed else {
        return SendState::Open;
    };

    // Refresh the resubscribe aliases + epics off the FRESH fetch (docs/03 §5).
    *aliases = composed.fetch.aliases.clone();
    *epics = epics_of(aliases, id);

    let snapshot = MarketUpdate::Chain(chain_snapshot(&composed.fetch, now_utc()));
    tokio::select! {
        biased;
        () = cancel.cancelled() => SendState::Open,
        outcome = sink.send(snapshot) => outcome,
    }
}

/// The current per-epic native symbols for this provider, the resubscription set.
fn epics_of(aliases: &AliasCatalog, provider: &ProviderId) -> Vec<String> {
    aliases
        .instruments()
        .filter(|instrument| &instrument.provider == provider)
        .map(|instrument| instrument.native_symbol.clone())
        .collect()
}

/// Build a per-epic [`QuoteUpdate`] from a raw price event, resolving the epic to
/// its [`Instrument`] via the alias catalog. `None` for an unknown epic (dropped)
/// or a crossed quote (rejected), so a stray update never keys an unknown row.
fn quote_update(
    event: &RawPriceEvent,
    aliases: &AliasCatalog,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Option<QuoteUpdate> {
    let key = aliases.resolve_symbol(&event.epic)?.clone();
    let instrument = aliases.instrument(&key, provider)?.clone();
    let quote = normalize_quote(event.bid, event.offer).ok()?;
    if quote.bid.is_none() && quote.ask.is_none() {
        return None;
    }
    Some(QuoteUpdate {
        instrument,
        bid: quote.bid,
        ask: quote.ask,
        last: None,
        bid_size: None,
        ask_size: None,
        event_time: None,
        received_time: received,
    })
}

/// Assemble a streaming-current [`ChainSnapshot`] from a re-polled [`ChainFetch`] —
/// the same `AliasCatalog` carried forward with no re-derivation. The source is
/// [`ChainSource::Merged`] (REST seeds structure + local Greeks, the stream overlays
/// quotes).
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
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use tokio::sync::mpsc;

    use super::*;
    use crate::chain::{
        ChainStore, GreeksOrigin, LegStatus, MergeOutcome, PricingInputs, QuoteClocks,
        compute_leg_greeks,
    };

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
    fn utc_rfc3339(s: &str) -> DateTime<Utc> {
        match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => panic!("invalid rfc3339 `{s}`: {e}"),
        }
    }

    #[track_caller]
    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        match NaiveDate::from_ymd_opt(y, m, d) {
            Some(date) => date,
            None => panic!("invalid test date {y}-{m}-{d}"),
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
        let _ = env.insert("CHAINVIEW_IG_USERNAME".to_owned(), "alice".to_owned());
        let _ = env.insert(
            "CHAINVIEW_IG_PASSWORD".to_owned(),
            "do-not-log-this-password".to_owned(),
        );
        let _ = env.insert(
            "CHAINVIEW_IG_API_KEY".to_owned(),
            "do-not-log-this-key".to_owned(),
        );
        MapEnv(env)
    }

    #[track_caller]
    fn sample_adapter() -> IgAdapter {
        match IgAdapter::from_env(&creds_env()) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed with all creds present: {e}"),
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

    fn drain(rx: &mut mpsc::Receiver<MarketUpdate>) -> Vec<MarketUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    // === Fixture: a recorded market-navigation shape =========================

    const MARKET_NAVIGATION: &str =
        include_str!("../../tests/fixtures/ig/market_navigation_spx.json");

    /// Parse the recorded `MarketNavigationResponse` shape and map its markets onto
    /// the neutral [`RawMarket`] views the assembly path consumes — the same map
    /// `LiveMarketSource::search` applies, so this proves navigation-tree assembly
    /// from a recorded shape.
    #[track_caller]
    fn navigation_markets() -> Vec<RawMarket> {
        use ig_client::model::responses::MarketNavigationResponse;
        let response: MarketNavigationResponse = match serde_json::from_str(MARKET_NAVIGATION) {
            Ok(r) => r,
            Err(e) => panic!("the market-navigation fixture must deserialize: {e}"),
        };
        response.markets.iter().map(map_market_data).collect()
    }

    /// The fixture's expiry (`18-JUL-26`) resolved via the SAME session the
    /// assembly uses for its legs. A navigation `MarketData` carries no currency,
    /// so `session_for(None)` selects the IG-home Europe/London 16:30 close
    /// (BST/+1 in July -> 15:30 UTC) — the target the legs must equal.
    #[track_caller]
    fn fixture_expiry_utc() -> DateTime<Utc> {
        let session = session_for(None);
        match resolve_expiry(None, "18-JUL-26", &session) {
            Ok(dt) => dt,
            Err(e) => panic!("fixture expiry must resolve: {e}"),
        }
    }

    // === Identity + capabilities =============================================

    #[test]
    fn test_ig_id_is_valid_and_reserved() {
        let id = ig_provider_id();
        assert_eq!(id.as_str(), "ig");
        assert!(id.is_reserved());
        assert!(ProviderId::new(IG_ID).is_ok());
    }

    #[test]
    fn test_ig_capabilities_match_section_8_row() {
        let caps = ig_capabilities();
        assert_eq!(caps.chain, ChainCapability::Partial);
        assert!(!caps.depth, "IG populates no option depth ladder (#50)");
        assert_eq!(caps.greeks, GreeksCapability::ComputedLocally);
        assert_eq!(
            caps.option_stream,
            OptionStreamCapability::ChainQuotes { verified: false }
        );
        assert!(
            !caps.underlying_stream,
            "IG folds no underlying quote (only option epics stream; #40/#41 honesty)"
        );
        assert_eq!(
            caps.chain_poll,
            ChainPollCapability::Poll {
                interval_hint_secs: REFRESH_HINT_SECS
            }
        );
        assert!(!caps.trades_tape, "IG's trade stream is deal confirmations");
        assert_eq!(caps.auth, AuthKind::UserPass);
    }

    #[test]
    fn test_adapter_reports_capabilities_and_id_via_trait() {
        let adapter: Box<dyn Provider> = Box::new(sample_adapter());
        assert_eq!(adapter.id().as_str(), "ig");
        assert_eq!(adapter.capabilities().chain, ChainCapability::Partial);
        assert_eq!(
            adapter.capabilities().greeks,
            GreeksCapability::ComputedLocally
        );
    }

    // === Credentials: env-only, never logged =================================

    #[test]
    fn test_from_env_reads_chainview_namespace_only() {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_IG_USERNAME".to_owned(), "alice".to_owned());
        let _ = env.insert("CHAINVIEW_IG_PASSWORD".to_owned(), "pw".to_owned());
        let _ = env.insert("CHAINVIEW_IG_API_KEY".to_owned(), "key".to_owned());
        // A foreign-namespace value must be ignored.
        let _ = env.insert("IG_USERNAME".to_owned(), "foreign".to_owned());
        let adapter = match IgAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed: {e}"),
        };
        assert_eq!(adapter.username.expose(), "alice");
        assert_eq!(adapter.api_key.expose(), "key");
        assert_eq!(adapter.environment, IgEnvironment::Demo);
        assert_eq!(adapter.account_id, "");
    }

    #[test]
    fn test_from_env_environment_and_account_id() {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_IG_USERNAME".to_owned(), "u".to_owned());
        let _ = env.insert("CHAINVIEW_IG_PASSWORD".to_owned(), "p".to_owned());
        let _ = env.insert("CHAINVIEW_IG_API_KEY".to_owned(), "k".to_owned());
        let _ = env.insert("CHAINVIEW_IG_ACCOUNT_ID".to_owned(), "ABC123".to_owned());
        let _ = env.insert("CHAINVIEW_IG_ENVIRONMENT".to_owned(), "LIVE".to_owned());
        match IgAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => {
                assert_eq!(adapter.environment, IgEnvironment::Live);
                assert_eq!(adapter.account_id, "ABC123");
            }
            Err(e) => panic!("from_env should succeed: {e}"),
        }
    }

    #[test]
    fn test_from_env_missing_credential_is_error() {
        let mut env = HashMap::new();
        let _ = env.insert("CHAINVIEW_IG_USERNAME".to_owned(), "alice".to_owned());
        // Password + api_key absent.
        match IgAdapter::from_env(&MapEnv(env)) {
            Err(crate::error::ConfigError::MissingCredential(id)) => {
                assert_eq!(id.as_str(), "ig");
            }
            Err(other) => panic!("expected MissingCredential, got: {other}"),
            Ok(_) => panic!("expected MissingCredential, got Ok"),
        }
    }

    #[test]
    fn test_secret_debug_never_reveals_credentials() {
        let adapter = sample_adapter();
        let rendered = format!("{:?} {:?}", adapter.password, adapter.api_key);
        assert!(!rendered.contains("do-not-log-this-password"));
        assert!(!rendered.contains("do-not-log-this-key"));
        assert!(rendered.contains("redacted"));
    }

    // === Captured-log proof: credentials never appear in a log or error ======
    //
    // docs/TESTING.md §13.1 (the alpaca #99 precedent, adapted): drive the
    // adapter's construction + error path under a capturing subscriber and assert
    // the credential strings never appear. IG needs no live socket for this: the
    // credential guarantee is that `from_env` -> `Config::from_credentials` never
    // logs, and a redaction-safe `ProviderError` never carries a secret.

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<StdMutex<Vec<u8>>>);

    impl LogBuffer {
        fn contents(&self) -> String {
            match self.0.lock() {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(poisoned) => String::from_utf8_lossy(&poisoned.into_inner()).into_owned(),
            }
        }
    }

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut bytes) = self.0.lock() {
                bytes.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogBuffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    const CONTROL_CANARY: &str = "chainview-ig-canary-4b2e-present";

    #[test]
    fn test_construction_and_errors_never_log_credentials() {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // Prove the sink captures content, so the redaction assertion is non-vacuous.
        tracing::debug!("{CONTROL_CANARY}");

        let adapter = sample_adapter();
        // The upstream Config Debug/Display redact; render it to confirm no leak.
        tracing::debug!(config = ?adapter.upstream_config(), "ig config built");
        tracing::debug!(creds = ?adapter.credentials(), "ig credentials built");

        // A redaction-safe ProviderError never carries a secret.
        let err = ig_error(AppError::Auth(ig_client::error::AuthError::Other(
            "boom".to_owned(),
        )));
        tracing::debug!(error = %err, "ig error rendered");

        let output = logs.contents();
        assert!(output.contains(CONTROL_CANARY), "sink must capture content");
        assert!(
            !output.contains("do-not-log-this-password"),
            "the password leaked into logs:\n{output}"
        );
        assert!(
            !output.contains("do-not-log-this-key"),
            "the api key leaked into logs:\n{output}"
        );
        assert!(
            !format!("{err} {err:?}").contains("do-not-log-this-key"),
            "a ProviderError must never carry a credential"
        );
    }

    // === Expiry resolution ====================================================

    #[test]
    fn test_expiry_date_only_resolves_via_uk_session_close() {
        // A GBP/default market: 16:30 Europe/London. 18-JUL-26 is BST (+1) ->
        // 15:30 UTC.
        let session = session_for(None);
        match resolve_expiry(None, "18-JUL-26", &session) {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-07-18T15:30:00+00:00"),
            Err(e) => panic!("date-only UK expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_date_only_us_session_close() {
        // A USD market: 16:00 US Eastern. 18-JUL-26 is EDT (-4) -> 20:00 UTC.
        let session = session_for(Some("USD"));
        match resolve_expiry(None, "18-JUL-26", &session) {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-07-18T20:00:00+00:00"),
            Err(e) => panic!("date-only US expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_timestamped_field_wins_over_date_only() {
        // An authoritative naive lastDealingDate carries its own time-of-day and
        // WINS over the date-only session close. UK zone, BST (+1): 20:30 local ->
        // 19:30 UTC, which differs from the 15:30 date-only close.
        let session = session_for(None);
        let resolved = match resolve_expiry(Some("2026-07-18T20:30"), "18-JUL-26", &session) {
            Ok(dt) => dt,
            Err(e) => panic!("timestamped expiry should resolve, got: {e}"),
        };
        assert_eq!(resolved.to_rfc3339(), "2026-07-18T19:30:00+00:00");
        // It is NOT the date-only session close.
        let date_only = match resolve_expiry(None, "18-JUL-26", &session) {
            Ok(dt) => dt,
            Err(e) => panic!("date-only should resolve, got: {e}"),
        };
        assert_ne!(resolved, date_only, "the timestamped field must win");
    }

    #[test]
    fn test_expiry_timestamped_rfc3339_offset_is_absolute() {
        let session = session_for(None);
        match resolve_expiry(Some("2026-07-18T16:30:00-04:00"), "18-JUL-26", &session) {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-07-18T20:30:00+00:00"),
            Err(e) => panic!("offset-carrying timestamp should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_ambiguous_and_unparseable_are_normalize_errors() {
        let session = session_for(None);
        // Unparseable date-only.
        assert_eq!(
            resolve_expiry(None, "not-a-date", &session),
            Err(NormalizeKind::UnparseableExpiry)
        );
        // Present-but-garbage authoritative field is an error, never a silent
        // fall-through to the date-only value.
        assert_eq!(
            resolve_expiry(Some("garbage-stamp"), "18-JUL-26", &session),
            Err(NormalizeKind::UnparseableExpiry)
        );
        // A malformed month abbreviation.
        assert_eq!(
            resolve_expiry(None, "18-XXX-26", &session),
            Err(NormalizeKind::UnparseableExpiry)
        );
    }

    #[test]
    fn test_dst_boundaries() {
        // UK: last Sunday of March 2026 = 2026-03-29 (BST at 16:30).
        assert!(uk_dst(date(2026, 3, 29)));
        assert!(!uk_dst(date(2026, 3, 28)));
        // Last Sunday of October 2026 = 2026-10-25 (back to GMT).
        assert!(!uk_dst(date(2026, 10, 25)));
        assert!(uk_dst(date(2026, 10, 24)));
        // US Eastern: second Sunday of March 2026 = 2026-03-08.
        assert!(us_eastern_dst(date(2026, 3, 8)));
        assert!(!us_eastern_dst(date(2026, 3, 7)));
    }

    #[test]
    fn test_parse_ig_date_forms() {
        assert_eq!(parse_ig_date("18-JUL-26"), Some(date(2026, 7, 18)));
        assert_eq!(parse_ig_date("2026-07-18"), Some(date(2026, 7, 18)));
        assert_eq!(parse_ig_date("18-XXX-26"), None);
        assert_eq!(parse_ig_date("garbage"), None);
    }

    // === Numeric normalization at the seam ====================================

    #[test]
    fn test_strike_positive_parses_and_rejects() {
        match strike_positive("7500") {
            Ok(strike) => assert_eq!(strike, pos(7500.0)),
            Err(e) => panic!("7500 should parse, got: {e}"),
        }
        match strike_positive("10.5") {
            Ok(strike) => assert_eq!(strike, pos(10.5)),
            Err(e) => panic!("10.5 should parse, got: {e}"),
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
    fn test_normalize_quote_rules() {
        match normalize_quote(Some(0.0), Some(1.0)) {
            Ok(q) => {
                assert_eq!(q.bid, Some(Positive::ZERO));
                assert_eq!(q.ask, Some(pos(1.0)));
            }
            Err(e) => panic!("zero bid valid, got: {e}"),
        }
        assert_eq!(
            normalize_quote(Some(5.0), Some(3.0)),
            Err(NormalizeKind::OutOfRange("ask"))
        );
        assert_eq!(
            normalize_quote(Some(5.0), Some(0.0)),
            Err(NormalizeKind::OutOfRange("ask"))
        );
        // A NaN field drops only that field.
        match normalize_quote(Some(f64::NAN), Some(2.0)) {
            Ok(q) => {
                assert_eq!(q.bid, None);
                assert_eq!(q.ask, Some(pos(2.0)));
            }
            Err(e) => panic!("NaN bid drops only that field, got: {e}"),
        }
    }

    // === Navigation-tree assembly from a recorded shape =======================

    #[test]
    fn test_assemble_chain_from_recorded_navigation() {
        let markets = navigation_markets();
        let assembled = match assemble_chain(&markets, "SPX", fixture_expiry_utc(), &pid("ig")) {
            Ok(a) => a,
            Err(e) => panic!("assembly should succeed, got: {e}"),
        };
        // The recorded shape has 7400/7500/7600 strikes, each call+put.
        let strikes: Vec<Positive> = assembled
            .fetch
            .chain
            .options
            .iter()
            .map(|o| o.strike_price)
            .collect();
        assert_eq!(strikes, vec![pos(7400.0), pos(7500.0), pos(7600.0)]);
        // Six option legs (3 strikes x call/put) in the alias catalog.
        let leg_count = assembled
            .fetch
            .aliases
            .instruments()
            .filter(|i| i.provider == pid("ig"))
            .count();
        assert_eq!(leg_count, 6);
        // The chain expiry is the absolute UTC instant, not a relative offset.
        assert_eq!(
            assembled.fetch.expiry_source.expiration_utc,
            fixture_expiry_utc()
        );
        // A non-option navigation entry (the underlying future) contributes no leg.
        assert!(
            !assembled
                .fetch
                .aliases
                .instruments()
                .any(|i| i.native_symbol == "IX.D.SPX.DAILY.IP"),
        );
    }

    #[test]
    fn test_assemble_chain_wrong_expiry_is_no_chain() {
        let markets = navigation_markets();
        let other = utc_rfc3339("2027-01-15T20:00:00+00:00");
        match assemble_chain(&markets, "SPX", other, &pid("ig")) {
            Err(ProviderError::NoChain { underlying, .. }) => assert_eq!(underlying, "SPX"),
            other => panic!("expected NoChain for a mismatched expiry, got {other:?}"),
        }
    }

    #[test]
    fn test_epic_root_extracts_product_code() {
        assert_eq!(epic_root("OP.D.SPX2.7500C.IP"), "OP.D.SPX2");
        assert_eq!(epic_root("SIMPLE"), "SIMPLE");
    }

    // === Local Greeks: the store computes them, tagged ComputedLocally ========

    #[track_caller]
    fn seeded_store() -> ChainStore {
        let markets = navigation_markets();
        let assembled = match assemble_chain(&markets, "SPX", fixture_expiry_utc(), &pid("ig")) {
            Ok(a) => a,
            Err(e) => panic!("assembly should succeed, got: {e}"),
        };
        // Seed a store before the expiry so time-to-expiry is positive.
        let seeded_at = utc_rfc3339("2026-07-01T14:00:00+00:00");
        ChainStore::seed(
            assembled.fetch,
            ChainSource::Merged,
            Duration::from_secs(2),
            seeded_at,
        )
    }

    #[test]
    fn test_local_greeks_are_computed_and_tagged_computed_locally() {
        let store = seeded_store();
        // The seed pass runs the local Greeks engine over the assembled two-sided
        // quotes; every leg's analytics are ComputedLocally (IG has no venue Greeks).
        let key = InstrumentKey {
            underlying: "SPX".to_owned(),
            expiration_utc: fixture_expiry_utc(),
            strike: pos(7500.0),
            style: OptionStyle::Call,
        };
        match store.leg_greeks(&key) {
            Some(leg) => {
                assert_eq!(leg.status, LegStatus::Computed);
                assert!(
                    leg.iv.is_some(),
                    "IV inverted locally from the two-sided quote"
                );
                assert_eq!(leg.iv_origin, GreeksOrigin::ComputedLocally);
                assert!(leg.delta.is_some());
                assert_eq!(leg.delta_origin, GreeksOrigin::ComputedLocally);
                assert!(leg.theta.is_some());
                assert_eq!(leg.theta_origin, GreeksOrigin::ComputedLocally);
                assert!(leg.vega.is_some());
                assert!(leg.gamma.is_some());
                assert_eq!(leg.gamma_origin, GreeksOrigin::ComputedLocally);
            }
            None => panic!("expected a computed-locally sidecar entry for the 7500 call"),
        }
    }

    #[test]
    fn test_crossed_input_leaves_leg_none_never_a_stale_greek() {
        // A one-strike chain with a CROSSED call quote: the local engine must leave
        // the crossed leg's analytics None (LegStatus::Crossed), never a fabricated
        // Greek.
        let mut chain = OptionChain::new(
            "SPX",
            pos(7500.0),
            fixture_expiry_utc().to_rfc3339(),
            None,
            None,
        );
        // call bid 5.0 > ask 3.0 -> crossed; put is a valid two-sided quote.
        chain.add_option(
            pos(7500.0),
            Some(pos(5.0)),
            Some(pos(3.0)),
            Some(pos(4.0)),
            Some(pos(4.5)),
            Positive::ZERO,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let ctx = PricingInputs::new(pos(7500.0), utc_rfc3339("2026-07-01T14:00:00+00:00"), 1);
        let mut sidecar = crate::chain::GreeksSidecar::new();
        match compute_leg_greeks(&chain, &ctx, &QuoteClocks::new(), &mut sidecar) {
            Ok(()) => {}
            Err(e) => panic!("compute_leg_greeks failed: {e}"),
        }
        let call_key = InstrumentKey {
            underlying: "SPX".to_owned(),
            expiration_utc: fixture_expiry_utc(),
            strike: pos(7500.0),
            style: OptionStyle::Call,
        };
        match sidecar.get(&call_key) {
            Some(leg) => {
                assert_eq!(leg.status, LegStatus::Crossed);
                assert!(
                    leg.iv.is_none(),
                    "a crossed input never yields a computed IV"
                );
                assert!(leg.delta.is_none());
            }
            None => panic!("expected a cleared sidecar entry for the crossed call"),
        }
        // The uncrossed put still computes.
        let put_key = InstrumentKey {
            style: OptionStyle::Put,
            ..call_key
        };
        match sidecar.get(&put_key) {
            Some(leg) => assert_eq!(leg.status, LegStatus::Computed),
            None => panic!("expected a computed entry for the uncrossed put"),
        }
    }

    // === discover ============================================================

    struct MockSource {
        markets: Vec<RawMarket>,
        roots: Vec<String>,
    }

    #[async_trait]
    impl IgMarketSource for MockSource {
        async fn search(&self, _term: &str) -> Result<Vec<RawMarket>, ProviderError> {
            Ok(self.markets.clone())
        }
        async fn navigation_roots(&self) -> Result<Vec<String>, ProviderError> {
            Ok(self.roots.clone())
        }
    }

    #[tokio::test]
    async fn test_discover_returns_navigation_roots_as_underlyings() {
        let source = MockSource {
            markets: Vec::new(),
            roots: vec!["Indices".to_owned(), "Shares".to_owned()],
        };
        match discover_underlyings(&source).await {
            Ok(refs) => {
                let names: Vec<String> = refs.into_iter().map(|r| r.underlying).collect();
                assert_eq!(names, vec!["Indices".to_owned(), "Shares".to_owned()]);
            }
            Err(e) => panic!("discover should succeed, got: {e}"),
        }
    }

    #[tokio::test]
    async fn test_compose_chain_over_mock_source() {
        let source = MockSource {
            markets: navigation_markets(),
            roots: Vec::new(),
        };
        let expiration = ExpirationDate::DateTime(fixture_expiry_utc());
        let received = utc_rfc3339("2026-07-01T14:00:00+00:00");
        match compose_chain(&source, "spx", &expiration, &pid("ig"), received).await {
            Ok(assembled) => {
                assert_eq!(assembled.fetch.chain.symbol, "SPX");
                assert_eq!(assembled.fetch.chain.options.len(), 3);
            }
            Err(e) => panic!("compose_chain should succeed, got: {e}"),
        }
    }

    // === Streaming lifecycle over a mock transport (no socket, no wall clock) ==

    /// A scripted mock stream transport: `attempts[n]` is the event list for
    /// connection attempt `n`, drained then a drop; once every attempt is exhausted
    /// it cancels the token so the loop stops. It records each connect and serves a
    /// composed backfill on `poll`.
    struct MockTransport {
        attempts: Vec<Vec<RawPriceEvent>>,
        attempt_idx: usize,
        cursor: usize,
        backfill: Option<AssembledChain>,
        connects: Arc<StdMutex<u32>>,
        cancel: CancellationToken,
    }

    #[async_trait]
    impl IgStreamTransport for MockTransport {
        async fn connect_and_subscribe(&mut self, _epics: &[String]) -> Result<(), TransportGone> {
            if let Ok(mut count) = self.connects.lock() {
                *count += 1;
            }
            self.cursor = 0;
            Ok(())
        }

        async fn receive(&mut self) -> Result<RawPriceEvent, TransportGone> {
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
        ) -> Option<AssembledChain> {
            self.backfill.clone()
        }
    }

    struct PendingTransport;

    #[async_trait]
    impl IgStreamTransport for PendingTransport {
        async fn connect_and_subscribe(&mut self, _epics: &[String]) -> Result<(), TransportGone> {
            Ok(())
        }
        async fn receive(&mut self) -> Result<RawPriceEvent, TransportGone> {
            std::future::pending::<()>().await;
            Err(TransportGone)
        }
        async fn poll(
            &mut self,
            _underlying: &str,
            _expiration: &ExpirationDate,
            _received: DateTime<Utc>,
        ) -> Option<AssembledChain> {
            None
        }
    }

    #[track_caller]
    fn fixture_assembled() -> AssembledChain {
        let markets = navigation_markets();
        match assemble_chain(&markets, "SPX", fixture_expiry_utc(), &pid("ig")) {
            Ok(a) => a,
            Err(e) => panic!("assembly should succeed, got: {e}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_reconnect_loop_emits_reconnecting_and_backfills_without_panic() {
        // Attempt 0 delivers a valid per-epic quote then drops; attempt 1 the same,
        // then the loop stops. Each connect re-polls the chain (backfill) and the
        // drop surfaces Health(Reconnecting).
        let assembled = fixture_assembled();
        let cancel = CancellationToken::new();
        let connects = Arc::new(StdMutex::new(0));
        // Resolve a real epic from the fixture to drive a quote through.
        let epic = match assembled
            .fetch
            .aliases
            .instruments()
            .find(|i| i.provider == pid("ig"))
        {
            Some(instrument) => instrument.native_symbol.clone(),
            None => panic!("the fixture must carry at least one epic"),
        };
        let quote = RawPriceEvent {
            epic,
            bid: Some(12.5),
            offer: Some(13.5),
        };
        let transport = MockTransport {
            attempts: vec![vec![quote.clone()], vec![quote]],
            attempt_idx: 0,
            cursor: 0,
            backfill: Some(assembled.clone()),
            connects: Arc::clone(&connects),
            cancel: cancel.clone(),
        };
        let (sink, mut rx_control, mut rx_coalesced) = test_sink(256);
        run_reconnect_loop(
            transport,
            pid("ig"),
            "SPX".to_owned(),
            fixture_expiry_utc(),
            assembled.fetch.aliases.clone(),
            sink,
            cancel,
        )
        .await;

        assert_eq!(
            *connects.lock().unwrap_or_else(|e| e.into_inner()),
            2,
            "connected twice"
        );
        let control = drain(&mut rx_control);
        assert!(
            control.iter().any(|u| matches!(u, MarketUpdate::Chain(_))),
            "a Chain backfill was emitted"
        );
        assert!(
            control
                .iter()
                .any(|u| matches!(u, MarketUpdate::Health(_, StreamHealth::Live))),
            "a Live health was emitted"
        );
        assert!(
            control.iter().any(|u| matches!(
                u,
                MarketUpdate::Health(_, StreamHealth::Reconnecting { .. })
            )),
            "a Reconnecting health was emitted on the stream drop"
        );
        // The per-epic quote reached the coalesced channel.
        let coalesced = drain(&mut rx_coalesced);
        assert!(
            coalesced
                .iter()
                .any(|u| matches!(u, MarketUpdate::Quote(_))),
            "the normalized per-epic quote reached the coalesced channel"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_reconnect_loop_drops_unknown_epic_quote() {
        // A quote for an epic NOT in the aliases must be dropped (never keyed).
        let assembled = fixture_assembled();
        let cancel = CancellationToken::new();
        let connects = Arc::new(StdMutex::new(0));
        let stray = RawPriceEvent {
            epic: "OP.D.UNKNOWN.9999C.IP".to_owned(),
            bid: Some(1.0),
            offer: Some(2.0),
        };
        let transport = MockTransport {
            attempts: vec![vec![stray]],
            attempt_idx: 0,
            cursor: 0,
            backfill: Some(assembled.clone()),
            connects: Arc::clone(&connects),
            cancel: cancel.clone(),
        };
        let (sink, mut _rx_control, mut rx_coalesced) = test_sink(256);
        run_reconnect_loop(
            transport,
            pid("ig"),
            "SPX".to_owned(),
            fixture_expiry_utc(),
            assembled.fetch.aliases.clone(),
            sink,
            cancel,
        )
        .await;
        let coalesced = drain(&mut rx_coalesced);
        assert!(
            !coalesced
                .iter()
                .any(|u| matches!(u, MarketUpdate::Quote(_))),
            "a quote for an unknown epic must be dropped, never keyed"
        );
    }

    #[tokio::test]
    async fn test_reconnect_loop_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let (sink, _rx_control, _rx_coalesced) = test_sink(8);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            PendingTransport,
            pid("ig"),
            "SPX".to_owned(),
            fixture_expiry_utc(),
            AliasCatalog::new(),
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

    // === Poll->stream merge is order-independent (spot check) =================

    #[test]
    fn test_stream_quote_merges_into_seeded_store() {
        let mut store = seeded_store();
        let key = InstrumentKey {
            underlying: "SPX".to_owned(),
            expiration_utc: fixture_expiry_utc(),
            strike: pos(7500.0),
            style: OptionStyle::Call,
        };
        let instrument = Instrument {
            key: key.clone(),
            provider: pid("ig"),
            native_symbol: "OP.D.SPX2.7500C.IP".to_owned(),
            stream_symbol: Some("MARKET:OP.D.SPX2.7500C.IP".to_owned()),
            spec: ig_fingerprint("OP.D.SPX2", DEFAULT_QUOTE_CURRENCY),
        };
        let event = RawPriceEvent {
            epic: "OP.D.SPX2.7500C.IP".to_owned(),
            bid: Some(20.0),
            offer: Some(21.0),
        };
        let mut aliases = AliasCatalog::new();
        aliases.insert(instrument);
        let update = match quote_update(&event, &aliases, &pid("ig"), now_utc()) {
            Some(u) => u,
            None => panic!("the quote should normalize"),
        };
        // The overlay merges onto the seeded strike (not Buffered).
        assert_eq!(store.apply_quote(&update), MergeOutcome::Applied);
        let _ = key;
    }

    // === Property: normalization is total (never a panic) =====================

    proptest::proptest! {
        /// `resolve_expiry` is TOTAL: any strings yield `Ok` or `UnparseableExpiry`,
        /// never a panic (contributes to normalize_total).
        #[test]
        fn prop_resolve_expiry_total(
            ts in "\\PC{0,20}",
            date_str in "\\PC{0,20}",
        ) {
            let session = session_for(None);
            let ts_opt = if ts.is_empty() { None } else { Some(ts.as_str()) };
            let _ = resolve_expiry(ts_opt, &date_str, &session);
        }

        /// `strike_positive` is TOTAL over any string (contributes to normalize_total).
        #[test]
        fn prop_strike_positive_total(raw in "\\PC{0,12}") {
            match strike_positive(&raw) {
                Ok(strike) => proptest::prop_assert!(strike > Positive::ZERO),
                Err(kind) => proptest::prop_assert_eq!(kind, NormalizeKind::OutOfRange("strike")),
            }
        }

        /// `normalize_quote` is TOTAL over any bid/ask pair (contributes to
        /// normalize_total / normalize_rejects_unknown).
        #[test]
        fn prop_normalize_quote_total(bid in -1.0e6f64..1.0e6, ask in -1.0e6f64..1.0e6) {
            match normalize_quote(Some(bid), Some(ask)) {
                Ok(quote) => {
                    if let (Some(b), Some(a)) = (quote.bid, quote.ask) {
                        proptest::prop_assert!(a >= b, "an accepted quote is never crossed");
                    }
                }
                Err(kind) => proptest::prop_assert_eq!(kind, NormalizeKind::OutOfRange("ask")),
            }
        }
    }
}
