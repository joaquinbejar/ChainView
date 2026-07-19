//! Deterministic generator for the committed replay-bundle fixtures and the HP-4
//! decode bench (issue #36, `docs/TESTING.md` §6, `docs/06-performance.md` §3.4).
//!
//! This module is **shared test/bench infrastructure**, not a test target: it
//! lives under `tests/common/` (a subdirectory, so cargo never compiles it as its
//! own integration binary) and is pulled into both
//! `tests/replay_bundle_fixtures.rs` (which regenerates the committed corpus and
//! reads it back through the real [`chainview::BundleReader`]) and
//! `benches/bench_replay_decode.rs` (which writes a larger bundle into a tempdir
//! and times the decode).
//!
//! # Determinism
//!
//! Every byte is a pure function of the row count and the fixed constants below —
//! **no timestamps, no RNG, no wall-clock read**. The Arrow/Parquet writers are
//! deterministic for identical input, so regenerating the corpus reproduces the
//! committed bytes byte-for-byte on the same `parquet` crate version. The money in
//! every table is integer cents; the only `f64` produced is `equity.drawdown`, the
//! documented analytic ratio.
//!
//! # Chain-conformant shape
//!
//! The conformant tables satisfy the **entire** #32 validation chain: one fill and
//! one position per step (`position_id == step`, so `fills.position_id` references a
//! real leg), the equity identity
//! (`equity_cents == cash_cents + position_value_cents`), the mark-to-market
//! attribution identity (`residual` absorbs the per-step remainder against the
//! equity delta / opening capital), a contiguous 0-based step domain with per-step
//! `ts_ns` agreement across all four tables, and the `contract_id` round-trip. The
//! adversarial builders each derive a single targeted defect from this base.

// Each of the two includers (the fixtures test and the bench) uses only a subset
// of these builders, so the unused ones would otherwise trip `-D dead_code`.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use arrow_array::{
    ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

// --- Fixed conformant constants (money is integer cents) ---------------------

/// The single supported schema tag (the compatibility gate).
pub const SCHEMA: &str = "ironcondor.bundle.v1";
/// A major-incompatible schema tag (the `bad_schema` defect).
pub const BAD_SCHEMA: &str = "ironcondor.bundle.v2";
/// The opaque producer run identity — used only as a referential key.
pub const RUN_ID: &str = "run-036-conformance";
/// Opening capital in integer cents (the `config.initial_capital` projection).
pub const CAPITAL_CENTS: i64 = 1_000_000;
/// The base event time (ns since the Unix epoch); step `s` carries `BASE_TS + s`.
pub const BASE_TS: i64 = 1_700_000_000_000_000_000;
/// The shared leg's resolved absolute-UTC expiry (ns since epoch).
pub const EXP_NS: i64 = 1_735_286_400_000_000_000;
/// The shared versioned `contract_id` join key (round-trips the columns below).
pub const CID: &str = "v1:BTC:1735286400000000000:6000000:C";
/// The shared underlying (colon-free, grammar-valid).
pub const UNDERLYING: &str = "BTC";
/// The shared strike in integer cents.
pub const STRIKE_CENTS: i64 = 6_000_000;

/// The constant attribution terms; `base_terms = theta + delta + vega + spread −
/// fees = -85`, which the per-step `residual` reconciles against the equity delta.
const THETA: i64 = 40;
const DELTA: i64 = -120;
const VEGA: i64 = 15;
const SPREAD: i64 = 10;
const FEES: i64 = 30;
const BASE_TERMS: i64 = THETA + DELTA + VEGA + SPREAD - FEES;

/// The conformant per-step equity in integer cents (a +1/step ramp).
fn equity_of(step: i64) -> i64 {
    988_500 + step
}

// --- The four typed table batches (conformant) -------------------------------

