//! ChainView's typed, **read-only** views of the IronCondor **result bundle**
//! (`docs/01-domain-model.md` §9, `docs/04-replay-mode.md` §2).
//!
//! A result bundle is a directory holding `manifest.json` plus four Parquet
//! tables — `fills.parquet`, `equity_curve.parquet`, `positions.parquet`,
//! `greeks_attribution.parquet`. IronCondor **writes** it; ChainView replay mode
//! **consumes** it read-only and never mutates it. The schema is **owned by
//! IronCondor**, tagged `"ironcondor.bundle.v1"`, reproduced here for the reader
//! but never changed unilaterally
//! ([ADR-0004](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0004-ironcondor-result-bundle-as-replay-format.md)).
//!
//! This module (issue #29) fixes the **type shapes** every later replay issue is
//! written against: the reader body (#30), the Parquet decoders (#31), and the
//! validation chain (#32). It defines the types and their `serde` shape only —
//! **no I/O, no Parquet decode, and no validation** live here.
//!
//! # Binding properties (set by the type shapes themselves)
//!
//! - **Money is integer cents** (`i64`/`u64`) on every monetary field. The
//!   **only** `f64` in the replay types is [`EquityPoint::drawdown`], an analytic
//!   ratio that is never money — enforced by
//!   [`tests::test_only_pub_f64_field_is_drawdown`].
//! - **`run_id` is opaque.** [`BundleManifest::run_id`] is an uninterpreted
//!   `String`: the reader stores it, labels with it, and uses it for the
//!   `fills.strategy_run_id == run_id` referential check (#32), but the type
//!   offers **no** way to derive, predict, or re-hash it — IronCondor owns run
//!   identity (`docs/04-replay-mode.md` §2.1).
//! - **`metrics`/`config`/`strategy`/`data_source` are opaque.** They are read as
//!   raw [`serde_json::Value`] and displayed, never parsed field-by-field,
//!   computed on, or compared in the equivalence oracle. The **one** narrow
//!   exception is [`CapitalConfig`], the typed projection of the single
//!   `config.initial_capital` field (IronCondor's writer field for opening
//!   capital — integer cents; there is no `capital_cents` field).
//!
//! # Serde posture
//!
//! [`BundleManifest`] is **permissive** — no `deny_unknown_fields`, so a newer
//! minor of the same `schema` tag still opens (`docs/04-replay-mode.md` §3). The
//! four row structs carry `deny_unknown_fields`, which is scoped to this module's
//! **JSON round-trip / test path only**: it asserts the typed shape stays exactly
//! the documented column set. It does NOT narrow the bundle contract — the
//! production Parquet decode (#31) reads by **column projection** and stays
//! permissive toward unknown extra columns, exactly per `docs/04-replay-mode.md`
//! §3 ("a newer minor still opens"). A missing required field is a `serde`
//! error, never a silent default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use optionstratlib::OptionStyle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::BundleError;

/// The versioned `contract_id` join-key format, fixed here as the single source
/// of truth for the round-trip check the validation chain (#32) enforces.
///
/// A `contract_id` is colon-delimited:
/// `"v1:{UNDERLYING}:{expiration_ns}:{strike_cents}:{C|P}"`, where `{C|P}` is a
/// single upper-case style char (`C` = call, `P` = put), `expiration_ns` is the
/// resolved absolute UTC instant (nanoseconds since epoch), and `strike_cents` is
/// the strike in integer cents. `UNDERLYING` matches
/// [`CONTRACT_ID_UNDERLYING_PATTERN`] and is therefore colon-free, so the five
/// fields split unambiguously (`docs/04-replay-mode.md` §2.3). The **parser**
/// that enforces this round-trip lives in #32; only the grammar is fixed here.
pub const CONTRACT_ID_FORMAT: &str = "v1:{UNDERLYING}:{expiration_ns}:{strike_cents}:{C|P}";

/// The current `contract_id` version prefix. A bumped prefix is a
/// major-incompatible schema change (`docs/04-replay-mode.md` §5, SEMVER.md).
pub const CONTRACT_ID_VERSION_PREFIX: &str = "v1";

/// The grammar the `contract_id` `UNDERLYING` segment must match:
/// `^[A-Z0-9._]{1,32}$` — upper-case letters, digits, `.` and `_`, 1–32 chars,
/// and deliberately **colon-free** so the join key splits unambiguously
/// (`docs/04-replay-mode.md` §2.3). Held as a documented pattern string; the
/// matcher is #32's work.
pub const CONTRACT_ID_UNDERLYING_PATTERN: &str = "^[A-Z0-9._]{1,32}$";

