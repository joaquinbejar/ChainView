//! The Interactive Brokers adapter — the TWS/Gateway socket, native-Greeks
//! poll->stream provider (issue #120, `docs/03-data-providers.md` §7.6). Behind
//! the DISABLED-by-default `ibkr` Cargo feature (a **dependency-weight** gate,
//! [ADR-0014], not a credential-logging security gate), so under the feature it is
//! a **real built-in** registered by [`with_builtins`](crate::ChainViewAppBuilder::with_builtins)
//! when its `CHAINVIEW_IBKR_ENDPOINT` is configured.
//!
//! IBKR exposes an option chain through `reqSecDefOptParams`
//! ([`Client::option_chain`](ibapi::Client::option_chain)) — expiries + strikes +
//! trading class + multiplier for an underlying — plus per-contract
//! `contract_details` for the authoritative last-trade-time. The adapter
//! **assembles** the strike ladder from that ([`ChainCapability::Assemble`]) and
//! overlays live quotes + **native** model Greeks/IV streamed by
//! `tickOptionComputation` (venue-provided, so [`GreeksCapability::Provided`], NOT
//! [`ComputedLocally`](crate::providers::GreeksCapability::ComputedLocally)).
//!
//! # Auth lives inside TWS/Gateway — ChainView carries no credential
//!
//! Authentication happens INSIDE the operator's own TWS/IB Gateway; ChainView
//! only opens a local socket to it. [`from_env`](IbkrAdapter::from_env) reads the
//! ChainView-namespaced `CHAINVIEW_IBKR_ENDPOINT` (host:port, e.g.
//! `127.0.0.1:7497`) and the optional `CHAINVIEW_IBKR_CLIENT_ID` (an `i32`, default
//! [`DEFAULT_CLIENT_ID`]) and connects via
//! [`Client::connect`](ibapi::Client::connect). There is **no** secret at the seam
//! ([`AuthKind::None`]): the endpoint/client-id are non-secret config, read
//! env-only, never logged, and never echoed in a [`ProviderError`] (the error
//! mapping is category-only). `ibapi 3.3.0` performs no dotenv load, reads no
//! foreign env namespace, and installs no global tracing subscriber on
//! construction (verified against the 3.3.0 checkout).
//!
//! # Expiry -> one absolute UTC instant at the seam
//!
//! IBKR expiries are `YYYYMMDD` strings on the `reqSecDefOptParams` result; the
//! authoritative `last_trade_time` on a `contract_details` snapshot **wins** when
//! present. A date-only expiry resolves through the option's exchange session
//! close in a **bounded**, DST-aware IANA-zone set (no timezone database is
//! pulled); an ambiguous / unparseable expiry is a typed
//! [`ProviderError::Normalize`] with [`NormalizeKind::UnparseableExpiry`], never a
//! silently-keyed row (`docs/03-data-providers.md` §3, `docs/01-domain-model.md`
//! §4).
//!
//! # Native Greeks/IV via `tickOptionComputation`
//!
//! IBKR streams model Greeks + IV natively; the adapter normalizes each
//! `OptionComputation` to a [`GreeksRow`] tagged [`GreeksOrigin::Provider`] (the
//! venue provides them). Every `f64` is checked into [`Decimal`]/[`Positive`] at
//! the seam (`NaN`/`Inf` guarded); a crossed/stale computation whose analytics all
//! drop leaves the leg `None`, never a fabricated Greek.
//!
//! # The subscribe leg is PACING-BOUNDED
//!
//! IBKR market-data lines are paced (~100 concurrent by default). The adapter
//! NEVER fires an unbounded `reqMktData`: the reconnect loop subscribes an
//! **ATM-centered window** of at most [`MAX_SUBSCRIPTIONS`] contracts
//! ([`pacing_window`]), and the transport enforces the same cap belt-and-suspenders.
//!
//! # Numerics at the f64 seam
//!
//! IBKR prices/Greeks are `f64`. Every price/strike is checked into a [`Positive`]
//! (rejecting `NaN`/`Inf`/negative) and every Greek into a [`Decimal`] (rejecting
//! `NaN`/`Inf`) before it enters the chain; a crossed quote drops only the quote.
//! `f64` never flows past `src/providers/*` (`CLAUDE.md` governance deviation 2).
//!
//! # Reconnect + resubscribe (`docs/03-data-providers.md` §5, [ADR-0009])
//!
//! The reconnect/resubscribe loop is **ChainView's**, driven behind the
//! [`SubscriptionHandle`]: on a dropped stream it emits `Health(Reconnecting)`,
//! backs off with jittered exponential backoff (never a hot loop), re-polls the
//! chain (the backfill), and **resubscribes off the fresh [`ChainFetch`] aliases**
//! (respecting the pacing governor). Every [`MarketUpdate`] is handed to the
//! two-class [`MarketUpdateSink`]. Raw `ibapi` DTOs stop in this module.
//!
//! [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md
//! [ADR-0014]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0014-ibkr-builtin-packaging.md

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeDelta, Utc, Weekday};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use ibapi::contracts::tick_types::TickType;
use ibapi::contracts::{
    Contract, ContractDetails, OptionChain as IbOptionChain, OptionComputation, OptionRight,
    SecurityType,
};
use ibapi::market_data::realtime::{TickPrice, TickSize, TickTypes};
use ibapi::prelude::StreamExt;
use ibapi::subscriptions::SubscriptionItem;

use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, MarketUpdateSink,
    OptionStreamCapability, Provider, ProviderCapabilities, SendState, SubscriptionHandle,
    SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSnapshot, ChainSource, ContractSpecFingerprint, ExerciseStyle,
    ExpirySource, GreeksOrigin, GreeksRow, Instrument, InstrumentKey, MarketUpdate, ProviderId,
    QuoteUpdate, SettlementStyle, StreamHealth,
};
use crate::config::{EnvSource, provider_env_var};
use crate::error::{ConfigError, NormalizeKind, ProviderError, TransportDetail, TransportKind};

/// The reserved provider id this adapter registers under
/// ([`RESERVED_PROVIDER_IDS`](crate::chain::RESERVED_PROVIDER_IDS)).
const IBKR_ID: &str = "ibkr";

/// The required non-secret endpoint (host:port) key; its absence omits the
/// built-in (the alpaca/ig omit-on-missing pattern).
const ENDPOINT_KEY: &str = "ENDPOINT";

/// The optional non-secret client-id key (an `i32`); absent -> [`DEFAULT_CLIENT_ID`].
const CLIENT_ID_KEY: &str = "CLIENT_ID";

/// The default TWS/Gateway API client id when `CHAINVIEW_IBKR_CLIENT_ID` is unset.
const DEFAULT_CLIENT_ID: i32 = 1;

/// The pacing cap on concurrent market-data subscriptions (`docs/03-data-providers.md`
/// §7.6). IBKR paces market-data lines (~100 concurrent by default), so the
/// subscribe leg is bounded to an ATM-centered window of this many option
/// contracts — the adapter NEVER fires unbounded `reqMktData`.
const MAX_SUBSCRIPTIONS: usize = 100;

/// The assembled-strike ceiling — a runaway guard on chain assembly. A single
/// underlying's `reqSecDefOptParams` strike ladder is a few hundred entries; this
/// is a safety valve, never a normal limit.
const MAX_STRIKES: usize = 4_096;

/// The cap name carried by the typed limit error when assembly exceeds
/// [`MAX_STRIKES`] (a compile-time `&'static str`).
const STRIKE_CAP: &str = "ibkr strike cap";

/// The suggested chain-refresh cadence, in seconds — a hint only; the effective
/// interval is `config.refresh_interval`.
const REFRESH_HINT_SECS: u32 = 5;

/// The default listed-option contract multiplier when the venue multiplier will
/// not parse (US listed options are `100x`).
const DEFAULT_CONTRACT_MULTIPLIER: u32 = 100;

/// The default premium quote currency for a US listed option.
const DEFAULT_QUOTE_CURRENCY: &str = "USD";

/// The bounded, documented set of candidate option underlyings [`discover`] lists
/// (`docs/03-data-providers.md` §7.6). IBKR has no cheap "list every optionable
/// underlying" call, so `discover` returns this known-liquid US set rather than a
/// runaway contract scan; a caller resolves any one via
/// [`fetch_chain`](IbkrAdapter::fetch_chain).
const CANDIDATE_UNDERLYINGS: [&str; 10] = [
    "SPX", "SPY", "QQQ", "IWM", "AAPL", "MSFT", "NVDA", "AMZN", "TSLA", "META",
];

/// The underlyings ChainView treats as cash-settled **index** options (security
/// type `Index`); anything else is a `Stock`. A small, documented set.
const INDEX_UNDERLYINGS: [&str; 5] = ["SPX", "NDX", "RUT", "VIX", "XSP"];

/// The bounded wall-clock cap on collecting the `reqSecDefOptParams` snapshot
/// (the subscription self-terminates on its end sentinel; this is only a safety
/// bound).
const OPTION_PARAMS_TIMEOUT_SECS: u64 = 10;

// --- Reconnect backoff (docs/03-data-providers.md §5) ------------------------

/// The reconnect backoff base, in milliseconds (`BASE = 250 ms`).
const BACKOFF_BASE_MS: f64 = 250.0;
/// The reconnect backoff ceiling, in milliseconds (`MAX = 30 s`).
const BACKOFF_MAX_MS: f64 = 30_000.0;
/// The reconnect jitter magnitude — the delay is scaled by `1 + jitter`,
/// `jitter in [-0.2, 0.2]`.
const JITTER_MAGNITUDE: f64 = 0.2;
/// The largest exponent applied to `2^attempt` before the [`BACKOFF_MAX_MS`] cap.
const BACKOFF_MAX_SHIFT: u32 = 20;

// ---------------------------------------------------------------------------
// The adapter.
// ---------------------------------------------------------------------------

/// The IBKR `Provider` adapter (crate-internal; behind the disabled `ibkr`
/// feature).
///
/// Holds the reserved [`ProviderId`], the non-secret TWS/Gateway `endpoint`
/// (host:port), and the API `client_id`. There is NO credential — the gateway
/// holds the session. `Clone` is cheap so a clone can move into the spawned
/// reconnect loop.
#[derive(Clone)]
pub(crate) struct IbkrAdapter {
    id: ProviderId,
    endpoint: String,
    client_id: i32,
}

impl std::fmt::Debug for IbkrAdapter {
    /// Redacts the endpoint and client-id: neither is a secret, but the
    /// "never log the endpoint/client-id in a way that could leak" guarantee holds
    /// even through `Debug`, so a stray `{adapter:?}` cannot surface them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IbkrAdapter")
            .field("id", &self.id)
            .field("endpoint", &"<redacted>")
            .field("client_id", &"<redacted>")
            .finish()
    }
}

