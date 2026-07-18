//! FAIL: an external crate CANNOT match `GreeksCapability` exhaustively without a
//! wildcard arm — the enum is `#[non_exhaustive]`, so a `_` arm is mandatory. If
//! this ever compiles, `GreeksCapability` has lost `#[non_exhaustive]` and adding
//! a variant silently becomes a MAJOR break
//! (docs/SEMVER.md#provider-port-versioning).

use chainview::GreeksCapability;

fn describe(greeks: GreeksCapability) -> &'static str {
    match greeks {
        GreeksCapability::Provided => "provided",
        GreeksCapability::ComputedLocally => "computed-locally",
        GreeksCapability::None => "none",
    }
}

fn main() {
    let _ = describe(GreeksCapability::None);
}
