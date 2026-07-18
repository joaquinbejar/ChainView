//! The load-time **validation chain** and the cross-repo **equivalence oracle**
//! over a decoded [`LoadedBundle`] (`docs/04-replay-mode.md` §5, §2.3).
//!
//! Issue #30 landed the reader spine + resource ceilings; issue #31 the typed
//! per-column decode into the four `Vec`s in **file order** (the reader never
//! sorts). This module (#32) is the second half of `load()`: once the tables are
//! decoded, [`run_validation_chain`] turns every malformed bundle into a typed
//! [`BundleError`] — never a panic — by running the documented §5 checks **in
//! order** over the loaded tables:
//!
//! 1. §5 step 4 — **ordering / uniqueness**: each table is non-decreasing on its
//!    stated sort key; `fills` is unique on `(step, order_id, fill_seq)`;
//!    `positions` has ≤ 1 row per `(position_id, step)` ([`check_ordering`]).
//! 2. §5 step 5 — the integer **equity identity**
//!    `equity_cents == cash_cents + position_value_cents` ([`check_equity_identity`]).
//! 3. §5 step 6 — the typed [`CapitalConfig`](super::CapitalConfig) projection
//!    (present, integer, `>= 0`) plus the cross-table **attribution identity**
//!    `theta + delta + vega + spread_capture − fees + residual == step_pnl`, with
//!    `step_pnl` the producer's own equity delta (step 0 vs opening capital) —
//!    `|residual|` is **advisory**, never a load failure ([`check_attribution_identity`]).
//! 4. §5 step 7 — the contiguous 0-based **step domain** shared by `equity_curve`
//!    and `greeks_attribution`, every `fills`/`positions` step inside it, and
//!    per-step `ts_ns` equality across all four tables ([`check_step_domain`]).
//! 5. §5 step 8/9 — **referential integrity**: `fills.strategy_run_id ==
//!    manifest.run_id`, the `contract_id` round-trip against each row's structured
//!    columns, stable `position_id → (trade_id, contract_id, side)`, and the
//!    delimiter-safe `UNDERLYING` grammar validated **before** any join-key split
//!    ([`check_referential_integrity`]).
//! 6. §5 step 10 (value domain deferred to #32) — `quantity > 0` and
//!    `drawdown <= 0` ([`check_value_domains`]).
//!
//! The **equivalence oracle** [`compare_bundles`] is the agreement check ChainView
//! shares with IronCondor: money columns compared **exactly** (integer cents), the
//! one analytic float (`drawdown`) under the combined [`ORACLE_ABS_TOL`] /
//! [`ORACLE_REL_TOL`] tolerance, each table in its stated sort-key order (copies
//! sorted **only for comparison** — `load` never sorts), and the manifest as
//! canonical JSON with `created_utc` and `metrics` excluded. It reports the first
//! divergence as a typed [`BundleDivergence`] with table / column / row context.
//!
//! All echoed dynamic strings are length-clamped via
//! [`clamp_echo`](super::clamp_echo); every sum uses `checked_*` and surfaces
//! overflow as [`BundleError::Invariant`]; there are no `as` casts.

use std::cmp::Ordering;
use std::collections::HashMap;

use optionstratlib::OptionStyle;
use serde_json::Value;

use crate::error::BundleError;

use super::{
    BundleManifest, CONTRACT_ID_VERSION_PREFIX, EquityPoint, Fill, GreeksAttribution, LoadedBundle,
    PositionRow, PositionSide, clamp_echo, invariant,
};

// ---------------------------------------------------------------------------
// The validation chain (`docs/04-replay-mode.md` §5, post-decode steps 4–10).
// ---------------------------------------------------------------------------

/// Run the full post-decode validation chain over a materialised bundle, in the
/// documented §5 order (steps 4–10; the schema gate/ceilings/presence/typed
/// decode are #30/#31's steps 1–3 and the `try_from` narrowing of step 10).
///
/// # Errors
///
/// Returns [`BundleError::Invariant`] naming the offending table + row (or field)
/// on the first violated check — an out-of-order or duplicate sort key, a broken
/// equity or attribution identity, a missing/negative `capital_cents`, a
/// step-domain gap/duplicate/`ts_ns` disagreement, a `run_id`/`contract_id`
/// referential failure, an out-of-grammar `underlying`, a zero quantity, or a
/// positive `drawdown`.
pub(super) fn run_validation_chain(loaded: &LoadedBundle) -> Result<(), BundleError> {
    // §5 step 4 — ordering + uniqueness on each stated sort key.
    check_ordering(loaded)?;
    // §5 step 5 — the integer equity identity (tolerance zero).
    check_equity_identity(&loaded.equity)?;
    // §5 step 6 — typed capital projection + the cross-table attribution identity.
    check_attribution_identity(&loaded.manifest, &loaded.equity, &loaded.greeks)?;
    // §5 step 7 — the contiguous step domain + per-step ts_ns equality.
    check_step_domain(
        &loaded.equity,
        &loaded.greeks,
        &loaded.fills,
        &loaded.positions,
    )?;
    // §5 step 8/9 — referential integrity + the delimiter-safe underlying grammar.
    check_referential_integrity(&loaded.manifest, &loaded.fills, &loaded.positions)?;
    // §5 step 10 (value-domain checks deferred to #32) — quantity > 0, drawdown <= 0.
    check_value_domains(&loaded.fills, &loaded.positions, &loaded.equity)?;
    Ok(())
}

// --- §5 step 4: ordering + uniqueness ---------------------------------------

/// §5 step 4 — every table non-decreasing on its stated sort key, with the
/// uniqueness sub-checks (`fills` unique on `(step, order_id, fill_seq)`,
/// `positions` ≤ 1 row per `(position_id, step)`). This is **non-vacuous**: #31
/// preserves file order precisely so a writer that emits out-of-order rows is
/// caught here rather than silently repaired.
fn check_ordering(loaded: &LoadedBundle) -> Result<(), BundleError> {
    check_sorted(
        "fills",
        &loaded.fills,
        |f| (f.step, f.order_id, f.fill_seq),
        true,
    )?;
    check_sorted(
        "positions",
        &loaded.positions,
        |p| (p.step, p.position_id),
        true,
    )?;
    check_sorted("equity_curve", &loaded.equity, |e| e.step, false)?;
    check_sorted("greeks_attribution", &loaded.greeks, |g| g.step, false)?;
    Ok(())
}

