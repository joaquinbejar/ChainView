//! The **neutral**, shared dxfeed event-decode helpers (issue #38,
//! `docs/03-data-providers.md` §3, §7.2, §7.3, §12).
//!
//! The dxfeed quote/Greeks event surface reaches ChainView through **two**
//! upstream crates that carry the *same* market data in **structurally
//! different** types:
//!
//! - the `tastytrade` crate's bundled binding — `get_event()` yields
//!   `dxfeed::Event { sym, data }` with `EventData::Quote(DxfQuoteT)` /
//!   `EventData::Greeks(DxfGreeksT)`, where sizes are `i64` and a venue
//!   `time` (ms epoch) rides along (§7.2, consumed by the tastytrade adapter,
//!   issue #40);
//! - the `dxlink` crate's typed `MarketEvent::{ Quote(QuoteEvent),
//!   Greeks(GreeksEvent) }`, where sizes are `f64` and there is **no** time
//!   field (§7.3, consumed by the standalone dxlink overlay, issue #42).
//!
//! Decoding either into a normalized [`QuoteUpdate`] / [`GreeksRow`] is
//! identical work, so this module is the **single** place that decode lives —
//! the dxlink adapter reuses the tastytrade decode path **without** an
//! adapter-to-adapter edge: both adapters depend on *this* module, and neither
//! depends on the other (the module-graph hard rule, §12,
//! [ADR-0002](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0002-provider-trait-over-direct-clients.md)).
//!
//! # A neutral intermediate view, not the upstream types
//!
//! Because the two crates expose structurally different event types — and
//! because this neutral module depends on **neither** upstream crate — the
//! shared surface is a pair of small ChainView-owned input views,
//! [`DxQuoteEvent`] / [`DxGreeksEvent`], that both adapters map their raw event
//! onto (the "neutral intermediate" the spec fixes, task 2). One decode body
//! then serves both call sites. A raw `dxfeed::Event` / `MarketEvent` never
//! enters or leaves this module, and this module never `use`s a sibling adapter,
//! `src/app.rs`, or `src/ui/*`.
//!
//! # The checked `f64` seam (governance deviation 2)
//!
//! Every dxfeed field is an upstream `f64`; `f64` never flows past this module.
//! Prices / IV / sizes become [`Positive`] and Greeks become [`Decimal`] through
//! a **checked** conversion at the seam that rejects `NaN` / `Inf` / negative
//! **before** a domain value is constructed
//! ([`CLAUDE.md`](https://github.com/joaquinbejar/ChainView/blob/main/CLAUDE.md)
//! governance deviation 2, `docs/03-data-providers.md` §3). These are live
//! display analytics, never accounting values — no money field is touched here.
//!
//! # Field-specific numeric policy (`docs/03-data-providers.md` §3 table)
//!
//! - a **zero bid is valid** (a real zero — kept, midpoint still derivable), so
//!   zero is *not* treated as absent;
//! - a **zero ask on a non-zero bid**, or any `ask < bid`, is **crossed** — the
//!   whole quote update is rejected ([`ProviderError::Normalize`] naming `ask`)
//!   so a torn quote never overwrites a good one; the caller keeps the prior;
//! - a per-field `NaN` / `Inf` / negative is **dropped** to `None` (an absent or
//!   invalid field stays `None`; the store keeps the prior value), never a
//!   fabricated zero;
//! - dxfeed IV is **already a decimal fraction** — it is carried **as-is** (no
//!   `/100`; the percentage-form division is a Deribit concern, not a dxfeed
//!   one, §3);
//! - Greeks may legitimately be negative, so there is no sign check — only the
//!   finiteness guard.
//!
//! This decoder decodes the **raw** event: the tastytrade streamed-IV narrowing
//! (§7.2, applied in #40) and the DXLink overlay equivalence gate (§7.3, applied
//! in #42) are **adapter-level** policies layered on top, not this module's job.
//!
//! # Identity is the adapter's job
//!
//! Resolving the incoming dxfeed **symbol** back to the shared
//! [`InstrumentKey`](crate::chain::InstrumentKey) via the alias catalog is the
//! *adapter's* responsibility (§4); this module takes an already-resolved
//! [`Instrument`] so it stays free of any provider's symbol scheme. The neutral
//! view still carries the raw event `symbol` opaquely for the adapter's own
//! logging — [`clamp_symbol`] bounds it before it is ever echoed into a
//! tracing field (the redaction-safe house rule, `docs/SECURITY.md` §6).

use chrono::{DateTime, Utc};
use optionstratlib::prelude::{Decimal, Positive};

use crate::chain::{GreeksOrigin, GreeksRow, Instrument, QuoteUpdate};
use crate::error::{NormalizeKind, ProviderError};

