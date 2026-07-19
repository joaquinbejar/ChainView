//! Normalized streaming update events and the freshness clocks
//! (`docs/01-domain-model.md` §5 and §5.1).
//!
//! A provider adapter emits these events across the seam; the `ChainStore`
//! (issue #7) folds them into the live `optionstratlib` chain. They are the
//! payloads of `AppEvent::Market` (`docs/02-tui-architecture.md` §4). Because a
//! **provider emits them**, they are DOMAIN types (they live under
//! `src/chain/*` and know nothing about ratatui) and can never be UI types.
//!
//! # Numbers are display analytics, never accounting values
//!
//! Every quote, IV, size, and Greek carried here is a **display analytic**
//! derived from a provider float — it is *never* an accounting value. Money
//! that must reconcile (the replay bundle) is integer cents elsewhere; these
//! live numbers are `Positive`/`Decimal` for rendering only. Greeks are
//! [`Decimal`] (`rust_decimal`, re-exported by `optionstratlib`) per the
//! ruleset; prices/IV/sizes are [`Positive`]. The checked conversion that
//! rejects `NaN`/`Inf`/negative lives in the adapter seam (issue #15), so these
//! types only ever accept already-valid domain numerics.
//!
//! # Every numeric field is `Option`
//!
//! A provider that does not supply a value leaves the field `None`; the widget
//! renders an em dash (`—`), **never a fabricated zero**. The type therefore
//! forbids inventing a value for something the feed did not send.
//!
//! # Two clocks per update (§5.1)
//!
//! Each event carries two timestamps so the store can tell a *silent* feed from
//! a *delayed* one:
//!
//! - [`event_time`](QuoteUpdate::event_time) — the venue's own exchange
//!   timestamp, `Some` **only** when the feed carries one (Deribit ticker/book,
//!   dxfeed events, Alpaca bars do; some IG price updates do not). It is the
//!   truth for *how old the venue says this is*.
//! - `received_time` — set by the adapter at normalization and **always
//!   present**. It is the truth for *when ChainView last heard anything*.
//!
//! The watermark / per-component staleness / feed-delay logic that consumes
//! these two clocks and the [freshness thresholds](#freshness-thresholds) is
//! **store behaviour** and lands in issue #7 — this module provides only the
//! fields and the threshold values.
//!
//! # Freshness thresholds
//!
//! The named threshold constants ([`QUOTE_STALE_AFTER`], [`GREEKS_STALE_AFTER`],
//! [`CHAIN_STALE_SLACK`] / [`chain_stale_after`], [`FEED_DELAY_WARN`], and
//! [`DIRECTION_DECAY`]) match the documented defaults in
//! `docs/01-domain-model.md` §5.1 and §6. The store (issue #7) compares a
//! component's age against them; this module only defines the values.

use std::time::Duration;

use chrono::{DateTime, Utc};
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};

use super::fetch::AliasCatalog;
use super::identity::{Instrument, ProviderId};

// --- Streaming events (§5) ---------------------------------------------------

/// A quote refresh for one [`Instrument`] (`docs/01-domain-model.md` §5).
///
/// `bid`/`ask`/`last`/sizes are **display analytics** (provider floats), never
/// accounting values. Every numeric field is `Option`: a value the feed omits
/// stays `None` and renders as an em dash (`—`), never a fabricated zero.
/// Updates are last-value-wins per [`Instrument`], so a coalescing channel may
/// drop intermediates without correctness loss.
///
/// Carries the two clocks of §5.1: `event_time` (venue timestamp, optional) and
/// `received_time` (normalization time, always present).
#[derive(Debug, Clone)]
pub struct QuoteUpdate {
    /// The provider-agnostic identity this quote applies to (issue #4).
    pub instrument: Instrument,
    /// Best bid price, or `None` when the feed omits it (renders as `—`).
    pub bid: Option<Positive>,
    /// Best ask price, or `None` when the feed omits it (renders as `—`).
    pub ask: Option<Positive>,
    /// Last traded price, or `None` when the feed omits it (renders as `—`).
    pub last: Option<Positive>,
    /// Size resting at the best bid, or `None` when the feed omits it.
    pub bid_size: Option<Positive>,
    /// Size resting at the best ask, or `None` when the feed omits it.
    pub ask_size: Option<Positive>,
    /// The venue's exchange timestamp — `Some` only when the feed carries one
    /// (§5.1). `None` means order by `received_time` and never advance the
    /// event-time watermark.
    pub event_time: Option<DateTime<Utc>>,
    /// When ChainView normalized this update — **always present** (§5.1).
    pub received_time: DateTime<Utc>,
}