/// Verify `rows` is non-decreasing on `key`; when `unique`, an equal adjacent key
/// is a duplicate. Names the table + the first offending row index.
fn check_sorted<T, K, F>(
    table: &'static str,
    rows: &[T],
    key: F,
    unique: bool,
) -> Result<(), BundleError>
where
    F: Fn(&T) -> K,
    K: Ord,
{
    for (i, pair) in rows.windows(2).enumerate() {
        let (Some(prev), Some(cur)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        // Checked, not saturating (a banned method): `i` is a windows(2) index so
        // `i + 1` cannot overflow in practice, but an overflow must be a typed
        // error, never a silently-wrong row number.
        let row = i
            .checked_add(1)
            .ok_or_else(|| invariant(format!("{table}: row index overflowed usize")))?;
        match key(cur).cmp(&key(prev)) {
            Ordering::Less => {
                return Err(invariant(format!(
                    "{table} row {row}: not non-decreasing on its stated sort key"
                )));
            }
            Ordering::Equal if unique => {
                return Err(invariant(format!(
                    "{table} row {row}: duplicate stated sort key (must be unique)"
                )));
            }
            Ordering::Equal | Ordering::Greater => {}
        }
    }
    Ok(())
}

// --- §5 step 5: equity identity ---------------------------------------------

/// §5 step 5 — per row, `equity_cents == cash_cents + position_value_cents` at
/// exact integer cents (tolerance zero). The sum is `checked_add`; an overflow is
/// itself an integrity violation.
fn check_equity_identity(equity: &[EquityPoint]) -> Result<(), BundleError> {
    for (i, e) in equity.iter().enumerate() {
        let sum = e
            .cash_cents
            .checked_add(e.position_value_cents)
            .ok_or_else(|| {
                invariant(format!(
                    "equity_curve row {i}: cash_cents + position_value_cents overflowed i64"
                ))
            })?;
        if sum != e.equity_cents {
            return Err(invariant(format!(
                "equity_curve row {i}: equity_cents {} != cash_cents {} + position_value_cents {}",
                e.equity_cents, e.cash_cents, e.position_value_cents
            )));
        }
    }
    Ok(())
}

// --- §5 step 6: capital projection + attribution identity -------------------

/// §5 step 6 — the typed `config.capital_cents` projection (present, integer,
/// `>= 0`) plus the cross-table attribution identity. For each step,
/// `step_pnl = equity_cents[step] − equity_cents[step−1]` (step 0 vs opening
/// capital) and `theta + delta + vega + spread_capture − fees + residual ==
/// step_pnl` at exact integer cents. `|residual|` is advisory only — it never
/// fails the load.
fn check_attribution_identity(
    manifest: &BundleManifest,
    equity: &[EquityPoint],
    greeks: &[GreeksAttribution],
) -> Result<(), BundleError> {
    let capital = capital_cents(manifest)?;
    if equity.len() != greeks.len() {
        return Err(invariant(format!(
            "attribution identity: equity_curve has {} rows but greeks_attribution has {} — \
             exactly one row per step is required",
            equity.len(),
            greeks.len()
        )));
    }
    // `prev` seeds step 0 against opening capital (`equity_{-1} = capital`), then
    // rolls forward as the beginning-of-step equity for each subsequent step.
    let mut prev = capital;
    for (i, (e, g)) in equity.iter().zip(greeks.iter()).enumerate() {
        if e.step != g.step {
            return Err(invariant(format!(
                "attribution identity row {i}: equity_curve step {} != greeks_attribution step {}",
                e.step, g.step
            )));
        }
        let step_pnl = e.equity_cents.checked_sub(prev).ok_or_else(|| {
            invariant(format!(
                "attribution identity row {i}: equity step delta overflowed i64"
            ))
        })?;
        // `fees_cents` is the one attribution field ChainView reads as `u64`; it is
        // contractually `>= 0`, narrowed back to `i64` here for the signed sum.
        let fees = i64::try_from(g.fees_cents).map_err(|_| {
            invariant(format!(
                "attribution identity row {i}: fees_cents {} exceeds i64",
                g.fees_cents
            ))
        })?;
        let terms = g
            .theta_pnl_cents
            .checked_add(g.delta_pnl_cents)
            .and_then(|x| x.checked_add(g.vega_pnl_cents))
            .and_then(|x| x.checked_add(g.spread_capture_cents))
            .and_then(|x| x.checked_sub(fees))
            .and_then(|x| x.checked_add(g.residual_cents))
            .ok_or_else(|| {
                invariant(format!(
                    "attribution identity row {i}: attribution term sum overflowed i64"
                ))
            })?;
        if terms != step_pnl {
            return Err(invariant(format!(
                "attribution identity row {i} (step {}): terms sum to {terms} but the \
                 equity-delta step_pnl is {step_pnl}",
                e.step
            )));
        }
        prev = e.equity_cents;
    }
    Ok(())
}

/// Project + validate `config.capital_cents` via the typed
/// [`CapitalConfig`](super::CapitalConfig): present, integer, `>= 0`. A missing or
/// non-integer field is [`BundleError::Invariant`] (never a silent `0`); the rest
/// of `config` stays uninterpreted.
fn capital_cents(manifest: &BundleManifest) -> Result<i64, BundleError> {
    let capital = manifest.capital_config().map_err(|e| {
        invariant(format!(
            "config.initial_capital missing or non-integer: {}",
            clamp_echo(&e.to_string())
        ))
    })?;
    // The wire field is unsigned (the IronCondor writer shape, #29), so negative
    // is unrepresentable; the checked narrowing into the signed-cents domain is
    // the only remaining failure (a typed Invariant beyond i64::MAX).
    capital.capital_cents()
}

// --- §5 step 7: contiguous step domain + per-step ts_ns ---------------------

/// §5 step 7 — `equity_curve` and `greeks_attribution` each hold exactly one row
/// per step over a contiguous 0-based domain `0..N` sharing the same `N`; every
/// `fills`/`positions` step is in `0..N`; and all rows sharing one step across all
/// four tables carry the same canonical `ts_ns` (the `equity_curve` row's).
fn check_step_domain(
    equity: &[EquityPoint],
    greeks: &[GreeksAttribution],
    fills: &[Fill],
    positions: &[PositionRow],
) -> Result<(), BundleError> {
    // equity_curve: one row per step, contiguous 0-based (already non-decreasing).
    for (i, e) in equity.iter().enumerate() {
        let expected = u32::try_from(i)
            .map_err(|_| invariant("equity_curve: step domain index exceeds u32".to_owned()))?;
        if e.step != expected {
            return Err(invariant(format!(
                "equity_curve row {i}: step {} breaks the contiguous 0-based domain (expected {expected})",
                e.step
            )));
        }
    }
    // greeks_attribution: same contiguous domain and the same N.
    if greeks.len() != equity.len() {
        return Err(invariant(format!(
            "step domain: equity_curve spans {} steps but greeks_attribution spans {} — \
             the two tables must share N",
            equity.len(),
            greeks.len()
        )));
    }
    for (i, g) in greeks.iter().enumerate() {
        let expected = u32::try_from(i).map_err(|_| {
            invariant("greeks_attribution: step domain index exceeds u32".to_owned())
        })?;
        if g.step != expected {
            return Err(invariant(format!(
                "greeks_attribution row {i}: step {} breaks the contiguous 0-based domain (expected {expected})",
                g.step
            )));
        }
    }
    let n = u32::try_from(equity.len())
        .map_err(|_| invariant("step domain: N exceeds u32".to_owned()))?;

    // Every fills/positions step is inside 0..N and shares the canonical ts_ns.
    for (i, f) in fills.iter().enumerate() {
        let Some(e) = step_row(equity, f.step, n) else {
            return Err(invariant(format!(
                "fills row {i}: step {} is outside the step domain 0..{n}",
                f.step
            )));
        };
        if f.ts_ns != e.ts_ns {
            return Err(invariant(format!(
                "fills row {i}: ts_ns {} disagrees with the canonical step {} ts_ns {}",
                f.ts_ns, f.step, e.ts_ns
            )));
        }
    }
    for (i, p) in positions.iter().enumerate() {
        let Some(e) = step_row(equity, p.step, n) else {
            return Err(invariant(format!(
                "positions row {i}: step {} is outside the step domain 0..{n}",
                p.step
            )));
        };
        if p.ts_ns != e.ts_ns {
            return Err(invariant(format!(
                "positions row {i}: ts_ns {} disagrees with the canonical step {} ts_ns {}",
                p.ts_ns, p.step, e.ts_ns
            )));
        }
    }
    // greeks_attribution row i is step i (verified above); check its ts_ns too.
    for (i, (e, g)) in equity.iter().zip(greeks.iter()).enumerate() {
        if g.ts_ns != e.ts_ns {
            return Err(invariant(format!(
                "greeks_attribution row {i}: ts_ns {} disagrees with the canonical step {} ts_ns {}",
                g.ts_ns, e.step, e.ts_ns
            )));
        }
    }
    Ok(())
}

/// The canonical `equity_curve` row for `step` (its array index equals its step
/// after the contiguity check), or `None` when `step` is outside `0..n`.
fn step_row(equity: &[EquityPoint], step: u32, n: u32) -> Option<&EquityPoint> {
    if step >= n {
        return None;
    }
    let idx = usize::try_from(step).ok()?;
    equity.get(idx)
}

// --- §5 step 8/9: referential integrity + contract_id grammar ---------------

/// §5 step 8/9 — referential integrity across the tables plus the delimiter-safe
/// `UNDERLYING` grammar: every `fills.strategy_run_id == manifest.run_id`, each
/// `contract_id` round-trips against its structured columns, and each
/// `position_id` keeps a stable `(trade_id, contract_id, side)`.
fn check_referential_integrity(
    manifest: &BundleManifest,
    fills: &[Fill],
    positions: &[PositionRow],
) -> Result<(), BundleError> {
    check_strategy_run_id(manifest, fills)?;
    check_fills_contract_ids(fills)?;
    check_positions_contract_ids(positions)?;
    check_position_id_stability(positions)?;
    Ok(())
}

/// Every `fills.strategy_run_id` equals the opaque `manifest.run_id` (§2.1) — the
/// run id is used as a key only, never re-derived.
fn check_strategy_run_id(manifest: &BundleManifest, fills: &[Fill]) -> Result<(), BundleError> {
    for (i, f) in fills.iter().enumerate() {
        if f.strategy_run_id != manifest.run_id {
            return Err(invariant(format!(
                "fills.strategy_run_id row {i}: `{}` != manifest.run_id `{}`",
                clamp_echo(&f.strategy_run_id),
                clamp_echo(&manifest.run_id)
            )));
        }
    }
    Ok(())
}

/// Each `fills` `contract_id` round-trips against the row's own `underlying`,
/// `expiration_ns`, `strike_cents`, and `style` columns (§8). The `underlying`
/// column grammar (§9) is validated **before** the split so a colon-injected
/// underlying can never mis-parse the join key.
fn check_fills_contract_ids(fills: &[Fill]) -> Result<(), BundleError> {
    for (i, f) in fills.iter().enumerate() {
        // §9 — validate the underlying column grammar before any split.
        if !is_valid_underlying(&f.underlying) {
            return Err(invariant(format!(
                "fills.underlying row {i}: `{}` is out of grammar {} (colon-free join key)",
                clamp_echo(&f.underlying),
                super::CONTRACT_ID_UNDERLYING_PATTERN
            )));
        }
        let parsed = parse_contract_id(&f.contract_id).map_err(|reason| {
            invariant(format!(
                "fills.contract_id row {i}: malformed `{}`: {reason}",
                clamp_echo(&f.contract_id)
            ))
        })?;
        if parsed.underlying != f.underlying {
            return Err(round_trip_mismatch("fills", "underlying", i));
        }
        if parsed.expiration_ns != f.expiration_ns {
            return Err(round_trip_mismatch("fills", "expiration_ns", i));
        }
        if parsed.strike_cents != f.strike_cents {
            return Err(round_trip_mismatch("fills", "strike_cents", i));
        }
        if parsed.style != f.style {
            return Err(round_trip_mismatch("fills", "style", i));
        }
    }
    Ok(())
}

/// Each `positions` `contract_id` parses to a well-formed `v1` id. `positions`
/// carries no structured `underlying`/`expiration_ns`/`strike_cents`/`style`
/// columns (§2.2), so the §8 round-trip degenerates here to a well-formedness
/// parse — version prefix, colon-free grammar-valid `UNDERLYING`, numeric fields,
/// and a `C|P` style. The join-key consistency of a `position_id`'s contract is
/// separately guaranteed by [`check_position_id_stability`].
fn check_positions_contract_ids(positions: &[PositionRow]) -> Result<(), BundleError> {
    for (i, p) in positions.iter().enumerate() {
        let _parsed = parse_contract_id(&p.contract_id).map_err(|reason| {
            invariant(format!(
                "positions.contract_id row {i}: malformed `{}`: {reason}",
                clamp_echo(&p.contract_id)
            ))
        })?;
    }
    Ok(())
}

/// Each `position_id` maps to a **stable** `(trade_id, contract_id, side)` over
/// its whole lifetime; a `position_id` that changes any of the three across its
/// rows is [`BundleError::Invariant`].
fn check_position_id_stability(positions: &[PositionRow]) -> Result<(), BundleError> {
    let mut seen: HashMap<u64, (u64, &str, PositionSide)> = HashMap::new();
    for (i, p) in positions.iter().enumerate() {
        let identity = (p.trade_id, p.contract_id.as_str(), p.side);
        match seen.get(&p.position_id) {
            Some(first) if *first != identity => {
                return Err(invariant(format!(
                    "positions row {i}: position_id {} changes its (trade_id, contract_id, side) \
                     identity across rows",
                    p.position_id
                )));
            }
            Some(_) => {}
            None => {
                let _ = seen.insert(p.position_id, identity);
            }
        }
    }
    Ok(())
}

/// A `contract_id` whose parsed field disagrees with the row's own structured
/// column (§8) — names the table + column + row.
fn round_trip_mismatch(table: &str, column: &str, row: usize) -> BundleError {
    invariant(format!(
        "{table}.contract_id row {row}: parsed `{column}` does not round-trip against the \
         row's `{column}` column"
    ))
}

/// The `UNDERLYING` grammar `^[A-Z0-9._]{1,32}$` (upper-case ASCII, digits, `.`,
/// `_`; deliberately colon-free so the join key splits unambiguously).
fn is_valid_underlying(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'.' || b == b'_')
}

