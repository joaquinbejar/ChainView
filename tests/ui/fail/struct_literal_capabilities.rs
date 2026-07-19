//! FAIL (load-bearing): an external crate CANNOT construct `ProviderCapabilities`
//! with a struct literal — it is `#[non_exhaustive]`, so the only cross-crate
//! construction path is `ProviderCapabilities::builder()`. If this ever compiles,
//! adding a field silently becomes a MAJOR break; this fixture fails the build
//! the day `#[non_exhaustive]` is dropped from the struct
//! (docs/SEMVER.md#provider-port-versioning, docs/03-data-providers.md §2).

use chainview::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
    ProviderCapabilities,
};

fn main() {
    let _caps = ProviderCapabilities {
        chain: ChainCapability::Assemble,
        depth: true,
        greeks: GreeksCapability::Provided,
        option_stream: OptionStreamCapability::None,
        underlying_stream: false,
        chain_poll: ChainPollCapability::None,
        trades_tape: false,
        auth: AuthKind::None,
    };
}
