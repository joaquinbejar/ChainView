//! FAIL: an external crate CANNOT match `ChainPollCapability` exhaustively
//! without a wildcard arm — the enum is `#[non_exhaustive]`, so a `_` arm is
//! mandatory even with the struct variant bound here. If this ever compiles,
//! `ChainPollCapability` has lost `#[non_exhaustive]` and adding a variant
//! silently becomes a MAJOR break (docs/SEMVER.md#provider-port-versioning).

use chainview::ChainPollCapability;

fn describe(poll: ChainPollCapability) -> &'static str {
    match poll {
        ChainPollCapability::None => "none",
        ChainPollCapability::Poll {
            interval_hint_secs: _,
        } => "poll",
    }
}

fn main() {
    let _ = describe(ChainPollCapability::None);
}
