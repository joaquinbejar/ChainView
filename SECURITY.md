# Security Policy

`chainview` is a terminal UI for options traders. This file is the **intake
channel** for security reports. The fuller security **posture** — threat model,
trust boundaries, the credential guarantee, and the supply-chain controls —
lives in [`docs/SECURITY.md`](docs/SECURITY.md); this file points there, not
vice-versa.

## Supported versions

ChainView is pre-1.0 and under active development. Security fixes are applied to
the latest published release on crates.io and to the `main` branch. Older 0.x
versions do not receive backported fixes.

| Version                    | Supported          |
|----------------------------|--------------------|
| latest release + `main`    | yes                |
| older 0.x releases         | no — please upgrade|

## Reporting a vulnerability

Please report suspected vulnerabilities **privately**. Do **not** open a public
GitHub issue, pull request, or discussion for a security report.

Two channels, in order of preference:

1. **GitHub private vulnerability reporting** (preferred). On
   <https://github.com/joaquinbejar/ChainView>, open the **Security** tab and
   choose **Report a vulnerability**. This creates a private advisory visible
   only to the maintainer and to you.
2. **Email**: `jb@taunais.com`, with a subject line beginning
   `chainview security:`. No PGP key is published today; if you need to encrypt,
   say so in a first plaintext message and the maintainer will arrange a key.

Please include, where you can: the affected version or commit, a description of
the issue, reproduction steps or a proof of concept, and the impact you
observed. **Do not** include live credentials or third-party secrets in a
report.

## Coordinated disclosure

ChainView is a solo-maintained, pre-1.0 project; response is **best-effort**,
not a contractual SLA. The maintainer aims to:

- acknowledge a report within **7 days**;
- share an initial assessment within **14 days**;
- agree a fix and a disclosure timeline with the reporter.

Please allow a **90-day coordinated-disclosure window** — or until a fix ships,
whichever comes first — before publishing details. If a report affects an
upstream dependency (a market-data client or `optionstratlib`), the fix may
depend on an upstream release, and the advisory will say so.

## Scope

**In scope** — the `chainview` crate itself: the terminal render path, the
provider adapters, the replay result-bundle reader, and the configuration
surface.

**Out of scope** (detailed in [`docs/SECURITY.md`](docs/SECURITY.md) §6.3): the
internals of an externally registered `Provider` that a third party writes and
links, the upstream venue clients' own wire correctness, and a host or terminal
emulator an attacker already controls.

## What ChainView guarantees

- **Credentials never reach a log sink, an error message, or the terminal.**
  They come from the environment only and are never committed
  ([`docs/SECURITY.md`](docs/SECURITY.md) §1).
- **`#![forbid(unsafe_code)]`** at the crate root — a whole memory-safety class
  of vulnerability is excluded by construction.
- **Supply-chain gates in CI.** `cargo audit` fails the build on a RUSTSEC
  advisory and `cargo deny` enforces the license / source / ban allow-lists, on
  every push and on a weekly schedule ([`deny.toml`](deny.toml),
  [`docs/SECURITY.md`](docs/SECURITY.md) §7).
