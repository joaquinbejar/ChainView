<!--
ChainView pull-request checklist. The pre-submission checklist is binding
(docs/TESTING.md §10, rules/global_rules.md). CI enforces the four
non-negotiables + the supply-chain gates (.github/workflows/ci.yml).
-->

## Summary

<!-- What changed and why. Link the issue: Closes #NNN -->

## Pre-submission checklist (binding — docs/TESTING.md §10)

- [ ] `cargo fmt --all --check` clean.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo test --all-features` green (fixtures only — the live smoke stays `#[ignore]`).
- [ ] `cargo build --release` succeeds.
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features` clean.
- [ ] No `.unwrap()` / `.expect()` / unchecked `[]` in production code; no `unsafe`.
- [ ] No `println!` / `eprintln!` / `dbg!`; no I/O or `.await` in the draw path.
- [ ] `CHANGELOG.md` entry added for every user-visible change.

## Supply-chain (docs/SECURITY.md §7, docs/TESTING.md §13.5)

- [ ] **A DEPENDENCY ADDITION CARRIES AN AUDIT NOTE.** Any new crate in
      `Cargo.toml` (direct or a notable transitive it pulls) is called out in the
      PR body and the `CHANGELOG.md` entry with: why it is needed, what it pulls
      in, its license, and any advisory it introduces.
- [ ] `cargo deny check` passes (advisories + licenses + bans + sources).
- [ ] `cargo audit` passes.
- [ ] A NEW advisory ignore / license-allow / duplicate-skip in `deny.toml`
      carries a written reason **and** a re-evaluation trigger, and the
      mechanism entry is added to `docs/SECURITY.md §7` (never a broad suppression).
- [ ] If the MSRV must move, `Cargo.toml` `rust-version`, the CI `msrv` job pin,
      and a `CHANGELOG.md` `### Changed` MSRV callout are updated together
      (docs/SEMVER.md — raising the MSRV is a minor bump).

## Docs

- [ ] Design docs / ADR updated if the architecture or a decision changed.
- [ ] Render goldens updated if a screen's layout or content changed.
- [ ] Provider fixtures + capabilities matrix updated if a provider/channel landed.
