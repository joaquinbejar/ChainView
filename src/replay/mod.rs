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
//! Issue #29 fixed the **type shapes** every later replay issue is written
//! against; issue #30 lands the **reader body and the untrusted-input hardening
//! spine**: [`BundleReader::open`] validates the directory + `manifest.json`
//! (presence, its **own byte ceiling** — an oversized manifest is a *manifest
//! bomb*, rejected on the pre-read `stat` before a byte is slurped — schema gate,
//! `row_counts` shape) and validates the operator-supplied [`ResourceCeilings`]
//! **on the enforcement path**, and [`BundleReader::load`] enforces the **three
//! resource ceilings** ([`ResourceCeilings`], `docs/04-replay-mode.md` §3) via a
//! **measured, batched, cancellable** Parquet decode that stops the moment the
//! running working set would exceed the budget — so a malformed or oversized
//! bundle is a typed [`BundleError`], never a panic or an unbounded allocation.
//! The per-batch cap ([`MAX_BATCH_BYTES`]) is a **post-materialization** reject —
//! a batch is decoded and measured *before* it is checked — so the reader's true
//! transient peak is ~one batch (bounded by [`MAX_BATCH_ROWS`] × the column
//! widths), not the whole table; that is the documented residual #36's
//! adversarial fixtures probe. The **typed per-column decode** into `Vec<Fill>`
//! etc. (#31, `src/replay/tables.rs`) is wired **inside** that batched,
//! budget-measured loop, so `load` returns a [`LoadedBundle`] whose four tables
//! are **populated in file order** (the reader never sorts); the stated sort-key
//! ordering is a WRITER guarantee the **cross-table validation chain** (#32)
//! verifies (`Invariant` on violation) before downstream consumers rely on it.
//!
//! # Read-only
//!
//! [`BundleReader`] only ever **stats, opens, and reads** files under `root`; it
//! never writes, moves, renames, or mutates a bundle (`CLAUDE.md` "Module
//! Boundaries"). The decode is synchronous and runs off the render thread (on the
//! replay load/seek worker), polling a caller-supplied cancellation probe at every
//! batch boundary — the domain stays free of `tokio`, so cancellation is a plain
//! `&dyn Fn() -> bool` the app seam adapts from its shutdown token.
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
use std::fs;
use std::path::{Path, PathBuf};

use arrow_array::RecordBatch;
use optionstratlib::OptionStyle;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{BundleError, ConfigError};

mod tables;
mod timeline;
mod validate;

// `Playback` is the single playback enum (issue #34 collapsed the earlier
// `app::Playback` stub into this domain type): the crate root exports it bare, the
// app-state `ReplayState::play` field and the tick fold reference it, and
// `crate::replay::Playback` is its canonical path.
pub use timeline::{Playback, PlaybackSpeed, TimelineCursor};
pub use validate::{BundleDivergence, ORACLE_ABS_TOL, ORACLE_REL_TOL, compare_bundles};
// The `contract_id` parser is crate-internal: the replay payoff-at-head build (#49)
// recovers a `positions` leg's strike/style/expiry from the join key, since
// `positions.parquet` carries no structured strike/style columns.
pub(crate) use validate::parse_contract_id;

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
/// in **file order** as the writer appended it (`docs/04-replay-mode.md` §3).
/// Produced by [`BundleReader::load`].
///
/// Issue #30 lands the manifest + the resource-ceiling spine; issue #31 wires the
/// **typed per-column decode** (`src/replay/tables.rs`) into that batched,
/// budget-measured loop, so `load` now returns this with the four `Vec`s
/// **populated in file order** (the reader never sorts). The stated sort-key
/// ordering is a WRITER guarantee the cross-table validation chain — the ordering,
/// equity/attribution identities, and referential-integrity checks of issue #32 —
/// verifies (`Invariant` on violation).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedBundle {
    /// The validated manifest.
    pub manifest: BundleManifest,
    /// Fills, in file order (writer sort key `(step, order_id, fill_seq)`, #32-verified).
    pub fills: Vec<Fill>,
    /// Equity curve, in file order (writer sort key `step`, #32-verified).
    pub equity: Vec<EquityPoint>,
    /// Position rows, in file order (writer sort key `(step, position_id)`, #32-verified).
    pub positions: Vec<PositionRow>,
    /// Greeks attribution, in file order (writer sort key `step`, #32-verified).
    pub greeks: Vec<GreeksAttribution>,
}

// ---------------------------------------------------------------------------
// Resource ceilings (`docs/04-replay-mode.md` §3). The bundle is UNTRUSTED
// external input: `manifest.row_counts` is an integrity hint that never sizes an
// allocation, every footer integer crosses into an allocation size via checked
// `try_from`/`checked_mul` (never an `as` cast), and the working-set budget is a
// measured, batched, cancellable tally that stops the moment the running total
// would exceed the ceiling — so the allocation never actually reaches it.
// ---------------------------------------------------------------------------

/// Bundle file names, in the order the reader stats + decodes them, paired with
/// each table's `row_counts` key (`docs/04-replay-mode.md` §2.2).
const TABLES: [(&str, &str); 4] = [
    ("fills.parquet", "fills"),
    ("equity_curve.parquet", "equity_curve"),
    ("positions.parquet", "positions"),
    ("greeks_attribution.parquet", "greeks_attribution"),
];

/// The manifest file name at the bundle root.
const MANIFEST_FILE: &str = "manifest.json";

/// The single supported bundle schema tag — the compatibility gate
/// (`docs/04-replay-mode.md` §5 step 1). A `manifest.schema` other than this is
/// [`BundleError::UnsupportedSchema`].
pub const SUPPORTED_SCHEMA: &str = "ironcondor.bundle.v1";

/// **Manifest ceiling** default — reject a `manifest.json` whose on-disk size
/// exceeds this (8 MiB) on the pre-read `stat`, *before* it is slurped into a
/// `Vec<u8>`. A valid manifest is tiny (run provenance + a few opaque JSON
/// blobs), so a giant one is a *manifest bomb* — an OOM on attacker-controlled
/// input the three table ceilings would not catch (they apply only to the four
/// Parquet tables). `docs/SECURITY.md` §6.2 per-file ceiling; `docs/04` §3.
pub const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
/// **Ceiling 1** default — reject a Parquet file whose on-disk size exceeds this
/// (512 MiB), *before* opening it.
pub const MAX_TABLE_BYTES: u64 = 512 * 1024 * 1024;
/// **Ceiling 2** default — reject a table whose Parquet **footer** declares more
/// rows than this (5,000,000), *before* decode.
pub const MAX_TABLE_ROWS: u64 = 5_000_000;
/// **Ceiling 3** default — the total measured working set across all four tables
/// may not exceed this (2 GiB); the decode stops the moment it would.
pub const MAX_WORKING_SET: u64 = 2 * 1024 * 1024 * 1024;
/// Decode granularity — rows per `RecordBatch` (65,536), so the working set is
/// measured and capped incrementally rather than after materialising a table.
pub const MAX_BATCH_ROWS: usize = 65_536;
/// Per-batch measured-size cap (256 MiB) — a single decoded batch larger than
/// this is [`BundleError::TooLarge`], independent of the running total. This is a
/// **post-materialization** reject: the batch is decoded and measured *before* it
/// is checked, so the reader's true transient peak is ~one batch (bounded by
/// [`MAX_BATCH_ROWS`] × the column widths), not the whole table — the documented
/// residual #36's adversarial fixtures probe.
pub const MAX_BATCH_BYTES: u64 = 256 * 1024 * 1024;
/// Decoded-overhead multiplier in **per-mille** (1500 = 1.5×). Applied to each
/// decoded `RecordBatch`'s Arrow array memory size to cover the **transient decode
/// workspace** — the scratch buffers the Parquet→Arrow decode touches beyond the
/// batch's own arrays, freed when the batch drops. It does **not** estimate the
/// **retained** owned rows the typed decode (#31) materialises: those are measured
/// EXACTLY (`rows * size_of::<RowType>()` plus every owned `String`'s heap bytes) by
/// each decoder and accounted separately, so a dictionary-encoded UTF8 column —
/// counted once in the Arrow batch but copied per row into the owned `Vec` — cannot
/// slip past the working-set ceiling behind this estimate. Held as an integer
/// per-mille so the budget arithmetic stays exact (no `f64` on the allocation path).
pub const DECODED_OVERHEAD_PERMILLE: u64 = 1_500;
/// Decompression-bomb reject ratio (20×) — a footer whose declared uncompressed
/// size exceeds this multiple of the on-disk/compressed size is rejected before
/// decode.
pub const MAX_EXPANSION_RATIO: u64 = 20;

/// The configurable resource ceilings the reader enforces on an untrusted bundle
/// (`docs/04-replay-mode.md` §3). Defaults are the documented `MAX_*` constants;
/// tests tighten them to exercise each ceiling on a tiny fixture.
///
/// These are configuration knobs: [`validate`](Self::validate) checks them at
/// startup so an out-of-range value is a typed [`ConfigError`], never a panic or
/// a runaway allocation. (Wiring a CLI/env/file override into [`crate::Config`]
/// is deferred to the config surface; the defaults are always valid.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCeilings {
    /// Manifest ceiling — per-`manifest.json` on-disk byte limit, stat-checked
    /// before the manifest is read (a *manifest bomb* is rejected pre-read).
    pub max_manifest_bytes: u64,
    /// Ceiling 1 — per-file on-disk byte limit.
    pub max_table_bytes: u64,
    /// Ceiling 2 — per-table Parquet-footer row limit.
    pub max_table_rows: u64,
    /// Ceiling 3 — total measured working-set limit across all four tables.
    pub max_working_set: u64,
    /// Rows per decoded `RecordBatch`.
    pub max_batch_rows: usize,
    /// Per-batch measured-size cap.
    pub max_batch_bytes: u64,
    /// Decoded-overhead multiplier, in per-mille (1000 = 1.0×).
    pub decoded_overhead_permille: u64,
    /// Decompression-bomb reject ratio.
    pub max_expansion_ratio: u64,
}

impl Default for ResourceCeilings {
    fn default() -> Self {
        Self {
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            max_table_bytes: MAX_TABLE_BYTES,
            max_table_rows: MAX_TABLE_ROWS,
            max_working_set: MAX_WORKING_SET,
            max_batch_rows: MAX_BATCH_ROWS,
            max_batch_bytes: MAX_BATCH_BYTES,
            decoded_overhead_permille: DECODED_OVERHEAD_PERMILLE,
            max_expansion_ratio: MAX_EXPANSION_RATIO,
        }
    }
}

