# ChainView fuzz targets

The two v1.0 security-gate fuzz targets (issue #53), driving ChainView's two
**parser surfaces** — the places untrusted external bytes become domain values
(`docs/SECURITY.md` §6.2, §7; `docs/TESTING.md` §13.4).

| Target | Surface | Invariant |
|--------|---------|-----------|
| `fuzz_replay_decode` | replay `manifest.json` + Parquet decode via `BundleReader::open`/`load` | `Ok(LoadedBundle)` or a typed `BundleError`, never a panic or an allocation past `MAX_WORKING_SET` |
| `fuzz_provider_normalize` | Deribit `ticker.`/`book.`/instrument-name normalize seam via `chainview::fuzz_support` | typed reject / dropped field or a valid `QuoteUpdate`/`GreeksRow`/`DepthLadder`, never a panic, never a NaN/Inf reaching a domain value |

This is a **separate cargo-fuzz crate**, excluded from the default build. The
repo root declares no `[workspace]`, so `cargo build` / `cargo test` at the root
never touch `fuzz/`; the empty `[workspace]` in `fuzz/Cargo.toml` isolates it in
both directions. `libfuzzer-sys` / `tempfile` are dev tooling here, never in the
root `[dependencies]`, so `cargo audit` / `cargo deny` over the shipped tree are
unaffected.

## Running (nightly required)

cargo-fuzz needs a nightly toolchain.

```sh
# Seed the corpora from the committed fixtures (idempotent; gitignored output).
./fuzz/gen_corpus.sh

# A short, bounded smoke — the CI shape (per target, ~60s):
cargo +nightly fuzz run fuzz_replay_decode      -- -max_total_time=60 -rss_limit_mb=3072
cargo +nightly fuzz run fuzz_provider_normalize -- -max_total_time=60
```

`fuzz/corpus/`, `fuzz/artifacts/`, `fuzz/coverage/`, and `fuzz/target/` are
gitignored: the corpus is reproducible from the shared fixtures via
`gen_corpus.sh`, and a crashing artifact becomes a committed regression test (see
below), not a checked-in blob.

## Seed corpora

`gen_corpus.sh` derives the seeds from the committed shared fixtures
(`tests/fixtures/deribit/**` and `tests/fixtures/bundle/**`, from issues #17/#36
and the provider fixtures), prefixing the one-byte selector each target reads:

- `fuzz_provider_normalize`: byte `0` ticker, `1` book, `2` instrument-name.
- `fuzz_replay_decode`: byte `0` manifest, `1` fills, `2` equity, `3` positions,
  `4` greeks — the **overridden** member; the other four stay the valid fixture,
  so a single fuzzed member still reaches the full bounded/batched decode.

## Crash to regression flow

A fuzz run is a **bounded smoke**, not an open-ended campaign. When a run finds a
crash:

1. **Reproduce + minimize.** cargo-fuzz drops the input under
   `fuzz/artifacts/<target>/`. Minimize it:
   `cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<crash>`.
2. **Fix the seam, not the target.** The fix is a typed error or a checked
   conversion in the production code (`src/replay/*` for a bundle crash,
   `src/providers/deribit.rs` for a normalize crash) — never a guard in the fuzz
   harness. Fuzzing hardens the parser; it does not license papering over a bug.
3. **Land the minimized input as a committed unit regression.** A replay crash
   becomes a named directory in the adversarial set
   (`tests/fixtures/bundle/<name>/`) with a `#[test]` asserting it now returns its
   specific typed `BundleError` (`docs/TESTING.md` §13.3). A normalize crash
   becomes a `#[test]` (or a `tests/fixtures/deribit/**` fixture) asserting the
   bytes now return their typed `NormalizeKind` / dropped field. The fuzz find
   thereby becomes a permanent, fast unit assertion that runs in the default
   suite.

## Known open finding (issue #53 first run)

The very first `fuzz_replay_decode` smoke found a genuine crash, which is the
point of the gate. A `greeks_attribution.parquet` whose footer carries a
**malformed embedded Arrow schema** (the base64 `ARROW:schema` flatbuffer
key-value metadata) makes the **upstream** `arrow-ipc` crate panic in
`get_data_type` (`arrow-ipc/convert.rs:332`), reached through
`BundleReader::load` -> `ParquetRecordBatchReaderBuilder::try_new`. ChainView's
`.map_err` on that call cannot catch a `panic!`, so the panic escapes the
reader — an untrusted-input panic on the replay parser surface
(`docs/SECURITY.md` §6.2 says a malformed bundle must be a typed `BundleError`,
never a panic).

- **Status:** OPEN, deferred to `replay-expert` — the fix is a production change
  (`src/replay/*`), not a fuzz-harness guard, and is out of the architect's
  write scope. `fuzz_provider_normalize` ran clean (809,601 runs, 0 findings).
- **Recommended fix:** wrap the Parquet builder construction and the batch
  iteration in `src/replay/mod.rs` in `std::panic::catch_unwind` (safe under
  `#![forbid(unsafe_code)]`), converting an upstream decoder panic into
  `BundleError::Parquet`; or adopt an upstream arrow-rs release that returns
  `Err` instead of panicking. Then land the minimized input under
  `tests/fixtures/bundle/malformed_arrow_schema/` with a `#[test]` asserting the
  now-typed `BundleError`, per the flow above.

Until the mitigation lands, the `fuzz_replay_decode` CI step is expected to be
red — the gate is correctly reporting a real defect, not a flake.