/// A Greeks/IV refresh for one [`Instrument`] (`docs/01-domain-model.md` §5).
///
/// Sourced from the provider (Deribit ticker, dxfeed `Greeks` event, Alpaca
/// snapshot) or computed locally for providers that lack them (IG) — the
/// [`origin`](GreeksRow::origin) tag records which, so the UI can badge origin
/// honesty. Greeks are [`Decimal`] and IV is [`Positive`]: all **display
/// analytics**, never accounting values. Every numeric field is `Option` — a
/// value the source cannot supply stays `None` and renders as an em dash (`—`),
/// never a fabricated zero. Carries the two clocks of §5.1.
#[derive(Debug, Clone)]
pub struct GreeksRow {
    /// The provider-agnostic identity these analytics apply to (issue #4).
    pub instrument: Instrument,
    /// Implied volatility, or `None` when unavailable (renders as `—`).
    pub iv: Option<Positive>,
    /// Delta, or `None` when unavailable (renders as `—`).
    pub delta: Option<Decimal>,
    /// Gamma, or `None` when unavailable (renders as `—`).
    pub gamma: Option<Decimal>,
    /// Theta, or `None` when unavailable (renders as `—`).
    pub theta: Option<Decimal>,
    /// Vega, or `None` when unavailable (renders as `—`).
    pub vega: Option<Decimal>,
    /// Rho, or `None` when unavailable (renders as `—`).
    pub rho: Option<Decimal>,
    /// Whether these Greeks came from the venue or ChainView's local
    /// computation ([`GreeksOrigin`]) — surfaced in the UI for honesty.
    pub origin: GreeksOrigin,
    /// The venue's exchange timestamp — `Some` only when the feed carries one
    /// (§5.1).
    pub event_time: Option<DateTime<Utc>>,
    /// When ChainView normalized this update — **always present** (§5.1).
    pub received_time: DateTime<Utc>,
}

/// Where a [`GreeksRow`]'s analytics came from (`docs/01-domain-model.md` §5).
///
/// [`GreeksOrigin::ComputedLocally`] is surfaced in the UI (a subtle glyph) so
/// the trader knows the Greeks are ChainView's local Black-Scholes, not the
/// venue's — honesty over polish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GreeksOrigin {
    /// Supplied by the venue feed (Deribit ticker, dxfeed `Greeks`, Alpaca).
    Provider,
    /// Computed locally by ChainView via `optionstratlib` (IG, or any missing
    /// analytic) — badged in the UI.
    ComputedLocally,
}

/// A normalized order-book depth snapshot for one [`Instrument`]
/// (`docs/01-domain-model.md` §5) — a DOMAIN type (a provider emits it via
/// `MarketUpdate::Depth`), never a UI type.
///
/// Only depth-capable providers populate it (Deribit option instruments;
/// Alpaca crypto-spot only, which is outside the v1 option-chain product; IG
/// unverified-pending-fixture — see `docs/03-data-providers.md`). Others never
/// emit it and the depth screen renders its empty state. Levels are best-first.
/// Carries the two clocks of §5.1 plus an optional venue sequence id.
#[derive(Debug, Clone)]
pub struct DepthLadder {
    /// The provider-agnostic identity this book applies to (issue #4).
    pub instrument: Instrument,
    /// Bid levels, **best-first** (highest price at index 0).
    pub bids: Vec<DepthLevel>,
    /// Ask levels, **best-first** (lowest price at index 0).
    pub asks: Vec<DepthLevel>,
    /// The venue's book timestamp — `Some` only when the feed carries one
    /// (§5.1).
    pub event_time: Option<DateTime<Utc>>,
    /// When ChainView normalized this snapshot — **always present** (§5.1).
    pub received_time: DateTime<Utc>,
    /// The upstream sequence / change id for gap detection and resync (Deribit),
    /// or `None` when the feed carries no sequence. Consumed by the depth path
    /// in v0.5; carried now.
    pub change_id: Option<u64>,
}

