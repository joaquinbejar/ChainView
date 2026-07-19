//! PASS: constructing a capability-enum variant from an external crate always
//! compiles — `#[non_exhaustive]` on an enum gates *exhaustive matching* (and a
//! struct literal of the struct), never construction of an existing variant,
//! including the struct variants. A future added variant is therefore a
//! source-compatible minor addition an adapter may set
//! (docs/03-data-providers.md §2).

use chainview::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
};

fn main() {
    let _ = ChainCapability::Native;
    let _ = ChainCapability::Assemble;
    let _ = ChainCapability::Partial;
    let _ = ChainCapability::None;

    let _ = GreeksCapability::Provided;
    let _ = GreeksCapability::ComputedLocally;
    let _ = GreeksCapability::None;

    let _ = OptionStreamCapability::None;
    let _ = OptionStreamCapability::SymbolOnly { verified: false };
    let _ = OptionStreamCapability::ChainQuotes { verified: true };

    let _ = ChainPollCapability::None;
    let _ = ChainPollCapability::Poll {
        interval_hint_secs: 5,
    };

    let _ = AuthKind::None;
    let _ = AuthKind::Token;
    let _ = AuthKind::KeySecret;
    let _ = AuthKind::UserPass;
}
