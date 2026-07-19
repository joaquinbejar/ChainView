//! The tastytrade adapter — the poll->stream merge provider, behind a DISABLED
//! build feature (`docs/03-data-providers.md` §7.2, `docs/SECURITY.md` §2).
//!
//! tastytrade has a **native** chain endpoint, so this adapter seeds strikes from
//! the REST nested chain ([`nested_option_chain_for`](TastyTrade::nested_option_chain_for)
//! → `NestedOptionChain`) and overlays live quotes/Greeks from tastytrade's bundled
//! **dxfeed** stream, joining the two through the per-leg alias catalog because the
//! REST symbol (OCC) and the streaming symbol (dxfeed) differ
//! ([`get_streamer_symbol`](TastyTrade::get_streamer_symbol) maps OCC → dxfeed).
//!
//! # The gate — credential logging upstream (`docs/SECURITY.md` §2)
//!
//! The published `tastytrade` 0.3.0 — the checksum-pinned artifact ChainView
//! actually resolves (`Cargo.lock`) — logs credential material at `DEBUG` in four
//! sites (`docs/SECURITY.md` §2.1):
//!
//! - `src/api/client.rs` `login` — `debug!("{creds:?}")` of the `LoginResponse`
//!   (carries the `session_token`);
//! - `src/api/client.rs` `create_quote_streamer` / `quote_streamer.rs` — the
//!   session/DXLink token paths;
//! - `src/api/quote_streaming.rs` — the raw quote-token response body.
//!
//! ChainView does **not** modify that repo. Instead the whole adapter sits behind
//! the DISABLED-by-default `tastytrade` Cargo feature and is **excluded from
//! `with_builtins()`**; it is reachable only via the explicit `with_gated_builtin`
//! opt-in, which returns a typed startup error while the gate holds. So a stock
//! binary can **never** execute the upstream's logging — the credential guarantee
//! holds **by construction**, not author discipline (`docs/SECURITY.md` §3). The
//! gate lifts only when an upstream release redacts all four paths, a captured-log
//! test proves it, and the matrix cell flips in the same PR (**tastytrade#... — the
//! upstream redaction issue**, `docs/SECURITY.md` §2.3).
//!
//! # Auth is injected programmatically (no dotenv, no foreign env namespace)
//!
//! Unlike a crate that hardcodes its own env/file loading, `TastyTradeConfig` has
//! **all-public fields**, so [`login`](TastytradeAdapter::login) builds it as a
//! struct literal from ChainView-namespaced `CHAINVIEW_TASTYTRADE_*` env vars and
//! calls [`TastyTrade::login`] directly. It never calls `TastyTradeConfig::from_env`
//! / `::new` / `::default` (which would read the foreign `TASTYTRADE_*` namespace,
//! load a `.env` file, AND install a `tracing` subscriber via
//! `setup_logger_with_level`) — so ChainView owns its own tracing sink and the
//! credential is read only from `Secret::expose` at the single hand-off site
//! (`CLAUDE.md` "Credentials from env only", `docs/03-data-providers.md` §11.3).
//!
//! # Normalization happens at this seam
//!
//! Every raw `tastytrade` DTO stops here (`CLAUDE.md` "Module Boundaries"). The REST
//! strike price is a `Decimal` → checked into [`Positive`] via
//! [`Positive::new_decimal`]; the streaming `f64`s are checked into `Positive` /
//! `Decimal` inside the neutral [`dxfeed_decode`](super::dxfeed_decode) module,
//! which this adapter feeds its bundled `dxfeed::Event`s through (never an
//! adapter-to-adapter edge, `docs/03-data-providers.md` §12). Expiry is the
//! US-equity **`16:00 America/New_York` → UTC** rule, resolved DST-aware here (the
//! fixed `21:00 UTC` upstream helper is **not** used — it is DST-wrong half the
//! year, `docs/03-data-providers.md` §3).
//!
//! # IV source: the streamed dxfeed Greeks event (`docs/03-data-providers.md` §7.2)
//!
//! The REST `nested_option_chain_for` snapshot carries **no** IV field, so the
//! streamed dxfeed **Greeks** event is this provider's sole venue IV source. The
//! published `tastytrade` 0.3.0 preserves that IV through its streamer
//! (`volatility: greeks.volatility`) and has **no** `optionstratlib` dependency, so
//! there is no "conversion zeroes IV" step: the value reaches the neutral
//! [`decode_greeks`](super::dxfeed_decode::decode_greeks) helper (#38) unchanged and
//! lands in the analytics sidecar tagged `GreeksOrigin::Provider` (the #25
//! precedence). It is carried **as-is** — dxfeed IV is already a decimal fraction,
//! so no `/100`, no fabricated zero, and no narrowing. A REST leg with no overlay
//! yet falls back to local inversion, the origin glyph distinguishing the two.
//!
//! # Adapter robustness bypasses (`docs/03-data-providers.md` §7.2)
//!
//! Two upstream hazards are handled **inside** this adapter, not trusted:
//!
//! - the venue response is **checked non-empty before use** — a chain with no
//!   matching expiration, or an expiration with no normalizable strike, is a typed
//!   [`ProviderError::NoChain`], never a panic or an out-of-bounds index (the
//!   upstream nested-chain helper removes item zero; ChainView never indexes).
//! - the streamer's racy one-time sender-map clone is **not relied on**: the adapter
//!   owns the reconnect/resubscribe loop and re-subscribes the **full** leg set off
//!   the fresh aliases on every (re)connect, so a leg first seen on a later poll is
//!   always observed. A pinned upstream fix can later replace either bypass.
//!
//! # Reconnect + two update classes (`docs/03-data-providers.md` §5, [ADR-0009])
//!
//! The reconnect/resubscribe loop is **ChainView's**, driven behind the
//! [`SubscriptionHandle`]; on a dropped stream it emits `Health(Reconnecting)`,
//! backs off with jittered exponential backoff, re-`fetch_chain`s to reconcile
//! drift, and resubscribes off the fresh aliases (backfill = current state). Every
//! [`MarketUpdate`] is handed to the two-class [`MarketUpdateSink`], which routes
//! `Chain`/`Health` to the control channel and coalesces `Quote`/`Greeks`.
//!
//! [ADR-0009]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0009-provider-sink-two-class-routing.md

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, Utc, Weekday};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle};
use tokio_util::sync::CancellationToken;

use tastytrade::TastyTrade;
use tastytrade::prelude::{
    DXF_ET_GREEKS, DXF_ET_QUOTE, DxFeedSymbol, Event, EventData, InstrumentType, OptionExpiration,
    OptionNestedChain, OptionStrike, QuoteStreamer, QuoteSubscription, TastyTradeConfig,
};

use super::dxfeed_decode::{DxGreeksEvent, DxQuoteEvent, decode_greeks, decode_quote};
use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, MarketUpdateSink,
    OptionStreamCapability, Provider, ProviderCapabilities, SendState, SubscriptionHandle,
    SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSnapshot, ChainSource, ContractSpecFingerprint, ExerciseStyle,
    ExpirySource, Instrument, InstrumentKey, MarketUpdate, ProviderId, SettlementStyle,
    StreamHealth,
};
use crate::config::{EnvSource, Secret, require_credentials};
use crate::error::{NormalizeKind, ProviderError, TransportDetail, TransportKind};

/// The reserved provider id this adapter registers under
/// ([`RESERVED_PROVIDER_IDS`](crate::chain::RESERVED_PROVIDER_IDS)).
const TASTYTRADE_ID: &str = "tastytrade";

/// The tastytrade production REST base URL — a public, non-secret venue endpoint
/// (mirrors the upstream crate's private constant). Never a credential; the
/// `providers.tastytrade.endpoint` config knob may override it.
const TASTYTRADE_BASE_URL: &str = "https://api.tastyworks.com";

/// The tastytrade production streamer URL — a public, non-secret venue endpoint.
const TASTYTRADE_WS_URL: &str = "wss://streamer.tastyworks.com";

/// The credential field names read from the environment for `UserPass` auth
/// (`CHAINVIEW_TASTYTRADE_USERNAME` / `CHAINVIEW_TASTYTRADE_PASSWORD`,
/// `docs/03-data-providers.md` §11.3).
const CREDENTIAL_KEYS: [&str; 2] = ["username", "password"];

/// The suggested chain-refresh cadence, in seconds — a hint only; the effective
/// interval is `config.refresh_interval` (`docs/03-data-providers.md` §2).
const REFRESH_HINT_SECS: u32 = 2;

/// The default US-equity contract multiplier (shares per contract) when the venue
/// figure is absent or out of range.
const DEFAULT_SHARES_PER_CONTRACT: u32 = 100;

/// The quote currency US-equity option premiums settle in.
const QUOTE_CURRENCY: &str = "USD";