/// `fills.parquet` — `n` rows, one per step, `position_id == step` so every fill
/// references a real leg. `dangling` optionally rewrites one row's `position_id`
/// to a value with no matching `positions` row (the referential-gap fixture) —
/// which the #32 chain does NOT currently reject (the documented ADR-0011 gap).
pub fn fills_batch(n: usize, dangling: Option<(usize, u64)>) -> RecordBatch {
    let steps: Vec<i32> = (0..n).map(clamp_i32).collect();
    let ts: Vec<i64> = steps.iter().map(|&s| BASE_TS + i64::from(s)).collect();
    let trade: Vec<i64> = steps.iter().map(|&s| 100 + i64::from(s)).collect();
    let mut pos: Vec<i64> = steps.iter().map(|&s| i64::from(s)).collect();
    if let Some((idx, id)) = dangling
        && let Some(slot) = pos.get_mut(idx)
    {
        *slot = i64::try_from(id).unwrap_or(i64::MAX);
    }
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
        Arc::new(StringArray::from(vec![RUN_ID; n])),
        Arc::new(Int64Array::from(trade)),
        Arc::new(Int64Array::from(pos)),
        Arc::new(Int64Array::from(order)),
        Arc::new(Int32Array::from(vec![0_i32; n])),
        Arc::new(StringArray::from(vec![UNDERLYING; n])),
        Arc::new(Int64Array::from(vec![EXP_NS; n])),
        Arc::new(StringArray::from(vec![CID; n])),
        Arc::new(Int64Array::from(vec![STRIKE_CENTS; n])),
        Arc::new(StringArray::from(vec!["call"; n])),
        Arc::new(StringArray::from(vec!["long"; n])),
        Arc::new(Int32Array::from(vec![1_i32; n])),
        Arc::new(Int64Array::from(vec![12_500_i64; n])),
        Arc::new(Int64Array::from(vec![FEES; n])),
        Arc::new(Int64Array::from(vec![-15_i64; n])),
        Arc::new(StringArray::from(vec!["realistic"; n])),
    ];
    build("fills", schema, cols)
}

/// A **decompression-bomb** batch: a single `step: INT64` column of `n` identical
/// values. PLAIN-encoded (no dictionary) its uncompressed footer size is `8 × n`,
/// but the identical bytes crush under ZSTD to near-nothing, so
/// `total_byte_size ≫ file_bytes` clears the expansion ratio with a ~1 KB on-disk
/// file. This table is **never decoded** (the pre-decode bomb check aborts the
/// load), so a single fat degenerate column is deliberate — only its footer row
/// count and byte sizes are read.
pub fn bomb_fills_batch(n: usize) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "step",
        DataType::Int64,
        false,
    )]));
    let cols: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(vec![0_i64; n]))];
    build("fills(bomb)", schema, cols)
}

/// `equity_curve.parquet` — `n` rows; the equity identity holds and `drawdown`
/// (the sole `f64`) is the documented `(−∞, 0]` analytic ratio.
pub fn equity_batch(n: usize) -> RecordBatch {
    let steps: Vec<i32> = (0..n).map(clamp_i32).collect();
    let ts: Vec<i64> = steps.iter().map(|&s| BASE_TS + i64::from(s)).collect();
    let cash: Vec<i64> = steps.iter().map(|&s| 990_000 + i64::from(s)).collect();
    let equity: Vec<i64> = steps.iter().map(|&s| equity_of(i64::from(s))).collect();

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
    build("equity_curve", schema, cols)
}