impl IbkrAdapter {
    /// Build the adapter from the ChainView-namespaced environment
    /// (`CHAINVIEW_IBKR_ENDPOINT`, optional `CHAINVIEW_IBKR_CLIENT_ID`). Read
    /// **only** here (env-only policy).
    ///
    /// # Errors
    ///
    /// [`ConfigError::MissingCredential`] (naming the provider) when the required
    /// endpoint is unset/empty — the registry then OMITS the built-in (the
    /// alpaca/ig pattern), so the endpoint is the required configuration whose
    /// absence disables IBKR, even though IBKR carries no secret.
    /// [`ConfigError::InvalidValue`] for a malformed endpoint or client-id (a
    /// present-but-bad value fails startup loudly, never silently omits).
    pub(crate) fn from_env(env: &dyn EnvSource) -> Result<Self, ConfigError> {
        let id = ibkr_provider_id();
        let endpoint = env
            .get(&provider_env_var(id.as_str(), ENDPOINT_KEY))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let Some(endpoint) = endpoint else {
            return Err(ConfigError::MissingCredential(id));
        };
        if !is_valid_endpoint(&endpoint) {
            return Err(ConfigError::InvalidValue {
                field: "ibkr endpoint".to_owned(),
                reason: "CHAINVIEW_IBKR_ENDPOINT must be host:port (e.g. 127.0.0.1:7497)"
                    .to_owned(),
            });
        }
        let client_id = match env
            .get(&provider_env_var(id.as_str(), CLIENT_ID_KEY))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            Some(raw) => raw.parse::<i32>().map_err(|_| ConfigError::InvalidValue {
                field: "ibkr client id".to_owned(),
                reason: "CHAINVIEW_IBKR_CLIENT_ID must be an integer".to_owned(),
            })?,
            None => DEFAULT_CLIENT_ID,
        };
        Ok(Self {
            id,
            endpoint,
            client_id,
        })
    }
}

#[async_trait]
impl Provider for IbkrAdapter {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ibkr_capabilities()
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        // The bounded, documented candidate set — no network scan (§7.6).
        Ok(CANDIDATE_UNDERLYINGS
            .iter()
            .map(|symbol| UnderlyingRef::new(*symbol))
            .collect())
    }

    async fn fetch_chain(
        &self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        let source = LiveChainSource::connect(self).await?;
        let assembled = compose_chain(&source, underlying, expiration, &self.id, now_utc()).await?;
        Ok(assembled.fetch)
    }

    async fn subscribe(
        &self,
        req: SubscriptionRequest,
        sink: MarketUpdateSink,
    ) -> Result<SubscriptionHandle, ProviderError> {
        let transport = LiveStreamTransport::new(self.clone());
        let id = self.id.clone();
        let SubscriptionRequest {
            underlying,
            expiration_utc,
            instruments,
            cancel,
        } = req;
        // Seed the resubscribe aliases from the poll leg's catalog so the loop
        // never re-derives symbols from strikes (docs/03 §4).
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

/// The adapter's reserved [`ProviderId`]. `"ibkr"` is a compile-time literal that
/// satisfies the grammar (proven by `test_ibkr_id_is_valid_and_reserved`), so the
/// fallback arm is unreachable.
fn ibkr_provider_id() -> ProviderId {
    match ProviderId::new(IBKR_ID) {
        Ok(id) => id,
        Err(_) => unreachable!("`ibkr` is a valid, reserved provider id literal"),
    }
}

/// IBKR's honest capability self-declaration — the `docs/03-data-providers.md` §8
/// row: an **Assemble** chain (`reqSecDefOptParams` + contract details), **no**
/// depth (option L2 is subscription/venue-dependent and unproven from a recorded
/// fixture, so the honest claim is `false`), **venue-provided** Greeks/IV
/// (`tickOptionComputation`), an unverified per-contract `ChainQuotes` overlay,
/// **no** underlying stream (the subscribe leg carries only option contracts and
/// no `UnderlyingQuote` is folded — the #40/#41 honesty rule), REST chain polling,
/// **no** public trades tape, and **no** ChainView-side auth (the TWS/Gateway
/// holds the session).
#[must_use]
pub(crate) fn ibkr_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        // Depth is `false` (not claimed): IBKR can stream option L2, but ChainView
        // has no recorded option-epic depth fixture to prove it, so it declares no
        // depth rather than an aspirational cell.
        .depth(false)
        // Venue-provided native model Greeks + IV via `tickOptionComputation`.
        .greeks(GreeksCapability::Provided)
        .option_stream(OptionStreamCapability::ChainQuotes { verified: false })
        // FALSE per the #40/#41 honesty rule: the subscribe leg subscribes ONLY
        // option contracts (never the underlying stock/index contract), and no
        // `MarketUpdate::UnderlyingQuote` is folded — the OptionComputation's
        // `underlying_price` is not surfaced as an underlying stream. Returns to
        // true only when the underlying contract is subscribed AND folded.
        .underlying_stream(false)
        .chain_poll(ChainPollCapability::Poll {
            interval_hint_secs: REFRESH_HINT_SECS,
        })
        .trades_tape(false)
        .auth(AuthKind::None)
        .build()
}

/// Validate a `host:port` endpoint shape (bounded): a non-empty host, a `:`, and a
/// `u16` port. Rejects anything else so a malformed endpoint fails startup rather
/// than reaching [`Client::connect`](ibapi::Client::connect).
fn is_valid_endpoint(endpoint: &str) -> bool {
    let Some((host, port)) = endpoint.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && !port.is_empty() && port.parse::<u16>().is_ok()
}

/// Whether an underlying is treated as a cash-settled index option.
fn is_index_underlying(underlying: &str) -> bool {
    INDEX_UNDERLYINGS.contains(&underlying)
}

/// The IBKR security type used to look up an underlying's option parameters.
fn security_type_for(underlying: &str) -> SecurityType {
    if is_index_underlying(underlying) {
        SecurityType::Index
    } else {
        SecurityType::Stock
    }
}

// ---------------------------------------------------------------------------
// Expiry resolution: authoritative last-trade-time wins, else date-only via
// session close, DST-aware in a bounded IANA-zone set (no timezone DB pulled).
// ---------------------------------------------------------------------------

/// A supported market session zone — a **bounded** set (ChainView pulls no
/// timezone database, `docs/03-data-providers.md` §3), so an expiry whose exchange
/// maps to no supported zone resolves through the default rather than a fabricated
/// instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionZone {
    /// US Eastern (America/New_York): EST (`-5`) or EDT (`-4`).
    UsEastern,
    /// UK (Europe/London): GMT (`+0`) or BST (`+1`).
    UkLondon,
}

/// A market's session close — the zone plus the local close time-of-day used to
/// resolve a **date-only** expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionClose {
    zone: SessionZone,
    hour: u32,
    minute: u32,
}

impl SessionClose {
    /// Construct a session close.
    const fn new(zone: SessionZone, hour: u32, minute: u32) -> Self {
        Self { zone, hour, minute }
    }
}

/// The default IBKR session close — US Eastern 16:00 (US listed options), the
/// dominant case for the candidate set.
fn default_session() -> SessionClose {
    SessionClose::new(SessionZone::UsEastern, 16, 0)
}

/// Resolve a session close from a `contract_details` time-zone id. `US/Eastern`,
/// `EST`, `EDT`, or an America/* id -> US Eastern; a London/GMT/BST/Europe id -> UK;
/// anything else -> the default (US Eastern), a documented, deterministic
/// heuristic.
fn session_from_zone_id(zone_id: &str) -> SessionClose {
    let upper = zone_id.to_ascii_uppercase();
    if upper.contains("LONDON") || upper.contains("GMT") || upper.contains("BST") {
        SessionClose::new(SessionZone::UkLondon, 16, 30)
    } else {
        default_session()
    }
}

/// Resolve an IBKR expiry to **one absolute UTC instant** (`docs/03-data-providers.md`
/// §3): the authoritative `last_trade_time` (paired with the `YYYYMMDD` date)
/// **wins** when present; an absent one falls to the date-only expiry resolved
/// through the exchange session close. A present-but-unparseable time, an
/// unparseable date, or a non-representable local instant is
/// [`NormalizeKind::UnparseableExpiry`].
///
/// # Errors
///
/// [`NormalizeKind::UnparseableExpiry`] for a malformed time, a malformed date, or
/// an unresolvable local instant.
fn resolve_expiry(
    last_trade_time: Option<&str>,
    ymd: &str,
    session: &SessionClose,
) -> Result<DateTime<Utc>, NormalizeKind> {
    let date = parse_ymd(ymd).ok_or(NormalizeKind::UnparseableExpiry)?;
    let naive = match last_trade_time.map(str::trim).filter(|s| !s.is_empty()) {
        // The authoritative field is present: it MUST resolve (a garbage value is
        // an error, never a silent fall-through to the date-only close).
        Some(stamp) => {
            parse_ib_last_trade_time(stamp, date).ok_or(NormalizeKind::UnparseableExpiry)?
        }
        None => date
            .and_hms_opt(session.hour, session.minute, 0)
            .ok_or(NormalizeKind::UnparseableExpiry)?,
    };
    local_to_utc(naive, session.zone).ok_or(NormalizeKind::UnparseableExpiry)
}

