//! Fuzz-only harness surface for the two v1.0 parser-surface fuzz targets
//! (`fuzz/fuzz_targets/*`, issue #53, `docs/TESTING.md` §13.4,
//! `docs/SECURITY.md` §7), compiled **only** under the `fuzz` Cargo feature.
//!
//! # Why this exists
//!
//! A `fuzz/fuzz_targets/*.rs` file is a **separate crate** that links `chainview`
//! and sees only its **public** API. One of the two parser surfaces the security
//! gate fuzzes — the provider payload normalizer — is not on that surface: the
//! Deribit `ticker.`/`book.`/instrument-name normalize seam is `pub(crate)`
//! (raw upstream DTOs stop at `src/providers/*`, `CLAUDE.md` "Module
//! Boundaries"). This module exposes exactly the byte-in entry point the
//! `fuzz_provider_normalize` target needs, driving the REAL adapter seam with no
//! socket and no wall clock. The other surface — the replay manifest/Parquet
//! decode — is already public ([`crate::BundleReader`]), so the
//! `fuzz_replay_decode` target drives it directly and needs nothing here.
//!
//! # It is NOT the public API
//!
//! The whole module is `#[cfg(feature = "fuzz")]`, OFF by default. A normal build
//! never compiles it, so nothing here appears on the semver-governed surface (the
//! same discipline as [`crate::bench_support`]). The entry point takes arbitrary
//! bytes and asserts the normalize seam's invariants — typed reject or valid
//! domain row, never a panic, never a `NaN`/`Inf`/negative reaching a domain
//! value (governance item 2). A broken invariant panics; that is the implicit
//! libfuzzer contract the fuzz targets rely on.

use crate::providers::deribit;

/// The number of Deribit normalize seams `provider_normalize` fans across — the
/// modulus applied to the input's leading selector byte.
const NORMALIZE_SEAMS: u8 = 3;

/// Drive one of the Deribit payload normalizers with arbitrary bytes (the
/// `fuzz_provider_normalize` target, issue #53).
///
/// The input's **first byte** selects the seam (`% NORMALIZE_SEAMS`): `0` the
/// `ticker.` -> [`QuoteUpdate`](crate::QuoteUpdate)/[`GreeksRow`](crate::GreeksRow)
/// path, `1` the grouped `book.` -> [`DepthLadder`](crate::DepthLadder) path, `2`
/// the `instrument_name` -> [`InstrumentKey`](crate::InstrumentKey) parser.
/// The remaining bytes are the fuzzed payload fed through the REAL adapter seam.
/// Empty input is a no-op. Deribit is the only shippable, default-compiled
/// built-in with a JSON→domain byte-parser seam; the gated adapters
/// (tastytrade/alpaca/dxlink) extend this target when their features are enabled.
///
/// The seam either rejects the bytes (typed error / a dropped field) or produces
/// a valid domain row — never a panic. A produced value that violates a seam
/// invariant panics, which is how the fuzzer records a finding.
pub fn provider_normalize(data: &[u8]) {
    let Some((selector, payload)) = data.split_first() else {
        return;
    };
    match selector % NORMALIZE_SEAMS {
        0 => deribit::fuzz_normalize_ticker(payload),
        1 => deribit::fuzz_normalize_book(payload),
        // `NORMALIZE_SEAMS == 3`, so the only remaining residue is `2`.
        _ => deribit::fuzz_instrument_key_from_name(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Committed provider fixtures (the same shapes the fuzz seed corpus is
    // derived from) — feeding them through the harness proves it is wired and
    // non-vacuous under `cargo test --all-features`, without the fuzz binaries.
    const TICKER_NORMAL: &[u8] =
        include_bytes!("../tests/fixtures/deribit/ticker/ticker_normal.json");
    const TICKER_NON_FINITE: &[u8] =
        include_bytes!("../tests/fixtures/deribit/ticker/ticker_non_finite.json");
    const TICKER_CROSSED: &[u8] =
        include_bytes!("../tests/fixtures/deribit/ticker/ticker_crossed.json");
    const BOOK_GROUPED: &[u8] =
        include_bytes!("../tests/fixtures/deribit/book/book_grouped_snapshot.json");

    /// A seam-selector byte prepended to a payload, as `provider_normalize`
    /// expects it (and as the fuzz seed corpus is generated).
    fn with_selector(seam: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(payload.len().saturating_add(1));
        v.push(seam);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn test_provider_normalize_survives_the_committed_fixtures() {
        // Every recorded fixture, on every seam, is a normal reject or a valid
        // row — never a panic. (The non-finite / crossed tickers exercise the
        // f64-seam guards specifically.)
        for seam in 0..=(NORMALIZE_SEAMS + 1) {
            for payload in [
                TICKER_NORMAL,
                TICKER_NON_FINITE,
                TICKER_CROSSED,
                BOOK_GROUPED,
            ] {
                provider_normalize(&with_selector(seam, payload));
            }
        }
    }

    #[test]
    fn test_provider_normalize_survives_degenerate_inputs() {
        // Empty, selector-only, and short junk inputs are all no-panic no-ops or
        // typed rejects.
        provider_normalize(&[]);
        provider_normalize(&[0]);
        provider_normalize(&[1]);
        provider_normalize(&[2]);
        provider_normalize(&with_selector(0, b"not json"));
        provider_normalize(&with_selector(1, b"\xff\x00\xfe"));
        provider_normalize(&with_selector(2, b"BTC-27JUN25-60000-C"));
        provider_normalize(&with_selector(2, b"garbage-name"));
    }
}