/// `positions.parquet` — `n` rows, one per step, `position_id == step`. Step 0
/// carries a terminal `exit_reason`; the last step is open at feed exhaustion.
pub fn positions_batch(n: usize) -> RecordBatch {
    let steps: Vec<i32> = (0..n).map(clamp_i32).collect();
    let ts: Vec<i64> = steps.iter().map(|&s| BASE_TS + i64::from(s)).collect();
    let pos: Vec<i64> = steps.iter().map(|&s| i64::from(s)).collect();
    let exit: Vec<Option<&str>> = steps
        .iter()
        .map(|&s| if s == 0 { Some("expiry") } else { None })
        .collect();
    let open_at_end: Vec<bool> = steps
        .iter()
        .map(|&s| usize::try_from(s).unwrap_or(usize::MAX) + 1 == n)
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
        Arc::new(StringArray::from(vec![CID; n])),
        Arc::new(StringArray::from(vec!["short"; n])),
        Arc::new(Int32Array::from(vec![1_i32; n])),
        Arc::new(Int64Array::from(vec![12_000_i64; n])),
        Arc::new(Int64Array::from(vec![11_800_i64; n])),
        Arc::new(Int64Array::from(vec![200_i64; n])),
        Arc::new(BooleanArray::from(vec![false; n])),
        Arc::new(StringArray::from(exit)),
        Arc::new(BooleanArray::from(open_at_end)),
    ];
    build("positions", schema, cols)
}

/// `greeks_attribution.parquet` — `n` rows; `residual` absorbs the per-step
/// remainder so the attribution identity holds against the equity-delta `step_pnl`
/// (step 0 measured against `CAPITAL_CENTS`).
pub fn greeks_batch(n: usize) -> RecordBatch {
    let steps: Vec<i32> = (0..n).map(clamp_i32).collect();
    let ts: Vec<i64> = steps.iter().map(|&s| BASE_TS + i64::from(s)).collect();
    let residual: Vec<i64> = steps
        .iter()
        .map(|&s| {
            let step_pnl = if s == 0 {
                equity_of(0) - CAPITAL_CENTS
            } else {
                equity_of(i64::from(s)) - equity_of(i64::from(s) - 1)
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
        Arc::new(Int64Array::from(vec![THETA; n])),
        Arc::new(Int64Array::from(vec![DELTA; n])),
        Arc::new(Int64Array::from(vec![VEGA; n])),
        Arc::new(Int64Array::from(vec![SPREAD; n])),
        Arc::new(Int64Array::from(vec![FEES; n])),
        Arc::new(Int64Array::from(residual)),
    ];
    build("greeks_attribution", schema, cols)
}

// --- Manifest ----------------------------------------------------------------

/// Render a `manifest.json` with the given schema tag, `run_id`, opening capital,
/// and the four per-table `row_counts`. Every field is present (the reader is
/// permissive toward unknown extras but rejects a missing required field).
pub fn manifest_json(schema: &str, run_id: &str, capital_cents: i64, counts: [u64; 4]) -> String {
    let [f, e, p, g] = counts;
    format!(
        r#"{{
  "schema": "{schema}",
  "run_id": "{run_id}",
  "created_utc": "2026-07-17T00:00:00Z",
  "code_version": "0.3.0",
  "lockfile_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "seed": 36,
  "config": {{ "initial_capital": {capital_cents}, "execution_mode": "realistic" }},
  "strategy": {{ "kind": "iron_condor", "params": {{}} }},
  "data_source": {{ "kind": "simulator", "seed": 36 }},
  "metrics": {{ "total_pnl_cents": -11415, "sharpe": 1.25 }},
  "row_counts": {{ "fills": {f}, "equity_curve": {e}, "positions": {p}, "greeks_attribution": {g} }}
}}
"#
    )
}

// --- Low-level writers -------------------------------------------------------

/// Write a `RecordBatch` to `path` as an uncompressed Parquet file (the conformant
/// on-disk form).
pub fn write_uncompressed(path: &Path, batch: &RecordBatch) {
    write_with_props(path, batch, None);
}

/// Write a `RecordBatch` to `path` as a ZSTD-compressed, **dictionary-disabled**
/// Parquet file — the shape whose uncompressed footer size dwarfs its on-disk size
/// (the decompression-bomb form: PLAIN-encoded repeated values expand under decode
/// but crush under ZSTD, so `total_byte_size ≫ file_bytes`).
pub fn write_zstd_no_dict(path: &Path, batch: &RecordBatch) {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_dictionary_enabled(false)
        .build();
    write_with_props(path, batch, Some(props));
}