/// The five structured fields recovered from a `contract_id` join key.
///
/// `pub(crate)` (with `pub(crate)` fields) so the replay payoff-at-head build
/// (#49, `src/app/replay_payoff_build.rs`) can recover a `positions` leg's
/// `strike_cents` / `style` / `expiration_ns` — which `positions.parquet` carries
/// **only** inside the join key, not as structured columns (`docs/04-replay-mode.md`
/// §2.2). The validation chain (#32) has already proven every `contract_id` parses
/// before any consumer reaches this, so the payoff build's re-parse never fails on a
/// validated bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedContractId {
    pub(crate) underlying: String,
    pub(crate) expiration_ns: i64,
    pub(crate) strike_cents: u64,
    pub(crate) style: OptionStyle,
}

/// Parse a `contract_id` — `"v1:{UNDERLYING}:{expiration_ns}:{strike_cents}:{C|P}"`
/// (`docs/04-replay-mode.md` §2.3) — into its structured fields. The parser lives
/// in #32 per the #29 grammar note.
///
/// Returns a bounded, non-echoing `&'static str` reason on any malformed shape:
/// the wrong colon-delimited field count, an unsupported version prefix, an
/// out-of-grammar `UNDERLYING`, a non-numeric `expiration_ns`/`strike_cents`, or a
/// style char other than `C`/`P`. The caller wraps the reason with the clamped
/// offending id.
pub(crate) fn parse_contract_id(cid: &str) -> Result<ParsedContractId, &'static str> {
    let mut parts = cid.split(':');
    let version = parts.next().ok_or("empty contract_id")?;
    let underlying = parts.next().ok_or("missing UNDERLYING field")?;
    let expiration = parts.next().ok_or("missing expiration_ns field")?;
    let strike = parts.next().ok_or("missing strike_cents field")?;
    let style = parts.next().ok_or("missing style field")?;
    if parts.next().is_some() {
        return Err("too many colon-delimited fields");
    }
    if version != CONTRACT_ID_VERSION_PREFIX {
        return Err("unsupported contract_id version prefix");
    }
    if !is_valid_underlying(underlying) {
        return Err("UNDERLYING out of grammar ^[A-Z0-9._]{1,32}$");
    }
    let expiration_ns: i64 = expiration
        .parse()
        .map_err(|_| "non-numeric expiration_ns")?;
    let strike_cents: u64 = strike.parse().map_err(|_| "non-numeric strike_cents")?;
    let style = match style {
        "C" => OptionStyle::Call,
        "P" => OptionStyle::Put,
        _ => return Err("style field must be C or P"),
    };
    Ok(ParsedContractId {
        underlying: underlying.to_owned(),
        expiration_ns,
        strike_cents,
        style,
    })
}

