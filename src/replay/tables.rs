//! Typed, per-batch Parquet decoders for the four IronCondor result-bundle
//! tables (`docs/04-replay-mode.md` §2.2/§3/§5).
//!
//! Issue #30 lands the read-only reader spine — the manifest gate, the resource
//! ceilings, and the **measured, batched, cancellable** `RecordBatch` loop. This
//! module (#31) crosses the **wire → domain boundary** for each decoded batch:
//! [`read_fills`], [`read_equity`], [`read_positions`], and [`read_greeks`] map a
//! table's [`RecordBatch`] onto the #29 row types and **append** the rows into the
//! eager `Vec` the working-set budget accounts for, in **file order**. The reader
//! never sorts: each table's stated sort key (`fills` by
//! `(step, order_id, fill_seq)`, `positions` by `(step, position_id)`,
//! `equity_curve` / `greeks_attribution` by `step`) is a WRITER guarantee the #32
//! validation chain verifies (non-decreasing on that key, else `Invariant`);
//! repairing it on load would silently mask the exact writer bug #32 must reject.
//!
//! # Binding rules (`docs/04-replay-mode.md` §5)
//!
//! - **Column projection, permissive to extras.** Each documented column is
//!   looked up **by name**; an **unknown extra column is ignored**, so a newer
//!   minor of the same schema tag still decodes (§3). A **missing required**
//!   column or a **wrong Arrow/Parquet type** is a typed
//!   [`BundleError::Schema`], never a silent read of a partial table.
//! - **Checked wire → domain narrowing (never an `as` cast).** Parquet stores the
//!   domain-unsigned fields (`step`, `fill_seq`, `quantity`, the ids, and the
//!   non-negative cents fields) as signed physical `INT32`/`INT64`. Each crosses
//!   into its `u32`/`u64` reader type via [`u32::try_from`]/[`u64::try_from`], so a
//!   **negative** value is a typed [`BundleError::Invariant`] naming the table +
//!   column + row, and a producer value up to the signed-wire maximum decodes
//!   losslessly. The legitimately-signed fields (`ts_ns`, `expiration_ns`,
//!   `slippage_cents`, `cash_cents`, the attribution terms, …) stay `i64`.
//!   `fees_cents` is the one field IronCondor keeps `i64` while ChainView reads it
//!   `u64` (fees are contractually `>= 0`); the same checked `u64::try_from`
//!   narrows it.
//! - **Money stays integer cents.** Every monetary column decodes into an
//!   `i64`/`u64` field. The **only** `f64` produced here is
//!   [`EquityPoint::drawdown`], read from the `DOUBLE` column with a **non-finite
//!   guard** (`NaN`/`±∞` is a typed error, never a stored `NaN`).
//! - **Nullability per the contract.** `exit_reason` is the one nullable column
//!   (→ `Option<String>`); a NULL in any other (non-nullable) column is a typed
//!   [`BundleError::Invariant`] naming the table + column + row.
//!
//! # Dictionary-encoded strings
//!
//! `docs/04-replay-mode.md` §2.2 marks the string columns `UTF8` and is silent on
//! page encoding, so the reader accepts a string column as either a plain
//! `StringArray` (`Utf8`) **or** a `DictionaryArray<Int32, Utf8>` defensively — a
//! writer that dictionary-encodes `style`/`side`/`mode` (or any UTF8 column) still
//! decodes. The dictionary key is bounds-checked against its value array before
//! the string is read.
//!
//! # Retained-bytes accounting
//!
//! Each decoder RETURNS the exact retained-bytes delta it appended — the owned row
//! structs (`rows * size_of::<RowType>()`) plus every owned `String`'s heap bytes —
//! which the reader's working-set budget (`src/replay/mod.rs`) accounts in ADDITION
//! to the decoded batch's Arrow memory footprint. This closes the dictionary-encoding
//! gap: a `DictionaryArray<Int32, Utf8>` stores its values ONCE while the owned rows
//! copy the string per row, so the Arrow batch size alone under-counts the retained
//! working set by up to an order of magnitude. The tally is all checked arithmetic;
//! an overflow is a typed [`BundleError::TooLarge`], never a wrapping multiply.

use arrow_array::types::Int32Type;
use arrow_array::{
    Array, BooleanArray, DictionaryArray, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray,
};
use optionstratlib::OptionStyle;

use crate::error::BundleError;

use super::{EquityPoint, ExecMode, Fill, GreeksAttribution, PositionRow, PositionSide};

/// Maximum characters retained from an attacker-supplied **cell value** (an
/// unrecognized enum wire string) echoed into a [`BundleError`] message. A valid
/// value is a handful of chars, so a longer one is clamped on a `char` boundary
/// with an ellipsis so the error stays bounded regardless of bundle input
/// (`docs/SECURITY.md` §6). Mirrors `mod.rs`'s schema-tag clamp.
const MAX_ECHO_CHARS: usize = 64;

/// Clamp an attacker-supplied `value` to [`MAX_ECHO_CHARS`] characters, appending
/// a single `…` marker when it was longer. Operates on **`char` boundaries**, so a
/// multi-byte UTF-8 value never panics on a byte split and the result is bounded.
fn clamp_echo(value: &str) -> String {
    if value.chars().count() <= MAX_ECHO_CHARS {
        return value.to_owned();
    }
    let mut clamped: String = value.chars().take(MAX_ECHO_CHARS).collect();
    clamped.push('…');
    clamped
}

// --- Error constructors (cold; bounded, non-secret messages) ----------------

/// A required column is absent from the batch schema.
#[cold]
#[inline(never)]
fn schema_missing(table: &str, column: &str) -> BundleError {
    BundleError::Schema(format!(
        "{table}: required column `{}` is absent",
        clamp_echo(column)
    ))
}

/// A column is present but at the wrong Arrow/Parquet type.
#[cold]
#[inline(never)]
fn schema_type(table: &str, column: &str, expected: &str, found: &dyn Array) -> BundleError {
    // The Arrow `DataType` derives from the UNTRUSTED Parquet footer schema — a
    // deeply-nested type renders long — so route its `{:?}` rendering through the
    // same `clamp_echo` (64 chars + ellipsis) the `BundleError::Schema` doc promises
    // for every echoed dynamic string.
    BundleError::Schema(format!(
        "{table}: column `{}` has wrong type (expected {expected}, found {})",
        clamp_echo(column),
        clamp_echo(&format!("{:?}", found.data_type()))
    ))
}

/// A NULL appeared in a non-nullable column.
#[cold]
#[inline(never)]
fn null_cell(table: &str, column: &str, row: usize) -> BundleError {
    BundleError::Invariant(format!(
        "{table}.{column} row {row}: null in a non-nullable column"
    ))
}

/// A negative value appeared in an unsigned-domain column.
#[cold]
#[inline(never)]
fn negative_cell(table: &str, column: &str, row: usize, value: i64) -> BundleError {
    BundleError::Invariant(format!(
        "{table}.{column} row {row}: negative value {value} in an unsigned-domain column"
    ))
}

/// A non-finite (`NaN`/`±∞`) value appeared in the `drawdown` column.
#[cold]
#[inline(never)]
fn nonfinite_cell(table: &str, column: &str, row: usize) -> BundleError {
    BundleError::Invariant(format!(
        "{table}.{column} row {row}: non-finite value in an analytic-ratio column"
    ))
}

