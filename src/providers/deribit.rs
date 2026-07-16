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
//! # Scope
//!
//! This module lands the poll leg (`discover`/`fetch_chain`) and the honest
//! [`capabilities`](Provider::capabilities). The streaming overlay + reconnect
//! loop is issue #16, so [`subscribe`](Provider::subscribe) returns
//! [`ProviderError::Unsupported`] here.
//!
//! [ADR-0003]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0003-zero-config-first-run.md

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use deribit_http::model::instrument::{
    Instrument as DeribitInstrument, OptionType as DeribitOptionType,
};
use deribit_http::model::other::OptionInstrument;
use deribit_http::{DeribitHttpClient, HttpConfig, HttpError};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use optionstratlib::{ExpirationDate, OptionStyle};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
    Provider, ProviderCapabilities, SubscriptionHandle, SubscriptionRequest, UnderlyingRef,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ContractSpecFingerprint, ExerciseStyle, ExpirySource, Instrument,
    InstrumentKey, MarketUpdate, ProviderId, SettlementStyle,
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

// ---------------------------------------------------------------------------
// The adapter.
// ---------------------------------------------------------------------------

/// The Deribit `Provider` adapter (crate-internal; registered through
/// [`ChainViewAppBuilder::with_builtins`](crate::ChainViewAppBuilder)).
///
/// Holds the upstream REST client (built for the production venue, no
/// credentials) and its reserved [`ProviderId`]. Raw upstream types stay inside
/// this module — nothing on the public surface names a `deribit-http` DTO.
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
    /// normalizes it. A ticker fetch that fails, a task that panics, or a payload
    /// that will not normalize degrades **that leg only** (it is skipped) — never
    /// the whole chain — preserving the sequential path's error semantics. The
    /// completion order does not matter: [`assemble_chain`] groups by strike.
    async fn hydrate_legs(&self, selected: Vec<DeribitInstrument>) -> Vec<NormalizedLeg> {
        let mut pending = selected.into_iter();
        let mut join_set: JoinSet<Option<NormalizedLeg>> = JoinSet::new();

        // Prime up to the concurrency cap.
        for _ in 0..MAX_CONCURRENT_TICKERS {
            let Some(instrument) = pending.next() else {
                break;
            };
            self.spawn_ticker(&mut join_set, instrument);
        }

        let mut legs = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            // `Err` = the task panicked/was cancelled; `Ok(None)` = the ticker
            // failed or the payload would not normalize. Either way, skip the leg.
            if let Ok(Some(leg)) = joined {
                legs.push(leg);
            }
            if let Some(instrument) = pending.next() {
                self.spawn_ticker(&mut join_set, instrument);
            }
        }
        legs
    }

    /// Spawn one bounded ticker-hydration task onto `join_set`. The task owns a
    /// cloned client and the instrument, so it is `'static`; a fetch/normalize
    /// failure resolves to `None` (the leg is skipped), never an abort.
    fn spawn_ticker(
        &self,
        join_set: &mut JoinSet<Option<NormalizedLeg>>,
        instrument: DeribitInstrument,
    ) {
        let client = self.client.clone();
        let _ = join_set.spawn(async move {
            let ticker = client.get_ticker(&instrument.instrument_name).await.ok()?;
            let option = OptionInstrument { instrument, ticker };
            normalize_leg(&option).ok()
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

        let legs = self.hydrate_legs(selected).await;

        if legs.is_empty() {
            return Err(ProviderError::NoChain {
                underlying: currency,
                expiration: target.to_rfc3339(),
            });
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
        _req: SubscriptionRequest,
        _tx: mpsc::Sender<MarketUpdate>,
    ) -> Result<SubscriptionHandle, ProviderError> {
        // The streaming overlay + reconnect/resubscribe loop is issue #16; the
        // poll leg above is the supported live path until then.
        Err(ProviderError::Unsupported("deribit streaming lands in #16"))
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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use deribit_http::model::instrument::InstrumentKind;
    use deribit_http::model::other::Greeks;
    use deribit_http::model::ticker::{TickerData, TickerStats};
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

    /// Drive a future that resolves on the first poll (the adapter's `subscribe`
    /// returns immediately) without pulling in a tokio runtime. `Waker::noop` is
    /// stable from Rust 1.85 (the crate MSRV), so no `unsafe` is needed.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future parked; deribit subscribe must resolve at once"),
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

    // --- subscribe is honestly unsupported until #16 -------------------------

    #[test]
    fn test_deribit_subscribe_is_unsupported() {
        let adapter = DeribitAdapter::new();
        let (tx, _rx) = mpsc::channel::<MarketUpdate>(1);
        let request = SubscriptionRequest::new("BTC", utc_millis(1_751_011_200_000), Vec::new());
        match block_on(adapter.subscribe(request, tx)) {
            Err(ProviderError::Unsupported(what)) => {
                assert_eq!(what, "deribit streaming lands in #16");
            }
            other => panic!("expected Unsupported, got {other:?}"),
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
}
