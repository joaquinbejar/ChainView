# Benchmark baseline — `chainview`

| Field      | Value                                       |
|------------|---------------------------------------------|
| Status     | v0.1 baseline (HP-1…HP-3) + v0.3 HP-4 + v1.0 regression gate (section 6) |
| Last run   | 2026-07-17 (HP-4); 2026-07-16 (HP-1…HP-3)   |
| Suite      | `bench_render_chain`, `bench_event_fanin`, `bench_chain_merge`, `bench_replay_decode` |
| Issue      | #21 (HP-1…HP-3), #36 (HP-4), #52 (gate)     |

These are **real measured runs on the machine below** — not design targets and
not fabricated. The no-fabricated-benchmarks rule is absolute (`CLAUDE.md`): every
number here came from `cargo bench --features bench` on this host. Re-run before
quoting on other hardware. HP-4 (`bench_replay_decode`) landed at v0.3 (#36) and
is the replay decode entry below. The CI regression gate that fails on a
regression past a documented per-path threshold landed at v1.0 (#52) and is
specified in **section 6** — the baselines in section 3 are the numbers it gates
against, and the machine-readable thresholds live in the section 6 perf-gate
block.

## 1. Measurement environment

| Field       | Value                                                        |
|-------------|--------------------------------------------------------------|
| CPU         | Apple M4 Max (16 physical / 16 logical cores), `Mac16,9`     |
| Memory      | 64 GiB                                                        |
| OS          | macOS 26.5.2 (build 25F84) — Darwin 25.5.0, `arm64`          |
| Toolchain   | `rustc 1.97.0 (2d8144b7 2026-07-07)`, `cargo 1.97.0`, stable (HP-1…HP-3); `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo 1.97.1`, stable (HP-4) — same host, a patch bump between runs |
| Build       | `cargo bench --features bench` (the `bench` profile inherits `[profile.release]`: `opt-level = 3`, `lto = true`, `codegen-units = 1`) |
| Harness     | `criterion 0.5.1` (timing) + `hdrhistogram 7.5.4` (tail)     |
| Decode deps | `parquet 59.1.0` + `arrow-array 59.1.0` (+ `zstd 0.13.3`), for HP-4 |
| Data        | fixture/synthetic producers only — no live venue, no socket, no wall-clock read in the measured path |

Numbers are from an interactive laptop under a normal desktop load (not an
isolated CI runner), so the far tail (`max`) carries scheduler noise; the p50/p99
are stable across re-runs. Treat p99.9/max as indicative, not a gate.

## 2. Methodology

- **Tail is the headline; mean is context.** A frame budget is a tail property
  ([06 §3.1](docs/06-performance.md#31-frame-budget-hp-1-hp-2)), so the reported
  metric is `hdrhistogram` **p50 / p99 / p99.9** (3 significant figures), not
  criterion's mean. Criterion's mean/median is quoted only as a cross-check.
- **Two harnesses, one binary.** Each `benches/*.rs` is a `harness = false`
  binary whose `main` runs (a) a controlled `hdrhistogram` loop — documented
  warmup then a documented sample count, each sample timed with
  `Instant::now()`, recorded in nanoseconds — and (b) a standard `criterion`
  `bench_function` for the mean/median cross-check. Integrating `hdrhistogram`
  *inside* criterion's sampler is awkward (criterion owns its own iteration
  loop), so this repo uses criterion for timing **and** records the
  `hdrhistogram` percentiles from the controlled run
  ([TESTING.md §11](docs/TESTING.md#11-performance--regression-benchmarks),
  the ecosystem `bench-hdr` convention).
- **Unit sizes.** HP-1's unit is **one full draw** of the fullest chain matrix.
  HP-2's and HP-3's unit is **one full-chain burst** — a `ticker.`+`book.`
  refresh for every one of the `N = 128` subscribed legs (`2 × 64` strikes),
  i.e. `384` updates for the merge and the same burst folded through the fan-in.
  Per-update figures are the per-burst figure ÷ the update count.
- **Coordinated omission.** Each latency bench discloses its stance inline in the
  output (reproduced in §3). In short: these hot paths have **no external
  fixed-arrival schedule** — the render loop draws on demand, and the fan-in /
  merge drain a **bounded, coalescing** channel that collapses a burst to the
  freshest value per instrument (the coalescing *is* the backpressure, so there
  is no unbounded queue in which a slow operation could hide). The benches
  therefore measure **service time** back-to-back, with **no fixed-rate injection
  and no CO correction**. Where a natural venue arrival rate exists (HP-3), the
  bounded+coalescing staging maps make tail *service* time — not queue wait — the
  quantity that bounds the frame budget. This is stated honestly rather than
  applying a CO correction that would not model the real backpressure.

## 3. Results

All three latency figures are in **microseconds**. The 60 fps frame budget is
**16 000 µs at p99** (NFR-14).

> **#47 surface screen — off the four benched hot paths.** The vol-smile /
> Greek-curve / single-expiry-surface draw is a **bounded, cached-projection paint**:
> the geometry is projected off the draw path (the `ViewState` cache) and the surface
> grid is capped at `MAX_SURFACE_CELLS` and downsampled to the visible rows/columns,
> so per-frame work is O(visible cells), not O(chain). It adds no new hot-path bench —
> this note closes the #47 DoD item "a hot-path render change carries bench evidence
> in `BENCH.md`" explicitly (the #27 precedent).

### HP-1 — `bench_render_chain` (render loop)

Draw the fullest 64-strike chain matrix into a `TestBackend` at 120×40 through
the public `render` path.

| Metric | Value (µs) |
|--------|-----------:|
| p50    |    204.671 |
| p99    |    232.319 |
| p99.9  |    279.807 |
| max    |    624.639 |
| mean (context) | 205.708 |

- Samples: **20 000** after **1 000** warmup iterations.
- Criterion cross-check: `[203.32 µs 204.06 µs 204.87 µs]` (lower/est./upper mean).
- Coordinated omission: **none applied** — no natural external arrival rate; the
  loop draws on demand, so this is per-draw service time. The distribution *is*
  what the 16 ms/60 fps p99 budget bounds.

### HP-2 — `bench_event_fanin` (event fan-in)

Fold one 128-leg `ticker.`+`book.` burst (`384` `MarketUpdate`s) through
`App::on_event`; burst generation is outside the timed region (owned, no clone).

| Metric | Value (µs) |
|--------|-----------:|
| p50    |     84.223 |
| p99    |     96.639 |
| p99.9  |    107.583 |
| max    |    154.623 |
| mean (context) | 83.160 |

- Samples: **5 000** after **500** warmup iterations.
- Criterion cross-check: `[86.079 µs 86.747 µs 87.499 µs]`.
- Per-update: ≈ **0.25 µs** (p99 96.639 µs ÷ 384 updates).
- Coordinated omission: **none applied** — the fan-in drains a bounded, coalescing
  channel; the coalescing is the backpressure, so service time is the metric.

### HP-3 — `bench_chain_merge` (chain-store update under full-chain streaming)

Normalize a Deribit `ticker.`+`book.` burst through the **real** adapter seam,
publish it on the bounded coalescing `EventBridge` channel, and merge into the
`OptionChain`.

| Metric | Value (µs) |
|--------|-----------:|
| p50    |    338.431 |
| p99    |    384.511 |
| p99.9  |    412.671 |
| max    |    441.855 |
| mean (context) | 338.725 |

- Samples: **5 000** after **500** warmup iterations.
- Criterion cross-check: `[332.96 µs 334.63 µs 336.64 µs]`.
- Per-update: ≈ **1.0 µs** (p99 384.511 µs ÷ 384 updates), covering normalize +
  coalesce + strike-keyed clone/patch/re-insert merge.
- Coordinated omission: **none applied** — natural venue arrival rate exists, but
  the producer/consumer staging maps collapse a burst to O(N) last-value-wins, so
  tail service time (not queue wait) is the frame-budget-bounding quantity.

### HP-3 — bounded memory (NFR-15), measured

The `bench_chain_merge` staging probe pushes a full burst per round for **2 000**
rounds and reads the consumer-side coalescer bound each round:

| Quantity                         | Measured                       |
|----------------------------------|--------------------------------|
| Subscribed instruments `N`       | 128                            |
| Updates pushed per burst         | 384 (quote + Greeks + depth)   |
| Max staged slots                 | **128** (= `N`; **not** 384)   |
| Staging-map capacity across bursts | **224, flat** (`true`)       |
| Store pending buffer             | **0** (≤ `MAX_PENDING = 256`)  |

`384` updates per burst collapse to `128` slots (one per instrument), and the
staging allocation is **flat** across all 2 000 bursts — memory is O(`N`
subscribed), not O(burst rate) or O(session length). This is the **measured**
face of NFR-15, demonstrated, not asserted from the design.

### HP-4 — `bench_replay_decode` (replay table decode)

Open + decode a conformance IronCondor result bundle — all four tables
(`fills` / `equity_curve` / `positions` / `greeks_attribution`) at **20 000
steps = 80 000 rows** — through the **real, public** `BundleReader` open+load
path under the default resource ceilings, **including the #32 cross-table
validation chain**. The unit is **one full open+decode+validate of the bundle**;
the bundle is generated once (before measurement) by `tests/common/bundle_gen.rs`
into a tempdir. Figures are in **microseconds**.

| Metric | Value (µs) |
|--------|-----------:|
| p50    |   7471.103 |
| p99    |   8044.543 |
| p99.9  |   8183.807 |
| max    |   8237.055 |
| mean (context) | 7492.403 |

- Samples: **1 000** after **100** warmup iterations.
- Criterion cross-check: `[7.4449 ms 7.4783 ms 7.5173 ms]` (lower/est./upper mean).
- Per-row: ≈ **0.10 µs** (p50 7471 µs ÷ 80 000 rows), covering the Parquet decode,
  the checked wire→domain narrowing (#31), and the full O(n) validation chain (#32).
- Coordinated omission: **none applied** — a decode has no natural external
  arrival rate; the reader decodes a bundle **on demand off the render thread**, so
  this is per-load **service time** back-to-back (no fixed-rate injection, no CO
  correction).
- **This is the one-shot load, not a per-frame path.** Decode runs off the draw
  path ([06 §3.4](docs/06-performance.md#34-replay-decode-hp-4)); at ≈ 7.5 ms for
  an 80 000-row bundle it opens the replay screen well within an interactive feel.
  The 16 ms/60 fps p99 **frame** budget applies to the scrub cursor's per-step
  recompute (a separate interactive path), **not** to this bulk decode.

## 4. NFR re-baseline

| NFR | Target | Status | Evidence |
|-----|--------|--------|----------|
| **NFR-14** — 16 ms / 60 fps p99 frame budget | draw ≤ 16 000 µs @ p99 | **MEASURED — met** | HP-1 p99 = **232 µs** (≈ 1.4 % of budget). Even render + fan-in + merge combined per frame is ≈ 713 µs p99 (sum of the three p99s), well under 16 000 µs. |
| **NFR-15** — bounded memory under N-instrument streaming | steady-state working set O(`N`), not O(burst)/O(session) | **MEASURED — met** | HP-3 staging probe: 384 updates/burst → 128 slots, capacity flat across 2 000 bursts, store pending 0. |
| **NFR-16** — startup-to-first-chain < 1 s (cold) | first Deribit chain < 1 s | **PENDING** | Cold, network-dominated (one public-venue round trip). Deliberately **not** measured here — the suite is fixture-fed and deterministic. Per [06 §3.3](docs/06-performance.md#33-startup-to-first-chain-hp-1--hp-3-cold) it is measured as a **distribution against a live venue** in a future `#[ignore]` smoke, never a hard CI gate. |

## 5. Reproduce

```
cargo bench --features bench                     # all four (+ lib/bin no-op targets)
cargo bench --features bench --bench bench_render_chain
cargo bench --features bench --bench bench_event_fanin
cargo bench --features bench --bench bench_chain_merge
cargo bench --features bench --bench bench_replay_decode   # HP-4 (#36)
```

The `hdrhistogram` tail report prints first (the headline table above), then
criterion's mean cross-check. Without `--features bench` the benches are skipped
(`required-features`) and the public surface is unchanged.

## 6. Regression gate (CI, issue #52)

The v1.0 stability commitment turns the section 3 baselines into an enforced
CI gate: a hot-path benchmark whose **p99** regresses past its documented
per-path ceiling **fails the build** (NFR-17,
[06 §5](docs/06-performance.md#5-regression-gates),
[TESTING.md §11](docs/TESTING.md#11-performance--regression-benchmarks)). The
gated metric is `hdrhistogram` **p99** — a frame budget is a tail property, so
the mean is never gated; **p99.9 and max are indicative, not gated** (section 1).

### 6.1 Mechanics

- `scripts/check-perf.sh` reads the machine-readable perf-gate block below
  (baseline p99 + per-path threshold, both in us), runs the four benches, parses
  each headline p99, and fails when `measured p99 > baseline + threshold`. It
  reads the **committed** file, so the job can never rewrite the baseline it
  gates against.
- **`ceiling = baseline + threshold`.** The threshold is the documented
  regression band: a conservative noise + headroom margin (see 6.2), so a warmed
  re-run on baseline-class hardware never flakes and only a **structural**
  regression breaches it.
- **`scripts/check-perf.sh --self-test`** proves the gate FIRES without running a
  bench: it feeds the comparison engine synthetic measured sets derived from the
  committed baselines/thresholds — a within-threshold set (passes), a
  deliberately slowed set (fails on every path), a missing-measurement set
  (fails), and a mixed set (exactly one fail). It is deterministic and
  hardware-independent, so it is the **CI-blocking** proof the gate is not
  vacuous. It also proves this perf-gate block parses.

### 6.2 The perf-gate block (source of truth for the gate)

`scripts/check-perf.sh` parses the fenced block between the `perf-gate` markers.
Columns: `bench_name  metric  baseline_us  threshold_us`. The baselines are the
section 3 p99 figures (Apple M4 Max); the thresholds are a conservative
regression band roughly the size of the baseline (headroom for a comparable
machine under desktop load), tightened only where the absolute number is large
and stable (HP-4). A breach is a structural regression, not jitter.

<!-- perf-gate:begin -->
```text
bench_render_chain    p99    232.319     250.000
bench_event_fanin     p99     96.639     150.000
bench_chain_merge     p99    384.511     300.000
bench_replay_decode   p99   8044.543    3000.000
```
<!-- perf-gate:end -->

| Bench | Baseline p99 (us) | Threshold (us) | Ceiling (us) |
|-------|------------------:|---------------:|-------------:|
| `bench_render_chain` (HP-1)  |  232.319 |  250.000 |   482.319 |
| `bench_event_fanin` (HP-2)   |   96.639 |  150.000 |   246.639 |
| `bench_chain_merge` (HP-3)   |  384.511 |  300.000 |   684.511 |
| `bench_replay_decode` (HP-4) | 8044.543 | 3000.000 | 11044.543 |

### 6.3 Runner-noise + wall-clock deviation (honest mechanics)

Two realities make an absolute-p99 gate on GitHub-hosted runners dishonest, so
the gate is wired around them (see `.github/workflows/ci.yml`, job
`perf-regression`):

1. **Hardware class.** A shared runner is a **slower, noisier** class than the
   section 1 baseline host (Apple M4 Max), so an absolute-p99 gate against these
   baselines would always breach there — a flake generator, not a regression
   catcher.
2. **Wall-clock.** `bench_render_chain` (HP-1) and `bench_replay_decode` (HP-4)
   are fast, but `bench_event_fanin` (HP-2) and `bench_chain_merge` (HP-3)
   **rebuild and normalize the full leg set through the real Deribit seam on
   every sample** (the burst generation, untimed but executed thousands of times
   across the hdr loop and criterion's iterations), so each runs for **many
   minutes** on the baseline host and longer on a runner — unbounded for a
   bounded CI job. The GATED number (the hdr p99 fold service time) is still
   fast; the wall-clock to produce it is not.

The honest wiring:

- **CI-blocking:** `scripts/check-perf.sh --self-test` — deterministic and
  hardware-independent, it proves the gate detects a synthetic regression and
  accepts a within-threshold run. This is the enforced acceptance-criterion
  check ("a deliberately slowed run the gate rejects").
- **CI-informational:** `scripts/check-perf.sh --run --only bench_render_chain
  --report-only` — runs the ONE fast bench end-to-end on the runner, exercising
  the real hdrhistogram-output parser (so a bench-format change is caught), and
  never fails the build. The three other benches are not run in CI: HP-2/HP-3
  exceed a bounded CI wall-clock (above), and an absolute breach on a
  non-baseline-class runner is not a regression anyway.
- **Enforced absolute gate:** `scripts/check-perf.sh --run` (via `make perf`) on
  **baseline-class hardware** (the developer machine that recorded these numbers,
  or a self-hosted M4 runner), where the measured p99 is comparable to the
  committed baseline. This is the real four-bench absolute-threshold enforcement;
  budget several minutes for the HP-2/HP-3 generation cost.

### 6.4 Re-baselining legitimately (never to hide a regression)

A genuine, understood performance change re-baselines through a **reviewed
BENCH.md edit in the same PR**, never by the job rewriting the file:

1. Re-run `cargo bench --features bench` on baseline-class hardware and record
   the new section 3 p99 numbers with the environment (section 1).
2. Update the matching row in the section 6.2 perf-gate block (baseline, and the
   threshold if the noise band genuinely changed) and the mirror table.
3. Explain the delta in the PR (what changed and why the new number is expected)
   — a re-baseline that hides a regression is a review 🔴. Because the gate reads
   the committed block, the reviewer is gating the number, not the CI job.

The three NFR figures (frame budget NFR-14, bounded memory NFR-15, startup
NFR-16) are re-baselined the same way; NFR-16 stays PENDING (section 4) until a
live-venue distribution is measured — a fabricated startup number would violate
the no-fabricated-benchmarks rule.