/// `manifest.json` — run provenance plus the config / strategy / data-source /
/// metrics blobs and the per-table `row_counts` integrity hint. Every field is
/// **required** at `ironcondor.bundle.v1` (`docs/04-replay-mode.md` §2.1).
///
/// # Opacity
///
/// [`config`](Self::config), [`strategy`](Self::strategy),
/// [`data_source`](Self::data_source), and [`metrics`](Self::metrics) are
/// **displayed raw and never interpreted** — they are opaque [`serde_json::Value`]
/// blobs. The one narrow typed projection over `config` is [`CapitalConfig`],
/// reachable via [`capital_config`](Self::capital_config).
/// [`run_id`](Self::run_id) is an opaque producer identity — the reader stores it
/// but never derives it.
///
/// # Serde
///
/// Deserialization is **permissive**: there is intentionally no
/// `deny_unknown_fields`, so a newer minor of the same `schema` tag opens with
/// its extra fields ignored (`docs/04-replay-mode.md` §3). A missing required
/// field is still a `serde` error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleManifest {
    /// Versioned schema tag (`"ironcondor.bundle.v1"`) — the compatibility gate.
    pub schema: String,
    /// **OPAQUE** producer-owned run identity. ChainView treats it as an
    /// uninterpreted `String`: it is never derived, predicted, or re-hashed here
    /// (`docs/04-replay-mode.md` §2.1). No recompute helper exists by design.
    pub run_id: String,
    /// RFC3339 write time — **provenance only**, excluded from every
    /// determinism/equality comparison.
    pub created_utc: String,
    /// IronCondor crate version (`CARGO_PKG_VERSION`) — provenance + build
    /// identity (no per-commit git sha; build identity is
    /// `code_version + lockfile_sha256`).
    pub code_version: String,
    /// sha256 of the producer's `Cargo.lock` (provenance).
    pub lockfile_sha256: String,
    /// Engine RNG seed.
    pub seed: u64,
    /// `BacktestConfig` verbatim (`mode`, `slippage`, `fees`, `limits`,
    /// `liquidity_profile`, `initial_capital` — money in cents). **Opaque**:
    /// read [`capital_config`](Self::capital_config) for the one typed
    /// projection (`config.initial_capital`); the rest — including the fill
    /// model in `mode` (not `execution_mode`) — is displayed, not interpreted.
    pub config: Value,
    /// `kind` + params + `ExitPolicy`. **Opaque** to ChainView.
    pub strategy: Value,
    /// `DataSourceSpec`, tagged by `"kind"` (`csv` | `parquet` | `simulator`) —
    /// provenance and a re-read locator. **Opaque** to ChainView.
    pub data_source: Value,
    /// **VERSIONED, opaque-displayed-raw** metrics graph. The reader may
    /// pretty-print it but never parses a field out of it, computes on it, or
    /// compares it in the equivalence oracle (`docs/04-replay-mode.md` §2.1).
    pub metrics: Value,
    /// Per-table integrity hint — a map `{ table_name -> row count }`. Untrusted:
    /// cross-checked against decoded table lengths (#32), never used to size an
    /// allocation.
    pub row_counts: BTreeMap<String, u64>,
}

impl BundleManifest {
    /// Project the single `config.initial_capital` field out of the otherwise
    /// **opaque** [`config`](Self::config) blob into the typed [`CapitalConfig`].
    ///
    /// This is the **one** narrow interpretation of `config`; the rest stays
    /// uninterpreted. It performs `serde` deserialization only — the load-time
    /// validation (present, integer), the checked narrowing to the reader's
    /// internal signed cents ([`CapitalConfig::capital_cents`]), and the mapping
    /// of an absent field to [`BundleError::Invariant`] are the validation
    /// chain's (#32) job.
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] when `config` is not an object carrying an
    /// unsigned-integer `initial_capital` field — the projection never silently
    /// defaults to `0`.
    pub fn capital_config(&self) -> Result<CapitalConfig, serde_json::Error> {
        CapitalConfig::deserialize(&self.config)
    }
}

/// The **one** narrow typed projection over the manifest `config` blob: the
/// `config.initial_capital` field, read as **unsigned integer cents**
/// (`docs/04-replay-mode.md` §5).
///
/// IronCondor's writer emits opening capital as `config.initial_capital` — an
/// unsigned integer number of cents (e.g. `10_000_000` = `$100,000.00`); there
/// is **no** `capital_cents` field. This projection deserializes from a
/// representative `config` object and **ignores every other field** (`mode`,
/// `slippage`, `fees`, `limits`, `liquidity_profile`, `data_source`, …), which
/// stay opaque on [`BundleManifest::config`]. `initial_capital` is
/// **required** — a `config` missing it is a `serde` error, never a silent `0`;
/// the caller (#32) turns that into [`BundleError::Invariant`].
///
/// Capital is unsigned on the wire; [`capital_cents`](Self::capital_cents)
/// narrows it to the reader's internal **signed** cents (`i64`) via a
/// **checked** conversion, so a value beyond the `i64` domain is a typed error,
/// never an `as` cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapitalConfig {
    /// Opening capital in **integer cents**, unsigned — capital is never
    /// negative in the writer's shape. Read verbatim from `config.initial_capital`.
    pub initial_capital: u64,
}

impl CapitalConfig {
    /// The opening capital as the reader's internal **signed** cents (`i64`),
    /// via a **checked** `u64 -> i64` narrowing.
    ///
    /// Every other money field in the reader is signed `i64` cents; this keeps
    /// capital in the same domain for the equity/attribution identity checks
    /// (#32). Capital never *is* negative — the narrowing only guards the
    /// unrepresentable case `initial_capital > i64::MAX`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Invariant`] when `initial_capital` exceeds the
    /// `i64` cents domain — a checked conversion, never an `as` cast or a panic.
    pub fn capital_cents(&self) -> Result<i64, BundleError> {
        i64::try_from(self.initial_capital).map_err(|_| {
            BundleError::Invariant(format!(
                "config.initial_capital {} exceeds the i64 cents domain",
                self.initial_capital
            ))
        })
    }
}

