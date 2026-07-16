//! HP-1 — the render loop (`docs/06-performance.md` §2, §3.1; issue #21).
//!
//! Draws the fullest reachable screen — a wide, fully-populated chain matrix — into
//! a `ratatui` `TestBackend` at a fixed 120×40 through the public [`render`] path,
//! and reports the per-draw latency distribution. The 16 ms/60 fps p99 frame
//! budget (NFR-14) is a **tail** metric, so `hdrhistogram`'s p50/p99/p99.9 is the
//! headline and criterion's mean is context only.
//!
//! Deterministic: the `App` is built from [`chainview::bench_support`] over a
//! synthetic 64-strike chain — no live venue, no socket, no wall clock read in the
//! draw path.

// A bench binary prints its measured report to stdout; it is NEVER the TUI, so the
// crate-wide `print_stdout`/`print_stderr` ban (which protects the live display)
// does not apply here (`CLAUDE.md` "Governance precedence" item 3 — a bench never
// owns the terminal).
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use chainview::{ViewState, bench_support, render};
use criterion::Criterion;
use hdrhistogram::Histogram;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Strikes in the synthetic chain — enough to overfill a 40-row body.
const STRIKES: usize = 64;
const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;
const WARMUP: u64 = 1_000;
const SAMPLES: u64 = 20_000;

/// The coordinated-omission disclosure for this bench (see `report`).
const CO_DISCLOSURE: &str = "none applied — a render has no natural external arrival rate (the loop draws on demand when `dirty`, never on a fixed clock), so this measures per-draw SERVICE time back-to-back, not response latency under a fixed request schedule. No fixed-rate injection and no CO correction; the reported distribution is service time, which is exactly what the 16 ms/60 fps p99 frame budget bounds.";

fn main() {
    hdr_report();
    let mut criterion = Criterion::default()
        .sample_size(60)
        .measurement_time(Duration::from_secs(3))
        .configure_from_args();
    criterion_bench(&mut criterion);
    criterion.final_summary();
}

/// The `hdrhistogram` tail report — the headline numbers pasted into `BENCH.md`.
fn hdr_report() {
    let app = bench_support::live_ready_app(STRIKES);
    let mut terminal = match Terminal::new(TestBackend::new(WIDTH, HEIGHT)) {
        Ok(terminal) => terminal,
        Err(error) => {
            println!("bench_render_chain: TestBackend init failed: {error}");
            return;
        }
    };
    let mut hist = match Histogram::<u64>::new(3) {
        Ok(hist) => hist,
        Err(error) => {
            println!("bench_render_chain: histogram init failed: {error}");
            return;
        }
    };
    let mut view = ViewState::new();
    view.sync(&app);
    for _ in 0..WARMUP {
        let _ = terminal.draw(|frame| render(&app, &view, frame));
    }
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let _ = terminal.draw(|frame| render(black_box(&app), &view, frame));
        record(&mut hist, start);
    }
    report(
        "HP-1 bench_render_chain — draw the fullest 64-strike chain matrix @ 120x40",
        &hist,
        SAMPLES,
        CO_DISCLOSURE,
    );
}

/// The criterion timing harness (context; mean/median only).
fn criterion_bench(c: &mut Criterion) {
    let app = bench_support::live_ready_app(STRIKES);
    let mut terminal = match Terminal::new(TestBackend::new(WIDTH, HEIGHT)) {
        Ok(terminal) => terminal,
        Err(error) => {
            println!("bench_render_chain: TestBackend init failed: {error}");
            return;
        }
    };
    let mut view = ViewState::new();
    view.sync(&app);
    c.bench_function("render_chain_120x40", |b| {
        b.iter(|| {
            let _ = terminal.draw(|frame| render(black_box(&app), &view, frame));
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
