# Roadmap — `chainview`

| Field      | Value                                       |
|------------|---------------------------------------------|
| Status     | Living                                      |
| Last edit  | 2026-07-12                                  |

This is a living document. Each version has a tight scope and clear
acceptance criteria. New work that does not fit the current version
goes to the wishlist or the anti-roadmap; it never sneaks into an
in-flight release. The versions here match the scope sections in
[PRD.md §6](PRD.md#6-scope) one-to-one.

Each `## vX.Y` section carries the durable narrative — the **Goal**, the
**Acceptance** criteria, and the security/performance gate — then closes with
an `### Issues` checklist that is the live, per-issue work order. The checklists
use the **real GitHub issue numbers** (`joaquinbejar/ChainView`), which match
the local `milestones/` tree one-to-one, so the
[`/implement-roadmap`](../.claude/commands/implement-roadmap.md) command can
drive them top to bottom and tick each box as its PR merges.

## Where we are

No implementation code exists yet. The crate published to crates.io as
`v0.0.1` is a **name reservation** — `src/lib.rs` is a placeholder and
`[dependencies]` is empty. What exists is the design set under `docs/`
**and, now, the fully filed issue tracker**: every milestone spec under
`milestones/` has a live GitHub issue. Every statement below about
runtime behavior is still planned work, in future tense — the issues are
filed, the code is not.

> **Status 2026-07-12:** `v0.0.1` name reservation live on crates.io;
> **no implementation code exists yet** — `src/lib.rs` is a placeholder and
> `[dependencies]` is empty. What changed since the design-only phase: the
> work is now **fully filed and tracked**. All **58 issues** (`#1`–`#58`)
> are live on GitHub (`joaquinbejar/ChainView`), one per `milestones/` spec,
> mapped onto milestones **v0.1–v1.0** with labels and an assignee — the
> ChainView slice of the **167-issue** plan across the three ecosystem repos.
> The `docs/` design set (this roadmap, the PRD, the numbered design docs,
> the ADRs, the provider specs) is the input each issue references. The next
> actionable step is **#1 — Bootstrap the chainview crate skeleton, lints,
> and toolchain** (v0.1).

**Recommended execution order:** ascending issue number, `#1 → #58`,
honoring the `depends on` note on each `### Issues` line. Every issue's
dependencies carry a **lower** number than the issue itself, so ascending
order is always a valid dependency-respecting plan; `/implement-roadmap`
picks the first unchecked issue whose dependencies are all merged. Next up:
**#1**.

Workflow rules for the build phase: one PR at a time, each behind a
design-doc update first (docs precede code, per repo-root `CLAUDE.md`);
`Closes #<n>` in the PR description (the tracker now exists — see the
per-version `### Issues` checklists below); the full pre-submission checklist
([TESTING.md §10](TESTING.md#10-pre-submission-checklist-binding))
per PR. Providers land behind fixtures — no live-only feature.

## v0.1 — TUI skeleton + Deribit + chain view

**Goal.** A user can `cargo install chainview`, run it with no
arguments, and watch a live Deribit BTC option chain — no credentials,
no config file. This version proves the render loop, the async-data /
sync-render split, and the first provider end-to-end.

**Acceptance.**
- `cargo install chainview && chainview` renders a live Deribit chain
  on a machine with no credentials and no config.
- A panic during a render leaves the terminal usable (raw mode off,
  main screen restored) — test-proven with a panic-injection harness.
- The render path contains zero `.await` / blocking calls (enforced by
  layering; a `src/ui/*` import of a provider or `tokio` I/O is a 🔴).
- Deribit normalization matches the recorded chain fixture exactly.

**Security / performance gate (v0.1).**
- `cargo audit` + `cargo deny` wired in CI **from this release** — a
  `RUSTSEC` advisory or a license/source violation fails the build
  ([SECURITY.md §7](SECURITY.md#7-supply-chain-and-dependency-controls),
  [ADR-0007](adr/0007-production-performance-and-security.md)).
- The **first bench suite + `BENCH.md` baseline** land here (render,
  event fan-in, chain merge), reported as `criterion` + `hdrhistogram`
  p50/p99/p99.9 — no performance claim before the numbers exist
  ([06-performance.md §4](06-performance.md#4-benchmark-methodology)).
- The frame-budget (NFR-14), bounded-memory (NFR-15), and
  startup-to-first-chain (NFR-16) DESIGN TARGETS are re-baselined against
  these first real runs.
- Terminal escape-sequence hygiene: a render golden proves a
  venue-controlled string carrying an escape sequence renders as inert
  text ([SECURITY.md §6.4](SECURITY.md#64-terminal-escape-sequence-hygiene-detail)).

Build order is load-bearing: the guaranteed terminal restore (#8) lands
before anything else — nothing ships until a panic mid-render leaves a
clean shell — and the async-data / sync-render split (#9, #10, #13) is
proven end-to-end before the first real adapter arrives. A trades tape is
**deferred**: `MarketUpdate` carries no trade event yet, so `trades.` is not
subscribed in v0.1
([03-data-providers.md §8](03-data-providers.md#8-capability-matrix)).

### Issues
- [ ] #1 — Bootstrap the chainview crate skeleton, lints, and toolchain (S; no deps)
- [ ] #2 — Define the thiserror boundary error types (M; depends on #1)
- [ ] #3 — Implement the typed Config, source precedence, and secret handling (M; depends on #1, #2)
- [ ] #4 — Add InstrumentKey, Instrument, ContractSpecFingerprint, and the ProviderId newtype (M; depends on #2)
- [ ] #5 — Add the normalized streaming update event types and freshness clocks (M; depends on #4)
- [ ] #6 — Define the Provider port trait, capabilities, and the ChainFetch artifact (L; depends on #5)
- [ ] #7 — Build the ChainStore with poll to stream merge and freshness tracking (L; depends on #6)
- [ ] #8 — Implement the RAII terminal guard and panic-hook restore (M; depends on #2)
- [ ] #9 — Build the App mode state machine and event fan-in (L; depends on #5, #7)
- [ ] #10 — Wire the bounded, coalescing provider-to-app channels (M; depends on #5, #9)
- [ ] #11 — Add the task supervisor and ordered shutdown (M; depends on #8, #9, #10)
- [ ] #12 — Implement the ProviderRegistry and ChainViewApp builder (M; depends on #6, #9)
- [ ] #13 — Build the synchronous render loop, dispatch, and keybinding input (M; depends on #9)
- [ ] #14 — Add the theme, keybinding map, help overlay, and NO_COLOR fallback (M; depends on #13)
- [ ] #15 — Implement the Deribit adapter chain assembly and capabilities (L; depends on #6, #7)
- [ ] #16 — Add the Deribit ticker/book stream overlay and adapter reconnect loop (L; depends on #15, #10)
- [ ] #17 — Add Deribit normalization fixtures and transport-lifecycle tests (M; depends on #16)
- [ ] #18 — Build the chain matrix screen with honest empty/loading/error states (L; depends on #7, #13, #14)
- [ ] #19 — Add chain render goldens and terminal escape-sequence hygiene (M; depends on #18, #15)
- [ ] #20 — Wire the CI pipeline with cargo audit and cargo deny from v0.1 (M; depends on #1)
- [ ] #21 — Land the first bench suite and BENCH.md baseline (M; depends on #13, #9, #16)
- [ ] #22 — Add the live-path integration test and layering arch test (M; depends on #18, #16, #12)

Full per-issue specs: `milestones/v0.1-tui-deribit-chain/` (local).

## v0.2 — Payoff diagrams + Greeks

**Goal.** Turn the chain into a strategy cockpit: multi-leg payoff
diagrams and a fully populated Greeks column, all sourced from
`optionstratlib` — no hand-rolled math.

**Acceptance.**
- A payoff diagram for a known multi-leg strategy matches
  `optionstratlib`'s break-even points and max-profit / max-loss.
- Computed Greeks on a Deribit contract agree with the venue's
  ticker Greeks within a documented tolerance.
- The chain matrix shows a full Greeks row for the selected expiry.

Two deliverables here outlive the milestone: the `GraphData → ratatui`
adapter (#23) is reused by every later geometry screen because ratatui does
not consume `optionstratlib`'s `GraphData` directly
([ADR-0001](adr/0001-ratatui-crossterm.md)), and the local Greeks/IV engine
(#24) is a hard prerequisite for the IG provider in v0.4. The matrix visibly
labels every Greek as venue-supplied or computed-locally, so a mixed-origin
row is never silently blended (#25).

### Issues
- [ ] #23 — Build the GraphData to ratatui dataset adapter (M; depends on #18, #13)
- [ ] #24 — Implement the local Greeks/IV fill-in engine (L; depends on #7, #5)
- [ ] #25 — Wire per-leg Greeks precedence through the chain matrix (M; depends on #24, #18)
- [ ] #26 — Add the multi-leg payoff builder state and keybindings (M; depends on #13, #18)
- [ ] #27 — Build the payoff screen with expiration and t+0 curves (L; depends on #23, #26, #24)
- [ ] #28 — Add payoff goldens and computed-Greeks tolerance tests (M; depends on #27, #25)

Full per-issue specs: `milestones/v0.2-payoff-and-greeks/` (local).

## v0.3 — Replay mode

**Goal.** ChainView opens a finished IronCondor backtest and lets the
user study it — equity curve, P&L attribution by Greek, per-trade
drill-down — with no network access. This is the second half of the
product loop.

**Acceptance.**
- `chainview replay <dir>` renders a valid bundle end-to-end with no
  network I/O.
- Replay mode in v0.3 exposes exactly two reachable screens — equity +
  attribution and trade drill-down; the payoff panel is **not** reachable
  until v0.5 (no dead Payoff key in replay mode meanwhile).
- An unknown / unsupported `manifest.json` schema version is rejected
  with a clear error, not a partial render.
- Scrubbing the timeline updates the equity curve, attribution, and
  selected-trade panels consistently.
- The four Parquet tables round-trip through the reader against a
  conformance fixture shared with IronCondor.

**Security / performance gate (v0.3).**
- The replay decode path (HP-4) gains its bench (`bench_replay_decode`)
  and its resource-ceiling adversarial fixtures — oversized footer,
  row-count lie, truncated file, decompression bomb — each returning a
  typed `BundleError` **without unbounded allocation**
  ([04-replay-mode.md §3](04-replay-mode.md#3-the-reader),
  [SECURITY.md §6.2](SECURITY.md#62-trust-boundaries-and-untrusted-inputs);
  tracked by `docs/CODEX.md` `CV-CODEX-067` (predecessor `CV-CODEX-032`),
  referenced not resolved).

The bundle is untrusted external input, decoded to integer cents with no
network I/O by a bounded, checked-conversion reader. The replay
payoff-at-head panel is **deferred to v0.5** — the v0.3 bundle lacks the
inputs to reprice a position exactly
([04-replay-mode.md §6](04-replay-mode.md#6-the-replay-screens)) — so v0.3
ships exactly two reachable screens: equity + attribution and the per-trade
drill-down.

### Issues
- [ ] #29 — Define replay domain types and the manifest schema (M; depends on #2, #4)
- [ ] #30 — Implement the bundle reader, schema validation, and resource ceilings (L; depends on #29)
- [ ] #31 — Add the typed Parquet table readers (L; depends on #30)
- [ ] #32 — Implement bundle validation and the equivalence oracle (L; depends on #31)
- [ ] #33 — Build the timeline cursor and scrubbing model (M; depends on #31)
- [ ] #34 — Add the replay state machine and the replay subcommand (M; depends on #9, #30, #3)
- [ ] #35 — Build the replay screen: equity, attribution, and drill-down (L; depends on #34, #33, #13, #23)
- [ ] #36 — Add replay adversarial fixtures and the decode bench (M; depends on #32, #30)
- [ ] #37 — Add the replay integration test and render goldens (M; depends on #35, #32)

Full per-issue specs: `milestones/v0.3-replay-mode/` (local).

## v0.4 — Remaining providers

**Goal.** Bring the live-mode provider set to parity behind the same
trait, honoring each venue's real capabilities and degrading gracefully
where a venue lacks a feature.

**Acceptance.**
- Each provider normalizes into the same `OptionChain` and renders in
  the unchanged chain matrix.
- The capabilities matrix in [specs/providers.md](specs/providers.md)
  matches the actual behavior of every adapter (no aspirational cells).
- A provider missing a capability shows an explicit unavailable state,
  never fabricated data.
- Each provider ships with at least one normalization fixture.
- The **public provider port** is exercised end-to-end by a test-only
  external provider registered through `ChainViewApp::builder().register(..)`
  — it normalizes to `OptionChain`, gates screens off its declared
  capabilities, and renders in the same goldens with no built-in special-
  casing ([TESTING.md §7](TESTING.md#7-integration-tests)).
- The `capabilities_source_compat` (`trybuild`) gate proves adding an
  optional capability dimension stays a source-compatible minor bump
  ([SEMVER.md](SEMVER.md#provider-port-versioning)).

Honesty and credential-safety by construction dominate this milestone.
tastytrade (#40), Alpaca (#41), and standalone DXLink (#42) ship **gated** —
written and tested but excluded from `with_builtins()` and reachable only
through the `with_gated_builtin` opt-in that fails while the gate holds —
because their upstreams verifiably log secrets at `DEBUG`; only IG (#39) and
the public-Deribit path stay gate-clear built-ins
([SECURITY.md §2](SECURITY.md#2-tastytrade--gated-blocked-on-upstream)). The
public, semver-governed provider port itself (#43–#45) is delivered here as a
first-class surface an external crate compiles against with no fork of
ChainView ([ADR-0006](adr/0006-open-provider-extension.md)).

### Issues
- [ ] #38 — Add the shared neutral dxfeed decode helpers (M; depends on #6, #5)
- [ ] #39 — Implement the IG provider with local Greeks (L; depends on #6, #7, #24)
- [ ] #40 — Write the tastytrade adapter behind a disabled feature gate (L; depends on #6, #38, #10)
- [ ] #41 — Implement the Alpaca adapter (gated on upstream) (L; depends on #6, #7, #10)
- [ ] #42 — Add the standalone DXLink overlay adapter and equivalence gate (L; depends on #38, #6, #12)
- [ ] #43 — Finalize the public provider port surface, namespacing, and docs (M; depends on #6, #12)
- [ ] #44 — Add the capabilities_source_compat trybuild gate (M; depends on #43, #6)
- [ ] #45 — Add the external-provider end-to-end conformance test (M; depends on #43, #12, #22)
- [ ] #46 — Add v0.4 provider normalization and overlay fixtures (M; depends on #39, #40, #41, #42)

Full per-issue specs: `milestones/v0.4-remaining-providers/` (local).

## v0.5 — Vol surface + depth

**Goal.** The last two screens: implied-volatility structure and
order-book depth where the venue supports it.

**Acceptance.**
- The vol smile for a Deribit expiry matches `optionstratlib`'s
  `VolatilitySmile::smile()` output.
- The replay payoff panel renders the open position's expiration payoff
  and its `mark`-based t+0 curve from `positions.parquet`, with no claim of
  bit-exact upstream repricing.
- The depth ladder updates from live Deribit book deltas without
  freezing a frame.
- Depth-less providers show the unavailable state, not an empty ladder
  that looks like a real one.

All three surface sources — the smile, the greek-vs-strike curve, and the
single-expiry projected surface — are **fallible** and degrade to an explicit
empty state off the draw path; a cross-expiry surface stays out of v1 scope.
The depth ladder has real data only on Deribit, so every other venue renders
the honest unavailable state, and the replay payoff-at-head panel deferred
from v0.3 finally lands (#49), making `2 Payoff` the second reachable replay
screen.

### Issues
- [ ] #47 — Build the vol surface and smile screen (L; depends on #23, #24, #13)
- [ ] #48 — Build the depth ladder screen with sequence-gap resync (L; depends on #16, #13, #14)
- [ ] #49 — Add the replay payoff-at-head panel (M; depends on #33, #27, #35)
- [ ] #50 — Add surface/depth goldens and the IG depth option-epic fixture (M; depends on #47, #48, #49, #39)
- [ ] #51 — Add depth sequence-gap and surface fallible-path tests (M; depends on #48, #47)

Full per-issue specs: `milestones/v0.5-vol-surface-and-depth/` (local).

## v1.0 — Stability commitment

**Goal.** Polish, then freeze the contracts.

Promote to `v1.0.0` when each surface has been stable — shipped without
a breaking change — across one release cycle:

- **CLI surface.** Subcommands, flags, and provider ids stable.
- **Config surface.** Environment variables and precedence stable.
- **Bundle compatibility.** The result-bundle schema ChainView reads is
  frozen against the IronCondor-published version; a new schema version
  is a coordinated, documented change
  ([SEMVER.md](SEMVER.md)).
- **Keybindings.** The documented key map is stable; new keys are
  additive.
- **Zero-config path.** `cargo install chainview && chainview` renders a
  live Deribit chain on a clean machine — the headline acceptance gate.

Polish work carried into v1.0: theme + help-overlay pass, empty /
loading / error states audited on every screen, keybinding consistency,
`cargo binstall` verified.

**Security / performance gate (v1.0).**
- **CI perf-regression gate active.** A hot-path benchmark that regresses
  beyond its documented `BENCH.md` threshold fails the build
  ([06-performance.md §5](06-performance.md#5-regression-gates), NFR-17).
- **Fuzzing the parser surfaces.** Fuzz targets for the replay
  Parquet/manifest decode and the provider payload normalization land
  before the stability commitment
  ([SECURITY.md §7](SECURITY.md#7-supply-chain-and-dependency-controls),
  [TESTING.md §13](TESTING.md#13-security-tests)).
- **Root `SECURITY.md` vulnerability-report channel** in place
  ([SECURITY.md §8](SECURITY.md#8-reporting-a-vulnerability)).
- The frame-budget, bounded-memory, and startup DESIGN TARGETS are
  reported as **measured** numbers in `BENCH.md`, no longer as pending
  targets, before any headline performance claim is made.

v1.0 adds no new screen and no new provider: it lands the security and
performance gates named above and freezes the four durable surfaces (CLI,
config, keybindings, bundle compatibility), each of which must ship one
release cycle without a breaking change before promotion.

### Issues
- [ ] #52 — Activate the CI perf-regression gate (M; depends on #21, #36)
- [~] #53 — Add fuzz targets for the parser surfaces (M; depends on #31, #15) —
      `fuzz/` crate + both targets + fixture-seeded corpus + nightly CI smoke +
      docs landed. `fuzz_provider_normalize` clean (809,601 runs); the first
      `fuzz_replay_decode` run found an OPEN upstream `arrow-ipc` panic on a
      malformed Parquet footer schema (deferred to `replay-expert`: a
      `catch_unwind`/upstream-bump mitigation in `src/replay/*` + regression
      fixture — see `fuzz/README.md`).
- [~] #54 — Add the vulnerability-report channel and finalize supply-chain gates (S; depends on #20) —
      root `SECURITY.md` intake channel added (GitHub private vulnerability
      reporting + email fallback + 90-day disclosure window); `deny.toml`
      license/source/ban allow-lists + the three transitive advisory ignores
      frozen as the reviewed v1.0 gate; `cargo audit` / `cargo deny` proven to
      fail closed on a dry-run breach and on the un-ignored advisories.
- [~] #55 — Freeze the CLI/config/keybinding surfaces and SemVer discipline (M; depends on #3, #14, #43) —
      the three surfaces (CLI `src/main.rs`, config `src/config.rs`, keybindings
      `src/app/keymap.rs`) audited against their `docs/` pages and frozen in a new
      `docs/SEMVER.md` "v1.0 surface-stability audit (#55)" section;
      `RESERVED_PROVIDER_IDS` confirmed to hold exactly the five built-ins and the
      port types confirmed `#[non_exhaustive]`/builder-shaped (minor-bump proof via
      the #44 `trybuild` gate). `scripts/check-changelog.sh` + the blocking
      `changelog-check` CI job and `scripts/surface-diff.sh` + the informational
      `surface-diff` job land with self-tests; three doc↔code drifts fixed (keymap
      source in SEMVER.md + `docs/05` §1/§3; the stale `ProviderId` grammar in
      `.env.example`). `[package].version` stays pre-1.0 (the v1.0.0-cut rule is
      recorded, not executed). Docs + CI + scripts only.
- [ ] #56 — Freeze bundle compatibility against the IronCondor schema (M; depends on #32, #36)
- [ ] #57 — Run the polish pass on states, theme, and keybindings (M; depends on #18, #27, #35, #47, #48, #49)
- [~] #58 — Verify packaging and the zero-config headline acceptance (M; depends on #22, #52, #54) —
      `[package.metadata.binstall]` packaging config (validated well-formed via
      `cargo metadata`, matching the release-artifact naming) + a tag-triggered
      `.github/workflows/release.yml` (build the four native targets, smoke
      `--version`/`--help` + `cargo install --path .`, publish, attach the
      `cargo binstall` archives) + `scripts/changelog-section.sh` (the shared
      release-notes extractor) landed. `docs/RELEASE-PROCESS.md` §6/§12 documents
      the clean-machine zero-config acceptance as a POST-PUBLISH release-cut step
      (not a live-venue CI gate); `BENCH.md` §7 ships NFR-14 (re-confirmed p99
      254.335 us, within the gate ceiling) + NFR-15 (committed HP-3 staging probe)
      as measured facts and records NFR-16 HONESTLY as a release-cut distribution
      pending first publish. Packaging + docs + CI only; `[package].version` stays
      pre-1.0 (the v1.0.0 cut is the human's release action).

Full per-issue specs: `milestones/v1.0-stability/` (local).

## Dependency notes

- v0.2's local Greeks fill-in (`src/chain/greeks.rs`) is a hard
  prerequisite for the IG provider in v0.4 — IG supplies no Greeks / IV.
- Replay mode (v0.3) depends on the IronCondor bundle schema being
  published and versioned; the reader is written against a conformance
  fixture shared across both repos
  ([ADR-0004](adr/0004-ironcondor-result-bundle-as-replay-format.md)).
- The poll→stream merge in the chain store (v0.1) is reused by
  tastytrade and Alpaca in v0.4 — get it right once on Deribit.
- The depth screen (v0.5) only has real data on Deribit and Alpaca
  crypto; do not gate the release on venues that structurally cannot
  supply it.

## Anti-roadmap

What ChainView explicitly will **not** become:

- An order-entry or order-management tool. It is a viewer. The upstream
  clients place orders; ChainView does not.
- A market-data vendor / server. It renders the upstream clients; it
  does not re-expose a data API or persist a tape.
- A reimplementation of any wire protocol (WS / REST / Lightstreamer /
  dxfeed) — those live upstream.
- An options-math library. Pricing, Greeks, payoff, curves, and
  surfaces come from `optionstratlib`.
- A writer of result bundles. Replay is read-only; IronCondor is the
  sole writer.
- A GUI or web app. Terminal only.
- A monorepo of every venue adapter. The **stock binary** ships only the
  bundled built-in set; any other venue integrates through the library's
  open provider extension surface
  ([ADR-0006](adr/0006-open-provider-extension.md)) — maintained by its
  author, outside this roadmap.

## Wishlist

Ideas worth tracking but not scheduled:

- `chainview --demo`: a bundled offline fixture set so the UI can be
  explored with no network and no credentials.
- Persisted UI state (last provider / underlying / layout) in a config
  file.
- Alerting on a strike / Greek threshold, surfaced in an in-app pane.
- A "compare" view: two chains (or two expiries) side by side.
- Diffing two IronCondor result bundles in replay mode.
- A screenshot / export command (render a screen to SVG or text).
- Additional providers behind a feature flag as new ecosystem clients
  land.

## Changelog

Appended by [`/implement-roadmap`](../.claude/commands/implement-roadmap.md)
Step 7 after each PR merges — one row per merged issue, newest at the bottom.
It starts **empty** because no issue has been implemented yet (the issues are
filed; the code does not exist). The first row will record `#1`.

| Date | Issue | PR | Summary |
|------|-------|----|---------|
| _—_  | _—_   | _—_ | _Empty until the first PR merges. `/implement-roadmap` Step 7 appends `\| <date> \| #<n> \| <PR link> \| <one-line summary> \|` here after each merge._ |
