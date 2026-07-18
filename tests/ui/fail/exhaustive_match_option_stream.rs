//! FAIL: an external crate CANNOT match `OptionStreamCapability` exhaustively
//! without a wildcard arm — the enum is `#[non_exhaustive]`, so a `_` arm is
//! mandatory even though every current variant (including the struct variants) is
//! bound here. If this ever compiles, `OptionStreamCapability` has lost
//! `#[non_exhaustive]` and adding a variant silently becomes a MAJOR break
//! (docs/SEMVER.md#provider-port-versioning).

use chainview::OptionStreamCapability;

fn describe(stream: OptionStreamCapability) -> &'static str {
    match stream {
        OptionStreamCapability::None => "none",
        OptionStreamCapability::SymbolOnly { verified: _ } => "symbol-only",
        OptionStreamCapability::ChainQuotes { verified: _ } => "chain-quotes",
    }
}

fn main() {
    let _ = describe(OptionStreamCapability::None);
}