// ---------------------------------------------------------------------------
// The neutral input views both adapters map their raw dxfeed event onto.
// ---------------------------------------------------------------------------

/// A neutral, crate-internal view of a dxfeed **quote** event that both the
/// tastytrade `EventData::Quote(DxfQuoteT)` and the dxlink
/// `MarketEvent::Quote(QuoteEvent)` map onto, so [`decode_quote`] serves both
/// call sites (`docs/03-data-providers.md` §3 responsibility 4).
///
/// Numeric fields are the raw upstream `f64` (dxfeed's absent sentinel is
/// `NaN`); the checked seam in [`decode_quote`] turns each into an
/// `Option<Positive>`. The adapter builds this view from its own raw event —
/// mapping tastytrade's `i64` sizes / `time` or dxlink's `f64` sizes / absent
/// time — so the structural difference between the two crates is bridged
/// *before* decode, never inside it.
#[derive(Debug, Clone)]
pub(crate) struct DxQuoteEvent {
    /// The raw dxfeed event symbol, carried **opaque** — identity resolution is
    /// the adapter's job (§4). Echoed only through [`clamp_symbol`].
    pub(crate) symbol: String,
    /// Best bid price (`NaN` = the venue sent none).
    pub(crate) bid: f64,
    /// Best ask price (`NaN` = the venue sent none).
    pub(crate) ask: f64,
    /// Size resting at the best bid (`NaN` = none). `f64` is the common type
    /// both call sites can fill: dxlink sizes are natively fractional `f64`;
    /// tastytrade's `i64` sizes cross via an explicit checked conversion that
    /// is exact up to 2^53 (~9·10¹⁵) — a bound no real contract quantity
    /// approaches, documented here as the accepted precision envelope (#40
    /// justifies its cast against it).
    pub(crate) bid_size: f64,
    /// Size resting at the best ask (`NaN` = none). Same `f64` rationale and
    /// 2^53 precision envelope as [`bid_size`](Self::bid_size).
    pub(crate) ask_size: f64,
    /// Last traded price when the adapter has one to attach — a **dxfeed Quote
    /// event carries no last** (it arrives on a separate Trade event, out of
    /// scope for #38), so both current adapters map this to `None`. Present in
    /// the view so the decode output matches the §3 field list faithfully.
    pub(crate) last: Option<f64>,
    /// The venue's exchange timestamp — `Some` only when the feed carries one
    /// (tastytrade's `time`; dxlink's Quote event has none, so `None`)
    /// (`docs/01-domain-model.md` §5.1).
    pub(crate) event_time: Option<DateTime<Utc>>,
    /// When the adapter lifted this event off its socket — stamped at the
    /// normalization boundary (before decode), mirroring the Deribit adapter's
    /// `received` capture in its reconnect loop (§5), so [`decode_quote`] stays a
    /// **pure, deterministic** function of its inputs.
    pub(crate) received_time: DateTime<Utc>,
}