// --- §5 step 10: value domain (deferred to #32) -----------------------------

/// §5 step 10 (value-domain rules deferred to #32) — `quantity > 0` on `fills`
/// and `positions`, and `drawdown <= 0` per the contract's `(−∞, 0]` sign
/// convention (§2.2). `NaN`/`±∞` `drawdown` is already rejected at decode (#31);
/// a positive value is rejected here.
fn check_value_domains(
    fills: &[Fill],
    positions: &[PositionRow],
    equity: &[EquityPoint],
) -> Result<(), BundleError> {
    for (i, f) in fills.iter().enumerate() {
        if f.quantity == 0 {
            return Err(invariant(format!(
                "fills.quantity row {i}: quantity must be > 0"
            )));
        }
    }
    for (i, p) in positions.iter().enumerate() {
        if p.quantity == 0 {
            return Err(invariant(format!(
                "positions.quantity row {i}: quantity must be > 0"
            )));
        }
    }
    for (i, e) in equity.iter().enumerate() {
        if e.drawdown > 0.0 {
            return Err(invariant(format!(
                "equity_curve.drawdown row {i}: {} is positive; drawdown must be <= 0",
                e.drawdown
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The equivalence oracle (`docs/04-replay-mode.md` §5, cross-repo agreement).
// ---------------------------------------------------------------------------

/// Absolute tolerance for the analytic-float (`drawdown`) comparison in the
/// equivalence oracle. Paired with [`ORACLE_REL_TOL`] as a combined absolute /
/// relative bound. **Must match IronCondor's copy exactly** — the shared
/// near-boundary fixtures produce identical pass/fail on both repositories
/// (`docs/04-replay-mode.md` §5, `docs/TESTING.md` §6).
pub const ORACLE_ABS_TOL: f64 = 1e-9;

/// Relative tolerance for the analytic-float (`drawdown`) comparison in the
/// equivalence oracle. Paired with [`ORACLE_ABS_TOL`]. **Must match IronCondor's
/// copy exactly** (`docs/04-replay-mode.md` §5, `docs/TESTING.md` §6).
pub const ORACLE_REL_TOL: f64 = 1e-6;

/// The first point at which two bundles diverge under the equivalence oracle — a
/// typed report (not a bool) carrying the table, column, and row context of the
/// divergence, with a bounded `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleDivergence {
    /// The table (`"fills"` / `"equity_curve"` / `"positions"` /
    /// `"greeks_attribution"`) or `"manifest"` where the two bundles first differ.
    pub table: &'static str,
    /// The column (or `"row_count"` / `"canonical_json"`) that diverged.
    pub column: &'static str,
    /// The row index of the divergence, or `None` for a table-level (row-count) or
    /// manifest-level difference.
    pub row: Option<usize>,
    /// A bounded, non-secret description of the divergence.
    pub detail: String,
}

/// Compare two decoded bundles under the IronCondor equivalence oracle
/// (`docs/04-replay-mode.md` §5).
///
/// Money columns compare **exactly** (integer cents); the one analytic float,
/// `drawdown`, compares under `|a − b| ≤ max(ABS_TOL, REL_TOL × max(|a|, |b|))`
/// with signed zero equal, `NaN` never equal, and `±∞` equal only to the same
/// infinity. Each table is compared in its stated sort-key order — the oracle
/// sorts **copies** only, so a differently-ordered-but-equivalent pair still
/// matches (`load` itself never sorts). The manifest compares as canonical JSON
/// with `created_utc` and the opaque `metrics` excluded.
///
/// Returns `Ok(())` when the bundles are equivalent, or the first
/// [`BundleDivergence`] otherwise. The `Result` return makes the outcome
/// use-mandatory.
pub fn compare_bundles(a: &LoadedBundle, b: &LoadedBundle) -> Result<(), BundleDivergence> {
    compare_fills(&a.fills, &b.fills)?;
    compare_equity(&a.equity, &b.equity)?;
    compare_positions(&a.positions, &b.positions)?;
    compare_greeks(&a.greeks, &b.greeks)?;
    compare_manifest(&a.manifest, &b.manifest)?;
    Ok(())
}

/// A table-level row-count divergence.
fn count_divergence(table: &'static str, la: usize, lb: usize) -> BundleDivergence {
    BundleDivergence {
        table,
        column: "row_count",
        row: None,
        detail: format!("row counts differ: {la} vs {lb}"),
    }
}

/// A field-level divergence at `row`.
fn field_divergence(table: &'static str, column: &'static str, row: usize) -> BundleDivergence {
    BundleDivergence {
        table,
        column,
        row: Some(row),
        detail: format!("{table}.{column} differs between the two bundles at row {row}"),
    }
}

fn compare_fills(a: &[Fill], b: &[Fill]) -> Result<(), BundleDivergence> {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    let key = |f: &Fill| (f.step, f.order_id, f.fill_seq);
    a.sort_by_key(key);
    b.sort_by_key(key);
    if a.len() != b.len() {
        return Err(count_divergence("fills", a.len(), b.len()));
    }
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if let Some(column) = fill_diff(x, y) {
            return Err(field_divergence("fills", column, i));
        }
    }
    Ok(())
}

/// The first differing `fills` column (all exact — no float), or `None`.
fn fill_diff(x: &Fill, y: &Fill) -> Option<&'static str> {
    if x.step != y.step {
        return Some("step");
    }
    if x.ts_ns != y.ts_ns {
        return Some("ts_ns");
    }
    if x.strategy_run_id != y.strategy_run_id {
        return Some("strategy_run_id");
    }
    if x.trade_id != y.trade_id {
        return Some("trade_id");
    }
    if x.position_id != y.position_id {
        return Some("position_id");
    }
    if x.order_id != y.order_id {
        return Some("order_id");
    }
    if x.fill_seq != y.fill_seq {
        return Some("fill_seq");
    }
    if x.underlying != y.underlying {
        return Some("underlying");
    }
    if x.expiration_ns != y.expiration_ns {
        return Some("expiration_ns");
    }
    if x.contract_id != y.contract_id {
        return Some("contract_id");
    }
    if x.strike_cents != y.strike_cents {
        return Some("strike_cents");
    }
    if x.style != y.style {
        return Some("style");
    }
    if x.side != y.side {
        return Some("side");
    }
    if x.quantity != y.quantity {
        return Some("quantity");
    }
    if x.price_cents != y.price_cents {
        return Some("price_cents");
    }
    if x.fees_cents != y.fees_cents {
        return Some("fees_cents");
    }
    if x.slippage_cents != y.slippage_cents {
        return Some("slippage_cents");
    }
    if x.mode != y.mode {
        return Some("mode");
    }
    None
}

fn compare_equity(a: &[EquityPoint], b: &[EquityPoint]) -> Result<(), BundleDivergence> {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort_by_key(|e| e.step);
    b.sort_by_key(|e| e.step);
    if a.len() != b.len() {
        return Err(count_divergence("equity_curve", a.len(), b.len()));
    }
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x.step != y.step {
            return Err(field_divergence("equity_curve", "step", i));
        }
        if x.ts_ns != y.ts_ns {
            return Err(field_divergence("equity_curve", "ts_ns", i));
        }
        if x.cash_cents != y.cash_cents {
            return Err(field_divergence("equity_curve", "cash_cents", i));
        }
        if x.position_value_cents != y.position_value_cents {
            return Err(field_divergence("equity_curve", "position_value_cents", i));
        }
        if x.equity_cents != y.equity_cents {
            return Err(field_divergence("equity_curve", "equity_cents", i));
        }
        if !drawdown_equivalent(x.drawdown, y.drawdown) {
            return Err(BundleDivergence {
                table: "equity_curve",
                column: "drawdown",
                row: Some(i),
                detail: format!(
                    "drawdown {} vs {} exceeds the oracle tolerance",
                    x.drawdown, y.drawdown
                ),
            });
        }
    }
    Ok(())
}

fn compare_positions(a: &[PositionRow], b: &[PositionRow]) -> Result<(), BundleDivergence> {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    let key = |p: &PositionRow| (p.step, p.position_id);
    a.sort_by_key(key);
    b.sort_by_key(key);
    if a.len() != b.len() {
        return Err(count_divergence("positions", a.len(), b.len()));
    }
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if let Some(column) = position_diff(x, y) {
            return Err(field_divergence("positions", column, i));
        }
    }
    Ok(())
}

/// The first differing `positions` column (all exact — no float), or `None`.
fn position_diff(x: &PositionRow, y: &PositionRow) -> Option<&'static str> {
    if x.step != y.step {
        return Some("step");
    }
    if x.ts_ns != y.ts_ns {
        return Some("ts_ns");
    }
    if x.position_id != y.position_id {
        return Some("position_id");
    }
    if x.trade_id != y.trade_id {
        return Some("trade_id");
    }
    if x.contract_id != y.contract_id {
        return Some("contract_id");
    }
    if x.side != y.side {
        return Some("side");
    }
    if x.quantity != y.quantity {
        return Some("quantity");
    }
    if x.avg_price_cents != y.avg_price_cents {
        return Some("avg_price_cents");
    }
    if x.mark_cents != y.mark_cents {
        return Some("mark_cents");
    }
    if x.unrealized_cents != y.unrealized_cents {
        return Some("unrealized_cents");
    }
    if x.stale_mark != y.stale_mark {
        return Some("stale_mark");
    }
    if x.exit_reason != y.exit_reason {
        return Some("exit_reason");
    }
    if x.open_at_end != y.open_at_end {
        return Some("open_at_end");
    }
    None
}