/// The venue `time` value that means "no venue clock". tastytrade's dxfeed binding
/// converts DXLink events with `time == 0` (DXLink carries no exchange timestamp),
/// so a zero `time` is treated as **absent** rather than the 1970 epoch instant.
const NO_VENUE_TIME_MS: i64 = 0;

// --- Reconnect backoff (docs/03-data-providers.md §5) ------------------------

/// The reconnect backoff base, in milliseconds (`BASE = 250 ms`).
const BACKOFF_BASE_MS: f64 = 250.0;
/// The reconnect backoff ceiling, in milliseconds (`MAX = 30 s`).
const BACKOFF_MAX_MS: f64 = 30_000.0;
/// The reconnect jitter magnitude — the delay is scaled by `1 + jitter`,
/// `jitter in [-0.2, 0.2]`.
const JITTER_MAGNITUDE: f64 = 0.2;
/// The largest exponent applied to `2^attempt` before the [`BACKOFF_MAX_MS`] cap
/// takes over — a ceiling that keeps `attempt` growth harmless and avoids a
/// `powi` overflow at a very large `attempt`.
const BACKOFF_MAX_SHIFT: u32 = 20;

// ---------------------------------------------------------------------------
// The adapter.
// ---------------------------------------------------------------------------

/// The tastytrade `Provider` adapter (crate-internal; behind the disabled
/// `tastytrade` feature and reachable only via `with_gated_builtin`).
///
/// Holds the reserved [`ProviderId`], the env-resolved credentials (wrapped in
/// [`Secret`], never logged), and the venue base URL. `Clone` is cheap — a clone
/// is moved into the spawned reconnect loop so it can re-`fetch_chain` and
/// re-`login` on reconnect without borrowing `&self` across the task boundary.
#[derive(Clone)]
pub(crate) struct TastytradeAdapter {
    id: ProviderId,
    username: Secret,
    password: Secret,
    base_url: String,
}

impl TastytradeAdapter {
    /// Build the adapter from the ChainView-namespaced environment
    /// (`CHAINVIEW_TASTYTRADE_USERNAME` / `CHAINVIEW_TASTYTRADE_PASSWORD`). The
    /// credentials are read **only** here (env-only policy) and wrapped in
    /// [`Secret`]; they are never logged or echoed in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::MissingCredential`](crate::error::ConfigError::MissingCredential)
    /// (naming the provider, never the key) when either credential is unset/empty.
    pub(crate) fn from_env(env: &dyn EnvSource) -> Result<Self, crate::error::ConfigError> {
        let id = tastytrade_provider_id();
        let creds = require_credentials(env, &id, &CREDENTIAL_KEYS)?;
        let username = creds
            .get("USERNAME")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        let password = creds
            .get("PASSWORD")
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::MissingCredential(id.clone()))?;
        Ok(Self {
            id,
            username,
            password,
            base_url: TASTYTRADE_BASE_URL.to_owned(),
        })
    }

    /// Log in to tastytrade, building [`TastyTradeConfig`] as a struct literal from
    /// the injected credentials so the upstream `from_env` (dotenv + foreign
    /// namespace + logger install) is never touched. The credential is exposed only
    /// at this single hand-off site and never logged.
    ///
    /// # Errors
    ///
    /// A redaction-safe [`ProviderError`] — [`ProviderError::Auth`] for a rejected
    /// credential (never carrying it), else a categorized transport failure.
    async fn login(&self) -> Result<TastyTrade, ProviderError> {
        let config = TastyTradeConfig {
            username: self.username.expose().to_owned(),
            password: self.password.expose().to_owned(),
            use_demo: false,
            // Never consulted: only `from_env`/`from_file` call `setup_logger_with_level`.
            log_level: "OFF".to_owned(),
            remember_me: false,
            base_url: self.base_url.clone(),
            websocket_url: TASTYTRADE_WS_URL.to_owned(),
        };
        TastyTrade::login(&config).await.map_err(login_error)
    }
}

#[async_trait]
impl Provider for TastytradeAdapter {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        tastytrade_capabilities()
    }

    async fn discover(&self) -> Result<Vec<UnderlyingRef>, ProviderError> {
        // tastytrade exposes no cheap "list every optionable underlying" endpoint;
        // the caller names the underlying and `fetch_chain` resolves its chain.
        Err(ProviderError::Unsupported("underlying discovery"))
    }

    async fn fetch_chain(
        &self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Result<ChainFetch, ProviderError> {
        let symbol = underlying.to_ascii_uppercase();

        // Resolve the requested expiry to an absolute-UTC target day; a relative
        // `Days` offset never reaches an `InstrumentKey`.
        let target = expiration
            .get_date()
            .map_err(|_| ProviderError::Normalize {
                kind: NormalizeKind::UnparseableExpiry,
            })?;
        let target_day = target.date_naive();

        let client = self.login().await?;
        let nested = client
            .nested_option_chain_for(symbol.clone())
            .await
            .map_err(|err| transport_error(&err))?;

        // Empty-response bypass: select the requested expiry WITHOUT indexing; a
        // missing expiry is `NoChain`, never a panic.
        let Some(chosen) = select_expiration(&nested, target_day) else {
            return Err(ProviderError::NoChain {
                underlying: symbol,
                expiration: target.to_rfc3339(),
            });
        };

        // Resolve OCC -> dxfeed streamer symbols per unique leg for the alias
        // catalog (the poll->stream join, §4). A leg whose streamer symbol cannot
        // be resolved keeps its REST identity; the stream simply will not overlay
        // it until a later poll resolves it.
        let mut streamers: HashMap<String, String> = HashMap::new();
        for strike in &chosen.strikes {
            for occ in [&strike.call, &strike.put] {
                if !streamers.contains_key(&occ.0)
                    && let Ok(dx) = client
                        .get_streamer_symbol(&InstrumentType::EquityOption, occ)
                        .await
                {
                    let _ = streamers.insert(occ.0.clone(), dx.0);
                }
            }
        }

        assemble_chain(&nested, chosen, &self.id, &|occ| {
            streamers.get(occ).cloned()
        })
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
            instruments,
            cancel,
        } = req;
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            transport,
            id,
            underlying,
            expiration_utc,
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

/// The adapter's reserved [`ProviderId`]. `"tastytrade"` is a compile-time literal
/// that satisfies the grammar (proven by `test_tastytrade_id_is_valid_and_reserved`),
/// so construction cannot fail; the fallback arm is unreachable.
fn tastytrade_provider_id() -> ProviderId {
    match ProviderId::new(TASTYTRADE_ID) {
        Ok(id) => id,
        Err(_) => unreachable!("`tastytrade` is a valid, reserved provider id literal"),
    }
}

/// tastytrade's honest capability self-declaration — the
/// `docs/03-data-providers.md` §8 row: a native REST chain, no depth,
/// venue-provided Greeks, an (unverified) contract quote stream, **no underlying
/// stream** (capability honesty: the dxfeed subscription contains only option
/// aliases and nothing folds `OptionChain::underlying_price`, so advertising an
/// underlying stream would leave the median-strike placeholder posing as a live
/// spot — declared unavailable until a real underlying quote is subscribed AND
/// folded, the same honesty resolution as the Deribit ADR-0009 row), REST chain
/// polling, no trades tape, and `UserPass` auth.
#[must_use]
pub(crate) fn tastytrade_capabilities() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Native)
        .depth(false)
        .greeks(GreeksCapability::Provided)
        .option_stream(OptionStreamCapability::ChainQuotes { verified: false })
        .underlying_stream(false)
        .chain_poll(ChainPollCapability::Poll {
            interval_hint_secs: REFRESH_HINT_SECS,
        })
        .trades_tape(false)
        .auth(AuthKind::UserPass)
        .build()
}

// ---------------------------------------------------------------------------
// Expiry resolution: 16:00 America/New_York -> UTC, DST-aware.
// ---------------------------------------------------------------------------

/// Resolve a US-equity `YYYY-MM-DD` expiry to an absolute UTC instant at the
/// venue's **`16:00 America/New_York`** close, DST-aware
/// (`docs/03-data-providers.md` §3). The fixed `21:00 UTC` upstream helper is
/// **not** used — it is DST-wrong from mid-March to early November.
///
/// The Eastern offset at a 16:00 wall-clock time is unambiguous: both DST
/// transitions occur at `02:00`, well before the close, so a same-day 16:00 is
/// EDT (`UTC-4` -> `20:00 UTC`) inside the DST window and EST (`UTC-5` ->
/// `21:00 UTC`) outside it.
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
    // UTC = local + |offset|: 16:00 EDT -> 20:00 UTC, 16:00 EST -> 21:00 UTC.
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