/// An unrecognized enum wire string (its value is clamped).
#[cold]
#[inline(never)]
fn enum_cell(table: &str, column: &str, row: usize, value: &str) -> BundleError {
    BundleError::Invariant(format!(
        "{table}.{column} row {row}: unrecognized value `{}`",
        clamp_echo(value)
    ))
}

/// A dictionary key was negative or out of range for its value array.
#[cold]
#[inline(never)]
fn dict_key_cell(table: &str, column: &str, row: usize) -> BundleError {
    BundleError::Invariant(format!(
        "{table}.{column} row {row}: dictionary key out of range"
    ))
}

/// A decoded batch's retained-bytes tally overflowed `u64` — an absurd batch,
/// rejected as [`BundleError::TooLarge`] rather than a wrapped multiply.
#[cold]
#[inline(never)]
fn retained_overflow(table: &str) -> BundleError {
    BundleError::TooLarge(format!("{table}: retained-bytes tally overflowed u64"))
}

// --- Column lookup + strict downcast by name --------------------------------

/// Look up `name` as an `INT32` column and downcast, else a typed schema error.
fn i32_col<'a>(
    batch: &'a RecordBatch,
    table: &str,
    name: &str,
) -> Result<&'a Int32Array, BundleError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| schema_missing(table, name))?;
    arr.as_any()
        .downcast_ref::<Int32Array>()
        .ok_or_else(|| schema_type(table, name, "INT32", &**arr))
}

/// Look up `name` as an `INT64` column and downcast, else a typed schema error.
fn i64_col<'a>(
    batch: &'a RecordBatch,
    table: &str,
    name: &str,
) -> Result<&'a Int64Array, BundleError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| schema_missing(table, name))?;
    arr.as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| schema_type(table, name, "INT64", &**arr))
}

/// Look up `name` as a `DOUBLE` column and downcast, else a typed schema error.
fn f64_col<'a>(
    batch: &'a RecordBatch,
    table: &str,
    name: &str,
) -> Result<&'a Float64Array, BundleError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| schema_missing(table, name))?;
    arr.as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| schema_type(table, name, "DOUBLE", &**arr))
}

/// Look up `name` as a `BOOLEAN` column and downcast, else a typed schema error.
fn bool_col<'a>(
    batch: &'a RecordBatch,
    table: &str,
    name: &str,
) -> Result<&'a BooleanArray, BundleError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| schema_missing(table, name))?;
    arr.as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| schema_type(table, name, "BOOLEAN", &**arr))
}

/// A `UTF8` column accessor that transparently handles a plain `StringArray` and a
/// `DictionaryArray<Int32, Utf8>` (§2.2 is silent on page encoding, so both are
/// accepted defensively).
enum Utf8Col<'a> {
    /// A plain `Utf8` string array.
    Plain(&'a StringArray),
    /// A `Dictionary(Int32, Utf8)` — the `keys` index into `values`.
    Dict(&'a Int32Array, &'a StringArray),
}

/// Look up `name` as a `UTF8` column (plain or dictionary-encoded), else a typed
/// schema error.
fn utf8_col<'a>(
    batch: &'a RecordBatch,
    table: &str,
    name: &str,
) -> Result<Utf8Col<'a>, BundleError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| schema_missing(table, name))?;
    if let Some(s) = arr.as_any().downcast_ref::<StringArray>() {
        return Ok(Utf8Col::Plain(s));
    }
    if let Some(dict) = arr.as_any().downcast_ref::<DictionaryArray<Int32Type>>() {
        let values = dict
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                schema_type(table, name, "UTF8 (dictionary values)", &**dict.values())
            })?;
        return Ok(Utf8Col::Dict(dict.keys(), values));
    }
    Err(schema_type(table, name, "UTF8", &**arr))
}

impl<'a> Utf8Col<'a> {
    /// The string at `row`, or `None` when the cell is NULL.
    fn opt(&self, table: &str, column: &str, row: usize) -> Result<Option<&'a str>, BundleError> {
        match *self {
            Utf8Col::Plain(s) => {
                if s.is_null(row) {
                    Ok(None)
                } else {
                    Ok(Some(s.value(row)))
                }
            }
            Utf8Col::Dict(keys, values) => {
                if keys.is_null(row) {
                    return Ok(None);
                }
                let key = keys.value(row);
                let idx = usize::try_from(key).map_err(|_| dict_key_cell(table, column, row))?;
                if idx >= values.len() {
                    return Err(dict_key_cell(table, column, row));
                }
                if values.is_null(idx) {
                    Ok(None)
                } else {
                    Ok(Some(values.value(idx)))
                }
            }
        }
    }

    /// The string at `row`; a NULL is a typed error (the column is non-nullable).
    fn required(&self, table: &str, column: &str, row: usize) -> Result<&'a str, BundleError> {
        self.opt(table, column, row)?
            .ok_or_else(|| null_cell(table, column, row))
    }
}

// --- Per-cell accessors (null-checked) --------------------------------------

/// The `i32` at `row`; a NULL is a typed error.
fn i32_at(arr: &Int32Array, table: &str, column: &str, row: usize) -> Result<i32, BundleError> {
    if arr.is_null(row) {
        return Err(null_cell(table, column, row));
    }
    Ok(arr.value(row))
}

/// The `i64` at `row`; a NULL is a typed error.
fn i64_at(arr: &Int64Array, table: &str, column: &str, row: usize) -> Result<i64, BundleError> {
    if arr.is_null(row) {
        return Err(null_cell(table, column, row));
    }
    Ok(arr.value(row))
}

/// The `f64` at `row`; a NULL is a typed error.
fn f64_at(arr: &Float64Array, table: &str, column: &str, row: usize) -> Result<f64, BundleError> {
    if arr.is_null(row) {
        return Err(null_cell(table, column, row));
    }
    Ok(arr.value(row))
}

/// The `bool` at `row`; a NULL is a typed error.
fn bool_at(arr: &BooleanArray, table: &str, column: &str, row: usize) -> Result<bool, BundleError> {
    if arr.is_null(row) {
        return Err(null_cell(table, column, row));
    }
    Ok(arr.value(row))
}

// --- Checked wire → domain narrowing (never an `as` cast) -------------------

/// Narrow a signed-wire `INT32` cell into its `u32` domain, null-checked; a
/// negative value is a typed [`BundleError::Invariant`] naming the column.
fn u32_field(arr: &Int32Array, table: &str, column: &str, row: usize) -> Result<u32, BundleError> {
    let v = i32_at(arr, table, column, row)?;
    u32::try_from(v).map_err(|_| negative_cell(table, column, row, i64::from(v)))
}

/// Narrow a signed-wire `INT64` cell into its `u64` domain, null-checked; a
/// negative value is a typed [`BundleError::Invariant`] naming the column.
fn u64_field(arr: &Int64Array, table: &str, column: &str, row: usize) -> Result<u64, BundleError> {
    let v = i64_at(arr, table, column, row)?;
    u64::try_from(v).map_err(|_| negative_cell(table, column, row, v))
}

/// The `drawdown` ratio at `row`, guarded non-finite; a `NaN`/`±∞` is a typed
/// error, never a stored `NaN`. The `<= 0` range check is #32's job.
fn drawdown_field(
    arr: &Float64Array,
    table: &str,
    column: &str,
    row: usize,
) -> Result<f64, BundleError> {
    let v = f64_at(arr, table, column, row)?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(nonfinite_cell(table, column, row))
    }
}