/// Which direction a leg is held.
///
/// Round-trips the bundle's exact wire strings `long` / `short`; an unknown
/// string is a deserialization error, never a silent fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum PositionSide {
    /// A long (bought) leg.
    Long,
    /// A short (sold) leg.
    Short,
}

/// IronCondor's dual fill model. ChainView surfaces it so the trader can tell
/// which fill model produced the P&L, but renders both identically
/// (`docs/04-replay-mode.md` §2.2).
///
/// Round-trips the bundle's exact wire strings `naive` / `realistic`; an unknown
/// string is a deserialization error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ExecMode {
    /// Mid/spread + slippage fill.
    Naive,
    /// Order-book-level fill walked through the option-chain order book.
    Realistic,
}

/// `fills.parquet` — one row per executed fill. A realistic order walking N price
/// levels emits N rows; a naive order emits 1. UNIQUE and sort key
/// **`(step, order_id, fill_seq)`** (`docs/04-replay-mode.md` §2.2).
///
/// The unsigned-domain fields (`step`, `quantity`, and the cents `u64`s) are the
/// **reader** types; the checked `try_from` narrowing from the signed Parquet
/// wire (a negative is [`BundleError::Invariant`], never an `as` cast) is #31/#32.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fill {
    /// The engine step this fill belongs to.
    pub step: u32,
    /// Event time — **nanoseconds since the Unix epoch, UTC**.
    pub ts_ns: i64,
    /// Producer run identity — equals [`BundleManifest::run_id`] (opaque).
    pub strategy_run_id: String,
    /// The trade this fill is part of.
    pub trade_id: u64,
    /// The position (leg) this fill is against.
    pub position_id: u64,
    /// The order this fill is part of.
    pub order_id: u64,
    /// 0-based fill index **within the order** (a realistic order emits several).
    pub fill_seq: u32,
    /// Underlying ticker — matches [`CONTRACT_ID_UNDERLYING_PATTERN`]
    /// (upper-case, colon-free).
    pub underlying: String,
    /// The leg's expiry as the **resolved absolute UTC instant** (nanoseconds
    /// since epoch) — never a relative offset; the reader never re-resolves it.
    pub expiration_ns: i64,
    /// Versioned join key ([`CONTRACT_ID_FORMAT`]).
    pub contract_id: String,
    /// Strike price in **integer cents** (`>= 0`).
    pub strike_cents: u64,
    /// Call or put. Wire strings `call` / `put`.
    #[serde(with = "option_style_serde")]
    pub style: OptionStyle,
    /// Long or short.
    pub side: PositionSide,
    /// Contract quantity (`> 0`).
    pub quantity: u32,
    /// Fill price in **integer cents** (`>= 0`).
    pub price_cents: u64,
    /// Fees in **integer cents** — **always `>= 0`**, subtracted in P&L.
    pub fees_cents: u64,
    /// Slippage in **integer cents** — **signed**; positive = adverse vs
    /// `decision_mid`.
    pub slippage_cents: i64,
    /// Which fill model produced this row.
    pub mode: ExecMode,
}

/// `equity_curve.parquet` — one row per step. Sort key **`step`**
/// (`docs/04-replay-mode.md` §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EquityPoint {
    /// The engine step.
    pub step: u32,
    /// Event time — **nanoseconds since the Unix epoch, UTC**.
    pub ts_ns: i64,
    /// Cash balance in **integer cents** — signed.
    pub cash_cents: i64,
    /// Mark-to-market position value in **integer cents** — signed
    /// (long +, short −).
    pub position_value_cents: i64,
    /// Total equity in **integer cents** — signed, and
    /// `== cash_cents + position_value_cents` (checked on load, #32).
    pub equity_cents: i64,
    /// Drawdown ratio in `(−∞, 0]` — the **only** `f64` in the replay types and
    /// **never money**. `0` at a peak, `−1` at zero equity, below `−1` when
    /// equity is negative; **reported as-is, never clamped**
    /// (`docs/04-replay-mode.md` §2.2).
    pub drawdown: f64,
}