impl ResourceCeilings {
    /// Validate the ceiling knobs at startup (`rules/global_rules.md`
    /// "Configuration"). Every knob must be usable: the byte/row limits and the
    /// batch granularity must be non-zero, the per-batch cap must fit inside the
    /// working-set limit, the overhead multiplier must be at least `1.0×` (else it
    /// would under-count the decoded working set), and the expansion ratio must be
    /// at least `1×`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidValue`] naming the offending field when a
    /// knob is out of range — an operator misconfiguration is a clear typed error,
    /// never a panic or an unbounded allocation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |field: &str, reason: String| ConfigError::InvalidValue {
            field: field.to_owned(),
            reason,
        };
        if self.max_manifest_bytes == 0 {
            return Err(invalid(
                "replay.max_manifest_bytes",
                "must be > 0".to_owned(),
            ));
        }
        if self.max_table_bytes == 0 {
            return Err(invalid("replay.max_table_bytes", "must be > 0".to_owned()));
        }
        if self.max_table_rows == 0 {
            return Err(invalid("replay.max_table_rows", "must be > 0".to_owned()));
        }
        if self.max_working_set == 0 {
            return Err(invalid("replay.max_working_set", "must be > 0".to_owned()));
        }
        if self.max_batch_rows == 0 {
            return Err(invalid("replay.max_batch_rows", "must be > 0".to_owned()));
        }
        if self.max_batch_bytes == 0 {
            return Err(invalid("replay.max_batch_bytes", "must be > 0".to_owned()));
        }
        if self.max_batch_bytes > self.max_working_set {
            return Err(invalid(
                "replay.max_batch_bytes",
                format!(
                    "per-batch cap {} must not exceed the working-set ceiling {}",
                    self.max_batch_bytes, self.max_working_set
                ),
            ));
        }
        if self.decoded_overhead_permille < 1_000 {
            return Err(invalid(
                "replay.decoded_overhead_permille",
                format!(
                    "must be >= 1000 (1.0x); got {}",
                    self.decoded_overhead_permille
                ),
            ));
        }
        if self.max_expansion_ratio == 0 {
            return Err(invalid(
                "replay.max_expansion_ratio",
                "must be >= 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Build a [`BundleError::TooLarge`] on the cold ceiling-reject path.
#[cold]
#[inline(never)]
fn too_large(detail: String) -> BundleError {
    BundleError::TooLarge(detail)
}

/// Build a [`BundleError::Invariant`] on the cold integrity-reject path.
#[cold]
#[inline(never)]
fn invariant(detail: String) -> BundleError {
    BundleError::Invariant(detail)
}

/// Build a [`BundleError::Io`] on the cold filesystem-error path (non-secret).
#[cold]
#[inline(never)]
fn io_err(detail: String) -> BundleError {
    BundleError::Io(detail)
}

/// Build a [`BundleError::Parquet`] on the cold decode-error path.
#[cold]
#[inline(never)]
fn parquet_err(detail: String) -> BundleError {
    BundleError::Parquet(detail)
}

/// Run an upstream Parquet/Arrow decode `op`, converting a **panic** that escapes
/// the upstream decoder into a typed [`BundleError::Parquet`] instead of unwinding
/// out of the reader.
///
/// The upstream `arrow-ipc` footer / embedded-`ARROW:schema` decode (reached via
/// [`ParquetRecordBatchReaderBuilder::try_new`]) and the per-batch page decode
/// `panic!` on some malformed inputs (issue #53: a corrupt embedded schema
/// flatbuffer panics `arrow-ipc`'s `get_data_type`) — a `panic!` the reader's
/// `.map_err` chain cannot catch, so it would otherwise escape the reader. Wrapping
/// ONLY the upstream call in [`std::panic::catch_unwind`] (which needs **no**
/// `unsafe`, so `#![forbid(unsafe_code)]` holds) preserves the module contract that
/// a malformed bundle is a typed error, never a panic (`docs/04-replay-mode.md` §5,
/// `docs/SECURITY.md` §6.2). A codec/decode failure is `Parquet` per that §5.
///
/// `op` returns a `Result`, so a value it produces normally — including a
/// [`BundleError::Cancelled`] or a ceiling reject — passes through UNCHANGED; only
/// an actual unwinding panic is mapped to `Parquet`. The panic payload is
/// deliberately NOT interpolated into the message: the error names the table only,
/// so a hostile bundle cannot steer the (bounded, non-secret) error string.
/// [`std::panic::AssertUnwindSafe`] is sound here because the reader is abandoned on
/// the panic path — the caller returns `Err` and never observes the
/// partially-decoded upstream state again.
///
/// # Caveat: the process panic hook still runs
///
/// `catch_unwind` catches the unwind but does NOT suppress the process panic hook,
/// which fires (a `stderr` line by default, or the TUI restore hook installed at
/// startup) before this returns. This reader is a domain seam that must stay free
/// of terminal knowledge, so it does not touch the global hook; coordinating hook
/// suppression with the TUI is an app-layer concern, outside this reader's scope.
fn catch_decode_panic<T>(
    file: &str,
    op: impl FnOnce() -> Result<T, BundleError>,
) -> Result<T, BundleError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
        Ok(inner) => inner,
        Err(_) => Err(parquet_err(format!(
            "{file}: upstream Parquet/Arrow decoder panicked on malformed input"
        ))),
    }
}

/// Build a [`BundleError::MissingTable`] on the cold absent-file path.
#[cold]
#[inline(never)]
fn missing_table(detail: String) -> BundleError {
    BundleError::MissingTable(detail)
}

/// Maximum characters retained from an attacker-supplied `manifest.schema` tag
/// in a [`BundleError::UnsupportedSchema`] message. A valid tag is ~20 chars, so
/// a longer one is clamped (on a `char` boundary) with an ellipsis marker to keep
/// the error message bounded regardless of how much junk the bundle supplies
/// (`docs/SECURITY.md` §6). Further sanitized at the render edge.
const MAX_SCHEMA_TAG_CHARS: usize = 64;

/// Clamp an attacker-supplied schema `tag` to [`MAX_SCHEMA_TAG_CHARS`] characters,
/// appending a single `…` marker when it was longer. Operates on **`char`
/// boundaries** (`chars().take(..)`), so a multi-byte UTF-8 tag never panics on a
/// byte-index split, and the result is bounded no matter the input length.
fn clamp_schema_tag(tag: String) -> String {
    if tag.chars().count() <= MAX_SCHEMA_TAG_CHARS {
        return tag;
    }
    let mut clamped: String = tag.chars().take(MAX_SCHEMA_TAG_CHARS).collect();
    clamped.push('…');
    clamped
}

/// Build a [`BundleError::UnsupportedSchema`] on the cold schema-gate path,
/// **clamping** the attacker-supplied tag ([`clamp_schema_tag`]) so a
/// length-unbounded junk tag cannot bloat the error message.
#[cold]
#[inline(never)]
fn unsupported_schema(tag: String) -> BundleError {
    BundleError::UnsupportedSchema(clamp_schema_tag(tag))
}

/// Maximum characters retained from an attacker-supplied **cell value** (an
/// unrecognized enum wire string, a malformed `contract_id`/`underlying`, a
/// `run_id` echo) interpolated into a [`BundleError`] message. A valid value is a
/// handful of chars, so a longer one is clamped on a `char` boundary with an
/// ellipsis so the error stays bounded regardless of bundle input
/// (`docs/SECURITY.md` §6). Shared by the typed decoders (#31,
/// `src/replay/tables.rs`) and the validation chain (#32, `src/replay/validate.rs`).
const MAX_ECHO_CHARS: usize = 64;

/// Clamp an attacker-supplied `value` to [`MAX_ECHO_CHARS`] characters, appending
/// a single `…` marker when it was longer. Operates on **`char` boundaries**, so a
/// multi-byte UTF-8 value never panics on a byte split and the result is bounded.
/// Shared by the decoders and the validation chain (both children of this module).
fn clamp_echo(value: &str) -> String {
    if value.chars().count() <= MAX_ECHO_CHARS {
        return value.to_owned();
    }
    let mut clamped: String = value.chars().take(MAX_ECHO_CHARS).collect();
    clamped.push('…');
    clamped
}

/// Convert a Parquet footer `i64` (a row count or byte size) into a `u64` via a
/// **checked** conversion — never an `as` cast. A negative (corrupt or lying)
/// footer value is [`BundleError::TooLarge`], not a wrapped giant.
#[inline]
fn footer_i64_to_u64(raw: i64, what: &str) -> Result<u64, BundleError> {
    u64::try_from(raw).map_err(|_| too_large(format!("{what}: negative footer value {raw}")))
}

/// Apply the decoded-overhead multiplier to a measured byte count using
/// **checked** integer arithmetic. A multiplication overflow is
/// [`BundleError::TooLarge`] (the count is absurd), never a wrapped `as` cast.
#[inline]
fn apply_overhead(bytes: u64, permille: u64) -> Result<u64, BundleError> {
    bytes
        .checked_mul(permille)
        .map(|scaled| scaled / 1_000)
        .ok_or_else(|| {
            too_large(format!(
                "decoded size {bytes} overflows the overhead multiplier"
            ))
        })
}

/// The running measured working set across all four tables. It commits a batch's
/// bytes only if doing so keeps the total under the ceiling, so on the rejecting
/// path [`used`](Self::used) is always strictly below `max_working_set` — the
/// allocation the budget guards never reaches the ceiling
/// (`docs/04-replay-mode.md` §3).
#[derive(Debug, Clone, Copy)]
struct WorkingSetBudget {
    used: u64,
    max_working_set: u64,
    max_batch_bytes: u64,
}

impl WorkingSetBudget {
    fn new(ceilings: &ResourceCeilings) -> Self {
        Self {
            used: 0,
            max_working_set: ceilings.max_working_set,
            max_batch_bytes: ceilings.max_batch_bytes,
        }
    }

    /// Account one decoded batch's measured working-set bytes. Rejects with
    /// [`BundleError::TooLarge`] — **without committing** — the moment the batch
    /// exceeds the per-batch cap or the running total *would* exceed the
    /// working-set ceiling, so [`used`](Self::used) never crosses the ceiling.
    fn account(&mut self, batch_bytes: u64) -> Result<(), BundleError> {
        if batch_bytes > self.max_batch_bytes {
            return Err(too_large(format!(
                "decoded batch {batch_bytes} B exceeds per-batch cap {} B",
                self.max_batch_bytes
            )));
        }
        let next = self
            .used
            .checked_add(batch_bytes)
            .ok_or_else(|| too_large("cumulative working set overflowed u64".to_owned()))?;
        if next > self.max_working_set {
            return Err(too_large(format!(
                "cumulative working set {next} B would exceed ceiling {} B",
                self.max_working_set
            )));
        }
        self.used = next;
        Ok(())
    }

    /// The committed working set so far — always `< max_working_set`.
    #[inline]
    fn used(&self) -> u64 {
        self.used
    }
}

/// Reject a `row_counts` map that is not the **fixed shape** — exactly the four
/// table keys (`docs/04-replay-mode.md` §2.1). It is an integrity hint, so a
/// wrong-keyed map is [`BundleError::Invariant`]; negative/non-integer counts are
/// already rejected by the `u64` typed parse in [`BundleManifest`].
fn validate_row_counts_shape(row_counts: &BTreeMap<String, u64>) -> Result<(), BundleError> {
    for (_file, key) in TABLES {
        if !row_counts.contains_key(key) {
            return Err(invariant(format!(
                "row_counts missing required key `{key}`"
            )));
        }
    }
    if row_counts.len() != TABLES.len() {
        return Err(invariant(format!(
            "row_counts carries {} keys; exactly the {} table names are required",
            row_counts.len(),
            TABLES.len()
        )));
    }
    Ok(())
}

/// A **read-only** reader over a result-bundle directory
/// (`docs/04-replay-mode.md` §3).
///
/// It records the bundle `root` and the already-validated [`BundleManifest`] from
/// [`open`](Self::open) and never writes, moves, renames, or otherwise mutates
/// anything under `root` — a bundle is immutable from ChainView's side. Money is
/// never touched here; this stage only stats, gate-checks, and measure-decodes.
#[derive(Debug, Clone)]
pub struct BundleReader {
    root: PathBuf,
    manifest: BundleManifest,
    ceilings: ResourceCeilings,
}

impl BundleReader {
    /// Open a bundle directory with the default [`ResourceCeilings`]: verify the
    /// directory exists, parse and **schema-gate** `manifest.json`, and confirm
    /// the four Parquet tables are present. No table is decoded here — that is
    /// [`load`](Self::load).
    ///
    /// # Errors
    ///
    /// - [`BundleError::Io`] if `root` cannot be accessed or is not a directory;
    /// - [`BundleError::MissingTable`] if `manifest.json` or any of the four
    ///   Parquet files is absent;
    /// - [`BundleError::Invariant`] if `manifest.json` is malformed or its
    ///   `row_counts` is not the fixed four-key shape;
    /// - [`BundleError::UnsupportedSchema`] if `manifest.schema` is not
    ///   [`SUPPORTED_SCHEMA`];
    /// - [`BundleError::TooLarge`] if `manifest.json` exceeds
    ///   [`MAX_MANIFEST_BYTES`] (rejected on the pre-read `stat`).
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, BundleError> {
        Self::open_with_ceilings(root, ResourceCeilings::default())
    }

    /// Open a bundle directory with explicit [`ResourceCeilings`] — the same
    /// validation as [`open`](Self::open), used by tests to exercise a ceiling on
    /// a tiny fixture and by future config wiring to pass operator overrides. The
    /// ceilings are [`validate`](ResourceCeilings::validate)d on entry, so a
    /// misconfigured knob cannot silently disable a guard.
    ///
    /// # Errors
    ///
    /// The same set as [`open`](Self::open), plus [`BundleError::Config`] if the
    /// supplied [`ResourceCeilings`] fail validation.
    pub fn open_with_ceilings(
        root: impl Into<PathBuf>,
        ceilings: ResourceCeilings,
    ) -> Result<Self, BundleError> {
        // Validate the operator-supplied ceilings on the ENFORCEMENT path — a
        // misconfigured knob (e.g. `max_batch_rows: 0`, which would make
        // `with_batch_size(0)` yield zero batches and silently disable the
        // measured working-set guard) is a typed `BundleError::Config`, never a
        // silent open. `ceilings` is `Copy`, so this borrows before the store.
        ceilings.validate()?;

        let root = root.into();

        // The bundle root must exist and be a directory (never written to).
        let meta = fs::metadata(&root).map_err(|e| {
            io_err(format!(
                "cannot access bundle directory {}: {e}",
                root.display()
            ))
        })?;
        if !meta.is_dir() {
            return Err(io_err(format!(
                "bundle root is not a directory: {}",
                root.display()
            )));
        }

        // `manifest.json` must be present and within its byte ceiling. STAT it
        // before reading, so an oversized `manifest.json` (a *manifest bomb*) is
        // rejected before it is slurped into a `Vec<u8>` — the table ceilings do
        // not cover the manifest, so it carries its own.
        let manifest_path = root.join(MANIFEST_FILE);
        let manifest_len = match fs::metadata(&manifest_path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(missing_table(MANIFEST_FILE.to_owned()));
            }
            Err(e) => return Err(io_err(format!("stat {MANIFEST_FILE}: {e}"))),
        };
        if manifest_len > ceilings.max_manifest_bytes {
            return Err(too_large(format!(
                "{MANIFEST_FILE} is {manifest_len} B; exceeds manifest ceiling {} B",
                ceilings.max_manifest_bytes
            )));
        }

        // Now read it — bounded by the ceiling just checked.
        let manifest_bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(missing_table(MANIFEST_FILE.to_owned()));
            }
            Err(e) => return Err(io_err(format!("read {MANIFEST_FILE}: {e}"))),
        };

        // Parse permissively (unknown fields ignored; a missing required field is
        // a `serde` error surfaced as `Invariant`).
        let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| invariant(format!("malformed {MANIFEST_FILE}: {e}")))?;

        // Schema gate — a major-incompatible tag is rejected, not partially read.
        if manifest.schema != SUPPORTED_SCHEMA {
            return Err(unsupported_schema(manifest.schema));
        }

        // `row_counts` is the fixed four-key integrity hint.
        validate_row_counts_shape(&manifest.row_counts)?;

        // All four Parquet tables must be present (presence only; column/type
        // checks are #31/#32).
        for (file, _key) in TABLES {
            if !root.join(file).is_file() {
                return Err(missing_table(file.to_owned()));
            }
        }

        Ok(Self {
            root,
            manifest,
            ceilings,
        })
    }

    /// The bundle directory this reader was opened over.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The validated manifest parsed at [`open`](Self::open).
    #[must_use]
    pub fn manifest(&self) -> &BundleManifest {
        &self.manifest
    }

    /// The resource ceilings this reader enforces.
    #[must_use]
    pub fn ceilings(&self) -> &ResourceCeilings {
        &self.ceilings
    }

    /// Enforce the three resource ceilings over the four tables, then return the
    /// [`LoadedBundle`] (`docs/04-replay-mode.md` §3). Uncancellable — equivalent
    /// to [`load_cancellable`](Self::load_cancellable) with a probe that never
    /// trips. The ceilings still bound the total work, so this always terminates.
    ///
    /// # Errors
    ///
    /// [`BundleError::TooLarge`] if any ceiling is exceeded, [`BundleError::Io`] /
    /// [`BundleError::Parquet`] on a filesystem/decode failure, or
    /// [`BundleError::Invariant`] on a `row_counts`/footer mismatch.
    pub fn load(&self) -> Result<LoadedBundle, BundleError> {
        self.load_cancellable(&|| false)
    }

    /// Enforce the three resource ceilings over the four tables via a **measured,
    /// batched, cancellable** decode, then return the [`LoadedBundle`]
    /// (`docs/04-replay-mode.md` §3).
    ///
    /// `cancelled` is polled at every batch boundary (and before touching the
    /// filesystem): when it returns `true`, the load aborts promptly with
    /// [`BundleError::Cancelled`] without reading the rest of the bundle. The app
    /// seam passes `&|| token.is_cancelled()` from its shutdown token
    /// (`docs/02-tui-architecture.md` §12); the domain stays free of `tokio`.
    ///
    /// The typed per-column decode (#31, `src/replay/tables.rs`) runs **inside**
    /// this batched, budget-measured loop: each decoded batch is accounted against
    /// the working-set budget first, then mapped onto the #29 row types and
    /// appended into the eager `Vec` the budget measures. Rows stay in **file
    /// order** — the reader never sorts. After decode, the **cross-table validation
    /// chain** (#32, `src/replay/validate.rs`) runs over the four tables in the
    /// documented §5 order — ordering/uniqueness, the equity + attribution
    /// identities, the contiguous step domain, referential integrity, the
    /// delimiter-safe `contract_id` grammar, and the value-domain rules — turning
    /// every stated-sort-key / cross-table violation into `Invariant`.
    ///
    /// # Errors
    ///
    /// - [`BundleError::Cancelled`] if `cancelled` trips at a batch boundary (or
    ///   before the post-decode validation chain);
    /// - [`BundleError::TooLarge`] if any of the three ceilings is exceeded;
    /// - [`BundleError::MissingTable`] / [`BundleError::Io`] on a filesystem
    ///   failure, [`BundleError::Parquet`] on a decode failure;
    /// - [`BundleError::Schema`] if a required column is missing or wrong-typed;
    /// - [`BundleError::Invariant`] on a footer/`row_counts` disagreement, a
    ///   decoded-rows/footer disagreement, a per-cell decode violation (a negative
    ///   unsigned value, a NULL in a non-nullable column, a non-finite `drawdown`,
    ///   or an unrecognized enum wire string), **or** a validation-chain violation
    ///   (an out-of-order/duplicate sort key, a broken equity/attribution identity,
    ///   a missing/negative `capital_cents`, a step-domain gap or `ts_ns`
    ///   disagreement, a `run_id`/`contract_id` referential failure, an
    ///   out-of-grammar `underlying`, a zero quantity, or a positive `drawdown`).
    pub fn load_cancellable(
        &self,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<LoadedBundle, BundleError> {
        // A pre-cancelled load returns before touching the filesystem.
        if cancelled() {
            return Err(BundleError::Cancelled);
        }

        // The working-set budget spans ALL four tables (§3): the running total is
        // checked after every batch of every table. Each table's rows are decoded
        // batch-by-batch into its eager `Vec` (grown from ACTUAL decoded batch
        // sizes, never pre-reserved from the untrusted `row_counts` hint) as the
        // budget accounts them, and stay in file order (the reader never sorts).
        let mut budget = WorkingSetBudget::new(&self.ceilings);

        let mut fills: Vec<Fill> = Vec::new();
        if cancelled() {
            return Err(BundleError::Cancelled);
        }
        self.scan_table("fills.parquet", "fills", &mut budget, cancelled, &mut |b| {
            tables::read_fills(b, &mut fills)
        })?;

        let mut equity: Vec<EquityPoint> = Vec::new();
        if cancelled() {
            return Err(BundleError::Cancelled);
        }
        self.scan_table(
            "equity_curve.parquet",
            "equity_curve",
            &mut budget,
            cancelled,
            &mut |b| tables::read_equity(b, &mut equity),
        )?;

        let mut positions: Vec<PositionRow> = Vec::new();
        if cancelled() {
            return Err(BundleError::Cancelled);
        }
        self.scan_table(
            "positions.parquet",
            "positions",
            &mut budget,
            cancelled,
            &mut |b| tables::read_positions(b, &mut positions),
        )?;

        let mut greeks: Vec<GreeksAttribution> = Vec::new();
        if cancelled() {
            return Err(BundleError::Cancelled);
        }
        self.scan_table(
            "greeks_attribution.parquet",
            "greeks_attribution",
            &mut budget,
            cancelled,
            &mut |b| tables::read_greeks(b, &mut greeks),
        )?;

        // A cancellation requested during/after the decode aborts before the O(n)
        // validation passes — the chain is bounded by the ceilings but need not run
        // once the caller has asked to stop.
        if cancelled() {
            return Err(BundleError::Cancelled);
        }

        // Rows stay in FILE order — the reader never sorts. The stated sort-key
        // ordering (§2.2) is a WRITER guarantee the #32 validation chain VERIFIES
        // (non-decreasing on its sort key, else `Invariant`); repairing it here
        // would silently mask the exact writer bug #32 must reject and make its
        // monotonic check vacuous — and cost O(n log n) on up to 5M conformant
        // rows. Downstream consumers (#33 timeline) rely on the ordering only once
        // #32 has verified it.
        let loaded = LoadedBundle {
            manifest: self.manifest.clone(),
            fills,
            equity,
            positions,
            greeks,
        };

        // The full §5 post-decode validation chain (#32): ordering/uniqueness, the
        // equity + attribution identities, the step domain, referential integrity,
        // the delimiter-safe `contract_id` grammar, and the value-domain rules. A
        // malformed bundle is a typed `BundleError::Invariant`, never a partial read.
        validate::run_validation_chain(&loaded)?;

        Ok(loaded)
    }

    /// Stat, footer-gate, and measure-decode one table under the three ceilings,
    /// folding its measured working set into `budget` and handing each
    /// budget-accounted batch to `decode` (the #31 typed per-column decode, which
    /// appends the rows into the caller's eager `Vec`). `decode` RETURNS the exact
    /// retained-bytes delta it appended (owned row structs + every owned `String`'s
    /// heap bytes), which is accounted against `budget` in ADDITION to the batch's
    /// Arrow footprint — so a dictionary-encoded UTF8 column, counted once in the
    /// Arrow batch but copied per row, cannot slip past the ceiling. Strictly
    /// read-only.
    fn scan_table(
        &self,
        file: &str,
        key: &str,
        budget: &mut WorkingSetBudget,
        cancelled: &dyn Fn() -> bool,
        decode: &mut dyn FnMut(&RecordBatch) -> Result<u64, BundleError>,
    ) -> Result<(), BundleError> {
        let path = self.root.join(file);

        // --- Ceiling 1: per-file on-disk bytes, before opening the file. ---
        let file_bytes = fs::metadata(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => missing_table(file.to_owned()),
                _ => io_err(format!("stat {file}: {e}")),
            })?
            .len();
        if file_bytes > self.ceilings.max_table_bytes {
            return Err(too_large(format!(
                "{file} is {file_bytes} B; exceeds per-file ceiling {} B",
                self.ceilings.max_table_bytes
            )));
        }

        // Open the file read-only and read its Parquet footer.
        let handle = fs::File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => missing_table(file.to_owned()),
            _ => io_err(format!("open {file}: {e}")),
        })?;
        // `try_new` reads the Parquet footer, which includes the embedded
        // `ARROW:schema` flatbuffer. The upstream `arrow-ipc` decoder `panic!`s on
        // some malformed footers (issue #53, `arrow-ipc` `get_data_type`) — a panic
        // a `.map_err` cannot catch. Wrap ONLY this upstream call in `catch_unwind`
        // so a decoder panic becomes a typed `BundleError::Parquet`, never an
        // escaping panic (`docs/04-replay-mode.md` §5, `docs/SECURITY.md` §6.2).
        let builder = catch_decode_panic(file, move || {
            ParquetRecordBatchReaderBuilder::try_new(handle)
                .map_err(|e| parquet_err(format!("{file}: {e}")))
        })?;

        // --- Ceiling 2: footer row count, before decode. ---
        let (footer_rows, uncompressed, compressed) = {
            let metadata = builder.metadata();
            let footer_rows = footer_i64_to_u64(
                metadata.file_metadata().num_rows(),
                &format!("{file} footer row count"),
            )?;
            let mut uncompressed: u64 = 0;
            let mut compressed: u64 = 0;
            for rg in metadata.row_groups() {
                uncompressed = uncompressed
                    .checked_add(footer_i64_to_u64(
                        rg.total_byte_size(),
                        &format!("{file} row-group uncompressed size"),
                    )?)
                    .ok_or_else(|| {
                        too_large(format!("{file}: footer uncompressed size overflowed u64"))
                    })?;
                compressed = compressed
                    .checked_add(footer_i64_to_u64(
                        rg.compressed_size(),
                        &format!("{file} row-group compressed size"),
                    )?)
                    .ok_or_else(|| {
                        too_large(format!("{file}: footer compressed size overflowed u64"))
                    })?;
            }
            (footer_rows, uncompressed, compressed)
        };
        if footer_rows > self.ceilings.max_table_rows {
            return Err(too_large(format!(
                "{file} footer declares {footer_rows} rows; exceeds per-table ceiling {}",
                self.ceilings.max_table_rows
            )));
        }
        // Cross-check the footer against the manifest integrity hint — the hint is
        // NEVER used to size an allocation, only to detect a mismatch.
        let declared = self
            .manifest
            .row_counts
            .get(key)
            .copied()
            .ok_or_else(|| invariant(format!("row_counts missing key `{key}`")))?;
        if declared != footer_rows {
            return Err(invariant(format!(
                "{file}: row_counts says {declared} but the Parquet footer says {footer_rows}"
            )));
        }

        // --- Ceiling 3(a): decompression-bomb reject + cheap footer pre-check. ---
        // A footer whose uncompressed size dwarfs the on-disk/compressed size is a
        // decompression bomb.
        let on_disk = file_bytes.max(compressed);
        // Checked, not saturating (a banned method): a limit that OVERFLOWS u64
        // would silently disable this reject, so an on-disk size so large that
        // `on_disk * ratio` cannot be represented is itself rejected as beyond
        // any plausible ceiling - the check fails CLOSED, never off.
        let Some(bomb_limit) = on_disk.checked_mul(self.ceilings.max_expansion_ratio) else {
            return Err(too_large(format!(
                "{file}: on-disk size {on_disk} B is too large to bound at {}x - \
                 rejected before decode",
                self.ceilings.max_expansion_ratio
            )));
        };
        if uncompressed > bomb_limit {
            return Err(too_large(format!(
                "{file}: uncompressed {uncompressed} B exceeds {}x its on-disk size — \
                 decompression bomb rejected",
                self.ceilings.max_expansion_ratio
            )));
        }
        // Reject an obviously-oversized bundle before the first batch — a cheap
        // early-out from the footer estimate, NEVER the sole guard (a footer that
        // lies low still gets caught by the measured tally in 3(b)).
        let estimate = apply_overhead(uncompressed, self.ceilings.decoded_overhead_permille)?;
        if budget
            .used()
            .checked_add(estimate)
            .is_none_or(|total| total > self.ceilings.max_working_set)
        {
            return Err(too_large(format!(
                "{file}: estimated decoded working set would exceed ceiling {} B",
                self.ceilings.max_working_set
            )));
        }

        // --- Ceiling 3(b): measured, batched, cancellable decode. ---
        // `total_uncompressed_size` counts uncompressed PAGES, not the memory the
        // decoded arrays occupy (dictionary/RLE/repeated UTF8 can expand well
        // beyond it), so the measured per-batch tally — not the footer — is the
        // real guard. That tally has TWO parts, each accounted against the budget:
        // the decoded batch's Arrow footprint (transient, freed when the batch
        // drops) and the typed decode's RETAINED owned rows (persistent). The Arrow
        // footprint counts a dictionary-encoded UTF8 column once, while the owned
        // rows copy the string per row, so the retained part — returned by `decode`
        // — is what closes that gap. Built under the #53 panic boundary: a
        // malformed embedded schema can panic the upstream builder.
        let mut reader = catch_decode_panic(file, || {
            builder
                .with_batch_size(self.ceilings.max_batch_rows)
                .build()
                .map_err(|e| parquet_err(format!("{file}: {e}")))
        })?;
        let mut decoded_rows: usize = 0;
        loop {
            // Abort at the batch boundary before decoding/accounting the next one.
            if cancelled() {
                return Err(BundleError::Cancelled);
            }
            // Pull the next batch under a panic boundary: a malformed data page can
            // `panic!` inside the upstream arrow decoder the same way a corrupt
            // footer schema does (issue #53), so the pull — the ONLY upstream call
            // in this loop — is wrapped, while the ceiling accounting and typed
            // decode below stay OUTSIDE the boundary (ChainView logic, and the
            // cancellation check / `budget.account` must never be swallowed). On a
            // panic the reader is abandoned: we return `Err` and never poll it
            // again, so `AssertUnwindSafe` observes no partially-decoded state.
            let next = catch_decode_panic(file, || {
                reader
                    .next()
                    .transpose()
                    .map_err(|e| parquet_err(format!("{file}: {e}")))
            })?;
            let Some(batch) = next else { break };
            let batch_bytes = u64::try_from(batch.get_array_memory_size())
                .map_err(|_| too_large(format!("{file}: decoded batch size exceeds u64")))?;
            let measured = apply_overhead(batch_bytes, self.ceilings.decoded_overhead_permille)?;
            // Account the batch's TRANSIENT decode workspace FIRST — a batch that
            // would breach the working-set ceiling is rejected before the typed
            // decode allocates any owned rows.
            budget.account(measured)?;
            // Then cross the wire→domain boundary, appending this batch's rows into
            // the caller's eager `Vec` (capacity grown from the ACTUAL batch size).
            // `decode` returns the EXACT retained-bytes delta (owned row structs +
            // every owned `String`'s heap bytes); account it against the same budget
            // so the persistent working set — not just the transient batch estimate
            // above — is bounded, closing the dictionary-encoding gap.
            let rows = batch.num_rows();
            let retained = decode(&batch)?;
            budget.account(retained)?;
            decoded_rows = decoded_rows
                .checked_add(rows)
                .ok_or_else(|| too_large(format!("{file}: decoded row count overflowed usize")))?;
        }

        // Cross-check the ACTUAL decoded row total against the footer count (the
        // footer already agreed with the `row_counts` hint above); a truncated or
        // corrupt table that yields fewer rows is a typed integrity error, never a
        // silently-short `Vec`. `row_counts` stays a hint — never an allocation size.
        let decoded_rows = u64::try_from(decoded_rows)
            .map_err(|_| too_large(format!("{file}: decoded row count exceeds u64")))?;
        if decoded_rows != footer_rows {
            return Err(invariant(format!(
                "{file}: decoded {decoded_rows} rows but the Parquet footer declares {footer_rows}"
            )));
        }

        Ok(())
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

    // =====================================================================
    // #30 — BundleReader::open/load, schema gate, and resource ceilings
    // =====================================================================

    // --- Bundle-writing test helpers (real Parquet under a RAII tempdir) ------

    /// A tempdir that auto-cleans on drop.
    fn temp_bundle_dir() -> tempfile::TempDir {
        match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("create tempdir: {e}"),
        }
    }

    /// Write a minimal single-column (`step: INT32`) Parquet file with `num_rows`
    /// rows — enough for the #30 ceiling paths (which read the footer + measure
    /// batches, not typed columns).
    fn write_parquet(path: &Path, num_rows: usize) {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Int32Array, RecordBatch};
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;

        let schema = Arc::new(Schema::new(vec![Field::new(
            "step",
            DataType::Int32,
            false,
        )]));
        let steps: Vec<i32> = (0..num_rows)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let column: ArrayRef = Arc::new(Int32Array::from(steps));
        let batch = match RecordBatch::try_new(Arc::clone(&schema), vec![column]) {
            Ok(b) => b,
            Err(e) => panic!("build record batch: {e}"),
        };
        let file = match std::fs::File::create(path) {
            Ok(f) => f,
            Err(e) => panic!("create {}: {e}", path.display()),
        };
        let mut writer = match ArrowWriter::try_new(file, schema, None) {
            Ok(w) => w,
            Err(e) => panic!("arrow writer: {e}"),
        };
        if let Err(e) = writer.write(&batch) {
            panic!("write batch: {e}");
        }
        if let Err(e) = writer.close() {
            panic!("close writer: {e}");
        }
    }

    /// Render a manifest JSON string with a given schema tag, the four
    /// `row_counts`, and an optional unknown extra top-level field.
    fn manifest_json_with(schema: &str, counts: [u64; 4], extra: bool) -> String {
        let [c_fills, c_eq, c_pos, c_greeks] = counts;
        let extra_field = if extra {
            "\"future_field\": {\"x\": 1},"
        } else {
            ""
        };
        format!(
            r#"{{ "schema": "{schema}", "run_id": "run-abc123",
                 "created_utc": "2026-07-16T00:00:00Z", "code_version": "0.3.0",
                 "lockfile_sha256": "deadbeef", "seed": 42, {extra_field}
                 "config": {{ "initial_capital": 1000000, "mode": "realistic" }},
                 "strategy": {{ "kind": "iron_condor" }},
                 "data_source": {{ "kind": "parquet", "path": "tape.parquet", "sha256": "cafe" }},
                 "metrics": {{ "sharpe": 1.5 }},
                 "row_counts": {{ "fills": {c_fills}, "equity_curve": {c_eq},
                                  "positions": {c_pos}, "greeks_attribution": {c_greeks} }} }}"#
        )
    }

    /// Write a full bundle: `manifest.json` plus the four Parquet tables, each with
    /// its `file_rows` count (which may deliberately disagree with `counts` to
    /// exercise the integrity cross-check).
    fn write_bundle(
        dir: &Path,
        schema: &str,
        file_rows: [usize; 4],
        counts: [u64; 4],
        extra: bool,
    ) {
        if let Err(e) = std::fs::write(
            dir.join(MANIFEST_FILE),
            manifest_json_with(schema, counts, extra),
        ) {
            panic!("write manifest: {e}");
        }
        let [(f0, _), (f1, _), (f2, _), (f3, _)] = TABLES;
        let [r0, r1, r2, r3] = file_rows;
        write_parquet(&dir.join(f0), r0);
        write_parquet(&dir.join(f1), r1);
        write_parquet(&dir.join(f2), r2);
        write_parquet(&dir.join(f3), r3);
    }

    // --- Full-schema table writers (#31: the typed decode reaches every column) --
    //
    // The #30 single-column `write_parquet` is enough for the ceiling/open paths
    // (they reject before the typed decode). The tests that drive `load` through
    // the #31 decoders need REAL tables with every documented column. Each builder
    // emits `n` valid rows in **ascending** `step` order — a CONFORMANT writer that
    // already appends in its stated sort key, which the reader preserves in file
    // order (it never sorts). `order_id`/`position_id` track `step`, so each table's
    // stated sort key orders the same way.

    fn fills_batch(n: usize) -> RecordBatch {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Int32Array, Int64Array, StringArray};
        use arrow_schema::{DataType, Field, Schema};

        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = steps
            .iter()
            .map(|&s| 1_700_000_000_000_000_000 + i64::from(s))
            .collect();
        let trade: Vec<i64> = steps.iter().map(|&s| 100 + i64::from(s)).collect();
        let pos: Vec<i64> = steps.iter().map(|&s| 200 + i64::from(s)).collect();
        let order: Vec<i64> = steps.iter().map(|&s| i64::from(s)).collect();

        let schema = Arc::new(Schema::new(vec![
            Field::new("step", DataType::Int32, false),
            Field::new("ts_ns", DataType::Int64, false),
            Field::new("strategy_run_id", DataType::Utf8, false),
            Field::new("trade_id", DataType::Int64, false),
            Field::new("position_id", DataType::Int64, false),
            Field::new("order_id", DataType::Int64, false),
            Field::new("fill_seq", DataType::Int32, false),
            Field::new("underlying", DataType::Utf8, false),
            Field::new("expiration_ns", DataType::Int64, false),
            Field::new("contract_id", DataType::Utf8, false),
            Field::new("strike_cents", DataType::Int64, false),
            Field::new("style", DataType::Utf8, false),
            Field::new("side", DataType::Utf8, false),
            Field::new("quantity", DataType::Int32, false),
            Field::new("price_cents", DataType::Int64, false),
            Field::new("fees_cents", DataType::Int64, false),
            Field::new("slippage_cents", DataType::Int64, false),
            Field::new("mode", DataType::Utf8, false),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(StringArray::from(vec!["run-abc123"; n])),
            Arc::new(Int64Array::from(trade)),
            Arc::new(Int64Array::from(pos)),
            Arc::new(Int64Array::from(order)),
            Arc::new(Int32Array::from(vec![0_i32; n])),
            Arc::new(StringArray::from(vec!["BTC"; n])),
            Arc::new(Int64Array::from(vec![1_735_286_400_000_000_000_i64; n])),
            Arc::new(StringArray::from(vec![
                "v1:BTC:1735286400000000000:6000000:C";
                n
            ])),
            Arc::new(Int64Array::from(vec![6_000_000_i64; n])),
            Arc::new(StringArray::from(vec!["call"; n])),
            Arc::new(StringArray::from(vec!["long"; n])),
            Arc::new(Int32Array::from(vec![1_i32; n])),
            Arc::new(Int64Array::from(vec![12_500_i64; n])),
            Arc::new(Int64Array::from(vec![30_i64; n])),
            Arc::new(Int64Array::from(vec![-15_i64; n])),
            Arc::new(StringArray::from(vec!["realistic"; n])),
        ];
        match RecordBatch::try_new(schema, cols) {
            Ok(b) => b,
            Err(e) => panic!("build fills batch: {e}"),
        }
    }

    fn equity_batch(n: usize) -> RecordBatch {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Float64Array, Int32Array, Int64Array};
        use arrow_schema::{DataType, Field, Schema};

        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = steps
            .iter()
            .map(|&s| 1_700_000_000_000_000_000 + i64::from(s))
            .collect();
        let cash: Vec<i64> = steps.iter().map(|&s| 990_000 + i64::from(s)).collect();
        let equity: Vec<i64> = steps.iter().map(|&s| 988_500 + i64::from(s)).collect();

        let schema = Arc::new(Schema::new(vec![
            Field::new("step", DataType::Int32, false),
            Field::new("ts_ns", DataType::Int64, false),
            Field::new("cash_cents", DataType::Int64, false),
            Field::new("position_value_cents", DataType::Int64, false),
            Field::new("equity_cents", DataType::Int64, false),
            Field::new("drawdown", DataType::Float64, false),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(Int64Array::from(cash)),
            Arc::new(Int64Array::from(vec![-1_500_i64; n])),
            Arc::new(Int64Array::from(equity)),
            Arc::new(Float64Array::from(vec![-0.015_f64; n])),
        ];
        match RecordBatch::try_new(schema, cols) {
            Ok(b) => b,
            Err(e) => panic!("build equity batch: {e}"),
        }
    }

    fn positions_batch(n: usize) -> RecordBatch {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, BooleanArray, Int32Array, Int64Array, StringArray};
        use arrow_schema::{DataType, Field, Schema};

        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = steps
            .iter()
            .map(|&s| 1_700_000_000_000_000_000 + i64::from(s))
            .collect();
        let pos: Vec<i64> = steps.iter().map(|&s| i64::from(s)).collect();
        // The first (earliest) step carries a terminal `exit_reason`; the last
        // (latest) step is still open at feed exhaustion. Others are open, no exit.
        let exit: Vec<Option<&str>> = steps
            .iter()
            .map(|&s| if s == 0 { Some("expiry") } else { None })
            .collect();
        let open_at_end: Vec<bool> = steps
            .iter()
            .map(|&s| usize::try_from(s).unwrap_or(usize::MAX) == n - 1)
            .collect();

        let schema = Arc::new(Schema::new(vec![
            Field::new("step", DataType::Int32, false),
            Field::new("ts_ns", DataType::Int64, false),
            Field::new("position_id", DataType::Int64, false),
            Field::new("trade_id", DataType::Int64, false),
            Field::new("contract_id", DataType::Utf8, false),
            Field::new("side", DataType::Utf8, false),
            Field::new("quantity", DataType::Int32, false),
            Field::new("avg_price_cents", DataType::Int64, false),
            Field::new("mark_cents", DataType::Int64, false),
            Field::new("unrealized_cents", DataType::Int64, false),
            Field::new("stale_mark", DataType::Boolean, false),
            Field::new("exit_reason", DataType::Utf8, true),
            Field::new("open_at_end", DataType::Boolean, false),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(Int64Array::from(pos)),
            Arc::new(Int64Array::from(vec![7_i64; n])),
            Arc::new(StringArray::from(vec![
                "v1:BTC:1735286400000000000:6000000:C";
                n
            ])),
            Arc::new(StringArray::from(vec!["short"; n])),
            Arc::new(Int32Array::from(vec![1_i32; n])),
            Arc::new(Int64Array::from(vec![12_000_i64; n])),
            Arc::new(Int64Array::from(vec![11_800_i64; n])),
            Arc::new(Int64Array::from(vec![200_i64; n])),
            Arc::new(BooleanArray::from(vec![false; n])),
            Arc::new(StringArray::from(exit)),
            Arc::new(BooleanArray::from(open_at_end)),
        ];
        match RecordBatch::try_new(schema, cols) {
            Ok(b) => b,
            Err(e) => panic!("build positions batch: {e}"),
        }
    }

    fn greeks_batch(n: usize) -> RecordBatch {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Int32Array, Int64Array};
        use arrow_schema::{DataType, Field, Schema};

        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = steps
            .iter()
            .map(|&s| 1_700_000_000_000_000_000 + i64::from(s))
            .collect();
        // `residual` absorbs the remainder so the CONFORMANT fixture satisfies the
        // #32 attribution identity `theta+delta+vega+spread-fees+residual ==
        // step_pnl`, where `step_pnl` is the `equity_batch` step-over-step equity
        // delta (`equity_cents = 988_500 + step`), and step 0 is measured against
        // the manifest's `capital_cents = 1_000_000` (see `manifest_json_with`).
        // The other terms are constant, so `base_terms = 40-120+15+10-30 = -85`.
        const CAPITAL: i64 = 1_000_000;
        const BASE_TERMS: i64 = 40 - 120 + 15 + 10 - 30;
        let equity_of = |s: i32| -> i64 { 988_500 + i64::from(s) };
        let residual: Vec<i64> = steps
            .iter()
            .map(|&s| {
                let step_pnl = if s == 0 {
                    equity_of(0) - CAPITAL
                } else {
                    equity_of(s) - equity_of(s - 1)
                };
                step_pnl - BASE_TERMS
            })
            .collect();

        let schema = Arc::new(Schema::new(vec![
            Field::new("step", DataType::Int32, false),
            Field::new("ts_ns", DataType::Int64, false),
            Field::new("theta_pnl_cents", DataType::Int64, false),
            Field::new("delta_pnl_cents", DataType::Int64, false),
            Field::new("vega_pnl_cents", DataType::Int64, false),
            Field::new("spread_capture_cents", DataType::Int64, false),
            Field::new("fees_cents", DataType::Int64, false),
            Field::new("residual_cents", DataType::Int64, false),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(Int64Array::from(vec![40_i64; n])),
            Arc::new(Int64Array::from(vec![-120_i64; n])),
            Arc::new(Int64Array::from(vec![15_i64; n])),
            Arc::new(Int64Array::from(vec![10_i64; n])),
            Arc::new(Int64Array::from(vec![30_i64; n])),
            Arc::new(Int64Array::from(residual)),
        ];
        match RecordBatch::try_new(schema, cols) {
            Ok(b) => b,
            Err(e) => panic!("build greeks batch: {e}"),
        }
    }

    /// Serialise one `RecordBatch` to a Parquet file, optionally ZSTD-compressed.
    fn write_record_batch(path: &Path, batch: &RecordBatch, compressed: bool) {
        use parquet::arrow::ArrowWriter;
        use parquet::basic::{Compression, ZstdLevel};
        use parquet::file::properties::WriterProperties;

        let file = match std::fs::File::create(path) {
            Ok(f) => f,
            Err(e) => panic!("create {}: {e}", path.display()),
        };
        let props = if compressed {
            Some(
                WriterProperties::builder()
                    .set_compression(Compression::ZSTD(ZstdLevel::default()))
                    .build(),
            )
        } else {
            None
        };
        let mut writer = match ArrowWriter::try_new(file, batch.schema(), props) {
            Ok(w) => w,
            Err(e) => panic!("arrow writer: {e}"),
        };
        if let Err(e) = writer.write(batch) {
            panic!("write batch: {e}");
        }
        if let Err(e) = writer.close() {
            panic!("close writer: {e}");
        }
    }

    /// Write a **decompression-bomb** Parquet file at `path`: a single
    /// `step: INT64` column of `num_rows` identical values, PLAIN-encoded (no
    /// dictionary) and ZSTD-compressed. The uncompressed footer size is `8 ×
    /// num_rows` but the identical bytes crush to near-nothing, so
    /// `total_byte_size ≫ file_bytes` clears the default expansion ratio — the
    /// shape #36's `decompression_bomb` fixture commits. Mirrors
    /// `tests/common/bundle_gen.rs::bomb_fills_batch`.
    fn write_bomb_parquet(path: &Path, num_rows: usize) {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Int64Array, RecordBatch};
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        use parquet::basic::{Compression, ZstdLevel};
        use parquet::file::properties::WriterProperties;

        let schema = Arc::new(Schema::new(vec![Field::new(
            "step",
            DataType::Int64,
            false,
        )]));
        let col: ArrayRef = Arc::new(Int64Array::from(vec![0_i64; num_rows]));
        let batch = match RecordBatch::try_new(Arc::clone(&schema), vec![col]) {
            Ok(b) => b,
            Err(e) => panic!("build bomb batch: {e}"),
        };
        let file = match std::fs::File::create(path) {
            Ok(f) => f,
            Err(e) => panic!("create {}: {e}", path.display()),
        };
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .set_dictionary_enabled(false)
            .build();
        let mut writer = match ArrowWriter::try_new(file, schema, Some(props)) {
            Ok(w) => w,
            Err(e) => panic!("arrow writer: {e}"),
        };
        if let Err(e) = writer.write(&batch) {
            panic!("write bomb batch: {e}");
        }
        if let Err(e) = writer.close() {
            panic!("close bomb writer: {e}");
        }
    }

    /// Write a full-schema bundle: `manifest.json` plus the four Parquet tables
    /// with EVERY documented column, so `load`'s typed decoders run end-to-end.
    fn write_full_bundle(
        dir: &Path,
        schema: &str,
        file_rows: [usize; 4],
        counts: [u64; 4],
        extra: bool,
        compressed: bool,
    ) {
        if let Err(e) = std::fs::write(
            dir.join(MANIFEST_FILE),
            manifest_json_with(schema, counts, extra),
        ) {
            panic!("write manifest: {e}");
        }
        let [(f0, _), (f1, _), (f2, _), (f3, _)] = TABLES;
        let [r0, r1, r2, r3] = file_rows;
        write_record_batch(&dir.join(f0), &fills_batch(r0), compressed);
        write_record_batch(&dir.join(f1), &equity_batch(r1), compressed);
        write_record_batch(&dir.join(f2), &positions_batch(r2), compressed);
        write_record_batch(&dir.join(f3), &greeks_batch(r3), compressed);
    }

    #[track_caller]
    fn open_ok(root: &Path, ceilings: ResourceCeilings) -> BundleReader {
        match BundleReader::open_with_ceilings(root, ceilings) {
            Ok(r) => r,
            Err(e) => panic!("open_with_ceilings should succeed: {e}"),
        }
    }

    /// A sorted `(name, size)` snapshot of a directory's entries.
    fn dir_snapshot(dir: &Path) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => panic!("read_dir: {e}"),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => panic!("dir entry: {e}"),
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let len = match entry.metadata() {
                Ok(m) => m.len(),
                Err(e) => panic!("metadata: {e}"),
            };
            out.push((name, len));
        }
        out.sort();
        out
    }

    // --- open: presence, schema gate, row_counts shape ------------------------

    #[test]
    fn test_open_and_load_happy_path() {
        let dir = temp_bundle_dir();
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [4, 10, 8, 10],
            [4, 10, 8, 10],
            false,
            false,
        );
        let reader = match BundleReader::open(dir.path()) {
            Ok(r) => r,
            Err(e) => panic!("a well-formed bundle should open: {e}"),
        };
        assert_eq!(reader.manifest().schema, SUPPORTED_SCHEMA);
        let loaded = match reader.load() {
            Ok(l) => l,
            Err(e) => panic!("a well-formed bundle should load under default ceilings: {e}"),
        };
        assert_eq!(loaded.manifest.run_id, "run-abc123");
        // #31 wires the typed decode: the four tables are now populated in FILE
        // order (the reader never sorts). The conformant fixture appends rows
        // already in sort-key order, so they come back non-decreasing on each key.
        assert_eq!(loaded.fills.len(), 4);
        assert_eq!(loaded.equity.len(), 10);
        assert_eq!(loaded.positions.len(), 8);
        assert_eq!(loaded.greeks.len(), 10);
        assert!(
            loaded
                .fills
                .is_sorted_by_key(|f| (f.step, f.order_id, f.fill_seq))
        );
        assert!(loaded.equity.is_sorted_by_key(|e| e.step));
        assert!(
            loaded
                .positions
                .is_sorted_by_key(|p| (p.step, p.position_id))
        );
        assert!(loaded.greeks.is_sorted_by_key(|g| g.step));
    }

    #[test]
    fn test_open_missing_directory_is_io() {
        match BundleReader::open("/no/such/bundle/dir-xyz") {
            Err(BundleError::Io(_)) => {}
            other => panic!("a missing bundle dir must be a typed Io error, got {other:?}"),
        }
    }

    #[test]
    fn test_open_missing_manifest_is_missing_table() {
        let dir = temp_bundle_dir();
        // The four Parquet files but NO manifest.
        for (file, _key) in TABLES {
            write_parquet(&dir.path().join(file), 1);
        }
        match BundleReader::open(dir.path()) {
            Err(BundleError::MissingTable(t)) => assert_eq!(t, MANIFEST_FILE),
            other => panic!("a missing manifest must be MissingTable, got {other:?}"),
        }
    }

    #[test]
    fn test_open_bad_schema_is_unsupported() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            "ironcondor.bundle.v2",
            [1, 1, 1, 1],
            [1, 1, 1, 1],
            false,
        );
        match BundleReader::open(dir.path()) {
            Err(BundleError::UnsupportedSchema(tag)) => assert_eq!(tag, "ironcondor.bundle.v2"),
            other => panic!("a major-incompatible tag must be UnsupportedSchema, got {other:?}"),
        }
    }

    #[test]
    fn test_open_unknown_manifest_field_still_opens() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [1, 1, 1, 1],
            [1, 1, 1, 1],
            true,
        );
        assert!(
            BundleReader::open(dir.path()).is_ok(),
            "a newer-minor extra manifest field must still open (permissive)"
        );
    }

    #[test]
    fn test_open_missing_table_is_missing_table() {
        let dir = temp_bundle_dir();
        if let Err(e) = std::fs::write(
            dir.path().join(MANIFEST_FILE),
            manifest_json_with(SUPPORTED_SCHEMA, [1, 1, 1, 1], false),
        ) {
            panic!("write manifest: {e}");
        }
        // Only three of the four tables.
        write_parquet(&dir.path().join("fills.parquet"), 1);
        write_parquet(&dir.path().join("equity_curve.parquet"), 1);
        write_parquet(&dir.path().join("positions.parquet"), 1);
        match BundleReader::open(dir.path()) {
            Err(BundleError::MissingTable(t)) => assert_eq!(t, "greeks_attribution.parquet"),
            other => panic!("a missing table must be MissingTable, got {other:?}"),
        }
    }

    #[test]
    fn test_open_wrong_keyed_row_counts_is_invariant() {
        let dir = temp_bundle_dir();
        // A `row_counts` missing the `greeks_attribution` key.
        let manifest = r#"{ "schema": "ironcondor.bundle.v1", "run_id": "r",
            "created_utc": "t", "code_version": "v", "lockfile_sha256": "s", "seed": 1,
            "config": {}, "strategy": {}, "data_source": {}, "metrics": {},
            "row_counts": { "fills": 1, "equity_curve": 1, "positions": 1 } }"#;
        if let Err(e) = std::fs::write(dir.path().join(MANIFEST_FILE), manifest) {
            panic!("write manifest: {e}");
        }
        for (file, _key) in TABLES {
            write_parquet(&dir.path().join(file), 1);
        }
        match BundleReader::open(dir.path()) {
            Err(BundleError::Invariant(_)) => {}
            other => panic!("a wrong-keyed row_counts must be Invariant, got {other:?}"),
        }
    }

    // --- Ceiling 1 / 2 / 3 (each rejects with the allocation bounded) ---------

    #[test]
    fn test_ceiling1_oversized_file_is_too_large_pre_open() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [16, 16, 16, 16],
            [16, 16, 16, 16],
            false,
        );
        // 1-byte per-file ceiling: every real Parquet file exceeds it, rejected on
        // the pre-open `stat`, before the footer or any batch is read.
        let reader = open_ok(
            dir.path(),
            ResourceCeilings {
                max_table_bytes: 1,
                ..ResourceCeilings::default()
            },
        );
        match reader.load() {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("an oversized file must be TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn test_ceiling2_footer_rowcount_is_too_large_pre_decode() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [10, 10, 10, 10],
            [10, 10, 10, 10],
            false,
        );
        let reader = open_ok(
            dir.path(),
            ResourceCeilings {
                max_table_rows: 5,
                ..ResourceCeilings::default()
            },
        );
        match reader.load() {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("a footer row count over the ceiling must be TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn test_ceiling2_footer_rowcount_mismatch_is_invariant() {
        let dir = temp_bundle_dir();
        // Manifest claims 10 fills rows; the file has 7 — the integrity hint and
        // the footer disagree.
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [7, 10, 8, 10],
            [10, 10, 8, 10],
            false,
        );
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        match reader.load() {
            Err(BundleError::Invariant(_)) => {}
            other => panic!("a footer/row_counts mismatch must be Invariant, got {other:?}"),
        }
    }

    #[test]
    fn test_ceiling3_tiny_working_set_is_too_large() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [64, 64, 64, 64],
            [64, 64, 64, 64],
            false,
        );
        let reader = open_ok(
            dir.path(),
            ResourceCeilings {
                max_working_set: 8,
                max_batch_bytes: 8,
                ..ResourceCeilings::default()
            },
        );
        match reader.load() {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("a working set over the ceiling must be TooLarge, got {other:?}"),
        }
    }

    // --- Working-set budget: the measured, bounded, stop-before-crossing guard -

    #[test]
    fn test_working_set_budget_stops_before_crossing_ceiling() {
        // This is the authoritative bounded-invariant assertion behind the
        // "lying footer" case: the measured cumulative tally stops mid-decode and
        // `used()` stays strictly under the ceiling on the rejecting path.
        let ceilings = ResourceCeilings {
            max_working_set: 100,
            max_batch_bytes: 60,
            ..ResourceCeilings::default()
        };
        let mut budget = WorkingSetBudget::new(&ceilings);
        assert!(budget.account(40).is_ok());
        assert!(budget.account(40).is_ok());
        assert_eq!(budget.used(), 80);
        // A third 40-byte batch would reach 120 > 100 — rejected, NOT committed.
        match budget.account(40) {
            Err(BundleError::TooLarge(_)) => {}
            other => {
                panic!("the batch that would exceed the ceiling must be TooLarge, got {other:?}")
            }
        }
        assert!(
            budget.used() < ceilings.max_working_set,
            "cumulative bytes must stay strictly under the ceiling even on the reject path"
        );
        assert_eq!(budget.used(), 80, "a rejected batch is never committed");
    }

    #[test]
    fn test_working_set_budget_rejects_oversized_batch() {
        let ceilings = ResourceCeilings {
            max_working_set: 1_000,
            max_batch_bytes: 50,
            ..ResourceCeilings::default()
        };
        let mut budget = WorkingSetBudget::new(&ceilings);
        match budget.account(51) {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("a batch over the per-batch cap must be TooLarge, got {other:?}"),
        }
        assert_eq!(budget.used(), 0, "a rejected batch is never committed");
    }

    // --- #36 adversarial bounded reject: the decoder is never invoked ----------
    //
    // These drive `scan_table` directly with a probe `decode` closure so the
    // "reject without materializing the hostile payload" property is asserted
    // POSITIVELY — the closure (which is where the #31 typed decode allocates the
    // owned rows) is never called and the working-set budget commits nothing — not
    // merely on the returned error variant. They are the unit-level counterpart of
    // the committed `tests/fixtures/bundle/{decompression_bomb,oversized_footer,
    // rowcount_lie}` fixtures (#36), reusing the #30 machinery.

    /// The absent-others scaffold for a bomb bundle: `manifest.json` (honest
    /// `fills` count so the footer/`row_counts` cross-check passes) plus three tiny
    /// stub tables so `open` finds all four present; `fills.parquet` is the bomb.
    fn write_bomb_bundle(dir: &Path, fills_rows: usize) {
        let counts = [u64::try_from(fills_rows).unwrap_or(u64::MAX), 1, 1, 1];
        if let Err(e) = std::fs::write(
            dir.join(MANIFEST_FILE),
            manifest_json_with(SUPPORTED_SCHEMA, counts, false),
        ) {
            panic!("write manifest: {e}");
        }
        write_bomb_parquet(&dir.join("fills.parquet"), fills_rows);
        write_parquet(&dir.join("equity_curve.parquet"), 1);
        write_parquet(&dir.join("positions.parquet"), 1);
        write_parquet(&dir.join("greeks_attribution.parquet"), 1);
    }

    /// Scan `fills.parquet` through the real ceiling path with a probe decoder,
    /// returning the scan result plus whether the decoder was ever invoked and the
    /// working set committed. The decoder flips a flag — if a pre-decode ceiling
    /// rejects, it is never called and no batch is materialized.
    #[track_caller]
    fn scan_fills_probing_decoder(reader: &BundleReader) -> (Result<(), BundleError>, bool, u64) {
        use std::cell::Cell;

        let mut budget = WorkingSetBudget::new(reader.ceilings());
        let invoked = Cell::new(false);
        let result = reader.scan_table(
            "fills.parquet",
            "fills",
            &mut budget,
            &|| false,
            &mut |_batch| {
                invoked.set(true);
                Ok(0)
            },
        );
        (result, invoked.get(), budget.used())
    }

    #[test]
    fn test_decompression_bomb_rejects_before_invoking_the_decoder() {
        let dir = temp_bundle_dir();
        // 8 000 identical 8-byte values -> ~64 KiB uncompressed, ~sub-KiB on disk:
        // the footer's uncompressed size clears the default 20x expansion ratio.
        write_bomb_bundle(dir.path(), 8_000);
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        let (result, invoked, used) = scan_fills_probing_decoder(&reader);
        match result {
            Err(BundleError::TooLarge(detail)) => assert!(
                detail.contains("decompression bomb"),
                "the bomb must be rejected by the pre-decode bomb check: {detail}"
            ),
            other => panic!("a decompression bomb must be TooLarge, got {other:?}"),
        }
        assert!(
            !invoked,
            "the bomb must be rejected BEFORE the decoder materializes any batch"
        );
        assert_eq!(
            used, 0,
            "no working set is committed on the pre-decode reject"
        );
    }

    #[test]
    fn test_oversized_footer_rows_reject_before_invoking_the_decoder() {
        let dir = temp_bundle_dir();
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [8, 8, 8, 8],
            [8, 8, 8, 8],
            false,
            false,
        );
        // A per-table row ceiling below the honest 8-row footer trips ceiling 2.
        let reader = open_ok(
            dir.path(),
            ResourceCeilings {
                max_table_rows: 4,
                ..ResourceCeilings::default()
            },
        );
        let (result, invoked, used) = scan_fills_probing_decoder(&reader);
        assert!(
            matches!(result, Err(BundleError::TooLarge(_))),
            "an over-ceiling footer must be TooLarge, got {result:?}"
        );
        assert!(!invoked, "ceiling 2 rejects before the decoder runs");
        assert_eq!(
            used, 0,
            "no working set is committed on the pre-decode reject"
        );
    }

    #[test]
    fn test_rowcount_lie_rejects_before_invoking_the_decoder() {
        let dir = temp_bundle_dir();
        // The fills FILE has 4 rows; the manifest lies and claims 10.
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [4, 10, 8, 10],
            [10, 10, 8, 10],
            false,
            false,
        );
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        let (result, invoked, used) = scan_fills_probing_decoder(&reader);
        assert!(
            matches!(result, Err(BundleError::Invariant(_))),
            "a footer/row_counts lie must be Invariant, got {result:?}"
        );
        assert!(
            !invoked,
            "the row_counts cross-check rejects before the decoder runs — the hint \
             never sizes an allocation"
        );
        assert_eq!(
            used, 0,
            "no working set is committed on the pre-decode reject"
        );
    }

    // --- Checked conversions (never an `as` cast on a footer value) ------------

    #[test]
    fn test_checked_footer_conversion_rejects_negative() {
        // A negative (corrupt/lying) footer value is TooLarge, not a wrapped giant.
        match footer_i64_to_u64(-1, "footer row count") {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("a negative footer value must be TooLarge, got {other:?}"),
        }
        match footer_i64_to_u64(i64::MIN, "footer size") {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("i64::MIN must be TooLarge, got {other:?}"),
        }
        assert_eq!(footer_i64_to_u64(42, "x").ok(), Some(42));
    }

    #[test]
    fn test_apply_overhead_overflow_is_too_large() {
        // The overhead multiply is checked: an absurd measured size that overflows
        // u64 is TooLarge, never a wrapped allocation size.
        match apply_overhead(u64::MAX, DECODED_OVERHEAD_PERMILLE) {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("an overflowing overhead multiply must be TooLarge, got {other:?}"),
        }
        assert_eq!(apply_overhead(1_000, 1_500).ok(), Some(1_500));
    }

    // --- Cancellation at a batch boundary -------------------------------------

    #[test]
    fn test_load_pre_cancelled_returns_promptly() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [4, 10, 8, 10],
            [4, 10, 8, 10],
            false,
        );
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        match reader.load_cancellable(&|| true) {
            Err(BundleError::Cancelled) => {}
            other => panic!("a pre-cancelled load must be Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn test_load_cancels_mid_decode_at_batch_boundary() {
        use std::cell::Cell;

        let dir = temp_bundle_dir();
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [8, 8, 8, 8],
            [8, 8, 8, 8],
            false,
            false,
        );
        // One row per batch -> many batch boundaries to observe cancellation at.
        let reader = open_ok(
            dir.path(),
            ResourceCeilings {
                max_batch_rows: 1,
                ..ResourceCeilings::default()
            },
        );
        let polls = Cell::new(0_u32);
        let probe = || {
            let n = polls.get().saturating_add(1);
            polls.set(n);
            n > 3
        };
        match reader.load_cancellable(&probe) {
            Err(BundleError::Cancelled) => {}
            other => panic!("a mid-decode cancellation must be Cancelled, got {other:?}"),
        }
        // Promptly: far fewer polls than a full 32-row (4-table) decode would need.
        assert!(
            polls.get() <= 6,
            "cancellation must abort promptly, got {} polls",
            polls.get()
        );
    }

    // --- Read-only: load never mutates the bundle -----------------------------

    #[test]
    fn test_load_is_read_only() {
        let dir = temp_bundle_dir();
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [4, 10, 8, 10],
            [4, 10, 8, 10],
            false,
            false,
        );
        let before = dir_snapshot(dir.path());
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        let _ = reader.load();
        let after = dir_snapshot(dir.path());
        assert_eq!(
            before, after,
            "load must not add, remove, or modify any bundle file"
        );
    }

    // --- Ceiling knobs: documented defaults validate; out-of-range is typed ----

    #[test]
    fn test_resource_ceilings_default_validates() {
        assert!(
            ResourceCeilings::default().validate().is_ok(),
            "the documented defaults must be valid"
        );
    }

    #[test]
    fn test_resource_ceilings_out_of_range_is_config_error() {
        let zero = ResourceCeilings {
            max_table_bytes: 0,
            ..ResourceCeilings::default()
        };
        match zero.validate() {
            Err(ConfigError::InvalidValue { field, .. }) => {
                assert_eq!(field, "replay.max_table_bytes")
            }
            other => panic!("a zero ceiling must be a ConfigError, got {other:?}"),
        }
        let bad_overhead = ResourceCeilings {
            decoded_overhead_permille: 900,
            ..ResourceCeilings::default()
        };
        assert!(
            matches!(
                bad_overhead.validate(),
                Err(ConfigError::InvalidValue { .. })
            ),
            "an overhead below 1.0x must be rejected"
        );
        let bad_batch = ResourceCeilings {
            max_batch_bytes: MAX_WORKING_SET + 1,
            ..ResourceCeilings::default()
        };
        assert!(
            matches!(bad_batch.validate(), Err(ConfigError::InvalidValue { .. })),
            "a per-batch cap above the working-set ceiling must be rejected"
        );
        // The manifest ceiling is a validated knob too.
        let bad_manifest = ResourceCeilings {
            max_manifest_bytes: 0,
            ..ResourceCeilings::default()
        };
        match bad_manifest.validate() {
            Err(ConfigError::InvalidValue { field, .. }) => {
                assert_eq!(field, "replay.max_manifest_bytes")
            }
            other => panic!("a zero manifest ceiling must be a ConfigError, got {other:?}"),
        }
    }

    // --- BLOCKER: manifest byte ceiling (manifest bomb) -----------------------

    #[test]
    fn test_ceiling_oversized_manifest_rejects_pre_read() {
        let dir = temp_bundle_dir();
        // The four tables are present, so table presence is not the failure.
        for (file, _key) in TABLES {
            write_parquet(&dir.path().join(file), 1);
        }
        // A manifest LARGER than a tiny ceiling whose CONTENT is garbage: it must
        // be rejected on the pre-read `stat`, before a byte is parsed — proving
        // the reject fires from the stat, since a read+parse of this junk would be
        // `Invariant`, not `TooLarge`. (In the real bomb this Vec is 10 GiB.)
        let junk = "x".repeat(4096);
        if let Err(e) = std::fs::write(dir.path().join(MANIFEST_FILE), &junk) {
            panic!("write manifest: {e}");
        }
        let ceilings = ResourceCeilings {
            max_manifest_bytes: 64,
            ..ResourceCeilings::default()
        };
        match BundleReader::open_with_ceilings(dir.path(), ceilings) {
            Err(BundleError::TooLarge(detail)) => assert!(
                detail.contains(MANIFEST_FILE),
                "the oversized artifact must be named the manifest: {detail}"
            ),
            other => {
                panic!("an oversized manifest must be TooLarge pre-read, got {other:?}")
            }
        }
    }

    #[test]
    fn test_normal_manifest_opens_under_a_finite_manifest_ceiling() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [1, 1, 1, 1],
            [1, 1, 1, 1],
            false,
        );
        // A generous-but-finite manifest ceiling still opens a tiny valid manifest.
        let ceilings = ResourceCeilings {
            max_manifest_bytes: 8 * 1024,
            ..ResourceCeilings::default()
        };
        assert!(
            BundleReader::open_with_ceilings(dir.path(), ceilings).is_ok(),
            "a normal manifest must open under a finite manifest ceiling"
        );
    }

    // --- SHOULD-FIX 1: ceilings validated on the enforcement path -------------

    #[test]
    fn test_open_with_invalid_ceilings_is_typed_error_not_silent_open() {
        let dir = temp_bundle_dir();
        write_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [1, 1, 1, 1],
            [1, 1, 1, 1],
            false,
        );
        // `max_batch_rows: 0` would make `with_batch_size(0)` yield zero batches,
        // silently disabling the measured working-set guard — so it must be a
        // typed error on open, never a silent open.
        let bad = ResourceCeilings {
            max_batch_rows: 0,
            ..ResourceCeilings::default()
        };
        match BundleReader::open_with_ceilings(dir.path(), bad) {
            Err(BundleError::Config(ConfigError::InvalidValue { field, .. })) => {
                assert_eq!(field, "replay.max_batch_rows")
            }
            other => panic!(
                "an invalid ceiling on the enforcement path must be a typed \
                 BundleError::Config, never a silent open, got {other:?}"
            ),
        }
    }

    // --- SHOULD-FIX 2: attacker-controlled schema tag is clamped --------------

    #[test]
    fn test_unsupported_schema_tag_is_clamped() {
        // A 10 KiB junk tag must not ride into the error message length-unbounded.
        let junk = "z".repeat(10 * 1024);
        let err = unsupported_schema(junk);
        match &err {
            BundleError::UnsupportedSchema(tag) => assert!(
                tag.chars().count() <= MAX_SCHEMA_TAG_CHARS + 1,
                "the tag must be clamped (<= {} chars + ellipsis), got {}",
                MAX_SCHEMA_TAG_CHARS,
                tag.chars().count()
            ),
            other => panic!("expected UnsupportedSchema, got {other:?}"),
        }
        // The whole rendered Display is bounded: the 20-char category prefix plus
        // the clamped tag (<= MAX_SCHEMA_TAG_CHARS + 1 for the ellipsis) — ~85,
        // orders of magnitude below the 10 KiB input, and still names the variant.
        let rendered = err.to_string();
        let bound = "unsupported schema: ".chars().count() + MAX_SCHEMA_TAG_CHARS + 1;
        assert!(
            rendered.chars().count() <= bound,
            "rendered message must be bounded (<= {bound}), got {}",
            rendered.chars().count()
        );
        assert!(rendered.starts_with("unsupported schema: "));
    }

    #[test]
    fn test_clamp_schema_tag_never_panics_on_multibyte_utf8() {
        // Multi-byte chars: clamping on char boundaries must never split a byte
        // index and must stay bounded.
        let multibyte = "🚀".repeat(200);
        let clamped = clamp_schema_tag(multibyte);
        assert!(clamped.chars().count() <= MAX_SCHEMA_TAG_CHARS + 1);
        // A short tag is returned unchanged (no ellipsis).
        assert_eq!(
            clamp_schema_tag("ironcondor.bundle.v9".to_owned()),
            "ironcondor.bundle.v9"
        );
    }

    // --- Cheap extra: a ZSTD-compressed bundle decodes (codec feature works) ----

    #[test]
    fn test_load_decodes_zstd_compressed_tables() {
        let dir = temp_bundle_dir();
        // The four full-schema tables written with ZSTD page compression — proving
        // the enabled `zstd` codec feature decodes through the typed #31 path.
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [3, 3, 3, 3],
            [3, 3, 3, 3],
            false,
            true,
        );
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        match reader.load() {
            Ok(loaded) => {
                assert_eq!(loaded.manifest.schema, SUPPORTED_SCHEMA);
                assert_eq!(loaded.fills.len(), 3);
                assert_eq!(loaded.equity.len(), 3);
                assert_eq!(loaded.positions.len(), 3);
                assert_eq!(loaded.greeks.len(), 3);
            }
            Err(e) => {
                panic!("a ZSTD-compressed bundle must decode under the enabled codec: {e}")
            }
        }
    }

    // --- #31: `load` returns the exact rows written, in FILE order (no sort) ------

    #[test]
    fn test_load_returns_typed_rows_in_file_order() {
        let dir = temp_bundle_dir();
        // A CONFORMANT writer: the fixtures append rows already in each table's
        // stated sort-key order (ascending `step`). The reader NEVER sorts — it
        // returns rows in FILE order — so a conformant bundle round-trips verbatim.
        // (An out-of-order bundle would come back out-of-order too; rejecting the
        // ordering violation is the #32 validation chain's job, not this reader's.)
        write_full_bundle(
            dir.path(),
            SUPPORTED_SCHEMA,
            [3, 3, 3, 3],
            [3, 3, 3, 3],
            false,
            false,
        );
        let reader = open_ok(dir.path(), ResourceCeilings::default());
        let loaded = match reader.load() {
            Ok(l) => l,
            Err(e) => panic!("a well-formed full bundle should load: {e}"),
        };

        // Exact row equality, field for field, in FILE order — the same ascending
        // order the conformant fixtures wrote (money stays integer cents, the signed
        // fields keep their sign, `exit_reason`/`open_at_end` decode per-row).
        let base_ts = 1_700_000_000_000_000_000_i64;
        let cid = "v1:BTC:1735286400000000000:6000000:C";

        let expected_fills: Vec<Fill> = (0..3_u32)
            .map(|s| Fill {
                step: s,
                ts_ns: base_ts + i64::from(s),
                strategy_run_id: "run-abc123".to_owned(),
                trade_id: 100 + u64::from(s),
                position_id: 200 + u64::from(s),
                order_id: u64::from(s),
                fill_seq: 0,
                underlying: "BTC".to_owned(),
                expiration_ns: 1_735_286_400_000_000_000,
                contract_id: cid.to_owned(),
                strike_cents: 6_000_000,
                style: OptionStyle::Call,
                side: PositionSide::Long,
                quantity: 1,
                price_cents: 12_500,
                fees_cents: 30,
                slippage_cents: -15,
                mode: ExecMode::Realistic,
            })
            .collect();

        let expected_equity: Vec<EquityPoint> = (0..3_u32)
            .map(|s| EquityPoint {
                step: s,
                ts_ns: base_ts + i64::from(s),
                cash_cents: 990_000 + i64::from(s),
                position_value_cents: -1_500,
                equity_cents: 988_500 + i64::from(s),
                drawdown: -0.015,
            })
            .collect();

        let expected_positions: Vec<PositionRow> = (0..3_u32)
            .map(|s| PositionRow {
                step: s,
                ts_ns: base_ts + i64::from(s),
                position_id: u64::from(s),
                trade_id: 7,
                contract_id: cid.to_owned(),
                side: PositionSide::Short,
                quantity: 1,
                avg_price_cents: 12_000,
                mark_cents: 11_800,
                unrealized_cents: 200,
                stale_mark: false,
                exit_reason: if s == 0 {
                    Some("expiry".to_owned())
                } else {
                    None
                },
                open_at_end: s == 2,
            })
            .collect();

        // `residual` absorbs the remainder so the conformant fixture satisfies the
        // #32 attribution identity: step 0 is measured against `capital_cents`
        // (`988_500 − 1_000_000 = −11_500`, minus `base_terms = −85` ⇒ `−11_415`),
        // later steps against the +1/step equity delta (`1 − (−85) = 86`).
        let expected_greeks: Vec<GreeksAttribution> = (0..3_u32)
            .map(|s| GreeksAttribution {
                step: s,
                ts_ns: base_ts + i64::from(s),
                theta_pnl_cents: 40,
                delta_pnl_cents: -120,
                vega_pnl_cents: 15,
                spread_capture_cents: 10,
                fees_cents: 30,
                residual_cents: if s == 0 { -11_415 } else { 86 },
            })
            .collect();

        assert_eq!(
            loaded.fills, expected_fills,
            "fills must round-trip verbatim in file order"
        );
        assert_eq!(
            loaded.equity, expected_equity,
            "equity must round-trip verbatim in file order"
        );
        assert_eq!(
            loaded.positions, expected_positions,
            "positions must round-trip verbatim in file order"
        );
        assert_eq!(
            loaded.greeks, expected_greeks,
            "greeks must round-trip verbatim in file order"
        );
    }

    // --- #31 review fix: the working-set budget measures the RETAINED rows --------

    /// A full-schema fills batch whose `contract_id` column is a
    /// `Dictionary(Int32, Utf8)` holding ONE `value`, keyed by every row — the Arrow
    /// batch stores that string once, while the typed decode copies it per row.
    fn fills_batch_with_dict_contract_id(n: usize, value: &str) -> RecordBatch {
        use std::sync::Arc;

        use arrow_array::types::Int32Type;
        use arrow_array::{ArrayRef, DictionaryArray, Int32Array, StringArray};
        use arrow_schema::{DataType, Field, Schema};

        let base = fills_batch(n);
        let keys = Int32Array::from(vec![0_i32; n]);
        let values = Arc::new(StringArray::from(vec![value])) as ArrayRef;
        let dict: ArrayRef = match DictionaryArray::<Int32Type>::try_new(keys, values) {
            Ok(d) => Arc::new(d),
            Err(e) => panic!("build dictionary array: {e}"),
        };
        let dict_type = DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8));

        let mut fields: Vec<Field> = base
            .schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        let mut cols: Vec<ArrayRef> = base.columns().to_vec();
        let idx = match base.schema().index_of("contract_id") {
            Ok(i) => i,
            Err(e) => panic!("fills batch missing contract_id: {e}"),
        };
        if let Some(slot) = fields.get_mut(idx) {
            *slot = Field::new("contract_id", dict_type, false);
        }
        if let Some(slot) = cols.get_mut(idx) {
            *slot = dict;
        }
        match RecordBatch::try_new(Arc::new(Schema::new(fields)), cols) {
            Ok(b) => b,
            Err(e) => panic!("build dict fills batch: {e}"),
        }
    }

    #[test]
    fn test_working_set_budget_trips_on_retained_dictionary_bytes() {
        // The dictionary-encoding gap: a `Dictionary(Int32, Utf8)` stores its long
        // value ONCE (plus one Int32 key per row), so the Arrow batch footprint stays
        // small — but the typed decode copies the value into EVERY owned row. The
        // retained accounting must catch what the batch estimate alone misses.
        let n = 64;
        let long = "L".repeat(1024);
        let batch = fills_batch_with_dict_contract_id(n, &long);

        // What the measured loop accounts for the batch itself: Arrow array memory
        // size inflated by the transient-overhead factor.
        let arrow_bytes = u64::try_from(batch.get_array_memory_size()).unwrap_or(u64::MAX);
        let batch_estimate = match apply_overhead(arrow_bytes, DECODED_OVERHEAD_PERMILLE) {
            Ok(v) => v,
            Err(e) => panic!("overhead multiply: {e}"),
        };

        // What the typed decode retains: owned rows + the value copied into all N rows.
        let mut out = Vec::new();
        let retained = match tables::read_fills(&batch, &mut out) {
            Ok(r) => r,
            Err(e) => panic!("read_fills should succeed: {e}"),
        };
        assert_eq!(out.len(), n);
        assert!(
            retained > batch_estimate,
            "the per-row copied dictionary string must make retained {retained} exceed the \
             Arrow batch estimate {batch_estimate}"
        );

        // A ceiling that ADMITS the batch estimate but not the estimate + retained:
        // the transient batch accounting alone would pass, yet the retained accounting
        // trips it — exactly the working-set breach the old batch-only tally missed.
        let ceilings = ResourceCeilings {
            max_working_set: batch_estimate + retained - 1,
            max_batch_bytes: retained,
            ..ResourceCeilings::default()
        };
        let mut budget = WorkingSetBudget::new(&ceilings);
        match budget.account(batch_estimate) {
            Ok(()) => {}
            Err(e) => panic!("the transient batch estimate alone must fit under the ceiling: {e}"),
        }
        match budget.account(retained) {
            Err(BundleError::TooLarge(_)) => {}
            other => panic!("the retained bytes must trip the working-set ceiling, got {other:?}"),
        }
        assert!(
            budget.used() < ceilings.max_working_set,
            "a rejected retained batch is never committed; used stays under the ceiling"
        );
    }
}