/// One price/size level in a [`DepthLadder`] (`docs/01-domain-model.md` §5).
///
/// Both fields are `Positive` **display analytics**, never accounting values.
#[derive(Debug, Clone)]
pub struct DepthLevel {
    /// The level's price (non-negative).
    pub price: Positive,
    /// The size resting at that price (non-negative).
    pub size: Positive,
}

// --- The market-update fan-in enum (§5, docs/02 §4) --------------------------

/// A normalized provider update — the payload of `AppEvent::Market`
/// (`docs/02-tui-architecture.md` §4).
///
/// This is a **closed set** matched exhaustively downstream with **no wildcard
/// `_` arm**, so adding a variant later forces every match site to be revisited
/// by the compiler.
///
/// The [`Chain`](MarketUpdate::Chain) and [`Health`](MarketUpdate::Health)
/// variants reference the store types [`ChainSnapshot`] and [`StreamHealth`].
/// [`ChainSnapshot`]'s data shape is complete as of issue #6 (its `aliases` and
/// `source` fields landed with [`AliasCatalog`] and [`ChainSource`]);
/// [`StreamHealth`] remains a thin forward declaration. The store **logic** that
/// produces both — the poll→stream merge and the health machine — lands with the
/// chain store in issue #7.
#[derive(Debug, Clone)]
pub enum MarketUpdate {
    /// A quote refresh for one instrument (§5).
    Quote(QuoteUpdate),
    /// A Greeks/IV refresh for one instrument (§5).
    Greeks(GreeksRow),
    /// An order-book depth snapshot for one instrument — depth-capable
    /// providers only (§5).
    Depth(DepthLadder),
    /// A full (re)poll of a chain for one `(provider, underlying, expiry)`.
    Chain(ChainSnapshot),
    /// A connection-health transition for a provider's stream.
    Health(ProviderId, StreamHealth),
}

// --- Store types (data shape here; the store LOGIC lands in #7) ---------------

/// The streaming-current chain snapshot (`docs/01-domain-model.md` §6), landed
/// here so [`MarketUpdate::Chain`] can be a closed variant.
///
/// Issue #6 completed its `aliases` and `source` fields now that
/// [`AliasCatalog`] and [`ChainSource`] exist. It is assembled from a
/// [`ChainFetch`](super::fetch::ChainFetch), carrying the SAME `AliasCatalog`
/// forward with no copy or re-derivation. The store LOGIC that produces and
/// mutates a `ChainSnapshot` — the poll→stream merge, the watermark/staleness
/// transitions, and the health machine — lands with the `ChainStore` in issue
/// #7, which may relocate this type into the store module.
#[derive(Debug, Clone)]
pub struct ChainSnapshot {
    /// `(source provider, underlying, absolute-UTC expiry)` — the chain's
    /// identity. No relative `Days` offset ever reaches it (issue #15 resolves
    /// expiry at the seam).
    pub chain_key: (ProviderId, String, DateTime<Utc>),
    /// The normalized `optionstratlib` chain — the source of truth.
    pub chain: OptionChain,
    /// The per-leg native+stream alias catalog carried forward from the
    /// [`ChainFetch`](super::fetch::ChainFetch) the poll leg returned — for
    /// subscribe/resubscribe/join (`docs/01-domain-model.md` §6,
    /// `docs/03-data-providers.md` §4).
    pub aliases: AliasCatalog,
    /// How the chain is being kept current ([`ChainSource`]). The store (#7) sets
    /// it from the active merge.
    pub source: ChainSource,
    /// How current the chain is ([`StreamHealth`]).
    pub health: StreamHealth,
    /// The wall-time of the last full poll, or `None` before the first poll.
    pub last_full_poll: Option<DateTime<Utc>>,
}