/// `positions.parquet` — one row per leg for every step it is open, **plus** one
/// terminal row at the step the leg closes (carrying `exit_reason`). At most one
/// row per `(position_id, step)`. Sort key **`(step, position_id)`**
/// (`docs/04-replay-mode.md` §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionRow {
    /// The engine step.
    pub step: u32,
    /// Event time — **nanoseconds since the Unix epoch, UTC**.
    pub ts_ns: i64,
    /// The position (leg) identity.
    pub position_id: u64,
    /// The trade this leg belongs to.
    pub trade_id: u64,
    /// Versioned join key ([`CONTRACT_ID_FORMAT`]).
    pub contract_id: String,
    /// Long or short.
    pub side: PositionSide,
    /// Contract quantity (`> 0`).
    pub quantity: u32,
    /// Average entry price in **integer cents** (`>= 0`).
    pub avg_price_cents: u64,
    /// Snapshot mid in **integer cents** (`>= 0`); the **last-known mid carried
    /// forward** when [`stale_mark`](Self::stale_mark) is `true`.
    pub mark_cents: u64,
    /// Unrealized P&L in **integer cents** — signed.
    pub unrealized_cents: i64,
    /// `true` when this contract had no quote this step and its last-known mark
    /// was carried forward (`docs/04-replay-mode.md` §2.2).
    pub stale_mark: bool,
    /// `null` while the leg is open; the `ExitReason` string on the terminal
    /// closing-step row. Open-set: an `Option<String>` (the reason vocabulary is
    /// an open set the reader does not enumerate).
    pub exit_reason: Option<String>,
    /// `true` on the last open row of a leg still open at feed exhaustion — that
    /// leg has **no** terminal row.
    pub open_at_end: bool,
}

/// `greeks_attribution.parquet` — one row per step; sort key **`step`**. The
/// terms sum **exactly** (integer cents) to the step's mark-to-market P&L
/// (`step_pnl`, the equity delta — not a realised-only figure)
/// (`docs/04-replay-mode.md` §2.2, §2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GreeksAttribution {
    /// The engine step.
    pub step: u32,
    /// Event time — **nanoseconds since the Unix epoch, UTC**.
    pub ts_ns: i64,
    /// Theta P&L contribution in **integer cents** — signed.
    pub theta_pnl_cents: i64,
    /// Delta P&L contribution in **integer cents** — signed.
    pub delta_pnl_cents: i64,
    /// Vega P&L contribution in **integer cents** — signed.
    pub vega_pnl_cents: i64,
    /// Spread capture in **integer cents** — **signed**, positive = favourable;
    /// `= −Σ slippage of the step` (`docs/04-replay-mode.md` §2.3).
    pub spread_capture_cents: i64,
    /// Fees in **integer cents** — **always `>= 0`**, subtracted.
    pub fees_cents: u64,
    /// Exact remainder in **integer cents** — signed;
    /// `= step_pnl − Σ other terms` (`docs/04-replay-mode.md` §2.3).
    pub residual_cents: i64,
}

/// A fully materialised bundle — the manifest plus the four decoded tables, each
/// sorted by its stated sort key (`docs/04-replay-mode.md` §3). Produced by
/// [`BundleReader::load`] (body lands in #30).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedBundle {
    /// The validated manifest.
    pub manifest: BundleManifest,
    /// Fills, sorted by `(step, order_id, fill_seq)`.
    pub fills: Vec<Fill>,
    /// Equity curve, sorted by `step`.
    pub equity: Vec<EquityPoint>,
    /// Position rows, sorted by `(step, position_id)`.
    pub positions: Vec<PositionRow>,
    /// Greeks attribution, sorted by `step`.
    pub greeks: Vec<GreeksAttribution>,
}

/// A **read-only** reader over a result-bundle directory
/// (`docs/04-replay-mode.md` §3).
///
/// It records the bundle `root` and never writes, moves, renames, or otherwise
/// mutates anything under it — a bundle is immutable from ChainView's side. The
/// [`open`](Self::open) / [`load`](Self::load) bodies are implemented in #30;
/// this issue (#29) fixes only the stable signature so #30/#31/#32 compile
/// against it.
#[derive(Debug, Clone)]
pub struct BundleReader {
    root: PathBuf,
}

impl BundleReader {
    /// Record the bundle `root`. This stub performs **no** filesystem access —
    /// manifest validation is wired in #30 — so it never mutates the bundle and
    /// succeeds even for a not-yet-existing path.
    ///
    /// # Errors
    ///
    /// Infallible today; the signature stays `Result` because #30's real body
    /// validates the manifest and may return [`BundleError`].
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, BundleError> {
        Ok(Self { root: root.into() })
    }

    /// The bundle directory this reader was opened over.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate the manifest + ceilings, then read, sort, and verify every table
    /// (`docs/04-replay-mode.md` §5).
    ///
    /// # Errors
    ///
    /// The body lands in #30; until then this returns
    /// [`BundleError::NotImplemented`] — a pending-reader placeholder, **not** a
    /// data-integrity failure — rather than a panic, so callers compile against a
    /// stable, non-panicking surface.
    pub fn load(&self) -> Result<LoadedBundle, BundleError> {
        Err(BundleError::NotImplemented)
    }
}

/// `serde` adapter mapping [`OptionStyle`] to the bundle's lower-case wire
/// strings `call` / `put`.
///
/// The `optionstratlib` enum's own `serde` renders `Call` / `Put`; the bundle
/// contract uses `call` / `put` (`docs/04-replay-mode.md` §2.2), so the `style`
/// field carries this adapter. An unknown string is a deserialization error.
mod option_style_serde {
    use optionstratlib::OptionStyle;
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    /// The accepted wire strings, for error reporting.
    const VARIANTS: &[&str] = &["call", "put"];

