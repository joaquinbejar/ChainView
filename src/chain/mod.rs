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

mod identity;

pub use identity::{
    ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId,
    RESERVED_PROVIDER_IDS, SettlementStyle,
};
