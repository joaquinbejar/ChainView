//! HP-3 — the chain-store update under full-chain streaming
//! (`docs/06-performance.md` §2, §3.2; issue #21).
//!
//! Drives the busiest provider path end to end for Deribit: a `ticker.`+`book.`
//! burst normalized through the **real** adapter seam, published on the bounded,
//! coalescing [`chainview::EventBridge`] channel, and merged into a
//! [`chainview::ChainStore`] (`OptionChain`). Reports both:
//!
//! - the per-burst **merge latency** distribution (`hdrhistogram` p50/p99/p99.9,
//!   the headline; criterion mean is context only); and
//! - the **NFR-15 bounded-memory** demonstration — the staging map's slot count
//!   stays ≤ `N` (subscribed instruments) and its allocation stays flat across
//!   bursts regardless of how many updates are pushed. Memory is *measured*, not
//!   asserted from the design.
//!
//! Deterministic: fixtures/synthetic producers only — no live venue, no socket.

// A bench binary prints its measured report to stdout; it is NEVER the TUI, so the
// crate-wide `print_stdout`/`print_stderr` ban (which protects the live display)
// does not apply here (`CLAUDE.md` "Governance precedence" item 3).
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use chainview::bench_support::ChainMergeHarness;
use criterion::Criterion;
use hdrhistogram::Histogram;

/// Strikes in the synthetic chain — `2·STRIKES = 128` subscribed legs (`N`);
/// each burst is `3·N = 384` coalesced-class updates (quote + Greeks + depth).
const STRIKES: usize = 64;
/// The bounded coalesced-channel capacity (the documented default). Above the
/// per-burst size, so every distinct instrument delivers and coalesces to one
/// slot — the `384 updates → 128 slots` (O(N), not O(burst)) demonstration.
const CHANNEL_CAPACITY: usize = 1_024;
const WARMUP: u64 = 500;
const SAMPLES: u64 = 5_000;
/// Rounds for the bounded-memory probe (flat allocation across many bursts).
const STAGING_ROUNDS: u64 = 2_000;

/// The coordinated-omission disclosure for this bench (see `report`).
const CO_DISCLOSURE: &str = "none applied — same bounded+coalescing backpressure as HP-2. The provider path has a natural venue arrival rate, but the producer/consumer staging maps collapse a burst to O(N) last-value-wins, so tail SERVICE time (not queue wait) is the metric that bounds the frame budget. Measured as per-burst service time (normalize + coalesce + merge); no fixed-rate injection or CO correction.";

fn main() {
    hdr_report();
    staging_report();
    let mut criterion = Criterion::default()
        .sample_size(60)
        .measurement_time(Duration::from_secs(3))
        .configure_from_args();
    criterion_bench(&mut criterion);
    criterion.final_summary();
}

/// The `hdrhistogram` merge-latency tail report — the headline for `BENCH.md`.
fn hdr_report() {
    let mut harness = ChainMergeHarness::new(STRIKES, CHANNEL_CAPACITY);
    let mut hist = match Histogram::<u64>::new(3) {
        Ok(hist) => hist,
        Err(error) => {
            println!("bench_chain_merge: histogram init failed: {error}");
            return;
        }
    };
    // One monotonically advancing round counter across warmup + samples so each
    // burst's venue `event_time` stays above the store watermark (real merges).
    for round in 0..WARMUP {
        let _ = harness.run_burst(round);
    }
    for index in 0..SAMPLES {
        let round = WARMUP.checked_add(index).unwrap_or(index);
        let start = Instant::now();
        let applied = harness.run_burst(black_box(round));
        record(&mut hist, start);
        let _ = black_box(applied);
    }
    let legs = harness.legs();
    report(
        &format!(
            "HP-3 bench_chain_merge — Deribit ticker+book burst ({legs} legs) -> coalescing merge"
        ),
        &hist,
        SAMPLES,
        CO_DISCLOSURE,
    );
}

/// The NFR-15 bounded-memory demonstration: across many bursts the staging map's
/// slot count stays ≤ `N` and its allocation stays flat — `3·N` updates per burst
/// collapse to `N` slots, and the store's pending buffer stays bounded.
fn staging_report() {
    let mut harness = ChainMergeHarness::new(STRIKES, CHANNEL_CAPACITY);
    let legs = harness.legs();
    let mut max_slots: usize = 0;
    let mut first_capacity: Option<usize> = None;
    let mut capacity_flat = true;
    for round in 0..STAGING_ROUNDS {
        let (slots, capacity) = harness.staging_bound(round);
        if slots > max_slots {
            max_slots = slots;
        }
        match first_capacity {
            None => first_capacity = Some(capacity),
            Some(first) if capacity != first => capacity_flat = false,
            Some(_) => {}
        }
    }
    let capacity = first_capacity.unwrap_or(0);
    println!();
    println!("=== HP-3 bounded memory (NFR-15) — staging bound over {STAGING_ROUNDS} bursts ===");
    println!("  subscribed instruments N : {legs}");
    println!(
        "  updates pushed per burst : {} (quote + Greeks + depth)",
        legs.checked_mul(3).unwrap_or(0)
    );
    println!("  max staged slots         : {max_slots} (<= N: coalesced O(N), NOT O(burst))");
    println!("  staging map capacity     : {capacity} (flat across bursts: {capacity_flat})");
    println!(
        "  store pending buffer     : {} (<= MAX_PENDING; never grows with tick volume)",
        harness.store_pending()
    );
}

/// The criterion timing harness (context; mean/median only). Each iteration runs
/// one full burst (normalize + coalesce + merge), advancing the round so the
/// store watermark keeps accepting.
fn criterion_bench(c: &mut Criterion) {
    let mut harness = ChainMergeHarness::new(STRIKES, CHANNEL_CAPACITY);
    let mut round: u64 = 0;
    c.bench_function("chain_merge_burst", |b| {
        b.iter(|| {
            round = round.checked_add(1).unwrap_or(round);
            black_box(harness.run_burst(black_box(round)))
        });
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
