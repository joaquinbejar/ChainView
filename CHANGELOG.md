# Changelog

All notable changes to `chainview` are documented in this file.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(the full versioning policy lives in the design docs, local until v0.1.0).

> **Status:** `v0.0.1` is a crates.io name reservation — no implementation
> code exists yet. The first real entries land with the v0.1 work from the
> roadmap (local during the design phase).

## [Unreleased]

### Added

- **Release packaging and the zero-config headline-acceptance procedure** (issue
  #58; `Cargo.toml` `[package.metadata.binstall]`, `.github/workflows/release.yml`,
  `scripts/changelog-section.sh`, `docs/RELEASE-PROCESS.md` §6/§12, `BENCH.md` §7).
  Adds the `cargo binstall chainview` packaging config (well-formed and validated
  against the binstall spec via `cargo metadata`; the release-artifact naming
  `chainview-<target>.tar.gz` matches the new tag-triggered release workflow) and
  a `Release` workflow that, at a `v*` tag cut, builds the four native target
  binaries, smokes `--version`/`--help` and `cargo install --path .`, publishes to
  crates.io, and attaches the prebuilt archives to the GitHub Release. No new
  runtime dependency — the binstall block is metadata only.
  - **The clean-machine zero-config acceptance is documented as a POST-PUBLISH
    release-cut step** (`docs/RELEASE-PROCESS.md` §12), not a CI job against a live
    venue: on a fresh host with no `CHAINVIEW_*` env and no config file,
    `cargo install chainview && chainview` renders a live Deribit BTC chain
    ([ADR-0003](docs/adr/0003-zero-config-first-run.md)). It is underwritten
    offline by the #22 live-path integration test in CI and by the operator's
    `#[ignore]` Deribit live smoke (`docs/TESTING.md` §8, no credentials).
  - **The three NFR figures ship as measured facts where measurable** (`BENCH.md`
    §7). NFR-14 (frame budget) re-confirmed on the baseline host — HP-1 p99
    254.335 us, within the committed gate ceiling, ~1.6 % of the 16 ms budget;
    NFR-15 (bounded memory) stands on the committed HP-3 staging probe. NFR-16
    (startup-to-first-chain) is recorded HONESTLY as a release-cut distribution to
    be measured on the clean machine post-publish — never fabricated, never a hard
    live-venue CI gate.
  - **Honesty note.** The crate on crates.io is still the `v0.0.1` name-reservation
    placeholder, so `cargo binstall`/`cargo install` end-to-end and the live-venue
    startup measurement are release-cut steps proven at the actual v1.0 cut, not
    claimed as verified here. `[package].version` stays pre-1.0.