fn compare_greeks(
    a: &[GreeksAttribution],
    b: &[GreeksAttribution],
) -> Result<(), BundleDivergence> {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort_by_key(|g| g.step);
    b.sort_by_key(|g| g.step);
    if a.len() != b.len() {
        return Err(count_divergence("greeks_attribution", a.len(), b.len()));
    }
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if let Some(column) = greeks_diff(x, y) {
            return Err(field_divergence("greeks_attribution", column, i));
        }
    }
    Ok(())
}

/// The first differing `greeks_attribution` column (all exact — no float), or
/// `None`.
fn greeks_diff(x: &GreeksAttribution, y: &GreeksAttribution) -> Option<&'static str> {
    if x.step != y.step {
        return Some("step");
    }
    if x.ts_ns != y.ts_ns {
        return Some("ts_ns");
    }
    if x.theta_pnl_cents != y.theta_pnl_cents {
        return Some("theta_pnl_cents");
    }
    if x.delta_pnl_cents != y.delta_pnl_cents {
        return Some("delta_pnl_cents");
    }
    if x.vega_pnl_cents != y.vega_pnl_cents {
        return Some("vega_pnl_cents");
    }
    if x.spread_capture_cents != y.spread_capture_cents {
        return Some("spread_capture_cents");
    }
    if x.fees_cents != y.fees_cents {
        return Some("fees_cents");
    }
    if x.residual_cents != y.residual_cents {
        return Some("residual_cents");
    }
    None
}

/// Compare two manifests as canonical JSON with `created_utc` (provenance-only)
/// and the opaque `metrics` excluded. `serde_json::Value` equality is
/// key-order-independent, so this is the canonical (sorted-key) comparison.
fn compare_manifest(a: &BundleManifest, b: &BundleManifest) -> Result<(), BundleDivergence> {
    let ca = canonical_manifest(a)?;
    let cb = canonical_manifest(b)?;
    if ca != cb {
        return Err(BundleDivergence {
            table: "manifest",
            column: "canonical_json",
            row: None,
            detail: "manifests differ after excluding created_utc and metrics".to_owned(),
        });
    }
    Ok(())
}

/// Project a manifest to its canonical comparison `Value` with `created_utc` and
/// `metrics` removed.
fn canonical_manifest(m: &BundleManifest) -> Result<Value, BundleDivergence> {
    let mut value = serde_json::to_value(m).map_err(|e| BundleDivergence {
        table: "manifest",
        column: "serialize",
        row: None,
        detail: clamp_echo(&e.to_string()),
    })?;
    if let Value::Object(map) = &mut value {
        let _ = map.remove("created_utc");
        let _ = map.remove("metrics");
    }
    Ok(value)
}

