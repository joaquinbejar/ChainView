//! The normalized option-chain domain.
//!
//! Will define the `ChainStore` (poll → stream merge over the
//! `optionstratlib` chain model) and the normalized streaming update events.
//! Lands with issues #5–#7.
//!
//! Issue #4 landed the provider-agnostic instrument identity in the
//! [`identity`] submodule — [`InstrumentKey`], [`Instrument`],
//! [`ContractSpecFingerprint`] and its style enums, and the open, validated
//! [`ProviderId`] newtype with [`RESERVED_PROVIDER_IDS`]
//! (`docs/01-domain-model.md` §4).
//!
//! Issue #5 landed the normalized streaming update events in the [`events`]
//! submodule — [`QuoteUpdate`], [`GreeksRow`], [`DepthLadder`]/[`DepthLevel`],
//! the [`GreeksOrigin`] tag, the closed [`MarketUpdate`] fan-in enum, forward
//! declarations of the store types [`ChainSnapshot`]/[`StreamHealth`], and the
//! freshness threshold constants (`docs/01-domain-model.md` §5 and §5.1).
//!
//! Issue #6 landed the named fetch artifact in the [`fetch`] submodule —
//! [`ChainFetch`], [`ExpirySource`], and the per-leg [`AliasCatalog`] (with the
//! `overlay_compatible` fingerprint gate) — the DOMAIN types the provider port's
//! `fetch_chain` emits and the future `ChainStore` consumes, re-exported through
//! the port surface (`docs/01-domain-model.md` §6,
//! `docs/03-data-providers.md` §2, §11.1). It also completed [`ChainSnapshot`]'s
//! `aliases`/`source` fields now that [`AliasCatalog`] and [`ChainSource`] exist
//! (the store LOGIC that drives them lands in #7).
//!
//! Issue #24 landed the local Greeks/IV fill-in engine in the [`greeks`]
//! submodule — the style-keyed [`GreeksSidecar`]/[`LegGreeks`], the fully-sourced
//! [`PricingInputs`] ([`PricingModel`]/[`QuoteSelect`]), and [`compute_leg_greeks`]:
//! it builds an `optionstratlib::Options` and calls the `optionstratlib` Greeks
//! functions plus IV inversion (never a hand-rolled Black-Scholes/root-finder),
//! deterministically and cached by `input_generation`, clearing a crossed /
//! stale-quote / stale / solver-failed leg to `None` with a recorded
//! [`LegStatus`]. Per-leg stream-quote freshness reaches the kernel as pure data
//! ([`QuoteClocks`]) so a silent feed's last quote is never inverted into a
//! spuriously-`Computed` IV (`docs/01-domain-model.md` §7, §5.1).
//!
//! Issue #7 landed the live [`ChainStore`] in the [`store`] submodule — the
//! deterministic poll -> stream merge over the `optionstratlib` chain: the
//! strike-keyed clone/patch/re-insert row update, the field-fold rules
//! (crossed/zero), the bounded-generation merge with tombstones and the
//! [`MAX_PENDING`]/[`pending_ttl`] pending-unknown-strike buffer, the two-clock
//! freshness/watermark model ([`Freshness`]), the retained/decayed price
//! direction ([`TickDir`]), the [`MergeOutcome`] of each fold, and the wired
//! cross-provider overlay gate (`docs/01-domain-model.md` §5.1, §6,
//! `docs/03-data-providers.md` §3, §4).

mod events;
mod fetch;
mod greeks;
mod identity;
mod store;

pub use events::{
    CHAIN_STALE_SLACK, ChainSnapshot, ChainSource, DIRECTION_DECAY, DepthLadder, DepthLevel,
    FEED_DELAY_WARN, GREEKS_STALE_AFTER, GreeksOrigin, GreeksRow, MarketUpdate, QUOTE_STALE_AFTER,
    QuoteUpdate, StreamHealth, chain_stale_after,
};
pub use fetch::{AliasCatalog, ChainFetch, ExpirySource};
pub use greeks::{
    DEFAULT_DIVIDEND_YIELD, DEFAULT_RISK_FREE_RATE, GreeksSidecar, LegGreeks, LegStatus,
    PricingInputs, PricingModel, QuoteClocks, QuoteSelect, compute_leg_greeks,
};
pub use identity::{
    ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId,
    RESERVED_PROVIDER_IDS, SettlementStyle,
};
pub use store::{ChainStore, Freshness, MAX_PENDING, MergeOutcome, TickDir, pending_ttl};