fn write_with_props(path: &Path, batch: &RecordBatch, props: Option<WriterProperties>) {
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => panic!("create {}: {e}", path.display()),
    };
    let mut writer = match ArrowWriter::try_new(file, batch.schema(), props) {
        Ok(w) => w,
        Err(e) => panic!("arrow writer for {}: {e}", path.display()),
    };
    if let Err(e) = writer.write(batch) {
        panic!("write {}: {e}", path.display());
    }
    if let Err(e) = writer.close() {
        panic!("close {}: {e}", path.display());
    }
}

fn write_manifest(dir: &Path, json: &str) {
    if let Err(e) = std::fs::write(dir.join("manifest.json"), json) {
        panic!("write manifest into {}: {e}", dir.display());
    }
}

fn build(table: &str, schema: Arc<Schema>, cols: Vec<ArrayRef>) -> RecordBatch {
    match RecordBatch::try_new(schema, cols) {
        Ok(b) => b,
        Err(e) => panic!("build {table} batch: {e}"),
    }
}

fn clamp_i32(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// Create `dir` (idempotently), removing any stale contents first so a regenerated
/// fixture is exactly its four (or three) files.
fn fresh_dir(dir: &Path) {
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        panic!("clear {}: {e}", dir.display());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        panic!("create {}: {e}", dir.display());
    }
}

// --- Documented sizes (referenced by the fixtures test and the bench) --------

/// Rows in the conformant / bad_schema / missing_table / rowcount_lie / truncated
/// / dangling fixtures — tiny, so the committed corpus stays in the low KBs.
pub const VALID_STEPS: usize = 4;
/// Rows in the `oversized_footer` fixture; the test loads it with
/// [`OVERSIZED_MAX_TABLE_ROWS`] (below this), so the footer trips ceiling 2.
pub const OVERSIZED_STEPS: usize = 8;
/// The lowered per-table row ceiling the `oversized_footer` test enforces.
pub const OVERSIZED_MAX_TABLE_ROWS: u64 = 4;
/// The row count the `rowcount_lie` manifest falsely claims for `fills` (the file
/// actually holds [`VALID_STEPS`]); large enough that pre-sizing a `Vec` from it
/// would be a visible over-allocation the reader never performs.
pub const ROWCOUNT_LIE_CLAIMED: u64 = 500;
/// Rows in the `decompression_bomb` fixture's single-column `fills` table — enough
/// repeated, PLAIN-encoded 8-byte values (`8 × BOMB_STEPS ≈ 64 KiB` uncompressed)
/// that the footer clears the default 20× expansion ratio while the ZSTD on-disk
/// file stays ~1 KB.
pub const BOMB_STEPS: usize = 8_000;
/// The dangling `position_id` written into one `fills` row of the referential-gap
/// fixture — no `positions` row carries it.
pub const DANGLING_POSITION_ID: u64 = 9_999;
/// The `fills` row index that carries [`DANGLING_POSITION_ID`].
pub const DANGLING_ROW: usize = 2;

// --- Fixture builders (one targeted defect each) -----------------------------

/// Write the shared conformance bundle — internally consistent and passing the
/// entire #32 validation chain.
pub fn write_valid(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    write_manifest(
        dir,
        &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts(n)),
    );
    write_uncompressed(&dir.join("fills.parquet"), &fills_batch(n, None));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a bundle whose only defect is a major-incompatible `schema` tag.
pub fn write_bad_schema(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    write_manifest(
        dir,
        &manifest_json(BAD_SCHEMA, RUN_ID, CAPITAL_CENTS, counts(n)),
    );
    write_uncompressed(&dir.join("fills.parquet"), &fills_batch(n, None));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a bundle whose only defect is a missing required table
/// (`greeks_attribution.parquet` is absent; the manifest still declares it).
pub fn write_missing_table(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    write_manifest(
        dir,
        &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts(n)),
    );
    write_uncompressed(&dir.join("fills.parquet"), &fills_batch(n, None));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    // greeks_attribution.parquet deliberately omitted.
}