/// Parse an IBKR `YYYYMMDD` expiry date. Returns `None` for any other shape.
fn parse_ymd(s: &str) -> Option<NaiveDate> {
    let t = s.trim();
    if t.len() != 8 || !t.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let year = t.get(0..4)?.parse::<i32>().ok()?;
    let month = t.get(4..6)?.parse::<u32>().ok()?;
    let day = t.get(6..8)?.parse::<u32>().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Parse an IBKR `last_trade_time` into a naive local datetime, paired with the
/// expiry `date` when the field carries only a time-of-day. Accepts `HH:MM[:SS]`
/// (time only, combined with `date`) and `YYYYMMDD-HH:MM[:SS]` / `YYYYMMDD HH:MM[:SS]`
/// (date + time). Returns `None` for anything else.
fn parse_ib_last_trade_time(s: &str, date: NaiveDate) -> Option<NaiveDateTime> {
    let t = s.trim();
    if let Some((maybe_date, time_part)) = t.split_once('-').or_else(|| t.split_once(' '))
        && let Some(full_date) = parse_ymd(maybe_date)
    {
        return combine_date_time(full_date, time_part);
    }
    combine_date_time(date, t)
}

/// Combine a date with a `HH:MM[:SS]` time string into a naive datetime.
fn combine_date_time(date: NaiveDate, time_str: &str) -> Option<NaiveDateTime> {
    let mut parts = time_str.trim().split(':');
    let hour = parts.next()?.trim().parse::<u32>().ok()?;
    let minute = parts.next()?.trim().parse::<u32>().ok()?;
    let second = match parts.next() {
        Some(sec) => sec.trim().parse::<u32>().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    date.and_hms_opt(hour, minute, second)
}

/// The zone offset EAST of UTC, in hours (local = UTC + offset), for a date.
fn zone_offset_hours(date: NaiveDate, zone: SessionZone) -> i64 {
    match zone {
        SessionZone::UsEastern => {
            if us_eastern_dst(date) {
                -4
            } else {
                -5
            }
        }
        SessionZone::UkLondon => {
            if uk_dst(date) {
                1
            } else {
                0
            }
        }
    }
}

/// Convert a naive local close time in `zone` to an absolute UTC instant,
/// DST-aware. The offset at a 16:00/16:30 close is unambiguous.
fn local_to_utc(naive: NaiveDateTime, zone: SessionZone) -> Option<DateTime<Utc>> {
    let offset = zone_offset_hours(naive.date(), zone);
    let utc_naive = naive.checked_sub_signed(TimeDelta::hours(offset))?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(utc_naive, Utc))
}

/// The calendar date of `utc` observed in `zone` — used to match a target expiry
/// instant back to its `YYYYMMDD` string. Option expiries close in the local
/// afternoon (16:00/16:30), so the UTC instant is same-day-evening and no date
/// rollover occurs.
fn utc_to_zone_date(utc: DateTime<Utc>, zone: SessionZone) -> NaiveDate {
    let offset = zone_offset_hours(utc.date_naive(), zone);
    let local = utc
        .checked_add_signed(TimeDelta::hours(offset))
        .unwrap_or(utc);
    local.date_naive()
}

/// Format an absolute UTC instant as the `YYYYMMDD` string of its `zone` date.
fn format_ymd_in_zone(utc: DateTime<Utc>, zone: SessionZone) -> String {
    utc_to_zone_date(utc, zone).format("%Y%m%d").to_string()
}

/// Whether UK summer time (BST) is in effect: last Sunday of March through the day
/// before the last Sunday of October.
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

/// Whether US Eastern DST (EDT) is in effect: second Sunday of March through the
/// day before the first Sunday of November.
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

/// The `n`-th (1-based) `weekday` in `month` of `year`.
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

/// A checked price/size field: `NaN`/`Inf`/negative is **dropped** (returns
/// `None`). Zero is a valid value and is kept.
fn positive_or_drop(value: f64) -> Option<Positive> {
    if !value.is_finite() {
        return None;
    }
    Positive::new(value).ok()
}

/// A checked IBKR implied volatility (already a decimal fraction, e.g. `0.25`).
/// `NaN`/`Inf`/negative is dropped.
fn iv_or_drop(value: f64) -> Option<Positive> {
    if !value.is_finite() {
        return None;
    }
    Positive::new(value).ok()
}

/// A checked Greek: `NaN`/`Inf` is dropped (Greeks may legitimately be negative,
/// so there is no sign check). Uses the std `TryFrom<f64>` conversion.
fn greek_or_drop(value: Option<f64>) -> Option<Decimal> {
    let raw = value?;
    if !raw.is_finite() {
        return None;
    }
    Decimal::try_from(raw).ok()
}

/// A checked strike from the venue `f64`: `NaN`/`Inf`/non-positive is rejected.
fn strike_positive(value: f64) -> Result<Positive, NormalizeKind> {
    let strike = positive_or_drop(value).ok_or(NormalizeKind::OutOfRange("strike"))?;
    if strike == Positive::ZERO {
        return Err(NormalizeKind::OutOfRange("strike"));
    }
    Ok(strike)
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

/// The IBKR economic-equivalence fingerprint: a US listed option is
/// **physically-settled**, American-exercise, quoted in USD, keyed by the trading
/// class. Index options (SPX) are actually European/cash — a documented default
/// that a cross-provider overlay refuses on mismatch (the safe direction); the
/// fingerprint never gates the within-provider merge.
fn ibkr_fingerprint(multiplier: u32, trading_class: &str) -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: multiplier,
        settlement: SettlementStyle::Physical,
        exercise: ExerciseStyle::American,
        quote_currency: DEFAULT_QUOTE_CURRENCY.to_owned(),
        venue_product_code: trading_class.to_owned(),
    }
}

/// The synthetic, deterministic native symbol for one option leg — the alias key
/// the stream ticks resolve against. `(underlying, ymd, strike, style)` is unique,
/// and the transport reconstructs the `Contract` from the [`InstrumentKey`], so the
/// exact string only needs to be stable.
fn ibkr_native_symbol(underlying: &str, ymd: &str, strike: Positive, style: OptionStyle) -> String {
    format!("{underlying}-{ymd}-{strike}-{}", style_char(style))
}

/// The single-character option-right marker.
fn style_char(style: OptionStyle) -> char {
    match style {
        OptionStyle::Call => 'C',
        OptionStyle::Put => 'P',
    }
}

// ---------------------------------------------------------------------------
// Raw DTO neutral views — mapped from the upstream types, never escaping here.
// ---------------------------------------------------------------------------

/// A neutral view of one `reqSecDefOptParams` result, mapped from the upstream
/// [`IbOptionChain`] so no raw DTO escapes. Every numeric is a raw `f64`/`String`
/// checked at the seam before it enters a domain type.
#[derive(Debug, Clone)]
pub(crate) struct RawOptionParams {
    /// The option trading class (the venue product code).
    trading_class: String,
    /// The option multiplier as the venue string (e.g. `"100"`).
    multiplier: String,
    /// The listing exchange.
    #[allow(dead_code)]
    exchange: String,
    /// The available expiries, each `YYYYMMDD`.
    expirations: Vec<String>,
    /// The available strikes.
    strikes: Vec<f64>,
    /// The underlying contract id.
    #[allow(dead_code)]
    underlying_conid: i32,
}

/// A neutral view of one `contract_details` snapshot, mapped from the upstream
/// [`ContractDetails`] so no raw DTO escapes — carrying the authoritative
/// last-trade-time + exchange zone for expiry resolution.
#[derive(Debug, Clone)]
pub(crate) struct RawContractDetail {
    /// The authoritative last-trade-time (`HH:MM[:SS]` or `YYYYMMDD-HH:MM[:SS]`),
    /// when the venue supplies one.
    last_trade_time: Option<String>,
    /// The exchange trading-hours time-zone id (e.g. `"US/Eastern"`).
    time_zone_id: String,
}

/// A normalized streaming tick — either a full current-quote snapshot or a native
/// Greeks/IV computation for one contract (resolved to an [`Instrument`] via the
/// alias catalog by its `symbol`).
#[derive(Debug, Clone)]
enum RawTick {
    /// A complete current-quote snapshot (the transport accumulates individual
    /// price/size ticks per contract, so a coalesced update never loses a field).
    Quote(RawQuoteTick),
    /// Native model Greeks + IV from one `tickOptionComputation`.
    Greeks(RawGreeksTick),
}

/// A complete current-quote snapshot for one contract.
#[derive(Debug, Clone, Default, PartialEq)]
struct RawQuoteTick {
    symbol: String,
    bid: Option<f64>,
    ask: Option<f64>,
    last: Option<f64>,
    bid_size: Option<f64>,
    ask_size: Option<f64>,
}

/// Native model Greeks + IV for one contract.
#[derive(Debug, Clone, Default, PartialEq)]
struct RawGreeksTick {
    symbol: String,
    iv: Option<f64>,
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
}

/// Map one upstream [`IbOptionChain`] (`reqSecDefOptParams`) onto the neutral
/// [`RawOptionParams`].
fn map_option_chain(chain: &IbOptionChain) -> RawOptionParams {
    RawOptionParams {
        trading_class: chain.trading_class.clone(),
        multiplier: chain.multiplier.clone(),
        exchange: chain.exchange.clone(),
        expirations: chain.expirations.clone(),
        strikes: chain.strikes.clone(),
        underlying_conid: chain.underlying_contract_id,
    }
}

/// Map one upstream [`ContractDetails`] onto the neutral [`RawContractDetail`].
fn map_contract_details(details: &ContractDetails) -> RawContractDetail {
    let last_trade_time = if details.last_trade_time.trim().is_empty() {
        None
    } else {
        Some(details.last_trade_time.clone())
    };
    RawContractDetail {
        last_trade_time,
        time_zone_id: details.time_zone_id.clone(),
    }
}

/// Map one upstream [`OptionComputation`] onto the neutral [`RawGreeksTick`]. The
/// `underlying_price` is deliberately NOT surfaced (no underlying stream — the
/// #40/#41 honesty rule).
fn map_option_computation(symbol: &str, oc: &OptionComputation) -> RawGreeksTick {
    RawGreeksTick {
        symbol: symbol.to_owned(),
        iv: oc.implied_volatility,
        delta: oc.delta,
        gamma: oc.gamma,
        theta: oc.theta,
        vega: oc.vega,
    }
}

/// A per-contract accumulator that folds individual `TickPrice`/`TickSize` ticks
/// into a complete current-quote snapshot. Because IBKR sends bid/ask/last/sizes
/// as SEPARATE ticks, emitting a partial `QuoteUpdate` (only bid) could be lost to
/// the coalescing sink (latest-value-wins per kind would overwrite it with an
/// ask-only update); accumulating and re-emitting the FULL current quote each tick
/// keeps the coalesced value correct.
#[derive(Debug, Clone, Default)]
struct QuoteAccumulator {
    bid: Option<f64>,
    ask: Option<f64>,
    last: Option<f64>,
    bid_size: Option<f64>,
    ask_size: Option<f64>,
}

impl QuoteAccumulator {
    /// Apply a price tick keyed by its [`TickType`]. Returns `true` when a
    /// quote-relevant field (bid/ask/last) was updated.
    fn apply_price(&mut self, tick_type: TickType, price: f64) -> bool {
        match tick_type {
            TickType::Bid | TickType::BidOption => self.bid = Some(price),
            TickType::Ask | TickType::AskOption => self.ask = Some(price),
            TickType::Last | TickType::LastOption => self.last = Some(price),
            _ => return false,
        }
        true
    }

    /// Apply a size tick keyed by its [`TickType`]. Returns `true` when a
    /// quote-relevant field (bid/ask size) was updated.
    fn apply_size(&mut self, tick_type: TickType, size: f64) -> bool {
        match tick_type {
            TickType::BidSize => self.bid_size = Some(size),
            TickType::AskSize => self.ask_size = Some(size),
            _ => return false,
        }
        true
    }

    /// The current full quote snapshot for `symbol`.
    fn snapshot(&self, symbol: &str) -> RawQuoteTick {
        RawQuoteTick {
            symbol: symbol.to_owned(),
            bid: self.bid,
            ask: self.ask,
            last: self.last,
            bid_size: self.bid_size,
            ask_size: self.ask_size,
        }
    }
}