// --- Enum wire-string decode (unknown value → typed error) ------------------

/// Decode the `call` / `put` wire string into [`OptionStyle`].
fn decode_style(
    table: &str,
    column: &str,
    row: usize,
    wire: &str,
) -> Result<OptionStyle, BundleError> {
    match wire {
        "call" => Ok(OptionStyle::Call),
        "put" => Ok(OptionStyle::Put),
        _ => Err(enum_cell(table, column, row, wire)),
    }
}

/// Decode the `long` / `short` wire string into [`PositionSide`].
fn decode_side(
    table: &str,
    column: &str,
    row: usize,
    wire: &str,
) -> Result<PositionSide, BundleError> {
    match wire {
        "long" => Ok(PositionSide::Long),
        "short" => Ok(PositionSide::Short),
        _ => Err(enum_cell(table, column, row, wire)),
    }
}

/// Decode the `naive` / `realistic` wire string into [`ExecMode`].
fn decode_mode(table: &str, column: &str, row: usize, wire: &str) -> Result<ExecMode, BundleError> {
    match wire {
        "naive" => Ok(ExecMode::Naive),
        "realistic" => Ok(ExecMode::Realistic),
        _ => Err(enum_cell(table, column, row, wire)),
    }
}

// --- Retained-bytes tally (the exact owned-rows delta, checked) -------------

/// Add an owned string's byte length to a running retained-bytes tally, checked;
/// an overflow is [`BundleError::TooLarge`] naming the table (never a wrapping add).
#[inline]
fn add_len(acc: u64, s: &str, table: &str) -> Result<u64, BundleError> {
    let len = u64::try_from(s.len()).map_err(|_| retained_overflow(table))?;
    acc.checked_add(len).ok_or_else(|| retained_overflow(table))
}

/// The exact retained-bytes delta a decoded batch appends to its typed `Vec`:
/// `rows * row_size` (the owned row structs) plus `string_bytes` (the heap bytes of
/// every owned `String` copied out of the batch). All checked (`checked_mul` /
/// `checked_add`); an overflow is [`BundleError::TooLarge`] naming the table, never
/// an `as` cast or a wrapping multiply.
///
/// The reader's working-set budget accounts this in ADDITION to the batch's Arrow
/// memory footprint, so a dictionary-encoded UTF8 column — counted once in the Arrow
/// batch but copied per row into the owned rows — cannot slip past the ceiling.
#[inline]
fn retained_bytes(
    table: &str,
    rows: usize,
    row_size: usize,
    string_bytes: u64,
) -> Result<u64, BundleError> {
    let rows = u64::try_from(rows).map_err(|_| retained_overflow(table))?;
    let row_size = u64::try_from(row_size).map_err(|_| retained_overflow(table))?;
    rows.checked_mul(row_size)
        .and_then(|structs| structs.checked_add(string_bytes))
        .ok_or_else(|| retained_overflow(table))
}

// --- The four table decoders (per batch, append into the eager `Vec`) -------

/// Decode one `fills.parquet` [`RecordBatch`] and **append** its rows to `out`.
///
/// Reads the documented `fills` columns by name (§2.2), narrowing
/// `step`/`fill_seq`/`quantity` via checked `u32::try_from` and
/// `trade_id`/`position_id`/`order_id`/`strike_cents`/`price_cents`/`fees_cents`
/// via checked `u64::try_from`, keeping `ts_ns`/`expiration_ns`/`slippage_cents`
/// signed, and mapping `style`/`side`/`mode` wire strings to their enums. Rows are
/// appended in **file order**; the writer's `(step, order_id, fill_seq)` sort key
/// is a guarantee #32 verifies, never something the reader repairs.
///
/// Returns the exact **retained-bytes delta** appended (the owned `Fill` structs
/// plus every owned `String`'s heap bytes), which the reader's working-set budget
/// accounts separately from the batch's Arrow footprint.
///
/// # Errors
///
/// [`BundleError::Schema`] if a required column is missing or wrong-typed;
/// [`BundleError::Invariant`] naming the table + column + row on a NULL in a
/// non-nullable column, a negative value in an unsigned-domain column, or an
/// unrecognized enum wire string; [`BundleError::TooLarge`] if the retained-bytes
/// tally overflows `u64`.
pub(crate) fn read_fills(batch: &RecordBatch, out: &mut Vec<Fill>) -> Result<u64, BundleError> {
    let table = "fills";
    let step = i32_col(batch, table, "step")?;
    let ts_ns = i64_col(batch, table, "ts_ns")?;
    let strategy_run_id = utf8_col(batch, table, "strategy_run_id")?;
    let trade_id = i64_col(batch, table, "trade_id")?;
    let position_id = i64_col(batch, table, "position_id")?;
    let order_id = i64_col(batch, table, "order_id")?;
    let fill_seq = i32_col(batch, table, "fill_seq")?;
    let underlying = utf8_col(batch, table, "underlying")?;
    let expiration_ns = i64_col(batch, table, "expiration_ns")?;
    let contract_id = utf8_col(batch, table, "contract_id")?;
    let strike_cents = i64_col(batch, table, "strike_cents")?;
    let style = utf8_col(batch, table, "style")?;
    let side = utf8_col(batch, table, "side")?;
    let quantity = i32_col(batch, table, "quantity")?;
    let price_cents = i64_col(batch, table, "price_cents")?;
    let fees_cents = i64_col(batch, table, "fees_cents")?;
    let slippage_cents = i64_col(batch, table, "slippage_cents")?;
    let mode = utf8_col(batch, table, "mode")?;

    let rows = batch.num_rows();
    let start = out.len();
    out.reserve(rows);
    for row in 0..rows {
        out.push(Fill {
            step: u32_field(step, table, "step", row)?,
            ts_ns: i64_at(ts_ns, table, "ts_ns", row)?,
            strategy_run_id: strategy_run_id
                .required(table, "strategy_run_id", row)?
                .to_owned(),
            trade_id: u64_field(trade_id, table, "trade_id", row)?,
            position_id: u64_field(position_id, table, "position_id", row)?,
            order_id: u64_field(order_id, table, "order_id", row)?,
            fill_seq: u32_field(fill_seq, table, "fill_seq", row)?,
            underlying: underlying.required(table, "underlying", row)?.to_owned(),
            expiration_ns: i64_at(expiration_ns, table, "expiration_ns", row)?,
            contract_id: contract_id.required(table, "contract_id", row)?.to_owned(),
            strike_cents: u64_field(strike_cents, table, "strike_cents", row)?,
            style: decode_style(table, "style", row, style.required(table, "style", row)?)?,
            side: decode_side(table, "side", row, side.required(table, "side", row)?)?,
            quantity: u32_field(quantity, table, "quantity", row)?,
            price_cents: u64_field(price_cents, table, "price_cents", row)?,
            fees_cents: u64_field(fees_cents, table, "fees_cents", row)?,
            slippage_cents: i64_at(slippage_cents, table, "slippage_cents", row)?,
            mode: decode_mode(table, "mode", row, mode.required(table, "mode", row)?)?,
        });
    }
    // Measure the EXACT retained delta: the owned row structs plus every owned
    // `String`'s heap bytes (the strings were copied above — the length is free).
    let mut string_bytes: u64 = 0;
    for f in out.iter().skip(start) {
        string_bytes = add_len(string_bytes, &f.strategy_run_id, table)?;
        string_bytes = add_len(string_bytes, &f.underlying, table)?;
        string_bytes = add_len(string_bytes, &f.contract_id, table)?;
    }
    retained_bytes(table, rows, size_of::<Fill>(), string_bytes)
}