/// Write a bundle whose tables carry more footer rows than a lowered per-table
/// ceiling — the honest manifest agrees with the footers, so the reject is ceiling
/// 2, not a row-count mismatch.
pub fn write_oversized_footer(dir: &Path) {
    fresh_dir(dir);
    let n = OVERSIZED_STEPS;
    write_manifest(
        dir,
        &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts(n)),
    );
    write_uncompressed(&dir.join("fills.parquet"), &fills_batch(n, None));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a bundle whose `manifest.row_counts.fills` lies (claims
/// [`ROWCOUNT_LIE_CLAIMED`]) while `fills.parquet` holds [`VALID_STEPS`].
pub fn write_rowcount_lie(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    let lying = [ROWCOUNT_LIE_CLAIMED, n as u64, n as u64, n as u64];
    write_manifest(dir, &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, lying));
    write_uncompressed(&dir.join("fills.parquet"), &fills_batch(n, None));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a bundle whose `fills.parquet` is cut off mid-file (its Parquet footer is
/// destroyed), so opening the table is a typed decode error.
pub fn write_truncated(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    write_manifest(
        dir,
        &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts(n)),
    );
    let fills = dir.join("fills.parquet");
    write_uncompressed(&fills, &fills_batch(n, None));
    truncate_file(&fills);
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a bundle whose `fills.parquet` is a decompression bomb: many PLAIN-encoded
/// repeated rows whose uncompressed footer size clears the expansion ratio while
/// the ZSTD file stays tiny. The manifest agrees with the (honest) footer row
/// count, so the reject is the pre-decode bomb check, not a mismatch.
pub fn write_decompression_bomb(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    let counts = [BOMB_STEPS as u64, n as u64, n as u64, n as u64];
    write_manifest(dir, &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts));
    write_zstd_no_dict(&dir.join("fills.parquet"), &bomb_fills_batch(BOMB_STEPS));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a bundle that loads clean through the whole chain but carries a fill whose
/// `position_id` references no `positions` row — the documented ADR-0011 referential
/// gap the drill-down must degrade over gracefully.
pub fn write_dangling_position_id(dir: &Path) {
    fresh_dir(dir);
    let n = VALID_STEPS;
    write_manifest(
        dir,
        &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts(n)),
    );
    write_uncompressed(
        &dir.join("fills.parquet"),
        &fills_batch(n, Some((DANGLING_ROW, DANGLING_POSITION_ID))),
    );
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(n));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(n));
    write_uncompressed(&dir.join("greeks_attribution.parquet"), &greeks_batch(n));
}

/// Write a conformant bundle of `steps` steps into `dir` — the HP-4 bench target.
pub fn write_bench_bundle(dir: &Path, steps: usize) {
    fresh_dir(dir);
    write_manifest(
        dir,
        &manifest_json(SCHEMA, RUN_ID, CAPITAL_CENTS, counts(steps)),
    );
    write_uncompressed(&dir.join("fills.parquet"), &fills_batch(steps, None));
    write_uncompressed(&dir.join("equity_curve.parquet"), &equity_batch(steps));
    write_uncompressed(&dir.join("positions.parquet"), &positions_batch(steps));
    write_uncompressed(
        &dir.join("greeks_attribution.parquet"),
        &greeks_batch(steps),
    );
}

/// The four equal per-table row counts for an `n`-step conformant bundle.
fn counts(n: usize) -> [u64; 4] {
    let n = n as u64;
    [n, n, n, n]
}

/// Truncate `path` to a deterministic prefix that destroys the Parquet footer
/// (kept at 55 % of the original length — well before the trailing footer + magic).
fn truncate_file(path: &Path) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => panic!("read {} for truncation: {e}", path.display()),
    };
    let keep = bytes.len().saturating_mul(55) / 100;
    let prefix = bytes.get(..keep).unwrap_or(&bytes);
    if let Err(e) = std::fs::write(path, prefix) {
        panic!("truncate {}: {e}", path.display());
    }
}
