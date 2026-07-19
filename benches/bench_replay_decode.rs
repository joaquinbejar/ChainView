//! HP-4 — the replay table decode (`docs/06-performance.md` §2, §3.4; issue #36).
//!
//! Opens and decodes a conformance IronCondor result bundle — all four tables
//! (`fills` / `equity_curve` / `positions` / `greeks_attribution`) through the
//! **real**, public [`chainview::BundleReader`] open+load path under the resource
//! ceilings, including the #32 cross-table validation chain — and reports the
//! per-load latency distribution. Decode runs off the draw path, so the metric is
//! decode **service time**; the tail (`hdrhistogram` p50/p99/p99.9) is the
//! headline and criterion's mean is context only.
//!
//! Deterministic: the bundle is produced once, before measurement, by the shared
//! `tests/common/bundle_gen.rs` generator into a RAII tempdir — no live venue, no
//! socket, no wall-clock read in the decoded path. The committed adversarial corpus
//! (`tests/fixtures/bundle/`) is the security half of the same issue.

// A bench binary prints its measured report to stdout; it is NEVER the TUI, so the
// crate-wide `print_stdout`/`print_stderr` ban (which protects the live display)
// does not apply here (`CLAUDE.md` "Governance precedence" item 3 — a bench never
// owns the terminal).
#![allow(clippy::print_stdout, clippy::print_stderr)]

#[path = "../tests/common/bundle_gen.rs"]
mod bundle_gen;

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use chainview::BundleReader;
use criterion::Criterion;
use hdrhistogram::Histogram;

/// Steps in the conformance bundle — one fill / equity / position / greeks row per
/// step, i.e. `4 × STEPS` decoded rows per load. A mid-sized backtest tape.
const STEPS: usize = 20_000;
const WARMUP: u64 = 100;
const SAMPLES: u64 = 1_000;

/// The coordinated-omission disclosure for this bench (see `report`).
const CO_DISCLOSURE: &str = "none applied — a replay decode has no natural external arrival rate; the reader decodes a bundle on demand off the render thread (the scrub cursor's per-step recompute, not the one-shot decode, shares the frame budget). This measures per-load SERVICE time back-to-back, with no fixed-rate injection and no CO correction; the distribution is decode service time under the resource ceilings.";

fn main() {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(error) => {
            println!("bench_replay_decode: tempdir failed: {error}");
            return;
        }
    };
    bundle_gen::write_bench_bundle(dir.path(), STEPS);

    hdr_report(dir.path());
    let mut criterion = Criterion::default()
        .sample_size(30)
        .measurement_time(Duration::from_secs(5))
        .configure_from_args();
    criterion_bench(&mut criterion, dir.path());
    criterion.final_summary();
}

/// The `hdrhistogram` decode-latency tail report — the headline for `BENCH.md`.
fn hdr_report(dir: &Path) {
    let mut hist = match Histogram::<u64>::new(3) {
        Ok(hist) => hist,
        Err(error) => {
            println!("bench_replay_decode: histogram init failed: {error}");
            return;
        }
    };
    for _ in 0..WARMUP {
        run_once(dir);
    }
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let rows = run_once(dir);
        record(&mut hist, start);
        let _ = black_box(rows);
    }
    report(
        &format!(
            "HP-4 bench_replay_decode — open + decode the 4 conformance tables ({STEPS} steps -> {} rows)",
            STEPS.checked_mul(4).unwrap_or(0)
        ),
        &hist,
        SAMPLES,
        CO_DISCLOSURE,
    );
}

/// One measured unit: open + full decode + the #32 validation chain over the
/// conformance bundle. Returns the total decoded row count (kept live so the
/// optimizer cannot elide the decode).
fn run_once(dir: &Path) -> usize {
    match BundleReader::open(black_box(dir)).and_then(|reader| reader.load()) {
        Ok(loaded) => {
            loaded.fills.len() + loaded.equity.len() + loaded.positions.len() + loaded.greeks.len()
        }
        Err(error) => {
            println!("bench_replay_decode: unexpected load error: {error}");
            0
        }
    }
}

/// The criterion timing harness (context; mean/median only).
fn criterion_bench(c: &mut Criterion, dir: &Path) {
    c.bench_function("replay_decode_bundle", |b| {
        b.iter(|| black_box(run_once(black_box(dir))));
    });
}

/// Record one elapsed sample (nanoseconds) into the histogram.
fn record(hist: &mut Histogram<u64>, start: Instant) {
    let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let _ = hist.record(nanos);
}

/// Print the p50/p99/p99.9 tail (µs), the mean (context), and the CO disclosure.
fn report(label: &str, hist: &Histogram<u64>, samples: u64, disclosure: &str) {
    println!();
    println!("=== {label} ===");
    println!("  samples : {samples} (after {WARMUP} warmup iterations)");
    println!("  p50     : {:.3} us", us(hist.value_at_quantile(0.50)));
    println!("  p99     : {:.3} us", us(hist.value_at_quantile(0.99)));
    println!("  p99.9   : {:.3} us", us(hist.value_at_quantile(0.999)));
    println!("  max     : {:.3} us", us(hist.max()));
    println!(
        "  mean    : {:.3} us (context only; the tail is the metric)",
        hist.mean() / 1_000.0
    );
    println!("  coordinated-omission: {disclosure}");
}

/// Nanoseconds to microseconds (reporting only).
#[allow(clippy::cast_precision_loss)]
fn us(nanos: u64) -> f64 {
    nanos as f64 / 1_000.0
}
