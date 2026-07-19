//! HP-2 — the event fan-in (`docs/06-performance.md` §2, §3.1; issue #21).
//!
//! Folds a scripted [`chainview::MarketUpdate`] burst through the synchronous
//! fan-in ([`App::on_event`]) and reports the per-burst latency distribution.
//! Like HP-1 the frame budget is a tail property, so `hdrhistogram`'s
//! p50/p99/p99.9 is the headline and criterion's mean is context only.
//!
//! Deterministic: the burst is a fresh, non-crossed `ticker.`+`book.` refresh for
//! every subscribed leg, normalized through the real Deribit seam and pre-built
//! **outside** the timed region — no live venue, no socket, no clone in the fold.

// A bench binary prints its measured report to stdout; it is NEVER the TUI, so the
// crate-wide `print_stdout`/`print_stderr` ban (which protects the live display)
// does not apply here (`CLAUDE.md` "Governance precedence" item 3).
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use chainview::{App, AppEvent, bench_support};
use criterion::{BatchSize, Criterion};
use hdrhistogram::Histogram;

/// Strikes in the synthetic chain — `2·STRIKES = 128` subscribed legs.
const STRIKES: usize = 64;
const WARMUP: u64 = 500;
const SAMPLES: u64 = 5_000;

/// The coordinated-omission disclosure for this bench (see `report`).
const CO_DISCLOSURE: &str = "none applied — the fan-in drains a bounded, COALESCING channel that collapses a burst to the freshest value per instrument (docs/02 §5), so there is no unbounded queue in which a slow fold could hide (the coalescing IS the backpressure). This measures per-burst SERVICE time folding a pre-built OWNED burst (burst generation is outside the timed region); no fixed-rate injection or CO correction is applied.";

fn main() {
    hdr_report();
    let mut criterion = Criterion::default()
        .sample_size(60)
        .measurement_time(Duration::from_secs(3))
        .configure_from_args();
    criterion_bench(&mut criterion);
    criterion.final_summary();
}

/// Fold `burst` through the fan-in — the timed unit.
fn fold_burst(app: &mut App, burst: Vec<chainview::MarketUpdate>) {
    for update in burst {
        app.on_event(AppEvent::Market(update));
    }
}

/// The `hdrhistogram` tail report — the headline numbers pasted into `BENCH.md`.
fn hdr_report() {
    let mut app = bench_support::live_ready_app(STRIKES);
    let mut hist = match Histogram::<u64>::new(3) {
        Ok(hist) => hist,
        Err(error) => {
            println!("bench_event_fanin: histogram init failed: {error}");
            return;
        }
    };
    // Warmup and samples share ONE monotonically advancing round counter so each
    // burst's venue `event_time` stays above the store's watermark (an equal or
    // lower timestamp would drop as out-of-order and hollow out the fold).
    for round in 0..WARMUP {
        fold_burst(&mut app, bench_support::market_burst(STRIKES, round));
    }
    for index in 0..SAMPLES {
        let round = WARMUP.checked_add(index).unwrap_or(index);
        // Generation is UNTIMED; only the fold is measured.
        let burst = bench_support::market_burst(STRIKES, round);
        let start = Instant::now();
        fold_burst(black_box(&mut app), black_box(burst));
        record(&mut hist, start);
    }
    report(
        "HP-2 bench_event_fanin — fold a 128-leg ticker+book burst through App::on_event",
        &hist,
        SAMPLES,
        CO_DISCLOSURE,
    );
}

/// The criterion timing harness (context; mean/median only). `iter_batched`
/// generates each fresh owned burst in the untimed setup, so only the fold is
/// measured.
fn criterion_bench(c: &mut Criterion) {
    let mut app = bench_support::live_ready_app(STRIKES);
    let mut round: u64 = 0;
    c.bench_function("event_fanin_burst", |b| {
        b.iter_batched(
            || {
                round = round.checked_add(1).unwrap_or(round);
                bench_support::market_burst(STRIKES, round)
            },
            |burst| fold_burst(&mut app, burst),
            BatchSize::SmallInput,
        );
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