/// How a [`ChainSnapshot`] is being kept current (`docs/01-domain-model.md` §6).
///
/// A pure data enum here; the store (issue #7) sets it from the active merge.
/// `Merged` means a REST poll seeded the strikes and a stream overlays
/// quotes/Greeks (the tastytrade and Alpaca case,
/// `docs/03-data-providers.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChainSource {
    /// Kept current by REST polling only.
    Poll,
    /// Kept current by a live stream only.
    Stream,
    /// A REST poll seeds the strikes and a stream overlays quotes/Greeks.
    Merged,
}

/// **Thin forward declaration** of the stream connection health
/// (`docs/01-domain-model.md` §6), landed here so [`MarketUpdate::Health`] and
/// [`ChainSnapshot`] can name it. It is a pure data enum (no logic); the store
/// (issue #7) drives the transitions between its variants and the per-component
/// staleness that flips a component to `Stale`.
#[derive(Debug, Clone)]
pub enum StreamHealth {
    /// The stream is connected and fresh.
    Live,
    /// The stream is up but a component has aged past its threshold, silent
    /// since the given instant (§5.1).
    Stale {
        /// When the component was last fresh.
        since: DateTime<Utc>,
    },
    /// The stream dropped and is reconnecting (with the current attempt count).
    Reconnecting {
        /// The current reconnect attempt (1-based).
        attempt: u32,
    },
}

// --- Freshness thresholds (§5.1, §6) -----------------------------------------

/// Quotes older than this (measured from their `received_time`) are badged
/// `stale` — default **5 s** (`docs/01-domain-model.md` §5.1).
pub const QUOTE_STALE_AFTER: Duration = Duration::from_secs(5);

/// Greeks older than this (measured from their `received_time`) are badged
/// `stale` — default **10 s** (`docs/01-domain-model.md` §5.1).
pub const GREEKS_STALE_AFTER: Duration = Duration::from_secs(10);

/// The fixed slack added on top of one `refresh_interval` before a chain's
/// structure is badged `stale`. `docs/01-domain-model.md` §5.1 defines
/// `CHAIN_STALE_AFTER` as *one `refresh_interval` + slack* (a formula, since
/// `refresh_interval` is runtime config); this constant fixes the slack term.
///
/// Default **2 s** — one default-cadence poll of headroom over the `2 s`
/// `DEFAULT_REFRESH` (`src/config.rs`), so a single missed poll plus jitter does
/// not badge a healthy chain stale. Combine it with a refresh interval via
/// [`chain_stale_after`].
pub const CHAIN_STALE_SLACK: Duration = Duration::from_secs(2);

/// A feed delay (`now − event_time`) beyond this badges the component `delayed`
/// (distinct from `stale`): the feed is live but the venue data is lagging —
/// default **2 s** (`docs/01-domain-model.md` §5.1).
pub const FEED_DELAY_WARN: Duration = Duration::from_secs(2);

/// The price-direction indicator (`TickDir`) decays to `Flat` after this long
/// with no further change — default **3 s** (`docs/01-domain-model.md` §6).
pub const DIRECTION_DECAY: Duration = Duration::from_secs(3);

