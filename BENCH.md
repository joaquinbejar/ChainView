# Benchmark baseline — `chainview`

| Field      | Value                                       |
|------------|---------------------------------------------|
| Status     | v0.1 first baseline                         |
| Last run   | 2026-07-16                                  |
| Suite      | `bench_render_chain`, `bench_event_fanin`, `bench_chain_merge` |
| Issue      | #21                                         |

These are **real measured runs on the machine below** — not design targets and
not fabricated. The no-fabricated-benchmarks rule is absolute (`CLAUDE.md`): every
number here came from `cargo bench --features bench` on this host. Re-run before
quoting on other hardware. HP-4 (`bench_replay_decode`) is v0.3 and not in this
baseline; the CI regression gate that fails on a regression past a documented
threshold is v1.0 (#52), not here.

## 1. Measurement environment

| Field       | Value                                                        |
|-------------|--------------------------------------------------------------|
| CPU         | Apple M4 Max (16 physical / 16 logical cores), `Mac16,9`     |
| Memory      | 64 GiB                                                        |
| OS          | macOS 26.5.2 (build 25F84) — Darwin 25.5.0, `arm64`          |
| Toolchain   | `rustc 1.97.0 (2d8144b7 2026-07-07)`, `cargo 1.97.0`, stable |
| Build       | `cargo bench --features bench` (the `bench` profile inherits `[profile.release]`: `opt-level = 3`, `lto = true`, `codegen-units = 1`) |
| Harness     | `criterion 0.5.1` (timing) + `hdrhistogram 7.5.4` (tail)     |
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

## 4. NFR re-baseline

| NFR | Target | Status | Evidence |
|-----|--------|--------|----------|
| **NFR-14** — 16 ms / 60 fps p99 frame budget | draw ≤ 16 000 µs @ p99 | **MEASURED — met** | HP-1 p99 = **232 µs** (≈ 1.4 % of budget). Even render + fan-in + merge combined per frame is ≈ 713 µs p99 (sum of the three p99s), well under 16 000 µs. |
| **NFR-15** — bounded memory under N-instrument streaming | steady-state working set O(`N`), not O(burst)/O(session) | **MEASURED — met** | HP-3 staging probe: 384 updates/burst → 128 slots, capacity flat across 2 000 bursts, store pending 0. |
| **NFR-16** — startup-to-first-chain < 1 s (cold) | first Deribit chain < 1 s | **PENDING** | Cold, network-dominated (one public-venue round trip). Deliberately **not** measured here — the suite is fixture-fed and deterministic. Per [06 §3.3](docs/06-performance.md#33-startup-to-first-chain-hp-1--hp-3-cold) it is measured as a **distribution against a live venue** in a future `#[ignore]` smoke, never a hard CI gate. |

## 5. Reproduce

```
cargo bench --features bench                     # all three (+ lib/bin no-op targets)
cargo bench --features bench --bench bench_render_chain
cargo bench --features bench --bench bench_event_fanin
cargo bench --features bench --bench bench_chain_merge
```

The `hdrhistogram` tail report prints first (the headline table above), then
criterion's mean cross-check. Without `--features bench` the benches are skipped
(`required-features`) and the public surface is unchanged.