/// The analytic-float (`drawdown`) equivalence rule (`docs/04-replay-mode.md` §5):
/// `NaN` never equal, `±∞` equal only to the same infinity, signed zero equal,
/// and otherwise `|a − b| ≤ max(ABS_TOL, REL_TOL × max(|a|, |b|))`. Symmetric in
/// `a`/`b`.
fn drawdown_equivalent(a: f64, b: f64) -> bool {
    if a.is_nan() || b.is_nan() {
        return false;
    }
    if a.is_infinite() || b.is_infinite() {
        // Equal only when both are infinite with the same sign.
        return a.is_infinite() && b.is_infinite() && a.is_sign_positive() == b.is_sign_positive();
    }
    let diff = (a - b).abs();
    let tol = ORACLE_ABS_TOL.max(ORACLE_REL_TOL * a.abs().max(b.abs()));
    diff <= tol
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::replay::{ExecMode, SUPPORTED_SCHEMA};

    // --- conformant in-memory bundle (no Parquet) --------------------------------

    const BASE_TS: i64 = 1_700_000_000_000_000_000;
    const CAPITAL: i64 = 1_000_000;
    const CID: &str = "v1:BTC:1735286400000000000:6000000:C";
    const EXP_NS: i64 = 1_735_286_400_000_000_000;
    const RUN: &str = "run-xyz";

    fn ts(step: u32) -> i64 {
        BASE_TS + i64::from(step)
    }

    fn equity_cents_at(step: u32) -> i64 {
        988_500 + i64::from(step)
    }

    fn valid_manifest(capital: i64) -> BundleManifest {
        let mut row_counts = BTreeMap::new();
        let _ = row_counts.insert("fills".to_owned(), 0);
        let _ = row_counts.insert("equity_curve".to_owned(), 0);
        let _ = row_counts.insert("positions".to_owned(), 0);
        let _ = row_counts.insert("greeks_attribution".to_owned(), 0);
        BundleManifest {
            schema: SUPPORTED_SCHEMA.to_owned(),
            run_id: RUN.to_owned(),
            created_utc: "2026-07-16T00:00:00Z".to_owned(),
            code_version: "0.3.0".to_owned(),
            lockfile_sha256: "deadbeef".to_owned(),
            seed: 7,
            config: serde_json::json!({ "initial_capital": capital, "mode": "realistic" }),
            strategy: serde_json::json!({ "kind": "iron_condor" }),
            data_source: serde_json::json!({ "kind": "simulator" }),
            metrics: serde_json::json!({ "sharpe": 1.25 }),
            row_counts,
        }
    }

    /// A conformant bundle of `n` steps that passes the entire validation chain.
    fn valid_bundle(n: u32) -> LoadedBundle {
        let base_terms: i64 = 40 - 120 + 15 + 10 - 30; // theta+delta+vega+spread-fees

        let fills = (0..n)
            .map(|s| Fill {
                step: s,
                ts_ns: ts(s),
                strategy_run_id: RUN.to_owned(),
                trade_id: 100 + u64::from(s),
                position_id: u64::from(s),
                order_id: u64::from(s),
                fill_seq: 0,
                underlying: "BTC".to_owned(),
                expiration_ns: EXP_NS,
                contract_id: CID.to_owned(),
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

        let equity = (0..n)
            .map(|s| EquityPoint {
                step: s,
                ts_ns: ts(s),
                cash_cents: 990_000 + i64::from(s),
                position_value_cents: -1_500,
                equity_cents: equity_cents_at(s),
                drawdown: -0.01,
            })
            .collect();

        let positions = (0..n)
            .map(|s| PositionRow {
                step: s,
                ts_ns: ts(s),
                position_id: u64::from(s),
                trade_id: 7,
                contract_id: CID.to_owned(),
                side: PositionSide::Short,
                quantity: 1,
                avg_price_cents: 12_000,
                mark_cents: 11_800,
                unrealized_cents: 200,
                stale_mark: false,
                exit_reason: None,
                open_at_end: s + 1 == n,
            })
            .collect();

        let greeks = (0..n)
            .map(|s| {
                let step_pnl = if s == 0 {
                    equity_cents_at(0) - CAPITAL
                } else {
                    equity_cents_at(s) - equity_cents_at(s - 1)
                };
                GreeksAttribution {
                    step: s,
                    ts_ns: ts(s),
                    theta_pnl_cents: 40,
                    delta_pnl_cents: -120,
                    vega_pnl_cents: 15,
                    spread_capture_cents: 10,
                    fees_cents: 30,
                    residual_cents: step_pnl - base_terms,
                }
            })
            .collect();

        LoadedBundle {
            manifest: valid_manifest(CAPITAL),
            fills,
            equity,
            positions,
            greeks,
        }
    }

    #[track_caller]
    fn expect_invariant(result: Result<(), BundleError>, needle: &str) {
        match result {
            Err(BundleError::Invariant(msg)) => assert!(
                msg.contains(needle),
                "the Invariant message must mention `{needle}`: {msg}"
            ),
            other => panic!("expected Invariant mentioning `{needle}`, got {other:?}"),
        }
    }

    // --- the conformant bundle passes the whole chain ----------------------------

    #[test]
    fn test_conformant_bundle_passes_the_chain() {
        for n in 1..8_u32 {
            match run_validation_chain(&valid_bundle(n)) {
                Ok(()) => {}
                Err(e) => panic!("a conformant {n}-step bundle must pass the chain: {e}"),
            }
        }
    }

    // --- §5 step 4: ordering + uniqueness ----------------------------------------

    #[test]
    fn test_out_of_order_fills_is_invariant() {
        let mut b = valid_bundle(3);
        b.fills.swap(0, 1);
        expect_invariant(run_validation_chain(&b), "fills row 1");
    }

    #[test]
    fn test_out_of_order_equity_is_invariant() {
        let mut b = valid_bundle(3);
        b.equity.swap(0, 1);
        expect_invariant(run_validation_chain(&b), "equity_curve row 1");
    }

    #[test]
    fn test_out_of_order_positions_is_invariant() {
        let mut b = valid_bundle(3);
        b.positions.swap(0, 1);
        expect_invariant(run_validation_chain(&b), "positions row 1");
    }

    #[test]
    fn test_out_of_order_greeks_is_invariant() {
        let mut b = valid_bundle(3);
        b.greeks.swap(0, 1);
        expect_invariant(run_validation_chain(&b), "greeks_attribution row 1");
    }

    #[test]
    fn test_duplicate_fill_key_is_invariant() {
        let mut b = valid_bundle(3);
        // Make row 2 duplicate row 1's (step, order_id, fill_seq) key.
        if let Some(f) = b.fills.get_mut(2) {
            f.step = 1;
            f.order_id = 1;
            f.fill_seq = 0;
        }
        expect_invariant(run_validation_chain(&b), "fills row 2");
    }

    #[test]
    fn test_duplicate_position_step_key_is_invariant() {
        let mut b = valid_bundle(3);
        // Two rows sharing the same (position_id, step) is > 1 row per key.
        if let Some(p) = b.positions.get_mut(2) {
            p.step = 1;
            p.position_id = 1;
        }
        expect_invariant(run_validation_chain(&b), "positions row 2");
    }

    // --- §5 step 5: equity identity ----------------------------------------------

    #[test]
    fn test_broken_equity_identity_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(e) = b.equity.get_mut(1) {
            e.equity_cents += 1; // no longer cash + position_value
        }
        expect_invariant(run_validation_chain(&b), "equity_curve row 1");
    }

    // --- §5 step 6: capital + attribution identity -------------------------------

    #[test]
    fn test_one_cent_attribution_perturbation_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(g) = b.greeks.get_mut(1) {
            g.theta_pnl_cents += 1; // skew a single term by one cent
        }
        expect_invariant(run_validation_chain(&b), "attribution identity row 1");
    }

    #[test]
    fn test_skewed_equity_delta_is_invariant() {
        let mut b = valid_bundle(3);
        // Keep the equity identity intact (bump cash + equity together) but skew the
        // step-over-step equity delta the attribution terms reconcile against.
        if let Some(e) = b.equity.get_mut(1) {
            e.equity_cents += 100;
            e.cash_cents += 100;
        }
        expect_invariant(run_validation_chain(&b), "attribution identity row 1");
    }

    #[test]
    fn test_large_residual_that_satisfies_identity_loads() {
        // A large |residual| that STILL satisfies the identity is not a load
        // failure — the residual drives an advisory badge, not a BundleError.
        let mut b = valid_bundle(3);
        if let Some(g) = b.greeks.get_mut(1) {
            // step_pnl at step 1 is 1; keep terms - fees + residual == 1 with a big residual.
            g.theta_pnl_cents = -999_999;
            g.delta_pnl_cents = 0;
            g.vega_pnl_cents = 0;
            g.spread_capture_cents = 0;
            g.fees_cents = 0;
            g.residual_cents = 1_000_000;
        }
        match run_validation_chain(&b) {
            Ok(()) => {}
            Err(e) => panic!("a large-but-identity-satisfying residual must load: {e}"),
        }
    }

    #[test]
    fn test_missing_initial_capital_is_invariant() {
        let mut b = valid_bundle(3);
        b.manifest.config = serde_json::json!({ "mode": "realistic" });
        expect_invariant(run_validation_chain(&b), "initial_capital");
    }

    #[test]
    fn test_non_integer_initial_capital_is_invariant() {
        let mut b = valid_bundle(3);
        b.manifest.config = serde_json::json!({ "initial_capital": "lots" });
        expect_invariant(run_validation_chain(&b), "initial_capital");
    }

    #[test]
    fn test_negative_initial_capital_is_invariant() {
        // The wire field is unsigned (#29), so a negative value fails DESERIALIZE
        // by type - surfaced as the same typed missing-or-non-integer Invariant.
        let mut b = valid_bundle(3);
        b.manifest.config = serde_json::json!({ "initial_capital": -5 });
        expect_invariant(run_validation_chain(&b), "initial_capital");
    }

    // --- §5 step 7: step-domain invariants ---------------------------------------

    #[test]
    fn test_step_domain_gap_is_invariant() {
        let mut b = valid_bundle(3);
        // Break contiguity while keeping ordering/attribution valid (step field only).
        if let Some(e) = b.equity.get_mut(2) {
            e.step = 3;
        }
        if let Some(g) = b.greeks.get_mut(2) {
            g.step = 3;
        }
        expect_invariant(run_validation_chain(&b), "equity_curve row 2");
    }

    #[test]
    fn test_mismatched_equity_greeks_span_is_invariant() {
        let mut b = valid_bundle(3);
        // Drop greeks' last row so equity spans 3 steps but greeks spans 2. The
        // equal-length precondition of the attribution identity (§5 step 6) catches
        // this before the step-domain check (§5 step 7) — both require one greeks
        // row per equity step, so the mismatch is a typed Invariant either way.
        let _ = b.greeks.pop();
        expect_invariant(run_validation_chain(&b), "one row per step");
    }

    #[test]
    fn test_step_domain_n_guard_rejects_mismatched_span_directly() {
        // Exercise the step-domain check in isolation (§5 step 7) so its own
        // shared-N guard is covered, not only the attribution precondition.
        let b = valid_bundle(3);
        let mut greeks = b.greeks.clone();
        let _ = greeks.pop();
        expect_invariant(
            check_step_domain(&b.equity, &greeks, &b.fills, &b.positions),
            "share N",
        );
    }

    #[test]
    fn test_per_step_ts_ns_disagreement_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(f) = b.fills.get_mut(1) {
            f.ts_ns += 5; // disagrees with the canonical step ts_ns
        }
        expect_invariant(run_validation_chain(&b), "fills row 1");
    }

    // --- §5 step 8/9: referential integrity + grammar ----------------------------

    #[test]
    fn test_run_id_mismatch_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(f) = b.fills.get_mut(1) {
            f.strategy_run_id = "other-run".to_owned();
        }
        expect_invariant(run_validation_chain(&b), "strategy_run_id");
    }

    #[test]
    fn test_contract_id_not_round_tripping_is_invariant() {
        let mut b = valid_bundle(3);
        // A strike segment that disagrees with the row's strike_cents column.
        if let Some(f) = b.fills.get_mut(1) {
            f.contract_id = "v1:BTC:1735286400000000000:9999999:C".to_owned();
        }
        expect_invariant(run_validation_chain(&b), "strike_cents");
    }

    #[test]
    fn test_colon_bearing_underlying_is_invariant_before_join() {
        let mut b = valid_bundle(3);
        if let Some(f) = b.fills.get_mut(1) {
            f.underlying = "BT:C".to_owned();
        }
        expect_invariant(run_validation_chain(&b), "out of grammar");
    }

    #[test]
    fn test_unstable_position_id_is_invariant() {
        let mut b = valid_bundle(3);
        // position_id 1 reused with a different trade_id → unstable identity.
        if let Some(p) = b.positions.get_mut(2) {
            p.position_id = 1;
            p.trade_id = 999;
        }
        expect_invariant(run_validation_chain(&b), "position_id 1 changes");
    }

    // --- §5 step 10: value domain ------------------------------------------------

    #[test]
    fn test_zero_quantity_fill_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(f) = b.fills.get_mut(1) {
            f.quantity = 0;
        }
        expect_invariant(run_validation_chain(&b), "fills.quantity row 1");
    }

    #[test]
    fn test_zero_quantity_position_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(p) = b.positions.get_mut(1) {
            p.quantity = 0;
        }
        expect_invariant(run_validation_chain(&b), "positions.quantity row 1");
    }

    #[test]
    fn test_positive_drawdown_is_invariant() {
        let mut b = valid_bundle(3);
        if let Some(e) = b.equity.get_mut(1) {
            e.drawdown = 0.5;
        }
        expect_invariant(run_validation_chain(&b), "drawdown row 1");
    }

    #[test]
    fn test_zero_drawdown_at_peak_is_allowed() {
        let mut b = valid_bundle(3);
        if let Some(e) = b.equity.get_mut(1) {
            e.drawdown = 0.0; // 0 at a peak is within (−∞, 0]
        }
        match run_validation_chain(&b) {
            Ok(()) => {}
            Err(e) => panic!("a zero drawdown at a peak must be allowed: {e}"),
        }
    }

    // --- contract_id parser ------------------------------------------------------

    #[test]
    fn test_parse_contract_id_happy_path() {
        let parsed = match parse_contract_id(CID) {
            Ok(p) => p,
            Err(e) => panic!("a valid contract_id must parse: {e}"),
        };
        assert_eq!(parsed.underlying, "BTC");
        assert_eq!(parsed.expiration_ns, EXP_NS);
        assert_eq!(parsed.strike_cents, 6_000_000);
        assert_eq!(parsed.style, OptionStyle::Call);
    }

    #[test]
    fn test_parse_contract_id_rejects_malformed_shapes() {
        // bad version prefix
        assert!(parse_contract_id("v2:BTC:1:2:C").is_err());
        // out-of-grammar (lower-case) underlying
        assert!(parse_contract_id("v1:btc:1:2:C").is_err());
        // colon-bearing underlying → too many fields
        assert!(parse_contract_id("v1:BT:C:1:2:C").is_err());
        // wrong field count (missing style)
        assert!(parse_contract_id("v1:BTC:1:2").is_err());
        // non-numeric expiration_ns
        assert!(parse_contract_id("v1:BTC:notanum:2:C").is_err());
        // non-numeric strike_cents
        assert!(parse_contract_id("v1:BTC:1:notanum:C").is_err());
        // bad style char
        assert!(parse_contract_id("v1:BTC:1:2:X").is_err());
    }

    // --- the equivalence oracle --------------------------------------------------

    #[test]
    fn test_oracle_is_reflexive() {
        let b = valid_bundle(5);
        assert_eq!(compare_bundles(&b, &b), Ok(()));
    }

    #[test]
    fn test_oracle_is_sort_insensitive() {
        let a = valid_bundle(5);
        let mut b = a.clone();
        // Reverse every table: a differently-ordered-but-equivalent copy still matches.
        b.fills.reverse();
        b.equity.reverse();
        b.positions.reverse();
        b.greeks.reverse();
        assert_eq!(compare_bundles(&a, &b), Ok(()));
    }

    #[test]
    fn test_oracle_detects_fills_money_mutation() {
        let a = valid_bundle(5);
        let mut b = a.clone();
        if let Some(f) = b.fills.get_mut(2) {
            f.price_cents += 1;
        }
        match compare_bundles(&a, &b) {
            Err(d) => {
                assert_eq!(d.table, "fills");
                assert_eq!(d.column, "price_cents");
                assert_eq!(d.row, Some(2));
            }
            Ok(()) => panic!("a one-cent fills mutation must be detected"),
        }
    }

    #[test]
    fn test_oracle_detects_greeks_mutation() {
        let a = valid_bundle(5);
        let mut b = a.clone();
        if let Some(g) = b.greeks.get_mut(3) {
            g.residual_cents += 7;
        }
        match compare_bundles(&a, &b) {
            Err(d) => {
                assert_eq!(d.table, "greeks_attribution");
                assert_eq!(d.column, "residual_cents");
            }
            Ok(()) => panic!("a greeks mutation must be detected"),
        }
    }

    #[test]
    fn test_oracle_detects_positions_enum_mutation() {
        let a = valid_bundle(5);
        let mut b = a.clone();
        if let Some(p) = b.positions.get_mut(1) {
            p.side = PositionSide::Long;
        }
        match compare_bundles(&a, &b) {
            Err(d) => {
                assert_eq!(d.table, "positions");
                assert_eq!(d.column, "side");
            }
            Ok(()) => panic!("a positions enum mutation must be detected"),
        }
    }

    #[test]
    fn test_oracle_manifest_run_id_mutation_detected() {
        let a = valid_bundle(3);
        let mut b = a.clone();
        b.manifest.run_id = "different".to_owned();
        match compare_bundles(&a, &b) {
            Err(d) => assert_eq!(d.table, "manifest"),
            Ok(()) => panic!("a manifest run_id difference must be detected"),
        }
    }

    #[test]
    fn test_oracle_excludes_created_utc_and_metrics() {
        let a = valid_bundle(3);
        let mut b = a.clone();
        b.manifest.created_utc = "2099-01-01T00:00:00Z".to_owned();
        b.manifest.metrics = serde_json::json!({ "totally": "different" });
        assert_eq!(
            compare_bundles(&a, &b),
            Ok(()),
            "created_utc + metrics are excluded from the oracle"
        );
    }

    #[test]
    fn test_oracle_drawdown_within_tolerance_is_equivalent() {
        let a = valid_bundle(3);
        let mut b = a.clone();
        if let Some(e) = b.equity.get_mut(1) {
            e.drawdown += 1e-12; // well within ABS_TOL
        }
        assert_eq!(compare_bundles(&a, &b), Ok(()));
    }

    #[test]
    fn test_oracle_drawdown_beyond_tolerance_diverges() {
        let a = valid_bundle(3);
        let mut b = a.clone();
        if let Some(e) = b.equity.get_mut(1) {
            e.drawdown -= 1e-3; // beyond the tolerance
        }
        match compare_bundles(&a, &b) {
            Err(d) => {
                assert_eq!(d.table, "equity_curve");
                assert_eq!(d.column, "drawdown");
            }
            Ok(()) => panic!("a drawdown beyond tolerance must diverge"),
        }
    }

    #[test]
    fn test_oracle_row_count_divergence() {
        let a = valid_bundle(5);
        let mut b = a.clone();
        let _ = b.fills.pop();
        match compare_bundles(&a, &b) {
            Err(d) => {
                assert_eq!(d.table, "fills");
                assert_eq!(d.column, "row_count");
            }
            Ok(()) => panic!("a differing row count must diverge"),
        }
    }

    #[test]
    fn test_drawdown_equivalent_nonfinite_rules() {
        // NaN never equal, even to itself.
        assert!(!drawdown_equivalent(f64::NAN, f64::NAN));
        assert!(!drawdown_equivalent(f64::NAN, 0.0));
        // Same infinity equal; opposite/finite not.
        assert!(drawdown_equivalent(f64::INFINITY, f64::INFINITY));
        assert!(drawdown_equivalent(f64::NEG_INFINITY, f64::NEG_INFINITY));
        assert!(!drawdown_equivalent(f64::INFINITY, f64::NEG_INFINITY));
        assert!(!drawdown_equivalent(f64::INFINITY, 1.0));
        // Signed zero compares equal.
        assert!(drawdown_equivalent(-0.0, 0.0));
    }

    // --- property tests ----------------------------------------------------------

    fn gen_underlying() -> impl Strategy<Value = String> {
        let ch = prop_oneof![
            (b'A'..=b'Z').prop_map(char::from),
            (b'0'..=b'9').prop_map(char::from),
            Just('.'),
            Just('_'),
        ];
        prop::collection::vec(ch, 1..=32).prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// A `contract_id` built from valid components parses back to those exact
        /// components (round-trip identity).
        #[test]
        fn contract_id_roundtrips(
            underlying in gen_underlying(),
            expiration_ns in 0_i64..i64::MAX,
            strike_cents in 0_u64..u64::MAX,
            is_call in any::<bool>(),
        ) {
            let style_char = if is_call { 'C' } else { 'P' };
            let cid = format!("v1:{underlying}:{expiration_ns}:{strike_cents}:{style_char}");
            let parsed = match parse_contract_id(&cid) {
                Ok(p) => p,
                Err(e) => panic!("a valid contract_id `{cid}` must parse: {e}"),
            };
            let expected_style = if is_call { OptionStyle::Call } else { OptionStyle::Put };
            prop_assert_eq!(parsed.underlying, underlying);
            prop_assert_eq!(parsed.expiration_ns, expiration_ns);
            prop_assert_eq!(parsed.strike_cents, strike_cents);
            prop_assert_eq!(parsed.style, expected_style);
        }

        /// Over generated well-formed tables (equity built from cumulative deltas,
        /// residual absorbing the remainder), the attribution identity holds.
        #[test]
        fn attribution_identity_holds(
            capital in 0_i64..1_000_000_000,
            deltas in prop::collection::vec(-100_000_i64..100_000, 1..12),
            theta in -50_000_i64..50_000,
            delta in -50_000_i64..50_000,
            vega in -50_000_i64..50_000,
            spread in -50_000_i64..50_000,
            fees in 0_i64..50_000,
        ) {
            let base_terms = theta + delta + vega + spread - fees;
            let mut equity = Vec::new();
            let mut greeks = Vec::new();
            let mut running = capital;
            for (i, d) in deltas.iter().enumerate() {
                let step = match u32::try_from(i) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                running += *d; // equity_cents[step]
                equity.push(EquityPoint {
                    step,
                    ts_ns: ts(step),
                    cash_cents: running,
                    position_value_cents: 0,
                    equity_cents: running,
                    drawdown: -0.01,
                });
                // step_pnl == *d, so residual absorbs the remainder to satisfy the identity.
                let fees_u64 = u64::try_from(fees).unwrap_or_default();
                greeks.push(GreeksAttribution {
                    step,
                    ts_ns: ts(step),
                    theta_pnl_cents: theta,
                    delta_pnl_cents: delta,
                    vega_pnl_cents: vega,
                    spread_capture_cents: spread,
                    fees_cents: fees_u64,
                    residual_cents: *d - base_terms,
                });
            }
            let manifest = valid_manifest(capital);
            prop_assert!(check_attribution_identity(&manifest, &equity, &greeks).is_ok());
        }

        /// The analytic-float tolerance rule is symmetric in its arguments, for any
        /// pair of floats (including NaN/±∞).
        #[test]
        fn oracle_tolerance_is_symmetric(a in any::<f64>(), b in any::<f64>()) {
            prop_assert_eq!(drawdown_equivalent(a, b), drawdown_equivalent(b, a));
        }
    }
}
