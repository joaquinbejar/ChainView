//! PASS: the least-capable default construction — every dimension left at its
//! safe default via `ProviderCapabilities::builder().build()`. This mirrors the
//! #43 crate-root doctest's `capabilities()` body, and is the construction that
//! a future added optional dimension (a new field with a safe default) leaves
//! compiling unchanged (docs/SEMVER.md#provider-port-versioning).

use chainview::ProviderCapabilities;

fn main() {
    let caps = ProviderCapabilities::builder().build();

    // The zero-config default is depth-less and unauthenticated.
    assert!(!caps.depth);
}