/// Whether US Eastern DST (EDT) is in effect on `date` at the 16:00 close: from
/// the **second Sunday of March** through the day **before** the first Sunday of
/// November (the first Sunday of November falls back to EST at 02:00, so its 16:00
/// is already EST).
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

// ---------------------------------------------------------------------------
// Numeric normalization at the seam.
// ---------------------------------------------------------------------------

/// A checked strike: the REST `Decimal` becomes a [`Positive`] via the checked
/// [`Positive::new_decimal`] (rejecting a negative), and a zero strike is refused
/// (not a real contract).
fn strike_positive(value: Decimal) -> Result<Positive, NormalizeKind> {
    let strike = Positive::new_decimal(value).map_err(|_| NormalizeKind::OutOfRange("strike"))?;
    if strike == Positive::ZERO {
        return Err(NormalizeKind::OutOfRange("strike"));
    }
    Ok(strike)
}

/// The venue shares-per-contract as a checked contract multiplier, defaulting to
/// [`DEFAULT_SHARES_PER_CONTRACT`] when the figure is zero or overflows `u32`.
fn multiplier_of(shares_per_contract: u64) -> u32 {
    u32::try_from(shares_per_contract)
        .ok()
        .filter(|value| *value >= 1)
        .unwrap_or(DEFAULT_SHARES_PER_CONTRACT)
}