- **The cross-screen v1.0 polish pass on states, theme, and keybindings** (issue
  #57; `src/ui/depth.rs`, `src/ui/replay.rs`, `src/ui/surface.rs`,
  `src/ui/payoff.rs`, `src/tests_integration.rs`, `src/tests_replay_integration.rs`,
  `tests/render/golden/`, `docs/05-views-and-ux.md` §5). The first point the whole
  screen set is audited as one coherent product — every screen carries the full
  empty / loading / not-supported / stale-reconnecting / computed-Greeks / error
  state set, with the mode-correct retry key on every error state.
  - **Depth ladder price precision (closes #109).** The order-book ladder's price /
    size formatter now uses **venue-scale-aware precision** at the render edge: a
    sub-unit BTC option premium quoted in fractions of a coin keeps its significant
    figures (`0.0490` / `0.0480`) instead of collapsing to two decimals (the old
    `0.04` truncation). Index/underlying-scale prices stay at two-decimal cents. The
    `depth/deribit_btc_ladder` golden is regenerated in the same commit as a visible
    diff, and the ladder stays `NO_COLOR`-safe.
  - **The replay fills list follows the drill-down selection.** When a `,` step moves
    the selected fill above the recent-tape window, the list now **scrolls up** so the
    selected fill stays on screen (its highlight never leaves the viewport), replacing
    the v0.3 off-window `▸ selected fill ↑ (step N)` indicator. The window is a pure
    function of the on-screen row count, the as-of tape length, and the selection
    index, so the draw stays pure and stashes no scroll offset.
  - **The vol-surface screen gained its stale / reconnecting state.** A dropped stream
    no longer leaves a bright, trusted-looking smile/curve: the title carries the
    stream-health badge (`◐ stale` / `↻ reconnecting (n)`) and the plotted line dims,
    mirroring the chain/depth stale idiom — the §6 state the surface previously lacked.
  - **The replay payoff-at-head panel renders a real bundle-error state.** A failed
    bundle previously fell through to the "loading bundle…" note; it now shows the
    actionable error with the **mode-correct** `R` retry key (re-open + revalidate) —
    a bundle error never surfaces the Live `r` provider-reconnect key.
  - **Accessibility + layout goldens.** A `NO_COLOR` golden pair for the chain matrix
    proves the visible marker set is identical with and without color (a style-level
    probe proves color is genuinely stripped, not that both happen to match); a new
    `common/too_small` golden pins the cross-screen "widen the terminal" state; and new
    per-state goldens cover the surface stale render and the replay payoff error render.
- **The bundle-compatibility freeze against `ironcondor.bundle.v1`** (issue #56;
  `src/replay/mod.rs`, `src/error.rs`, `tests/replay_bundle_fixtures.rs`,
  `tests/common/sha256.rs`, `tests/fixtures/bundle/valid/SHA256SUMS`,
  `docs/04-replay-mode.md` §2, `docs/TESTING.md` §6,
  `docs/adr/0004-ironcondor-result-bundle-as-replay-format.md`). Freezes the set of
  result-bundle `schema` tags the reader accepts, records the shared conformance
  fixture's digest as the cross-repo handshake, and documents the coordinated
  schema-change process — a new tag is a documented, cross-repo change, never a
  unilateral drift on either side. The reader stays **read-only**, money stays
  **integer cents**, and `run_id`/`metrics` stay **opaque** — the freeze relaxes
  none of it.
  - **The accepted-tag gate is exactly `{ "ironcondor.bundle.v1" }`.** The existing
    `SUPPORTED_SCHEMA` pin is confirmed and covered by new tests: the gate accepts
    exactly `v1` and rejects every other tag (a bumped version, an unrelated format,
    a case-variant, an empty tag) with a typed `BundleError::UnsupportedSchema`
    naming the (length-clamped) tag — never a partial render. A freeze-pin canary
    asserts the const is `ironcondor.bundle.v1` so any drift is a reviewed edit. The
    within-tag permissive rule is proven end-to-end: a newer-minor bundle with an
    extra optional column still opens **and** loads, while a missing required
    table/column stays a hard `BundleError` (`MissingTable`/`Schema`).
  - **The conformance-fixture `sha256` is recorded and asserted.** A committed
    `shasum -a 256`-format sidecar (`tests/fixtures/bundle/valid/SHA256SUMS`) pins
    the digest of each of the five conformance-bundle files;
    `test_conformance_fixture_sha256_is_frozen` recomputes and asserts them, so a
    byte drift on either side fails a test rather than diverging silently. The
    ignored regenerator refreshes the sidecar in lockstep. **No new dependency:** the
    digest is computed by a self-contained, no-`sha2` SHA-256 (`tests/common/sha256.rs`)
    verified against published NIST vectors and the system `shasum -a 256` — no
    hashing crate is added to the tree.
  - **`BundleError` is now `#[non_exhaustive]`.** Post-1.0 a new reject reason (a new
    tag, a new integrity check) can land as a source-compatible addition; in-crate
    match sites still exhaustiveness-check and downstream matches already carry the
    mandatory wildcard arm, so no match site changed.
  - **Honest status resolution.** `docs/04-replay-mode.md` §2's PROPOSED note is
    resolved to the gated state — **frozen tag agreed, reader pinned, conformance
    digest recorded; awaiting the upstream IronCondor writer release** — with no
    fabricated cited frozen writer version. A new §2.4 documents the cross-repo
    schema-change process (a new tag = coordinated ADR on both sides; ChainView adds
    the new tag as a minor while still reading `v1`, drops `v1` only in a major; the
    shared fixture regenerated in lockstep), and ADR-0004 records the freeze as an
    implementation of its unchanged decision.
- **The CLI / config / keybinding surface freeze and the SemVer CI machinery**
  (issue #55; `docs/SEMVER.md`, `docs/05-views-and-ux.md` §1/§3,
  `docs/07-configuration.md` §4, `.env.example`, `scripts/check-changelog.sh`,
  `scripts/surface-diff.sh`, `.github/workflows/ci.yml`, `docs/ROADMAP.md`,
  `docs/specs/providers.md`). Audits and freezes the three ChainView-owned public
  surfaces — the CLI, the config, and the documented keybinding map — as
  stable-with-additive-only growth from `v1.0.0`, and wires the SemVer + CHANGELOG
  CI enforcement. Docs + CI + scripts only; **no production source change**.
  - **The three surfaces enumerated and frozen against their source.** The CLI
    grammar (the `replay <dir>` subcommand, the nine live/global flags, exit codes
    `0`/`1`, and the built-in provider ids — `src/main.rs`), the config
    environment variables and `CLI > env > file > default` precedence
    (`src/config.rs`), and the keybinding map (`src/app/keymap.rs` `KEYMAP` ->
    `docs/05-views-and-ux.md` §3) each match their `docs/` page cell-for-cell; the
    frozen set and the audit are recorded in a new `docs/SEMVER.md`
    "v1.0 surface-stability audit (#55)" section. `RESERVED_PROVIDER_IDS` is
    confirmed to hold **exactly** the five built-ins
    (`deribit`/`tastytrade`/`dxlink`/`ig`/`alpaca`), and the provider-port types
    (`ProviderCapabilities` + `ChainCapability`/`GreeksCapability`/
    `OptionStreamCapability`/`ChainPollCapability`/`AuthKind`) are confirmed
    `#[non_exhaustive]` + builder-shaped, so an optional capability dimension stays
    a source-compatible **minor** bump (proven by the #44
    `capabilities_source_compat` `trybuild` gate this references, does not
    duplicate).
  - **Three doc<->code drifts fixed.** The keybinding-map source of truth moved to
    the application layer at #14 but three pages still named `src/ui/theme.rs`:
    `docs/SEMVER.md` (the surface table), `docs/05-views-and-ux.md` §1 and §3 now
    name `src/app/keymap.rs` (the `KEYMAP`) with `src/ui/theme.rs` as the overlay
    renderer. `.env.example` advertised the **superseded** `ProviderId` grammar
    (`^[a-z][a-z0-9_-]{1,31}$`); it is corrected to the tightened ADR-0008 form
    (`^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$`, 2-32 chars, isolated separators) that
    `src/chain/identity.rs` and `docs/07` §5.1 already use.
  - **`changelog-check` — the CHANGELOG gate** (`scripts/check-changelog.sh` +
    the CI job). A user-visible PR must add a new line under `## [Unreleased]`;
    the gate skips on the `chore:`/`refactor:`/`test:`/`docs:`/`ci:`/`bench:`
    title prefixes and the `[skip changelog]` token, and fails closed otherwise.
    The comparison is **section-scoped** (only the `[Unreleased]` block is
    diffed) so unrelated edits neither satisfy nor break it. A `--self-test`
    proves the gate is non-vacuous (an added line passes; a no-added-line diff
    fails; both skip paths skip) with no git and no dependency.
  - **`surface-diff` — the public-surface change reminder**
    (`scripts/surface-diff.sh` + the CI job). Informational: it flags a PR
    touching a frozen public-surface source (`src/main.rs`, `src/config.rs`,
    `src/app/keymap.rs`, `src/ui/theme.rs`, `src/providers/mod.rs`,
    `src/chain/identity.rs`, `src/lib.rs`) with a reviewer classification
    checklist, so a rename/removal is caught in review rather than shipped
    silently under a minor bump. It never fails on a mere touch; its `--self-test`
    (flags a surface touch, ignores an internal-only change) runs as the BLOCKING
    proof-of-liveness step. Both jobs are `pull_request`-scoped and bounded, so an
    unrelated PR never flakes.
  - **`[package].version` v1.0.0-cut discipline recorded.** The crate stays at its
    pre-1.0 `[package].version`; the release-cut rule (rename `[Unreleased]`, bump
    `[package].version` to `1.0.0`) is documented in `docs/SEMVER.md`. This issue
    does **not** bump the version.
  - **Routed #54 fold-in: `docs/specs/providers.md` §0 pin refresh.** The §0
    provenance table had drifted from `Cargo.lock` ground truth — refreshed per
    the file's own procedure: `alpaca-websocket` `0.4.0` -> **`0.6.0`** and
    `optionstratlib` `0.17.2` -> **`0.18.0`** (the versions `Cargo.lock` resolves
    and `Cargo.toml` pins), each commit cell marked unverified against the new
    version; and the stale "unapproved in `Cargo.toml`" preamble is reconciled
    (the deps are approved and in `Cargo.toml` now).
- **Fuzzing the parser surfaces — the v1.0 security gate** (issue #53; the
  `fuzz/` cargo-fuzz crate, `src/fuzz_support.rs`, `src/providers/deribit.rs`,
  `.github/workflows/ci.yml`, `docs/TESTING.md` §13.4, `docs/SECURITY.md` §7,
  `docs/ROADMAP.md`). Lands the two fuzz targets the security gate requires, each
  driving a real **parser surface** — the two places untrusted external bytes
  become domain values — with arbitrary bytes: any input is a typed error or a
  valid bundle / domain row, never a panic or an out-of-budget allocation.
  - **`fuzz_replay_decode`** — feeds arbitrary bytes as a `manifest.json` and as
    each Parquet table into the public `BundleReader::open`/`load` (the bounded,
    batched, ceiling-measured decode). A leading selector byte overrides one of
    the five bundle members while the other four stay the committed VALID fixture,
    so a single fuzzed member still reaches the full decode; the outcome is
    `Ok(LoadedBundle)` or a typed `BundleError`, never a panic and never an
    allocation past `MAX_WORKING_SET`.
  - **`fuzz_provider_normalize`** — feeds arbitrary bytes through the REAL Deribit
    `ticker.`/`book.`/instrument-name normalize seam (via the feature-gated
    `chainview::fuzz_support`, which reaches the `pub(crate)` seam a separate
    crate cannot). The outcome is a typed reject / dropped field or a valid
    `QuoteUpdate`/`GreeksRow`/`DepthLadder`, with every produced `Positive`/`Decimal`
    asserted a valid domain numeric — so a NaN/Inf/negative never reaches a domain
    value (governance item 2). Deribit is the only shippable, default-compiled
    built-in with a JSON→domain byte-parser seam; the gated adapters extend the
    target when their features are enabled.
  - **Seed corpora derived from the shared fixtures.** `fuzz/gen_corpus.sh` builds
    the seeds from the committed provider + replay fixtures
    (`tests/fixtures/deribit/**`, `tests/fixtures/bundle/**`), never hand-typed
    bytes, so the fuzzer starts from valid shapes and mutates outward. The
    generated corpus is gitignored (reproducible from the fixtures).
  - **A time-boxed CI smoke job on nightly.** A separate `fuzz` job pins a nightly
    toolchain (cargo-fuzz needs it), seeds the corpus, and runs each target for a
    bounded `-max_total_time`, failing on any crash/panic — distinct from an
    open-ended campaign, and separate from the MSRV/stable non-negotiables. The
    crash→regression flow (minimize, fix the seam, land the minimized input as a
    committed unit regression in the adversarial set) is documented in
    `fuzz/README.md` and `docs/TESTING.md` §13.3–§13.4.
  - **Dev-tooling audit note (ADR-0007).** The `fuzz` Cargo feature adds NO
    dependency to the main tree (`fuzz = []`); `libfuzzer-sys` and `tempfile` live
    only in `fuzz/Cargo.toml`, a separate crate with its own `[workspace]`,
    excluded from the default build (the root declares no `[workspace]`, so
    `cargo build`/`cargo test`/`cargo package` never touch `fuzz/`). `cargo audit`
    / `cargo deny` over the shipped tree are unaffected. `#![forbid(unsafe_code)]`
    stays — fuzzing hardens the parser, it does not license `unsafe`.
  - **First-run finding — FOUND AND FIXED.** The initial smoke proved the gate
    works: `fuzz_provider_normalize` ran clean (809,601 runs), and
    `fuzz_replay_decode` found a genuine untrusted-input panic — a
    `greeks_attribution.parquet` with a malformed embedded Arrow schema panics the
    **upstream** `arrow-ipc` crate (`get_data_type`) inside
    `ParquetRecordBatchReaderBuilder::try_new`, which `BundleReader::load`'s
    `.map_err` cannot catch (`docs/SECURITY.md` §6.2 requires a typed `BundleError`,
    never a panic). **Mitigation:** `BundleReader`'s `scan_table` now wraps the two
    upstream decode calls — the footer/`ARROW:schema` read
    (`ParquetRecordBatchReaderBuilder::try_new`, and the reader `build`) and the
    per-batch pull (`reader.next()` in the ceiling loop) — in
    `std::panic::catch_unwind` (safe under `#![forbid(unsafe_code)]`; no `unsafe`),
    so an escaping upstream decoder panic becomes a typed `BundleError::Parquet`
    (bounded, non-secret, naming the table only; the panic payload is never echoed).
    The wrap is tight: the cancellation check and the working-set `budget.account`
    stay OUTSIDE the boundary, so a `BundleError::Cancelled` and the ceilings are
    never swallowed. **Caveat:** `catch_unwind` still runs the process panic hook
    (an `stderr` line, or the TUI restore hook) before returning; this domain seam
    stays free of terminal knowledge and does not touch the global hook.
    **Regression:** a committed full-bundle fixture
    `tests/fixtures/bundle/malformed_arrow_schema/` (the minimized fuzz artifact as
    the malformed greeks table + three valid siblings + a valid manifest) drives the
    exact panic path through the real `BundleReader::open`+`load` and asserts the
    typed `BundleError::Parquet` (`tests/replay_bundle_fixtures.rs`,
    `tests/common/bundle_gen.rs::write_malformed_arrow_schema`) — red before the
    `catch_unwind` boundary, green after (`docs/TESTING.md` §13.3). The
    `fuzz_replay_decode` CI step is expected green.
- **The CI perf-regression gate — v1.0 opens** (issue #52; `scripts/check-perf.sh`,
  `.github/workflows/ci.yml`, `Makefile`, `BENCH.md` §6, `docs/06-performance.md`
  §5, `docs/TESTING.md` §11, NFR-17). Turns the ADR-0007 performance budget into a
  CI-enforced gate: a hot-path benchmark (HP-1…HP-4) whose `hdrhistogram` **p99**
  regresses past its documented per-path ceiling (`baseline + threshold`) is
  rejected. No new dependency (script + CI + docs only); no runtime or dev-dep
  added.
  - **`scripts/check-perf.sh` — the comparison gate.** Reads the machine-readable
    perf-gate block committed in `BENCH.md` §6.2 (baseline p99 + per-path
    threshold, in us), runs the four benches, parses each headline p99, and fails
    when `measured > baseline + threshold`, printing the offending path, baseline,
    threshold, ceiling, and measured. It reads the **committed** file, so the job
    can never rewrite the baseline it gates against — a re-baseline is a reviewed
    `BENCH.md` edit in the same PR. The gated metric is p99 (a frame budget is a
    tail property); p99.9 and max stay indicative, not gated.
  - **`--self-test` — the non-vacuous proof.** Feeds the comparison engine
    synthetic measured sets derived from the REAL committed baselines/thresholds —
    a within-threshold set (passes), a deliberately slowed set (fails on every
    path), a missing-measurement set (fails; never a vacuous pass), and a mixed
    set (exactly one fail). Deterministic and hardware-independent, so a green run
    means "the gate detects a regression", not "the check never ran". It also
    proves the `BENCH.md` perf-gate block parses.
  - **Honest wiring for a noisy shared runner (documented deviation).** Two
    realities make an absolute-p99 CI gate dishonest: a GitHub-hosted runner is a
    slower, noisier hardware class than the `BENCH.md` baseline host (Apple M4
    Max); and HP-2 `bench_event_fanin` / HP-3 `bench_chain_merge` rebuild and
    normalize the full leg set through the real Deribit seam on **every sample**
    (untimed generation, executed thousands of times), so each runs many minutes
    of wall-clock even on the baseline host — unbounded for a bounded CI job (the
    gated fold p99 is still fast; the wall-clock to produce it is not). The new
    `perf-regression` CI job therefore **blocks** on `--self-test` (deterministic,
    hardware-independent, the enforced acceptance-criterion check) and runs
    `--run --only bench_render_chain --report-only` **informationally** — one fast
    bench end-to-end to exercise the real hdrhistogram-output parser, never
    failing the build. The **absolute four-bench enforcement** (`--run`, `make
    perf`) runs on baseline-class hardware, where the multi-minute wall-clock is
    acceptable and the measured p99 is comparable to the committed baseline.
    Pinned to the MSRV floor (1.88 — the issue named 1.85, raised at #20) so the
    gate and baseline share a toolchain; `timeout-minutes: 20` bounds the job.
  - **`BENCH.md` §6 + doc reconcile.** Adds the regression-gate section (mechanics,
    the perf-gate block + mirror table with the four p99 baselines and thresholds,
    the runner-noise deviation, and the legitimate re-baseline procedure —
    "never to hide a regression"). `docs/06-performance.md` §5 and
    `docs/TESTING.md` §11 are updated from "before v1.0" to the ACTIVE gate and
    their stale pre-code / all-DESIGN-TARGET wording is corrected (NFR-14/15
    MEASURED, NFR-16 stays PENDING as a live-venue distribution — a fabricated
    startup number would violate the absolute no-fabricated-benchmarks rule).
    `make perf` / `make perf-selftest` targets added (out of `pre-push` — the
    bench run is minutes-long).
- **Depth sequence-gap and surface fallible-path behavioural tests** (issue #51;
  `docs/TESTING.md` §3/§7/§9, `docs/03-data-providers.md` §8/§5, ROADMAP §v0.5). The
  v0.5 acceptance-gate BEHAVIOURAL flows the #50 snapshot goldens cannot prove, added
  as Section 5 of the in-crate `src/tests_integration.rs` (they drive `pub(crate)`
  internals — `fixture_btc_depth_ladder`, `ui::depth::draw`, `ui::surface::draw` — the
  public `tests/` crate cannot reach). Tests only — **no production change**.
  - **Depth `change_id` gap → resync, driven through the REAL fan-in.** Each flow
    folds scripted grouped-book frames through `App::on_event(AppEvent::Market(
    MarketUpdate::Depth(..)))` → `apply_depth` → `DepthStore::apply`, then renders: a
    `change_id` **regression** (venue re-seed) and a **lost** sequence (`Some`→`None`)
    both flip the visible book `ResyncNeeded` → the "resyncing" badge renders and the
    ladder body dims; a forward **skip stays `Fresh`** and bright; and across a gap the
    store swaps a WHOLE snapshot (never a delta stitched across the gap — a torn book).
  - **RECONCILED gap semantics vs the stale spec text.** The 051 spec's scripted "a
    skip triggers a resync" line predates the #48 reconcile: under the GROUPED
    full-snapshot book channel a forward skip is a benign coalesced snapshot, so only a
    regression/lost sequence resyncs (`src/chain/depth.rs` `depth_continues`, the #48
    hand-off note). The tests implement the reconciled model and the divergence is
    documented in the module header and the test bodies.
  - **No-frozen-frame + bounded memory (coalescing).** A resync-needed state keeps
    producing frames (never a stale-bright frame, never a blank/panic), and a 128-frame
    book burst for one contract interleaves folds with full-frame renders — the store
    stays bounded at one book (latest-value-wins) while the render loop keeps drawing,
    proving the update path never blocks the draw path.
  - **Depth capability gate.** The Depth screen reachability reads the declared caps
    only (never a `ProviderId`): `depth: false` → unreachable and `set_screen` refuses
    it; the defensive unavailable render is the honest state, with no fabricated ladder.
  - **Vol-smile parity + surface fallible states.** The panel's built smile geometry
    (before the ratatui projection) equals `optionstratlib` `VolatilitySmile::smile()`
    output bit-exactly for a known chain (ChainView passes the upstream output through,
    it does not recompute it — `docs/TESTING.md` §9); an IV-sparse expiry renders the
    "insufficient IV" (`NoData`) empty state off the draw path, and a hard
    `CurveError`/`SurfaceError` renders the DISTINGUISHABLE "degenerate geometry"
    (`Degenerate`) state — never a corrupt chart, never a panic (the spec's "renders the
    insufficient-IV state" line reconciled to the #47 P3-01 refinement).

- **Surface / depth render goldens + the IG option-epic depth fixture** (issue #50;
  `docs/TESTING.md` §4/§5, `docs/03-data-providers.md` §8, v0.5). Locks the populated
  **and** degraded states of the three screens this milestone adds, all rendered
  through the REAL full-frame path (state -> `ViewState::sync` off the draw path ->
  `render` into a 120x40 `TestBackend`), and resolves the last `unverified` cell in
  the capability matrix.
  - **Seven 120x40 render goldens.** The vol surface (#47) in all three views —
    `surface/deribit_btc_smile` (IV smile), `surface/deribit_btc_curve` (the Greek
    curve), `surface/deribit_btc_surface` (the single-expiry surface heat map) — plus
    its `surface/surface_empty` insufficient-IV state; the Deribit depth ladder (#48)
    `depth/deribit_btc_ladder`, assembled from the committed grouped-book fixture
    through the REAL `normalize_book`, plus the first-class `depth/depth_unavailable`
    capability state for a depth-less venue; and the replay `replay/payoff_head` (#49
    reconciled scope) — an OPEN position's **expiration** curve with the mark-based MTM
    reference and the honest "not a bit-exact reprice" caveat, money rendered from
    integer cents. The companion "flat at this step" empty render is exercised in the
    same module. A `#[cfg(test)]` `fixture_btc_depth_ladder` helper (`src/providers/deribit.rs`)
    drives the grouped fixture bytes through the adapter's real normalize path.
  - **The IG option-epic depth cell resolved to `no` (evidence-on-file).** The
    `unverified` IG depth cell (`docs/03-data-providers.md` §8) is resolved: the
    committed `tests/fixtures/ig/depth/option_epic_price_snapshot.json` records the
    `ig-client` wire shape for a dated-option epic — a market-details top-of-book plus
    a Lightstreamer `PRICE` five-level DOM subscription whose depth fields arrive
    **empty** — so IG populates no order book and depth is **`no`**. Because the IG
    built-in adapter is **deferred** (#39) there is no adapter to drive it through, so
    the fixture is committed as a DATA artifact (evidence-on-file with a README); the
    shape test (`src/tests_capability_matrix.rs`) is the meaningful check without the
    adapter (the payload parses as the documented DOM shape, the five-level fields are
    unpopulated), and the on-file fixture drives the real depth path to confirm the
    `no` when #39 unblocks.
  - Tests/goldens/fixtures only — **no production change**. Noted for follow-up: the
    depth ladder's fixed 2-decimal price format under-resolves Deribit sub-unit option
    prices (the grouped fixture's `0.049`/`0.048` levels render as `0.04`); the golden
    pins this delivered behaviour so a future `src/ui/depth.rs` precision fix lands as
    a visible diff.

- **The replay payoff-at-head panel** (issue #49; `docs/04-replay-mode.md` §6,
  `docs/05-views-and-ux.md` §5, §2.1, v0.5). The Replay `Payoff` screen becomes the
  **second reachable replay screen**: it renders the payoff of the **open position at
  the scrub head** — resolved from `positions.parquet` by the timeline cursor (#33) —
  as the legs' **expiration** payoff (fully determined via `optionstratlib`
  `Profit`/`Payoff`, reusing the live payoff geometry #27 and the graph adapter #23),
  with the current **mark-to-market** reference, the break-even markers, and a zero
  line. It makes **no claim of bit-exact upstream repricing** — the bundle carries no
  step spot / IV / rate, so an exact t+0 reprice is not derivable and is never
  fabricated; the caveat is visible on the panel.
  - **The off-draw build seam** (`src/app/replay_payoff_build.rs`, new). On load and on
    every cursor move, the geometry is rebuilt from the cursor's **cached open set**
    (`open_positions`, resolved at seek time per the #33 contract — never per frame in
    `draw`) under a monotonic revision the ui view-cache diffs to re-project. Each leg's
    strike / style / expiry are recovered from the `contract_id` join key
    (`positions.parquet` carries no structured strike/style columns), reusing the
    validated #32 `parse_contract_id`. **Money stays integer cents**: the cents→
    `optionstratlib` `Positive` conversion is a checked `u64 → i64 → Decimal(scale 2)`
    path with **no `f64` on the money route**, and the net mark P&L is exact integer
    cents formatted to `$` only at the render edge. The DTE at the head is the
    deterministic, clock-free `Days` span from the head `ts_ns` to the leg
    `expiration_ns`.
  - **The panel + reachability flip** (`src/ui/payoff.rs` `draw_replay`). `ReplayScreen::Payoff`
    is now reachable (`is_replay_screen_reachable`), the `2 Payoff` number key switches
    to it, `Tab` cycles onto it, and the keybar/help overlay list it undimmed. The draw
    is **pure** — it reads only the pre-projected series and the cached header figures,
    building no `GraphData` and pricing nothing. States render first: the **"flat at this
    step"** empty state (no open position at the head — recovery is scrubbing to an open
    step), the honest "payoff unavailable" state (a degenerate/NaN-dropped curve), and
    the loading note, before the happy-path chart. The panel binds the same
    scrub/jump/playback keys as the equity screen (its `handle_key_replay`) so the
    "flat" recovery works in place.
- **The depth-ladder screen** (issue #48; `docs/05-views-and-ux.md` §2.1, §6,
  `docs/03-data-providers.md` §8, v0.5). The Live `Depth` screen renders the
  order-book bid/ask ladder for the **selected contract** — the chain cursor's
  focused strike row and call/put leg — read from a new domain depth store. It is
  **capability-gated on `depth`**, never a `ProviderId`, so it is reachable only
  where depth is verified (Deribit option instruments today) and a depth-less venue
  is skipped by `Tab` and flashes the number-key hint.
  - **The domain depth store** (`src/chain/depth.rs`, new). A bounded, per-instrument
    `DepthStore` of `DepthBook` slots keyed by `InstrumentKey`, folding each
    `MarketUpdate::Depth(DepthLadder)` latest-value-wins and hard-capped at
    `MAX_DEPTH_BOOKS` (a new instrument beyond the cap is dropped and counted, never
    unbounded growth). The **`change_id` continuity model** (`depth_continues`) sets a
    `DepthStatus`: an advancing (or repeating) sequence stays `Fresh`; a regression or
    a lost `change_id` flips to `ResyncNeeded`, which the screen surfaces as the
    "resyncing" state rather than trusting a discontinuous book. A forward skip is
    continuous — correct because the Deribit adapter streams full-snapshot frames
    (below), so a coalesced skip is never a gap. The **resync action** (re-fetching
    the snapshot) stays the adapter's job for the Deribit book path (#51).
  - **The Deribit depth subscription** (`src/providers/deribit.rs`). The book leg
    subscribes the **grouped** full-snapshot channel
    `book.{instrument}.{group}.{depth}.{interval}` (`group=none`, `depth=20`,
    `interval=100ms`) — **not** the raw `book.{instrument}.raw` delta feed. Each
    grouped frame is a *complete* aggregated snapshot, so the coalescing sink +
    latest-value-wins overwrite fold to a correct book on every frame; the raw delta
    feed would collapse through the coalescing overwrite to the last delta's one or
    two changed levels — a torn book on every normal update, on the zero-config
    venue. The `100ms` interval (never `raw`, which needs auth) keeps the
    unauthenticated default working.
  - `DepthLevel` now derives `PartialEq` (a public-surface addition on the
    `src/chain` domain type).
  - **The fold path** (`src/app.rs`). `LiveState` gains the `depth_store` slot and a
    `depth_scroll` offset; `MarketUpdate::Depth` now folds into the store (apart from
    the chain-affecting folds, so it skips the surface/t+0 rebuild) and requests a
    redraw only when the update is for the currently selected contract while the Depth
    screen is active — the busiest provider path stays off the idle-redraw budget.
    `selected_depth_key` resolves the chain cursor to the instrument the ladder is
    keyed by.
  - **The screen** (`src/ui/depth.rs`, was a placeholder). States first — the honest
    "depth not available on `<provider>`" body (a defensive capability check), the
    loading spinner, the provider-error message, the "select a contract" and "no book
    yet" empty states — then the ladder (asks worst-first down to best, then bids
    best-first, a spread footer). The ladder **opens centered on the inside market**
    (best bid/ask), so the tradeable levels are on-screen without scrolling; `↑↓ / kj`
    scroll (the chain idiom) with the offset **clamped to the viewport** — an at-limit
    or fits-entirely press changes nothing and triggers no phantom redraw. A `change_id`
    gap **or** a non-live source health (stale / reconnecting) **dims the body** and
    badges the title, mirroring the chain's stale idiom — never a bright, trusted-looking
    ladder over a stale badge. Each row carries a `bid`/`ask` **text** label so the side
    survives `NO_COLOR` (green/red only reinforce), a missing value renders `—` (never a
    fabricated `0`), and the venue instrument name is sanitized at the render edge (inert
    text, no cursor move or torn layout). The draw path is pure — no I/O, no `.await`; the
    render loop stashes the depth viewport height off the pure draw so the scroll clamp
    can couple to geometry.
  - **Keys** (`src/app/keymap.rs`, `src/ui/theme.rs`). The depth scroll binding is
    un-deferred (no longer `(v0.5)`), adds `k`/`j` alongside `↑`/`↓` (the chain idiom),
    and resolves through the one keybinding map via `resolve_depth`, so dispatch and the
    help overlay cannot drift. A new `Theme::book_side_style` gives the bid/ask sides
    their `NO_COLOR`-safe color.
- **The vol surface and smile screen** (issue #47; `docs/05-views-and-ux.md` §4,
  opens v0.5). The Live `Surface` screen renders three `optionstratlib`-sourced
  views, all built and cached in the application layer and projected **off** the
  draw path — `draw` builds no `GraphData` and prices nothing.
  - **The app-side builds** (`src/app/surface_build.rs`, new). The **vol smile**
    (`VolatilitySmile::smile()` — infallible, IV vs strike), a **Greek / IV / Price
    curve** (`BasicCurves::curve`, the `g`/`G` axis vs strike), and the
    **single-expiry surface** (`BasicSurfaces::surface`, the axis Greek/Price over
    strike × volatility — the `z` is honestly labeled a Greek/Price, never an "IV
    surface"). Reliable per-strike IV is overlaid from the #24 Greeks sidecar before
    `smile()` so a zero-IV strike never craters the wings, and a strike with no
    reliable IV is dropped (the #25 plausibility gate). Every Greek/Price build
    re-stamps the cloned chain to `ExpirationDate::Days(dte)` — a deterministic day
    count from the stored `last_full_poll`, never `Utc::now()` — resolving the #24
    clock trap so a curve/surface is a pure function of `(chain, sidecar, as_of)`
    (a determinism test asserts it).
  - **The `GraphSurface` projection arm** (`src/ui/graph.rs`). `Surface3D` projects
    to a new `ProjectedSurface` — a row-major normalized-`z` grid the screen paints
    as a `NO_COLOR`-safe glyph ramp (light → dense) — with strike / vol / `z` axis
    bounds and precomputed labels. The `Series` path is byte-untouched (its tests
    still hold); `MultiSeries` stays a deliberate `Empty(Unsupported)` (the screen
    cycles one Greek curve at a time, so no overlay shape is needed).
  - **The screen** (`src/ui/surface.rs`) with its states first (loading spinner,
    provider error, the deliberate "insufficient IV" empty state, and a
    distinguishable "degenerate geometry" state when a hard curve/surface build
    `Err` cannot price), then the smile / curve / surface views. `g`/`G` cycle the
    Greek/IV/Price axis, `x` cycles the view `smile → curve → surface`; both keys are
    un-deferred in the one keybinding map and appear in the help overlay — the `g`/`G`
    entry reads "Cycle axis (curve/surf)" (IV and price are not Greeks, and the axis
    applies to the curve/surface views). In the **smile** view the pending axis renders
    as a dim `axis: <axis> (curve/surface)` hint in the title, so `g`/`G` visibly moves
    the title without implying the smile depends on the axis. The smile / IV-curve
    `y`-axis is formatted as a percent at the render edge (IV is a fraction upstream);
    the surface honestly reports that the IV axis has no surface projection, and in the
    refused-axis state the title annotates `IV (n/a)` rather than reading as an IV
    surface. The heat-map ramp is all ink (starts at `·`), reserving a blank space
    strictly for a data gap, and its header marks the vol direction (top = high vol).
- **The v0.4 provider fixtures, the overlay pair, and the capability-matrix
  reconcile** (issue #46; `docs/TESTING.md` §5, `docs/03-data-providers.md` §8,
  `docs/specs/providers.md`). The v0.4 acceptance gate — fixture normalization
  for the landed adapters, the cross-provider overlay integration, and the
  documented capability matrix turned into an executable assertion. Tests and
  fixtures only; the default suite stays green and each gated adapter's tests run
  under its feature.
  - **DXLink symbol-stream fixtures** (`tests/fixtures/dxlink/`, new). Committed
    `quote/quote_symbol.json` + `greeks/greeks_symbol.json` at the pinned `dxlink`
    0.2.0 `QuoteEvent`/`GreeksEvent` wire shape (camelCase), each deserialized
    into the real upstream event and driven through the adapter's real
    `map_market_event` -> `route_event` decode into a `QuoteUpdate` / `GreeksRow`
    (dxlink provider, no venue time, IV carried as-is). A `README.md` documents
    the constructed-to-wire-shape rule, mirroring the Deribit fixtures.
  - **The cross-provider overlay pair** (`src/providers/dxlink.rs` tests). A
    Deribit source chain (from the committed `instruments_btc.json` fixture) is
    joined with a DXLink overlay leg through the domain `AliasCatalog`, then a
    decoded DXLink quote is folded through the **real `ChainStore` gate**:
    `overlay_matching.json` (fingerprint equals the source) merges and the overlay
    wins; `overlay_mismatched.json` (a differing `contract_multiplier`) is refused
    with the source leg kept and the leg badged overlay-refused — the
    `overlay_spec_gate` invariant end to end. The leg-selection rule (#42) is
    asserted: the overlay subscribes the dxfeed **stream** legs
    (`subscription_symbols`), never the Deribit native legs.
  - **The Alpaca spot pseudo-instrument harmlessness proof**
    (`src/providers/alpaca.rs` tests, #41). The underlying-spot fold into
    `chain.underlying_price` is out of this issue's scope and is NOT wired; a new
    store test proves the current behavior stays harmless — the `Positive::ONE`
    spot-sentinel `QuoteUpdate` **buffers** as an unknown strike and **TTL-expires**
    on a later poll, never touching a real chain row.
  - **The capability-matrix reconcile** (`src/tests_capability_matrix.rs`, new).
    A single executable table asserting every bundled adapter's live
    `ProviderCapabilities` equals its documented `docs/03-data-providers.md` §8
    row, cell by cell; the gated adapters are checked under their feature and the
    deferred `ig` row is marked N/A (stays reserved, no adapter ships).
- **The external-provider end-to-end conformance test** (`tests/integration.rs`,
  issue #45; `docs/TESTING.md` §7, `docs/03-data-providers.md` §11/§5, ADR-0006).
  Extends the `FauxProvider` conformance suite (a test-only external adapter
  built from `chainview::` public items alone, non-reserved `ProviderId("faux")`)
  so the `fetch -> store fold -> stream merge -> render` path is proven at
  **parity with a built-in through the port alone**:
  - **Domain store-fold parity.** The faux `fetch_chain` normalizes into an
    `optionstratlib` `OptionChain` and folds into the public `ChainStore`
    byte-identically to the same data seeded under a built-in id — the fold
    reads the normalized chain, never the `ProviderId` — and a streaming
    `QuoteUpdate` merges `MergeOutcome::Applied`, all through public items.
  - **Public-`render` end-to-end draw.** The faux source renders through the
    public `render` entry (the external id in the status bar, both faux strikes
    in the chain matrix body), with no built-in special-casing.
  - **Typed collision conformance.** A reserved built-in id used through the
    external `register(..)` path surfaces the typed `RegistryError::ReservedId`
    (a new deterministic case, plus the `prop_faux_registration_rejects_every_reserved_id`
    property strengthened from `is_err()` to the typed variant), companion to the
    existing `RegistryError::DuplicateId` case.
  - **Public/in-crate split documented.** The committed-golden render-parity
    proof stays in-crate (`src/tests_integration.rs`) because it needs the
    `pub(crate)` `ui::chain::draw` body and golden harness — promoting either
    would widen the semver-governed API; the external test proves parity through
    the public surface instead. Tests-only, deterministic, no network, mock
    transport, well under the 10 s bound; the four non-negotiable commands plus
    `cargo test --test arch` are clean.
- **The `capabilities_source_compat` source-compat compile gate**
  (`tests/trybuild.rs` + `tests/ui/*`, issue #44; `docs/TESTING.md` §3,
  `docs/SEMVER.md` provider-port versioning, `docs/03-data-providers.md` §2,
  ADR-0006). An executable `trybuild` gate that turns the provider-port SemVer
  promise — "adding an optional capability dimension is a source-compatible
  **minor** bump" — into a build-breaking test rather than a comment. Four
  should-pass fixtures build `ProviderCapabilities` through
  `ProviderCapabilities::builder()` (the only cross-crate construction path),
  construct capability-enum variants, and read an enum through the mandatory
  wildcard arm (the minor-bump forward-compat simulation); six should-fail
  fixtures with committed `.stderr` goldens prove a downstream struct literal
  for `ProviderCapabilities` (E0639) and an out-of-crate exhaustive match on
  each capability enum (`ChainCapability` / `GreeksCapability` /
  `OptionStreamCapability` / `ChainPollCapability` / `AuthKind`, E0004) are
  **rejected** by the compiler. The gate fails closed the day a capability type
  loses `#[non_exhaustive]` — the exact edit that would silently reclassify a
  minor bump as a major break. Fixtures name every port type through
  `chainview::` alone, so `trybuild` compiles them as separate crates against
  the built lib — the external-adapter story exactly. `SEMVER.md` and `03 §2`
  now cross-reference the gate as the executable proof of the minor rule (no
  rule change).
  - **New dev-dependency `trybuild` (1.0).** DEV-ONLY — it never rides in the
    release binary. Supply-chain audit note: all-permissive tree
    (`glob`/`termcolor`/`dissimilar` plus `serde`/`serde_json`/`toml` already
    in the graph), MIT/Apache-2.0 on the `deny.toml` allow-list, builds on the
    1.88 MSRV floor, and `cargo audit` + `cargo deny check` are clean with it
    added (no new advisory, no new license, no new source).
  - **Toolchain-pinned CI wiring.** The committed `.stderr` goldens are
    rustc-version-sensitive, so the gate is EXECUTED only in the pinned Rust
    1.88 CI job (via `CHAINVIEW_TRYBUILD_UI`) where the goldens are byte-stable;
    on the moving `stable` job (and any other toolchain) the test self-skips, so
    a benign compiler-message change never breaks an unrelated PR. The
    regeneration flow (`CHAINVIEW_TRYBUILD_UI=1 TRYBUILD=overwrite cargo +1.88
    test --test trybuild`) is documented in the harness so a message drift is a
    deliberate, reviewed golden update.
- **The delivered, documented, semver-governed public provider port**
  (`src/lib.rs`, `src/providers/mod.rs`, `src/config.rs`, issue #43;
  `docs/03-data-providers.md` §11, `docs/SEMVER.md`, `docs/07-configuration.md`
  §5, ADR-0006/ADR-0008). An external crate can now build a broker integration
  against `chainview` with no fork, naming every port type through `chainview::` (the two accepted companion
  deps: `async_trait` for the trait macro; `optionstratlib` only when the
  adapter builds an `OptionChain` itself).
  - **Complete crate-root re-export.** The port an external `impl Provider`
    compiles against is re-exported in full: the `Provider` trait, the
    `ProviderCapabilities` builder + its dimension enums, and every emitted /
    signature domain type (`ChainFetch`/`ExpirySource`/`AliasCatalog`,
    `UnderlyingRef`, `QuoteUpdate`/`GreeksRow`/`DepthLadder`/`MarketUpdate`,
    `Instrument`/`InstrumentKey`/`ContractSpecFingerprint`,
    `SubscriptionRequest`/`SubscriptionHandle`/`MarketUpdateSink`/`SinkSend`,
    `ProviderError`/`RegistryError`, `ProviderId`/`RESERVED_PROVIDER_IDS`). The
    export audit closed the last gap: the `optionstratlib` chain-model types the
    port names — `OptionChain` (the `ChainFetch.chain`), `ExpirationDate` (the
    `Provider::fetch_chain` / `UnderlyingRef` expiry type), and `Decimal` (every
    `GreeksRow` Greek) — are now re-exported at the crate root alongside the
    existing `Positive`/`OptionStyle`, so an external adapter needs no direct
    `optionstratlib` dependency to *name* the port (a chain-**producing** adapter
    still depends on it to *build* an `OptionChain`). The review closed the
    co-located `chrono` gap the same way: `DateTime`/`Utc` — the timestamp
    scalar on every emitted event/identity value, which no exported fn
    produces — are re-exported too, so constructing `QuoteUpdate`/`GreeksRow`/
    `InstrumentKey`/`ExpirySource` values needs no direct `chrono` dependency.
  - **External-integration crate docs + compile-tested doctest.** The `src/lib.rs`
    `//!` docs now document ChainView as a library with a stable provider port:
    the builder flow
    (`ChainViewApp::builder().with_builtins().register(MyBroker::new()).run()`),
    what is semver-governed (trait-signature change = **major**, new optional
    capability dimension = **minor**), the reserved ids and the reserved-id-growth
    = major rule, the `CHAINVIEW_<ID>_*` config namespacing, the external-provider
    security boundary, and dynamic-loading-out-of-scope. A `no_run` doctest
    compiles a minimal external `Provider` impl end-to-end from the `chainview::`
    surface plus the accepted `async_trait` companion — the seed for the #45 conformance test.
  - **Per-provider config namespacing for external ids.** `providers.<id>.*`
    file keys and `CHAINVIEW_<ID>_*` env keys (endpoint, refresh, credentials)
    now resolve for a non-reserved external id **identically** to a built-in,
    through the total id ↔ segment bijection; the per-provider env-var names are
    built through the single `provider_env_var` helper.
  - **Doc reconciliation.** `docs/03-data-providers.md` §11.1 (the port list) is
    reconciled with the shipped surface, the §2/§5 `subscribe` signature is
    corrected to the ADR-0009 two-class `MarketUpdateSink` (the stale
    `mpsc::Sender<MarketUpdate>` wording is dropped), §11.2's provider-id grammar
    is updated to the tightened ADR-0008 form, and `docs/SEMVER.md` records the
    port as delivered. No behavior change to adapters, the store, or the UI.
- **The standalone DXLink overlay adapter, behind a DISABLED-by-default feature
  gate** (`src/providers/dxlink.rs`, `src/providers/mod.rs`, `src/app/registry.rs`,
  issue #42; `docs/03-data-providers.md` §7.3, `docs/SECURITY.md` §2.4,
  `CLAUDE.md`). The `Provider` for DXLink: an **overlay-only** quote/Greeks feed
  with **no chain discovery** — `discover`/`fetch_chain` return
  `ProviderError::Unsupported` (`chain: None`, `option_stream: SymbolOnly`), so it
  is usable only as a symbol-level overlay onto **another** provider's chain. An
  external chain source is therefore required to select `dxlink`; a chain-less
  provider chosen as the live source is rejected by the capability-driven
  composite-source guard (`ConfigError::InvalidValue`). Streaming is
  `connect()` -> `create_feed_channel("AUTO")` -> `setup_feed(&[Quote,Greeks])` ->
  `subscribe(...)`, drained over the adapter-owned bounded two-class
  `MarketUpdateSink` behind a ChainView-owned reconnect/resubscribe loop (there is
  no chain to re-fetch — the source owns the structure and its own backfill).
  - **Shares the tastytrade dxfeed decode (the #38 second call site).** DXLink's
    typed `MarketEvent::{Quote,Greeks}` (sizes are `f64`, and there is **no** venue
    time field) is mapped onto the neutral `dxfeed_decode` views (`event_time:
    None`; `received_time` stamped at the boundary) and decoded by the SAME shared
    `decode_quote`/`decode_greeks` helpers the tastytrade adapter feeds — proving
    the neutral node with **no** `dxlink -> tastytrade` import edge (both depend on
    `dxfeed_decode`, neither on the other; enforced by the arch test). A
    crossed-tick decode error is a benign per-tick drop and an unknown symbol is
    dropped by the symbol guard — neither feeds reconnect/health.
  - **The cross-provider overlay equivalence gate.** The adapter emits
    `QuoteUpdate`/`GreeksRow` tagged `provider: dxlink` carrying the DXLink leg's
    `ContractSpecFingerprint`; the store merges a leg **iff** its fingerprint
    (multiplier, settlement, exercise, quote currency, venue product code) equals
    the source leg's. On a match the overlay wins the quote/Greek fields; on a
    mismatch the merge is a per-leg, non-fatal `OverlayError::SpecMismatch` — the
    overlay is refused, the source leg kept, and the leg badged overlay-refused, so
    two economically distinct contracts that merely share `(underlying, expiry,
    strike, style)` are never silently merged (the `overlay_spec_gate` property).
  - **Gated by construction, not discipline.** The whole adapter sits behind the
    disabled `dxlink` Cargo feature and is **excluded from `with_builtins()`**;
    it is reachable only via the explicit `with_gated_builtin`, which returns
    `RegistryError::Gated` while the gate holds. A stock binary can never enable it
    — a default `cargo build` does not even compile the adapter or pull the crate.
    Provenance for the lifter is cited in-module (`dxlink/src/client.rs` /
    `src/connection.rs`, commit `1c57a36`); the pinned `dxlink 0.2.0` already
    **redacts** the serialized `AuthMessage` token (`redact_sensitive`) and its
    `Debug` exposes only `has_token`, but — like the Alpaca gate — the gate stays
    until `docs/SECURITY.md` records the captured-log proof and flips the matrix
    cell; this issue does not lift it. It also inherits the tastytrade gate whenever
    the DXLink token is minted there.
  - **Auth injected programmatically, env-only.** `DXLinkClient::new(url, token)`
    takes the URL and token as plain arguments, so `from_env` reads the
    ChainView-namespaced `CHAINVIEW_DXLINK_TOKEN` / `CHAINVIEW_DXLINK_URL` (URL
    defaults to the production dxfeed endpoint) and builds the client directly —
    NO dotenv, NO foreign env namespace, NO global tracing subscriber. The token is
    wrapped in `Secret`, exposed only at the single client hand-off, and never
    logged or echoed in a `ProviderError` (redaction-safe by construction).
  - **New dependency (ADR-0007 audit note):** `dxlink = "0.2"`, OPTIONAL and behind
    the disabled `dxlink` feature (a normal build pulls nothing). It wraps the
    DXLink WebSocket protocol (no REST); ChainView never reimplements it.
    Supply-chain: adds `tokio-tungstenite` (native-tls) + `futures-util`, both
    already in the tree under the gated tastytrade / deribit-websocket features.
    Under `--all-features`, `dxlink` resolves at both 0.2.0 (this direct dep) and
    0.1.5 (transitive via the gated tastytrade 0.3.0) — a deny.toml
    `multiple-versions = "warn"` duplicate, never a failure. `cargo audit` + `cargo
    deny check` remain clean with the feature on (the same three transitive-only
    advisory ignores; no new source). The `dxfeed_decode` module-level `dead_code`
    allow is narrowed from `not(feature = "tastytrade")` to `not(any(feature =
    "tastytrade", feature = "dxlink"))` now that the second call site consumes the
    shared decode entry points.

- **The Alpaca adapter, behind a DISABLED-by-default feature gate**
  (`src/providers/alpaca.rs`, `src/providers/mod.rs`, `src/app/registry.rs`,
  issue #41; `docs/03-data-providers.md` §7.5, `docs/SECURITY.md` §2.4,
  `CLAUDE.md`). The `Provider` for Alpaca: the composed, **completeness-provable**
  option-chain path — `get_option_contracts` discovery (paginated, looping
  `next_page_token` until absent) -> `get_option_snapshots` hydration in bounded
  batches -> an **atomic** publish once every discovered contract is hydrated, so
  the UI never sees a half-filled chain as authoritative (the bare
  `get_option_chain`, which carries no next-page token or expiry filter, is not
  used). Snapshots carry venue Greeks/IV: `delta`/`gamma`/`iv` fold into the chain
  row, and the subscribe/backfill overlay replays per-leg `GreeksRow`s tagged
  `GreeksOrigin::Provider` (the #24/#25 seam) — ChainView never hand-rolls
  Black-Scholes for Alpaca.
  - **Gated by construction, not discipline.** Historically `alpaca-websocket`
    logged the API key and secret in its auth `debug!` (`docs/SECURITY.md` §2.4),
    so the whole adapter sits behind the disabled `alpaca` Cargo feature and is
    **excluded from `with_builtins()`**; it is reachable only via the explicit
    `with_gated_builtin`, which returns `RegistryError::Gated` while the gate
    holds. A stock binary can never enable it — a default `cargo build` does not
    even compile the adapter or pull the crates. Lifting the gate (the redaction
    release + a captured-log test + the matrix flip) is tracked in SECURITY.md and
    is out of scope here. Provenance for the lifter is cited in-module: the pinned
    `alpaca-websocket 0.6.0` already masks the key (`redact_key`) and never logs
    the secret, but this issue does not lift the gate unilaterally.
  - **Auth injected programmatically (no dotenv, no foreign namespace).** Both
    clients construct from `alpaca_base::Credentials::new(key, secret)` +
    `Environment::{Paper,Live}`, so the adapter reads ChainView-namespaced
    `CHAINVIEW_ALPACA_*` env vars and builds them directly — it never calls the
    crates' `from_env` (which would read the foreign `ALPACA_*` namespace and load
    a `.env` via `dotenv`) and installs no global tracing subscriber on
    construction (`init_logger` is opt-in and untouched), verified against the
    0.21.2 / 0.6.0 checkout. Credentials are read once via `Secret`, never logged
    or echoed in a `ProviderError` (which stays redaction-safe: category-only
    transport detail plus a non-secret HTTP status).
  - **Honest three-dimensional capabilities.** `chain: Native`, `depth: false`
    for options (crypto order books only, outside the v1 product),
    `greeks: Provided`, **`option_stream: None`** (the WS carries no option
    stream), **`underlying_stream: true`**, **`chain_poll: Poll`**,
    `auth: KeySecret` — so Alpaca's polled option chain can never be mis-badged as
    a real-time stream.
  - **Underlying stream over a ChainView-owned bounded bridge.** The upstream
    `MarketDataStream` (Trade/Quote/Bar for the underlying only) is drained into
    the two-class `MarketUpdateSink` — never handed raw to the app — the adapter
    re-runs `subscribe_market_data` on reconnect, and an upstream `Lagged` signal
    re-syncs by re-polling. A burst beyond capacity coalesces last-value-wins per
    instrument without unbounded growth (asserted). US-equity expiry resolves via
    `16:00 America/New_York` -> UTC, DST-aware.
  - **New dependencies (feature-gated): `alpaca-http = 0.21.2`,
    `alpaca-websocket = 0.6.0`, `futures-util = 0.3`** — all optional, behind the
    disabled `alpaca` feature (a default build pulls none of them). **Audit note
    (ADR-0007):** with the feature on, `cargo deny check` (deny.toml pins `all-features = true`)
    (advisories, bans, licenses, sources) and `cargo audit` are **clean** — the
    only advisories are the three already-ignored, transitive-only
    unmaintained/unsound warnings (RUSTSEC-2021-0141 `dotenv`, RUSTSEC-2024-0436
    `paste`, RUSTSEC-2026-0002 `lru`); the alpaca tree adds **no new** advisory.
    RUSTSEC-2021-0141's `deny.toml` reason now also names `alpaca-base` (the
    adapter never calls `Credentials::from_env`, so no `.env` load runs). The
    duplicate-version policy (`bans.multiple-versions = "warn"`) fires for the
    expected TLS-stack duplicates the alpaca tree pulls (e.g. two `webpki-roots`
    lines) — a warning, not a failure.
- **The tastytrade adapter, behind a DISABLED-by-default feature gate**
  (`src/providers/tastytrade.rs`, `src/providers/mod.rs`, `src/app/registry.rs`,
  issue #40; `docs/03-data-providers.md` §7.2, `docs/SECURITY.md` §2,
  `CLAUDE.md`). The `Provider` for tastytrade: the REST nested-chain snapshot that
  seeds strikes joined to tastytrade's bundled dxfeed `Quote`/`Greeks` stream
  through the per-leg alias catalog (OCC <-> dxfeed), decoded through the shared
  neutral `dxfeed_decode` helpers (#38) — no duplicate decode, no
  `tastytrade -> dxlink` import edge (the arch test confirms it).
  - **Gated by construction, not discipline.** The published `tastytrade` 0.3.0 —
    the checksum-pinned artifact ChainView resolves — logs the session token, the
    DXLink token, and the raw quote-token body at `DEBUG` (`docs/SECURITY.md`
    §2.1). The whole adapter therefore sits behind the disabled `tastytrade` Cargo
    feature and is **excluded from `with_builtins()`**; it is reachable only via
    the explicit `with_gated_builtin`, which returns `RegistryError::Gated` while
    the gate holds. A stock binary can never execute the upstream's logging — a
    default `cargo build` does not even compile the adapter or pull the crate.
  - **Auth injected programmatically (no dotenv, no foreign namespace).** Unlike a
    crate that hardcodes its own env/file loading, `TastyTradeConfig` has
    all-public fields, so the adapter builds it as a struct literal from
    ChainView-namespaced `CHAINVIEW_TASTYTRADE_*` env vars and calls
    `TastyTrade::login(&config)` directly — it never calls the upstream `from_env`
    (which would read the foreign `TASTYTRADE_*` namespace, load a `.env`, AND
    install a tracing subscriber). Credentials are read once via `Secret`, never
    logged or echoed in a `ProviderError`.
  - **US-equity expiry, DST-correct.** `16:00 America/New_York -> UTC` resolved
    DST-aware locally (EDT `20:00 UTC` / EST `21:00 UTC`), so the fixed `21:00 UTC`
    upstream helper (DST-wrong half the year) is not used; asserted on both
    March/November transition boundaries.
  - **IV source: the streamed dxfeed Greeks event.** The REST nested-chain
    snapshot carries **no** IV field, so the streamed dxfeed Greeks event is this
    provider's sole venue IV source. The published `tastytrade` 0.3.0 preserves
    that IV (`volatility: greeks.volatility`) and has no `optionstratlib`
    dependency, so there is no "conversion zeroes IV" step: the value flows through
    the neutral `decode_greeks` helper (#38) as-is (dxfeed IV is already decimal)
    and lands in the sidecar tagged `GreeksOrigin::Provider`.
  - **Robustness bypasses.** An empty venue response is `ProviderError::NoChain`,
    never a panic/OOB index (no slice is ever indexed); the adapter owns its
    reconnect/resubscribe loop and re-subscribes the full leg set off the fresh
    aliases on every (re)connect, so a leg first seen on a later poll is always
    observed (not the racy one-time sender-map clone) — proven over a mock
    transport.
  - **Honest capabilities** (`docs/03` §8 row): `chain: Native`, `depth: false`,
    `greeks: Provided`, `option_stream: ChainQuotes { verified: false }`,
    `underlying_stream: true`, `chain_poll: Poll`, `auth: UserPass`.
  - **Tests (+33, feature-gated).** Nested-chain assembly from a recorded SPY
    fixture, OCC<->dxfeed alias round-trips, DST/expiry cases (both boundaries),
    strike/streamer normalization, empty-response -> `NoChain`, the #38 view
    mapping (`i64` sizes + venue `time`, `time == 0` -> absent), streamed venue IV
    surviving to the emitted `GreeksRow` as-is, crossed/unknown-symbol benign
    drops, and the reconnect loop over a mock transport (first + later
    subscription observed; cancel stops the loop).
    The default suite stays green without the feature; gated reachability
    (`with_builtins` never registers it, `with_gated_builtin` fails) is proven by
    the existing `src/app/registry.rs` tests. The `dxfeed_decode` `#[allow(dead_code)]`
    is narrowed to `not(feature = "tastytrade")` now that this adapter consumes it;
    `clamp_symbol` and the views' opaque `symbol` field keep a per-item allow with
    a note (their only reader is the deferred tracing symbol-echo).
  - **New dependency `tastytrade = "0.3"` (optional, `tastytrade` feature) —
    ADR-0007 audit note.** `cargo audit` + `cargo deny check` (deny.toml pins `all-features = true`) (the
    `deny.toml` graph runs `all-features = true`) were run with the feature
    enabled: **advisories/bans/licenses/sources all pass**, no NEW advisory. The
    tastytrade tree adds a third source of the already-ignored **RUSTSEC-2021-0141**
    (`dotenv` unmaintained) — the `deny.toml` reason is updated to name tastytrade
    and note the adapter never calls the `.env`-loading `from_env`. Feature-on
    duplicate-version warnings (`tokio-tungstenite` 0.28/0.29 + `tungstenite`,
    from tastytrade's native-tls stack vs. `reqwest` 0.13) are the documented
    `warn`-level ban policy (upstream clients pull disjoint epochs), not failures.
- **Shared neutral dxfeed decode helpers — the v0.4 provider foundation**
  (`src/providers/dxfeed_decode.rs`, `src/providers/mod.rs`, issue #38;
  `docs/03-data-providers.md` §3/§7.2/§7.3/§12, `CLAUDE.md` module map). The
  single, neutral place dxfeed quote/Greeks decode lives, so both the tastytrade
  adapter (#40) and the standalone dxlink overlay (#42) reuse it **without** an
  adapter-to-adapter edge — both depend on THIS module, neither on the other (the
  module-map hard rule). No new crate deps.
  - **A neutral intermediate view, verified against the real upstream crates.**
    The spec assumed the two crates carry "the same event shapes"; the real
    types diverge structurally — `tastytrade` `DxfQuoteT` uses `i64` sizes + a
    `time` (ms) field, `dxlink` `QuoteEvent` uses `f64` sizes and **no** time
    (likewise the Greeks events). The module therefore depends on **neither**
    upstream crate and defines crate-internal `DxQuoteEvent` / `DxGreeksEvent`
    views both adapters map their raw event onto (exactly the "neutral
    intermediate" the spec fixes in task 2), so one decode body serves both call
    sites and no raw `dxfeed::Event` / `MarketEvent` ever crosses the seam.
  - **The checked `f64` seam (governance deviation 2).** `decode_quote` /
    `decode_greeks` reject `NaN` / `Inf` / negative before a domain value is
    built — prices/IV/sizes into `Positive`, Greeks into `Decimal`; `f64` never
    flows past this module and never touches a money field. Field policy per
    `docs/03` §3: a real **zero bid is kept** (not absent); a **zero ask on a
    non-zero bid** or `ask < bid` is **crossed** → the whole update is rejected as
    a typed `ProviderError::Normalize { OutOfRange("ask") }` (caller keeps prior);
    a per-field non-finite/negative is dropped to `None` (keeps prior); dxfeed IV
    is **already decimal**, carried **as-is** (no Deribit-style `/100`); a Greek
    may be negative and is preserved; the row is tagged `GreeksOrigin::Provider`.
  - **Redaction-safe + transport-agnostic.** A decode failure names the **field**,
    never a value; the raw symbol is carried opaque (identity resolution is the
    adapter's job) and `clamp_symbol` bounds it on a `char` boundary before any
    tracing echo (the `docs/SECURITY.md` §6 house rule). The module touches no
    I/O and stamps no wall clock — `received_time` rides on the neutral view, so
    decode is a pure, deterministic function.
  - **Tests (+26): unit + property.** Both call-site shapes decode identically for
    identical data; per-field NaN/Inf/negative rejection; zero-bid kept vs crossed
    rejection; IV carried as-is (no division); negative Greek preserved; the
    symbol clamp (incl. multi-byte); and `normalize_total` properties proving an
    arbitrary event decodes to a typed error or a valid row, never a panic. The
    arch test already classifies `dxfeed_decode` as the neutral node both future
    adapters may import (no extension needed).
- **Replay integration test + render goldens — the v0.3 acceptance gate**
  (`src/tests_replay_integration.rs`, `src/lib.rs`,
  `tests/render/golden/replay/*`, issue #37; `docs/TESTING.md` §4/§7,
  `docs/ROADMAP.md` v0.3, `docs/05-views-and-ux.md` §2.1/§5). Turns the v0.3
  ROADMAP acceptance bullets into committed, executable tests — it ADDS
  tests/goldens and changes no production behavior. New crate deps: none.
  - **End-to-end replay path, network-free + deterministic** — the committed
    `valid/` conformance fixture is opened + decoded through the **real**
    `BundleReader` (`open`+`load`, the same reader the off-thread worker runs),
    folded through the **real** `App::on_event(AppEvent::BundleLoaded(..))` (exactly
    what `spawn_bundle_load` delivers — building the `TimelineCursor` + equity
    geometry), synced off the draw path via `ViewState::sync`, and rendered through
    the **real** `render(&App, &ViewState, frame)` — the production composition minus
    the `spawn_blocking` worker thread. The rendered content (the populated equity
    chart, the as-authored attribution values, a fixture fill, and the status line)
    is asserted against the fixture's known data; the four Parquet tables round-trip
    through the reader (one row per step across all four).
  - **Six committed render goldens** at the fixed 120×40 (`tests/render/golden/replay/`,
    via the house `assert_golden`/`UPDATE_GOLDENS` harness, all produced through the
    real render path): `equity_curve.txt` (head at the last step — the full equity
    curve, attribution, and fills), `greek_attribution.txt` (a mid-run scrub head —
    the render-level scrub-consistency proof, fewer fills + a shorter curve),
    `trade_drilldown.txt` (a fill drilled into via `.`, the selected-fill detail
    panel), plus the `loading` / `error` / `empty` lifecycle states. The error golden
    is driven by the **real** reader's `UnsupportedSchema` rejection of the `bad_schema`
    fixture; the empty golden is a zero-row (degenerate) run rendering deliberate
    per-panel empty states (`—` attribution, "no equity rows", "no fills"), never a
    blank or a panic.
  - **Scrub consistency** — seeking to each step (through the real `ReplaySeek` fold)
    updates the framed title, the attribution header, the visible-fills count, and the
    head equity/greeks rows to **one coherent post-fill head** each time (the
    single-`position` invariant, observed at the render seam).
  - **Two reachable screens, no dead Payoff key** — asserted at the reachability seam
    (`is_replay_screen_reachable`: Replay yes, Payoff no) and the render seam: the
    Payoff number key (`2`) is a consumed global that flashes the "v0.5" hint and never
    switches; Tab/S-Tab cycle only reachable screens; and the keybar renders the Payoff
    slot dimmed/parenthesized (`2 (Payoff)`), never a live body.
  - **Unsupported schema rejected, not partial-rendered** — the `bad_schema` fixture
    is rejected at `open()` with `BundleError::UnsupportedSchema`, folded to a retryable
    `Error`, and rendered as the actionable error body (`! unsupported schema: …` +
    "press R to retry") with **none** of the populated panels leaking through.
  - **Load-error retry round-trip + playback tick end-to-end** — `Error → R` returns to
    `Loading` and re-issues a `Command::ReloadBundle` for the same dir; a fresh valid
    load completes the retry to `Ready`. `Space` starts playback; ticks advance the
    play-head by the speed quantum and **auto-pause at the tape end without wrapping**.
  - **In-crate, mirroring `src/tests_integration.rs`** — like the #19/#28 render
    goldens, these live in-crate because the golden harness (`assert_golden`/
    `buffer_to_text`) and the two-level key dispatch (`App::dispatch_key_global`/
    `KeyRoute`) are `pub(crate)`, not on the semver-governed surface. Every test is
    deterministic (committed fixture bytes + fixed `tick_count == 0`, no socket, no
    wall-clock read) and finishes far under the 10 s integration bound. +12 tests.
- **Replay adversarial fixtures + the HP-4 decode bench — the v0.3 security /
  performance gate** (`tests/fixtures/bundle/*`, `tests/common/bundle_gen.rs`,
  `tests/replay_bundle_fixtures.rs`, `benches/bench_replay_decode.rs`,
  `src/replay/mod.rs`, `Cargo.toml`, `BENCH.md`, `docs/adr/0011-*`, issue #36;
  `docs/TESTING.md` §6/§11/§13.3, `docs/04-replay-mode.md` §3/§5,
  `docs/06-performance.md` §3.4/§4, `docs/SECURITY.md` §6.2). New crate deps: none
  (the fixture generator + bench reuse the existing `parquet`/`arrow-array` deps and
  the `arrow-schema`/`tempfile` dev-deps).
  - **Committed adversarial corpus** under `tests/fixtures/bundle/` — deterministic,
    tiny (~100 KB total) bundles produced by the reproducible generator
    `tests/common/bundle_gen.rs` and loaded through the **real**
    `BundleReader::open`+`load` path: the shared **conformance** bundle (`valid/`,
    round-trips clean through the whole #32 chain — equity + attribution identities,
    `run_id` as an opaque key), plus `bad_schema/` → `UnsupportedSchema`,
    `missing_table/` → `MissingTable`, and the four resource adversaries
    `oversized_footer/` / `rowcount_lie/` / `truncated/` / `decompression_bomb/`
    each rejecting with its exact typed `BundleError`
    (`TooLarge`/`Invariant`/`Parquet`) at a **pre-materialization** stage. Money is
    integer cents throughout; a test asserts no `f64` on the decode path except the
    analytic `drawdown` ratio.
  - **Positive bounded-reject proofs** — three new `src/replay/mod.rs` unit tests
    drive `scan_table` with a probe decoder and assert each adversarial shape
    (bomb / over-ceiling footer / `row_counts` lie) rejects **without invoking the
    decoder** and with `budget.used() == 0` (reusing the #30 working-set machinery),
    so the reject provably never materializes the hostile payload.
  - **HP-4 `bench_replay_decode`** — a `criterion` + `hdrhistogram` (p50/p99/p99.9)
    bench of the open+decode+validate path over a 20 000-step (80 000-row)
    conformance bundle, gated behind the `bench` feature; the measured baseline +
    environment + coordinated-omission disclosure are recorded in `BENCH.md`.
  - **ADR-0011 (Proposed)** records the `fills.position_id → positions.position_id`
    referential gap (§5 does not require it; §6 drill-down joins on it): the check is
    **not** added unilaterally (it would false-reject until IronCondor's writer
    freezes), and the drill-down is proven to **degrade gracefully today** on a
    dangling `position_id` (the detail panel reads the fill's own columns;
    `open_positions` never fabricates a leg) by the committed
    `dangling_position_id/` fixture + tests.
- **Replay screen: equity, attribution, drill-down** (`src/ui/replay.rs`,
  `src/app/replay_view.rs`, `src/app.rs`, `src/ui/view.rs`, `src/ui/mod.rs`,
  `src/ui/driver.rs`, `src/app/keymap.rs`, issue #35; `docs/05-views-and-ux.md` §5/§6,
  `docs/04-replay-mode.md` §4/§6). Replaces the `Ready` placeholder with the real
  replay body at the scrub head, rendered as a pure function of app state (no I/O, no
  `.await`, no `GraphData` build in `draw`). New crate deps: none.
  - **Equity curve + drawdown** — the as-of equity slice (step → equity **cents**) is
    projected off the draw path into a `GraphData::Series`, cached on `LoadedReplay`
    and re-projected via `ViewState::sync` on an `equity_revision` bump (the #27
    revision-diff pattern, extended to replay). The line chart marks the scrub head
    and the panel shows the exact **peak drawdown** in `$` plus the authored drawdown
    ratio (`NaN`/`Inf`-guarded to `—`). The series is stride-sampled to a bounded
    point count so the draw stays `O(rendered)`, not `O(full backtest)`.
  - **P&L attribution panel** — the head `GreeksAttribution` row (Θ/Δ/ν/spread
    capture/fees/residual) rendered **as authored, never recomputed**, with `+`/`−`
    sign glyphs, cents→`$` magnitudes, and a magnitude-proportional bar; an absent
    head row renders every term as `—`, never a fabricated `0`.
  - **Trade drill-down** — `,` / `.` step the previous/next fill; the selection lives
    on `LoadedReplay.selection` and **follows the visible fills at the cursor**
    (clamped, cleared when it scrubs out of the as-of window). The selected fill's
    detail (contract, side, qty, price, fees, slippage, mode) renders in a panel — the
    fill's own position context, never a per-fill Greek split the bundle cannot
    derive. The `,` / `.` keymap entries are **un-deferred** (real bodies + truthful
    overlay); the selection step mutates in-memory state and the render loop's
    view-signature diff schedules the redraw.
  - **States first** — the #34 loading/error rendering stays; an empty run (zero rows /
    zero fills) renders deliberate per-panel empty states, never a blank or a panic.
  - **Money edge** — a single `fmt_cents_abs` seam converts integer cents to `$` at
    the widget only (thousands-grouped, two decimals, checked integer arithmetic, no
    `f64` money); the equity axis labels are the sole `$` derived from the projection's
    `f64` plot bounds (display geometry), guarded for non-finite.
  - **Review-pass fixes** (ux + architect). **Off-window selection indicator** — the
    drill-down selection walks the whole as-of tape but the fills list renders only its
    recent-tape window, so a selection stepped **above** the window kept updating the
    detail panel while its highlight silently vanished; now the TOP list row is
    replaced with a dim `▸ selected fill ↑ (step N)` indicator (the window anchors the
    newest fill at the bottom, so a selection can only leave via the top), still
    `O(rendered rows)`. Full scroll-to-selection stays deferred to the v1.0 polish pass
    (#57). **Axis thousands grouping** — the equity y-axis whole-dollar labels reuse the
    attribution `group_thousands` grouping (`$1000000` → `$1,000,000`, wider gutter
    accepted; a negative bound carries the shared U+2212 `−`). **Narrow-width
    attribution honesty** — the attribution row is budgeted label → sign → amount → bar,
    so the magnitude **bar drops first** at narrow widths to keep the amount room, and
    an amount that still cannot fit is elided with a trailing `…` (never a bare,
    misreadable `$1` prefix). **`−` glyph consistency** — `fmt_drawdown_ratio` now
    formats the magnitude `.abs()` behind the shared `pnl_sign_char` glyph, so a
    negative drawdown reads with the same U+2212 `−` as every other signed value (never
    an ASCII `-`). Docs: the fees attribution row + its −$-contribution convention are
    recorded in `docs/05` §5, `docs/04` §6, and the milestone; the `visible_greeks`
    docstring + `docs/04` §4 drop the "cumulative/summed" wording (the panel renders the
    head-step row as authored; the slice is currently unused). +3 tests (the off-window
    indicator + its in-window counter-case, the narrow-width elision); two existing
    formatter tests updated for the `−` glyph + the axis grouping.

- **Replay state machine + the `replay` subcommand** (`src/app.rs`,
  `src/app/replay_load.rs`, `src/event.rs`, `src/main.rs`, `src/ui/replay.rs`,
  `src/ui/mod.rs`, `src/ui/theme.rs`, `src/app/keymap.rs`, `src/lib.rs`, issue #34;
  `docs/02-tui-architecture.md` §3/§4, `docs/04-replay-mode.md` §3/§4,
  `docs/07-configuration.md` §4.1). Wires replay
  mode into the application: the bundle-load lifecycle, the CLI subcommand, and the
  seek/playback/tick fan-in. The render loop stays pure — the load runs off-thread
  and its outcome arrives as an event; `draw` reads state only. New crate deps: none.
  - **`ReplayState` filled in** — `BundleLoad { Loading, Ready(Box<LoadedReplay>),
    Error { message } }` and `LoadedReplay { bundle: LoadedBundle, cursor:
    TimelineCursor, selection: Option<Fill> }`. `apply_load_result` folds
    `Loading → Ready`/`Error`; `begin_reload` resets to `Loading` on `R`; `seek`
    moves the in-memory cursor; `apply_control` folds play/pause/speed; and
    `advance_playback` steps the play-head one tick, **auto-pausing at `end_step`**
    so the loop parks instead of spinning.
  - **`chainview replay <dir>` subcommand** — the one canonical spelling (no
    `--replay` flag). A missing / non-directory bundle path is a **friendly, pre-TUI
    CLI error** (`validate_replay_dir`) naming only the passed path; a present-but-
    malformed bundle becomes a retryable in-TUI `BundleLoad::Error` instead. `replay`
    ignores the live-only flags (config layer drops them).
  - **Off-thread load worker** — `spawn_bundle_load` runs `BundleReader::open_with_
    ceilings` + `load_cancellable` on a `spawn_blocking` worker with the supervisor's
    `CancellationToken` adapted as the `&|| token.is_cancelled()` probe (the #30
    seam), delivering `AppEvent::BundleLoaded(BundleLoadResult)`. A cancelled load
    emits nothing (shutdown); a failure carries only the non-secret `BundleError`
    text. Threads default `ResourceCeilings`.
  - **Event fan-in** — new `AppEvent::{ReplayControl, BundleLoaded}` and
    `ReplayControl { PlayPause, SpeedFaster, SpeedSlower }` / `BundleLoadResult
    { Loaded(Box<LoadedBundle>), Failed(String) }`. `ReplaySeek` now folds directly
    into the in-memory cursor (no I/O), so the dead `Command::SeekBundle` is
    **removed**; `ReloadBundle` remains the sole replay command. Every closed-set
    match site (`on_event`, `fold_event`, the wildcard-free fence) revisited.
  - **Playback collapse** — the `app::Playback` stub and the domain `replay::Playback`
    are now a **single** `Playback` (the richer #33 shape, `Playing { speed:
    PlaybackSpeed }`); the transitional `ReplayPlayback` crate-root alias is dropped
    and the bare `Playback` is exported from the crate root.
  - **Keymap un-deferrals** — `Space` (play/pause), `+`/`-` (speed), and `End`
    (jump-to-end) are no longer `deferred` now their bodies exist; the fill
    drill-down (`,` / `.`) stays deferred to its render (#35+). `ReplayScreen::Payoff`
    stays unreachable (v0.5).
  - **Tests** — CLI parse (`replay <dir>` → Replay, no subcommand → Live, missing
    dir → friendly error, missing/extra positional → clap error, live-only flags
    ignored); load transitions (`Loading→Ready`/`Error`, `Error→R→Loading→Ready`);
    seek fold (moves the cursor, clamps, no command); playback fold (play/pause,
    speed clamps, tick advances only while playing, auto-pause at end); the load
    worker (missing dir → `Failed`, pre-cancelled → no event); and the two-level
    replay dispatch end to end. Net +21 tests (25 added, 4 rewritten/renamed).
  - **Review-pass fixes** (ux + architect). The replay `draw` now renders the
    **bundle-load lifecycle** instead of always painting the `Ready` placeholder:
    `BundleLoad::Loading` shows the centered §6 spinner + "loading bundle `<run>`…",
    `BundleLoad::Error` shows the bounded, wrapped message + a discoverable "press
    `R` to retry" affordance (glyph-prefixed, `NO_COLOR`-safe), and `Ready` keeps the
    deliberate #35 hand-off placeholder — so a malformed bundle is no longer an
    invisible failure (`draw` gained `theme`/`tick`, mirroring `chain::draw`). The
    replay status bar gains a distinct playback badge — `▶ ×N` while playing, `⏸`
    while paused over a loaded bundle — so "playing" no longer reuses the loading
    spinner glyph (the spinner is now Loading-only; the Loading-or-Playing redraw
    predicate `App::is_in_motion` is unchanged, so the parking invariant holds). The
    help overlay is now truthful in both modes: the `+`/`-` speed labels note "(while
    playing)", the fill drill-down defers to `soon` (not `v0.3`, which reads as a
    future release while replay *is* v0.3), and `r` (Reconnect) is scoped out of the
    **replay** overlay where it is a no-op (the binding stays in `KEYMAP`; only the
    documentation is scoped). Docs: `docs/02-tui-architecture.md` §4 adds
    `AppEvent::{ReplayControl, BundleLoaded}` and drops the dead `Command::SeekBundle`
    (a scrub folds in-memory); `docs/04-replay-mode.md` §4 records the seek-while-
    playing rule. +9 tests (the three draw states + small-size fuzz, the playback
    badge + loading-spinner-only guard, the two overlay-truthfulness fixes, and a
    wildcard-free `ReplayControl` fence).
- **Replay domain types and the manifest schema** (`src/replay/mod.rs`, issue #29;
  `docs/01-domain-model.md` §9, `docs/04-replay-mode.md` §2, ADR-0004). Opens v0.3
  (replay mode) by fixing ChainView's typed, **read-only** views of the IronCondor
  result bundle — the shapes the reader (#30), the Parquet decoders (#31), and the
  validation chain (#32) are all written against. Types + serde + docs + tests
  only; **no I/O, no Parquet decode, no validation** (open/load are stubs).
  - **`BundleManifest`** — `schema`, opaque `run_id` (an uninterpreted `String`
    with no recompute helper), `created_utc`, `code_version`, `lockfile_sha256`,
    `seed: u64`, opaque `config`/`strategy`/`data_source`/`metrics` as
    `serde_json::Value`, and typed `row_counts: BTreeMap<String, u64>`. Parsing is
    **permissive** (no `deny_unknown_fields`) so a newer minor still opens.
  - **`CapitalConfig { capital_cents: i64 }`** — the one narrow typed projection of
    the `config.capital_cents` field, reachable via `BundleManifest::capital_config`;
    an absent field errors rather than silently defaulting to `0`.
  - **The four typed rows** — `Fill`, `EquityPoint`, `PositionRow`,
    `GreeksAttribution` — with every money field as integer cents (`i64`/`u64`);
    the **only** `f64` is `EquityPoint::drawdown` (an analytic ratio), asserted by
    a module grep test. Their `deny_unknown_fields` is scoped to the JSON
    round-trip/test path — the #31 Parquet decode reads by column projection and
    stays permissive toward unknown extra columns (`docs/04` §3), so the bundle
    contract is not narrowed.
  - **Closed-set enums** `PositionSide { Long, Short }` / `ExecMode { Naive,
    Realistic }` with `snake_case` wire strings; `style` reuses
    `optionstratlib::OptionStyle` mapped to the bundle's `call`/`put` wire form. An
    unknown enum string is a deserialization error, never a silent fallback.
  - **`contract_id` grammar constants** — `CONTRACT_ID_FORMAT`,
    `CONTRACT_ID_VERSION_PREFIX`, `CONTRACT_ID_UNDERLYING_PATTERN` — the fixed
    format the round-trip check (#32) enforces; the parser is #32's work.
  - **`BundleReader`/`LoadedBundle`** signatures so #30/#31/#32 compile against a
    stable surface: `open` records the bundle root with no filesystem access
    (read-only by construction), `load` returns the typed `BundleError::NotImplemented`
    placeholder — **no reachable panic**, `todo!()`, or `unimplemented!()`.
  - **Dependency:** promotes `serde_json` (`version = "1"`) to a **direct**
    dependency for the opaque manifest blobs. Supply-chain audit note: **no new
    surface** — `serde_json` (currently `1.0.150`) is already in the transitive tree
    via `deribit-http`/`deribit-websocket`; this only names an existing crate
    directly (ADR-0007).
- **Bundle reader + resource ceilings — the v0.3 untrusted-input hardening spine**
  (`src/replay/mod.rs`, `src/error.rs`, issue #30; `docs/04-replay-mode.md` §3/§5,
  ADR-0010). Lands the real `BundleReader::open`/`load` bodies against #29's type
  shapes: a **read-only** reader that validates the manifest, schema-gates it, and
  enforces the three resource ceilings via a **measured, batched, cancellable**
  Parquet decode — a malformed or oversized bundle is a typed `BundleError`, never
  a panic or an unbounded allocation. The typed per-column decode (#31) and the
  cross-table validation chain (#32) are still stubbed at a documented seam
  (`load` runs the ceilings and returns a `LoadedBundle` with empty tables).
  - **`BundleReader::open`** — verifies the directory exists, **stat-checks
    `manifest.json` against its own byte ceiling** (`MAX_MANIFEST_BYTES` 8 MiB)
    **before reading it** so a giant `manifest.json` (a *manifest bomb*) is
    `TooLarge` pre-read rather than an OOM, parses it **permissively**,
    **schema-gates** it (`manifest.schema` must equal `SUPPORTED_SCHEMA` =
    `"ironcondor.bundle.v1"`, else `UnsupportedSchema`), checks the `row_counts`
    **fixed four-key shape** (`Invariant` otherwise), and confirms the four Parquet
    files are present. The operator-supplied `ResourceCeilings` are `validate()`d
    **on this enforcement path** (a misconfigured knob such as `max_batch_rows: 0`,
    which would silently disable the measured guard, is a typed `BundleError::Config`,
    never a silent open). Read-only by construction — it only stats and reads.
  - **The three ceilings**, each rejecting **before** the offending allocation:
    **Ceiling 1** stats each file pre-open (`MAX_TABLE_BYTES` 512 MiB); **Ceiling
    2** reads the Parquet footer row count pre-decode (`MAX_TABLE_ROWS` 5,000,000)
    and cross-checks it against `row_counts` (mismatch → `Invariant`); **Ceiling 3**
    is a footer pre-check (`DECODED_OVERHEAD_PERMILLE` 1500 = 1.5×, plus a
    `MAX_EXPANSION_RATIO` 20× decompression-bomb reject) followed by a
    **measured per-batch** tally (`MAX_BATCH_ROWS` 65,536, `MAX_BATCH_BYTES`
    256 MiB) into the `WorkingSetBudget`'s running `used` total across all four
    tables, checked after every batch — the decode stops with `TooLarge` the moment
    the total *would* exceed `MAX_WORKING_SET` (2 GiB), so `used` stays strictly
    under the ceiling on the reject path (asserted by a unit test). `MAX_BATCH_BYTES`
    is a **post-materialization** reject, so the true transient peak is ~one batch.
    `manifest.row_counts` is an integrity **hint** — it never sizes an allocation.
  - **Checked conversions** — every footer integer crosses into a Rust size via
    `try_from`/`checked_mul` (never an `as` cast); a negative or overflowing footer
    value is `TooLarge`, not a giant allocation.
  - **Cancellable decode** — `load_cancellable(&dyn Fn() -> bool)` polls the probe
    at every batch boundary and returns `BundleError::Cancelled` promptly. The
    probe is a plain closure (the app seam adapts `&|| token.is_cancelled()` from
    its shutdown token), so `src/replay/*` stays **free of `tokio`** and the
    layering arch test stays green.
  - **`ResourceCeilings`** config knobs with documented `MAX_*` defaults (now
    including `MAX_MANIFEST_BYTES`) and a `validate() -> Result<(), ConfigError>`
    check, called both at startup **and on the `open` enforcement path** (out-of-range
    → typed error, never a panic). Wiring a CLI/env/file override into `Config` is
    deferred to the config surface; the defaults are always valid.
  - **`BundleError`** — added `Io` (a non-secret filesystem-failure summary),
    `Cancelled`, and `Config(#[from] ConfigError)` (a misconfigured ceiling on the
    enforcement path, thiserror `#[from]`-consistent with the other sub-boundaries);
    **removed** the #29 `NotImplemented` placeholder now that `load` has a real body.
    The attacker-supplied `manifest.schema` tag echoed by `UnsupportedSchema` is
    **clamped to a bounded length at construction** (64 chars + ellipsis, on `char`
    boundaries — no multi-byte panic), so a junk tag cannot bloat the message; the
    type-doc records it as attacker-supplied-but-non-secret, clamped, and further
    sanitized at the render edge.
  - **Dependency:** adds **`parquet = "59"`** (arrow-rs) with
    `default-features = false, features = ["arrow", "snap", "zstd", "lz4"]` — no
    `async`, no `object_store` (ADR-0010). MSRV `1.85` (verified under our `1.88`
    floor with `cargo +1.88 check`). Dev-only test-writer deps `arrow-array` /
    `arrow-schema` (matched to parquet 59) and `tempfile`. **Supply-chain audit
    note:** `cargo audit` exit 0 (**no new advisory** — the only overlap is
    `paste`, an already-ignored path); `cargo deny check` → advisories/bans/
    licenses/sources **ok** (arrow/parquet resolve to a single 59.1.0 epoch, all
    licenses already allow-listed).
- **Typed Parquet table decoders — the replay wire→domain boundary**
  (`src/replay/tables.rs`, `src/replay/mod.rs`, `src/error.rs`, issue #31;
  `docs/04-replay-mode.md` §2.2/§3/§5, ADR-0004). Replaces the #30 empty-tables
  seam: `BundleReader::load` now decodes the four tables into populated `Vec`s of
  the #29 row types **in file order** (the reader never sorts) — money stays integer
  cents, and the decode runs **inside** #30's batched, measured, cancellable loop
  (all #30 ceiling/cancellation guarantees preserved).
  - **Four per-batch decoders** — `read_fills` / `read_equity` / `read_positions`
    / `read_greeks` (`pub(crate)`; the public surface stays `BundleReader::load`).
    Each looks the documented columns up **by name** and appends its rows into the
    eager `Vec` the working-set budget accounts for, **in file order — the reader
    never sorts**. Each table's stated sort key (`fills` by
    `(step, order_id, fill_seq)`, `positions` by `(step, position_id)`,
    `equity_curve` / `greeks_attribution` by `step`) is a WRITER guarantee the #32
    validation chain verifies (`Invariant` on violation), not a repair the reader
    performs — repairing it on load would silently mask the exact writer bug #32
    must reject.
  - **Permissive to unknown columns, strict on the contract** — an **extra**
    column is ignored (a newer minor of the same schema tag still decodes, §3);
    a **missing required** column or a **wrong Arrow/Parquet type** is the new
    typed `BundleError::Schema` (naming the table + column + expected/actual type,
    no bundle payload). Both the column name and the actual type — the latter the
    `{:?}` rendering of the UNTRUSTED footer `DataType`, which a deeply-nested type
    renders long — are routed through `clamp_echo` (64 chars + ellipsis), so the
    error Display stays bounded regardless of the footer schema.
  - **Checked wire→domain narrowing, never an `as` cast** — every unsigned-domain
    field (`step`/`fill_seq`/`quantity`, the ids, and the non-negative cents
    fields, incl. `fees_cents` which IronCondor keeps `i64`) crosses via
    `u32`/`u64::try_from`; a negative value is `BundleError::Invariant` naming the
    table + column + row (tested per field), and a large id at the signed-wire max
    decodes losslessly. `slippage_cents` and the attribution terms stay signed
    `i64`; `drawdown` is the **only** `f64` and is guarded non-finite
    (`NaN`/`±∞` → typed error, never a stored `NaN`). `exit_reason` is the one
    nullable column (→ `Option<String>`); a NULL in any other column is a typed
    error. Unknown enum wire strings (`style`/`side`/`mode`) are typed errors with
    the offending value clamped; string columns are read as plain `Utf8` **or**
    `Dictionary<Int32, Utf8>` defensively (§2.2 is silent on page encoding).
  - **Ceilings stay in charge** — each batch is decoded only after it passes the
    `WorkingSetBudget`; capacity is grown from ACTUAL decoded batch sizes (never
    pre-reserved from the untrusted `row_counts` hint), the cancellation probe
    still fires at every batch boundary, and the ACTUAL decoded row total is
    cross-checked against the Parquet footer count (mismatch → `Invariant`).
  - **`BundleError`** — added `Schema(String)` for a missing/wrong-typed column
    during the typed decode (ChainView-authored, non-secret, bounded, dynamic
    echoes clamped).
  - **Dependency:** promotes **`arrow-array = "59"`** from a #30 dev-dependency to
    a direct dependency (the production decoders read its array types). **No new
    crate** enters the graph — `arrow-array` is already `parquet`'s transitive dep
    and was already a dev-dep, so this is a "no new surface" promotion (same
    pattern as #29's `serde_json`; ADR-0010). `arrow-schema` stays a dev-dep (only
    the `#[cfg(test)]` writers build schemas). **Supply-chain audit note:** no new
    advisory — the crate was already vetted in the tree.
- **Bundle validation chain + the equivalence oracle — the second half of `load()`**
  (`src/replay/validate.rs`, `src/replay/mod.rs`, issue #32;
  `docs/04-replay-mode.md` §5/§2.3, ADR-0004). Runs the full **post-decode
  validation chain** (`docs/04` §5 steps 4–10) over the file-order tables #31
  decodes, so a malformed bundle is a typed `BundleError::Invariant` naming the
  offending table + row (never a panic, never a partial read); adds the cross-repo
  **equivalence oracle** IronCondor and ChainView share. New crate deps: none.
  - **The §5 chain, in documented order** (`run_validation_chain`, run by `load`
    after decode; a cancellation requested during/after decode still aborts before
    the O(n) passes): (4) each table **non-decreasing on its stated sort key** with
    the uniqueness sub-checks (`fills` unique on `(step, order_id, fill_seq)`,
    `positions` ≤ 1 row per `(position_id, step)`) — non-vacuous because #31
    preserves file order; (5) the integer **equity identity**
    `equity_cents == cash_cents + position_value_cents` (tolerance zero); (6) the
    typed **`CapitalConfig`** projection (present, integer, `>= 0`) + the
    cross-table **attribution identity** `theta+delta+vega+spread_capture−fees+
    residual == step_pnl`, with `step_pnl` the producer's own equity delta (step 0
    vs `capital_cents`) — `|residual|` is **advisory**, never a load failure; (7)
    the contiguous 0-based **step domain** shared by `equity_curve`/
    `greeks_attribution`, every `fills`/`positions` step inside it, and per-step
    `ts_ns` equality across all four tables; (8/9) **referential integrity** —
    `fills.strategy_run_id == manifest.run_id`, the `contract_id` round-trip against
    each `fills` row's structured columns, stable `position_id → (trade_id,
    contract_id, side)`, and the delimiter-safe `UNDERLYING` grammar
    (`^[A-Z0-9._]{1,32}$`) validated **before** any join-key split; (10, value-domain
    checks deferred to #32) `quantity > 0` and `drawdown <= 0`.
  - **`contract_id` parser** (module-private, per the #29 "parsing lives in #32"
    note) — splits `"v1:{UNDERLYING}:{expiration_ns}:{strike_cents}:{C|P}"` into its
    five fields, rejecting the wrong field count, an unsupported version prefix, an
    out-of-grammar `UNDERLYING`, non-numeric `expiration_ns`/`strike_cents`, or a
    non-`C|P` style. In `fills` the parsed fields must **round-trip** against the
    row's own `underlying`/`expiration_ns`/`strike_cents`/`style` columns (a mismatch
    is `Invariant`); `positions` carries no structured columns (§2.2), so the
    round-trip there degenerates to a well-formedness parse and the contract's
    consistency is enforced by the `position_id` stability check instead.
  - **The equivalence oracle** `compare_bundles(a, b) -> Result<(),
    BundleDivergence>` (**pub**, for the cross-repo agreement check) — money columns
    compared **exactly** (integer cents); the one analytic float `drawdown` under the
    combined tolerance `|a−b| ≤ max(ABS_TOL, REL_TOL × max(|a|,|b|))` with signed
    zero equal, `NaN` never equal, `±∞` equal only to the same infinity; each table
    compared in its stated sort-key order (copies sorted **only for comparison** —
    `load` never sorts); the manifest as canonical JSON with `created_utc` and the
    opaque `metrics` excluded. Reports the **first** divergence as a typed
    `BundleDivergence { table, column, row, detail }` (bounded, non-secret). The
    tolerance constants **`ORACLE_ABS_TOL = 1e-9`** / **`ORACLE_REL_TOL = 1e-6`**
    live beside the oracle and **must match IronCondor's copy exactly** (`docs/04`
    §5, `docs/TESTING.md` §6).
  - **Public surface** gains only `compare_bundles`, `BundleDivergence`,
    `ORACLE_ABS_TOL`, `ORACLE_REL_TOL` — the validation checks and the `contract_id`
    parser stay module-private, and the reader surface is unchanged (the chain is
    wired inside `load`). Read-only, `#![forbid(unsafe_code)]`-clean, `tokio`-free
    (the arch layering test stays green); every echoed dynamic string routes through
    the shared `clamp_echo` (moved to `replay/mod.rs` so decoders + validation share
    it); every sum is `checked_*`; no `as` casts. The #31 conformance fixture's
    `greeks_attribution` residual now absorbs the step-`pnl` remainder so it satisfies
    the attribution identity end-to-end.
  - **Tests:** a conformant bundle passes the whole chain; each violation fires its
    exact typed error (out-of-order/duplicate keys per table, broken equity/
    attribution identities, missing/non-integer/negative `capital_cents`, step-domain
    gap + `ts_ns` disagreement + mismatched span, `run_id`/`contract_id` referential
    failures, colon-bearing `underlying`, unstable `position_id`, zero quantity,
    positive drawdown); an advisory large-`|residual|` bundle **loads**. Property
    tests: `attribution_identity_holds` (generated well-formed tables), the
    `contract_id` round-trip (valid components → parse → equal fields), and the
    oracle-tolerance symmetry; plus oracle reflexivity, sort-insensitivity, and
    single-field-mutation detection across tables/types (money, enum, float, string,
    and the excluded `created_utc`/`metrics`).
- **Timeline cursor and scrubbing model** (`src/replay/timeline.rs`,
  `src/replay/mod.rs`, `src/lib.rs`, issue #33; `docs/04-replay-mode.md` §4,
  `docs/01-domain-model.md` §10). Adds the read-only scrub model over the validated
  `LoadedBundle` #32 produces — the domain the replay screen (#35) and the deferred
  payoff-at-head (#49) both read. Deterministic (no wall clock, no RNG), domain-pure
  (no `ui`/`app`/`provider`/`tokio` import — the arch layering test stays green).
  New crate deps: none.
  - **`TimelineCursor`** — a `Copy` value holding only the scrub `position`,
    `end_step`, and one per-table index (`fills_ix`/`equity_ix`/`positions_ix`/
    `greeks_ix`) into the borrowed tables; it copies no row. `seek(SeekTo, &LoadedBundle)`
    resolves each index to the invariant "count of rows with `step <= position`" by
    two paths over the one integer `step` clock (`ts_ns` is display only): an
    **incremental** `SeekTo::StepBy` walks each index ±1 from its current value
    (O(rows moved), no rescan — a single step is O(1)) and an **arbitrary**
    `SeekTo::Step` binary-searches via `slice::partition_point` (O(log n)); both land
    on the same index, so the result depends only on the final clamped `position`.
    The `SeekTo` shape is consumed from `crate::event` (one shared type behind
    `AppEvent::ReplaySeek`, wired by #34).
  - **Post-fill open-position set** — `open_positions(&LoadedBundle)` is the single
    algorithm for open positions, selection, and payoff: the latest non-terminal
    `positions` row per `position_id` with `step <= position`, **excluding** any
    `position_id` whose terminal (`exit_reason`-bearing) row is at `step <= position`.
    A same-step open + close resolves deterministically (the terminal row wins the
    exclusion); an `open_at_end` leg with no terminal row stays open through
    `end_step`. Ordered by `position_id`, borrows into the bundle.
  - **As-of accessors** (all `#[must_use]`, all borrowed) — `visible_equity` /
    `visible_greeks` / `visible_fills` / `visible_positions` up to the head step,
    `head_equity` / `head_greeks` (the `step == position` row), and `head_fills` (the
    fill(s) at exactly the head step, for the drill-down). Every slice reflects one
    consistent `position`.
  - **Playback model** — domain `Playback { Paused, Playing { speed } }` +
    `PlaybackSpeed { X1, X2, X5, X10 }` (`multiplier`/`quantum`/`faster`/`slower`,
    clamped — no wrap). `Playback::tick_seek()` yields `SeekTo::StepBy(quantum)`; the
    cursor's clamp makes playback **stop at `end_step`** without wrapping. This models
    the quantum and the stop-at-end rule only — the tick cadence is #34's.
  - **#32 invariants relied on (never re-validated)** — dense contiguous `0..N`
    `equity`/`greeks` sharing `N` (`end_step`, `head_equity`/`head_greeks`),
    step-sorted tables (`partition_point` + the incremental walk), every
    `fills`/`positions` step in `0..N`, and stable `position_id` / positive `quantity`
    (`open_positions`); each reliance is documented at its use site.
  - **Re-exports** — `crate::replay::{TimelineCursor, Playback, PlaybackSpeed}`; the
    crate root exposes `TimelineCursor` + `PlaybackSpeed`, plus the domain `Playback`
    under the **transitional alias `ReplayPlayback`** (a bare `Playback` at the crate
    root would collide with the `app::Playback` stub). #34 (app-state wiring)
    reconciles the app stub with the domain type into a single `Playback`.
  - **Tests** — unit: construction at step 0, seek to mid/last, out-of-range and edge
    clamps (never a panic), `Home`/`End`, incremental-vs-arbitrary agreement on sparse
    tables, the no-rescan move count, as-of slice consistency, head-fills at/without a
    fill on the head step, the full open-position matrix (before open, latest
    non-terminal, terminal exclusion, same-step open+close, `open_at_end`), and empty
    / single-step degenerate runs. Property: `incremental_equals_arbitrary` (the
    binding property), `seek_lands_on_last_le_step` (index == `partition_point`, in
    bounds), `seek_is_idempotent`, and `step_then_back_returns`.
- **v0.2 acceptance gate — payoff goldens, break-even / max-P&L parity, and the
  computed-Greeks tolerance fixture** (`src/ui/payoff.rs`, `src/ui/chain.rs`,
  `src/providers/deribit.rs`, `tests/render/golden/payoff/`, issue #28; docs
  TESTING §3–§5, §9, ROADMAP v0.2). Turns the v0.2 ROADMAP acceptance bullets
  into committed, executable tests — it ADDS tests/fixtures/goldens and changes no
  production behavior.
  - **Payoff render goldens at the fixed 120×40**
    (`tests/render/golden/payoff/iron_condor_expiration.txt`, `iron_condor_t0.txt`,
    `payoff_empty.txt`, `payoff_invalid.txt`), built from a committed **iron
    condor** (short put spread + short call spread) through the REAL path
    (builder → commit → `active_graph_data` → `project` → `payoff::draw` into a
    `TestBackend`), rendering both `CurveMode`s plus the empty "add a leg" and the
    invalid "no mark" states so each looks deliberate.
  - **Break-even + max-profit / max-loss parity vs `optionstratlib`.** For the
    SAME iron condor, ChainView's rendered break-evens (`PayoffBuilder::break_even_points`)
    and its max/min of the committed expiration series MATCH the `optionstratlib`
    `CustomStrategy`'s own `get_break_even_points` / `get_max_profit` /
    `get_max_loss` — an **exact** match (break-evens 92 / 108, max +3, loss 2) well
    inside the tight, documented `BREAK_EVEN_TOLERANCE` (0.02) / `MAX_PNL_TOLERANCE`
    (0.01) constants. ChainView does not re-test the upstream math (TESTING §9) — it
    asserts its read of it agrees.
  - **Deribit computed-Greeks-vs-venue tolerance fixture.** The #24 engine, fed the
    **venue `mark_iv`** from the recorded `ticker_normal.json` (never a locally
    inverted IV — garbage for a Deribit inverse / BTC-settled contract, issue #83),
    reproduces the venue ticker's dimensionless **delta** (`DELTA_TOLERANCE` 0.02;
    measured 0.5648 vs 0.55) and **gamma** (`GAMMA_TOLERANCE` 1e-5; measured 0.0001031
    vs 0.0001) within tight, documented constants. **Theta / vega are scoped PENDING
    the #83 unit-aware inverse-contract fix** (local theta −125 vs venue −9.9 ≈ 12.7×,
    vega 30.5 vs 8.8 ≈ 3.5×, from the inverse-contract currency convention, not a
    365× per-year/per-day slip) — asserted only finite + correctly signed, never a
    fabricated wide tolerance.
  - **Populated-matrix golden assertion** (`test_populated_matrix_shows_greeks_row_and_computed_origin_glyph`)
    pins the #25 Greeks row + `~` origin glyph on the committed `chain/deribit_btc_atm`
    golden (Δ + Θ, the responsive set at the fixed 120 width).
  - **Draw-purity consolidation.** Drawing BOTH the committed payoff and the
    Greeks-populated matrix into a `TestBackend` runs no pricing / root-finder /
    `build_geometry` / `compute_leg_greeks` call and mutates no state (the committed
    geometry + analytics sidecar are byte-identical across the draw), and
    `render_never_panics` is extended across every payoff/matrix state and size.
- Payoff **curve** render — the expiration and t+0 diagrams from the committed
  builder (`src/app/payoff_build.rs`, `src/app.rs`, `src/ui/payoff.rs`,
  `src/ui/view.rs`, `src/ui/driver.rs`, `src/ui/mod.rs`, issue #27; docs 05 §4,
  02 §7). The committed strategy is sampled into a single
  `GraphData::Series(Series2D)` per curve mode — price → P&L across a bounded,
  strike/spot-anchored 121-point grid — **off** the draw path in the application
  layer: `Profit::calculate_profit_at`-equivalent per-leg `pnl_at_expiration` for
  the expiration line (IV-independent, so it renders from the frozen marks alone),
  and the `expiration(S) + Σ_legs[signed_BS(S)·qty − intrinsic(S)]` recipe
  (`ExpirationDate::Days`, per-leg IV from the #24 sidecar) for the t+0 line — whose
  entry premium is the **frozen commit-time** mark (P0), so t+0 is a locked-entry
  mark-to-market that shows the accrued unrealized P&L at spot and converges to the
  frozen expiration line at the wings (SF-1, the two curves share one cost basis;
  the prior code re-read the live mark each tick and hid unrealized P&L). The t+0
  curve additionally requires a **plausible** IV per leg: a sub-0.5% locally-computed
  leg IV (a #83-style inverse mispricing) makes **only** the t+0 curve unavailable
  while the expiration curve still renders, and a venue IV is trusted unfloored
  (SF-2) — gated by the `MIN_PLAUSIBLE_LOCAL_IV` floor **relocated to the domain**
  (`src/chain/greeks.rs`) so #25's chain matrix and #27's payoff share one
  definition. All `optionstratlib` math, never hand-rolled Black-Scholes, and
  deliberately **not** `Graph::graph_data()` (a `MultiSeries` the #23 adapter defers
  to #47). Break-evens are read off the expiration series' sign changes
  (linear-interpolated, `O(grid)`) on a grid that **anchors every leg strike** as an
  explicit sample so each payoff kink and between-adjacent-strike crossing is exact
  (double-crossings narrower than one uniform step remain a #28 concern) — the #27
  API-map's sanctioned alternative to `CustomStrategy::new`'s unbounded
  ~6M-iteration ctor scan, which is constructed on **no** path, so the render thread
  never freezes on commit and the tick refresh trivially never runs it.
  `PayoffBuilder::commit(&ChainStore)` builds and caches the geometry + frozen entry
  positions + break-evens on `CommittedStrategy` and bumps a **new** `graph_revision`
  (distinct from the cursor-edit `revision`, so a cursor-only edit never
  over-invalidates); a curve toggle (only when committed), a clear, and a t+0 tick
  refresh bump it only when the active series actually changes. `LiveState::apply_market`
  calls `PayoffBuilder::refresh_tplus0(&ChainStore)` only while the t+0 curve is
  shown, so the hot quote path does nothing under the (IV-independent) expiration
  view and re-prices the **frozen** commit-time positions directly (mutating only the
  sampled underlying and the current per-leg IV, never the entry premium) — never
  reconstructing a strategy. The
  projection cache is a render-loop-owned **ui** `ViewState` (not on app state, so
  the arch rule `application ↛ crate::ui` holds): the loop `sync`s it between the
  event fold and the draw (off the draw path, gated on `App::dirty`), diffing
  `graph_revision` to re-project only on a real change through the #23
  `GraphCache`/`project`. `render`, `run_render_loop`/`step`/`draw_frame`, and
  `payoff::draw` thread `&ViewState`/`&GraphProjection`, so the paint reads only the
  cached projection and builds no `GraphData`. `payoff::draw` renders states first —
  the empty "add a leg" hint, the in-progress leg list, the inline validation errors,
  then the committed line **chart** (a dim zero reference line, the payoff line, the
  break-even markers, and the current-spot marker, the `t`-selected curve) with a
  text header carrying spot + break-evens (legible under `NO_COLOR`, one shared
  number formatter; the spot marker differentiated by `Block` shape, not color) — the
  drawn y-bounds are widened to include `0` (with regenerated endpoint labels), so the
  zero line and the y=0 markers never clip when the P&L window sits entirely above or
  below zero (a fresh position's t+0 curve), payoff-local so the generic `graph.rs`
  adapter is untouched — or an honest "curve unavailable" state (expiration: no marks
  or a non-future expiry; t+0: "no reliable IV"), never a fabricated chart. The build
  is a deterministic pure function of (legs, store snapshot): the reference instant
  is a stored `last_full_poll` timestamp (never `Utc::now()`), the y-axis is honest
  premium-currency × contracts (no ×100 multiplier), and a NaN/degenerate coordinate
  routes to the empty state via the #23 finite gate. (The break-even/max-P&L-vs-
  `optionstratlib` goldens land in #28; the geometry here is verified against
  `Profit::calculate_profit_at`.)
- Multi-leg payoff builder state, keybindings, and validation (`src/app.rs`,
  `src/ui/payoff.rs`, `src/app/keymap.rs`, issue #26; docs 05 §3, §6) — the
  interaction layer the payoff screen (#27) will render, state + keys + validation
  only (no curve). The application layer gains the ordered builder state machine:
  `BuilderLeg { strike, style, side, qty }`, a `Side { Buy, Sell }`, a `CurveMode
  { Expiration, TPlus0 }`, a structured `LegError`, and a `CommittedStrategy`, all
  owned by a fleshed-out `PayoffBuilder` (an ordered `Vec<BuilderLeg>` + a cursor +
  a monotonic edit revision). The payoff screen's `handle_key` resolves through the
  single keymap (new `resolve_payoff`) and drives the builder: `a` appends the
  chain's focused leg (cursor strike + call/put, long by default; falls back to the
  nearest-spot strike when no row is focused), `x` removes the cursor leg, `+`/`-`
  increment/decrement the **cursor** leg's quantity (the direction read from the
  shared chord), `s` toggles its side, `Enter` validates and commits, `Esc`
  discards, and `t` toggles the expiration ⇄ t+0 curve mode (the curve itself
  renders in #27). Every edit is cursor-scoped and bounds-safe (`.get()`/`.get_mut()`,
  checked/floored quantity arithmetic — no overflow, no zero-underflow). `Enter`
  runs a **pure** `PayoffBuilder::validate(&chain)` returning a typed result — ≥ 1
  leg, no zero-qty leg, and every leg with a known/fresh mark
  (`BuilderLeg::mark_in`, the `—`-not-`0` rule) — and commits only when valid;
  otherwise it renders the inline per-leg error (e.g. `leg 2: no mark`) and commits
  nothing. `handle_key` is pure over `&mut LiveState` (no I/O, no `.await`), returns
  no `AppEvent`; the render loop learns a builder edit changed via the builder's
  revision (diffed alongside the chain `Selection`), so a mutation redraws while an
  ignored key leaves the frame clean. `payoff::draw` renders the states first — the
  empty "add a leg with `a`" hint, the in-progress leg list (cursor-marked, with
  each leg's mark or `—`), the inline validation errors, and the committed
  strategy's legs — never a blank, never a panic; color is never the only signal
  (BUY/SELL text, `▸` cursor glyph, `!` error, `✓` commit). The payoff keys are now
  wired (deferred markers cleared) and continue to appear in the help overlay from
  the one keymap. The **Chain**-screen `a` (`ChainAction::AddLeg`) is wired to the
  same append path, so the headline gesture — focus a strike on the chain with
  `c`/`p`, then press `a` to add it to the builder — is now live (its deferred marker
  cleared too), sharing `payoff::append_focused_leg` (ui→ui) and bumping the same
  builder revision the driver diffs to redraw.
- Per-leg Greeks precedence wired through the chain matrix (`src/chain/store.rs`,
  `src/ui/chain.rs`, issue #25; docs 01 §7, §8) — the projection+wiring layer on
  the #24 engine. `ChainStore` now **owns** the style-keyed `GreeksSidecar` and
  fills it on the market/tick event (never in `draw`): `apply_greeks` folds venue
  iv/gamma per style via `apply_venue_greeks`, and `apply_poll`/`apply_quote`/
  `apply_greeks`/`seed` run `compute_leg_greeks` to fill local theta/vega/rho (and
  the local iv/gamma/delta fallback), cached by a store `input_generation` that
  bumps only on an applied option-data change (a dropped/crossed update is a cache
  no-op). The reference instant is a store timestamp (`last_full_poll` / the
  update's `received_time`), **never** `Utc::now()`, so the fill stays
  deterministic. A new read-only `ChainStore::leg_greeks(&InstrumentKey)` exposes
  the cached analytics to the draw path — no `&mut` reaches draw. The chain-matrix
  projection (`project_call`/`project_put`/`resolve_leg`) now resolves each
  `LegView` field by the §7 per-field precedence: **delta** prefers the venue
  per-leg value (`delta_call`/`delta_put`) and falls back to the local sidecar;
  **gamma** comes from the style-keyed `LegGreeks` (venue-or-local per its origin,
  so unequal call/put gamma both survive — the shared-`OptionData`-field loss is
  fixed); **iv** follows a three-level precedence (`resolve_iv`) — a per-style
  **venue** sidecar IV → the **call-only** shared `OptionData.implied_volatility`
  (§7 documents that shared field as the call-side value, so the **put gets no
  shared fallback**, avoiding a call/put IV collision) → a **locally computed**
  sidecar IV; **theta**/**vega** are always locally computed. A **sub-plausibility
  local-IV honesty floor** (`MIN_PLAUSIBLE_LOCAL_IV` = 0.5%, IV stored as a
  fraction) clears a `ComputedLocally`-origin IV below the floor to `—` (the same
  economic-implausibility reasoning as the exact-zero venue sentinel), so a
  mispriced near-zero local inversion — e.g. a Deribit inverse (BTC-settled)
  contract priced as USD by the #24 engine — degrades to `—` rather than painting a
  fake percentage; a **venue** (`Provider`) IV is trusted and **never** floored, so
  a legitimate provider-computed IV (IG equities, always ≫ 0.5%) still shows. On the
  zero-config Deribit **seed frame** the call IV now shows the honest venue value
  (e.g. `49.22%` from the shared call-side field, `Provider` origin) and the put IV
  shows `—` (no per-style venue IV at seed; the near-zero local floored out), rather
  than the fabricated `0.00%`/`0.20%` a prior revision rendered. `LegView.greeks_origin`
  still rolls up to `ComputedLocally` when **any** resolved present field is local,
  but the `~` origin glyph now badges the **actual computed cell** (an
  iv/gamma/theta/vega/delta value whose resolved origin is `ComputedLocally`), never
  the trustworthy venue field beside it — so a mixed-origin row (venue delta + local
  theta) badges the local theta, and a leg with a local field but a `None` delta is
  still badged (glyph present iff a present resolved field is local; legible under
  `NO_COLOR`). A `None` field renders `—`, never a fabricated `0`. The matrix now
  carries the full **delta/gamma/theta/vega** columns, dropped responsively in the
  documented `Γ → ν → Θ` order (Θ retained first, Γ last). The projection is a
  **pure** read of the cached sidecar — no recompute and no pricing in `draw`.
  Populated-matrix render goldens (`deribit_btc_atm`, `stale`, `escape_hygiene`)
  regenerated for the honest per-leg IV + relocated origin glyph (the authoritative
  golden + tolerance fixtures land with #28). New unit/property/store tests:
  per-field precedence branches, the sub-plausibility local-IV floor (below → `—`,
  above → shown, venue never floored), the call-only shared-IV fallback and the
  put's non-collision, the per-cell origin glyph (including the `None`-delta-with-
  local-theta case), unequal call/put gamma survival in both arrival permutations,
  `—`-not-`0`, responsive column drop order, a draw-purity/no-pricing assertion, and
  store-side sidecar population + generation-cache no-op. No new dependency. The
  deeper honest-per-leg-IV-at-seed restoration (unit-aware inverse-contract IV
  inversion + per-style venue-IV seeding, so the local inversion itself is correct
  rather than floored) is deferred to a follow-up.
- The local Greeks/IV fill-in engine (`src/chain/greeks.rs`, issue #24; docs 01
  §7) — the analytics sidecar that fills the Greeks/IV `optionstratlib`'s
  `OptionData` cannot hold (it persists only iv/delta/shared-gamma, no
  theta/vega/rho). `GreeksSidecar { by_key: HashMap<InstrumentKey, LegGreeks> }`
  is keyed by the **style-bearing** `InstrumentKey`, so a call and a put leg of
  one strike keep **separate** entries — the venue's single shared
  `OptionData.iv`/`gamma` never collides call/put (asserted in both permutations).
  `LegGreeks` carries per-field `GreeksOrigin` (venue vs local) with
  theta/vega/rho **always** `ComputedLocally` (venue-streamed theta/vega/rho are
  discarded). `compute_leg_greeks(chain, ctx, sink)` builds an
  `optionstratlib::Options` and calls the real `optionstratlib::greeks::{delta,
  gamma, theta, vega, rho}` + `Options::calculate_implied_volatility` — **no
  hand-rolled Black-Scholes or root-finder**. The IV quote-selection is explicit
  (Mid of an uncrossed two-sided quote → else Absent); a crossed / stale / absent
  quote or a `GreeksError` clears the leg's local fields to `None` with a
  `LegStatus` recorded (never a bogus IV or a stale computed Greek shown as
  fresh). Recompute is event-driven and cached by `input_generation` (an
  unchanged generation does no work) — never in `draw`. The kernel is
  deterministic: expiry is priced via a `expiration_utc − as_of` day count
  through `ExpirationDate::Days`, deliberately avoiding `optionstratlib`'s
  `DateTime`-variant path that reads `Utc::now()`. Analytics are
  `Positive`/`Decimal` — no `f64` past the seam. 17 unit + property
  (`greeks_fill_deterministic`) tests. No new dependency. (The risk-free
  rate/dividend default to `0` pending a config knob.)
- The `GraphData` → ratatui dataset adapter (`src/ui/graph.rs`, issue #23;
  `docs/05-views-and-ux.md` §4, ADR-0001). ratatui does not consume
  `optionstratlib`'s `GraphData` directly, so `project(&GraphData) ->
  GraphProjection` maps `GraphData::Series(Series2D)` into the ratatui chart
  shape — a point series (`&[(f64, f64)]`), x/y `AxisBounds`, and precomputed
  numeric endpoint labels. The projection is **fallible and first-class-empty**:
  an empty, mismatched-length, or all-non-finite series yields
  `GraphProjection::Empty(EmptyReason::{NoData, Degenerate, Unsupported})` — never
  a panic and never a fabricated series — so the payoff (#27), replay (#35), and
  vol-surface (#47) screens render a deliberate empty state rather than blanking.
  Geometry is never built in `draw`: a `GraphCache { input, projection }` projects
  off the draw path and `draw` reads only the cached projection (`draw` takes
  `&State`, so it cannot re-project or fabricate a `GraphData`, asserted by a
  purity test). `Series2D`'s parallel `Vec<Decimal>` domain is formatted to plot
  `f64` at the UI edge only, and every coordinate crosses a single NaN/Inf gate
  before entering a dataset. The `MultiSeries`/`GraphSurface` variants are matched
  wildcard-free (→ `Unsupported`) so v0.5's overlaid Greek curves / surface fill
  those arms at compile-time. 15 unit + property tests. No new dependency
  (`ToPrimitive` from the `optionstratlib` prelude).
- The end-to-end live-path integration test, the layering arch test, the
  external faux-provider conformance, and the manual live smoke (issue #22 Part B;
  `docs/TESTING.md` §7–§8, `docs/03-data-providers.md` §11–§12). **Live-path
  integration** (`src/tests_integration.rs`, in-crate `#[cfg(test)]` because it
  needs the `pub(crate)` `ChainStore` merge, chain-matrix `draw`, recorded-fixture
  assembler, and golden helper): the recorded Deribit #17 fixtures →
  normalize/assemble seam → `ChainStore` **poll→stream merge** (seed + re-poll +
  an idempotent quote folded into every priced leg) → `chain::draw` → the
  committed `chain/deribit_btc_atm` golden, with **no network** and a fixed as-of
  instant — the zero-config Deribit acceptance proven against fixtures. It also
  proves the render path is **provider-id agnostic** (the same fixture chain under
  an external `ProviderId("faux")` renders the identical golden) and that the draw
  path runs with **no async runtime** (a plain sync `#[test]` over `render(&App)`,
  mutating nothing). **Layering arch test** (`tests/arch.rs`): a deterministic,
  filesystem-only grep of the `src/` import graph (production regions only —
  `#[cfg(test)] mod` blocks are masked) that **fails the build on any back-edge**
  — domain→adapter/port/ui, adapter→app/ui, adapter→adapter, a `src/ui/*` import
  of a provider or `tokio` I/O, and any `ui→` reverse edge — with a self-test
  proving the detector fires on a synthetic offender (not a vacuous pass).
  **Faux-provider conformance** (`tests/integration.rs`, PUBLIC API only): a
  test-only `FauxProvider` (non-reserved `ProviderId("faux")`) implementing **only**
  the public `Provider` port, registered via `ChainViewApp::builder().register(..)`
  and driven end-to-end — its `fetch_chain` returns the named `ChainFetch`, a
  forced reconnect **resubscribes off the fresh `ChainFetch.aliases`** (no bare
  `OptionChain`, no symbol re-derivation), it plugs into the ADR-0009 supervised
  composition seam identically to a built-in, and its declared
  `ProviderCapabilities` gate the screens (a `capabilities_total` proptest proves
  the gate is total over declared caps, never a `ProviderId` match) — proving the
  port reaches parity through the public surface with **no built-in
  special-casing**. **Live smoke** (`tests/live_smoke.rs`): a `#[ignore]`,
  `SMOKE_DERIBIT=1`-gated manual Deribit sanity check that never runs in CI. Every
  integration test is deterministic (no socket, no wall-clock wait) and finishes
  far under the 10 s bound.
- First bench suite and the `BENCH.md` baseline (issue #21;
  `docs/06-performance.md` §3–§5, `docs/TESTING.md` §11). Three `criterion`
  benches under `benches/`, each a `harness = false` binary that reports
  `hdrhistogram` p50/p99/p99.9 (the tail is the headline; criterion's mean is
  context only) with a per-bench coordinated-omission disclosure, warmup, and a
  documented sample count: **`bench_render_chain`** (HP-1, draw the fullest
  64-strike chain matrix into a `TestBackend` @ 120×40), **`bench_event_fanin`**
  (HP-2, fold a 128-leg `MarketUpdate` burst through `App::on_event`), and
  **`bench_chain_merge`** (HP-3, a Deribit `ticker.`+`book.` payload → coalescing
  `OptionChain` merge, plus the NFR-15 bounded-memory staging-bound probe).
  `BENCH.md` records the first measured baseline with the measurement
  environment; NFR-14 (16 ms/60 fps p99 frame budget) and NFR-15 (bounded memory)
  are re-baselined as **MEASURED**, NFR-16 (startup-to-first-chain) stays
  **PENDING** (a cold, network-dominated path measured against a live venue, not
  in the deterministic fixture suite). Harness seam: a `bench` Cargo feature
  (`[features] bench = []`, OFF by default) gates a `#[cfg(feature = "bench")]
  pub mod bench_support` exposing only the constructors the benches need (a
  populated render `App`, a seeded `ChainStore`, a scripted `MarketUpdate` burst,
  and the Deribit payload → coalescing-merge harness) plus a
  `#[cfg(feature = "bench")]` Deribit stream-normalize helper and the
  widened-cfg fixture/`EventBridge` staging accessors — so **the default public
  surface is unchanged** (nothing new appears without the feature; the benches
  set `required-features = ["bench"]`). Supply-chain: `criterion` and
  `hdrhistogram` are **`[dev-dependencies]`** — neither rides in the release
  binary, both are used only by the `bench`-gated targets. `criterion 0.5` is
  pulled with `default-features = false, features = ["cargo_bench_support"]` to
  drop the `plotters`/`rayon`/html-report tree; `hdrhistogram 7` is the
  percentile recorder. Both are well under the crate's 1.88 MSRV. Audit note:
  `cargo audit` reports **0 vulnerabilities** on the resulting tree; the
  `criterion 0.5.1` / `hdrhistogram 7.5.4` subtree (`ciborium`, `clap`, `half`,
  `tinytemplate`, `oorandom`, `anes`, …) introduces **no new RUSTSEC advisory** —
  the three pre-existing informational warnings (`dotenv` unmaintained via
  `deribit-*`, `paste` unmaintained via `nalgebra`/`ratatui`, `lru` unsound via
  `ratatui`) are unrelated and already tracked. As dev-dependencies neither crate
  ships in the release binary; the CI `cargo audit`/`cargo deny` gates (#20) cover
  them going forward.
- CI pipeline and supply-chain gates from v0.1 (`.github/workflows/ci.yml`,
  `deny.toml`, `.github/pull_request_template.md`, issue #20; `docs/SECURITY.md`
  §7.1, `docs/TESTING.md` §13.5, `docs/specs/providers.md` §0). The GitHub
  Actions pipeline runs five jobs: **check** (the four non-negotiables on stable
  — `fmt --all --check`, `clippy --all-targets --all-features -- -D warnings`,
  `test --all-features`, `build --release` — mirroring `make pre-push` exactly,
  plus the `RUSTDOCFLAGS="-D warnings" cargo doc` gate and the Spanish-text
  guard); a pinned **MSRV 1.88** build+test job; a non-blocking **coverage** job
  (`cargo-llvm-cov`); **audit** (`cargo audit`); and **deny** (`cargo deny
  check` — advisories + licenses + bans + sources). CI never contacts a real
  venue — fixtures only, and the future live smoke (#22) stays `#[ignore]`.
  `cargo-audit` / `cargo-deny` are CI tooling, **not** added to `Cargo.toml`
  `[dependencies]`. Supply-chain notes (no crate dependency was added):
  - **`deny.toml` policy validated against the ACTUAL tree.** License allow-list
    is the permissive union actually present (MIT/Apache-2.0 plus BSD-2/BSD-3/
    ISC/Zlib/Unicode-3.0/CC0-1.0 and the single-license data crates
    `CDLA-Permissive-2.0` and `bzip2-1.0.6`); sources are crates.io-only (a
    substitution fails); `[bans].multiple-versions = "warn"` because the mandated
    upstream clients (`deribit-http`/`deribit-websocket`, `optionstratlib`,
    `ratatui`) pull disjoint dependency-tree epochs — the duplicated families
    (`rand`/`rand_core`/`getrandom`, `hmac`/`sha2`/`digest`/`block-buffer`/
    `crypto-common`/`cpufeatures`, `darling`, `hashbrown`, `itertools`,
    `unicode-width`, `core-foundation`) are named in-comment.
  - **Three documented transitive advisory ignores** (mirrored in the CI
    `cargo audit --ignore` flags), each with a reason + re-evaluation trigger:
    RUSTSEC-2021-0141 (`dotenv`, unmaintained), RUSTSEC-2024-0436 (`paste`,
    unmaintained), and RUSTSEC-2026-0002 (`lru`, unsound — fixed only by the
    separate ratatui 0.30 upgrade). All three are fix-less unmaintained/unsound
    notices. The `time` DoS (RUSTSEC-2026-0009) is **patched, not ignored** (see
    `### Changed`). Verified locally: `cargo deny check` and `cargo audit --deny
    warnings` both pass green on the current tree (0 vulnerabilities).
  - The dependency-addition **audit-note convention** is documented durably in
    `.github/pull_request_template.md` and `docs/TESTING.md` §10.
- Chain render goldens and the terminal escape-sequence sanitizer
  (`src/ui/theme.rs`, `src/ui/golden.rs`, `tests/render/golden/chain/`, issue #19;
  `docs/TESTING.md` §4, §13.2, `docs/SECURITY.md` §6.4). Because stdout **is** the
  UI, a venue-controlled string (instrument name, symbol, venue error text) is
  **not** trusted display-safe; the sanitizer neutralizes it at the render edge,
  and the goldens pin the rendered chain screen byte-for-byte. Key behaviours:
  - **Hardened, single-source sanitizer.** `theme::sanitize` (the #14 status-line
    helper, promoted to `pub(crate)` and hardened) now **replaces** every C0 control
    (`0x00..=0x1F`, incl. `TAB`/`LF`/`CR` and the `ESC` `0x1B` introducer), `DEL`
    (`0x7F`), and C1 control (`0x80..=0x9F`, incl. the 8-bit `CSI` `0x9B` / `OSC`
    `0x9D` / `DCS` `0x90` introducers) with a visible placeholder (`U+FFFD`). Because
    a terminal only enters escape-processing on an introducer, replacing every
    introducer makes a whole `ESC`-prefixed sequence inert visible text without
    parsing it — an `OSC 52` clipboard-write, a `CSI` cursor-move, or a `TAB`/`LF`
    can neither fire a terminal side effect nor break the matrix layout. The pure
    helper is the **one** sanitizer every venue-string edge routes through; the
    chain matrix's local duplicate is removed, so the matrix and the status bar can
    never neutralize venue bytes differently.
  - **Wired at every venue-string render edge.** The chain matrix title
    symbol/expiry, the empty-state underlying/expiry, the loading provider id, the
    error message, and the status bar / keybar hint all pass through the shared
    sanitizer. The `NO_COLOR` / marker-not-color policy (#14) is unaffected —
    sanitization touches only control sequences, not ChainView's own styling.
  - **Render goldens at a fixed 120×40.** A new `ui::golden` test-support module
    renders a screen into a `TestBackend` and captures the buffer as text (one row
    per line), compared against a committed golden or, under `UPDATE_GOLDENS=1`,
    rewritten (the documented regeneration mechanism, so a deliberate screen change
    refreshes the golden in the same commit — a screen change without a golden
    update is caught by the mismatch). Committed: `chain/deribit_btc_atm.txt` (the
    populated matrix assembled from the recorded #17 Deribit fixture through the
    real adapter seam — fixture → `normalize_leg` → `assemble_chain` → `ChainStore`
    → `chain::draw`), `chain/loading.txt` (the pre-first-frame loading state),
    `chain/empty.txt` (the empty-Ready "no data" state), `chain/provider_error.txt`
    (the provider-unreachable state), and `chain/stale.txt` (the stale-feed state:
    the last chain dimmed with a `◐ stale` badge — the "never show stale as live"
    honesty guarantee, a #18 acceptance criterion). Deterministic: the fixture's own
    timestamps and a fixed as-of instant, never a wall clock or a live socket.
  - **A believable populated golden (UX-review fixes).** The golden fixture gives
    each option a **distinct** recorded ticker (`ticker_normal` / `ticker_put` /
    `ticker_61000_call`), so the matrix depicts a realistic call/put asymmetry — a
    60000 call Δ `+0.5500` beside a 60000 put Δ `-0.4500` (delta parity holds) and a
    61000 call Δ `+0.4200` — rather than one cloned ticker showing an impossible
    `+0.55` put; distinct per-leg Greeks also close the blind spot where a
    read-the-wrong-leg regression would have gone uncaught. The chain screen also
    gained forward render fixes visible in the goldens: a **Calls / Puts**
    super-header over the mirror halves (text, `NO_COLOR`-legible); the loading /
    empty / error state bodies **vertically centered** on a shared two-line
    baseline (no longer top-anchored); the title expiry formatted as a bare date
    (`exp 2025-06-27`, not the verbose RFC 3339); the `◀ATM` marker width reserved
    on every strike row so the digits form a clean ladder; and the redundant
    tick-direction glyph suppressed on an absent (`—`) price.
  - **Escape-hygiene golden.** `chain/escape_hygiene.txt` feeds a hostile
    underlying symbol carrying an `OSC` clipboard-write, a `CSI` clear-screen, a raw
    newline/tab, and an 8-bit C1 `CSI` through the adapter seam into the rendered
    matrix title; the golden proves it renders as inert visible text, and its
    committed bytes contain **no** raw `ESC` (`0x1B`), `BEL` (`0x07`), or 8-bit
    introducer — proof the sanitizer ran.
  - **No new dependency.** Tests: 6 in `src/ui/theme.rs` (C0/C1 replacement, the
    `OSC`/`CSI`/`DCS`-to-inert-text case, printable/glyph preservation, the
    control-range predicate, and two property tests — `prop_sanitize_*` — asserting
    the no-control/no-introducer invariant over arbitrary Unicode strings and
    arbitrary bytes) + 7 in `src/ui/chain.rs` (the six render goldens —
    populated / loading / empty / error / stale / escape-hygiene — and the
    render-hostile-name-inert-across-sizes case). Two new Deribit ticker fixtures
    (`ticker_put.json`, `ticker_61000_call.json`).
- The chain-matrix screen with honest empty/loading/error states
  (`src/ui/chain.rs`, issue #18; `docs/05-views-and-ux.md` §2.1, §6, §8,
  `docs/01-domain-model.md` §8, `docs/02-tui-architecture.md` §7). The first real
  screen body — strikes × call/put (bid/ask/mark/IV/Greeks) — replacing the #13
  placeholder. Key behaviours:
  - **States before the happy path.** `chain::draw` renders the **loading**
    (centered tick-driven spinner + "connecting to `<provider>`…"), **empty** ("no
    data for `<underlying> <expiry>`" + hint), and provider-**error** (actionable
    message + the `r` reconnect affordance) states first, driven off
    `LiveState.load` (`ScreenLoad`) and the store's emptiness/health per the §2.1
    prerequisite/recovery matrix — a screen that only knows how to render data is
    incomplete. On a dropped stream the last chain renders **dimmed** with a
    `◐ stale` / `↻ reconnecting (n)` badge (`health_span`); the render loop never
    blanks.
  - **`ChainRow`/`LegView` projected at draw time, borrowed, never owned.** The
    view models (`docs/01-domain-model.md` §8) are projected from the store's
    `OptionChain` inside `draw` — no computation, no pricing, no `GraphData`, no
    I/O, no mutation. A `LegView` field is `None` **iff** the underlying
    `OptionData` field is `None`; a missing value renders `—` (an em dash), never a
    fabricated `0`. `theta`/`vega` are v0.2 (no `OptionData` field yet); `bid_dir`/
    `ask_dir` read the store's decayed direction baseline as of the last-poll instant
    (the pure-draw reference — `draw` reads no wall clock); `greeks_origin` drives an
    origin glyph for locally-computed Greeks (v0.2). Display formatting never panics
    at the render edge: `fmt_iv` uses **checked** `× 100` (a finite-but-absurd IV can
    survive the adapter seam, which rejects NaN/Inf/negative but not magnitude, and
    `Decimal`'s `Mul` panics on overflow), rendering `—` on overflow (ADR-0007
    untrusted-input hardening).
  - **Absent-IV vs 0% IV decision (from the #15 review).** Upstream
    `OptionChain::add_option` takes a **non-`Option`** `Positive` IV, so the Deribit
    adapter defaults an absent IV to `Positive::ZERO` and a row cannot distinguish
    "venue sent no IV" from "IV = 0". `project_iv` projects an **exactly-zero** IV to
    `None` — a listed, quoted option always carries a strictly positive IV, so a bare
    `0` is the absent-sentinel, not a live quote — so the matrix renders `—`, never a
    fabricated-looking `0.00%`. Documented and unit-tested (`project_iv(ZERO) ==
    None`, and the populated matrix renders the em dash with **no** `0.00%`).
  - **ATM anchoring + the shaded strike column, color never the only signal.** The
    nearest listed strike to spot carries the `◀ATM` marker; the shared strike
    column shades by the `K/S` `StrikeRelation` bucket (BelowSpot/AtSpot/AboveSpot —
    **not** an ITM/OTM label); bid/ask cells carry a `▲`/`▼`/`·` tick glyph — all
    legible under `NO_COLOR`. The v0.1 greek column set is only the columns that can
    carry data — **Δ** (always) and **Γ** (from `OptionData`), Γ shown once one slot
    fits — so a common 120-col terminal shows Γ rather than the always-empty Θ/ν
    columns hiding it; the #14 `Γ → ν → Θ` drop-order primitive stays intact for when
    v0.2 populates Θ/ν and they join the set. Per-frame work is O(visible rows) via
    manual windowing around the cursor/ATM anchor, never O(full chain).
  - **Keyboard navigation resolved through the one keymap.** `chain::handle_key`
    resolves chords through a new `keymap::resolve_chain` (mirroring
    `resolve_replay`) — no parallel key table. Strike nav (`↑↓`/`kj`) moves the
    cursor (first move reveals it at the ATM anchor, then steps clamped) and leg
    focus (`c`/`p`) toggles the emphasized leg — both local `Selection` mutations the
    render loop detects (a `Selection` diff) to request a redraw. Expiry/underlying
    switch, drill-in, and add-leg resolve through the map but are documented no-ops
    pending their data plumbing (multi-expiry subscribe, underlying list, drill
    view, the v0.2 payoff builder); no screen ever performs I/O inline. `Selection`
    gains a `focused_leg` (new `LegFocus` enum, `Call`/`Put`).
  - **The help overlay advertises deferred keys honestly.** A `Binding` gains a
    `deferred: Option<&'static str>` marker (still single-source in the keymap, so it
    cannot drift): a key that **resolves and is documented but is not yet wired** (its
    `handle_key` is a no-op) renders a dim `(<version>)` suffix in `?` — so the four
    deferred chain keys (and the same cross-screen pattern on the deferred replay
    playback/speed/fill/end-jump keys and the not-yet-wired surface/depth/payoff-live
    keys) are no longer presented as live features. The resolution logic is
    unchanged; the marker only annotates + renders.
  - **Draw purity is proven.** `chain::draw(&LiveState, &mut Frame, Rect, Theme,
    u64)` takes `&LiveState` (plus the `Copy` resolved theme, for `NO_COLOR`, and
    tick, for the spinner), so it cannot mutate; a test asserts the store, poll
    clock, selection, and health are unchanged across a draw, and
    `prop_render_never_panics` (#13) plus a chain-local size sweep cover the
    populated/empty/loading/error/stale states.
  - `ChainRow`/`LegView`/`LegFocus`/`resolve_chain` are re-exported from the crate
    root for the render goldens (#19) and downstream screens (#25). **No new
    dependency.** Tests: 31 in `src/ui/chain.rs` (projection None-iff-None,
    `StrikeRelation` bucketing, direction projection, the absent-IV `—` rule, the
    `fmt_iv` overflow-renders-`—` edge, the v0.1 `{Δ, Γ}` column set + Γ-at-120,
    the windowing/anchoring helpers, the five reachable states across sizes, draw
    purity, and the keymap-resolved navigation) + 5 in `src/app/keymap.rs`
    (chain-chord map↔overlay cross-check, non-chain-chord ignore, deferred-marker on
    chain/replay keys, deferred-keys-still-resolve) + 1 in `src/ui/theme.rs` (the
    overlay renders the deferred `(vX)` suffix) + the driver dirty-on-local-nav
    regression.
- The Deribit `ticker`/`book` streaming overlay and the adapter-owned
  reconnect/resubscribe loop (`src/providers/deribit.rs`, issue #16;
  `docs/03-data-providers.md` §7.1, §5, `docs/01-domain-model.md` §5, §7,
  `docs/02-tui-architecture.md` §5). `Provider::subscribe` now opens the live
  overlay — it replaces the #15 `Unsupported` stub. Key behaviours:
  - **Ticker + book normalization at the seam.** `ticker.{instrument}`
    normalizes into a `QuoteUpdate` (bid/ask/last/sizes, checked at the `f64`
    seam — a crossed quote drops bid/ask, keeping the prior) **and** a
    `GreeksRow` (venue delta/gamma + percentage-form IV divided by 100);
    `book.{instrument}.{group}` normalizes into a `DepthLadder` with the upstream
    `change_id` captured for later gap-detect/resync, best-first levels, and
    per-level `f64` checks that drop an invalid level without dropping the ladder.
    Both the aggregated `[price, amount]` and raw `[action, price, amount]` book
    encodings decode. **Streamed theta/vega/rho are deliberately discarded**
    (`docs/01` §7) — not even deserialized; the `GreeksRow` always emits `None`
    for them. Raw `deribit-websocket` notification DTOs never leave the adapter.
    **`trades.` is not subscribed** (the tape is deferred), so `MarketUpdate`
    carries no trade event.
  - **Producer-side overwrite-on-full staging — completes the two-stage
    coalescing.** The adapter keeps a per-`InstrumentKey` `ProducerStaging` map
    (one slot per instrument, the latest of each kind held independently) and,
    when the bounded `mpsc::Sender<MarketUpdate>` is **full**, **overwrites the
    staged slot with the newest value** — reserving a channel slot *before*
    taking the staged value, so a full channel never drops it — and flushes on
    space. This is the producer mirror of #10's consumer `EventBridge`,
    completing the NFR-15 latest-value-wins guarantee under sustained saturation.
    The map is O(N subscribed) and reuses its allocation.
  - **Adapter-owned reconnect/resubscribe loop.** `deribit-websocket` (0.3.1)
    ships no auto-reconnect, so ChainView drives it behind the
    `SubscriptionHandle`: connect → resubscribe the `ticker`/`book` channels →
    drain updates; on a drop it emits `Health(id, Reconnecting{attempt})` —
    control-class updates (`Health`/`Chain`) are **await-sent** (never
    coalesced/dropped) on the **one** bounded `mpsc::Sender` the port provides,
    while coalesced-class updates use overwrite-on-full staging on the same
    sender; the single-sender port cannot physically separate a control channel,
    so the true two-class priority drain is the consumer bridge's concern and the
    port→bridge two-sender routing is reconciled at the composition seam (#22, per
    ADR-0009). It backs off with jittered exponential backoff
    (`BASE = 250 ms`, `MAX = 30 s`, `jitter ∈ [-0.2, 0.2]`, `attempt` reset to 0
    on a successful (re)subscribe — never a hot-loop, respecting the upstream
    token-bucket limiter), then **re-`fetch_chain`** (#15) to reconcile drift and
    resubscribes off the **fresh** `ChainFetch.aliases` (backfill = current
    state, no bare `OptionChain`, no symbol re-derivation; a fresh `Chain`
    snapshot is emitted to reconcile structure). Cancellation (handle drop) is
    observed at every `.await` via a `biased` `select!`, so the loop never opens
    a socket after cancellation; dropping the handle cancels the token and aborts
    the task (no fire-and-forget). `install_default_crypto_provider()` installs
    the rustls provider once before the first WS TLS handshake.
  - **Backoff as a pure, injectable-jitter kernel.** `backoff_delay(attempt,
    jitter)` is pure — the jitter is **injected** (the loop samples it from the
    process clock; tests pass a fixed value) — so the formula, the bounds (never
    above `MAX * 1.2` = 36 s, never below `BASE * 0.8` = 200 ms), the jitter
    range, and the `attempt = 0` → `BASE` reset are unit-tested with **no**
    wall-clock wait and no unseeded RNG in the kernel.
  - **`AliasCatalog::instruments()`** (new, `src/chain/fetch.rs`) enumerates
    every feed alias so the reconnect resubscribe walks the **fresh** aliases
    without re-deriving symbols from strikes.
  - Tested with CONSTRUCTED payloads, NO real socket, NO wall clock: the backoff
    kernel (bounds / jitter range / reset), ticker → `QuoteUpdate`/`GreeksRow`
    (incl. discarding theta/vega/rho and the percentage-form IV), book →
    `DepthLadder` with `change_id` (both level encodings), the producer staging
    (overwrite-on-full + flush-on-space + O(N) bound + closed-channel), frame
    routing (ticker/book publish, a ticker channel with a trailing interval
    suffix still routing to the right key, unknown-symbol guard, non-subscription
    / malformed frames ignored), the reconnect backfill snapshot, and property
    tests that `backoff_delay` / `normalize_ticker` / `normalize_book` are total
    (a malformed payload is a valid update or a dropped field, never a panic).
    The `subscribe` test spawns the loop on a current-thread runtime and drops
    the handle before it is polled, so no socket opens. The full mock-transport
    lifecycle (socket close/error/resubscribe/saturation/lag/shutdown) lands in
    #17. 27 new deribit tests plus the `AliasCatalog::instruments()` test in
    `src/chain/fetch.rs`.
- Deribit normalization fixtures and mock-transport lifecycle tests
  (`src/providers/deribit.rs`, `tests/fixtures/deribit/`, issue #17;
  `docs/TESTING.md` §5, §9, `docs/03-data-providers.md` §5, §3). Adds the
  recorded fixture corpus and the deterministic reconnect-lifecycle coverage #16
  deferred — **no real socket, no wall clock**. Key pieces:
  - **A minimal transport seam so the loop is testable.** The #16 reconnect loop
    reached straight for `DeribitWebSocketClient`, so a crate-internal
    `DeribitTransport` trait (private — the public `Provider` API is unchanged)
    now lifts the three impure loop operations (connect + subscribe, receive a
    frame, re-`fetch_chain` for the backfill) behind one seam. The production
    `LiveTransport` wraps the upstream WebSocket client plus the REST backfill,
    exactly as before; a test `MockTransport` yields scripted frames/errors and
    records connects/refetches/subscription-sets. No raw upstream DTO crosses the
    seam, and #16's tests stay green.
  - **Constructed-to-wire-shape fixtures** under `tests/fixtures/deribit/`
    (`include_str!`-baked, so byte-stable across machines): `instruments_btc`,
    `ticker_normal`, `book_snapshot`, `book_delta`, plus degraded shapes —
    zero-bid, crossed (`ask < bid`), negative, non-finite, and a
    missing-strike/unknown-style payload. Each is pinned to `deribit-http` 0.7.1 /
    `deribit-websocket` 0.3.1 (recorded in `docs/specs/providers.md` §0). JSON
    carries no `NaN`/`Inf` literal, so the non-finite fixture uses a non-numeric
    string field the adapter refuses at deserialization (the frame is dropped, no
    fabricated value); the `f64` `NaN`/`Inf` guards themselves stay covered by the
    property tests.
  - **Fixture → `OptionChain` / update assertions.** Each fixture normalizes to
    its recorded chain/update: the instrument list assembles a two-strike chain
    (perpetual filtered, IV / 100 reaches the leg), the ticker normalizes to a
    `QuoteUpdate` + `GreeksRow` (theta/vega/rho discarded), the book to a
    `DepthLadder` with `change_id`; the degraded fixtures prove a
    crossed/zero/negative field outcome and a row-fatal `Normalize` reject with no
    panic and no fabricated value.
  - **Mock-transport lifecycle tests (a)–(f):** socket close and stream error →
    `Health(Reconnecting)` + no panic; resubscribe → the reconnect re-issues
    `fetch_chain` **and** resubscribes off the fresh aliases (the new 61000-C leg
    appears), with the backoff **attempt reset-on-success** asserted at the loop
    level (both reconnects surface `attempt: 1`) — the assertion #16 deferred;
    saturation → a burst far beyond a cap-1 channel keeps the producer staging
    O(N instruments) (flat memory); lag → a slow consumer still receives the
    await-sent `Health`/`Chain`; shutdown → dropping the real `SubscriptionHandle`
    stops the loop. All run under `#[tokio::test(start_paused = true)]` with
    scripted frames and virtual-clock drains, deterministic and well under 10 s.
  - **Fixture corpus as a property seed.** The committed fixtures also feed a
    totality test (each normalizes to a valid update or a typed reject, never a
    panic), complementing #16's `normalize_total` property tests.
  - 16 new deribit tests (10 fixture-normalization + 6 lifecycle); the transport
    seam refactor keeps all 65 existing #16 deribit tests green.
- **`deribit-websocket` `0.3.1`** (`[dependencies]`, issue #16) — the upstream
  Deribit WebSocket client ChainView wraps for the streaming overlay; the
  JSON-RPC 2.0 over WebSocket protocol lives upstream and is never reimplemented
  here.
  - **Audit note (supply-chain).** An explicit-approval dependency addition
    (CLAUDE.md "Coding Rules"). Delta over #15's `deribit-http`: adds
    `tokio-tungstenite` (WS framing) and the default `rustls-aws-lc` TLS backend
    — `rustls` + the `aws-lc-rs` crypto provider, installed **once** via
    `install_default_crypto_provider()` before the first WS TLS handshake (this
    differs from #15, where the REST client used `reqwest`'s default TLS and
    needed no provider install) — plus `futures-util`. It shares `tokio`
    (feature-unified toward `full`), `serde`/`serde_json`, `url`, and `dotenv`
    with the existing tree; `aws-lc-rs` requires a C/ASM toolchain at build time.
    `RUSTSEC`-clean at this revision. The public data path needs no credential
    and logs none; the public endpoints send none.
- The Deribit adapter chain assembly, normalization, and honest capabilities
  (`src/providers/deribit.rs`, issue #15; `docs/03-data-providers.md` §7.1, §3,
  §8, ADR-0003) — the zero-config, public-data poll leg and the first provider
  wired end-to-end. Key behaviours:
  - **Chain assembly from an instrument list (no chain endpoint).** `fetch_chain`
    wraps `deribit-http` `get_instruments(currency, "option")` for structure and
    `get_ticker(instrument)` for mark/IV/Greeks, filters to the requested expiry
    day, and assembles one `optionstratlib::OptionChain` — call and put at each
    strike collapse into one `OptionData` row — returning the named `ChainFetch`
    with its per-leg `AliasCatalog` (native `instrument_name` + the Deribit
    `ContractSpecFingerprint`) and absolute-UTC `ExpirySource`. Per-contract
    tickers are hydrated with **bounded concurrency** (a `tokio::task::JoinSet`
    capped at 16 in-flight requests), so a large expiry meets the
    startup-to-first-chain budget without a sequential round-trip per instrument
    and without hammering the venue rate limiter (ADR-0007,
    `docs/06-performance.md`); assembly stays order-independent (grouped by
    strike). A per-ticker failure degrades that leg only, never the whole chain.
    `discover` lists the venue's currencies as underlyings. Public data needs
    **no credentials** (the adapter drives `HttpConfig::production()`), so it is
    the zero-config default.
  - **Field-specific numeric normalization at the `f64` seam.** Prices/IV/sizes
    become `Positive`, Greeks become `Decimal`, each checked before it enters the
    domain (CLAUDE.md "Governance precedence" item 2): Deribit IV is
    percentage-form and divided by 100 (`49.22` → `0.4922`); a zero bid is a real
    zero; a zero ask on a non-zero bid or any `ask < bid` is crossed and rejects
    the whole quote; a `NaN`/`Inf`/negative price/IV/Greek is dropped, never
    becoming a fabricated value; and only a payload that cannot yield a valid
    strike/style/expiry rejects the row as a typed `ProviderError::Normalize`
    naming the field.
  - **Symbol + direct-UTC expiry mapping.** A Deribit `instrument_name`
    (`BTC-27JUN25-60000-C`) maps to the provider-agnostic `InstrumentKey`; expiry
    is the direct UTC instant from the instrument's millisecond timestamp (or the
    `DDMMMYY` date code resolved to 08:00 UTC settlement), never a relative
    offset. Upstream errors map to a redaction-safe `ProviderError` by category
    only — no URL, body, or token is interpolated.
  - **Honest capabilities + honest streaming stub.** `capabilities()` matches the
    §8 Deribit row exactly (`chain: Assemble`, `depth: true`, `greeks: Provided`,
    `option_stream: ChainQuotes { verified: false }`, `underlying_stream: true`,
    `chain_poll: Poll`, `trades_tape: false`, `auth: None`). `subscribe` returns
    `Unsupported` — the streaming overlay + reconnect loop is issue #16. Raw
    `deribit-http` DTOs never leave the adapter.
  - **Registered through `with_builtins`.** `ChainViewAppBuilder::with_builtins`
    now registers the real Deribit adapter under the reserved `deribit` id (via a
    new `register_builtin` helper that expects the reserved id), so
    `builder().with_builtins().run()` resolves the Deribit live source instead of
    reporting an empty registry.
- **`deribit-http` `0.7.1`** (`[dependencies]`, issue #15) — the upstream Deribit
  REST client ChainView wraps for the poll leg; the JSON-RPC-over-HTTP protocol
  lives upstream and is never reimplemented here.
  - **Audit note (supply-chain).** An explicit-approval dependency addition
    (CLAUDE.md "Coding Rules"). It pulls `reqwest` (with its default TLS),
    `tokio` (feature-unified toward `full`), `serde_json`, request-signing crates
    (`hmac`/`sha2`/`base64`, unused on the public path), `url`, `serde_with`,
    `rand`, and `dotenv`. No `rustls` crypto-provider install is needed — the
    HTTP client relies on `reqwest`'s default TLS and exposes no
    `install_default_crypto_provider`; any provider install belongs to the
    websocket path (issue #16), and no live TLS handshake runs in the test suite.
    `deribit-websocket` (streaming) is a separate, deferred addition (#16). The
    public data path requires no credential and logs none; the public endpoints
    send none.
- The single-source keybinding map, the modal help overlay, the auto/dark/light
  theme, the truthful status bar / keybar, and the `NO_COLOR` fallback
  (`src/app/keymap.rs` + `src/ui/theme.rs`, issue #14; `docs/05-views-and-ux.md`
  §3, §7, §8). The terminal layer's interaction + accessibility seam. Key
  behaviours:
  - **One map both dispatch and the overlay read, so they cannot drift.** `KEYMAP`
    is a single declarative `(key, context, action)` table living in the
    **application** layer (`src/app/keymap.rs`, pure data + resolution, no
    `ratatui`). The key dispatch reads it — `App::dispatch_key_global` resolves
    globals through `keymap::resolve_global` (a `GlobalCommand`), and
    `ui::replay::handle_key` resolves scrub keys through `keymap::resolve_replay` —
    and the ui help overlay (`src/ui/theme.rs`) is **generated from the same table**
    via `keymap::help_sections` (`ui → application`, the mandated direction), so a
    key that does something appears in the overlay by construction. A cross-check
    test proves every dispatched global/replay chord is documented (a key not in the
    overlay is a 🔴). The full v0.1 table lands: `q`/`Ctrl-C`, `?`, `1`–`4`,
    `Tab`/`S-Tab`, `r`, `R`, the chain/depth/surface/payoff/replay keys — keys whose
    bodies land later (chain #18, surface/payoff v0.2, depth v0.5, replay
    playback/speed/fill v0.3) are declared now with their context and resolve to a
    documented no-op placeholder. No key is handled outside the map.
  - **Modal help precedence, ordered correctly.** While the overlay is open the
    dispatch honors only `?`/`Esc` (both close it); every other key is swallowed —
    including keys **outside** the keymap vocabulary (F-keys, PageUp/Down,
    Insert/Delete), because the modal intercept runs **before** the vocabulary check,
    so no key can reach the hidden screen behind the overlay. `Ctrl-C` is the one
    documented carve-out: it stays a hard terminal-interrupt quit even behind the
    modal.
  - **Two-column help overlay, readable on 80x24.** The overlay is laid out in two
    height-balanced columns (globals + a screen group per side) generated from the
    map, so every documented key — including the last (Payoff) section — is visible
    on a standard trader terminal instead of clipping off the bottom; help text is
    terse so nothing truncates mid-phrase. Every screen is listed even when it has no
    bindings yet: the v0.5 replay Payoff screen appears as a titled section with a
    "not available yet" note rather than being dropped.
  - **Reachability skip / one-line hint, capability-driven, never a `ProviderId`
    match.** `Tab`/`S-Tab` cycle only the reachable screens for the active
    mode+provider (reading `is_screen_reachable` / `is_replay_screen_reachable`), so
    they never land on an unavailable body; an unavailable number key flashes a
    transient keybar hint ("Depth not available on deribit" / "Payoff is v0.5") and
    does **not** switch, so `App.screen` stays reachable and `render` (#13) stays
    total. The hint decays on the next key or after `HINT_TICKS` ticks (~2 s), so it
    is never a near-zero flash.
  - **`ThemeChoice` (Auto/Dark/Light) resolution + `NO_COLOR` fallback; color is
    never the only signal.** `Theme::resolve` maps the choice to a variant painted
    from the 16 ANSI-named colors (legible on both dark and light terminals, zero
    config). Every color-encoded state pairs color with a color-independent
    marker — `◀ATM` at-spot, `+`/`−` P&L sign, `▲`/`▼`/`·` tick direction, glyph +
    text health badges — and when `NO_COLOR` is set the `Theme` drops every
    foreground/background color and keeps only intensity + the markers (asserted by
    tests that the rendered span carries no `fg`/`bg` but the glyph survives).
  - **Truthful one-line status bar + generated keybar, tick-driven animation.** The
    status bar shows provider / health badge / mode plus a braille spinner in motion
    states (loading / reconnecting / playing), driven by an `App.tick_count`
    advanced on every tick and read **purely in `draw`** — never a wall-clock read.
    A tick sets `dirty` **iff** the app is in a motion state (`App::is_in_motion`) or
    a hint decayed, so the spinner actually animates during the initial connect /
    reconnect / playback while a truly idle, non-motion app still parks and never
    redraws on a tick. Venue/user strings on the status line are stripped of
    control/escape characters at the render edge.
  - **Responsive chain column-drop order + cross-screen too-small guard.**
    `greek_columns_for_slots` fixes the drop **order** `Γ → ν → Θ` (Θ retained
    first, Γ dropped first) for the chain matrix (the columns themselves land in
    #18); below the minimum size (`MIN_WIDTH`×`MIN_HEIGHT`), `render` shows the
    cross-screen "widen the terminal" state instead of a corrupt layout, on any
    screen.
  - **Layering respected: keymap in the application layer, rendering in ui.** The
    ratatui-free keymap data + resolution (`KeyChord`/`Context`/`Action`/`Binding`/
    `KEYMAP`, `GlobalCommand`/`resolve_global`/`resolve_replay`/`help_bindings`)
    lives in `src/app/keymap.rs`; the `ratatui`-dependent rendering (`Theme`,
    `StrikeRelation` + its marker spans, `GreekColumn`/`greek_columns_for_slots`,
    the markers/spans, `MIN_WIDTH`/`MIN_HEIGHT`/`is_too_small`, and the status/keybar/
    overlay renderers) stays in `src/ui/theme.rs`. So `ui → application` holds and no
    application/domain/provider module imports `ui` — the single-source-of-truth
    guarantee is preserved with dispatch and overlay reading one table. Both surfaces
    are re-exported from the crate root (keymap from `app::keymap`, theme from
    `ui::theme`) for the chain matrix (#18) and the render goldens (#19). `App` gains
    `no_color`, `tick_count`, a transient `status_hint` with a `hint_ttl`, and an
    `is_in_motion` predicate; `App::dispatch_key_global` is refactored to read the map
    without breaking #9's / #13's tests. **No new dependency.** Tests: 5 in
    `src/app/keymap.rs` (map↔overlay cross-check for globals and replay, screen-switch
    slot binding, unmapped-key, chord normalization) + 19 in `src/ui/theme.rs`
    (`NO_COLOR` strips color but keeps every marker, theme resolution, `StrikeRelation`
    K/S bucketing, the `Γ→ν→Θ` drop order, the too-small guard + the widen state
    through `render`, the number-key hint / `Tab` skip, modal precedence, the overlay
    fitting 80x24 with every section, and the deferred replay-Payoff listing) + the
    dispatch/tick regressions in `src/app.rs` (out-of-vocab modal swallow, motion-tick
    animates while idle-tick parks, hint decays after N ticks) — 325 lib tests total.
- The synchronous render loop, the pure total draw dispatch, and the two-level
  key input (`src/ui/mod.rs`, `src/ui/driver.rs`, `src/ui/{chain,depth,surface,payoff,replay}.rs`,
  issue #13; `docs/02-tui-architecture.md` §7, §8, §9, §12,
  `docs/05-views-and-ux.md` §2, §8). The terminal layer's render seam: a draw path
  that is a pure function of app state, an event-driven loop that parks and redraws
  only when dirty, and the tick/input task seams the supervisor (#11) owns. Key
  behaviours:
  - **`render(&App, &mut Frame)` is pure and the dispatch is total, wildcard-free.**
    `render` takes `&App` (never `&mut`), so a draw cannot mutate state or perform
    I/O — the purity guarantee is enforced by the signature. It lays out the root
    (`layout_root`: status bar + body + hint line, via `Layout::areas` so there is
    no unchecked index and a zero-size area yields empty regions, never a panic),
    draws a minimal status placeholder, then the **mode-first, screen-exhaustive**
    match with **no `_` arm** — `Mode::Live(s) => match s.screen { Chain | Depth |
    Surface | Payoff }`, `Mode::Replay(s) => match s.screen { Replay | Payoff }` —
    then the help overlay when open. Adding a screen variant forces the matching
    mode arm to be revisited by the compiler.
  - **Screen-shaped module boundary.** Each screen (`chain`/`depth`/`surface`/
    `payoff`/`replay`) exposes a pure `draw(&State, &mut Frame, Rect)` and
    `handle_key(&mut State, KeyEvent) -> Option<AppEvent>` with honest placeholder
    bodies (a titled block — the real chain matrix is #18, the others v0.2/v0.3/
    v0.5); no I/O, no `.await`, no `GraphData` build in `draw`. The replay screen's
    `handle_key` demonstrates the seam: a scrub key returns `AppEvent::ReplaySeek`
    the loop folds back, so the widget emits an event rather than seeking inline.
  - **Event-driven render loop, redraw only when dirty.** `run_render_loop` runs on
    a dedicated blocking thread and **parks** on the bounded `AppEvent` channel via
    `blocking_recv` — no busy-poll. Per event it folds it (two-level key dispatch),
    **pumps the #10 `EventBridge` between frames** (draining coalesced quotes/Greeks/
    depth + the priority control channel and routing commands), and redraws **only
    when `App::dirty`**, clearing `dirty` after the draw; it breaks on
    `App::should_quit` and returns when the channel closes. The tick (default ~250 ms
    from `config.tick_interval`) is the bridge's flush cadence, so market updates are
    folded at least every tick with zero spinning.
  - **Two-level key dispatch, closed sets wildcard-free.** `App::dispatch_key_global`
    handles the globals (`q`/`Ctrl-C` quit, `?` help, `r` reconnect, `R` rediscover)
    and the **modal-help intercept** (only `?`/`Esc` close it; every other key is
    swallowed, never reaching the screen behind the overlay), returning a `KeyRoute`
    (`Consumed`/`ToScreen`); a `ToScreen` key is forwarded to the active screen's
    `handle_key`, whose follow-on `AppEvent` is folded back. Both the `AppEvent` fold
    and the mode→screen forwarding are exhaustive with no `_` arm (crossterm
    `KeyCode`/`Event` are the only open vocabularies). Extends `App::on_key` without
    breaking #9's tests.
  - **Tick + input tasks are supervisor-owned seams (§12).** `spawn_tick_task`
    (`tokio::spawn` + `interval` with `MissedTickBehavior::Skip`, `select!` on its
    child `CancellationToken`, non-blocking `try_send` so a full channel drops a
    harmless tick and a closed one ends the task) and `spawn_input_reader`
    (`spawn_blocking` polling with a bounded 100 ms timeout so cancellation is
    observed, `blocking_send` so a slow render never drops a keystroke, ignoring
    mouse/focus/paste per the v1 keyboard-only model) each return a `JoinHandle` the
    composition wraps in `TokioTask` and registers with the `Supervisor` (ancillary
    tasks; the render loop is the render task on `spawn_blocking`). The composition
    recipe is documented in `src/ui/driver.rs`.
  - **Tested with `TestBackend` and mocks; no socket, no real clock.** `render`,
    `layout_root`, the loop `step`, the two-level dispatch, and the crossterm-event
    normalization are unit-tested with a `ratatui::backend::TestBackend` and a
    crate-internal `App` test-support builder; the tick task is asserted on a paused
    virtual clock (zero real wait). `render`, `layout_root`, `RootLayout`,
    `run_render_loop`, `event_channel`, `EVENT_CHANNEL_CAPACITY`, `spawn_tick_task`,
    `spawn_input_reader`, and `KeyRoute` are re-exported from the crate root. **No new
    dependency and no new `tokio`/`crossterm` feature** — the render loop uses the
    `sync`/`rt`/`macros`/`time` features already present from #11 and crossterm's
    default `events`. Tests: 19 in-module (`src/ui/mod.rs` layout/purity/reachable-
    screen + `prop_render_never_panics` over both modes × every reachable screen ×
    help × every live load state × terminal sizes from 1x1; `src/ui/driver.rs`
    dirty-gated `step`, parked-loop drain/quit, two-level dispatch incl. modal-swallow
    and replay-scrub forward, event normalization, and the two tick-task lifecycle
    tests) plus 4 in `src/app.rs` (`dispatch_key_global` route/modal/non-press) —
    23 new; 298 lib tests total.
- The application-owned `ProviderRegistry` and the `ChainViewApp` builder
  (`src/app/registry.rs`, issue #12; `docs/02-tui-architecture.md` §11,
  `docs/03-data-providers.md` §9, ADR-0006). The open provider-extension entry
  point: the stock binary and any external thin binary compose the app through
  `ChainViewApp::builder()` and drive it with `run()`, so a developer plugs in
  their own venue with no fork and no central enum to edit. Key behaviours:
  - **Collision is a typed startup error, never a panic or silent last-wins.**
    `register(impl Provider + 'static)` reads `provider.id()`: a **reserved**
    built-in id used through the external path records
    `RegistryError::ReservedId`, a **duplicate** id records
    `RegistryError::DuplicateId`, a **gated** built-in requested via
    `with_gated_builtin(id)` records `RegistryError::Gated(id)`, and an **empty**
    registry at `run()` is `RegistryError::Empty` — all surface as
    `ChainViewError::Registry`. Every builder method returns `Self`; build-phase
    errors are accumulated first-error-wins and reported by `run()`, so no method
    returns a mid-chain `Result`.
  - **`with_builtins()` is an honest no-op in v0.1.** The only gate-clear
    built-in is Deribit (public, no auth), whose adapter lands in #15/#16 — so
    **no fake provider is fabricated** and `builder().with_builtins().run()`
    reports `RegistryError::Empty` until Deribit is registered here. The external
    `register()` path and the collision/empty validation are fully exercised
    today.
  - **`with_gated_builtin` fails while the gate holds.** No gated adapter ships
    in v0.1 (`docs/SECURITY.md` §2.3–§2.4), so the gate always holds and the opt-in
    records `RegistryError::Gated`; this is the *mechanism*, exercised in v0.4. It
    also resolves the CV-CODEX-051 drift: the concrete typed error is
    `RegistryError::Gated` (a runtime hard gate; gated adapter code absent), and
    the unattached `ChainViewError::ProviderGated` sketch is removed from
    `docs/02-tui-architecture.md` §11.
  - **`--provider` resolution and the capability-driven composite-source guard.**
    `run()` resolves `config.provider` against the registry: an absent id is
    `ConfigError::UnknownProvider` (a syntactically invalid id is
    `ConfigError::InvalidValue` at the `ProviderId::new` grammar gate, before it
    can reach the registry). The selected provider's capabilities are read
    **once** and wired into `App`'s `SourceBinding`; a **chain-less** provider
    (standalone dxlink) selected as the live *source* is `ConfigError::InvalidValue`
    — the composite-source guard, which reads the declared `ChainCapability`,
    **never** matches a `ProviderId`. Replay mode needs no live provider and skips
    resolution.
  - **`Arc<dyn Provider>`, immutable after validation; registry is UI-unreachable.**
    Each adapter is stored behind an `Arc` so one adapter is shared read-only
    across the poll + stream tasks (#13/#16) without re-fetching; the registry is
    immutable once `run()` validates it. `ProviderRegistry` is opaque
    (private field, assembled only through the builder) and **not** re-exported —
    the UI-facing gating seam is the `SourceBinding`'s `ProviderCapabilities` +
    `ProviderId`, never the registry or a `dyn Provider`.
  - **The composition seam is documented, not spun.** `run()` validates the
    registry and resolves the live source, then returns `Ok(())`; the tokio
    runtime, the `Supervisor` (#11), the bounded channels (#10), the seeded
    `ChainStore` (from the provider's first fetch, #15), and the render loop (#13)
    are assembled at the documented seam. A test-only `FakeProvider` implementing
    the public `Provider` port exercises `register()` without a real adapter
    (prefiguring the #22 faux provider). `ChainViewApp` and `ChainViewAppBuilder`
    are re-exported from the crate root. Adds `RegistryError::Gated` to `src/error.rs`
    (pre-v0.1 addition to an unshipped surface — no SemVer event). **No new
    dependency.** Tests: 13 unit + 3 property (`prop_registry_rejects_reserved_id`,
    `prop_registry_rejects_duplicate_id`, `prop_capabilities_total` — gating total
    over declared capabilities, never id) in `src/app/registry.rs`, plus the
    `RegistryError::Gated` display test in `src/error.rs`.
- The single task supervisor, cancellation-token tree, and ordered teardown
  (`src/app/supervisor.rs`, issue #11; `docs/02-tui-architecture.md` §12,
  ADR-0005). One `Supervisor`, owned by the application layer, owns **all** task
  handles and a root `tokio_util::sync::CancellationToken` so the invariant
  "every spawned task has a shutdown path" is enforceable process-wide. Key
  behaviours:
  - **Cancellation-token tree.** A root token has one `child_token` per task
    (`Supervisor::child_token`); cancelling the root cascades to every child,
    and `cancel_provider(id)` cancels a **single** provider's child without
    touching the others or the root (used on `Unsubscribe`/`Rediscover`). The
    supervisor coordinates by tokens + join handles, **never a lock across an
    `.await`** (`rules/global_rules.md` — Concurrency).
  - **All triggers converge on one teardown.** A clean quit (`request_quit`,
    wired from `App::should_quit`), a startup / provider-past-budget / channel
    close failure (`fail`), and a **panic in any task** (reported through the
    `ExitReporter` seam as `TaskExit::Panicked`, detected via a `JoinHandle`
    join result whose `JoinError::is_panic()` is true — `TokioTask::join`) all
    trip the root token. The panic path is a task-level fatal signal the
    supervisor records itself; it does **not** rely on the process panic hook
    alone (that only restores the terminal). No trigger leaves an orphan.
  - **Deterministic join order, terminal restored LAST.** Teardown (1) cancels +
    joins the **provider** tasks, then (2) the **input/tick/replay** tasks, then
    (3) lets the **render** task exit, and only then (4) runs the `FinalTeardown`
    that restores the terminal (`GuardTeardown` drops the #8 `TerminalGuard`) —
    the strictly-last step on every path, including panic.
  - **Bounded join, then abort.** Each join has a `DEFAULT_JOIN_BUDGET` (2 s)
    `tokio::time::timeout`; a task that ignores cancellation past the budget is
    `abort()`ed so a wedged upstream socket can never hang exit. The budget is
    asserted with a **controllable virtual clock** (`#[tokio::test(start_paused
    = true)]`), so the 2 s window is honored in virtual time with **zero real
    wall-clock wait**.
  - **Error propagation seam.** `run()` returns the first fatal `ExitCause`
    (`Clean` / `TaskPanicked` / `Failed(ChainViewError)`, first-fatal-wins), with
    `exit_code()` (0 clean, 1 failure) and a redaction-safe `failure_message()`.
    The supervisor **never** calls `std::process::exit` (that would bypass the
    guard `Drop`); `main` maps the returned cause to an exit code and prints the
    message on `stderr` **after** the terminal is restored (CLAUDE.md governance
    item 3).
  - **Testable with mocks, no socket / no real clock.** The `SupervisedTask`
    trait (real `TokioTask` vs. a recording mock) and the `FinalTeardown` trait
    (real `GuardTeardown` vs. a recorder) make the join-order and
    bounded-join-then-abort deterministic. `Supervisor`, `ExitCause`,
    `ExitReporter`, `FinalTeardown`, `GuardTeardown`, `SupervisedTask`,
    `TaskExit`, `TokioTask`, and `DEFAULT_JOIN_BUDGET` are re-exported from the
    crate root for the builder (#12) / render loop (#13) / `main.rs` to wire.
    Sequencing the guard last (#8), the channels (#10), the render loop (#13),
    and the provider reconnect internals (#16) are left as clean seams.
    Tests: 13 in-module (exit-code/message, request-quit cascade,
    cancel-provider isolation, bounded-join wedged-abort + cooperative-in-budget
    on a paused clock, a **real-`TokioTask`** wedged regression proving the
    timeout-then-abort truly cancels rather than detaching an orphan — it fails
    against a `take()`-based join, ordered provider→ancillary→render→restore,
    normal-quit all-join, reported-panic non-zero + restore-last + no-orphan,
    panic-at-join-over-clean-trigger, first-fatal-only, wedged-run
    abort-still-clean) plus 2 integration (`tests/supervisor_shutdown.rs`,
    real tokio tasks: normal-quit every-task-joined + terminal-restored, and an
    injected real provider panic → non-zero `TaskPanicked` exit, terminal
    restored, no orphan). Adds one dependency and extends `tokio` (audit notes):
  - `tokio-util` `0.7` (`default-features = false`) — only
    `tokio_util::sync::CancellationToken` for the root + per-task child-token
    tree (no codec/io features). RUSTSEC-clean; MSRV 1.71 (below our 1.85).
    Explicit-approval addition (CLAUDE.md "Coding Rules").
  - `tokio` gains `rt` / `macros` / `time` on top of the existing `sync`. This
    **supersedes** the two earlier notes that pinned tokio to `["sync"]`-only —
    the #6 provider-port entry ("no runtime / macros / net yet — the full runtime
    features land with the adapters and app loop in later issues") and the #10
    bridge entry ("no new `tokio` features … no `rt`/`macros`/`time` are pulled");
    #11 is that "later issue". `rt` for `JoinHandle`/`abort`/`JoinError::is_panic`,
    `macros` for the supervise `tokio::select!`, `time` for the bounded-join
    `tokio::time::timeout`. Still no `net`/`fs`/`rt-multi-thread` — the render loop
    (#13) picks the runtime flavor. A **dev-only** `test-util` feature (in
    `[dev-dependencies]`, never in the release binary) enables the paused virtual
    clock for the no-wall-clock budget tests. RUSTSEC-clean.
- The two-class bounded, coalescing provider -> app bridge (`src/app/bridge.rs`,
  issue #10; `docs/02-tui-architecture.md` §5, `docs/06-performance.md` §3.2,
  `docs/03-data-providers.md` §5). `EventBridge` is the seam that joins the async
  data layer to the synchronous render-loop fan-in (`App::on_event`), draining
  only over **bounded** `tokio::sync::mpsc` channels — no unbounded channel exists
  on the provider -> app path. Key behaviours:
  - **Two-class backpressure.** A bounded **coalesced** channel carries
    `Quote`/`Greeks`/`Depth` (capacity from `config.channel_capacity`); a
    **separate, small** bounded **control** channel carries `Chain`/`Health`. The
    fan-in wakeup (`pump`/`pump_into`) drains the control channel **first**
    (priority) so a `Health(Reconnecting)` or a fresh `Chain` is delivered
    promptly even while the coalesced channel is saturated with stale quote
    traffic — health can never sit behind a quote burst.
  - **Consumer-side conflation, O(N) not O(burst).** The coalesced channel drains
    into a per-instrument staging map keyed by `InstrumentKey` — one slot per
    instrument, last-value-wins — then the current values flush into
    `App::on_event`. The map is bounded by the subscribed instrument count N,
    never by burst rate or session length; a dropped intermediate quote is not a
    correctness loss (the chain shows the freshest price). Within a slot a quote,
    a Greeks row, and a depth ladder are held independently, so a Greeks refresh
    never clobbers a pending quote and the slot count stays exactly N. This is the
    **consumer-side** stage of the two-stage coalescing design (`docs/02` §5); the
    **producer-side** overwrite-on-full staging that preserves the freshest value
    when the bounded channel is *full* landed with the Deribit adapter in #16
    (`ProducerStaging`), so the two-stage coalescing is now complete and the
    NFR-15 latest-value-wins guarantee holds even under sustained channel
    saturation.
  - **HP-3 allocation discipline.** The staging map **reuses** its allocation
    across bursts — a flush drains via `HashMap::drain` and an unsubscribe prunes
    via `HashMap::retain`, both of which retain the bucket allocation, and a
    repeat update for an already-staged instrument clones no key — so once grown
    to fit N it performs zero steady-state per-burst allocation.
  - **Lifecycle tracks the subscription set.** A slot is created on the first
    update, overwritten on subsequent ones, and **removed** when the instrument
    is unsubscribed: `pump` drains the render -> data command channel and prunes
    the staging map on each `Command::Unsubscribe` (an absolute-expiry unsubscribe
    prunes the matching expiry precisely; a relative-days one, which cannot be
    resolved without a wall clock the fan-in deliberately lacks, prunes the whole
    underlying), while forwarding every drained command to a caller `route`
    closure — the clean seam the task supervisor (#11) fills to reach the provider
    layer.
  - **Testable with no socket and no wall clock.** The drain is `try_recv`-based
    and reads no clock; `EventBridge::new(coalesced_capacity)` returns the bridge
    plus a `BridgeSenders` bundle (the producer halves the supervisor wires to the
    adapters and to `App`). The adapter that produces updates (#16), the
    supervisor that owns the channel ends (#11), and the render loop that pumps
    between frames (#13) are separate issues; the seams for each are explicit.
    `EventBridge`, `BridgeSenders`, and the `CONTROL_CHANNEL_CAPACITY` /
    `COMMAND_CHANNEL_CAPACITY` constants are re-exported from the crate root.
    Tests: 17 in-module — staging-map overwrite / lossless-per-kind (quote+greeks,
    depth+quote) / control-not-staged / remove-on-unsubscribe (absolute expiry,
    relative days, other-underlying no-op), capacity-reuse and latest-value-over-
    burst (HP-3, deterministic memory-flatness by asserting `capacity()` is stable
    and `len() <= N` across 1000 bursts), plus `EventBridge` priority-drain,
    health-delivered-while-saturated, burst-beyond-channel-capacity-keeps-memory-
    flat, every-instrument-receives-latest, unsubscribe-prunes, command-routed,
    and two `pump`-into-a-live-`App` folds. **No new dependency and no new `tokio`
    features**: `tokio::sync::mpsc` channels with `try_send`/`try_recv` are
    runtime-free, so the existing `["sync"]` feature suffices — no `rt`/`macros`/
    `time` are pulled, keeping runtime features minimal.
- The application state machine and synchronous event fan-in (`src/app.rs`,
  `src/event.rs`, issue #9; `docs/02-tui-architecture.md` §3, §4). `App` owns all
  render-loop state as a `Live | Replay` `Mode` machine; the fan-in folds every
  event into state and keeps `ratatui` off the async executor. Key behaviours:
  - **Mode-scoped screens make out-of-mode pairs unrepresentable.** `LiveScreen
    { Chain, Depth, Surface, Payoff }` and `ReplayScreen { Replay, Payoff }` are
    owned by their mode's state, so `Replay` + `Chain` cannot be constructed — the
    type system, not a runtime fallback, prevents it, and the render dispatch (#13)
    stays a total, wildcard-free match.
  - **One exhaustive, wildcard-free fan-in.** `App::on_event` folds each
    `AppEvent { Key, Resize, Tick, Market, ReplaySeek }` in a single match with no
    `_` arm and sets `dirty` on any mutation; adding a variant forces every fold
    site to be revisited by the compiler. `Market(MarketUpdate)` folds into the
    `ChainStore` (`Quote`/`Greeks` → the merge path, `Chain` → a snapshot-driven
    `apply_poll`, `Health` → the correct side's badge); an idle tick does not set
    `dirty`.
  - **I/O never runs inline.** `on_event` is synchronous and never `.await`s; a
    handler that needs I/O (reconnect, re-discover, seek/reload the bundle,
    subscribe) emits a typed `Command { Subscribe, Unsubscribe, Reconnect,
    Rediscover, SeekBundle, ReloadBundle }` on a bounded command channel via a
    non-blocking `try_send`.
  - **Per-side composite health.** `LiveState` binds a `SourceBinding` plus an
    optional `OverlayBinding`, each carrying its own `ProviderCapabilities` and
    `StreamHealth`; a health transition routes to the matching side by id equality,
    so either side failing degrades only that side.
  - **Capability-driven reachability, never a `ProviderId` match.** The
    `is_screen_reachable(screen, caps)` helper and `LiveState::set_screen` gate on
    declared `ProviderCapabilities` (source ∪ overlay), so a screen is only ever
    set to a reachable value and a built-in and an external provider are gated
    identically. The `Tab` skip / number-key hint mechanics land in #13/#14.
  - **Documented stubs with stable shapes.** `ReplayState`/`BundleLoad`/
    `LoadedReplay`/`Playback` (v0.3) and `PayoffBuilder` (v0.2) are typed
    placeholders whose enum/struct shapes are fixed now so later work fills the
    internals without a breaking change; `StatusLine`/`Selection`/`ScreenLoad` are
    minimal, typed state the render loop (#13/#14) drives.
- The terminal lifecycle: the RAII `TerminalGuard` and the panic-hook restore
  (`src/terminal.rs`, issue #8; `docs/02-tui-architecture.md` §6, ADR-0001). The
  guard's constructor enables raw mode, enters the alternate screen, and hides the
  cursor; its `Drop` runs the exact inverse, so the terminal is restored on
  **every** exit path — a normal return, an early `?`, or a panic. Key behaviours:
  - **Transactional setup, tolerant teardown.** Setup records each applied step;
    a mid-sequence failure rolls back exactly the applied prefix and returns
    `ChainViewError::Terminal`, so a rejected setup (e.g. no TTY) leaves the shell
    clean. Teardown is best-effort, infallible, and idempotent — a
    partially-initialized guard and a double teardown both restore without
    panicking (a `restored` latch guarantees at-most-once).
  - **Panic hook chains, never swallows.** `install_panic_hook` captures the
    previously installed hook via `std::panic::take_hook`, restores the terminal
    first (show cursor, leave alternate screen, disable raw mode — synchronous,
    allocation-light, errors ignored), then invokes the captured hook, so the
    backtrace prints on a normal (non-raw) screen and is never lost.
  - **TTY-less testability.** The low-level operations are abstracted over a
    crate-internal `TerminalOps` trait; unit tests drive a recording fake to
    assert the setup order, the inverse teardown order, idempotent double-restore,
    partial-setup tolerance, setup-failure rollback, and teardown error tolerance
    — all deterministic, with no real terminal. The restore-before-chain ordering
    is proved by a small `restore_then_chain` primitive tested with fakes (no
    process-global hook). The concrete `crossterm` path (`CrosstermOps`) is
    exercised end to end by a `harness = false` subprocess in
    `tests/terminal_restore.rs`: the child installs the real hook and panics; the
    parent asserts the leave-alternate-screen + show-cursor escapes reach the
    child's stdout (restore ran) and the panic marker reaches stderr (chained hook
    not swallowed), and that the child exits non-zero without hanging.
  - **`main.rs` wiring.** Startup installs the panic hook, then enters the guard,
    so the render loop (#13) will be wrapped by a guaranteed restore. `main`
    returns the typed `ChainViewError` (`ConfigError` folds in via `#[from]`), so
    the `main.rs`-only `anyhow` deviation gate (`clippy.toml`) is left untouched.
    No `std::process::exit` bypasses `Drop`; the supervised, ordered teardown that
    sequences the guard last lands in #11. `TerminalGuard` and `install_panic_hook`
    are re-exported from the crate root so an external thin binary (ADR-0006) can
    drive the same restore.
  - Tests: 8 unit (`src/terminal.rs`) plus the subprocess integration harness.
  Adds the first two TUI dependencies (audit notes):
  - `ratatui` `0.29` (`features = ["crossterm"]`) — the widget/layout library
    (ADR-0001), first TUI pull approved by this issue. `ratatui` `0.30` requires
    rustc 1.88, above the crate's 1.85 MSRV, so the resolver pins `0.29`, which
    re-exports `crossterm` `0.28.1`. `RUSTSEC`-clean at this revision; the
    de-facto standard Rust TUI toolkit.
  - `crossterm` `0.28` — the terminal backend (raw mode, alternate screen, cursor
    control) named by ADR-0001; cross-platform including Windows. Pinned to the
    same `0.28` line `ratatui` `0.29` drives, so cargo unifies to a **single**
    `crossterm` instance — the one ChainView calls is exactly the one `ratatui`
    drives, with no two-version mismatch. `RUSTSEC`-clean.
- The live `ChainStore` (`src/chain/store.rs`, issue #7): the deterministic
  poll -> stream merge over the `optionstratlib` chain
  (`docs/01-domain-model.md` §5.1, §6, `docs/03-data-providers.md` §3, §4).
  Assembled from a `ChainFetch` via `ChainStore::seed`, carrying the same
  `AliasCatalog` forward with no re-derivation; `apply_poll` / `apply_quote` /
  `apply_greeks` / `apply_health` mutate it **only on the market/tick event**,
  never in draw, and `snapshot()` emits a `ChainSnapshot`. Key behaviours:
  - **Strike-keyed clone/patch/re-insert row update.** A `QuoteUpdate` /
    `GreeksRow` takes the row at its strike out of the upstream
    `BTreeSet<OptionData>` (via a strike-only probe, since `OptionData`'s `Ord`
    is its `strike_price`), clone-patches only the update's `OptionStyle` side,
    recomputes that side's `*_middle` (upstream's `(bid+ask)/2` rounded to 4 dp),
    and re-inserts — opposite leg and untouched fields preserved, set ordering
    unchanged.
  - **Field-fold rules.** A rejected (absent) field keeps its prior value; a
    **crossed** quote (`ask < bid`, or a zero ask on a non-zero bid) rejects the
    whole update and keeps the prior row (a zero bid is valid). `theta`/`vega`/
    `rho` have no `OptionData` field and are intentionally not folded — the
    per-style analytics sidecar lands in v0.2 (`docs/01` §7).
  - **Bounded-generation merge.** A monotonic snapshot generation stamps each
    poll; a de-listed strike is tombstoned (and never resurrected), a re-listed
    strike clears its tombstone. A stream update for an unknown strike is held in
    a bounded `MAX_PENDING` (256) FIFO buffer with a `pending_ttl` per-entry TTL
    (`refresh_interval` + slack); on overflow the oldest entry is dropped
    (counted via `dropped_overflow`); on the next poll a pending update whose
    strike is now present is applied, a tombstoned or past-TTL one is dropped.
    A stream update for a tombstoned strike is dropped immediately.
  - **Two-clock freshness (§5.1).** A per-instrument watermark = `max(event_time)`
    drops an out-of-order update (event time below the watermark) for value and
    direction and counts it (`dropped_stale`); a `None`-`event_time` update
    orders by receipt and never advances the watermark. Per-component staleness
    (`quote_freshness` / `greeks_freshness` / `chain_freshness`) against
    `QUOTE_STALE_AFTER` / `GREEKS_STALE_AFTER` / `chain_stale_after`, plus a
    feed-delay `Delayed` classification past `FEED_DELAY_WARN` with negative skew
    clamped to zero — surfaced as the new `Freshness` enum.
  - **Retained/decayed price direction.** Per-instrument prev bid/ask + last
    change time drive a `TickDir` (Up on strictly higher, Down on strictly lower,
    an equal value keeps the prior, first-ever `Flat`), decayed to `Flat` after
    `DIRECTION_DECAY` (3 s) and cleared to `Flat` immediately on a
    stale/reconnecting `apply_health` — mutated on the event, read pure in draw.
  - **Cross-provider overlay gate wired.** A leg whose overlay feed differs from
    the source provider merges only when `AliasCatalog::overlay_compatible`
    passes; a `ContractSpecFingerprint` mismatch refuses the leg
    (`MergeOutcome::OverlayRefused`), keeps the source leg, and badges it
    (`is_overlay_refused`); the within-provider merge is a no-op for the gate.
  - The `ChainStore`, `Freshness`, `MergeOutcome`, `TickDir`, `MAX_PENDING`, and
    `pending_ttl` are re-exported from the crate root; `ChainSnapshot` /
    `ChainSource` / `StreamHealth` stay in `src/chain/events.rs` with unchanged
    re-export paths (the forward declarations already matched the store's needs).
    Tests: 32 unit (clone/patch both legs and orders, crossed/zero/missing folds,
    staleness/delay/negative-skew, out-of-order + watermark, direction
    up/down/equal/decay/stale-clear, tombstone no-resurrection, pending
    new-listing/TTL/overflow, overlay gate) plus 4 property
    (`prop_chain_merge_idempotent`, `prop_overlay_spec_gate`,
    `prop_no_resurrection_and_bounded_memory` over scripted poll/stream
    interleavings, `prop_freshness_out_of_order_keeps_max_event_value`). No new
    dependency.
- The PUBLIC, semver-governed **provider port** (`src/providers/mod.rs`, issue #6):
  the `#[async_trait] Provider: Send + Sync` trait (`id` / `capabilities` /
  `discover` / `fetch_chain` / `subscribe`) an external adapter compiles against
  to plug in its own venue (`docs/03-data-providers.md` §2, §11.1, ADR-0006). The
  `#[non_exhaustive] ProviderCapabilities` capability self-declaration with its
  `ProviderCapabilitiesBuilder` — the ONLY cross-crate construction path — plus
  the `#[non_exhaustive]` dimension enums `ChainCapability` (Native / Assemble /
  Partial / None), `GreeksCapability` (Provided / ComputedLocally / None),
  `OptionStreamCapability` (None / SymbolOnly{verified} / ChainQuotes{verified}),
  `ChainPollCapability` (None / Poll{interval_hint_secs: u32}), and `AuthKind`
  (None / Token / KeySecret / UserPass). Streaming is **three independent
  dimensions** (`option_stream` / `underlying_stream` / `chain_poll`) so a
  real-time underlying is never mis-badged as a real-time option chain; every
  dimension defaults to its least-capable variant, so a future field lands with a
  safe default and keeps external adapters compiling and honest
  (`docs/SEMVER.md`). The port helper types `UnderlyingRef`,
  `SubscriptionRequest`, and the drop-cancels `SubscriptionHandle` (a `Send`
  cancel closure so the port stays agnostic to the adapter's cancellation
  mechanism). The `async_trait` per-call allocation is accepted and doc-noted —
  provider methods are cold-path, the hot render loop holds no `dyn Provider`.
- The named fetch artifact (`src/chain/fetch.rs`, issue #6): `ChainFetch { chain,
  expiry_source, aliases }` — the artifact `Provider::fetch_chain` returns,
  **never** a bare `OptionChain`, so the poll leg preserves the absolute-UTC
  expiry/source identity (`ExpirySource { underlying, expiration_utc, provider }`)
  and the per-leg `AliasCatalog` the merge/subscription/resubscription/DXLink
  overlay joins need (`docs/01-domain-model.md` §6, `docs/03-data-providers.md`
  §2, §4). `AliasCatalog` maps the provider-agnostic `InstrumentKey` to each
  feed's `Instrument` (native + stream symbols + spec fingerprint) with
  `instrument()`, `resolve_symbol()` (native AND stream symbol → shared key), and
  `overlay_compatible()` — the cross-provider economic-equivalence gate returning
  `Result<(), OverlayError>` with the **real `ContractSpecFingerprint`
  comparison** wired (first disagreeing dimension → `OverlayError::SpecMismatch`;
  the within-provider merge is a no-op; the store-level *invocation* lands in #7).
  These are DOMAIN types (the trait emits them, the future `ChainStore` consumes
  them) defined in `src/chain/*` and re-exported through the port surface so the
  module graph stays acyclic (port → domain, never domain → port,
  `docs/03-data-providers.md` §12). Now that `AliasCatalog` and a trivial
  `ChainSource` (Poll / Stream / Merged) enum exist, the forward-declared
  `ChainSnapshot` (issue #5) gains its documented `aliases: AliasCatalog` and
  `source: ChainSource` fields (the store LOGIC that drives them still lands in
  #7). The full port surface — trait, capabilities + builder + enums,
  `ChainFetch`/`ExpirySource`/`AliasCatalog`, and the helper types — is
  re-exported from the crate root. Adds two runtime dependencies (audit notes):
  - `async-trait` `0.1` — object-safe `async fn` methods on the `Provider` trait
    (the port must be `dyn`-dispatched via the `ProviderRegistry`, issue #12).
    The per-call box allocation is cold-path only. Ubiquitous, `RUSTSEC`-clean;
    the standard way to express an object-safe async trait on stable Rust.
  - `tokio` `1` (`default-features = false`, `features = ["sync"]`) — only
    `tokio::sync::mpsc::Sender<MarketUpdate>` for the `subscribe` bounded fan-in
    channel (`docs/03-data-providers.md` §5). Minimal features: **no** runtime /
    macros / net yet — the full runtime features land with the adapters and app
    loop in later issues. `RUSTSEC`-clean; the mandated async runtime
    (`rules/global_rules.md` "Concurrency").
- Normalized streaming update events and freshness clocks
  (`src/chain/events.rs`, issue #5): the DOMAIN payloads a provider emits across
  the seam (`docs/01-domain-model.md` §5 and §5.1). `QuoteUpdate` (bid/ask/last/
  bid_size/ask_size), `GreeksRow` (iv + delta/gamma/theta/vega/rho + a
  `GreeksOrigin` tag), and `DepthLadder`/`DepthLevel` (best-first bids/asks +
  an `Option<u64>` `change_id` for Deribit gap-detect/resync) — **every numeric
  field is `Option`** so a value the feed omits stays `None` and renders as an
  em dash, never a fabricated zero; quotes/IV/Greeks are `Positive`/`Decimal`
  **display analytics**, never accounting values. Each event carries the **two
  clocks** of §5.1: `event_time` (venue timestamp, optional) and `received_time`
  (normalization time, always present). The closed `MarketUpdate` fan-in enum
  (`Quote`/`Greeks`/`Depth`/`Chain`/`Health`) matched exhaustively downstream
  with no wildcard `_` arm, plus **thin forward declarations** of the store
  types `ChainSnapshot` and `StreamHealth` (self-contained fields only —
  `aliases: AliasCatalog` and `source: ChainSource` are **completed with the
  chain store in issues #6/#7**) so the enum can be closed now. The named
  freshness thresholds with no magic numbers — `QUOTE_STALE_AFTER` (5 s),
  `GREEKS_STALE_AFTER` (10 s), `FEED_DELAY_WARN` (2 s), `DIRECTION_DECAY` (3 s),
  and `CHAIN_STALE_SLACK` (2 s) with the `chain_stale_after(refresh_interval)`
  helper that fixes §5.1's `CHAIN_STALE_AFTER = refresh_interval + slack`
  formula (the store applies the comparison in #7). The event types, the
  `MarketUpdate` enum, the forward-declared `ChainSnapshot`/`StreamHealth`, and
  the threshold constants/helper are re-exported from the crate root. **No new
  dependency**: Greeks use the `Decimal` (`rust_decimal`) that `optionstratlib`
  already re-exports through its prelude.
- Normalized instrument identity (`src/chain/identity.rs`, issue #4): the
  provider-agnostic `InstrumentKey` (`underlying`, absolute-UTC
  `expiration_utc`, `strike`, `style`) with `Eq`/`Hash` over all four fields and
  deliberately **no** `ProviderId` — so a REST snapshot row and a stream overlay
  for the same option collapse to one map entry; the `ContractSpecFingerprint`
  economic-equivalence spec (`contract_multiplier`, `settlement`, `exercise`,
  `quote_currency`, `venue_product_code`) with the `SettlementStyle`/
  `ExerciseStyle` (`#[repr(u8)]`) enums, deriving `Eq`/`Hash` so the
  cross-provider overlay gate (issue #7) can compare it by value; and the
  `Instrument` view (key + owning `ProviderId` + native/stream aliases + spec)
  with **hand-written** `PartialEq`/`Eq`/`Hash` delegating to `key` only. The
  open, validated `ProviderId` newtype is completed from its issue #2/#3
  placeholder into the full form: a fallible `new()` → `ConfigError::InvalidValue`,
  `as_str()`, `is_reserved()`, `serde` via `try_from = "String"` / `into =
  "String"` (re-validates on the way in), `PartialOrd`/`Ord` retained for
  `Config`'s `BTreeMap`, not `Copy`; plus `RESERVED_PROVIDER_IDS` (the five
  built-in ids). `validate_provider_id` in `src/config.rs` now delegates to
  `ProviderId::new`. The identity types, the style enums, and
  `RESERVED_PROVIDER_IDS` are re-exported from the crate root, alongside
  `optionstratlib`'s `Positive` and `OptionStyle` (the domain numeric
  vocabulary on the public identity surface). Nothing may `match` on a
  `ProviderId` (documented; arch test lands in issue #22).
  Adds two runtime dependencies (audit notes):
  - `optionstratlib` `0.18.0` — the chain model and options math; supplies
    `Positive` (non-negative price/strike) and `OptionStyle` (call/put) on the
    identity surface. Default features are empty (`default = []`), so no
    tokio/reqwest/plotly is pulled; `RUSTSEC`-clean at this revision, first-party
    ecosystem crate. Named by `CLAUDE.md` "Key Decisions" as the mandated chain/
    math library.
  - `chrono` `0.4` (`default-features = false`, `features = ["std"]`) —
    `DateTime<Utc>` for the absolute-UTC expiry in `InstrumentKey`; the ecosystem
    timestamp type (`rules/global_rules.md` "Type Safety"). Minimal features (no
    `clock`/`serde`) requested; feature unification with `optionstratlib` adds no
    obligation. `RUSTSEC`-clean.
- Typed configuration surface (`src/config.rs`, issue #3): the immutable
  `Config` with `ProviderSettings`, `ThemeChoice`, and `ModeSelect`, assembled
  once at startup and validated into typed `ConfigError`s. A layered loader with
  `CLI flag > env (CHAINVIEW_*) > optional TOML file > typed default` precedence
  (`Config::assemble` is pure over an injectable `EnvSource`; `Config::load`
  does the XDG-aware file read, while `.env` is loaded by `main.rs` startup
  glue before config assembly); `humantime` durations with range checks
  (refresh `[250ms,300s]`, tick `[50ms,5s]`, channel-cap `[64,65536]`);
  `deny_unknown_fields` typo protection on the file. Env-only secrets: a
  credential key in a file is rejected, a missing required credential is
  `MissingCredential(provider)` (names the provider, never the key), and
  resolved values are wrapped in a redacted `Secret`. The reversible provider-id
  ↔ env-segment bijection (`encode_segment`/`decode_segment`/`provider_env_var`,
  `'_'→'__'`, `'-'→'_'`). The zero-config default resolves to Deribit BTC. The
  CLI grammar in `src/main.rs` (clap derive) including the `chainview replay
  <dir>` subcommand → `ModeSelect::Replay`. The `config` module and the headline
  types (`Config`, `CliOverrides`, `ModeSelect`, `ProviderSettings`,
  `ThemeChoice`) are re-exported as the public config surface; `ProviderId` gains
  `PartialOrd`/`Ord` so it can key `Config::providers`.
  Adds five runtime dependencies and one dev dependency, each named by
  `docs/07-configuration.md` §4 or required by the spec (audit notes):
  - `serde` (derive) — typed deserialize of the optional TOML file and the
    config enums; ubiquitous, `RUSTSEC`-clean, already ecosystem-standard.
  - `toml` — parse `~/.config/chainview/config.toml`; the canonical file format
    named in §2/§4.
  - `humantime` — the duration grammar (`250ms`/`2s`/`5m`) mandated by §4.
  - `clap` (derive) — the CLI grammar in `main.rs`; §4 names no CLI crate, so
    clap-with-derive is chosen and recorded here as the decision.
  - `dotenvy` — load `.env` at startup (§2); the maintained successor to the
    unmaintained `dotenv` crate.
  - `proptest` (dev) — property tests for the id↔env bijection and the
    humantime-parse ⇄ range gate (`docs/TESTING.md` §3).
  Design note: the documented §5.1 transliteration is a bijection only over ids
  with non-adjacent separators (realistic ids); it is not injective for the full
  grammar (`encode("a--") == encode("a_")`). Implemented verbatim, documented,
  and handed to issue #4 (owner of the `ProviderId` grammar) to resolve.
- Boundary error types (`src/error.rs`): `ChainViewError` and its per-boundary
  source enums — `ProviderError`, `BundleError`, `ConfigError`, `RegistryError`,
  and `OverlayError` — via `thiserror`, plus the `Redacted` trait and
  `TransportDetail`/`TransportKind` and the closed `NormalizeKind`. Redaction-safe
  by construction: no upstream error type reaches a widget and no secret can be
  interpolated into any `Display` (transport detail is a category + status only;
  normalize failures name a field, never a value). `ProviderError` converts into
  `ChainViewError` only through the explicit `ChainViewError::provider(id, source)`
  helper (the `Provider` variant carries the `ProviderId`); the other
  sub-boundaries convert via `#[from]`. A minimal `ProviderId` placeholder lands
  in `src/chain/mod.rs`, completed by #4. Re-exported from the crate root. Adds
  `thiserror` as the first runtime dependency.
- Bootstrap the single-crate (binary + lib) skeleton for v0.1: MSRV Rust 1.85
  on the 2024 edition, `#![forbid(unsafe_code)]` at both crate roots, the
  `[lints]` table (deny warnings, deny `unsafe_code`, clippy restriction
  lints), module stubs for `error` / `config` / `app` / `event` /
  `providers` / `chain` / `ui`, `rustfmt.toml` + `clippy.toml` (with `anyhow`
  scoped to `main.rs`/startup glue), the `make pre-push` toolchain skeleton,
  and `.env.example`. No runtime dependency added.

### Changed

- **MSRV raised from 1.85 to 1.88** (a minor bump per
  [docs/SEMVER.md §MSRV](docs/SEMVER.md), issue #20; `Cargo.toml`
  `rust-version = "1.88"`, the CI `msrv` job pin, and this callout move
  together). Two forces required it: `optionstratlib 0.18` → `zip` →
  `deflate64` uses the `unbounded_shr` intrinsic stabilized in Rust 1.87 (no
  1.85-compatible `deflate64` is resolvable), and adopting `time >=0.3.47` to
  **patch the RUSTSEC-2026-0009 DoS** (stack exhaustion) requires Rust 1.88.
  `time` was bumped **0.3.45 → 0.3.53** (pulling `num-conv`, `time-core`, and
  `time-macros` bumps in `Cargo.lock`), so the vulnerability is removed from the
  tree rather than ignored. A consumer on a toolchain below 1.88 must upgrade
  before `cargo install`.
- Tightened the `ProviderId` grammar from `^[a-z][a-z0-9_-]{1,31}$` to
  `^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$` (2–32 chars, `-`/`_` isolated between
  alphanumerics — no leading/trailing/adjacent separator), a strict superset
  check (issue #4, [ADR-0008](docs/adr/0008-provider-id-grammar-and-env-bijection.md)).
  This resolves the issue #3 non-injectivity defect in the
  `docs/07-configuration.md` §5.1 id ↔ env-segment transliteration
  (`encode("a--") == encode("a_") == "A__"`): under the tightened grammar the
  map is a **total bijection over the full valid-id space**, proved by property
  test (round-trip + no-collision) in `tests/property.rs`, which replaces the
  pinned-limitation test. All five built-ins and the documented examples
  (`my-broker`, `my_broker`, `td-ameritrade`) stay valid; `encode_segment`/
  `decode_segment` are unchanged and stay in `src/config.rs`. Pre-v0.1 narrowing
  of an unshipped surface — no SemVer event.

### Deprecated

### Removed

### Fixed

### Security

- **Root `SECURITY.md` vulnerability-report channel + frozen v1.0 supply-chain
  gate** (issue #54; new root `SECURITY.md`, `deny.toml`,
  `.github/workflows/ci.yml`, `docs/SECURITY.md` §7-§8, `docs/ROADMAP.md`). Lands
  the intake channel a researcher actually uses and freezes the `cargo audit` /
  `cargo deny` allow-lists as the reviewed v1.0 set. Docs + CI-config only — no
  runtime dependency added, no runtime surface changed.
  - **Root `SECURITY.md` (committed; GitHub Security tab).** Private
    vulnerability reporting on `joaquinbejar/ChainView` (preferred) with an email
    fallback (`jb@taunais.com`), a **90-day** coordinated-disclosure window, a
    pre-1.0 supported-versions statement, an in/out-of-scope note, and a pointer
    to the fuller `docs/SECURITY.md` posture. Acknowledgment/assessment aims are
    stated as **best-effort** for a solo maintainer; no fabricated PGP key and no
    contractual SLA.
  - **`deny.toml` frozen as the reviewed v1.0 policy.** The permissive license
    allow-list, the crates.io-only source allow-list, the `warn`-not-`deny`
    duplicate-versions policy, and the three transitive-only advisory ignores —
    RUSTSEC-2021-0141 (`dotenv`, unmaintained), RUSTSEC-2024-0436 (`paste`,
    unmaintained), RUSTSEC-2026-0002 (`lru`, unsound) — are frozen **unchanged**:
    each ignore keeps its pull-path reason and lift trigger; none is added or
    dropped. RUSTSEC-2026-0009 (`time` DoS) stays **patched**, not ignored.
  - **Gate proven to fail closed (real local runs, not fabricated).**
    `cargo deny check` passes (advisories/bans/licenses/sources ok); a dry-run
    breach of each list — dropping `Unicode-3.0` from the license allow-list
    (exit 4), banning an in-tree crate (exit 2), emptying the source allow-list
    (exit 8) — fails the gate. `cargo audit` with the documented `--ignore` flags
    exits 0; **without** them it exits 1 on exactly the three recorded
    advisories, the honest red-run proof that the audit gate is not vacuous.
  - **Dependency-addition audit-note workflow** confirmed wired into the
    binding pre-submission checklist (`docs/TESTING.md` §10, §13.5): a new
    dependency carries an audit note and both supply-chain gates must pass.

## [0.0.1] - 2026-07-12

### Added

- Reserve the `chainview` crate name on crates.io.
- Design documentation under `docs/` (PRD, roadmap, architecture, data
  providers, replay mode, views/UX, ADRs, and specs).

[Unreleased]: https://github.com/joaquinbejar/ChainView/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/joaquinbejar/ChainView/releases/tag/v0.0.1