/// The effective chain-staleness threshold for a given `refresh_interval`:
/// *`refresh_interval` + [`CHAIN_STALE_SLACK`]*, the `CHAIN_STALE_AFTER` of
/// `docs/01-domain-model.md` §5.1.
///
/// This is the threshold **definition** only; the store (issue #7) compares a
/// chain's age against it. The addition saturates to [`Duration::MAX`] rather
/// than overflowing (structurally impossible under the config `refresh_interval`
/// ceiling, but total for any input).
#[must_use]
pub fn chain_stale_after(refresh_interval: Duration) -> Duration {
    refresh_interval
        .checked_add(CHAIN_STALE_SLACK)
        .unwrap_or(Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::identity::{
        ContractSpecFingerprint, ExerciseStyle, InstrumentKey, SettlementStyle,
    };
    use optionstratlib::OptionStyle;

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
                underlying: "BTC".to_owned(),
                expiration_utc: utc(1_700_000_000),
                strike: pos(60_000.0),
                style: OptionStyle::Call,
            },
            provider: pid("deribit"),
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

    fn absent_quote() -> QuoteUpdate {
        QuoteUpdate {
            instrument: sample_instrument(),
            bid: None,
            ask: None,
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: None,
            received_time: utc(1_700_000_100),
        }
    }

    fn absent_greeks() -> GreeksRow {
        GreeksRow {
            instrument: sample_instrument(),
            iv: None,
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
            rho: None,
            origin: GreeksOrigin::Provider,
            event_time: None,
            received_time: utc(1_700_000_100),
        }
    }

    // --- Missing numeric fields stay `None` ----------------------------------

    #[test]
    fn test_quote_update_absent_fields_stay_none() {
        let q = absent_quote();
        assert!(q.bid.is_none());
        assert!(q.ask.is_none());
        assert!(q.last.is_none());
        assert!(q.bid_size.is_none());
        assert!(q.ask_size.is_none());
        assert!(q.event_time.is_none());
        // received_time is always present.
        assert_eq!(q.received_time, utc(1_700_000_100));
    }

    #[test]
    fn test_quote_update_present_fields_carry_values() {
        let q = QuoteUpdate {
            bid: Some(pos(1.5)),
            ask: Some(pos(1.7)),
            event_time: Some(utc(1_700_000_099)),
            ..absent_quote()
        };
        assert_eq!(q.bid, Some(pos(1.5)));
        assert_eq!(q.ask, Some(pos(1.7)));
        assert_eq!(q.event_time, Some(utc(1_700_000_099)));
    }

    #[test]
    fn test_greeks_row_absent_greeks_stay_none() {
        let g = absent_greeks();
        assert!(g.iv.is_none());
        assert!(g.delta.is_none());
        assert!(g.gamma.is_none());
        assert!(g.theta.is_none());
        assert!(g.vega.is_none());
        assert!(g.rho.is_none());
        assert!(g.event_time.is_none());
        assert_eq!(g.received_time, utc(1_700_000_100));
    }

    // --- GreeksOrigin ---------------------------------------------------------

    #[test]
    fn test_greeks_origin_computed_locally_is_representable() {
        let g = GreeksRow {
            origin: GreeksOrigin::ComputedLocally,
            ..absent_greeks()
        };
        assert_eq!(g.origin, GreeksOrigin::ComputedLocally);
    }

    #[test]
    fn test_greeks_origin_provider_ne_computed_locally() {
        assert_ne!(GreeksOrigin::Provider, GreeksOrigin::ComputedLocally);
    }

    // --- DepthLadder ----------------------------------------------------------

    #[test]
    fn test_depth_ladder_change_id_carried_when_present() {
        let ladder = DepthLadder {
            instrument: sample_instrument(),
            bids: vec![DepthLevel {
                price: pos(60_000.0),
                size: pos(2.0),
            }],
            asks: vec![DepthLevel {
                price: pos(60_010.0),
                size: pos(1.0),
            }],
            event_time: Some(utc(1_700_000_099)),
            received_time: utc(1_700_000_100),
            change_id: Some(42),
        };
        assert_eq!(ladder.change_id, Some(42u64));
        // Levels are best-first: the sole bid/ask sit at index 0.
        match (ladder.bids.first(), ladder.asks.first()) {
            (Some(bid), Some(ask)) => {
                assert_eq!(bid.price, pos(60_000.0));
                assert_eq!(ask.price, pos(60_010.0));
            }
            _ => panic!("expected one bid and one ask level"),
        }
    }

    #[test]
    fn test_depth_ladder_change_id_none_when_absent() {
        let ladder = DepthLadder {
            instrument: sample_instrument(),
            bids: Vec::new(),
            asks: Vec::new(),
            event_time: None,
            received_time: utc(1_700_000_100),
            change_id: None,
        };
        assert!(ladder.change_id.is_none());
    }

    // --- MarketUpdate variant construction (one per variant) -----------------

    #[test]
    fn test_market_update_quote_variant_constructs() {
        let update = MarketUpdate::Quote(absent_quote());
        match update {
            MarketUpdate::Quote(q) => assert!(q.bid.is_none()),
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn test_market_update_greeks_variant_constructs() {
        // -0.25 as a Decimal (mantissa -25, scale 2) — the `dec!` macro is
        // unavailable without a direct `rust_decimal` dependency, so build it
        // from the type `optionstratlib` re-exports.
        let delta = Decimal::new(-25, 2);
        let update = MarketUpdate::Greeks(GreeksRow {
            delta: Some(delta),
            origin: GreeksOrigin::ComputedLocally,
            ..absent_greeks()
        });
        match update {
            MarketUpdate::Greeks(g) => {
                assert_eq!(g.delta, Some(delta));
                assert_eq!(g.origin, GreeksOrigin::ComputedLocally);
            }
            other => panic!("expected Greeks, got {other:?}"),
        }
    }

    #[test]
    fn test_market_update_depth_variant_constructs() {
        let update = MarketUpdate::Depth(DepthLadder {
            instrument: sample_instrument(),
            bids: Vec::new(),
            asks: Vec::new(),
            event_time: None,
            received_time: utc(1_700_000_100),
            change_id: Some(7),
        });
        match update {
            MarketUpdate::Depth(d) => assert_eq!(d.change_id, Some(7u64)),
            other => panic!("expected Depth, got {other:?}"),
        }
    }

    #[test]
    fn test_market_update_chain_variant_constructs() {
        let snapshot = ChainSnapshot {
            chain_key: (pid("deribit"), "BTC".to_owned(), utc(1_700_000_000)),
            chain: OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None),
            aliases: AliasCatalog::new(),
            source: ChainSource::Poll,
            health: StreamHealth::Live,
            last_full_poll: Some(utc(1_700_000_100)),
        };
        let update = MarketUpdate::Chain(snapshot);
        match update {
            MarketUpdate::Chain(c) => {
                assert_eq!(c.chain.symbol, "BTC");
                assert_eq!(c.chain_key.1, "BTC");
                assert_eq!(c.source, ChainSource::Poll);
                assert!(c.aliases.is_empty());
            }
            other => panic!("expected Chain, got {other:?}"),
        }
    }

    #[test]
    fn test_market_update_health_variant_constructs() {
        let update =
            MarketUpdate::Health(pid("deribit"), StreamHealth::Reconnecting { attempt: 3 });
        match update {
            MarketUpdate::Health(provider, StreamHealth::Reconnecting { attempt }) => {
                assert_eq!(provider.as_str(), "deribit");
                assert_eq!(attempt, 3);
            }
            other => panic!("expected Health(_, Reconnecting), got {other:?}"),
        }
    }

    // --- StreamHealth forward declaration ------------------------------------

    #[test]
    fn test_stream_health_stale_carries_since_instant() {
        let health = StreamHealth::Stale {
            since: utc(1_700_000_050),
        };
        match health {
            StreamHealth::Stale { since } => assert_eq!(since, utc(1_700_000_050)),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    // --- Freshness thresholds equal the documented defaults ------------------

    #[test]
    fn test_quote_stale_after_equals_five_seconds() {
        assert_eq!(QUOTE_STALE_AFTER, Duration::from_secs(5));
    }

    #[test]
    fn test_greeks_stale_after_equals_ten_seconds() {
        assert_eq!(GREEKS_STALE_AFTER, Duration::from_secs(10));
    }

    #[test]
    fn test_chain_stale_slack_equals_two_seconds() {
        assert_eq!(CHAIN_STALE_SLACK, Duration::from_secs(2));
    }

    #[test]
    fn test_feed_delay_warn_equals_two_seconds() {
        assert_eq!(FEED_DELAY_WARN, Duration::from_secs(2));
    }

    #[test]
    fn test_direction_decay_equals_three_seconds() {
        assert_eq!(DIRECTION_DECAY, Duration::from_secs(3));
    }

    #[test]
    fn test_chain_stale_after_adds_slack_to_refresh() {
        let refresh = Duration::from_secs(2);
        assert_eq!(chain_stale_after(refresh), refresh + CHAIN_STALE_SLACK);
    }

    #[test]
    fn test_chain_stale_after_saturates_on_overflow() {
        assert_eq!(chain_stale_after(Duration::MAX), Duration::MAX);
    }
}