    pub(super) fn serialize<S>(style: &OptionStyle, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match style {
            OptionStyle::Call => "call",
            OptionStyle::Put => "put",
        })
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<OptionStyle, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "call" => Ok(OptionStyle::Call),
            "put" => Ok(OptionStyle::Put),
            _ => Err(D::Error::unknown_variant(&raw, VARIANTS)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Test helpers (no unwrap/expect/indexing per the ruleset) ------------

    #[track_caller]
    fn from_json<T: for<'de> Deserialize<'de>>(json: &str) -> T {
        match serde_json::from_str::<T>(json) {
            Ok(value) => value,
            Err(e) => panic!("expected `{json}` to deserialize: {e}"),
        }
    }

    #[track_caller]
    fn to_json<T: Serialize>(value: &T) -> String {
        match serde_json::to_string(value) {
            Ok(s) => s,
            Err(e) => panic!("serialize failed: {e}"),
        }
    }

    #[track_caller]
    fn to_value<T: Serialize>(value: &T) -> Value {
        match serde_json::to_value(value) {
            Ok(v) => v,
            Err(e) => panic!("to_value failed: {e}"),
        }
    }

    fn well_formed_manifest_json() -> String {
        r#"{
            "schema": "ironcondor.bundle.v1",
            "run_id": "run-abc123",
            "created_utc": "2026-07-16T00:00:00Z",
            "code_version": "0.3.0",
            "lockfile_sha256": "deadbeef",
            "seed": 42,
            "config": {
                "initial_capital": 1000000,
                "mode": "realistic",
                "fees": { "per_contract_cents": 65, "per_order_cents": 100 },
                "slippage": { "model": "none" }
            },
            "strategy": { "kind": "iron_condor", "params": {} },
            "data_source": { "kind": "parquet", "path": "tape.parquet", "sha256": "cafe" },
            "metrics": { "sharpe": 1.5, "nested": { "total_pnl_cents": 12345 } },
            "row_counts": {
                "fills": 4, "equity_curve": 10, "positions": 8, "greeks_attribution": 10
            }
        }"#
        .to_owned()
    }