/// Decode one `equity_curve.parquet` [`RecordBatch`] and **append** its rows to
/// `out`.
///
/// Narrows `step` via checked `u32::try_from`, keeps
/// `cash_cents`/`position_value_cents`/`equity_cents` signed, and reads the one
/// `f64` — `drawdown` — with a non-finite guard. Rows are appended in **file
/// order**; the writer's `step` sort key is #32-verified, not reader-repaired.
///
/// Returns the exact **retained-bytes delta** appended (the owned `EquityPoint`
/// structs — this table carries no owned `String`s).
///
/// # Errors
///
/// [`BundleError::Schema`] on a missing/wrong-typed column;
/// [`BundleError::Invariant`] on a NULL, a negative `step`, or a non-finite
/// `drawdown`; [`BundleError::TooLarge`] if the retained-bytes tally overflows `u64`.
pub(crate) fn read_equity(
    batch: &RecordBatch,
    out: &mut Vec<EquityPoint>,
) -> Result<u64, BundleError> {
    let table = "equity_curve";
    let step = i32_col(batch, table, "step")?;
    let ts_ns = i64_col(batch, table, "ts_ns")?;
    let cash_cents = i64_col(batch, table, "cash_cents")?;
    let position_value_cents = i64_col(batch, table, "position_value_cents")?;
    let equity_cents = i64_col(batch, table, "equity_cents")?;
    let drawdown = f64_col(batch, table, "drawdown")?;

    let rows = batch.num_rows();
    out.reserve(rows);
    for row in 0..rows {
        out.push(EquityPoint {
            step: u32_field(step, table, "step", row)?,
            ts_ns: i64_at(ts_ns, table, "ts_ns", row)?,
            cash_cents: i64_at(cash_cents, table, "cash_cents", row)?,
            position_value_cents: i64_at(position_value_cents, table, "position_value_cents", row)?,
            equity_cents: i64_at(equity_cents, table, "equity_cents", row)?,
            drawdown: drawdown_field(drawdown, table, "drawdown", row)?,
        });
    }
    // No owned `String`s on this table — the retained delta is the struct floor.
    retained_bytes(table, rows, size_of::<EquityPoint>(), 0)
}

/// Decode one `positions.parquet` [`RecordBatch`] and **append** its rows to
/// `out`.
///
/// Narrows `step`/`quantity` and `avg_price_cents`/`mark_cents` (`u64`) via
/// checked `try_from`, keeps `unrealized_cents` signed, reads the `stale_mark` /
/// `open_at_end` booleans, and reads the **one nullable** column `exit_reason`
/// into `Option<String>` (NULL → `None`). Rows are appended in **file order**; the
/// writer's `(step, position_id)` sort key is #32-verified, not reader-repaired.
///
/// Returns the exact **retained-bytes delta** appended (the owned `PositionRow`
/// structs plus every owned `String`'s heap bytes — `contract_id` and any present
/// `exit_reason`).
///
/// # Errors
///
/// [`BundleError::Schema`] on a missing/wrong-typed column;
/// [`BundleError::Invariant`] on a NULL in a non-nullable column, a negative
/// unsigned-domain value, or an unrecognized `side`; [`BundleError::TooLarge`] if
/// the retained-bytes tally overflows `u64`.
pub(crate) fn read_positions(
    batch: &RecordBatch,
    out: &mut Vec<PositionRow>,
) -> Result<u64, BundleError> {
    let table = "positions";
    let step = i32_col(batch, table, "step")?;
    let ts_ns = i64_col(batch, table, "ts_ns")?;
    let position_id = i64_col(batch, table, "position_id")?;
    let trade_id = i64_col(batch, table, "trade_id")?;
    let contract_id = utf8_col(batch, table, "contract_id")?;
    let side = utf8_col(batch, table, "side")?;
    let quantity = i32_col(batch, table, "quantity")?;
    let avg_price_cents = i64_col(batch, table, "avg_price_cents")?;
    let mark_cents = i64_col(batch, table, "mark_cents")?;
    let unrealized_cents = i64_col(batch, table, "unrealized_cents")?;
    let stale_mark = bool_col(batch, table, "stale_mark")?;
    let exit_reason = utf8_col(batch, table, "exit_reason")?;
    let open_at_end = bool_col(batch, table, "open_at_end")?;

    let rows = batch.num_rows();
    let start = out.len();
    out.reserve(rows);
    for row in 0..rows {
        out.push(PositionRow {
            step: u32_field(step, table, "step", row)?,
            ts_ns: i64_at(ts_ns, table, "ts_ns", row)?,
            position_id: u64_field(position_id, table, "position_id", row)?,
            trade_id: u64_field(trade_id, table, "trade_id", row)?,
            contract_id: contract_id.required(table, "contract_id", row)?.to_owned(),
            side: decode_side(table, "side", row, side.required(table, "side", row)?)?,
            quantity: u32_field(quantity, table, "quantity", row)?,
            avg_price_cents: u64_field(avg_price_cents, table, "avg_price_cents", row)?,
            mark_cents: u64_field(mark_cents, table, "mark_cents", row)?,
            unrealized_cents: i64_at(unrealized_cents, table, "unrealized_cents", row)?,
            stale_mark: bool_at(stale_mark, table, "stale_mark", row)?,
            exit_reason: exit_reason
                .opt(table, "exit_reason", row)?
                .map(str::to_owned),
            open_at_end: bool_at(open_at_end, table, "open_at_end", row)?,
        });
    }
    // Retained delta: the owned row structs plus every owned `String`'s heap bytes
    // (`contract_id` on every row, plus `exit_reason` on the terminal rows).
    let mut string_bytes: u64 = 0;
    for p in out.iter().skip(start) {
        string_bytes = add_len(string_bytes, &p.contract_id, table)?;
        if let Some(reason) = &p.exit_reason {
            string_bytes = add_len(string_bytes, reason, table)?;
        }
    }
    retained_bytes(table, rows, size_of::<PositionRow>(), string_bytes)
}

