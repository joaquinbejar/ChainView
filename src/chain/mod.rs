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

mod events;
mod fetch;
mod identity;

pub use events::{
    CHAIN_STALE_SLACK, ChainSnapshot, ChainSource, DIRECTION_DECAY, DepthLadder, DepthLevel,
    FEED_DELAY_WARN, GREEKS_STALE_AFTER, GreeksOrigin, GreeksRow, MarketUpdate, QUOTE_STALE_AFTER,
    QuoteUpdate, StreamHealth, chain_stale_after,
};
pub use fetch::{AliasCatalog, ChainFetch, ExpirySource};
pub use identity::{
    ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId,
    RESERVED_PROVIDER_IDS, SettlementStyle,
};
