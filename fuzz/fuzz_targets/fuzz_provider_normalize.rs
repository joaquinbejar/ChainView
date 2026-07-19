//! `fuzz_provider_normalize` (issue #53, docs/TESTING.md §13.4) — arbitrary
//! bytes into the provider payload normalizer.
//!
//! The input's leading byte selects the Deribit seam (ticker / book /
//! instrument-name) and the rest is the fuzzed payload; the harness in
//! `chainview::fuzz_support` drives the REAL normalize seam. The invariant is a
//! typed `ProviderError` / dropped field or a valid `OptionChain` row /
//! `QuoteUpdate` / `GreeksRow` / `DepthLadder`, never a panic and never a
//! NaN/Inf reaching a domain value. A finding is a panic libfuzzer records.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    chainview::fuzz_support::provider_normalize(data);
});
