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
