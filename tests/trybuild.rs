//! The `capabilities_source_compat` compile-gate (issue #44,
//! `docs/TESTING.md` §3, `docs/SEMVER.md` provider-port versioning).
//!
//! # What this proves
//!
//! The provider-port SemVer promise — "adding an **optional** capability
//! dimension is a **minor**, source-compatible bump" — is only true because
//! [`ProviderCapabilities`](chainview::ProviderCapabilities) and every
//! capability enum ([`ChainCapability`](chainview::ChainCapability) /
//! [`GreeksCapability`](chainview::GreeksCapability) /
//! [`OptionStreamCapability`](chainview::OptionStreamCapability) /
//! [`ChainPollCapability`](chainview::ChainPollCapability) /
//! [`AuthKind`](chainview::AuthKind)) are `#[non_exhaustive]` and an
//! out-of-crate adapter constructs capabilities only through
//! [`ProviderCapabilities::builder`](chainview::ProviderCapabilities::builder),
//! never a struct literal. This gate turns that architectural fact into a
//! build-breaking test:
//!
//! - the **pass** fixtures (`tests/ui/pass/*.rs`) build capabilities through the
//!   builder and read the enums with the mandatory wildcard arm — the exact
//!   shape an external adapter uses — and must compile and run;
//! - the **fail** fixtures (`tests/ui/fail/*.rs`) are the load-bearing half: a
//!   downstream struct literal for `ProviderCapabilities` (E0639) and an
//!   exhaustive match without a wildcard on each capability enum (E0004) must be
//!   **rejected**. Each committed `.stderr` is a compile golden. The day a
//!   capability type loses `#[non_exhaustive]` (or gains a public struct-literal
//!   path), its fail fixture starts compiling, trybuild reports "expected
//!   failure but succeeded", and the build breaks — catching the exact edit that
//!   would silently reclassify a minor change as a major break.
//!
//! Each fixture names every port type through `chainview::` alone, so trybuild
//! compiles it as a separate crate against the built lib — the external-adapter
//! story exactly (`docs/03-data-providers.md` §11.1).
//!
//! # Why it is toolchain-gated (and how to regenerate the goldens)
//!
//! A `.stderr` golden is byte-sensitive to the exact `rustc` version, so this
//! gate is **EXECUTED only on the pinned MSRV toolchain (Rust 1.88)** — the one
//! toolchain that never drifts — where the committed goldens are stable. On any
//! other toolchain (a moving `stable`, a newer local default) the test **skips**
//! so the four non-negotiable commands stay green regardless of the local
//! `rustc`. The load-bearing execution is the CI `msrv` job, which sets
//! `CHAINVIEW_TRYBUILD_UI=1`.
//!
//! To (re)generate the committed `.stderr` after an intentional message change,
//! run on the pinned toolchain:
//!
//! ```text
//! CHAINVIEW_TRYBUILD_UI=1 TRYBUILD=overwrite cargo +1.88 test --test trybuild
//! ```
//!
//! then re-run without `TRYBUILD=overwrite` to confirm the goldens are stable.
//! Committing the regenerated goldens makes a compiler-message drift a
//! deliberate, reviewed change rather than a silent pass.

/// The environment gate that scopes the rustc-version-sensitive fixtures to the
/// pinned MSRV toolchain (Rust 1.88). CI's `msrv` job sets it; every other
/// toolchain skips (see the module docs for the regeneration flow).
const TRYBUILD_GATE_ENV: &str = "CHAINVIEW_TRYBUILD_UI";

/// The `capabilities_source_compat` source-compat gate (`docs/TESTING.md` §3):
/// the builder path compiles, every `#[non_exhaustive]` struct-literal /
/// exhaustive-match path is rejected with its committed `.stderr`.
#[test]
fn capabilities_source_compat() {
    // Skip on any toolchain but the pinned 1.88, where the committed goldens are
    // byte-stable. A drifting `stable` would otherwise fail on a benign compiler
    // message change unrelated to the port.
    if std::env::var_os(TRYBUILD_GATE_ENV).is_none() {
        return;
    }

    let t = trybuild::TestCases::new();
    // The builder path (and the wildcard-arm read) an external adapter uses.
    t.pass("tests/ui/pass/*.rs");
    // The `#[non_exhaustive]`-rejected shapes, each with a committed `.stderr`.
    t.compile_fail("tests/ui/fail/*.rs");
}
