//! PASS + minor-bump simulation: an external adapter that (a) builds through the
//! builder and (b) reads a capability enum with the mandatory wildcard `_` arm
//! keeps compiling when ChainView adds a new optional dimension — a builder
//! field with a safe default, or a new enum variant. The wildcard arm, forced by
//! `#[non_exhaustive]`, is exactly what absorbs a future variant, so the add is a
//! source-compatible MINOR bump (docs/SEMVER.md#provider-port-versioning). This
//! fixture is the compile-time counterpart of the negative
//! `fail/exhaustive_match_chain.rs`: with the wildcard it compiles; without it,
//! it would not.

use chainview::{ChainCapability, ProviderCapabilities};

/// A downstream capability read that is forward-compatible with a future added
/// `ChainCapability` variant because of the wildcard arm.
fn describe(chain: ChainCapability) -> &'static str {
    match chain {
        ChainCapability::Native => "native",
        ChainCapability::Assemble => "assemble",
        ChainCapability::Partial => "partial",
        ChainCapability::None => "none",
        // Mandatory for a `#[non_exhaustive]` enum out-of-crate — and the reason
        // a future variant is not a breaking change for this adapter.
        _ => "unknown-future-dimension",
    }
}

fn main() {
    let caps = ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        .build();

    assert_eq!(describe(caps.chain), "assemble");
}
