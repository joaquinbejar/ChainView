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
//! the [`GreeksOrigin`] tag, the closed [`MarketUpdate`] fan-in enum, thin
//! forward declarations of the store types [`ChainSnapshot`]/[`StreamHealth`]
//! (completed with the store in #6/#7), and the freshness threshold constants
//! (`docs/01-domain-model.md` §5 and §5.1).

mod events;
mod identity;

pub use events::{
    CHAIN_STALE_SLACK, ChainSnapshot, DIRECTION_DECAY, DepthLadder, DepthLevel, FEED_DELAY_WARN,
    GREEKS_STALE_AFTER, GreeksOrigin, GreeksRow, MarketUpdate, QUOTE_STALE_AFTER, QuoteUpdate,
    StreamHealth, chain_stale_after,
};
pub use identity::{
    ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId,
    RESERVED_PROVIDER_IDS, SettlementStyle,
};