/// A neutral, crate-internal view of a dxfeed **Greeks** event that both the
/// tastytrade `EventData::Greeks(DxfGreeksT)` and the dxlink
/// `MarketEvent::Greeks(GreeksEvent)` map onto, so [`decode_greeks`] serves both
/// call sites (`docs/03-data-providers.md` §3 responsibility 4).
///
/// Every field is the raw upstream `f64`; the checked seam in [`decode_greeks`]
/// turns Greeks into `Option<Decimal>` and IV into `Option<Positive>`. dxfeed IV
/// (`volatility`) is already a decimal fraction and is carried **as-is** (§3).
#[derive(Debug, Clone)]
pub(crate) struct DxGreeksEvent {
    /// The raw dxfeed event symbol, carried opaque (see [`DxQuoteEvent::symbol`]).
    pub(crate) symbol: String,
    /// Delta (may be negative — no sign check).
    pub(crate) delta: f64,
    /// Gamma.
    pub(crate) gamma: f64,
    /// Theta (typically negative).
    pub(crate) theta: f64,
    /// Vega.
    pub(crate) vega: f64,
    /// Rho.
    pub(crate) rho: f64,
    /// Implied volatility — **already a decimal fraction**, carried as-is (no
    /// `/100`, unlike Deribit's percentage form, §3).
    pub(crate) volatility: f64,
    /// The venue's exchange timestamp — `Some` only when the feed carries one
    /// (tastytrade's `time`; dxlink's Greeks event has none, so `None`).
    pub(crate) event_time: Option<DateTime<Utc>>,
    /// When the adapter lifted this event off its socket (see
    /// [`DxQuoteEvent::received_time`]).
    pub(crate) received_time: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// The checked f64 seam — field-specific numeric normalization.
// ---------------------------------------------------------------------------

/// A checked price/size field: `NaN` / `Inf` / negative is **dropped** (returns
/// `None`), so a bad or absent tick never becomes a fabricated [`Positive`]. A
/// real **zero is valid** and is kept ([`Positive::new`] accepts `>= 0` in this
/// build, `docs/03-data-providers.md` §3 table).
fn positive_or_drop(value: f64) -> Option<Positive> {
    Positive::new(value).ok()
}

/// A checked dxfeed **IV** field. dxfeed IV is **already a decimal fraction**, so
/// it is carried **as-is** — no `/100` (contrast the Deribit percentage-form
/// division, §3). `NaN` / `Inf` / negative is dropped to `None` (field rejected,
/// prior kept); a zero IV is valid.
///
/// The body coincides with [`positive_or_drop`] because a decimal IV needs no
/// scaling; it is kept as a distinct, named seam so the "carried as-is" policy is
/// explicit and independently testable.
fn iv_or_drop(value: f64) -> Option<Positive> {
    Positive::new(value).ok()
}

/// A checked Greek: `NaN` / `Inf` is dropped to `None`; a finite value (including
/// a legitimately **negative** or zero Greek) becomes a [`Decimal`]. Uses the std
/// `TryFrom<f64>` conversion, so no `rust_decimal` trait import is needed.
fn greek_or_drop(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    Decimal::try_from(value).ok()
}

/// Normalize a best-bid/best-ask pair with the §3 field-specific rules:
///
/// - a per-field `NaN` / `Inf` / negative is **dropped** to `None` (keeps the
///   rest of the quote);
/// - a **zero bid is valid** (kept — the midpoint is still derivable);
/// - a **zero ask on a non-zero bid**, or any `ask < bid`, is **crossed** — the
///   whole update is rejected ([`OutOfRange`](NormalizeKind::OutOfRange) on
///   `ask`) so a torn quote never overwrites a good one. A zero ask on a
///   non-zero bid satisfies `ask < bid`, so both crossed cases collapse to the
///   single comparison.
fn checked_bid_ask(
    bid: f64,
    ask: f64,
) -> Result<(Option<Positive>, Option<Positive>), NormalizeKind> {
    let bid = positive_or_drop(bid);
    let ask = positive_or_drop(ask);
    if let (Some(bid_value), Some(ask_value)) = (bid, ask) {
        // A zero ask on a non-zero bid satisfies `ask < bid`, so both crossed
        // cases collapse to this single comparison.
        if ask_value < bid_value {
            return Err(NormalizeKind::OutOfRange("ask"));
        }
    }
    Ok((bid, ask))
}

// ---------------------------------------------------------------------------
// Symbol echo clamp (redaction-safe house rule, docs/SECURITY.md §6).
// ---------------------------------------------------------------------------

/// Maximum characters retained from a dxfeed event symbol when an adapter echoes
/// it into a log/tracing field. A dxfeed OCC/streamer symbol is ~25 chars, so a
/// longer one is clamped (on a `char` boundary) with an ellipsis marker, keeping
/// the field bounded no matter what the feed supplies (`docs/SECURITY.md` §6).
/// The symbol is a **non-secret** venue string — never a credential — and never
/// rides in a [`ProviderError`], whose kinds name a **field**, not a value (§6).
const MAX_SYMBOL_CHARS: usize = 48;

/// Clamp a dxfeed event `symbol` to [`MAX_SYMBOL_CHARS`] characters, appending a
/// single `…` marker when it was longer. Operates on **`char` boundaries**
/// (`chars().take(..)`), so a multi-byte UTF-8 symbol never panics on a byte
/// split and the result is bounded regardless of input length. The adapters
/// (#40/#42) call this before echoing a symbol into a tracing field (e.g. an
/// unknown-symbol warning), mirroring the replay `clamp_echo` house rule.
pub(crate) fn clamp_symbol(symbol: &str) -> String {
    if symbol.chars().count() <= MAX_SYMBOL_CHARS {
        return symbol.to_owned();
    }
    let mut clamped: String = symbol.chars().take(MAX_SYMBOL_CHARS).collect();
    clamped.push('…');
    clamped
}

// ---------------------------------------------------------------------------
// The neutral decode entry points (#40 + #42 both call these).
// ---------------------------------------------------------------------------

/// Decode a neutral dxfeed [`DxQuoteEvent`] into a normalized [`QuoteUpdate`] for
/// the already-resolved `instrument` (`docs/03-data-providers.md` §3).
///
/// Applies the checked `f64` seam and the field-specific numeric policy: a
/// per-field `NaN` / `Inf` / negative is dropped to `None`, a real zero bid is
/// kept, `last` and the sizes are dropped-or-kept the same way, and
/// `event_time` / `received_time` are carried from the view (§5.1). The raw
/// event symbol is *not* used for identity — that resolution is the adapter's
/// job (§4).
///
/// # Errors
///
/// [`ProviderError::Normalize`] with [`NormalizeKind::OutOfRange`] naming `ask`
/// when the quote is **crossed** (a zero ask on a non-zero bid, or `ask < bid`):
/// the whole update is rejected and the caller keeps the prior quote. The error
/// names the **field**, never a value — redaction-safe by construction (§6).
///
/// **Consumer contract (#40/#42):** a momentarily-crossed tick is a normal
/// microstructure event on a fast feed. Treat this error as a **benign
/// per-tick drop** (trace-level log, keep the prior) exactly as the deribit
/// adapter handles a per-tick normalize failure — it must NOT feed
/// reconnect/health/error-rate logic. The store-side merge-crossed fold
/// (`MergeOutcome::DroppedCrossed`) is the complementary stateful check.
pub(crate) fn decode_quote(
    ev: &DxQuoteEvent,
    instrument: &Instrument,
) -> Result<QuoteUpdate, ProviderError> {
    let (bid, ask) =
        checked_bid_ask(ev.bid, ev.ask).map_err(|kind| ProviderError::Normalize { kind })?;
    Ok(QuoteUpdate {
        instrument: instrument.clone(),
        bid,
        ask,
        last: ev.last.and_then(positive_or_drop),
        bid_size: positive_or_drop(ev.bid_size),
        ask_size: positive_or_drop(ev.ask_size),
        event_time: ev.event_time,
        received_time: ev.received_time,
    })
}

/// Decode a neutral dxfeed [`DxGreeksEvent`] into a normalized [`GreeksRow`] for
/// the already-resolved `instrument` (`docs/03-data-providers.md` §3).
///
/// Maps `delta` / `gamma` / `theta` / `vega` / `rho` into `Option<Decimal>` and
/// IV into `Option<Positive>` **carried as-is** (dxfeed IV is already decimal,
/// §3), tagging the row [`GreeksOrigin::Provider`] — these are venue-supplied
/// analytics. A per-field `NaN` / `Inf` (or a negative IV) is dropped to `None`;
/// a legitimately negative Greek is preserved. The raw event's IV is preserved
/// faithfully (no fabricated zero) — the tastytrade streamed-IV narrowing (§7.2)
/// is an **adapter-level** policy applied in #40, not here.
///
/// # Errors
///
/// Returns [`ProviderError`] for a uniform seam contract with [`decode_quote`]
/// (and to leave room for a future structural rejection). Today every field maps
/// independently to `Some` / `None` — a Greeks event carries no crossed-style
/// inconsistency and its identity is the pre-resolved `instrument` — so a
/// well-formed Greeks event always yields `Ok`.
pub(crate) fn decode_greeks(
    ev: &DxGreeksEvent,
    instrument: &Instrument,
) -> Result<GreeksRow, ProviderError> {
    Ok(GreeksRow {
        instrument: instrument.clone(),
        iv: iv_or_drop(ev.volatility),
        delta: greek_or_drop(ev.delta),
        gamma: greek_or_drop(ev.gamma),
        theta: greek_or_drop(ev.theta),
        vega: greek_or_drop(ev.vega),
        rho: greek_or_drop(ev.rho),
        origin: GreeksOrigin::Provider,
        event_time: ev.event_time,
        received_time: ev.received_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{
        ContractSpecFingerprint, ExerciseStyle, InstrumentKey, ProviderId, SettlementStyle,
    };
    use optionstratlib::OptionStyle;
    use proptest::prelude::*;

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

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

    fn sample_instrument() -> Instrument {
        Instrument {
            key: InstrumentKey {
                underlying: "SPY".to_owned(),
                expiration_utc: utc(1_700_000_000),
                strike: pos(400.0),
                style: OptionStyle::Call,
            },
            provider: pid("tastytrade"),
            native_symbol: "SPY   250101C00400000".to_owned(),
            stream_symbol: Some(".SPY250101C400".to_owned()),
            spec: ContractSpecFingerprint {
                contract_multiplier: 100,
                settlement: SettlementStyle::Cash,
                exercise: ExerciseStyle::European,
                quote_currency: "USD".to_owned(),
                venue_product_code: "SPY".to_owned(),
            },
        }
    }

    /// A neutral quote view carrying every field explicitly.
    fn quote_ev(bid: f64, ask: f64) -> DxQuoteEvent {
        DxQuoteEvent {
            symbol: ".SPY250101C400".to_owned(),
            bid,
            ask,
            bid_size: 10.0,
            ask_size: 20.0,
            last: None,
            event_time: Some(utc(1_700_000_099)),
            received_time: utc(1_700_000_100),
        }
    }

    /// A neutral Greeks view carrying every field explicitly.
    fn greeks_ev(delta: f64, iv: f64) -> DxGreeksEvent {
        DxGreeksEvent {
            symbol: ".SPY250101C400".to_owned(),
            delta,
            gamma: 0.01,
            theta: -0.05,
            vega: 0.20,
            rho: 0.03,
            volatility: iv,
            event_time: Some(utc(1_700_000_099)),
            received_time: utc(1_700_000_100),
        }
    }

    // The two call-site shapes, built the way each adapter (#40/#42) would map
    // its raw event onto the neutral view. The structural difference between the
    // crates (tastytrade: `i64` sizes + a `time` ms field; dxlink: `f64` sizes,
    // no time) is bridged HERE, before decode — never inside it.

    /// Build the neutral view a **tastytrade** adapter would produce from its
    /// `DxfQuoteT` (i32 sizes stand in for the upstream `i64`, converted without
    /// an `as` cast; `time` is a ms epoch).
    fn tastytrade_quote(
        bid: f64,
        ask: f64,
        bid_size: i32,
        ask_size: i32,
        time_ms: i64,
    ) -> DxQuoteEvent {
        DxQuoteEvent {
            symbol: ".SPY250101C400".to_owned(),
            bid,
            ask,
            bid_size: f64::from(bid_size),
            ask_size: f64::from(ask_size),
            last: None,
            event_time: DateTime::<Utc>::from_timestamp_millis(time_ms),
            received_time: utc(1_700_000_100),
        }
    }

    /// Build the neutral view a **dxlink** adapter would produce from its
    /// `QuoteEvent` (f64 sizes; no time field — the adapter supplies whatever
    /// `event_time` it can, here matching the tastytrade case for the identical-
    /// data equivalence proof).
    fn dxlink_quote(
        bid: f64,
        ask: f64,
        bid_size: f64,
        ask_size: f64,
        event_time: Option<DateTime<Utc>>,
    ) -> DxQuoteEvent {
        DxQuoteEvent {
            symbol: ".SPY250101C400".to_owned(),
            bid,
            ask,
            bid_size,
            ask_size,
            last: None,
            event_time,
            received_time: utc(1_700_000_100),
        }
    }

    fn tastytrade_greeks(delta: f64, iv: f64, time_ms: i64) -> DxGreeksEvent {
        DxGreeksEvent {
            symbol: ".SPY250101C400".to_owned(),
            delta,
            gamma: 0.01,
            theta: -0.05,
            vega: 0.20,
            rho: 0.03,
            volatility: iv,
            event_time: DateTime::<Utc>::from_timestamp_millis(time_ms),
            received_time: utc(1_700_000_100),
        }
    }

    fn dxlink_greeks(delta: f64, iv: f64, event_time: Option<DateTime<Utc>>) -> DxGreeksEvent {
        DxGreeksEvent {
            symbol: ".SPY250101C400".to_owned(),
            delta,
            gamma: 0.01,
            theta: -0.05,
            vega: 0.20,
            rho: 0.03,
            volatility: iv,
            event_time,
            received_time: utc(1_700_000_100),
        }
    }

    #[track_caller]
    fn decoded_quote(ev: &DxQuoteEvent) -> QuoteUpdate {
        match decode_quote(ev, &sample_instrument()) {
            Ok(q) => q,
            Err(e) => panic!("expected a decoded quote, got: {e}"),
        }
    }

    #[track_caller]
    fn decoded_greeks(ev: &DxGreeksEvent) -> GreeksRow {
        match decode_greeks(ev, &sample_instrument()) {
            Ok(g) => g,
            Err(e) => panic!("expected decoded greeks, got: {e}"),
        }
    }

    fn quotes_field_equal(a: &QuoteUpdate, b: &QuoteUpdate) -> bool {
        a.instrument.key == b.instrument.key
            && a.bid == b.bid
            && a.ask == b.ask
            && a.last == b.last
            && a.bid_size == b.bid_size
            && a.ask_size == b.ask_size
            && a.event_time == b.event_time
            && a.received_time == b.received_time
    }

    fn greeks_field_equal(a: &GreeksRow, b: &GreeksRow) -> bool {
        a.instrument.key == b.instrument.key
            && a.iv == b.iv
            && a.delta == b.delta
            && a.gamma == b.gamma
            && a.theta == b.theta
            && a.vega == b.vega
            && a.rho == b.rho
            && a.origin == b.origin
            && a.event_time == b.event_time
            && a.received_time == b.received_time
    }

    // --- decode_quote: present fields, clocks --------------------------------

    #[test]
    fn test_decode_quote_maps_present_fields() {
        let q = decoded_quote(&DxQuoteEvent {
            last: Some(1.6),
            ..quote_ev(1.5, 1.7)
        });
        assert_eq!(q.bid, Some(pos(1.5)));
        assert_eq!(q.ask, Some(pos(1.7)));
        assert_eq!(q.last, Some(pos(1.6)));
        assert_eq!(q.bid_size, Some(pos(10.0)));
        assert_eq!(q.ask_size, Some(pos(20.0)));
        assert_eq!(q.event_time, Some(utc(1_700_000_099)));
        assert_eq!(q.received_time, utc(1_700_000_100));
        assert_eq!(q.instrument.key.strike, pos(400.0));
    }

    #[test]
    fn test_decode_quote_event_time_none_when_absent() {
        let q = decoded_quote(&DxQuoteEvent {
            event_time: None,
            ..quote_ev(1.5, 1.7)
        });
        assert!(q.event_time.is_none());
        // received_time is always present.
        assert_eq!(q.received_time, utc(1_700_000_100));
    }

    // --- decode_quote: zero-as-absent is NOT applied to a real zero bid ------

    #[test]
    fn test_decode_quote_zero_bid_is_kept_not_absent() {
        // A real zero bid is a valid value (midpoint still derivable), so it is
        // KEPT as Some(0), never treated as absent (docs/03 §3 table).
        let q = decoded_quote(&quote_ev(0.0, 1.0));
        assert_eq!(q.bid, Some(Positive::ZERO));
        assert_eq!(q.ask, Some(pos(1.0)));
    }

    #[test]
    fn test_decode_quote_zero_bid_and_zero_ask_both_valid() {
        // Zero ask is valid only when bid is also zero (not crossed).
        let q = decoded_quote(&quote_ev(0.0, 0.0));
        assert_eq!(q.bid, Some(Positive::ZERO));
        assert_eq!(q.ask, Some(Positive::ZERO));
    }

    // --- decode_quote: crossed → reject the whole update ---------------------

    #[test]
    fn test_decode_quote_zero_ask_on_nonzero_bid_is_crossed() {
        match decode_quote(&quote_ev(1.5, 0.0), &sample_instrument()) {
            Err(ProviderError::Normalize {
                kind: NormalizeKind::OutOfRange(field),
            }) => assert_eq!(field, "ask"),
            other => panic!("expected crossed OutOfRange(\"ask\"), got {other:?}"),
        }
    }

    #[test]
    fn test_decode_quote_ask_below_bid_is_crossed() {
        match decode_quote(&quote_ev(2.0, 1.0), &sample_instrument()) {
            Err(ProviderError::Normalize {
                kind: NormalizeKind::OutOfRange(field),
            }) => assert_eq!(field, "ask"),
            other => panic!("expected crossed OutOfRange(\"ask\"), got {other:?}"),
        }
    }

    #[test]
    fn test_decode_quote_error_names_field_not_value() {
        // The redaction-safe rule: the error names the FIELD, and its Display
        // carries no bid/ask value (docs/03 §6).
        match decode_quote(&quote_ev(9.99, 0.01), &sample_instrument()) {
            Err(e) => {
                let rendered = e.to_string();
                assert!(
                    rendered.contains("ask"),
                    "should name the ask field: {rendered}"
                );
                assert!(
                    !rendered.contains("9.99"),
                    "must not echo the bid value: {rendered}"
                );
                assert!(
                    !rendered.contains("0.01"),
                    "must not echo the ask value: {rendered}"
                );
            }
            Ok(q) => panic!("expected a crossed rejection, got {q:?}"),
        }
    }

    // --- decode_quote: per-field NaN/Inf/negative dropped, keeps the rest ----

    #[test]
    fn test_decode_quote_nan_bid_dropped_keeps_ask() {
        let q = decoded_quote(&quote_ev(f64::NAN, 1.7));
        assert!(q.bid.is_none(), "a NaN bid is dropped to None (absent)");
        assert_eq!(q.ask, Some(pos(1.7)), "the ask survives the dropped bid");
    }

    #[test]
    fn test_decode_quote_inf_ask_dropped_keeps_bid() {
        let q = decoded_quote(&quote_ev(1.5, f64::INFINITY));
        assert_eq!(q.bid, Some(pos(1.5)));
        assert!(q.ask.is_none(), "an infinite ask is dropped to None");
    }

    #[test]
    fn test_decode_quote_negative_fields_dropped() {
        let q = decoded_quote(&DxQuoteEvent {
            bid_size: -1.0,
            ask_size: -2.0,
            last: Some(-5.0),
            ..quote_ev(1.5, 1.7)
        });
        assert!(q.bid_size.is_none());
        assert!(q.ask_size.is_none());
        assert!(q.last.is_none());
        // The valid bid/ask still come through.
        assert_eq!(q.bid, Some(pos(1.5)));
        assert_eq!(q.ask, Some(pos(1.7)));
    }

    #[test]
    fn test_decode_quote_absent_last_stays_none() {
        // A dxfeed Quote event carries no last; the view maps it to None and the
        // decode never fabricates one.
        let q = decoded_quote(&quote_ev(1.5, 1.7));
        assert!(q.last.is_none());
    }

    // --- decode_greeks: field mapping + origin -------------------------------

    #[test]
    fn test_decode_greeks_maps_all_fields() {
        let g = decoded_greeks(&greeks_ev(-0.25, 0.35));
        assert_eq!(g.delta, Some(Decimal::new(-25, 2)));
        assert_eq!(g.gamma, Some(Decimal::new(1, 2)));
        assert_eq!(g.theta, Some(Decimal::new(-5, 2)));
        assert_eq!(g.vega, Some(Decimal::new(20, 2)));
        assert_eq!(g.rho, Some(Decimal::new(3, 2)));
        assert_eq!(g.origin, GreeksOrigin::Provider);
        assert_eq!(g.event_time, Some(utc(1_700_000_099)));
        assert_eq!(g.received_time, utc(1_700_000_100));
    }

    #[test]
    fn test_decode_greeks_iv_carried_as_is_no_division() {
        // dxfeed IV is already a decimal fraction — a 0.35 IV stays 0.35, NOT
        // 0.0035 (no percentage-form /100, contrast Deribit, docs/03 §3).
        let g = decoded_greeks(&greeks_ev(0.5, 0.35));
        assert_eq!(g.iv, Some(pos(0.35)));
    }

    #[test]
    fn test_decode_greeks_zero_iv_is_valid() {
        let g = decoded_greeks(&greeks_ev(0.5, 0.0));
        assert_eq!(g.iv, Some(Positive::ZERO));
    }

    #[test]
    fn test_decode_greeks_negative_greek_preserved() {
        // Greeks may legitimately be negative — no sign check.
        let g = decoded_greeks(&greeks_ev(-0.75, 0.35));
        assert_eq!(g.delta, Some(Decimal::new(-75, 2)));
    }

    #[test]
    fn test_decode_greeks_nan_greek_dropped_keeps_others() {
        let g = decoded_greeks(&DxGreeksEvent {
            gamma: f64::NAN,
            ..greeks_ev(-0.25, 0.35)
        });
        assert!(g.gamma.is_none(), "a NaN gamma is dropped to None");
        assert_eq!(
            g.delta,
            Some(Decimal::new(-25, 2)),
            "the other Greeks survive"
        );
        assert_eq!(g.iv, Some(pos(0.35)));
    }

    #[test]
    fn test_decode_greeks_negative_iv_dropped() {
        let g = decoded_greeks(&greeks_ev(0.5, -0.1));
        assert!(g.iv.is_none(), "a negative IV is a field rejection (None)");
        // The Greeks are unaffected.
        assert_eq!(g.delta, Some(Decimal::new(50, 2)));
    }

    #[test]
    fn test_decode_greeks_all_absent_stays_none_still_provider() {
        let g = decoded_greeks(&DxGreeksEvent {
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
            rho: f64::INFINITY,
            volatility: f64::NAN,
            ..greeks_ev(0.0, 0.0)
        });
        assert!(g.iv.is_none());
        assert!(g.delta.is_none());
        assert!(g.gamma.is_none());
        assert!(g.theta.is_none());
        assert!(g.vega.is_none());
        assert!(g.rho.is_none());
        // Origin is still Provider even when every analytic is absent — never a
        // fabricated value (docs/03 §3).
        assert_eq!(g.origin, GreeksOrigin::Provider);
    }

    // --- Both call-site shapes decode identically for identical data ---------

    #[test]
    fn test_tastytrade_and_dxlink_quote_shapes_decode_identically() {
        // Identical logical data reaches decode two ways: the tastytrade adapter
        // maps i64 sizes + a `time` ms field; the dxlink adapter maps f64 sizes +
        // an event_time. Once on the neutral view, the SAME decode body yields
        // field-identical QuoteUpdates.
        let time_ms = 1_700_000_099_000;
        let taste = tastytrade_quote(1.5, 1.7, 10, 20, time_ms);
        let dxl = dxlink_quote(
            1.5,
            1.7,
            10.0,
            20.0,
            DateTime::<Utc>::from_timestamp_millis(time_ms),
        );
        let a = decoded_quote(&taste);
        let b = decoded_quote(&dxl);
        assert!(
            quotes_field_equal(&a, &b),
            "identical data must decode identically: {a:?} vs {b:?}"
        );
        assert_eq!(a.bid, Some(pos(1.5)));
        assert_eq!(a.event_time, Some(utc(1_700_000_099)));
    }

    #[test]
    fn test_tastytrade_and_dxlink_greeks_shapes_decode_identically() {
        let time_ms = 1_700_000_099_000;
        let taste = tastytrade_greeks(-0.25, 0.35, time_ms);
        let dxl = dxlink_greeks(-0.25, 0.35, DateTime::<Utc>::from_timestamp_millis(time_ms));
        let a = decoded_greeks(&taste);
        let b = decoded_greeks(&dxl);
        assert!(
            greeks_field_equal(&a, &b),
            "identical data must decode identically: {a:?} vs {b:?}"
        );
        assert_eq!(a.iv, Some(pos(0.35)));
        assert_eq!(a.delta, Some(Decimal::new(-25, 2)));
    }

    // --- clamp_symbol --------------------------------------------------------

    #[test]
    fn test_clamp_symbol_under_limit_unchanged() {
        let sym = ".SPY250101C400";
        assert_eq!(clamp_symbol(sym), sym);
    }

    #[test]
    fn test_clamp_symbol_over_limit_bounded_with_marker() {
        let sym = "A".repeat(MAX_SYMBOL_CHARS + 20);
        let clamped = clamp_symbol(&sym);
        assert_eq!(
            clamped.chars().count(),
            MAX_SYMBOL_CHARS + 1,
            "clamped to the cap plus one marker char"
        );
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn test_clamp_symbol_multibyte_no_panic() {
        // A multi-byte symbol over the cap must clamp on a char boundary, never
        // panic on a byte split (λ is a 2-byte UTF-8 code point).
        let sym = "λ".repeat(MAX_SYMBOL_CHARS + 5);
        let clamped = clamp_symbol(&sym);
        assert_eq!(clamped.chars().count(), MAX_SYMBOL_CHARS + 1);
        assert!(clamped.ends_with('…'));
    }

    // --- Property: normalize is total (never a panic; typed error or a row) ---

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

        /// `decode_quote` is total over any set of `f64` (including NaN/Inf): it
        /// returns without panic, never yields a crossed Ok, and every present
        /// price/size is a valid `Positive` (so NaN/Inf/negative never becomes
        /// one). An Err is exactly the crossed `OutOfRange("ask")`.
        #[test]
        fn prop_decode_quote_is_total(
            bid in proptest::num::f64::ANY,
            ask in proptest::num::f64::ANY,
            bid_size in proptest::num::f64::ANY,
            ask_size in proptest::num::f64::ANY,
            last in prop_oneof![Just(None), proptest::num::f64::ANY.prop_map(Some)],
        ) {
            let ev = DxQuoteEvent {
                symbol: "SYM".to_owned(),
                bid,
                ask,
                bid_size,
                ask_size,
                last,
                event_time: None,
                received_time: utc(1_700_000_100),
            };
            match decode_quote(&ev, &sample_instrument()) {
                Ok(q) => {
                    if let (Some(b), Some(a)) = (q.bid, q.ask) {
                        prop_assert!(a >= b, "an Ok quote is never crossed");
                    }
                    // received_time is always carried through.
                    prop_assert_eq!(q.received_time, utc(1_700_000_100));
                }
                Err(ProviderError::Normalize { kind }) => {
                    prop_assert_eq!(kind, NormalizeKind::OutOfRange("ask"));
                }
                Err(other) => prop_assert!(false, "unexpected error variant: {:?}", other),
            }
        }

        /// `decode_greeks` is total over any set of `f64`: it never panics, always
        /// yields a row, a present IV is a valid (finite, non-negative) `Positive`,
        /// and the origin is always `Provider`.
        #[test]
        fn prop_decode_greeks_is_total(
            delta in proptest::num::f64::ANY,
            gamma in proptest::num::f64::ANY,
            theta in proptest::num::f64::ANY,
            vega in proptest::num::f64::ANY,
            rho in proptest::num::f64::ANY,
            volatility in proptest::num::f64::ANY,
        ) {
            let ev = DxGreeksEvent {
                symbol: "SYM".to_owned(),
                delta,
                gamma,
                theta,
                vega,
                rho,
                volatility,
                event_time: None,
                received_time: utc(1_700_000_100),
            };
            match decode_greeks(&ev, &sample_instrument()) {
                Ok(g) => {
                    if let Some(iv) = g.iv {
                        prop_assert!(iv >= Positive::ZERO);
                    }
                    prop_assert_eq!(g.origin, GreeksOrigin::Provider);
                }
                Err(other) => prop_assert!(false, "greeks decode should not error: {:?}", other),
            }
        }

        /// `clamp_symbol` is total and bounded: the result never exceeds the cap
        /// plus a single marker char, for any input.
        #[test]
        fn prop_clamp_symbol_is_bounded(sym in ".{0,256}") {
            let clamped = clamp_symbol(&sym);
            prop_assert!(clamped.chars().count() <= MAX_SYMBOL_CHARS + 1);
        }
    }
}