/// Build a per-quote [`QuoteUpdate`] from a raw quote tick, resolving the symbol to
/// its [`Instrument`] via the alias catalog. `None` for an unknown symbol
/// (dropped, hard rule 5), a crossed quote (rejected), or an all-empty quote.
fn quote_update(
    tick: &RawQuoteTick,
    aliases: &AliasCatalog,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Option<QuoteUpdate> {
    let key = aliases.resolve_symbol(&tick.symbol)?.clone();
    let instrument = aliases.instrument(&key, provider)?.clone();
    let quote = normalize_quote(tick.bid, tick.ask).ok()?;
    if quote.bid.is_none() && quote.ask.is_none() {
        return None;
    }
    Some(QuoteUpdate {
        instrument,
        bid: quote.bid,
        ask: quote.ask,
        last: tick.last.and_then(positive_or_drop),
        bid_size: tick.bid_size.and_then(positive_or_drop),
        ask_size: tick.ask_size.and_then(positive_or_drop),
        event_time: None,
        received_time: received,
    })
}

/// Build a venue [`GreeksRow`] (tagged [`GreeksOrigin::Provider`]) from a raw
/// Greeks tick. `None` for an unknown symbol (dropped) or a crossed/stale
/// computation whose analytics ALL drop — leaving the leg `None`, never a
/// fabricated Greek.
fn greeks_row(
    tick: &RawGreeksTick,
    aliases: &AliasCatalog,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Option<GreeksRow> {
    let key = aliases.resolve_symbol(&tick.symbol)?.clone();
    let instrument = aliases.instrument(&key, provider)?.clone();
    let iv = tick.iv.and_then(iv_or_drop);
    let delta = greek_or_drop(tick.delta);
    let gamma = greek_or_drop(tick.gamma);
    let theta = greek_or_drop(tick.theta);
    let vega = greek_or_drop(tick.vega);
    if iv.is_none() && delta.is_none() && gamma.is_none() && theta.is_none() && vega.is_none() {
        // A crossed/stale input leaves the leg None (no fabricated Greek).
        return None;
    }
    Some(GreeksRow {
        instrument,
        iv,
        delta,
        gamma,
        theta,
        vega,
        rho: None,
        origin: GreeksOrigin::Provider,
        event_time: None,
        received_time: received,
    })
}

// ---------------------------------------------------------------------------
// Pacing governor.
// ---------------------------------------------------------------------------

/// Select the pacing-bounded, ATM-centered window of at most `cap` instruments to
/// subscribe (`docs/03-data-providers.md` §7.6). Sorted by strike, centered on the
/// median strike (the ATM proxy — a real spot is not known at subscribe time), the
/// returned slice NEVER exceeds `cap`, so `reqMktData` is never fired unbounded.
fn pacing_window(mut instruments: Vec<Instrument>, cap: usize) -> Vec<Instrument> {
    if instruments.len() <= cap {
        return instruments;
    }
    instruments.sort_by_key(|instrument| instrument.key.strike);
    let mid = instruments.len() / 2;
    // Center the window on the ATM proxy without running past either end.
    let start = mid
        .saturating_sub(cap / 2)
        .min(instruments.len().saturating_sub(cap));
    instruments.into_iter().skip(start).take(cap).collect()
}

/// The current instruments for this provider — the resubscription set.
fn instruments_of(aliases: &AliasCatalog, provider: &ProviderId) -> Vec<Instrument> {
    aliases
        .instruments()
        .filter(|instrument| &instrument.provider == provider)
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// The chain-source seam the poll path drives (mockable).
// ---------------------------------------------------------------------------

/// The REST/socket chain seam the poll path drives, so assembly runs
/// deterministically against a **mock** with no network. The production
/// [`LiveChainSource`] wraps the upstream `ibapi::Client`; a raw DTO is mapped to a
/// neutral view inside it and never crosses this seam.
#[async_trait]
trait IbkrChainSource: Send + Sync {
    /// The `reqSecDefOptParams` results for the underlying (one per trading
    /// class/exchange).
    async fn option_params(&self, underlying: &str) -> Result<Vec<RawOptionParams>, ProviderError>;

    /// The authoritative per-expiration `contract_details` snapshot (last-trade
    /// time + zone) for one representative option contract, when available.
    async fn expiry_detail(
        &self,
        underlying: &str,
        expiration_ymd: &str,
        strike: f64,
        style: OptionStyle,
    ) -> Option<RawContractDetail>;
}

/// The production [`IbkrChainSource`]: a connected upstream `ibapi::Client`. Raw
/// upstream types stay inside it.
struct LiveChainSource {
    client: ibapi::Client,
}

impl LiveChainSource {
    /// Connect the upstream client to the configured TWS/Gateway endpoint.
    ///
    /// # Errors
    ///
    /// A redaction-safe [`ProviderError`] when the connection fails (never carrying
    /// the endpoint).
    async fn connect(adapter: &IbkrAdapter) -> Result<Self, ProviderError> {
        let client = ibapi::Client::connect(&adapter.endpoint, adapter.client_id)
            .await
            .map_err(ibkr_error)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl IbkrChainSource for LiveChainSource {
    async fn option_params(&self, underlying: &str) -> Result<Vec<RawOptionParams>, ProviderError> {
        let security_type = security_type_for(underlying);
        let mut subscription = self
            .client
            .option_chain(underlying, "", security_type, 0)
            .await
            .map_err(ibkr_error)?;
        let chains = subscription
            .collect_for(Duration::from_secs(OPTION_PARAMS_TIMEOUT_SECS))
            .await;
        Ok(chains.iter().map(map_option_chain).collect())
    }

    async fn expiry_detail(
        &self,
        underlying: &str,
        expiration_ymd: &str,
        strike: f64,
        style: OptionStyle,
    ) -> Option<RawContractDetail> {
        let right = match style {
            OptionStyle::Call => OptionRight::Call,
            OptionStyle::Put => OptionRight::Put,
        };
        let contract = Contract::option(underlying, expiration_ymd, strike, right);
        let details = self.client.contract_details(&contract).await.ok()?;
        details.first().map(map_contract_details)
    }
}

// ---------------------------------------------------------------------------
// discover + the assembled chain path.
// ---------------------------------------------------------------------------

/// The assembled result: the poll-leg [`ChainFetch`]. Quotes and native Greeks
/// arrive via the stream overlay, so the assembled chain is a bare strike ladder.
#[derive(Debug, Clone)]
struct AssembledChain {
    fetch: ChainFetch,
}

/// Resolve the requested `expiration` to the absolute-UTC instant the chain keys
/// on. An absolute `DateTime` passes through; a relative offset is resolved via the
/// caller-supplied `received` reference and the default session close.
fn target_expiry(
    expiration: &ExpirationDate,
    received: DateTime<Utc>,
) -> Result<DateTime<Utc>, ProviderError> {
    match expiration {
        ExpirationDate::DateTime(dt) => Ok(*dt),
        ExpirationDate::Days(days) => {
            let seconds = (days.to_f64() * 86_400.0).round();
            if !seconds.is_finite() {
                return Err(normalize_err(NormalizeKind::UnparseableExpiry));
            }
            let seconds = seconds as i64;
            let instant = received
                .checked_add_signed(TimeDelta::seconds(seconds))
                .ok_or_else(|| normalize_err(NormalizeKind::UnparseableExpiry))?;
            let session = default_session();
            let naive = instant
                .date_naive()
                .and_hms_opt(session.hour, session.minute, 0)
                .ok_or_else(|| normalize_err(NormalizeKind::UnparseableExpiry))?;
            local_to_utc(naive, session.zone)
                .ok_or_else(|| normalize_err(NormalizeKind::UnparseableExpiry))
        }
    }
}

/// Select the `reqSecDefOptParams` result + `YYYYMMDD` string whose expiry date
/// (in the session zone) matches the target expiry instant.
fn select_expiration<'a>(
    params_list: &'a [RawOptionParams],
    target: DateTime<Utc>,
    session: &SessionClose,
) -> Option<(&'a RawOptionParams, String)> {
    let target_date = utc_to_zone_date(target, session.zone);
    for params in params_list {
        for ymd in &params.expirations {
            if parse_ymd(ymd) == Some(target_date) {
                return Some((params, ymd.clone()));
            }
        }
    }
    None
}

/// Assemble the chain for `(underlying, expiration)` from the underlying's
/// `reqSecDefOptParams` strike ladder, refining the keyed expiry with the
/// authoritative `contract_details` last-trade-time. The result is built once
/// (atomic; no half-filled chain).
///
/// # Errors
///
/// [`ProviderError::Normalize`] for an unparseable requested/authoritative expiry;
/// [`ProviderError::NoChain`] when no listed expiry matches the requested one; a
/// transport failure from the params fetch.
async fn compose_chain<S: IbkrChainSource + ?Sized>(
    source: &S,
    underlying: &str,
    expiration: &ExpirationDate,
    provider: &ProviderId,
    received: DateTime<Utc>,
) -> Result<AssembledChain, ProviderError> {
    let symbol = underlying.to_ascii_uppercase();
    let target = target_expiry(expiration, received)?;
    let params_list = source.option_params(&symbol).await?;
    let session = default_session();
    // Own the selected params + ymd BEFORE the detail await (no borrow across it).
    let (params, ymd) = match select_expiration(&params_list, target, &session) {
        Some((params, ymd)) => (params.clone(), ymd),
        None => return Err(no_chain(&symbol, target)),
    };
    // The authoritative last-trade-time (bounded: one detail fetch, a representative
    // strike/style). Its exchange zone refines the session, then the resolved
    // instant keys the chain.
    let repr_strike = representative_strike(&params.strikes).unwrap_or(1.0);
    let detail = source
        .expiry_detail(&symbol, &ymd, repr_strike, OptionStyle::Call)
        .await;
    let session = detail
        .as_ref()
        .map(|d| session_from_zone_id(&d.time_zone_id))
        .unwrap_or(session);
    let last_trade_time = detail.as_ref().and_then(|d| d.last_trade_time.clone());
    let expiry_utc =
        resolve_expiry(last_trade_time.as_deref(), &ymd, &session).map_err(normalize_err)?;
    assemble_chain(&params, &ymd, expiry_utc, &symbol, provider)
}

/// A representative strike (the median finite positive strike) for the one bounded
/// `contract_details` probe.
fn representative_strike(strikes: &[f64]) -> Option<f64> {
    let mut sorted: Vec<f64> = strikes
        .iter()
        .copied()
        .filter(|s| s.is_finite() && *s > 0.0)
        .collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted.get(sorted.len() / 2).copied()
}

/// Assemble the strike ladder into a single [`OptionChain`] plus its
/// [`AliasCatalog`]. The published strike set equals the normalizable strikes of
/// the requested expiry (each as a call + put leg); quotes/Greeks arrive via the
/// stream overlay.
///
/// # Errors
///
/// [`ProviderError::Normalize`] when the strike ladder exceeds [`MAX_STRIKES`];
/// [`ProviderError::NoChain`] when no strike normalizes.
fn assemble_chain(
    params: &RawOptionParams,
    expiration_ymd: &str,
    expiration_utc: DateTime<Utc>,
    underlying: &str,
    provider: &ProviderId,
) -> Result<AssembledChain, ProviderError> {
    if params.strikes.len() > MAX_STRIKES {
        return Err(normalize_err(NormalizeKind::LimitExceeded(STRIKE_CAP)));
    }
    let multiplier = params
        .multiplier
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_CONTRACT_MULTIPLIER);
    let spec = ibkr_fingerprint(multiplier, &params.trading_class);

    let mut aliases = AliasCatalog::new();
    let mut strikes: BTreeSet<Positive> = BTreeSet::new();
    for raw_strike in &params.strikes {
        let Ok(strike) = strike_positive(*raw_strike) else {
            continue;
        };
        let _ = strikes.insert(strike);
        for style in [OptionStyle::Call, OptionStyle::Put] {
            let native_symbol = ibkr_native_symbol(underlying, expiration_ymd, strike, style);
            let key = InstrumentKey {
                underlying: underlying.to_owned(),
                expiration_utc,
                strike,
                style,
            };
            aliases.insert(Instrument {
                key,
                provider: provider.clone(),
                native_symbol,
                stream_symbol: None,
                spec: spec.clone(),
            });
        }
    }

    if strikes.is_empty() {
        return Err(no_chain(underlying, expiration_utc));
    }

    // IBKR carries no spot on this path: seed the chain center to the MEDIAN strike
    // (a real value from the ladder), never a fabricated quote.
    let ordered: Vec<Positive> = strikes.iter().copied().collect();
    let spot = ordered
        .get(ordered.len() / 2)
        .copied()
        .unwrap_or(Positive::ONE);
    let mut chain = OptionChain::new(underlying, spot, expiration_utc.to_rfc3339(), None, None);
    for strike in &ordered {
        chain.add_option(
            *strike,
            // No quotes at assembly — the stream overlays them.
            None,
            None,
            None,
            None,
            // No venue IV at assembly: the sentinel (Positive::ZERO); venue Greeks
            // arrive via the stream tagged Provider.
            Positive::ZERO,
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

/// A [`ProviderError::Normalize`] from a [`NormalizeKind`].
fn normalize_err(kind: NormalizeKind) -> ProviderError {
    ProviderError::Normalize { kind }
}

/// A [`ProviderError::NoChain`] for a missing `(underlying, expiry)`.
fn no_chain(underlying: &str, expiration_utc: DateTime<Utc>) -> ProviderError {
    ProviderError::NoChain {
        underlying: underlying.to_owned(),
        expiration: expiration_utc.to_rfc3339(),
    }
}

// ---------------------------------------------------------------------------
// Redaction-safe transport error mapping.
// ---------------------------------------------------------------------------

/// Map an upstream [`ibapi::Error`] to a redaction-safe [`ProviderError`] by
/// **category only** — the inner message (which may hold an endpoint, a host, or a
/// diagnostic) is never carried (`docs/03-data-providers.md` §6, `docs/SECURITY.md`
/// §1).
fn ibkr_error(err: ibapi::Error) -> ProviderError {
    match err {
        ibapi::Error::ConnectionFailed
        | ibapi::Error::ConnectionRejected(_)
        | ibapi::Error::ConnectionReset
        | ibapi::Error::Io(_) => transport(TransportKind::Closed),
        ibapi::Error::Parse(_, _, _)
        | ibapi::Error::ParseInt(_)
        | ibapi::Error::ParseTime(_)
        | ibapi::Error::FromUtf8(_) => transport(TransportKind::Decode),
        _ => transport(TransportKind::Closed),
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
/// [`LiveStreamTransport`] wraps the upstream `ibapi::Client` market-data
/// subscriptions (pacing-bounded) plus the REST re-poll; a raw tick is decoded to
/// [`RawTick`] inside it and never crosses this seam.
#[async_trait]
trait IbkrStreamTransport: Send {
    /// Connect + subscribe the pacing-bounded `instruments` window (quotes +
    /// `tickOptionComputation`).
    async fn connect_and_subscribe(
        &mut self,
        instruments: &[Instrument],
    ) -> Result<(), TransportGone>;

    /// Await the next normalized tick. `Err(_)` means the stream ended — the loop
    /// reconnects.
    async fn receive(&mut self) -> Result<RawTick, TransportGone>;

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

/// The production [`IbkrStreamTransport`]: per-contract `ibapi::Client` market-data
/// subscriptions (bounded by [`MAX_SUBSCRIPTIONS`]) merged into one receiver, plus
/// the adapter's REST re-poll for the backfill. Raw upstream types stay private.
struct LiveStreamTransport {
    adapter: IbkrAdapter,
    /// The connected client — kept alive while its subscriptions stream (its `Drop`
    /// shuts down the message bus).
    client: Option<ibapi::Client>,
    /// The per-contract driver tasks, aborted on teardown.
    tasks: Vec<JoinHandle<()>>,
    /// The merged tick receiver drained by `receive`.
    receiver: Option<tokio::sync::mpsc::UnboundedReceiver<RawTick>>,
}

impl LiveStreamTransport {
    fn new(adapter: IbkrAdapter) -> Self {
        Self {
            adapter,
            client: None,
            tasks: Vec::new(),
            receiver: None,
        }
    }

    /// Abort the per-contract tasks and drop the client before a fresh connect, so
    /// a reconnect never leaves half-open subscriptions behind.
    fn teardown(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.receiver = None;
        // Drop the client last — its `Drop` requests the message-bus shutdown.
        self.client = None;
    }
}

impl Drop for LiveStreamTransport {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Reconstruct the upstream option [`Contract`] from an [`Instrument`] (its key +
/// spec), using the default session zone to format the `YYYYMMDD`.
fn build_contract(instrument: &Instrument) -> Contract {
    let key = &instrument.key;
    let ymd = format_ymd_in_zone(key.expiration_utc, default_session().zone);
    let right = match key.style {
        OptionStyle::Call => OptionRight::Call,
        OptionStyle::Put => OptionRight::Put,
    };
    let mut contract = Contract::option(&key.underlying, &ymd, key.strike.to_f64(), right);
    contract.trading_class = instrument.spec.venue_product_code.clone();
    contract.multiplier = instrument.spec.contract_multiplier.to_string();
    contract
}

/// Drive one contract's market-data subscription, folding individual ticks into a
/// per-contract [`QuoteAccumulator`] and forwarding complete [`RawTick`]s onto the
/// merged channel until the stream ends or the receiver is gone.
async fn drive_subscription(
    mut subscription: ibapi::subscriptions::Subscription<TickTypes>,
    symbol: String,
    tx: tokio::sync::mpsc::UnboundedSender<RawTick>,
) {
    let mut quote = QuoteAccumulator::default();
    while let Some(item) = subscription.next().await {
        let tick = match item {
            Ok(SubscriptionItem::Data(tick)) => tick,
            Ok(SubscriptionItem::Notice(_)) => continue,
            Err(_) => break,
        };
        let sent = match tick {
            TickTypes::Price(TickPrice {
                tick_type, price, ..
            }) => {
                if quote.apply_price(tick_type, price) {
                    tx.send(RawTick::Quote(quote.snapshot(&symbol)))
                } else {
                    Ok(())
                }
            }
            TickTypes::Size(TickSize { tick_type, size }) => {
                if quote.apply_size(tick_type, size) {
                    tx.send(RawTick::Quote(quote.snapshot(&symbol)))
                } else {
                    Ok(())
                }
            }
            TickTypes::OptionComputation(oc) => {
                tx.send(RawTick::Greeks(map_option_computation(&symbol, &oc)))
            }
            _ => Ok(()),
        };
        if sent.is_err() {
            break;
        }
    }
}

#[async_trait]
impl IbkrStreamTransport for LiveStreamTransport {
    async fn connect_and_subscribe(
        &mut self,
        instruments: &[Instrument],
    ) -> Result<(), TransportGone> {
        self.teardown();
        if instruments.is_empty() {
            return Err(TransportGone);
        }
        let client = ibapi::Client::connect(&self.adapter.endpoint, self.adapter.client_id)
            .await
            .map_err(|_| TransportGone)?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = Vec::new();
        // The pacing governor: never exceed MAX_SUBSCRIPTIONS concurrent lines
        // (belt-and-suspenders; the loop already windows the instruments).
        for instrument in instruments.iter().take(MAX_SUBSCRIPTIONS) {
            let contract = build_contract(instrument);
            let subscription = match client.market_data(&contract).subscribe().await {
                Ok(subscription) => subscription,
                // A single leg failing must not sink the whole subscription.
                Err(_) => continue,
            };
            let tx = tx.clone();
            let symbol = instrument.native_symbol.clone();
            tasks.push(tokio::spawn(drive_subscription(subscription, symbol, tx)));
        }
        if tasks.is_empty() {
            return Err(TransportGone);
        }
        self.client = Some(client);
        self.tasks = tasks;
        self.receiver = Some(rx);
        Ok(())
    }

    async fn receive(&mut self) -> Result<RawTick, TransportGone> {
        match self.receiver.as_mut() {
            Some(receiver) => match receiver.recv().await {
                Some(tick) => Ok(tick),
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
        let source = LiveChainSource::connect(&self.adapter).await.ok()?;
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
/// Connect + subscribe the pacing-bounded window, emit the Live health + a
/// re-polled backfill (`Chain`), drain the stream as per-instrument `Quote`s and
/// native `Greeks`; on a drop emit `Health(Reconnecting{attempt})`, back off with
/// jitter, and reconnect. The backfill refreshes the resubscribe window off the
/// FRESH [`ChainFetch`]. Cancellation is observed at every `.await`, so the loop
/// never opens a stream after cancellation and never hot-loops.
async fn run_reconnect_loop<T: IbkrStreamTransport>(
    mut transport: T,
    id: ProviderId,
    underlying: String,
    expiration_utc: DateTime<Utc>,
    mut aliases: AliasCatalog,
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
            exit = connect_stream_once(
                &mut transport, &id, &underlying, expiration_utc,
                &mut aliases, &mut sink, &cancel, &mut attempt,
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

/// One connection attempt: connect + subscribe the pacing window, emit the Live
/// backfill (`Chain`), then drain the stream as per-instrument `Quote`s and
/// `Greeks` until it drops or the subscription is cancelled. `attempt` resets to 0
/// on a successful (re)connect.
#[allow(clippy::too_many_arguments)]
async fn connect_stream_once<T: IbkrStreamTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    aliases: &mut AliasCatalog,
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
    attempt: &mut u32,
) -> StreamExit {
    let window = pacing_window(instruments_of(aliases, id), MAX_SUBSCRIPTIONS);
    let subscribed = tokio::select! {
        biased;
        () = cancel.cancelled() => return StreamExit::Shutdown,
        result = transport.connect_and_subscribe(&window) => result,
    };
    if subscribed.is_err() {
        return StreamExit::Reconnect;
    }

    *attempt = 0;
    if go_live_and_backfill(
        transport,
        id,
        underlying,
        expiration_utc,
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
        // Guard against a tick for an unknown symbol (hard rule 5): resolve it
        // against current aliases; an unknown symbol is dropped, never keyed.
        let update = match event {
            RawTick::Quote(tick) => {
                quote_update(&tick, aliases, id, now_utc()).map(MarketUpdate::Quote)
            }
            RawTick::Greeks(tick) => {
                greeks_row(&tick, aliases, id, now_utc()).map(MarketUpdate::Greeks)
            }
        };
        let Some(update) = update else {
            continue;
        };
        let sent = tokio::select! {
            biased;
            () = cancel.cancelled() => return StreamExit::Shutdown,
            outcome = sink.send(update) => outcome,
        };
        if sent == SendState::Closed {
            return StreamExit::Shutdown;
        }
    }
}

/// Emit the Live health then the re-polled backfill (a fresh `Chain`). Refreshes
/// `aliases` off the fresh fetch so a subsequent reconnect resubscribes off current
/// aliases. Cancellation short-circuits; [`SendState::Closed`] once the consumer is
/// gone.
async fn go_live_and_backfill<T: IbkrStreamTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
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

    // Refresh the resubscribe aliases off the FRESH fetch (docs/03 §5).
    *aliases = composed.fetch.aliases.clone();

    let snapshot = MarketUpdate::Chain(chain_snapshot(&composed.fetch, now_utc()));
    tokio::select! {
        biased;
        () = cancel.cancelled() => SendState::Open,
        outcome = sink.send(snapshot) => outcome,
    }
}

/// Assemble a streaming-current [`ChainSnapshot`] from a re-polled [`ChainFetch`] —
/// the same `AliasCatalog` carried forward with no re-derivation. The source is
/// [`ChainSource::Merged`] (REST seeds structure, the stream overlays quotes +
/// venue Greeks).
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
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use tokio::sync::mpsc;

    use super::*;
    use crate::chain::{ChainStore, MergeOutcome};

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
    struct MapEnv(StdHashMap<String, String>);

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn endpoint_env() -> MapEnv {
        let mut env = StdHashMap::new();
        let _ = env.insert(
            "CHAINVIEW_IBKR_ENDPOINT".to_owned(),
            "127.0.0.1:7497".to_owned(),
        );
        MapEnv(env)
    }

    #[track_caller]
    fn sample_adapter() -> IbkrAdapter {
        match IbkrAdapter::from_env(&endpoint_env()) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed with the endpoint present: {e}"),
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

    // === Fixture: a recorded sec_def_opt_params + contract_details shape ======

    const OPTION_PARAMS_FIXTURE: &str =
        include_str!("../../tests/fixtures/ibkr/sec_def_opt_params_spx.json");

    #[derive(serde::Deserialize)]
    struct FixtureParams {
        trading_class: String,
        multiplier: String,
        exchange: String,
        underlying_conid: i32,
        expirations: Vec<String>,
        strikes: Vec<f64>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureDetail {
        expiration_ymd: String,
        last_trade_time: String,
        time_zone_id: String,
    }

    #[derive(serde::Deserialize)]
    struct Fixture {
        params: FixtureParams,
        contract_detail: FixtureDetail,
    }

    #[track_caller]
    fn load_fixture() -> Fixture {
        match serde_json::from_str(OPTION_PARAMS_FIXTURE) {
            Ok(fixture) => fixture,
            Err(e) => panic!("the ibkr fixture must deserialize: {e}"),
        }
    }

    #[track_caller]
    fn fixture_params() -> RawOptionParams {
        let fixture = load_fixture();
        RawOptionParams {
            trading_class: fixture.params.trading_class,
            multiplier: fixture.params.multiplier,
            exchange: fixture.params.exchange,
            expirations: fixture.params.expirations,
            strikes: fixture.params.strikes,
            underlying_conid: fixture.params.underlying_conid,
        }
    }

    #[track_caller]
    fn fixture_detail() -> (RawContractDetail, String) {
        let fixture = load_fixture();
        (
            RawContractDetail {
                last_trade_time: Some(fixture.contract_detail.last_trade_time),
                time_zone_id: fixture.contract_detail.time_zone_id,
            },
            fixture.contract_detail.expiration_ymd,
        )
    }

    /// The fixture's target expiry resolved via the SAME authoritative last-trade
    /// time the assembly uses — 16:15 US/Eastern on 2026-07-17 (EDT -4) -> 20:15
    /// UTC.
    #[track_caller]
    fn fixture_expiry_utc() -> DateTime<Utc> {
        let (detail, ymd) = fixture_detail();
        let session = session_from_zone_id(&detail.time_zone_id);
        match resolve_expiry(detail.last_trade_time.as_deref(), &ymd, &session) {
            Ok(dt) => dt,
            Err(e) => panic!("fixture expiry must resolve: {e}"),
        }
    }

    #[track_caller]
    fn fixture_assembled() -> AssembledChain {
        let params = fixture_params();
        let (_, ymd) = fixture_detail();
        match assemble_chain(&params, &ymd, fixture_expiry_utc(), "SPX", &pid("ibkr")) {
            Ok(a) => a,
            Err(e) => panic!("assembly should succeed, got: {e}"),
        }
    }

    // === Identity + capabilities =============================================

    #[test]
    fn test_ibkr_id_is_valid_and_reserved() {
        let id = ibkr_provider_id();
        assert_eq!(id.as_str(), "ibkr");
        assert!(id.is_reserved());
        assert!(ProviderId::new(IBKR_ID).is_ok());
    }

    #[test]
    fn test_ibkr_capabilities_match_section_8_row() {
        let caps = ibkr_capabilities();
        assert_eq!(caps.chain, ChainCapability::Assemble);
        assert!(
            !caps.depth,
            "IBKR claims no option depth (no recorded fixture)"
        );
        assert_eq!(caps.greeks, GreeksCapability::Provided);
        assert_eq!(
            caps.option_stream,
            OptionStreamCapability::ChainQuotes { verified: false }
        );
        assert!(
            !caps.underlying_stream,
            "IBKR subscribes only option contracts (no underlying fold; #40/#41 honesty)"
        );
        assert_eq!(
            caps.chain_poll,
            ChainPollCapability::Poll {
                interval_hint_secs: REFRESH_HINT_SECS
            }
        );
        assert!(!caps.trades_tape, "no public trade tape on this path");
        assert_eq!(
            caps.auth,
            AuthKind::None,
            "the TWS/Gateway holds the session"
        );
    }

    #[test]
    fn test_adapter_reports_capabilities_and_id_via_trait() {
        let adapter: Box<dyn Provider> = Box::new(sample_adapter());
        assert_eq!(adapter.id().as_str(), "ibkr");
        assert_eq!(adapter.capabilities().chain, ChainCapability::Assemble);
        assert_eq!(adapter.capabilities().greeks, GreeksCapability::Provided);
        assert_eq!(adapter.capabilities().auth, AuthKind::None);
    }

    // === Config: env-only endpoint/client-id =================================

    #[test]
    fn test_from_env_reads_endpoint_and_default_client_id() {
        let adapter = sample_adapter();
        assert_eq!(adapter.endpoint, "127.0.0.1:7497");
        assert_eq!(adapter.client_id, DEFAULT_CLIENT_ID);
    }

    #[test]
    fn test_from_env_reads_client_id_when_set() {
        let mut env = StdHashMap::new();
        let _ = env.insert("CHAINVIEW_IBKR_ENDPOINT".to_owned(), "gw:4002".to_owned());
        let _ = env.insert("CHAINVIEW_IBKR_CLIENT_ID".to_owned(), "42".to_owned());
        match IbkrAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => {
                assert_eq!(adapter.endpoint, "gw:4002");
                assert_eq!(adapter.client_id, 42);
            }
            Err(e) => panic!("from_env should succeed: {e}"),
        }
    }

    #[test]
    fn test_from_env_missing_endpoint_is_missing_credential() {
        // Absence of the endpoint OMITS the built-in (the registry treats
        // MissingCredential as "unconfigured"), never a hard startup error.
        match IbkrAdapter::from_env(&MapEnv(StdHashMap::new())) {
            Err(ConfigError::MissingCredential(id)) => assert_eq!(id.as_str(), "ibkr"),
            other => panic!("expected MissingCredential(ibkr), got: {other:?}"),
        }
    }

    #[test]
    fn test_from_env_malformed_endpoint_is_invalid_value() {
        let mut env = StdHashMap::new();
        let _ = env.insert(
            "CHAINVIEW_IBKR_ENDPOINT".to_owned(),
            "not-a-socket".to_owned(),
        );
        match IbkrAdapter::from_env(&MapEnv(env)) {
            Err(ConfigError::InvalidValue { field, .. }) => assert_eq!(field, "ibkr endpoint"),
            other => panic!("expected InvalidValue on ibkr endpoint, got: {other:?}"),
        }
    }

    #[test]
    fn test_from_env_malformed_client_id_is_invalid_value() {
        let mut env = StdHashMap::new();
        let _ = env.insert(
            "CHAINVIEW_IBKR_ENDPOINT".to_owned(),
            "127.0.0.1:7497".to_owned(),
        );
        let _ = env.insert("CHAINVIEW_IBKR_CLIENT_ID".to_owned(), "abc".to_owned());
        match IbkrAdapter::from_env(&MapEnv(env)) {
            Err(ConfigError::InvalidValue { field, .. }) => assert_eq!(field, "ibkr client id"),
            other => panic!("expected InvalidValue on ibkr client id, got: {other:?}"),
        }
    }

    #[test]
    fn test_endpoint_validation() {
        assert!(is_valid_endpoint("127.0.0.1:7497"));
        assert!(is_valid_endpoint("gateway.local:4002"));
        assert!(!is_valid_endpoint("no-port"));
        assert!(!is_valid_endpoint(":7497"));
        assert!(!is_valid_endpoint("host:"));
        assert!(!is_valid_endpoint("host:notaport"));
        assert!(!is_valid_endpoint("host:99999999"));
    }

    // === Captured-log proof: no secret-shaped material is logged ==============

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

    const CONTROL_CANARY: &str = "chainview-ibkr-canary-7c1d-present";
    const SECRET_SHAPED_ENDPOINT: &str = "10.9.8.7:65001";

    #[test]
    fn test_construction_and_errors_never_log_secret_material() {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // Prove the sink captures content, so the redaction assertion is non-vacuous.
        tracing::debug!("{CONTROL_CANARY}");

        let mut env = StdHashMap::new();
        let _ = env.insert(
            "CHAINVIEW_IBKR_ENDPOINT".to_owned(),
            SECRET_SHAPED_ENDPOINT.to_owned(),
        );
        let _ = env.insert("CHAINVIEW_IBKR_CLIENT_ID".to_owned(), "31337".to_owned());
        let adapter = match IbkrAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed: {e}"),
        };
        let _ = &adapter;

        // A redaction-safe ProviderError never carries the endpoint/client-id.
        let err = ibkr_error(ibapi::Error::ConnectionFailed);
        tracing::debug!(error = %err, "ibkr error rendered");
        let decode = ibkr_error(ibapi::Error::NotImplemented);
        tracing::debug!(error = %decode, "ibkr error rendered");

        let output = logs.contents();
        assert!(output.contains(CONTROL_CANARY), "sink must capture content");
        assert!(
            !output.contains(SECRET_SHAPED_ENDPOINT),
            "the endpoint leaked into logs:\n{output}"
        );
        assert!(
            !output.contains("31337"),
            "the client id leaked into logs:\n{output}"
        );
        assert!(
            !format!("{err} {err:?}").contains(SECRET_SHAPED_ENDPOINT),
            "a ProviderError must never carry the endpoint"
        );
    }

    // === Expiry resolution ====================================================

    #[test]
    fn test_expiry_date_only_resolves_via_us_session_close() {
        // A date-only expiry -> 16:00 US/Eastern; 2026-07-17 is EDT (-4) -> 20:00 UTC.
        let session = default_session();
        match resolve_expiry(None, "20260717", &session) {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-07-17T20:00:00+00:00"),
            Err(e) => panic!("date-only US expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_authoritative_last_trade_time_wins() {
        // The authoritative time-of-day (16:15 EDT -> 20:15 UTC) WINS over the
        // 16:00 date-only close.
        let session = default_session();
        let resolved = match resolve_expiry(Some("16:15:00"), "20260717", &session) {
            Ok(dt) => dt,
            Err(e) => panic!("timestamped expiry should resolve, got: {e}"),
        };
        assert_eq!(resolved.to_rfc3339(), "2026-07-17T20:15:00+00:00");
        let date_only = match resolve_expiry(None, "20260717", &session) {
            Ok(dt) => dt,
            Err(e) => panic!("date-only should resolve, got: {e}"),
        };
        assert_ne!(
            resolved, date_only,
            "the authoritative last-trade time must win"
        );
    }

    #[test]
    fn test_expiry_authoritative_date_and_time_form() {
        // A self-contained `YYYYMMDD-HH:MM:SS` authoritative form.
        let session = default_session();
        match resolve_expiry(Some("20260717-16:15:00"), "20260717", &session) {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-07-17T20:15:00+00:00"),
            Err(e) => panic!("date+time form should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_ambiguous_and_unparseable_are_normalize_errors() {
        let session = default_session();
        // Unparseable date-only.
        assert_eq!(
            resolve_expiry(None, "notadate", &session),
            Err(NormalizeKind::UnparseableExpiry)
        );
        // Present-but-garbage authoritative field is an error, never a silent
        // fall-through to the date-only close.
        assert_eq!(
            resolve_expiry(Some("garbage"), "20260717", &session),
            Err(NormalizeKind::UnparseableExpiry)
        );
        // A malformed date.
        assert_eq!(
            resolve_expiry(None, "2026071", &session),
            Err(NormalizeKind::UnparseableExpiry)
        );
    }

    #[test]
    fn test_dst_boundaries() {
        // US Eastern: second Sunday of March 2026 = 2026-03-08.
        assert!(us_eastern_dst(date(2026, 3, 8)));
        assert!(!us_eastern_dst(date(2026, 3, 7)));
        // First Sunday of November 2026 = 2026-11-01 (back to EST).
        assert!(!us_eastern_dst(date(2026, 11, 1)));
        assert!(us_eastern_dst(date(2026, 10, 31)));
        // UK: last Sunday of March 2026 = 2026-03-29.
        assert!(uk_dst(date(2026, 3, 29)));
        assert!(!uk_dst(date(2026, 3, 28)));
    }

    #[test]
    fn test_parse_ymd_forms() {
        assert_eq!(parse_ymd("20260717"), Some(date(2026, 7, 17)));
        assert_eq!(parse_ymd("2026-07-17"), None);
        assert_eq!(parse_ymd("2026071"), None);
        assert_eq!(parse_ymd("2026071X"), None);
    }

    // === Numeric normalization at the seam ====================================

    #[test]
    fn test_strike_positive_parses_and_rejects() {
        match strike_positive(7500.0) {
            Ok(strike) => assert_eq!(strike, pos(7500.0)),
            Err(e) => panic!("7500 should parse, got: {e}"),
        }
        assert_eq!(
            strike_positive(0.0),
            Err(NormalizeKind::OutOfRange("strike"))
        );
        assert_eq!(
            strike_positive(-5.0),
            Err(NormalizeKind::OutOfRange("strike"))
        );
        assert_eq!(
            strike_positive(f64::NAN),
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
        // A NaN field drops only that field.
        match normalize_quote(Some(f64::NAN), Some(2.0)) {
            Ok(q) => {
                assert_eq!(q.bid, None);
                assert_eq!(q.ask, Some(pos(2.0)));
            }
            Err(e) => panic!("NaN bid drops only that field, got: {e}"),
        }
    }

    #[test]
    fn test_greek_or_drop_guards_non_finite() {
        assert_eq!(greek_or_drop(Some(0.55)), Some(Decimal::new(55, 2)));
        assert_eq!(greek_or_drop(Some(-0.45)), Some(Decimal::new(-45, 2)));
        assert_eq!(greek_or_drop(Some(f64::NAN)), None);
        assert_eq!(greek_or_drop(Some(f64::INFINITY)), None);
        assert_eq!(greek_or_drop(None), None);
    }

    // === ibapi DTO mapping (real upstream structs, no socket) =================

    #[test]
    fn test_map_option_chain_from_real_ibapi_struct() {
        let chain = IbOptionChain {
            underlying_contract_id: 416_904,
            trading_class: "SPX".to_owned(),
            multiplier: "100".to_owned(),
            exchange: "SMART".to_owned(),
            expirations: vec!["20260717".to_owned(), "20260821".to_owned()],
            strikes: vec![7400.0, 7500.0, 7600.0],
        };
        let raw = map_option_chain(&chain);
        assert_eq!(raw.trading_class, "SPX");
        assert_eq!(raw.multiplier, "100");
        assert_eq!(raw.expirations, vec!["20260717", "20260821"]);
        assert_eq!(raw.strikes, vec![7400.0, 7500.0, 7600.0]);
        assert_eq!(raw.underlying_conid, 416_904);
    }

    #[test]
    fn test_map_option_computation_from_real_ibapi_struct() {
        let oc = OptionComputation {
            implied_volatility: Some(0.25),
            delta: Some(0.55),
            gamma: Some(0.01),
            vega: Some(1.2),
            theta: Some(-0.5),
            underlying_price: Some(7500.0),
            ..Default::default()
        };
        let raw = map_option_computation("SPX-20260717-7500-C", &oc);
        assert_eq!(raw.iv, Some(0.25));
        assert_eq!(raw.delta, Some(0.55));
        assert_eq!(raw.gamma, Some(0.01));
        assert_eq!(raw.vega, Some(1.2));
        assert_eq!(raw.theta, Some(-0.5));
        // underlying_price is deliberately NOT surfaced (no underlying stream).
    }

    // === Native Greeks -> GreeksRow (Provider) ================================

    fn one_leg_aliases() -> (AliasCatalog, String) {
        let assembled = fixture_assembled();
        let symbol = match assembled
            .fetch
            .aliases
            .instruments()
            .find(|i| i.provider == pid("ibkr"))
        {
            Some(instrument) => instrument.native_symbol.clone(),
            None => panic!("the fixture must carry at least one leg"),
        };
        (assembled.fetch.aliases, symbol)
    }

    #[test]
    fn test_greeks_row_is_tagged_provider_and_checked_into_decimal() {
        let (aliases, symbol) = one_leg_aliases();
        let tick = RawGreeksTick {
            symbol,
            iv: Some(0.25),
            delta: Some(0.55),
            gamma: Some(0.01),
            theta: Some(-0.5),
            vega: Some(1.2),
        };
        match greeks_row(&tick, &aliases, &pid("ibkr"), now_utc()) {
            Some(row) => {
                assert_eq!(row.origin, GreeksOrigin::Provider);
                assert_eq!(row.iv, Some(pos(0.25)));
                assert_eq!(row.delta, Some(Decimal::new(55, 2)));
                assert_eq!(row.theta, Some(Decimal::new(-5, 1)));
                assert!(row.rho.is_none(), "IBKR OptionComputation carries no rho");
            }
            None => panic!("expected a Provider GreeksRow"),
        }
    }

    #[test]
    fn test_greeks_row_nan_is_guarded() {
        let (aliases, symbol) = one_leg_aliases();
        let tick = RawGreeksTick {
            symbol,
            iv: Some(f64::NAN),
            delta: Some(f64::INFINITY),
            gamma: Some(0.01),
            theta: None,
            vega: None,
        };
        match greeks_row(&tick, &aliases, &pid("ibkr"), now_utc()) {
            Some(row) => {
                assert!(row.iv.is_none(), "NaN IV dropped");
                assert!(row.delta.is_none(), "Inf delta dropped");
                assert_eq!(row.gamma, Some(Decimal::new(1, 2)));
            }
            None => panic!("a partially-valid computation still yields a row"),
        }
    }

    #[test]
    fn test_crossed_or_stale_computation_leaves_leg_none() {
        let (aliases, symbol) = one_leg_aliases();
        // Every analytic drops (all NaN / None) -> no GreeksRow (leg left None).
        let tick = RawGreeksTick {
            symbol,
            iv: Some(f64::NAN),
            delta: Some(f64::NAN),
            gamma: None,
            theta: None,
            vega: None,
        };
        assert!(
            greeks_row(&tick, &aliases, &pid("ibkr"), now_utc()).is_none(),
            "a stale computation whose analytics all drop leaves the leg None"
        );
    }

    #[test]
    fn test_greeks_row_unknown_symbol_is_dropped() {
        let (aliases, _) = one_leg_aliases();
        let tick = RawGreeksTick {
            symbol: "SPX-20260717-99999-C".to_owned(),
            iv: Some(0.25),
            delta: Some(0.5),
            gamma: None,
            theta: None,
            vega: None,
        };
        assert!(
            greeks_row(&tick, &aliases, &pid("ibkr"), now_utc()).is_none(),
            "a tick for an unknown symbol must be dropped, never keyed"
        );
    }

    // === QuoteAccumulator: partial ticks fold into a full snapshot ============

    #[test]
    fn test_quote_accumulator_folds_partial_ticks() {
        let mut acc = QuoteAccumulator::default();
        assert!(acc.apply_price(TickType::Bid, 12.5));
        assert!(acc.apply_price(TickType::Ask, 13.5));
        assert!(acc.apply_size(TickType::BidSize, 4.0));
        assert!(
            !acc.apply_price(TickType::Open, 10.0),
            "non-quote tick ignored"
        );
        let snapshot = acc.snapshot("SPX-20260717-7500-C");
        assert_eq!(snapshot.bid, Some(12.5));
        assert_eq!(snapshot.ask, Some(13.5));
        assert_eq!(snapshot.bid_size, Some(4.0));
        assert_eq!(snapshot.ask_size, None);
    }

    #[test]
    fn test_quote_update_from_accumulated_tick() {
        let (aliases, symbol) = one_leg_aliases();
        let tick = RawQuoteTick {
            symbol,
            bid: Some(12.5),
            ask: Some(13.5),
            last: Some(13.0),
            bid_size: Some(4.0),
            ask_size: Some(6.0),
        };
        match quote_update(&tick, &aliases, &pid("ibkr"), now_utc()) {
            Some(update) => {
                assert_eq!(update.bid, Some(pos(12.5)));
                assert_eq!(update.ask, Some(pos(13.5)));
                assert_eq!(update.last, Some(pos(13.0)));
            }
            None => panic!("the quote should normalize"),
        }
    }

    // === Chain assembly from the recorded shape ===============================

    #[test]
    fn test_assemble_chain_from_recorded_shape() {
        let assembled = fixture_assembled();
        let strikes: Vec<Positive> = assembled
            .fetch
            .chain
            .options
            .iter()
            .map(|o| o.strike_price)
            .collect();
        assert_eq!(strikes, vec![pos(7400.0), pos(7500.0), pos(7600.0)]);
        // Six legs (3 strikes x call/put) in the alias catalog.
        let leg_count = assembled
            .fetch
            .aliases
            .instruments()
            .filter(|i| i.provider == pid("ibkr"))
            .count();
        assert_eq!(leg_count, 6);
        // The multiplier is 100 on every leg's fingerprint.
        for instrument in assembled.fetch.aliases.instruments() {
            assert_eq!(instrument.spec.contract_multiplier, 100);
            assert_eq!(instrument.spec.venue_product_code, "SPX");
        }
        // The chain expiry is the absolute UTC instant (16:15 EDT -> 20:15 UTC).
        assert_eq!(
            assembled.fetch.expiry_source.expiration_utc,
            utc_rfc3339("2026-07-17T20:15:00+00:00")
        );
    }

    #[test]
    fn test_assemble_chain_strike_cap_exceeded() {
        let params = RawOptionParams {
            trading_class: "SPX".to_owned(),
            multiplier: "100".to_owned(),
            exchange: "SMART".to_owned(),
            expirations: vec!["20260717".to_owned()],
            strikes: vec![1.0; MAX_STRIKES + 1],
            underlying_conid: 1,
        };
        match assemble_chain(
            &params,
            "20260717",
            fixture_expiry_utc(),
            "SPX",
            &pid("ibkr"),
        ) {
            Err(ProviderError::Normalize {
                kind: NormalizeKind::LimitExceeded(cap),
            }) => assert_eq!(cap, STRIKE_CAP),
            other => panic!("expected LimitExceeded, got: {other:?}"),
        }
    }

    // === Pacing bound =========================================================

    fn window_instruments(count: usize) -> Vec<Instrument> {
        let spec = ibkr_fingerprint(100, "SPX");
        (0..count)
            .map(|i| {
                let strike = pos(1000.0 + i as f64);
                Instrument {
                    key: InstrumentKey {
                        underlying: "SPX".to_owned(),
                        expiration_utc: fixture_expiry_utc(),
                        strike,
                        style: OptionStyle::Call,
                    },
                    provider: pid("ibkr"),
                    native_symbol: format!("SPX-20260717-{strike}-C"),
                    stream_symbol: None,
                    spec: spec.clone(),
                }
            })
            .collect()
    }

    #[test]
    fn test_pacing_window_never_exceeds_cap() {
        for count in [0usize, 1, 50, MAX_SUBSCRIPTIONS, MAX_SUBSCRIPTIONS + 1, 300] {
            let window = pacing_window(window_instruments(count), MAX_SUBSCRIPTIONS);
            assert!(
                window.len() <= MAX_SUBSCRIPTIONS,
                "count {count}: window {} exceeds the pacing cap {MAX_SUBSCRIPTIONS}",
                window.len()
            );
            assert_eq!(window.len(), count.min(MAX_SUBSCRIPTIONS));
        }
    }

    #[test]
    fn test_pacing_window_is_atm_centered() {
        // 300 strikes from 1000..1299; the cap-100 window centers on the median
        // (~1150), so it excludes the far wings.
        let window = pacing_window(window_instruments(300), MAX_SUBSCRIPTIONS);
        let strikes: Vec<f64> = window.iter().map(|i| i.key.strike.to_f64()).collect();
        let lo = strikes.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = strikes.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(lo > 1000.0, "the low wing is excluded, got {lo}");
        assert!(hi < 1299.0, "the high wing is excluded, got {hi}");
        assert!(
            lo <= 1150.0 && hi >= 1150.0,
            "the window straddles the median"
        );
    }

    // === discover + compose over a mock source ================================

    struct MockChainSource {
        params: Vec<RawOptionParams>,
        detail: Option<RawContractDetail>,
    }

    #[async_trait]
    impl IbkrChainSource for MockChainSource {
        async fn option_params(
            &self,
            _underlying: &str,
        ) -> Result<Vec<RawOptionParams>, ProviderError> {
            Ok(self.params.clone())
        }
        async fn expiry_detail(
            &self,
            _underlying: &str,
            _expiration_ymd: &str,
            _strike: f64,
            _style: OptionStyle,
        ) -> Option<RawContractDetail> {
            self.detail.clone()
        }
    }

    #[tokio::test]
    async fn test_discover_returns_candidate_underlyings() {
        let adapter = sample_adapter();
        match adapter.discover().await {
            Ok(refs) => {
                let names: Vec<String> = refs.into_iter().map(|r| r.underlying).collect();
                assert!(names.contains(&"SPX".to_owned()));
                assert!(names.contains(&"SPY".to_owned()));
                assert_eq!(names.len(), CANDIDATE_UNDERLYINGS.len());
            }
            Err(e) => panic!("discover should succeed, got: {e}"),
        }
    }

    #[tokio::test]
    async fn test_compose_chain_over_mock_source() {
        let (detail, _) = fixture_detail();
        let source = MockChainSource {
            params: vec![fixture_params()],
            detail: Some(detail),
        };
        let expiration = ExpirationDate::DateTime(fixture_expiry_utc());
        let received = utc_rfc3339("2026-07-01T14:00:00+00:00");
        match compose_chain(&source, "spx", &expiration, &pid("ibkr"), received).await {
            Ok(assembled) => {
                assert_eq!(assembled.fetch.chain.symbol, "SPX");
                assert_eq!(assembled.fetch.chain.options.len(), 3);
                assert_eq!(
                    assembled.fetch.expiry_source.expiration_utc,
                    fixture_expiry_utc()
                );
            }
            Err(e) => panic!("compose_chain should succeed, got: {e}"),
        }
    }

    #[tokio::test]
    async fn test_compose_chain_mismatched_expiry_is_no_chain() {
        let source = MockChainSource {
            params: vec![fixture_params()],
            detail: None,
        };
        let other = ExpirationDate::DateTime(utc_rfc3339("2027-01-15T20:00:00+00:00"));
        let received = utc_rfc3339("2026-07-01T14:00:00+00:00");
        match compose_chain(&source, "spx", &other, &pid("ibkr"), received).await {
            Err(ProviderError::NoChain { underlying, .. }) => assert_eq!(underlying, "SPX"),
            other => panic!("expected NoChain for a mismatched expiry, got {other:?}"),
        }
    }

    // === Streaming lifecycle over a mock transport (no socket, no wall clock) ==

    struct MockTransport {
        attempts: Vec<Vec<RawTick>>,
        attempt_idx: usize,
        cursor: usize,
        backfill: Option<AssembledChain>,
        connects: Arc<StdMutex<u32>>,
        max_window: Arc<StdMutex<usize>>,
        cancel: CancellationToken,
    }

    #[async_trait]
    impl IbkrStreamTransport for MockTransport {
        async fn connect_and_subscribe(
            &mut self,
            instruments: &[Instrument],
        ) -> Result<(), TransportGone> {
            if let Ok(mut count) = self.connects.lock() {
                *count += 1;
            }
            if let Ok(mut max) = self.max_window.lock() {
                *max = (*max).max(instruments.len());
            }
            self.cursor = 0;
            Ok(())
        }

        async fn receive(&mut self) -> Result<RawTick, TransportGone> {
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
    impl IbkrStreamTransport for PendingTransport {
        async fn connect_and_subscribe(
            &mut self,
            _instruments: &[Instrument],
        ) -> Result<(), TransportGone> {
            Ok(())
        }
        async fn receive(&mut self) -> Result<RawTick, TransportGone> {
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

    #[tokio::test(start_paused = true)]
    async fn test_reconnect_loop_emits_quotes_greeks_backfill_without_panic() {
        let assembled = fixture_assembled();
        let cancel = CancellationToken::new();
        let connects = Arc::new(StdMutex::new(0));
        let max_window = Arc::new(StdMutex::new(0));
        let symbol = match assembled
            .fetch
            .aliases
            .instruments()
            .find(|i| i.provider == pid("ibkr"))
        {
            Some(instrument) => instrument.native_symbol.clone(),
            None => panic!("the fixture must carry at least one leg"),
        };
        let quote = RawTick::Quote(RawQuoteTick {
            symbol: symbol.clone(),
            bid: Some(12.5),
            ask: Some(13.5),
            last: None,
            bid_size: None,
            ask_size: None,
        });
        let greeks = RawTick::Greeks(RawGreeksTick {
            symbol,
            iv: Some(0.25),
            delta: Some(0.55),
            gamma: Some(0.01),
            theta: Some(-0.5),
            vega: Some(1.2),
        });
        let transport = MockTransport {
            attempts: vec![vec![quote.clone(), greeks.clone()], vec![quote, greeks]],
            attempt_idx: 0,
            cursor: 0,
            backfill: Some(assembled.clone()),
            connects: Arc::clone(&connects),
            max_window: Arc::clone(&max_window),
            cancel: cancel.clone(),
        };
        let (sink, mut rx_control, mut rx_coalesced) = test_sink(256);
        run_reconnect_loop(
            transport,
            pid("ibkr"),
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
        // The pacing bound held on every connect.
        assert!(
            *max_window.lock().unwrap_or_else(|e| e.into_inner()) <= MAX_SUBSCRIPTIONS,
            "the subscribe window never exceeds the pacing cap"
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
        let coalesced = drain(&mut rx_coalesced);
        assert!(
            coalesced
                .iter()
                .any(|u| matches!(u, MarketUpdate::Quote(_))),
            "the normalized quote reached the coalesced channel"
        );
        assert!(
            coalesced
                .iter()
                .any(|u| matches!(u, MarketUpdate::Greeks(_))),
            "the native Greeks reached the coalesced channel"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_reconnect_loop_drops_unknown_symbol_tick() {
        let assembled = fixture_assembled();
        let cancel = CancellationToken::new();
        let connects = Arc::new(StdMutex::new(0));
        let max_window = Arc::new(StdMutex::new(0));
        let stray = RawTick::Quote(RawQuoteTick {
            symbol: "SPX-20260717-99999-C".to_owned(),
            bid: Some(1.0),
            ask: Some(2.0),
            last: None,
            bid_size: None,
            ask_size: None,
        });
        let transport = MockTransport {
            attempts: vec![vec![stray]],
            attempt_idx: 0,
            cursor: 0,
            backfill: Some(assembled.clone()),
            connects: Arc::clone(&connects),
            max_window: Arc::clone(&max_window),
            cancel: cancel.clone(),
        };
        let (sink, mut _rx_control, mut rx_coalesced) = test_sink(256);
        run_reconnect_loop(
            transport,
            pid("ibkr"),
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
            "a tick for an unknown symbol must be dropped, never keyed"
        );
    }

    #[tokio::test]
    async fn test_reconnect_loop_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let (sink, _rx_control, _rx_coalesced) = test_sink(8);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            PendingTransport,
            pid("ibkr"),
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

    // === Poll->stream merge: venue Greeks + quote land in a seeded store =======

    #[track_caller]
    fn seeded_store() -> ChainStore {
        let assembled = fixture_assembled();
        let seeded_at = utc_rfc3339("2026-07-01T14:00:00+00:00");
        ChainStore::seed(
            assembled.fetch,
            ChainSource::Merged,
            Duration::from_secs(2),
            seeded_at,
        )
    }

    #[test]
    fn test_venue_greeks_and_quote_merge_into_seeded_store() {
        let mut store = seeded_store();
        let (aliases, symbol) = one_leg_aliases();
        let key = match aliases.resolve_symbol(&symbol) {
            Some(k) => k.clone(),
            None => panic!("the fixture symbol must resolve"),
        };
        let instrument = match aliases.instrument(&key, &pid("ibkr")) {
            Some(i) => i.clone(),
            None => panic!("the fixture instrument must resolve"),
        };
        let greeks = GreeksRow {
            instrument: instrument.clone(),
            iv: Some(pos(0.25)),
            delta: Some(Decimal::new(55, 2)),
            gamma: Some(Decimal::new(1, 2)),
            theta: Some(Decimal::new(-5, 1)),
            vega: Some(Decimal::new(12, 1)),
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: now_utc(),
        };
        assert_eq!(store.apply_greeks(&greeks), MergeOutcome::Applied);
        let quote = QuoteUpdate {
            instrument,
            bid: Some(pos(12.5)),
            ask: Some(pos(13.5)),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: None,
            received_time: now_utc(),
        };
        assert_eq!(store.apply_quote(&quote), MergeOutcome::Applied);
    }

    // === Property: normalization is total (never a panic) =====================

    proptest::proptest! {
        /// `resolve_expiry` is TOTAL: any strings yield `Ok` or `UnparseableExpiry`,
        /// never a panic (contributes to normalize_total).
        #[test]
        fn prop_resolve_expiry_total(
            ltt in "\\PC{0,20}",
            ymd in "\\PC{0,12}",
        ) {
            let session = default_session();
            let ltt_opt = if ltt.is_empty() { None } else { Some(ltt.as_str()) };
            let _ = resolve_expiry(ltt_opt, &ymd, &session);
        }

        /// `strike_positive` is TOTAL over any f64 (contributes to normalize_total).
        #[test]
        fn prop_strike_positive_total(raw in -1.0e9f64..1.0e9) {
            match strike_positive(raw) {
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