/// The tastytrade economic-equivalence fingerprint: standard US-equity options are
/// **physically-settled, American-exercise**, quoted in USD, keyed by the chain's
/// root symbol.
fn tastytrade_fingerprint(root_symbol: &str, multiplier: u32) -> ContractSpecFingerprint {
    ContractSpecFingerprint {
        contract_multiplier: multiplier,
        settlement: SettlementStyle::Physical,
        exercise: ExerciseStyle::American,
        quote_currency: QUOTE_CURRENCY.to_owned(),
        venue_product_code: root_symbol.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Chain assembly: NestedOptionChain -> ChainFetch.
// ---------------------------------------------------------------------------

/// Select the expiration whose `expiration_date` resolves to `target_day`, without
/// indexing (the empty-response bypass).
fn select_expiration(
    nested: &OptionNestedChain,
    target_day: NaiveDate,
) -> Option<&OptionExpiration> {
    nested.expirations.iter().find(|expiration| {
        parse_ymd(&expiration.expiration_date).is_ok_and(|day| day == target_day)
    })
}

/// One normalized contract leg assembled into an [`OptionChain`] row and its
/// [`AliasCatalog`] entry.
#[derive(Debug, Clone)]
struct NormalizedLeg {
    strike: Positive,
    native_symbol: String,
    stream_symbol: Option<String>,
    style: OptionStyle,
}

/// Normalize one strike's call/put OCC legs, resolving each to its dxfeed streamer
/// symbol. A strike whose price will not normalize contributes **no** legs (the
/// strike is skipped), never a panic.
fn normalize_strike(
    strike: &OptionStrike,
    resolve_streamer: &dyn Fn(&str) -> Option<String>,
) -> Vec<NormalizedLeg> {
    let Ok(strike_price) = strike_positive(strike.strike_price) else {
        return Vec::new();
    };
    let call = NormalizedLeg {
        strike: strike_price,
        native_symbol: strike.call.0.clone(),
        stream_symbol: resolve_streamer(&strike.call.0),
        style: OptionStyle::Call,
    };
    let put = NormalizedLeg {
        strike: strike_price,
        native_symbol: strike.put.0.clone(),
        stream_symbol: resolve_streamer(&strike.put.0),
        style: OptionStyle::Put,
    };
    vec![call, put]
}

/// Assemble the selected expiration's strikes into a single `optionstratlib`
/// [`OptionChain`] plus its [`AliasCatalog`] and [`ExpirySource`]
/// (`docs/03-data-providers.md` §7.2). The native symbol is the OCC id and the
/// stream symbol is the dxfeed streamer symbol, so the poll->stream merge (§4) can
/// resolve either back to the shared `InstrumentKey`.
///
/// # Errors
///
/// [`ProviderError::NoChain`] when the expiration yields no normalizable strike;
/// [`ProviderError::Normalize`] when the expiry date is unparseable.
fn assemble_chain(
    nested: &OptionNestedChain,
    expiration: &OptionExpiration,
    provider: &ProviderId,
    resolve_streamer: &dyn Fn(&str) -> Option<String>,
) -> Result<ChainFetch, ProviderError> {
    let underlying = nested.underlying_symbol.0.to_ascii_uppercase();
    let expiration_utc = expiry_to_utc(&expiration.expiration_date)
        .map_err(|kind| ProviderError::Normalize { kind })?;
    let multiplier = multiplier_of(nested.shares_per_contract);
    let spec = tastytrade_fingerprint(&nested.root_symbol.0, multiplier);

    let legs: Vec<NormalizedLeg> = expiration
        .strikes
        .iter()
        .flat_map(|strike| normalize_strike(strike, resolve_streamer))
        .collect();

    if legs.is_empty() {
        return Err(ProviderError::NoChain {
            underlying,
            expiration: expiration_utc.to_rfc3339(),
        });
    }

    // The alias catalog carries native (OCC) + stream (dxfeed) symbols per leg.
    let mut aliases = AliasCatalog::new();
    for leg in &legs {
        aliases.insert(Instrument {
            key: InstrumentKey {
                underlying: underlying.clone(),
                expiration_utc,
                strike: leg.strike,
                style: leg.style,
            },
            provider: provider.clone(),
            native_symbol: leg.native_symbol.clone(),
            stream_symbol: leg.stream_symbol.clone(),
            spec: spec.clone(),
        });
    }

    // Group call/put per strike into one OptionData row (deterministic, order-free).
    let mut by_strike: std::collections::BTreeMap<Positive, StrikePair<'_>> =
        std::collections::BTreeMap::new();
    for leg in &legs {
        let entry = by_strike.entry(leg.strike).or_default();
        match leg.style {
            OptionStyle::Call => entry.call = Some(leg),
            OptionStyle::Put => entry.put = Some(leg),
        }
    }

    // The REST nested chain carries no spot; seed the chain center to the MEDIAN
    // strike (a real value derived from the strike ladder, not a fabricated quote)
    // as a provisional center refreshed by the underlying stream / next poll.
    let spot = median_strike(&by_strike);

    let mut chain = OptionChain::new(&underlying, spot, expiration_utc.to_rfc3339(), None, None);
    for strike in by_strike.keys() {
        // The REST nested chain carries no quotes/Greeks/IV — those overlay from the
        // stream (§4). Seed each row with its strike and a valid zero IV (a real
        // zero per the normalization table, never a fabricated analytic).
        chain.add_option(
            *strike,
            None,
            None,
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
    }

    Ok(ChainFetch::new(
        chain,
        ExpirySource::new(underlying, expiration_utc, provider.clone()),
        aliases,
    ))
}

/// The call/put legs sharing one strike.
#[derive(Debug, Default)]
struct StrikePair<'a> {
    call: Option<&'a NormalizedLeg>,
    put: Option<&'a NormalizedLeg>,
}

/// The median strike of a sorted strike map — the provisional chain center used
/// when the REST payload carries no spot. The map is non-empty at every call site
/// (the caller returns `NoChain` first), so the fallback is never taken.
fn median_strike(by_strike: &std::collections::BTreeMap<Positive, StrikePair<'_>>) -> Positive {
    let strikes: Vec<Positive> = by_strike.keys().copied().collect();
    let mid = strikes.len() / 2;
    strikes.get(mid).copied().unwrap_or(Positive::ONE)
}

// ---------------------------------------------------------------------------
// Redaction-safe transport / auth error mapping.
// ---------------------------------------------------------------------------

/// Map a login failure to a redaction-safe [`ProviderError`] by **category only**
/// — the credential (and any upstream message) is never interpolated
/// (`docs/03-data-providers.md` §6, `docs/SECURITY.md` §1).
fn login_error(err: tastytrade::TastyTradeError) -> ProviderError {
    transport_error(&err)
}

/// Map an upstream [`TastyTradeError`](tastytrade::TastyTradeError) to a
/// redaction-safe [`ProviderError`] by **category only** — the inner message
/// (which may hold a token, URL, or body) is never interpolated
/// (`docs/03-data-providers.md` §6).
fn transport_error(err: &tastytrade::TastyTradeError) -> ProviderError {
    use tastytrade::TastyTradeError as E;
    match err {
        E::Auth(_) => ProviderError::Auth,
        E::Json(_) => transport(TransportKind::Decode),
        E::WebSocket(_) | E::DxFeed(_) | E::Streaming(_) | E::Connection(_) | E::Io(_) => {
            transport(TransportKind::Closed)
        }
        E::Http(_) | E::Api(_) | E::Unknown(_) | E::ConfigError(_) => {
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
// The transport seam: the venue I/O the reconnect loop drives (mockable).
// ---------------------------------------------------------------------------

/// The transport is gone — a connect/subscribe step failed or the stream
/// dropped/errored. A zero-size marker: it carries no upstream text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransportGone;

/// A neutral, adapter-internal view of one raw tastytrade dxfeed event, so the
/// reconnect loop is testable against a mock with **no** upstream type. The raw
/// `dxfeed::Event` is mapped onto this inside [`LiveTransport`] and never escapes.
#[derive(Debug, Clone)]
enum RawDxEvent {
    /// A quote event: `bid`/`ask` prices (`f64`), `bid_size`/`ask_size` (upstream
    /// `i64`), and a venue `time` in ms (`0` = none).
    Quote {
        symbol: String,
        bid: f64,
        ask: f64,
        bid_size: i64,
        ask_size: i64,
        time_ms: i64,
    },
    /// A Greeks event: the five Greeks + `volatility` (all `f64`) and a venue
    /// `time` in ms (`0` = none).
    Greeks {
        symbol: String,
        delta: f64,
        gamma: f64,
        theta: f64,
        vega: f64,
        rho: f64,
        volatility: f64,
        time_ms: i64,
    },
    /// A trade or other event the adapter does not overlay — ignored.
    Ignored,
}

/// The venue-I/O seam the reconnect loop drives, so the loop runs deterministically
/// against a **mock** — no socket, no wall clock. The production [`LiveTransport`]
/// wraps the upstream `QuoteStreamer`/`QuoteSubscription` plus the adapter's REST
/// `fetch_chain`; a raw `dxfeed::Event` is decoded to [`RawDxEvent`] inside it and
/// never crosses this seam.
#[async_trait]
trait TastyTransport: Send {
    /// Open one streamer and subscribe `symbols` (dxfeed streamer symbols).
    async fn connect_and_subscribe(&mut self, symbols: Vec<String>) -> Result<(), TransportGone>;

    /// Await the next event. `Err(_)` means the stream dropped — the loop
    /// reconnects.
    async fn receive(&mut self) -> Result<RawDxEvent, TransportGone>;

    /// Re-fetch the chain to reconcile drift on reconnect (backfill = current
    /// state, §5). `None` on a failed/cancelled fetch — the caller keeps prior.
    async fn refetch(
        &mut self,
        underlying: &str,
        expiration: &ExpirationDate,
    ) -> Option<ChainFetch>;
}

/// The production [`TastyTransport`]: the upstream `QuoteStreamer`/`QuoteSubscription`
/// for live dxfeed events and the adapter's REST `fetch_chain` for the reconnect
/// backfill. The raw upstream types stay private and never escape.
struct LiveTransport {
    adapter: TastytradeAdapter,
    streamer: Option<QuoteStreamer>,
    subscription: Option<Box<QuoteSubscription>>,
}

impl LiveTransport {
    fn new(adapter: TastytradeAdapter) -> Self {
        Self {
            adapter,
            streamer: None,
            subscription: None,
        }
    }
}

#[async_trait]
impl TastyTransport for LiveTransport {
    async fn connect_and_subscribe(&mut self, symbols: Vec<String>) -> Result<(), TransportGone> {
        let client = self.adapter.login().await.map_err(|_| TransportGone)?;
        let mut streamer = client
            .create_quote_streamer()
            .await
            .map_err(|_| TransportGone)?;
        let subscription = streamer.create_sub(DXF_ET_QUOTE | DXF_ET_GREEKS);
        let dxfeed_symbols: Vec<DxFeedSymbol> = symbols.into_iter().map(DxFeedSymbol).collect();
        subscription.add_symbols(&dxfeed_symbols);
        self.subscription = Some(subscription);
        self.streamer = Some(streamer);
        Ok(())
    }

    async fn receive(&mut self) -> Result<RawDxEvent, TransportGone> {
        match self.subscription.as_mut() {
            Some(subscription) => {
                let event = subscription.get_event().await.map_err(|_| TransportGone)?;
                Ok(map_dxfeed_event(event))
            }
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

/// Map a raw tastytrade `dxfeed::Event` onto the neutral [`RawDxEvent`] — the one
/// place a raw upstream event is touched (it never escapes [`LiveTransport`]).
fn map_dxfeed_event(event: Event) -> RawDxEvent {
    let symbol = event.sym;
    match event.data {
        EventData::Quote(quote) => RawDxEvent::Quote {
            symbol,
            bid: quote.bid_price,
            ask: quote.ask_price,
            bid_size: quote.bid_size,
            ask_size: quote.ask_size,
            time_ms: quote.time,
        },
        EventData::Greeks(greeks) => RawDxEvent::Greeks {
            symbol,
            delta: greeks.delta,
            gamma: greeks.gamma,
            theta: greeks.theta,
            vega: greeks.vega,
            rho: greeks.rho,
            volatility: greeks.volatility,
            time_ms: greeks.time,
        },
        EventData::Trade(_) => RawDxEvent::Ignored,
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
/// Connect, resubscribe the full leg set, drain updates; on a drop emit
/// `Health(Reconnecting{attempt})`, back off with jitter, re-`fetch_chain` to
/// reconcile drift, then resubscribe off the **fresh** aliases. `attempt` resets
/// to 0 on a successful (re)subscribe. Cancellation is observed at every `.await`,
/// so the loop never opens a stream after cancellation and never hot-loops.
async fn run_reconnect_loop<T: TastyTransport>(
    mut transport: T,
    id: ProviderId,
    underlying: String,
    expiration_utc: DateTime<Utc>,
    mut instruments: Vec<Instrument>,
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
        if health_sent == SendState::Closed {
            return;
        }
        let delay = backoff_delay(attempt, sample_jitter());
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        // Backfill = CURRENT STATE: re-fetch to reconcile drift, then resubscribe
        // off the fresh aliases next loop.
        if let Some(fresh) = refetch(
            &mut transport,
            &id,
            &underlying,
            expiration_utc,
            &mut sink,
            &cancel,
        )
        .await
            && !fresh.is_empty()
        {
            instruments = fresh;
        }
    }
}

/// One connection attempt: connect + subscribe the leg set's streamer symbols, then
/// drain events until the stream drops or the subscription is cancelled. `attempt`
/// resets to 0 on a successful (re)subscribe.
async fn connect_stream_once<T: TastyTransport>(
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
    if sink.send(live).await == SendState::Closed {
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
        if route_event(&event, &lookup, sink).await == SendState::Closed {
            return StreamExit::Shutdown;
        }
    }
}

/// Re-fetch the chain to reconcile drift and emit the fresh `Chain` snapshot,
/// returning the fresh legs for the next resubscribe (backfill = current state).
/// Cancellation short-circuits to `None`; a failed fetch keeps prior aliases.
async fn refetch<T: TastyTransport>(
    transport: &mut T,
    id: &ProviderId,
    underlying: &str,
    expiration_utc: DateTime<Utc>,
    sink: &mut MarketUpdateSink,
    cancel: &CancellationToken,
) -> Option<Vec<Instrument>> {
    let expiration = ExpirationDate::DateTime(expiration_utc);
    let fetched = tokio::select! {
        biased;
        () = cancel.cancelled() => return None,
        result = transport.refetch(underlying, &expiration) => result,
    };
    let fetch = fetched?;

    let snapshot = MarketUpdate::Chain(chain_snapshot(&fetch, now_utc()));
    let snapshot_sent = tokio::select! {
        biased;
        () = cancel.cancelled() => return None,
        outcome = sink.send(snapshot) => outcome,
    };
    if snapshot_sent == SendState::Closed {
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

/// Assemble a streaming-current [`ChainSnapshot`] from a re-fetched [`ChainFetch`]
/// — the same `AliasCatalog` carried forward with no re-derivation. The source is
/// [`ChainSource::Merged`] (REST seeds structure, the stream overlays quotes).
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

/// The dxfeed streamer symbols to subscribe for these legs — each leg's
/// `stream_symbol` (a leg with none is not streamable and is skipped).
fn subscription_symbols(instruments: &[Instrument]) -> Vec<String> {
    instruments
        .iter()
        .filter_map(|instrument| instrument.stream_symbol.clone())
        .collect()
}

/// Index the subscribed legs by their dxfeed streamer symbol, so an incoming event
/// resolves back to the normalized [`Instrument`] (the alias-catalog reverse join,
/// §4). An event for a symbol not in this map is dropped (the unknown-symbol guard).
fn stream_lookup(instruments: &[Instrument]) -> HashMap<String, Instrument> {
    instruments
        .iter()
        .filter_map(|instrument| {
            instrument
                .stream_symbol
                .clone()
                .map(|symbol| (symbol, instrument.clone()))
        })
        .collect()
}

/// Decode one raw dxfeed event and publish the normalized update through the
/// neutral [`dxfeed_decode`](super::dxfeed_decode) helpers.
///
/// A [`RawDxEvent::Quote`] yields a [`QuoteUpdate`](crate::chain::QuoteUpdate); a
/// [`RawDxEvent::Greeks`] a [`GreeksRow`](crate::chain::GreeksRow) carrying the
/// venue IV **as-is** (§7.2, the sole venue IV source). An unknown streamer symbol
/// is a **benign drop**
/// (trace, keep prior), and a **crossed** quote is likewise a benign per-tick drop
/// (trace, keep prior) — neither feeds reconnect/health/error-rate logic. Returns
/// [`SendState::Closed`] once the consumer is gone.
async fn route_event(
    event: &RawDxEvent,
    lookup: &HashMap<String, Instrument>,
    sink: &mut MarketUpdateSink,
) -> SendState {
    let received = now_utc();
    match event {
        RawDxEvent::Quote {
            symbol,
            bid,
            ask,
            bid_size,
            ask_size,
            time_ms,
        } => {
            let Some(instrument) = lookup.get(symbol) else {
                // Unknown-symbol guard: an event for a symbol not in the subscribed
                // set is dropped (never resurrects a strike). Once the tracing sink
                // lands (governance deviation 3) a bounded `clamp_symbol(symbol)`
                // echo goes here at TRACE — the deribit-adapter house pattern.
                return SendState::Open;
            };
            // A size beyond the 2^53 exact-f64 envelope is REJECTED per tick (the
            // same benign-drop contract as a crossed tick: keep the prior quote,
            // never a silently-rounded magnitude) - in release builds too, not
            // just under debug_assert.
            let (Some(bid_size), Some(ask_size)) = (size_to_f64(*bid_size), size_to_f64(*ask_size))
            else {
                return SendState::Open;
            };
            let view = DxQuoteEvent {
                symbol: symbol.clone(),
                bid: *bid,
                ask: *ask,
                bid_size,
                ask_size,
                // A dxfeed Quote event carries no last (it rides a Trade event).
                last: None,
                event_time: ms_to_event_time(*time_ms),
                received_time: received,
            };
            match decode_quote(&view, instrument) {
                Ok(quote) => sink.send(MarketUpdate::Quote(quote)).await,
                // A momentarily-crossed tick is a benign microstructure event on a
                // fast feed: keep the prior quote, do NOT feed reconnect/health/error
                // rate. Once the tracing sink lands a `clamp_symbol` TRACE goes here.
                Err(_) => SendState::Open,
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
            time_ms,
        } => {
            let Some(instrument) = lookup.get(symbol) else {
                // Unknown-symbol guard (see the quote arm): dropped, prior kept. A
                // bounded `clamp_symbol(symbol)` TRACE goes here once the tracing
                // sink lands (governance deviation 3).
                return SendState::Open;
            };
            let view = DxGreeksEvent {
                symbol: symbol.clone(),
                delta: *delta,
                gamma: *gamma,
                theta: *theta,
                vega: *vega,
                rho: *rho,
                volatility: *volatility,
                event_time: ms_to_event_time(*time_ms),
                received_time: received,
            };
            match decode_greeks(&view, instrument) {
                // The streamed dxfeed Greeks event is this provider's sole venue IV
                // source (the REST snapshot carries none); `decode_greeks` carries the
                // IV as-is (§7.2), so the row flows through tagged
                // `GreeksOrigin::Provider` with its venue IV intact.
                Ok(greeks) => sink.send(MarketUpdate::Greeks(greeks)).await,
                // decode_greeks is total for a well-formed event; a defensive drop.
                Err(_) => SendState::Open,
            }
        }
        RawDxEvent::Ignored => SendState::Open,
    }
}

/// Convert an upstream `i64` size to the `f64` the neutral [`DxQuoteEvent`] size
/// fields carry, or `None` when the magnitude exceeds `2^53` (~9x10^15) — the
/// exact-f64 precision envelope documented on
/// [`DxQuoteEvent::bid_size`](super::dxfeed_decode). No real contract quantity
/// approaches the bound, but an out-of-envelope external value is now REJECTED
/// (a typed per-tick drop at the caller) in every build profile, never silently
/// rounded - the debug_assert-only guard held no line in release.
#[allow(clippy::cast_precision_loss)]
fn size_to_f64(size: i64) -> Option<f64> {
    if size.unsigned_abs() < (1u64 << 53) {
        Some(size as f64)
    } else {
        None
    }
}

/// Resolve a venue `time` in ms to an optional exchange `event_time` — a `0` `time`
/// (tastytrade's dxfeed binding emits it for DXLink-sourced events, which carry no
/// exchange timestamp) is treated as **absent**, not the 1970 epoch.
fn ms_to_event_time(time_ms: i64) -> Option<DateTime<Utc>> {
    if time_ms == NO_VENUE_TIME_MS {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis(time_ms)
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
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use chrono::Timelike;
    use proptest::prelude::*;
    use tokio::sync::mpsc;

    use super::*;
    use crate::chain::{GreeksOrigin, MarketUpdate};

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

    /// A map-backed [`EnvSource`] — the process environment is never mutated
    /// (which is `unsafe` on the 2024 edition).
    struct MapEnv(HashMap<String, String>);

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn creds_env() -> MapEnv {
        let mut env = HashMap::new();
        let _ = env.insert(
            "CHAINVIEW_TASTYTRADE_USERNAME".to_owned(),
            "user@example.test".to_owned(),
        );
        let _ = env.insert(
            "CHAINVIEW_TASTYTRADE_PASSWORD".to_owned(),
            "do-not-log-this".to_owned(),
        );
        MapEnv(env)
    }

    #[track_caller]
    fn sample_adapter() -> TastytradeAdapter {
        match TastytradeAdapter::from_env(&creds_env()) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed with both creds present: {e}"),
        }
    }

    /// A recorded SPY nested-chain payload consistent with the real crate response
    /// shape (kebab-case; the extra streamer-symbol fields the wire carries are
    /// ignored by the crate's `NestedOptionChain`, mirroring production).
    const NESTED_SPY_JSON: &str =
        include_str!("../../tests/fixtures/tastytrade/nested_option_chain_spy.json");

    #[track_caller]
    fn nested_spy() -> OptionNestedChain {
        match serde_json::from_str::<OptionNestedChain>(NESTED_SPY_JSON) {
            Ok(chain) => chain,
            Err(e) => panic!("nested-chain fixture must deserialize: {e}"),
        }
    }

    /// The streamer-symbol resolver a test provides (mirrors `get_streamer_symbol`
    /// per leg): a fixed OCC -> dxfeed map.
    fn streamer_map() -> HashMap<String, String> {
        let mut map = HashMap::new();
        for (occ, dx) in [
            ("SPY   260320C00500000", ".SPY260320C500"),
            ("SPY   260320P00500000", ".SPY260320P500"),
            ("SPY   260320C00510000", ".SPY260320C510"),
            ("SPY   260320P00510000", ".SPY260320P510"),
        ] {
            let _ = map.insert(occ.to_owned(), dx.to_owned());
        }
        map
    }

    #[track_caller]
    fn assembled_spy() -> ChainFetch {
        let nested = nested_spy();
        let map = streamer_map();
        // The fixture's sole expiration is 2026-03-20 (a Friday inside EDT).
        let target = date("2026-03-20");
        let Some(expiration) = select_expiration(&nested, target) else {
            panic!("fixture must contain the 2026-03-20 expiration");
        };
        match assemble_chain(&nested, expiration, &pid("tastytrade"), &|occ| {
            map.get(occ).cloned()
        }) {
            Ok(fetch) => fetch,
            Err(e) => panic!("assemble_chain should succeed for the fixture, got: {e}"),
        }
    }

    // === Identity + capabilities ==============================================

    #[test]
    fn test_tastytrade_id_is_valid_and_reserved() {
        let id = tastytrade_provider_id();
        assert_eq!(id.as_str(), "tastytrade");
        assert!(id.is_reserved());
        assert!(ProviderId::new(TASTYTRADE_ID).is_ok());
    }

    #[test]
    fn test_tastytrade_capabilities_match_section_8_row() {
        let caps = tastytrade_capabilities();
        assert_eq!(caps.chain, ChainCapability::Native);
        assert!(!caps.depth);
        assert_eq!(caps.greeks, GreeksCapability::Provided);
        assert_eq!(
            caps.option_stream,
            OptionStreamCapability::ChainQuotes { verified: false }
        );
        assert!(
            !caps.underlying_stream,
            "declared FALSE until a real underlying quote is subscribed AND folded"
        );
        assert_eq!(
            caps.chain_poll,
            ChainPollCapability::Poll {
                interval_hint_secs: REFRESH_HINT_SECS
            }
        );
        assert!(!caps.trades_tape);
        assert_eq!(caps.auth, AuthKind::UserPass);
    }

    #[test]
    fn test_adapter_reports_capabilities_and_id_via_trait() {
        let adapter: Box<dyn Provider> = Box::new(sample_adapter());
        assert_eq!(adapter.id().as_str(), "tastytrade");
        assert_eq!(adapter.capabilities().chain, ChainCapability::Native);
    }

    #[test]
    fn test_credentials_never_appear_in_debug_of_adapter_secrets() {
        let adapter = sample_adapter();
        let rendered = format!("{:?}", adapter.username);
        assert!(!rendered.contains("user@example.test"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn test_from_env_reads_chainview_namespace_only() {
        // Credentials come from CHAINVIEW_TASTYTRADE_*, NOT the foreign
        // TASTYTRADE_* namespace the upstream `from_env` would read.
        let mut env = HashMap::new();
        let _ = env.insert(
            "CHAINVIEW_TASTYTRADE_USERNAME".to_owned(),
            "alice".to_owned(),
        );
        let _ = env.insert(
            "CHAINVIEW_TASTYTRADE_PASSWORD".to_owned(),
            "secret-pw".to_owned(),
        );
        // A foreign-namespace value must be ignored.
        let _ = env.insert("TASTYTRADE_USERNAME".to_owned(), "foreign".to_owned());
        let adapter = match TastytradeAdapter::from_env(&MapEnv(env)) {
            Ok(adapter) => adapter,
            Err(e) => panic!("from_env should succeed: {e}"),
        };
        assert_eq!(adapter.username.expose(), "alice");
        assert_eq!(adapter.password.expose(), "secret-pw");
        assert_eq!(adapter.base_url, TASTYTRADE_BASE_URL);
    }

    #[test]
    fn test_from_env_missing_credential_is_error() {
        let mut env = HashMap::new();
        let _ = env.insert(
            "CHAINVIEW_TASTYTRADE_USERNAME".to_owned(),
            "alice".to_owned(),
        );
        // Password absent.
        match TastytradeAdapter::from_env(&MapEnv(env)) {
            Err(crate::error::ConfigError::MissingCredential(id)) => {
                assert_eq!(id.as_str(), "tastytrade");
            }
            Err(other) => panic!("expected MissingCredential, got a different error: {other}"),
            Ok(_) => panic!("expected MissingCredential, got Ok (adapter not Debug by design)"),
        }
    }

    // === Expiry: 16:00 America/New_York -> UTC, DST-aware =====================

    #[test]
    fn test_expiry_edt_resolves_to_2000_utc() {
        // 2026-03-20 is after the second Sunday of March (2026-03-08) -> EDT ->
        // 16:00 EDT = 20:00 UTC.
        match expiry_to_utc("2026-03-20") {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-03-20T20:00:00+00:00"),
            Err(e) => panic!("EDT expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_est_resolves_to_2100_utc() {
        // 2026-11-20 is after the first Sunday of November (2026-11-01) -> EST ->
        // 16:00 EST = 21:00 UTC.
        match expiry_to_utc("2026-11-20") {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-11-20T21:00:00+00:00"),
            Err(e) => panic!("EST expiry should resolve, got: {e}"),
        }
    }

    #[test]
    fn test_expiry_dst_start_boundary_is_edt() {
        // The second Sunday of March 2026 is 2026-03-08: at 16:00 DST is already in
        // effect (transition at 02:00), so it is EDT -> 20:00 UTC.
        assert!(is_us_eastern_dst(date("2026-03-08")));
        match expiry_to_utc("2026-03-08") {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-03-08T20:00:00+00:00"),
            Err(e) => panic!("DST-start expiry should resolve, got: {e}"),
        }
        // The day before is still EST.
        assert!(!is_us_eastern_dst(date("2026-03-07")));
    }

    #[test]
    fn test_expiry_dst_end_boundary_is_est() {
        // The first Sunday of November 2026 is 2026-11-01: at 16:00 the fall-back
        // (02:00) has already happened, so it is EST -> 21:00 UTC.
        assert!(!is_us_eastern_dst(date("2026-11-01")));
        match expiry_to_utc("2026-11-01") {
            Ok(utc) => assert_eq!(utc.to_rfc3339(), "2026-11-01T21:00:00+00:00"),
            Err(e) => panic!("DST-end expiry should resolve, got: {e}"),
        }
        // The day before (2026-10-31) is still EDT.
        assert!(is_us_eastern_dst(date("2026-10-31")));
    }

    #[test]
    fn test_expiry_fixed_2100_helper_is_not_used_in_summer() {
        // A summer expiry must NOT resolve to the fixed 21:00 UTC (the DST-wrong
        // upstream helper); it is 20:00 UTC.
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
        assert_eq!(
            expiry_to_utc("2026-03-20-01"),
            Err(NormalizeKind::UnparseableExpiry)
        );
    }

    #[test]
    fn test_nth_weekday_of_month() {
        // Second Sunday of March 2026 = 2026-03-08; first Sunday of November 2026 =
        // 2026-11-01.
        assert_eq!(
            nth_weekday_of_month(2026, 3, Weekday::Sun, 2),
            Some(date("2026-03-08"))
        );
        assert_eq!(
            nth_weekday_of_month(2026, 11, Weekday::Sun, 1),
            Some(date("2026-11-01"))
        );
    }

    // === Strike normalization =================================================

    #[test]
    fn test_strike_positive_rejects_zero_and_negative() {
        assert_eq!(
            strike_positive(Decimal::ZERO),
            Err(NormalizeKind::OutOfRange("strike"))
        );
        assert_eq!(
            strike_positive(Decimal::new(-5, 0)),
            Err(NormalizeKind::OutOfRange("strike"))
        );
    }

    #[test]
    fn test_strike_positive_accepts_real_strike() {
        match strike_positive(Decimal::new(50000, 2)) {
            Ok(strike) => assert_eq!(strike, pos(500.0)),
            Err(e) => panic!("500.00 strike should normalize, got: {e}"),
        }
    }

    #[test]
    fn test_multiplier_of_defaults_and_clamps() {
        assert_eq!(multiplier_of(100), 100);
        assert_eq!(multiplier_of(0), DEFAULT_SHARES_PER_CONTRACT);
        assert_eq!(multiplier_of(u64::MAX), DEFAULT_SHARES_PER_CONTRACT);
    }

    // === Nested-chain assembly + OCC<->dxfeed alias round-trip ================

    #[test]
    fn test_assemble_chain_from_fixture_seeds_strikes() {
        let fetch = assembled_spy();
        assert_eq!(fetch.chain.symbol, "SPY");
        // The 2026-03-20 EDT expiry -> 20:00 UTC.
        assert_eq!(
            fetch.expiry_source.expiration_utc.to_rfc3339(),
            "2026-03-20T20:00:00+00:00"
        );
        // Two strikes (500, 510), each a call + put -> 4 distinct InstrumentKeys
        // (style is part of the key, so a call and a put never collapse).
        assert_eq!(fetch.aliases.len(), 4);
    }

    #[test]
    fn test_assemble_chain_alias_round_trips_occ_and_dxfeed() {
        let fetch = assembled_spy();
        // The dxfeed streamer symbol resolves back to the shared key...
        let Some(key) = fetch.aliases.resolve_symbol(".SPY260320C500") else {
            panic!("dxfeed streamer symbol should resolve to a key");
        };
        assert_eq!(key.strike, pos(500.0));
        assert_eq!(key.style, OptionStyle::Call);
        // ...and so does the OCC native symbol.
        assert_eq!(
            fetch.aliases.resolve_symbol("SPY   260320C00500000"),
            Some(key)
        );
        // The stored instrument carries both symbols for (re)subscription.
        match fetch.aliases.instrument(key, &pid("tastytrade")) {
            Some(instrument) => {
                assert_eq!(instrument.native_symbol, "SPY   260320C00500000");
                assert_eq!(instrument.stream_symbol.as_deref(), Some(".SPY260320C500"));
                assert_eq!(instrument.spec.contract_multiplier, 100);
                assert_eq!(instrument.spec.settlement, SettlementStyle::Physical);
                assert_eq!(instrument.spec.exercise, ExerciseStyle::American);
            }
            None => panic!("the tastytrade alias for the 500 call is missing"),
        }
    }

    #[test]
    fn test_assemble_chain_missing_streamer_leaves_leg_without_stream_symbol() {
        // A leg the resolver cannot map keeps its REST identity with no stream sym.
        let nested = nested_spy();
        let target = date("2026-03-20");
        let Some(expiration) = select_expiration(&nested, target) else {
            panic!("fixture expiration missing");
        };
        let fetch = match assemble_chain(&nested, expiration, &pid("tastytrade"), &|_| None) {
            Ok(fetch) => fetch,
            Err(e) => panic!("assemble should still succeed, got: {e}"),
        };
        let Some(key) = fetch.aliases.resolve_symbol("SPY   260320C00500000") else {
            panic!("native symbol should still resolve");
        };
        match fetch.aliases.instrument(key, &pid("tastytrade")) {
            Some(instrument) => assert!(instrument.stream_symbol.is_none()),
            None => panic!("instrument missing"),
        }
    }

    #[test]
    fn test_assemble_chain_empty_expiration_is_no_chain() {
        // An expiration with no normalizable strike is NoChain, never a panic.
        let nested = nested_spy();
        let target = date("2026-06-19"); // the fixture's empty-strikes expiration
        let Some(expiration) = select_expiration(&nested, target) else {
            panic!("fixture must contain the empty 2026-06-19 expiration");
        };
        match assemble_chain(&nested, expiration, &pid("tastytrade"), &|_| None) {
            Err(ProviderError::NoChain { underlying, .. }) => assert_eq!(underlying, "SPY"),
            other => panic!("expected NoChain, got {other:?}"),
        }
    }

    #[test]
    fn test_select_expiration_absent_is_none() {
        let nested = nested_spy();
        assert!(select_expiration(&nested, date("2030-01-18")).is_none());
    }

    // === #38 view mapping: i64 sizes + venue time ============================

    #[test]
    fn test_quote_event_maps_i64_sizes_and_venue_time() {
        let instrument = leg_instrument(".SPY260320C500", OptionStyle::Call);
        let mut lookup = HashMap::new();
        let _ = lookup.insert(".SPY260320C500".to_owned(), instrument);
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);

        let event = RawDxEvent::Quote {
            symbol: ".SPY260320C500".to_owned(),
            bid: 1.5,
            ask: 1.7,
            bid_size: 10,
            ask_size: 20,
            time_ms: 1_773_000_000_000, // a real venue ms timestamp
        };
        block(route_event(&event, &lookup, &mut sink));
        match rx_coalesced.try_recv() {
            Ok(MarketUpdate::Quote(quote)) => {
                assert_eq!(quote.bid, Some(pos(1.5)));
                assert_eq!(quote.ask, Some(pos(1.7)));
                // i64 sizes crossed to f64 exactly within the 2^53 envelope.
                assert_eq!(quote.bid_size, Some(pos(10.0)));
                assert_eq!(quote.ask_size, Some(pos(20.0)));
                // A non-zero venue time became an event_time.
                assert!(quote.event_time.is_some());
            }
            other => panic!("expected a routed Quote, got {other:?}"),
        }
    }

    #[test]
    fn test_quote_event_zero_time_is_absent_event_time() {
        let instrument = leg_instrument(".SPY260320C500", OptionStyle::Call);
        let mut lookup = HashMap::new();
        let _ = lookup.insert(".SPY260320C500".to_owned(), instrument);
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);

        let event = RawDxEvent::Quote {
            symbol: ".SPY260320C500".to_owned(),
            bid: 1.5,
            ask: 1.7,
            bid_size: 1,
            ask_size: 1,
            time_ms: 0, // DXLink-sourced: no exchange timestamp
        };
        block(route_event(&event, &lookup, &mut sink));
        match rx_coalesced.try_recv() {
            Ok(MarketUpdate::Quote(quote)) => assert!(quote.event_time.is_none()),
            other => panic!("expected a routed Quote, got {other:?}"),
        }
    }

    #[test]
    fn test_ms_to_event_time_zero_is_none() {
        assert!(ms_to_event_time(0).is_none());
        assert!(ms_to_event_time(1_773_000_000_000).is_some());
    }

    // === Streamed venue IV survives to the GreeksRow (docs/03 §7.2) ==========

    #[test]
    fn test_streamed_greeks_iv_survives_to_greeks_row() {
        // The streamed dxfeed Greeks event is this provider's sole venue IV source
        // (the REST snapshot carries none); its IV must survive `decode_greeks`
        // as-is and reach the emitted `GreeksRow` (and thus the sidecar seam),
        // tagged `GreeksOrigin::Provider`.
        let instrument = leg_instrument(".SPY260320C500", OptionStyle::Call);
        let mut lookup = HashMap::new();
        let _ = lookup.insert(".SPY260320C500".to_owned(), instrument);
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);

        let event = RawDxEvent::Greeks {
            symbol: ".SPY260320C500".to_owned(),
            delta: 0.55,
            gamma: 0.01,
            theta: -0.05,
            vega: 0.20,
            rho: 0.03,
            volatility: 0.35, // a non-zero streamed IV...
            time_ms: 1_773_000_000_000,
        };
        block(route_event(&event, &lookup, &mut sink));
        match rx_coalesced.try_recv() {
            Ok(MarketUpdate::Greeks(greeks)) => {
                // ...survives to the row as-is (dxfeed IV is already decimal, no /100).
                assert_eq!(
                    greeks.iv,
                    Some(pos(0.35)),
                    "streamed venue IV must survive to the GreeksRow"
                );
                // The Greeks themselves survive and are venue-sourced.
                assert_eq!(greeks.delta, Some(Decimal::new(55, 2)));
                assert_eq!(greeks.origin, GreeksOrigin::Provider);
            }
            other => panic!("expected a routed Greeks, got {other:?}"),
        }
    }

    // === Seam rejections: crossed / unknown symbol are benign drops ==========

    #[test]
    fn test_crossed_quote_is_benign_drop_not_a_panic() {
        let instrument = leg_instrument(".SPY260320C500", OptionStyle::Call);
        let mut lookup = HashMap::new();
        let _ = lookup.insert(".SPY260320C500".to_owned(), instrument);
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);

        // A crossed quote (ask < bid): the whole update is rejected, kept prior.
        let event = RawDxEvent::Quote {
            symbol: ".SPY260320C500".to_owned(),
            bid: 2.0,
            ask: 1.0,
            bid_size: 1,
            ask_size: 1,
            time_ms: 0,
        };
        let outcome = block(route_event(&event, &lookup, &mut sink));
        assert_eq!(outcome, SendState::Open, "a benign drop is not Closed");
        assert!(
            rx_coalesced.try_recv().is_err(),
            "a crossed quote publishes nothing"
        );
    }

    #[test]
    fn test_unknown_symbol_event_is_dropped() {
        let lookup: HashMap<String, Instrument> = HashMap::new();
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);
        let event = RawDxEvent::Quote {
            symbol: ".UNKNOWN".to_owned(),
            bid: 1.0,
            ask: 1.2,
            bid_size: 1,
            ask_size: 1,
            time_ms: 0,
        };
        let outcome = block(route_event(&event, &lookup, &mut sink));
        assert_eq!(outcome, SendState::Open);
        assert!(rx_coalesced.try_recv().is_err());
    }

    #[test]
    fn test_ignored_event_publishes_nothing() {
        let lookup: HashMap<String, Instrument> = HashMap::new();
        let (mut sink, _rx_control, mut rx_coalesced) = test_sink(8);
        let outcome = block(route_event(&RawDxEvent::Ignored, &lookup, &mut sink));
        assert_eq!(outcome, SendState::Open);
        assert!(rx_coalesced.try_recv().is_err());
    }

    // === Backoff kernel =======================================================

    #[test]
    fn test_backoff_delay_is_deterministic_with_injected_jitter() {
        // attempt=1, zero jitter -> 250ms * 2 = 500ms.
        assert_eq!(backoff_delay(1, 0.0), Duration::from_millis(500));
        // Capped at MAX * (1 + 0.2) = 36 s for a large attempt.
        assert!(backoff_delay(40, 0.2) <= Duration::from_millis(36_000));
        // Never below BASE * 2 * (1 - 0.2) at attempt 1.
        assert!(backoff_delay(1, -0.2) >= Duration::from_millis(400));
    }

    // === Reconnect loop over a MOCK transport (no socket, no wall clock) ======

    /// A scripted mock transport: `attempts[n]` is the event list for connection
    /// attempt `n`, drained then a drop; once every attempt is exhausted it cancels
    /// the token so the loop stops. It records each connect's symbol set and can
    /// return fresh instruments on refetch.
    struct MockTransport {
        attempts: Vec<Vec<RawDxEvent>>,
        attempt_idx: usize,
        cursor: usize,
        refetch: Option<Vec<Instrument>>,
        subscribed: Arc<StdMutex<Vec<Vec<String>>>>,
        cancel: CancellationToken,
    }

    #[async_trait]
    impl TastyTransport for MockTransport {
        async fn connect_and_subscribe(
            &mut self,
            symbols: Vec<String>,
        ) -> Result<(), TransportGone> {
            if let Ok(mut log) = self.subscribed.lock() {
                log.push(symbols);
            }
            self.cursor = 0;
            Ok(())
        }

        async fn receive(&mut self) -> Result<RawDxEvent, TransportGone> {
            let Some(events) = self.attempts.get(self.attempt_idx) else {
                self.cancel.cancel();
                return Err(TransportGone);
            };
            if let Some(event) = events.get(self.cursor) {
                self.cursor = self.cursor.saturating_add(1);
                return Ok(event.clone());
            }
            // This attempt drained: drop (reconnect). Advance to the next attempt;
            // if none remain, cancel so the loop stops after this drop.
            self.attempt_idx = self.attempt_idx.saturating_add(1);
            self.cursor = 0;
            if self.attempt_idx >= self.attempts.len() {
                self.cancel.cancel();
            }
            Err(TransportGone)
        }

        async fn refetch(
            &mut self,
            _underlying: &str,
            _expiration: &ExpirationDate,
        ) -> Option<ChainFetch> {
            let instruments = self.refetch.clone()?;
            let mut aliases = AliasCatalog::new();
            for instrument in instruments {
                aliases.insert(instrument);
            }
            Some(ChainFetch::new(
                OptionChain::new(
                    "SPY",
                    pos(500.0),
                    "2026-03-20T20:00:00+00:00".to_owned(),
                    None,
                    None,
                ),
                ExpirySource::new("SPY", expiry_utc(), pid("tastytrade")),
                aliases,
            ))
        }
    }

    /// A transport whose `receive` never resolves, so only cancellation stops the
    /// loop — the shutdown test.
    struct PendingTransport;

    #[async_trait]
    impl TastyTransport for PendingTransport {
        async fn connect_and_subscribe(
            &mut self,
            _symbols: Vec<String>,
        ) -> Result<(), TransportGone> {
            Ok(())
        }

        async fn receive(&mut self) -> Result<RawDxEvent, TransportGone> {
            std::future::pending::<()>().await;
            Err(TransportGone)
        }

        async fn refetch(
            &mut self,
            _underlying: &str,
            _expiration: &ExpirationDate,
        ) -> Option<ChainFetch> {
            None
        }
    }

    #[track_caller]
    fn expiry_utc() -> DateTime<Utc> {
        match DateTime::parse_from_rfc3339("2026-03-20T20:00:00+00:00") {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => panic!("expiry parse: {e}"),
        }
    }

    fn leg_instrument(stream: &str, style: OptionStyle) -> Instrument {
        let strike = match style {
            OptionStyle::Call => pos(500.0),
            OptionStyle::Put => pos(510.0),
        };
        Instrument {
            key: InstrumentKey {
                underlying: "SPY".to_owned(),
                expiration_utc: expiry_utc(),
                strike,
                style,
            },
            provider: pid("tastytrade"),
            native_symbol: format!("OCC:{stream}"),
            stream_symbol: Some(stream.to_owned()),
            spec: tastytrade_fingerprint("SPY", 100),
        }
    }

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

    /// Drive a future to completion on the current-thread runtime. Used only by the
    /// non-networked `route_event` tests (their futures resolve without I/O).
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

    fn quote_event(symbol: &str, bid: f64) -> RawDxEvent {
        RawDxEvent::Quote {
            symbol: symbol.to_owned(),
            bid,
            ask: bid + 0.2,
            bid_size: 5,
            ask_size: 5,
            time_ms: 0,
        }
    }

    fn drain(rx: &mut mpsc::Receiver<MarketUpdate>) -> Vec<MarketUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn test_reconnect_loop_observes_first_and_later_subscriptions() {
        // Attempt 0 subscribes the initial leg (C500) and emits its quote, then
        // drops. The reconnect refetch returns a FRESH leg set adding C510, and
        // attempt 1 subscribes both and emits the NEW leg's quote — proving a leg
        // first seen on a later poll is still observed (the racy-clone bypass).
        let initial = vec![leg_instrument(".SPY260320C500", OptionStyle::Call)];
        let fresh = vec![
            leg_instrument(".SPY260320C500", OptionStyle::Call),
            leg_instrument(".SPY260320P510", OptionStyle::Put),
        ];
        let subscribed = Arc::new(StdMutex::new(Vec::new()));
        let transport = MockTransport {
            attempts: vec![
                vec![quote_event(".SPY260320C500", 1.0)],
                vec![quote_event(".SPY260320P510", 2.0)],
            ],
            attempt_idx: 0,
            cursor: 0,
            refetch: Some(fresh),
            subscribed: Arc::clone(&subscribed),
            cancel: CancellationToken::new(),
        };
        let cancel = transport.cancel.clone();
        let (sink, mut rx_control, mut rx_coalesced) = test_sink(64);

        run_reconnect_loop(
            transport,
            pid("tastytrade"),
            "SPY".to_owned(),
            expiry_utc(),
            initial,
            sink,
            cancel,
        )
        .await;

        // Both legs' quotes reached the coalesced channel (different keys, both
        // survive).
        let coalesced = drain(&mut rx_coalesced);
        let bids: Vec<Positive> = coalesced
            .iter()
            .filter_map(|update| match update {
                MarketUpdate::Quote(q) => q.bid,
                _ => None,
            })
            .collect();
        assert!(
            bids.contains(&pos(1.0)),
            "first subscription's quote observed"
        );
        assert!(
            bids.contains(&pos(2.0)),
            "later subscription's quote observed"
        );

        // The control channel carried the Chain backfill + health transitions.
        let control = drain(&mut rx_control);
        assert!(
            control
                .iter()
                .any(|update| matches!(update, MarketUpdate::Chain(_))),
            "the reconnect refetch emitted a Chain backfill"
        );

        // The second connect subscribed the FRESH (larger) symbol set.
        match subscribed.lock() {
            Ok(log) => {
                assert_eq!(log.len(), 2, "connected twice");
                let second = log.get(1).cloned().unwrap_or_default();
                assert!(second.contains(&".SPY260320P510".to_owned()));
            }
            Err(_) => panic!("subscribed log poisoned"),
        }
    }

    // === Property: normalization is total (never a panic) ====================

    proptest! {
        /// `expiry_to_utc` is TOTAL: any string yields `Ok` or `UnparseableExpiry`,
        /// never a panic; and every valid US-equity date resolves to exactly the
        /// `20:00` (EDT) or `21:00` (EST) UTC close, never any other wall time
        /// (contributes to `normalize_total`).
        #[test]
        fn prop_expiry_to_utc_total_and_dst_shape(
            year in 2000i32..2100,
            month in 1u32..=12,
            day in 1u32..=28,
            junk in "\\PC{0,16}",
        ) {
            // Arbitrary junk is total (no panic).
            let _ = expiry_to_utc(&junk);
            // A valid date resolves to a 20:00 or 21:00 UTC instant.
            let date_str = format!("{year:04}-{month:02}-{day:02}");
            match expiry_to_utc(&date_str) {
                Ok(utc) => {
                    let hour = utc.hour();
                    prop_assert!(
                        hour == 20 || hour == 21,
                        "unexpected UTC hour {hour} for {date_str}"
                    );
                }
                Err(kind) => prop_assert_eq!(kind, NormalizeKind::UnparseableExpiry),
            }
        }

        /// `strike_positive` is TOTAL over any `Decimal`: a positive value yields a
        /// non-zero `Positive`, a zero/negative yields `OutOfRange`, never a panic
        /// (contributes to `normalize_total`).
        #[test]
        fn prop_strike_positive_total(mantissa in -1_000_000i64..1_000_000, scale in 0u32..4) {
            match strike_positive(Decimal::new(mantissa, scale)) {
                Ok(strike) => prop_assert!(strike > Positive::ZERO),
                Err(kind) => prop_assert_eq!(kind, NormalizeKind::OutOfRange("strike")),
            }
        }
    }

    #[tokio::test]
    async fn test_reconnect_loop_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let (sink, _rx_control, _rx_coalesced) = test_sink(8);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(run_reconnect_loop(
            PendingTransport,
            pid("tastytrade"),
            "SPY".to_owned(),
            expiry_utc(),
            vec![leg_instrument(".SPY260320C500", OptionStyle::Call)],
            sink,
            loop_cancel,
        ));
        // Give the loop a moment to connect and park in `receive`, then cancel.
        tokio::task::yield_now().await;
        cancel.cancel();
        match handle.await {
            Ok(()) => {}
            Err(e) => panic!("the loop task should join cleanly on cancel, got: {e}"),
        }
    }
}
