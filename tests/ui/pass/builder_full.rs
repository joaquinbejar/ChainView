//! PASS: an external-shaped adapter builds `ProviderCapabilities` through the
//! only cross-crate construction path — `ProviderCapabilities::builder()` —
//! setting every dimension from the public `chainview::` surface alone. This is
//! the source-compatible construction the SemVer minor rule rests on
//! (docs/SEMVER.md#provider-port-versioning, docs/03-data-providers.md §2).

use chainview::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
    ProviderCapabilities,
};

fn main() {
    let caps: ProviderCapabilities = ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        .depth(true)
        .greeks(GreeksCapability::Provided)
        .option_stream(OptionStreamCapability::ChainQuotes { verified: true })
        .underlying_stream(true)
        .chain_poll(ChainPollCapability::Poll {
            interval_hint_secs: 2,
        })
        .trades_tape(true)
        .auth(AuthKind::KeySecret)
        .build();

    // Reading a public field is fine — `#[non_exhaustive]` gates *construction*
    // by struct literal, not field access.
    assert!(caps.depth);
    assert_eq!(caps.chain, ChainCapability::Assemble);
}
