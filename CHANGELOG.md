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

- Bootstrap the single-crate (binary + lib) skeleton for v0.1: MSRV Rust 1.85
  on the 2024 edition, `#![forbid(unsafe_code)]` at both crate roots, the
  `[lints]` table (deny warnings, deny `unsafe_code`, clippy restriction
  lints), module stubs for `error` / `config` / `app` / `event` /
  `providers` / `chain` / `ui`, `rustfmt.toml` + `clippy.toml` (with `anyhow`
  scoped to `main.rs`/startup glue), the `make pre-push` toolchain skeleton,
  and `.env.example`. No runtime dependency added.

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.0.1] - 2026-07-12

### Added

- Reserve the `chainview` crate name on crates.io.
- Design documentation under `docs/` (PRD, roadmap, architecture, data
  providers, replay mode, views/UX, ADRs, and specs).

[Unreleased]: https://github.com/joaquinbejar/ChainView/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/joaquinbejar/ChainView/releases/tag/v0.0.1
