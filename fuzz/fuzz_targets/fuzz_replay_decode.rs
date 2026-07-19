//! `fuzz_replay_decode` (issue #53, docs/TESTING.md §13.4) — arbitrary bytes
//! into the replay manifest/Parquet reader.
//!
//! The input's leading byte selects which of the five bundle members
//! (`manifest.json` + the four Parquet tables) the fuzzer overrides; the rest is
//! written as that member while the other four stay the committed VALID
//! fixtures, so `BundleReader::open` reaches the bounded, batched decode even
//! when a single member is corrupted. The reader is the public, unchanged API
//! (docs/04-replay-mode.md §3): every outcome is `Ok(LoadedBundle)` or a typed
//! `BundleError`, never a panic that ESCAPES `load()`, and never an allocation
//! past `MAX_WORKING_SET` (the libfuzzer `-rss_limit_mb` backstop above the
//! reader's own ceiling catches a runaway).
//!
//! ## Why this target installs a non-aborting panic hook
//!
//! The reader's contract is that a panic never escapes `load()` — and it upholds
//! that by wrapping the upstream Parquet/Arrow decode in
//! `std::panic::catch_unwind`, mapping a decoder unwind to a typed
//! `BundleError::Parquet` (a real, fuzz-found upstream `arrow-ipc` panic on a
//! malformed embedded `ARROW:schema`, fixed in `scan_table`). But
//! `libfuzzer-sys` installs a panic hook that ABORTS on *any* panic, and a panic
//! hook runs DURING unwinding, before an inner `catch_unwind` can catch it — so
//! the default hook would turn every correctly-CONTAINED decoder panic into a
//! false "deadly signal" crash, flagging inputs the production reader handles.
//!
//! So this target replaces that hook with a silent, non-aborting one. This does
//! NOT weaken the fuzzer for OUR code: a panic that genuinely ESCAPES `load()`
//! (a real contract violation) still unwinds to `libfuzzer-sys`'s own
//! harness-boundary `catch_unwind`, which reports and aborts it as a crash. Only
//! the eager abort-before-catch is removed, letting the production
//! `catch_unwind` do exactly what it does in the shipped binary. The sibling
//! `fuzz_provider_normalize` keeps the default aborting hook on purpose — the
//! normalize seam has no `catch_unwind`, so a panic there IS a finding.

#![no_main]

use std::io::Write;
use std::path::Path;
use std::sync::{Once, OnceLock};

use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

/// Install a silent, non-aborting panic hook exactly once, replacing the
/// `libfuzzer-sys` hook that would abort before the reader's `catch_unwind`
/// runs. See the module docs for why this keeps genuine escaping panics as
/// crashes while letting a contained decoder panic map to a typed error.
static HOOK: Once = Once::new();

fn install_non_aborting_hook() {
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|_info| {
            // Silent on purpose: a contained decoder panic fires thousands of
            // times per second: printing would flood the CI log. A genuine
            // escape is still reported by libfuzzer-sys's boundary catch_unwind
            // (and captured by the libFuzzer/ASan signal handler).
        }));
    });
}

// The five members of a committed VALID bundle (tests/fixtures/bundle/valid),
// baked in so the seed shapes are real and the harness needs no fixtures at run
// time. Relative to fuzz/fuzz_targets/, `../../` is the repo root.
const MANIFEST: &[u8] = include_bytes!("../../tests/fixtures/bundle/valid/manifest.json");
const FILLS: &[u8] = include_bytes!("../../tests/fixtures/bundle/valid/fills.parquet");
const EQUITY: &[u8] = include_bytes!("../../tests/fixtures/bundle/valid/equity_curve.parquet");
const POSITIONS: &[u8] = include_bytes!("../../tests/fixtures/bundle/valid/positions.parquet");
const GREEKS: &[u8] =
    include_bytes!("../../tests/fixtures/bundle/valid/greeks_attribution.parquet");

/// The bundle member file names, in the order the reader stats them.
const MEMBERS: [&str; 5] = [
    "manifest.json",
    "fills.parquet",
    "equity_curve.parquet",
    "positions.parquet",
    "greeks_attribution.parquet",
];

/// The valid bytes for each member (parallel to [`MEMBERS`]).
const VALID: [&[u8]; 5] = [MANIFEST, FILLS, EQUITY, POSITIONS, GREEKS];

/// One reused scratch directory for the whole run — files are overwritten each
/// iteration, so the per-input cost is five writes plus the decode, not a fresh
/// tempdir. Cleaned up when the fuzzer process exits.
static SCRATCH: OnceLock<TempDir> = OnceLock::new();

/// Write `bytes` to `<dir>/<name>` (best effort — a write failure simply yields
/// an `Io`/`Parquet` `BundleError` downstream, which is a valid outcome).
fn write_member(dir: &Path, name: &str, bytes: &[u8]) {
    if let Ok(mut file) = std::fs::File::create(dir.join(name)) {
        let _ = file.write_all(bytes);
    }
}

fuzz_target!(|data: &[u8]| {
    install_non_aborting_hook();

    let scratch = SCRATCH.get_or_init(|| tempfile::tempdir().expect("create fuzz scratch dir"));
    let root = scratch.path();

    // The leading byte selects the overridden member; the rest is its bytes. An
    // empty input decodes the all-valid bundle. `usize::MAX` never matches an
    // index, so `None` leaves every member valid.
    let (selector, payload) = match data.split_first() {
        Some((first, rest)) => (usize::from(*first) % MEMBERS.len(), rest),
        None => (usize::MAX, &[][..]),
    };

    for (idx, (name, valid)) in MEMBERS.iter().zip(VALID.iter()).enumerate() {
        let bytes = if idx == selector { payload } else { *valid };
        write_member(root, name, bytes);
    }

    // The real bounded/batched reader over the public API — Ok or a typed
    // BundleError, never a panic or an out-of-budget allocation.
    if let Ok(reader) = chainview::BundleReader::open(root) {
        let _ = reader.load();
    }
});