/// Decode one `greeks_attribution.parquet` [`RecordBatch`] and **append** its rows
/// to `out`.
///
/// Narrows `step` via checked `u32::try_from` and `fees_cents` via checked
/// `u64::try_from`, and keeps the signed attribution terms
/// (`theta`/`delta`/`vega`/`spread_capture`/`residual`) `i64`. Rows are appended in
/// **file order**; the writer's `step` sort key is #32-verified, not
/// reader-repaired.
///
/// Returns the exact **retained-bytes delta** appended (the owned
/// `GreeksAttribution` structs — this table carries no owned `String`s).
///
/// # Errors
///
/// [`BundleError::Schema`] on a missing/wrong-typed column;
/// [`BundleError::Invariant`] on a NULL, a negative `step`, or a negative
/// `fees_cents`; [`BundleError::TooLarge`] if the retained-bytes tally overflows
/// `u64`.
pub(crate) fn read_greeks(
    batch: &RecordBatch,
    out: &mut Vec<GreeksAttribution>,
) -> Result<u64, BundleError> {
    let table = "greeks_attribution";
    let step = i32_col(batch, table, "step")?;
    let ts_ns = i64_col(batch, table, "ts_ns")?;
    let theta_pnl_cents = i64_col(batch, table, "theta_pnl_cents")?;
    let delta_pnl_cents = i64_col(batch, table, "delta_pnl_cents")?;
    let vega_pnl_cents = i64_col(batch, table, "vega_pnl_cents")?;
    let spread_capture_cents = i64_col(batch, table, "spread_capture_cents")?;
    let fees_cents = i64_col(batch, table, "fees_cents")?;
    let residual_cents = i64_col(batch, table, "residual_cents")?;

    let rows = batch.num_rows();
    out.reserve(rows);
    for row in 0..rows {
        out.push(GreeksAttribution {
            step: u32_field(step, table, "step", row)?,
            ts_ns: i64_at(ts_ns, table, "ts_ns", row)?,
            theta_pnl_cents: i64_at(theta_pnl_cents, table, "theta_pnl_cents", row)?,
            delta_pnl_cents: i64_at(delta_pnl_cents, table, "delta_pnl_cents", row)?,
            vega_pnl_cents: i64_at(vega_pnl_cents, table, "vega_pnl_cents", row)?,
            spread_capture_cents: i64_at(spread_capture_cents, table, "spread_capture_cents", row)?,
            fees_cents: u64_field(fees_cents, table, "fees_cents", row)?,
            residual_cents: i64_at(residual_cents, table, "residual_cents", row)?,
        });
    }
    // No owned `String`s on this table — the retained delta is the struct floor.
    retained_bytes(table, rows, size_of::<GreeksAttribution>(), 0)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::sync::Arc;

    use arrow_array::types::Int32Type;
    use arrow_array::{
        Array, ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int32Array, Int64Array,
        RecordBatch, StringArray, StructArray,
    };
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use proptest::prelude::*;

    use super::{read_equity, read_fills, read_greeks, read_positions};
    use crate::error::BundleError;
    use crate::replay::{ExecMode, Fill, PositionSide};
    use optionstratlib::OptionStyle;

    const TS_NS: i64 = 1_700_000_000_000_000_000;
    const EXP_NS: i64 = 1_735_286_400_000_000_000;
    const CID: &str = "v1:BTC:1735286400000000000:6000000:C";
    const RUN: &str = "run-abc123";

    // --- batch-construction helpers (no unwrap/expect/indexing per the ruleset) --

    #[track_caller]
    fn make_batch(fields: Vec<Field>, cols: Vec<ArrayRef>) -> RecordBatch {
        match RecordBatch::try_new(Arc::new(Schema::new(fields)), cols) {
            Ok(b) => b,
            Err(e) => panic!("build test batch: {e}"),
        }
    }

    #[track_caller]
    fn find(fields: &[Field], name: &str) -> usize {
        match fields.iter().position(|f| f.name().as_str() == name) {
            Some(i) => i,
            None => panic!("fixture is missing column `{name}`"),
        }
    }

    /// Replace the column `name`'s field + array in place (for wrong-type / null /
    /// negative / enum injection).
    fn replace(
        fields: &mut [Field],
        cols: &mut [ArrayRef],
        name: &str,
        field: Field,
        arr: ArrayRef,
    ) {
        let idx = find(fields, name);
        if let Some(slot) = fields.get_mut(idx) {
            *slot = field;
        }
        if let Some(slot) = cols.get_mut(idx) {
            *slot = arr;
        }
    }

    /// Drop the column `name` entirely (for the missing-required-column path).
    fn drop_col(fields: &mut Vec<Field>, cols: &mut Vec<ArrayRef>, name: &str) {
        let idx = find(fields, name);
        let _ = fields.remove(idx);
        let _ = cols.remove(idx);
    }

    // --- valid per-table fixtures (n rows, ascending step) -----------------------

    fn fills_parts(n: usize) -> (Vec<Field>, Vec<ArrayRef>) {
        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = (0..n)
            .map(|i| TS_NS + i64::try_from(i).unwrap_or(0))
            .collect();
        let ids: Vec<i64> = (0..n).map(|i| 10 + i64::try_from(i).unwrap_or(0)).collect();
        let order: Vec<i64> = (0..n)
            .map(|i| i64::try_from(i).unwrap_or(i64::MAX))
            .collect();

        let fields = vec![
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
        ];
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(StringArray::from(vec![RUN; n])),
            Arc::new(Int64Array::from(ids.clone())),
            Arc::new(Int64Array::from(ids)),
            Arc::new(Int64Array::from(order)),
            Arc::new(Int32Array::from(vec![0_i32; n])),
            Arc::new(StringArray::from(vec!["BTC"; n])),
            Arc::new(Int64Array::from(vec![EXP_NS; n])),
            Arc::new(StringArray::from(vec![CID; n])),
            Arc::new(Int64Array::from(vec![6_000_000_i64; n])),
            Arc::new(StringArray::from(vec!["call"; n])),
            Arc::new(StringArray::from(vec!["long"; n])),
            Arc::new(Int32Array::from(vec![1_i32; n])),
            Arc::new(Int64Array::from(vec![12_500_i64; n])),
            Arc::new(Int64Array::from(vec![30_i64; n])),
            Arc::new(Int64Array::from(vec![-15_i64; n])),
            Arc::new(StringArray::from(vec!["realistic"; n])),
        ];
        (fields, cols)
    }

    fn equity_parts(n: usize) -> (Vec<Field>, Vec<ArrayRef>) {
        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = (0..n)
            .map(|i| TS_NS + i64::try_from(i).unwrap_or(0))
            .collect();
        let fields = vec![
            Field::new("step", DataType::Int32, false),
            Field::new("ts_ns", DataType::Int64, false),
            Field::new("cash_cents", DataType::Int64, false),
            Field::new("position_value_cents", DataType::Int64, false),
            Field::new("equity_cents", DataType::Int64, false),
            Field::new("drawdown", DataType::Float64, false),
        ];
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(Int64Array::from(vec![1_000_i64; n])),
            Arc::new(Int64Array::from(vec![-100_i64; n])),
            Arc::new(Int64Array::from(vec![900_i64; n])),
            Arc::new(Float64Array::from(vec![-0.01_f64; n])),
        ];
        (fields, cols)
    }

    fn positions_parts(n: usize) -> (Vec<Field>, Vec<ArrayRef>) {
        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = (0..n)
            .map(|i| TS_NS + i64::try_from(i).unwrap_or(0))
            .collect();
        let pos: Vec<i64> = (0..n)
            .map(|i| i64::try_from(i).unwrap_or(i64::MAX))
            .collect();
        // Row 0 is the terminal (closed) leg; the rest are open with no exit reason.
        let exit: Vec<Option<&str>> = (0..n)
            .map(|i| if i == 0 { Some("expiry") } else { None })
            .collect();
        let fields = vec![
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
        ];
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
            Arc::new(BooleanArray::from(vec![false; n])),
        ];
        (fields, cols)
    }

    fn greeks_parts(n: usize) -> (Vec<Field>, Vec<ArrayRef>) {
        let steps: Vec<i32> = (0..n)
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        let ts: Vec<i64> = (0..n)
            .map(|i| TS_NS + i64::try_from(i).unwrap_or(0))
            .collect();
        let fields = vec![
            Field::new("step", DataType::Int32, false),
            Field::new("ts_ns", DataType::Int64, false),
            Field::new("theta_pnl_cents", DataType::Int64, false),
            Field::new("delta_pnl_cents", DataType::Int64, false),
            Field::new("vega_pnl_cents", DataType::Int64, false),
            Field::new("spread_capture_cents", DataType::Int64, false),
            Field::new("fees_cents", DataType::Int64, false),
            Field::new("residual_cents", DataType::Int64, false),
        ];
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int32Array::from(steps)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(Int64Array::from(vec![40_i64; n])),
            Arc::new(Int64Array::from(vec![-120_i64; n])),
            Arc::new(Int64Array::from(vec![15_i64; n])),
            Arc::new(Int64Array::from(vec![10_i64; n])),
            Arc::new(Int64Array::from(vec![30_i64; n])),
            Arc::new(Int64Array::from(vec![1_i64; n])),
        ];
        (fields, cols)
    }

    // --- happy paths -------------------------------------------------------------

    #[test]
    fn test_read_fills_happy_path() {
        let (fields, cols) = fills_parts(2);
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Ok(_) => {}
            Err(e) => panic!("read_fills should succeed: {e}"),
        }
        assert_eq!(out.len(), 2);
        match out.first() {
            Some(f) => {
                assert_eq!(f.step, 0);
                assert_eq!(f.strategy_run_id, RUN);
                assert_eq!(f.trade_id, 10);
                assert_eq!(f.strike_cents, 6_000_000);
                assert_eq!(f.fees_cents, 30);
                assert_eq!(f.slippage_cents, -15);
                assert_eq!(f.style, OptionStyle::Call);
                assert_eq!(f.side, PositionSide::Long);
                assert_eq!(f.mode, ExecMode::Realistic);
            }
            None => panic!("expected a decoded fill"),
        }
    }

    #[test]
    fn test_read_equity_happy_path() {
        let (fields, cols) = equity_parts(2);
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_equity(&batch, &mut out) {
            Ok(_) => {}
            Err(e) => panic!("read_equity should succeed: {e}"),
        }
        assert_eq!(out.len(), 2);
        match out.first() {
            Some(e) => {
                assert_eq!(e.cash_cents, 1_000);
                assert_eq!(e.position_value_cents, -100);
                assert_eq!(e.equity_cents, 900);
                assert!((e.drawdown - (-0.01)).abs() < 1e-12);
            }
            None => panic!("expected a decoded equity point"),
        }
    }

    #[test]
    fn test_read_positions_happy_path_and_exit_reason() {
        let (fields, cols) = positions_parts(2);
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_positions(&batch, &mut out) {
            Ok(_) => {}
            Err(e) => panic!("read_positions should succeed: {e}"),
        }
        assert_eq!(out.len(), 2);
        // Row 0: terminal (Some exit_reason). Row 1: open (None).
        match out.first() {
            Some(p) => {
                assert_eq!(p.side, PositionSide::Short);
                assert_eq!(p.avg_price_cents, 12_000);
                assert_eq!(p.mark_cents, 11_800);
                assert_eq!(p.unrealized_cents, 200);
                assert!(!p.stale_mark);
                assert_eq!(p.exit_reason.as_deref(), Some("expiry"));
            }
            None => panic!("expected a decoded position row"),
        }
        match out.get(1) {
            Some(p) => assert_eq!(p.exit_reason, None),
            None => panic!("expected a second position row"),
        }
    }

    #[test]
    fn test_read_greeks_happy_path() {
        let (fields, cols) = greeks_parts(2);
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_greeks(&batch, &mut out) {
            Ok(_) => {}
            Err(e) => panic!("read_greeks should succeed: {e}"),
        }
        assert_eq!(out.len(), 2);
        match out.first() {
            Some(g) => {
                assert_eq!(g.theta_pnl_cents, 40);
                assert_eq!(g.delta_pnl_cents, -120);
                assert_eq!(g.spread_capture_cents, 10);
                assert_eq!(g.fees_cents, 30);
                assert_eq!(g.residual_cents, 1);
            }
            None => panic!("expected a decoded greeks row"),
        }
    }

    // --- schema mismatches (missing / wrong type) --------------------------------

    #[test]
    fn test_missing_required_column_is_schema_error() {
        let (mut fields, mut cols) = fills_parts(1);
        drop_col(&mut fields, &mut cols, "quantity");
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Err(BundleError::Schema(msg)) => assert!(
                msg.contains("quantity"),
                "the error must name the missing column: {msg}"
            ),
            other => panic!("a missing required column must be Schema, got {other:?}"),
        }
    }

    #[test]
    fn test_wrong_column_type_is_schema_error() {
        let (mut fields, mut cols) = fills_parts(1);
        // `strike_cents` is INT64 in the contract; hand it an INT32 column.
        replace(
            &mut fields,
            &mut cols,
            "strike_cents",
            Field::new("strike_cents", DataType::Int32, false),
            Arc::new(Int32Array::from(vec![1_i32])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Err(BundleError::Schema(msg)) => assert!(
                msg.contains("strike_cents"),
                "the error must name the wrong-typed column: {msg}"
            ),
            other => panic!("a wrong column type must be Schema, got {other:?}"),
        }
    }

    #[test]
    fn test_wrong_type_error_display_is_bounded_for_a_deep_footer_type() {
        // The wrong-type echo renders the UNTRUSTED footer `DataType` via `{:?}`; a
        // deeply-nested type (here a `Struct` with a very long child-field name)
        // renders long. It must be routed through `clamp_echo`, so the error Display
        // stays bounded no matter how baroque the footer schema is.
        let (mut fields, mut cols) = fills_parts(1);
        let child: ArrayRef = Arc::new(Int32Array::from(vec![1_i32]));
        let child_field = Arc::new(Field::new(
            "a_child_field_name_deliberately_long_enough_to_exceed_the_sixty_four_char_clamp",
            DataType::Int32,
            false,
        ));
        let struct_arr = StructArray::from(vec![(child_field, child)]);
        // `strike_cents` is INT64 in the contract; hand it this nested struct column.
        replace(
            &mut fields,
            &mut cols,
            "strike_cents",
            Field::new("strike_cents", struct_arr.data_type().clone(), false),
            Arc::new(struct_arr),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Err(BundleError::Schema(msg)) => {
                assert!(
                    msg.contains("strike_cents"),
                    "the error must name the wrong-typed column: {msg}"
                );
                assert!(
                    msg.contains('…'),
                    "the long footer type must be clamped with an ellipsis: {msg}"
                );
                assert!(
                    msg.chars().count() < 200,
                    "the error Display must stay bounded regardless of the footer type: {msg}"
                );
            }
            other => panic!("a wrong column type must be Schema, got {other:?}"),
        }
    }

    // --- NULL in a non-nullable column ------------------------------------------

    #[test]
    fn test_null_in_non_nullable_column_is_invariant() {
        let (mut fields, mut cols) = fills_parts(1);
        // Mark the field nullable so the batch builds, then feed it a NULL cell.
        replace(
            &mut fields,
            &mut cols,
            "quantity",
            Field::new("quantity", DataType::Int32, true),
            Arc::new(Int32Array::from(vec![None::<i32>])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Err(BundleError::Invariant(msg)) => assert!(
                msg.contains("quantity"),
                "the error must name the null column: {msg}"
            ),
            other => panic!("a NULL in a non-nullable column must be Invariant, got {other:?}"),
        }
    }

    // --- negative in an unsigned-domain field (per field) ------------------------

    #[test]
    fn test_negative_unsigned_field_is_invariant_naming_the_field() {
        let i32_cols = ["step", "fill_seq", "quantity"];
        let i64_cols = [
            "trade_id",
            "position_id",
            "order_id",
            "strike_cents",
            "price_cents",
            "fees_cents",
        ];
        for name in i32_cols {
            let (mut fields, mut cols) = fills_parts(1);
            replace(
                &mut fields,
                &mut cols,
                name,
                Field::new(name, DataType::Int32, false),
                Arc::new(Int32Array::from(vec![-1_i32])),
            );
            let batch = make_batch(fields, cols);
            let mut out = Vec::new();
            match read_fills(&batch, &mut out) {
                Err(BundleError::Invariant(msg)) => assert!(
                    msg.contains(name),
                    "a negative `{name}` must name the field: {msg}"
                ),
                other => panic!("a negative `{name}` must be Invariant, got {other:?}"),
            }
        }
        for name in i64_cols {
            let (mut fields, mut cols) = fills_parts(1);
            replace(
                &mut fields,
                &mut cols,
                name,
                Field::new(name, DataType::Int64, false),
                Arc::new(Int64Array::from(vec![-1_i64])),
            );
            let batch = make_batch(fields, cols);
            let mut out = Vec::new();
            match read_fills(&batch, &mut out) {
                Err(BundleError::Invariant(msg)) => assert!(
                    msg.contains(name),
                    "a negative `{name}` must name the field: {msg}"
                ),
                other => panic!("a negative `{name}` must be Invariant, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_negative_greeks_fees_is_invariant() {
        let (mut fields, mut cols) = greeks_parts(1);
        replace(
            &mut fields,
            &mut cols,
            "fees_cents",
            Field::new("fees_cents", DataType::Int64, false),
            Arc::new(Int64Array::from(vec![-1_i64])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_greeks(&batch, &mut out) {
            Err(BundleError::Invariant(msg)) => assert!(msg.contains("fees_cents"), "{msg}"),
            other => panic!("a negative greeks fees must be Invariant, got {other:?}"),
        }
    }

    // --- signed fields keep their sign ------------------------------------------

    #[test]
    fn test_signed_fields_accept_negative_values() {
        // `slippage_cents` (fills) and the attribution terms are legitimately
        // signed; a negative value must NOT be rejected.
        let (mut fields, mut cols) = fills_parts(1);
        replace(
            &mut fields,
            &mut cols,
            "slippage_cents",
            Field::new("slippage_cents", DataType::Int64, false),
            Arc::new(Int64Array::from(vec![-9_999_i64])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Ok(_) => match out.first() {
                Some(f) => assert_eq!(f.slippage_cents, -9_999),
                None => panic!("expected a decoded fill"),
            },
            Err(e) => panic!("a negative slippage is legitimate signed money: {e}"),
        }
    }

    // --- lossless narrowing of a large unsigned domain value ---------------------

    #[test]
    fn test_large_id_decodes_losslessly() {
        let (mut fields, mut cols) = fills_parts(1);
        replace(
            &mut fields,
            &mut cols,
            "trade_id",
            Field::new("trade_id", DataType::Int64, false),
            Arc::new(Int64Array::from(vec![i64::MAX])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Ok(_) => match out.first() {
                Some(f) => assert_eq!(f.trade_id, 9_223_372_036_854_775_807),
                None => panic!("expected a decoded fill"),
            },
            Err(e) => panic!("a large id at the signed-wire max must decode losslessly: {e}"),
        }
    }

    // --- unknown enum wire string -----------------------------------------------

    #[test]
    fn test_unknown_enum_string_is_invariant_with_clamped_value() {
        let (mut fields, mut cols) = fills_parts(1);
        replace(
            &mut fields,
            &mut cols,
            "side",
            Field::new("side", DataType::Utf8, false),
            Arc::new(StringArray::from(vec!["sideways"])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Err(BundleError::Invariant(msg)) => {
                assert!(msg.contains("side"), "must name the column: {msg}");
                assert!(
                    msg.contains("sideways"),
                    "must echo the offending value: {msg}"
                );
            }
            other => panic!("an unknown enum string must be Invariant, got {other:?}"),
        }
    }

    // --- unknown extra column is ignored (permissive) ---------------------------

    #[test]
    fn test_unknown_extra_column_is_ignored() {
        let (mut fields, mut cols) = fills_parts(1);
        // A newer minor adds a column the reader does not know: it must be ignored.
        fields.push(Field::new("future_col", DataType::Int32, false));
        cols.push(Arc::new(Int32Array::from(vec![123_i32])));
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Ok(_) => match out.first() {
                Some(f) => assert_eq!(f.step, 0),
                None => panic!("expected a decoded fill"),
            },
            Err(e) => panic!("an unknown extra column must be ignored, not rejected: {e}"),
        }
    }

    // --- non-finite drawdown -----------------------------------------------------

    #[test]
    fn test_nan_drawdown_is_invariant() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let (mut fields, mut cols) = equity_parts(1);
            replace(
                &mut fields,
                &mut cols,
                "drawdown",
                Field::new("drawdown", DataType::Float64, false),
                Arc::new(Float64Array::from(vec![bad])),
            );
            let batch = make_batch(fields, cols);
            let mut out = Vec::new();
            match read_equity(&batch, &mut out) {
                Err(BundleError::Invariant(msg)) => {
                    assert!(msg.contains("drawdown"), "must name the column: {msg}");
                }
                other => panic!("a non-finite drawdown must be Invariant, got {other:?}"),
            }
        }
    }

    // --- dictionary-encoded string column ---------------------------------------

    #[test]
    fn test_dictionary_encoded_style_column_decodes() {
        let (mut fields, mut cols) = fills_parts(2);
        // Encode `style` as Dictionary(Int32, Utf8): row 0 -> "call", row 1 -> "put".
        let keys = Int32Array::from(vec![0_i32, 1_i32]);
        let values = Arc::new(StringArray::from(vec!["call", "put"])) as ArrayRef;
        let dict = match DictionaryArray::<Int32Type>::try_new(keys, values) {
            Ok(d) => d,
            Err(e) => panic!("build dictionary array: {e}"),
        };
        replace(
            &mut fields,
            &mut cols,
            "style",
            Field::new(
                "style",
                DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                false,
            ),
            Arc::new(dict),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        match read_fills(&batch, &mut out) {
            Ok(_) => {
                match out.first() {
                    Some(f) => assert_eq!(f.style, OptionStyle::Call),
                    None => panic!("expected a decoded fill"),
                }
                match out.get(1) {
                    Some(f) => assert_eq!(f.style, OptionStyle::Put),
                    None => panic!("expected a second decoded fill"),
                }
            }
            Err(e) => panic!("a dictionary-encoded style column must decode: {e}"),
        }
    }

    // --- retained-bytes tally counts the copied owned Strings -------------------

    #[test]
    fn test_retained_bytes_counts_owned_strings_above_struct_floor() {
        // Give every row a long owned `contract_id` string. The retained tally must
        // count these COPIED bytes, so it exceeds the struct-size floor alone — the
        // per-row string copy the reader's working-set budget must not miss.
        let long = "X".repeat(500);
        let (mut fields, mut cols) = fills_parts(4);
        replace(
            &mut fields,
            &mut cols,
            "contract_id",
            Field::new("contract_id", DataType::Utf8, false),
            Arc::new(StringArray::from(vec![long.as_str(); 4])),
        );
        let batch = make_batch(fields, cols);
        let mut out = Vec::new();
        let retained = match read_fills(&batch, &mut out) {
            Ok(n) => n,
            Err(e) => panic!("read_fills should succeed: {e}"),
        };
        // The struct-size floor alone (owned `Fill` structs, no heap Strings).
        let struct_size = u64::try_from(size_of::<Fill>()).unwrap_or(u64::MAX);
        let floor = struct_size.saturating_mul(4);
        // The exact copied heap bytes: RUN + "BTC" + the 500-char contract_id, per row.
        let per_row = u64::try_from(RUN.len() + "BTC".len() + long.len()).unwrap_or(u64::MAX);
        let expected = floor.saturating_add(per_row.saturating_mul(4));
        assert!(
            retained > floor,
            "retained {retained} must exceed the struct-size floor {floor}: owned Strings are counted"
        );
        assert_eq!(
            retained, expected,
            "retained must count exactly the struct floor plus every copied String's bytes"
        );
    }

    // --- property: parquet round-trip preserves order and signs ------------------

    #[derive(Clone, Debug)]
    struct GenFill {
        step: i32,
        order_id: i64,
        fill_seq: i32,
        strike: i64,
        price: i64,
        fees: i64,
        slippage: i64,
    }

    fn gen_fill() -> impl Strategy<Value = GenFill> {
        (
            0_i32..1_000,
            0_i64..1_000,
            0_i32..5,
            0_i64..10_000_000,
            0_i64..1_000_000,
            0_i64..10_000,
            -100_000_i64..100_000,
        )
            .prop_map(
                |(step, order_id, fill_seq, strike, price, fees, slippage)| GenFill {
                    step,
                    order_id,
                    fill_seq,
                    strike,
                    price,
                    fees,
                    slippage,
                },
            )
    }

    #[track_caller]
    fn to_u32(v: i32) -> u32 {
        match u32::try_from(v) {
            Ok(x) => x,
            Err(_) => panic!("generator produced a negative value"),
        }
    }

    #[track_caller]
    fn to_u64(v: i64) -> u64 {
        match u64::try_from(v) {
            Ok(x) => x,
            Err(_) => panic!("generator produced a negative value"),
        }
    }

    fn fills_batch_from(rows: &[GenFill]) -> RecordBatch {
        let n = rows.len();
        let (mut fields, mut cols) = fills_parts(n);
        // Hold the non-generated fields CONSTANT so `expected_fills` (which uses the
        // same constants) aligns row-for-row after the round-trip; `fills_parts`
        // otherwise varies `ts_ns`/`trade_id`/`position_id` by row index.
        replace(
            &mut fields,
            &mut cols,
            "ts_ns",
            Field::new("ts_ns", DataType::Int64, false),
            Arc::new(Int64Array::from(vec![TS_NS; n])),
        );
        replace(
            &mut fields,
            &mut cols,
            "trade_id",
            Field::new("trade_id", DataType::Int64, false),
            Arc::new(Int64Array::from(vec![10_i64; n])),
        );
        replace(
            &mut fields,
            &mut cols,
            "position_id",
            Field::new("position_id", DataType::Int64, false),
            Arc::new(Int64Array::from(vec![10_i64; n])),
        );
        replace(
            &mut fields,
            &mut cols,
            "step",
            Field::new("step", DataType::Int32, false),
            Arc::new(Int32Array::from(
                rows.iter().map(|r| r.step).collect::<Vec<_>>(),
            )),
        );
        replace(
            &mut fields,
            &mut cols,
            "order_id",
            Field::new("order_id", DataType::Int64, false),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.order_id).collect::<Vec<_>>(),
            )),
        );
        replace(
            &mut fields,
            &mut cols,
            "fill_seq",
            Field::new("fill_seq", DataType::Int32, false),
            Arc::new(Int32Array::from(
                rows.iter().map(|r| r.fill_seq).collect::<Vec<_>>(),
            )),
        );
        replace(
            &mut fields,
            &mut cols,
            "strike_cents",
            Field::new("strike_cents", DataType::Int64, false),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.strike).collect::<Vec<_>>(),
            )),
        );
        replace(
            &mut fields,
            &mut cols,
            "price_cents",
            Field::new("price_cents", DataType::Int64, false),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.price).collect::<Vec<_>>(),
            )),
        );
        replace(
            &mut fields,
            &mut cols,
            "fees_cents",
            Field::new("fees_cents", DataType::Int64, false),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.fees).collect::<Vec<_>>(),
            )),
        );
        replace(
            &mut fields,
            &mut cols,
            "slippage_cents",
            Field::new("slippage_cents", DataType::Int64, false),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.slippage).collect::<Vec<_>>(),
            )),
        );
        make_batch(fields, cols)
    }

    fn expected_fills(rows: &[GenFill]) -> Vec<Fill> {
        rows.iter()
            .map(|r| Fill {
                step: to_u32(r.step),
                ts_ns: TS_NS,
                strategy_run_id: RUN.to_owned(),
                trade_id: 10,
                position_id: 10,
                order_id: to_u64(r.order_id),
                fill_seq: to_u32(r.fill_seq),
                underlying: "BTC".to_owned(),
                expiration_ns: EXP_NS,
                contract_id: CID.to_owned(),
                strike_cents: to_u64(r.strike),
                style: OptionStyle::Call,
                side: PositionSide::Long,
                quantity: 1,
                price_cents: to_u64(r.price),
                fees_cents: to_u64(r.fees),
                slippage_cents: r.slippage,
                mode: ExecMode::Realistic,
            })
            .collect()
    }

    #[track_caller]
    fn roundtrip_fills(batch: &RecordBatch) -> Vec<Fill> {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("create tempdir: {e}"),
        };
        let path = dir.path().join("fills.parquet");
        {
            let file = match File::create(&path) {
                Ok(f) => f,
                Err(e) => panic!("create parquet: {e}"),
            };
            let mut writer = match ArrowWriter::try_new(file, batch.schema(), None) {
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
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => panic!("open parquet: {e}"),
        };
        let reader = match ParquetRecordBatchReaderBuilder::try_new(file) {
            Ok(b) => match b.build() {
                Ok(r) => r,
                Err(e) => panic!("build reader: {e}"),
            },
            Err(e) => panic!("reader builder: {e}"),
        };
        let mut out = Vec::new();
        for batch in reader {
            let batch = match batch {
                Ok(b) => b,
                Err(e) => panic!("read batch: {e}"),
            };
            if let Err(e) = read_fills(&batch, &mut out) {
                panic!("read_fills should succeed on a round-tripped batch: {e}");
            }
        }
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn parquet_roundtrip_preserves_order_and_signs(
            rows in prop::collection::vec(gen_fill(), 1..12)
        ) {
            let batch = fills_batch_from(&rows);
            let mut decoded = roundtrip_fills(&batch);
            decoded.sort_by_key(|f| (f.step, f.order_id, f.fill_seq));

            let mut expected = expected_fills(&rows);
            expected.sort_by_key(|f| (f.step, f.order_id, f.fill_seq));

            prop_assert_eq!(decoded, expected);
        }
    }
}
