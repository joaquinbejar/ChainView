//! FAIL: an external crate CANNOT match `ChainCapability` exhaustively without a
//! wildcard arm — the enum is `#[non_exhaustive]`, so a `_` arm is mandatory.
//! This is the forward-compatibility guarantee: a future added variant cannot
//! break a downstream match because the compiler already forces the wildcard. If
//! this ever compiles, `ChainCapability` has lost `#[non_exhaustive]` and adding
//! a variant silently becomes a MAJOR break
//! (docs/SEMVER.md#provider-port-versioning).

use chainview::ChainCapability;

fn describe(chain: ChainCapability) -> &'static str {
    match chain {
        ChainCapability::Native => "native",
        ChainCapability::Assemble => "assemble",
        ChainCapability::Partial => "partial",
        ChainCapability::None => "none",
    }
}

fn main() {
    let _ = describe(ChainCapability::None);
}