    /// The **real** IronCondor conformance manifest, copied verbatim from
    /// `IronCondor/tests/fixtures/conformance/manifest.json` — the ground-truth
    /// writer output ChainView's reader must accept. Note the config shape:
    /// `config.initial_capital` (unsigned cents) and `config.mode` (not
    /// `capital_cents`/`execution_mode`), plus `fees`/`limits`/`liquidity_profile`
    /// that stay opaque. Money in `metrics` is Decimal-as-string and opaque too.
    fn real_ironcondor_manifest_json() -> String {
        r#"{
          "code_version": "0.5.0",
          "config": {
            "data_source": {
              "kind": "parquet",
              "path": "conformance_condor.parquet",
              "sha256": ""
            },
            "fees": {
              "per_contract_cents": 65,
              "per_order_cents": 100
            },
            "initial_capital": 10000000,
            "limits": {
              "max_contracts_per_snapshot": 50000,
              "max_decompressed_bytes": 8589934592,
              "max_file_bytes": 4294967296,
              "max_manifest_bytes": 16777216,
              "max_rows_per_table": 100000000,
              "max_steps": 100000,
              "max_string_len": 65536,
              "max_total_bytes": 2147483648
            },
            "liquidity_profile": {
              "decay": "1",
              "depth_levels": 2,
              "touch_size": {
                "contracts": 3,
                "kind": "flat"
              }
            },
            "marketable_cap_ticks": 10,
            "mode": "realistic",
            "output_dir": "conformance-out",
            "overwrite": false,
            "seed": 7,
            "slippage": {
              "model": "none"
            }
          },
          "created_utc": "1970-01-01T00:00:00+00:00",
          "data_source": {
            "kind": "parquet",
            "path": "conformance_condor.parquet",
            "sha256": ""
          },
          "lockfile_sha256": "7757b4ea1d081633037b2f1795252453baca654186b607bb8c85f14e0009740b",
          "metrics": {
            "custom_metrics": {
              "max_drawdown_cents": "19625",
              "net_premium_cents": "1136000",
              "realized_pnl_cents": "-7425"
            },
            "general_performance": {
              "sharpe_ratio": "-0.577350269189625731058868041",
              "total_return": "-0.0019625"
            }
          },
          "row_counts": {
            "equity_curve": 5,
            "fills": 10,
            "greeks_attribution": 5,
            "positions": 18
          },
          "run_id": "c4cd155fc156f3ed1488661d9cfd448af59b216c962ea4716f76685f92b21459",
          "schema": "ironcondor.bundle.v1",
          "seed": 7,
          "strategy": {
            "IronCondor": {
              "close_fee": 65,
              "long_call_strike": 520000,
              "long_put_strike": 480000,
              "open_fee": 65,
              "quantity": 5,
              "short_call_strike": 510000,
              "short_put_strike": 490000,
              "underlying": "SPX",
              "underlying_price": 500000
            }
          }
        }"#
        .to_owned()
    }

    // --- BundleManifest -------------------------------------------------------

    #[test]
    fn test_bundle_manifest_roundtrips_well_formed() {
        let manifest: BundleManifest = from_json(&well_formed_manifest_json());
        assert_eq!(manifest.schema, "ironcondor.bundle.v1");
        assert_eq!(manifest.run_id, "run-abc123");
        assert_eq!(manifest.seed, 42);
        assert_eq!(manifest.row_counts.get("fills"), Some(&4));
        assert_eq!(manifest.row_counts.get("equity_curve"), Some(&10));
        // JSON out round-trips back to the same typed value.
        let reparsed: BundleManifest = from_json(&to_json(&manifest));
        assert_eq!(manifest, reparsed);
    }

    #[test]
    fn test_bundle_manifest_tolerates_unknown_fields() {
        // A newer minor of the same schema tag adds a top-level field the reader
        // does not know: it must still open (permissive, no deny_unknown_fields).
        let json = well_formed_manifest_json().replace(
            "\"seed\": 42,",
            "\"seed\": 42, \"future_added_field\": {\"x\": 1},",
        );
        let manifest: BundleManifest = from_json(&json);
        assert_eq!(manifest.schema, "ironcondor.bundle.v1");
    }

    #[test]
    fn test_bundle_manifest_missing_required_field_errors() {
        // Drop the required `seed` field entirely — this must be a serde error,
        // not a silent default.
        let json = well_formed_manifest_json().replace("\"seed\": 42,", "");
        assert!(
            serde_json::from_str::<BundleManifest>(&json).is_err(),
            "a manifest missing `seed` must fail to deserialize"
        );
    }

    #[test]
    fn test_bundle_manifest_metrics_stays_opaque() {
        let manifest: BundleManifest = from_json(&well_formed_manifest_json());
        // `metrics` survives as an opaque Value: no field is parsed out of it and
        // it round-trips byte-for-byte through the typed value.
        let expected = serde_json::json!({
            "sharpe": 1.5,
            "nested": { "total_pnl_cents": 12345 }
        });
        assert_eq!(manifest.metrics, expected);
    }

    // --- CapitalConfig --------------------------------------------------------

    #[test]
    fn test_capital_config_extracts_initial_capital_ignoring_other_fields() {
        let manifest: BundleManifest = from_json(&well_formed_manifest_json());
        let capital = match manifest.capital_config() {
            Ok(c) => c,
            Err(e) => panic!("capital_config should project initial_capital: {e}"),
        };
        assert_eq!(capital.initial_capital, 1_000_000);
    }

    #[test]
    fn test_capital_config_reads_real_ironcondor_manifest() {
        // The REAL IronCondor writer output must parse and project its opening
        // capital from `config.initial_capital` (10_000_000 cents = $100k).
        let manifest: BundleManifest = from_json(&real_ironcondor_manifest_json());
        assert_eq!(manifest.schema, "ironcondor.bundle.v1");
        assert_eq!(manifest.seed, 7);
        assert_eq!(manifest.row_counts.get("positions"), Some(&18));
        let capital = match manifest.capital_config() {
            Ok(c) => c,
            Err(e) => panic!("real manifest must project initial_capital: {e}"),
        };
        assert_eq!(capital.initial_capital, 10_000_000);
        // The checked narrowing into the reader's signed cents succeeds.
        match capital.capital_cents() {
            Ok(cents) => assert_eq!(cents, 10_000_000),
            Err(e) => panic!("checked capital narrowing must succeed: {e}"),
        }
    }

    #[test]
    fn test_capital_config_reports_absent_initial_capital() {
        // A config blob missing `initial_capital` is an error the caller (#32)
        // turns into Invariant — it never silently defaults to 0.
        let config = serde_json::json!({ "mode": "naive", "fees": { "per_order_cents": 5 } });
        assert!(
            CapitalConfig::deserialize(&config).is_err(),
            "an absent initial_capital must not default to 0"
        );
    }

    #[test]
    fn test_capital_config_rejects_legacy_capital_cents_shape() {
        // The pre-reconcile self-authored shape (`config.capital_cents`, no
        // `initial_capital`) is now REJECTED: the reader tracks the writer's true
        // field name, so a bundle carrying only `capital_cents` fails to project.
        let config = serde_json::json!({ "capital_cents": 1_000_000 });
        assert!(
            CapitalConfig::deserialize(&config).is_err(),
            "the legacy capital_cents-only shape must not project as initial_capital"
        );
    }

    #[test]
    fn test_capital_config_initial_capital_is_unsigned() {
        // Capital is unsigned on the wire: a negative `initial_capital` cannot
        // deserialize into u64 — the type rejects it, no `as` coercion.
        let config = serde_json::json!({ "initial_capital": -25 });
        assert!(
            CapitalConfig::deserialize(&config).is_err(),
            "a negative initial_capital must fail against the unsigned wire type"
        );
    }

    #[test]
    fn test_capital_config_capital_cents_checked_conversion_overflows() {
        // A capital beyond the i64 cents domain is a typed BundleError::Invariant
        // from the checked narrowing — never an `as` cast or a panic.
        let config = serde_json::json!({ "initial_capital": u64::MAX });
        let capital = match CapitalConfig::deserialize(&config) {
            Ok(c) => c,
            Err(e) => panic!("u64::MAX initial_capital should deserialize: {e}"),
        };
        assert_eq!(capital.initial_capital, u64::MAX);
        match capital.capital_cents() {
            Err(BundleError::Invariant(_)) => {}
            other => panic!("overflowing capital narrowing should be Invariant, got {other:?}"),
        }
    }

    // --- PositionSide / ExecMode / OptionStyle wire strings ------------------

    #[test]
    fn test_position_side_wire_strings_roundtrip() {
        assert_eq!(to_json(&PositionSide::Long), "\"long\"");
        assert_eq!(to_json(&PositionSide::Short), "\"short\"");
        assert_eq!(from_json::<PositionSide>("\"long\""), PositionSide::Long);
        assert_eq!(from_json::<PositionSide>("\"short\""), PositionSide::Short);
    }

    #[test]
    fn test_position_side_rejects_unknown_string() {
        assert!(serde_json::from_str::<PositionSide>("\"sideways\"").is_err());
        // The exact-case contract: `Long` (capitalized) is not the wire form.
        assert!(serde_json::from_str::<PositionSide>("\"Long\"").is_err());
    }

    #[test]
    fn test_exec_mode_wire_strings_roundtrip() {
        assert_eq!(to_json(&ExecMode::Naive), "\"naive\"");
        assert_eq!(to_json(&ExecMode::Realistic), "\"realistic\"");
        assert_eq!(from_json::<ExecMode>("\"naive\""), ExecMode::Naive);
        assert_eq!(from_json::<ExecMode>("\"realistic\""), ExecMode::Realistic);
    }

    #[test]
    fn test_exec_mode_rejects_unknown_string() {
        assert!(serde_json::from_str::<ExecMode>("\"paper\"").is_err());
    }

    #[test]
    fn test_option_style_wire_strings_roundtrip_via_fill() {
        // The `style` field maps OptionStyle to the bundle's lower-case wire form.
        let fill: Fill = from_json(&fill_json("call", "long", "realistic"));
        assert_eq!(fill.style, OptionStyle::Call);
        assert!(to_json(&fill).contains("\"style\":\"call\""));

        let put: Fill = from_json(&fill_json("put", "short", "naive"));
        assert_eq!(put.style, OptionStyle::Put);
        assert!(to_json(&put).contains("\"style\":\"put\""));
    }

    #[test]
    fn test_option_style_rejects_unknown_wire_string() {
        // Neither the capitalized optionstratlib form nor a nonsense value parses.
        assert!(serde_json::from_str::<Fill>(&fill_json("Call", "long", "naive")).is_err());
        assert!(serde_json::from_str::<Fill>(&fill_json("american", "long", "naive")).is_err());
    }

    // --- Row round-trips ------------------------------------------------------

    fn fill_json(style: &str, side: &str, mode: &str) -> String {
        format!(
            r#"{{
                "step": 3,
                "ts_ns": 1700000000000000000,
                "strategy_run_id": "run-abc123",
                "trade_id": 7,
                "position_id": 11,
                "order_id": 21,
                "fill_seq": 0,
                "underlying": "BTC",
                "expiration_ns": 1735286400000000000,
                "contract_id": "v1:BTC:1735286400000000000:6000000:C",
                "strike_cents": 6000000,
                "style": "{style}",
                "side": "{side}",
                "quantity": 2,
                "price_cents": 12500,
                "fees_cents": 30,
                "slippage_cents": -15,
                "mode": "{mode}"
            }}"#
        )
    }

    #[test]
    fn test_fill_roundtrips() {
        let fill: Fill = from_json(&fill_json("call", "long", "realistic"));
        assert_eq!(fill.step, 3);
        assert_eq!(fill.strike_cents, 6_000_000);
        assert_eq!(fill.fees_cents, 30);
        assert_eq!(fill.slippage_cents, -15);
        assert_eq!(fill.side, PositionSide::Long);
        assert_eq!(fill.mode, ExecMode::Realistic);
        let reparsed: Fill = from_json(&to_json(&fill));
        assert_eq!(fill, reparsed);
    }

    #[test]
    fn test_fill_strict_rejects_unknown_field() {
        // Rows are strict (deny_unknown_fields): an extra column is an error.
        let json = fill_json("call", "long", "naive")
            .replace("\"step\": 3,", "\"step\": 3, \"unexpected\": 1,");
        assert!(
            serde_json::from_str::<Fill>(&json).is_err(),
            "a row with an unknown field must be rejected (strict posture)"
        );
    }

    #[test]
    fn test_fill_missing_required_field_errors() {
        let json = fill_json("call", "long", "naive").replace("\"quantity\": 2,", "");
        assert!(serde_json::from_str::<Fill>(&json).is_err());
    }

    #[test]
    fn test_equity_point_roundtrips() {
        let json = r#"{
            "step": 5,
            "ts_ns": 1700000000000000000,
            "cash_cents": 990000,
            "position_value_cents": -1500,
            "equity_cents": 988500,
            "drawdown": -0.015
        }"#;
        let point: EquityPoint = from_json(json);
        assert_eq!(point.equity_cents, 988_500);
        assert_eq!(
            point.cash_cents + point.position_value_cents,
            point.equity_cents
        );
        assert!((point.drawdown - (-0.015)).abs() < 1e-12);
        let reparsed: EquityPoint = from_json(&to_json(&point));
        assert_eq!(point, reparsed);
    }

    #[test]
    fn test_position_row_open_leg_has_null_exit_reason() {
        let json = r#"{
            "step": 4,
            "ts_ns": 1700000000000000000,
            "position_id": 11,
            "trade_id": 7,
            "contract_id": "v1:BTC:1735286400000000000:6000000:C",
            "side": "short",
            "quantity": 1,
            "avg_price_cents": 12000,
            "mark_cents": 11800,
            "unrealized_cents": 200,
            "stale_mark": false,
            "exit_reason": null,
            "open_at_end": true
        }"#;
        let row: PositionRow = from_json(json);
        assert_eq!(row.side, PositionSide::Short);
        assert_eq!(row.exit_reason, None);
        assert!(row.open_at_end);
        assert!(!row.stale_mark);
        let reparsed: PositionRow = from_json(&to_json(&row));
        assert_eq!(row, reparsed);
    }

    #[test]
    fn test_position_row_terminal_carries_exit_reason() {
        let json = r#"{
            "step": 9,
            "ts_ns": 1700000000000000000,
            "position_id": 11,
            "trade_id": 7,
            "contract_id": "v1:BTC:1735286400000000000:6000000:C",
            "side": "short",
            "quantity": 1,
            "avg_price_cents": 12000,
            "mark_cents": 11800,
            "unrealized_cents": 200,
            "stale_mark": true,
            "exit_reason": "profit_target",
            "open_at_end": false
        }"#;
        let row: PositionRow = from_json(json);
        assert_eq!(row.exit_reason.as_deref(), Some("profit_target"));
        assert!(row.stale_mark);
    }

    #[test]
    fn test_greeks_attribution_roundtrips() {
        let json = r#"{
            "step": 5,
            "ts_ns": 1700000000000000000,
            "theta_pnl_cents": 40,
            "delta_pnl_cents": -120,
            "vega_pnl_cents": 15,
            "spread_capture_cents": 10,
            "fees_cents": 30,
            "residual_cents": 1
        }"#;
        let row: GreeksAttribution = from_json(json);
        assert_eq!(row.theta_pnl_cents, 40);
        assert_eq!(row.spread_capture_cents, 10);
        assert_eq!(row.fees_cents, 30);
        let reparsed: GreeksAttribution = from_json(&to_json(&row));
        assert_eq!(row, reparsed);
    }

    // --- contract_id grammar constants ---------------------------------------

    #[test]
    fn test_contract_id_grammar_constants_are_fixed() {
        assert_eq!(
            CONTRACT_ID_FORMAT,
            "v1:{UNDERLYING}:{expiration_ns}:{strike_cents}:{C|P}"
        );
        assert_eq!(CONTRACT_ID_VERSION_PREFIX, "v1");
        assert_eq!(CONTRACT_ID_UNDERLYING_PATTERN, "^[A-Z0-9._]{1,32}$");
        // The version prefix is the leading colon-delimited field of the format.
        assert!(CONTRACT_ID_FORMAT.starts_with(CONTRACT_ID_VERSION_PREFIX));
    }

    // --- run_id opacity -------------------------------------------------------

    #[test]
    fn test_run_id_is_a_plain_opaque_string() {
        // The manifest exposes run_id only as a stored String: it is readable and
        // usable for equality/labelling, but the type offers NO derive/recompute
        // helper (enforcement is the code-review contract; docs/04 §2.1).
        let manifest: BundleManifest = from_json(&well_formed_manifest_json());
        let id: &str = &manifest.run_id;
        assert_eq!(id, "run-abc123");
    }

    // --- BundleReader stub is read-only and non-panicking --------------------

    #[test]
    fn test_bundle_reader_open_does_no_io_and_load_is_pending() {
        // `open` records the root without any filesystem access, so it succeeds
        // even for a not-yet-existing path — read-only by construction.
        let reader = match BundleReader::open("/no/such/bundle/dir") {
            Ok(r) => r,
            Err(e) => panic!("open stub must not perform I/O: {e}"),
        };
        assert_eq!(reader.root(), Path::new("/no/such/bundle/dir"));
        // `load` is the #30 placeholder: a typed pending error, never a panic.
        match reader.load() {
            Err(BundleError::NotImplemented) => {}
            other => panic!("load stub should be pending, got {other:?}"),
        }
    }

    // --- Money discipline: the only pub f64 field is drawdown ----------------

    #[test]
    fn test_only_pub_f64_field_is_drawdown() {
        // Grep this module's own source (the acceptance check): every public field
        // declared as `f64` must be `drawdown` — every money field is integer
        // cents.
        let src = include_str!("mod.rs");
        let mut f64_field_lines = 0_usize;
        for line in src.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("pub ") && trimmed.contains(": f64") {
                assert!(
                    trimmed.contains("drawdown"),
                    "unexpected f64 field (money must be integer cents): `{trimmed}`"
                );
                f64_field_lines += 1;
            }
        }
        assert_eq!(
            f64_field_lines, 1,
            "exactly one public f64 field (EquityPoint::drawdown) is expected"
        );
    }

    // --- to_value sanity: rows project to JSON objects -----------------------

    #[test]
    fn test_fill_serializes_to_json_object() {
        let fill: Fill = from_json(&fill_json("put", "short", "realistic"));
        assert!(to_value(&fill).is_object());
    }
}
