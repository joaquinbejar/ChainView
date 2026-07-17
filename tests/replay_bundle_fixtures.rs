//! The committed replay-bundle fixture corpus, driven through the **real**
//! [`chainview::BundleReader`] open+load path (issue #36; `docs/TESTING.md` §6,
//! `docs/04-replay-mode.md` §3/§5, `docs/SECURITY.md` §6.2).
//!
//! This is the v0.3 security gate: the shared conformance bundle round-trips clean
//! through the whole #32 validation chain, and each negative / adversarial fixture
//! is proven to reject with its **exact typed [`chainview::BundleError`]** at a
//! **pre-materialization stage** (the reject-stage message locates the failure
//! before any batch is decoded — the bounded property the #30 working-set unit
//! tests prove positively via `budget.used()` / the decoder-not-invoked probe).
//!
//! # Committed, deterministic bytes
//!
//! The fixtures under `tests/fixtures/bundle/` are committed artifacts produced by
//! the reproducible generator in `tests/common/bundle_gen.rs`. Regenerate them with
//! the `#[ignore]`d [`regenerate_committed_fixtures`] test:
//!
//! ```text
//! cargo test --test replay_bundle_fixtures -- --ignored regenerate_committed_fixtures
//! ```
//!
//! Regeneration is deterministic (no timestamps / RNG), so the committed bytes are
//! stable on the same `parquet` crate version. Money in every fixture is integer
//! cents; the only `f64` on the decode path is `equity.drawdown` (asserted below).

#[path = "common/bundle_gen.rs"]
mod bundle_gen;

use std::path::{Path, PathBuf};

use chainview::{
    BundleError, BundleReader, LoadedBundle, PositionRow, ResourceCeilings, SeekTo, TimelineCursor,
    compare_bundles,
};

// --- Fixture locations -------------------------------------------------------

/// The committed corpus root, `tests/fixtures/bundle/`.
fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle")
}

/// The path of one committed fixture directory.
fn fixture(name: &str) -> PathBuf {
    corpus_root().join(name)
}

// --- Load helpers (no unwrap/expect/indexing per the ruleset) ----------------

#[track_caller]
fn open(name: &str) -> Result<BundleReader, BundleError> {
    BundleReader::open(fixture(name))
}

/// Open + load a fixture under the given ceilings, returning the load result.
#[track_caller]
fn load_with(name: &str, ceilings: ResourceCeilings) -> Result<LoadedBundle, BundleError> {
    let reader = match BundleReader::open_with_ceilings(fixture(name), ceilings) {
        Ok(r) => r,
        Err(e) => panic!("`{name}` must open before load is exercised: {e}"),
    };
    reader.load()
}

/// Open + load a fixture under the default ceilings, asserting success.
#[track_caller]
fn load_ok(name: &str) -> LoadedBundle {
    match open(name).and_then(|r| r.load()) {
        Ok(loaded) => loaded,
        Err(e) => panic!("`{name}` must load clean under default ceilings: {e}"),
    }
}

// =====================================================================
// (0) Regeneration of the committed corpus — #[ignore]d, run manually.
// =====================================================================

/// Regenerate every committed fixture under `tests/fixtures/bundle/`. `#[ignore]`d
/// so a normal `cargo test` (and CI) never rewrites the committed bytes; run it
/// explicitly to refresh the corpus after a generator change.
#[test]
#[ignore = "writes the committed fixture corpus; run explicitly to regenerate"]
fn regenerate_committed_fixtures() {
    let root = corpus_root();
    bundle_gen::write_valid(&root.join("valid"));
    bundle_gen::write_bad_schema(&root.join("bad_schema"));
    bundle_gen::write_missing_table(&root.join("missing_table"));
    bundle_gen::write_oversized_footer(&root.join("oversized_footer"));
    bundle_gen::write_rowcount_lie(&root.join("rowcount_lie"));
    bundle_gen::write_truncated(&root.join("truncated"));
    bundle_gen::write_decompression_bomb(&root.join("decompression_bomb"));
    bundle_gen::write_dangling_position_id(&root.join("dangling_position_id"));
}

// =====================================================================
// (1) The conformance bundle round-trips clean through the whole chain.
// =====================================================================

#[test]
fn test_valid_bundle_round_trips_through_the_full_chain() {
    let loaded = load_ok("valid");
    let n = bundle_gen::VALID_STEPS;
    assert_eq!(loaded.fills.len(), n, "one fill per step");
    assert_eq!(loaded.equity.len(), n, "one equity row per step");
    assert_eq!(loaded.positions.len(), n, "one position row per step");
    assert_eq!(loaded.greeks.len(), n, "one greeks row per step");

    // `run_id` is used only as an opaque referential key: it is the manifest value
    // and every fill's `strategy_run_id` equals it (the chain already enforced this;
    // re-assert the opaque-key contract explicitly).
    assert_eq!(loaded.manifest.run_id, bundle_gen::RUN_ID);
    for (i, f) in loaded.fills.iter().enumerate() {
        assert_eq!(
            f.strategy_run_id, loaded.manifest.run_id,
            "fills row {i}: strategy_run_id must equal the opaque run_id key"
        );
    }

    // The equity identity holds at exact integer cents (the chain enforced it; spot
    // it here to bind the fixture to the documented invariant).
    for (i, e) in loaded.equity.iter().enumerate() {
        assert_eq!(
            e.equity_cents,
            e.cash_cents + e.position_value_cents,
            "equity row {i}: equity_cents == cash_cents + position_value_cents"
        );
    }

    // The mark-to-market attribution identity holds: theta+delta+vega+spread-fees+
    // residual == step_pnl (equity delta; step 0 vs opening capital).
    let capital = match loaded.manifest.capital_config() {
        Ok(c) => c.capital_cents,
        Err(e) => panic!("capital_cents must project: {e}"),
    };
    let mut prev = capital;
    for (i, (e, g)) in loaded.equity.iter().zip(loaded.greeks.iter()).enumerate() {
        let step_pnl = e.equity_cents - prev;
        let fees = i64::try_from(g.fees_cents).unwrap_or(i64::MAX);
        let terms =
            g.theta_pnl_cents + g.delta_pnl_cents + g.vega_pnl_cents + g.spread_capture_cents
                - fees
                + g.residual_cents;
        assert_eq!(
            terms, step_pnl,
            "greeks row {i}: attribution terms must sum to the equity-delta step_pnl"
        );
        prev = e.equity_cents;
    }

    // The equivalence oracle is reflexive on the conformance bundle.
    assert_eq!(
        compare_bundles(&loaded, &loaded),
        Ok(()),
        "the oracle must find the conformance bundle equivalent to itself"
    );
}

#[test]
fn test_valid_bundle_load_is_deterministic() {
    // Two loads of the same committed bytes yield identical decoded tables — the
    // reader never sorts and carries no hidden state.
    let a = load_ok("valid");
    let b = load_ok("valid");
    assert_eq!(a, b, "loading the committed bytes twice must be identical");
    assert_eq!(compare_bundles(&a, &b), Ok(()));
}

// =====================================================================
// (2) Negative fixtures — clean typed errors at open(), not partial reads.
// =====================================================================

#[test]
fn test_bad_schema_is_unsupported_schema() {
    match open("bad_schema") {
        Err(BundleError::UnsupportedSchema(tag)) => {
            assert_eq!(
                tag,
                bundle_gen::BAD_SCHEMA,
                "the offending tag is echoed (clamped)"
            );
        }
        other => panic!("a major-incompatible schema must be UnsupportedSchema, got {other:?}"),
    }
}

#[test]
fn test_missing_table_is_missing_table() {
    match open("missing_table") {
        Err(BundleError::MissingTable(t)) => {
            assert_eq!(
                t, "greeks_attribution.parquet",
                "the absent required table is named, not a partial render"
            );
        }
        other => panic!("a missing required table must be MissingTable, got {other:?}"),
    }
}

// =====================================================================
// (3) Adversarial resource fixtures — typed reject at a pre-materialization
//     stage. The stage message locates the reject BEFORE any batch decode;
//     the positive `budget.used()` / decoder-not-invoked bounded proofs live in
//     the #30 unit tests (`src/replay/mod.rs`), reused per the issue.
// =====================================================================

#[test]
fn test_oversized_footer_is_too_large_pre_decode() {
    // A lowered per-table row ceiling (below the fixture's honest footer) trips
    // ceiling 2 before the decode loop — proving the reject is pre-decode.
    let ceilings = ResourceCeilings {
        max_table_rows: bundle_gen::OVERSIZED_MAX_TABLE_ROWS,
        ..ResourceCeilings::default()
    };
    match load_with("oversized_footer", ceilings) {
        Err(BundleError::TooLarge(detail)) => assert!(
            detail.contains("exceeds per-table ceiling"),
            "the reject must be the pre-decode footer-row ceiling: {detail}"
        ),
        other => panic!("an over-ceiling footer must be TooLarge pre-decode, got {other:?}"),
    }
}

#[test]
fn test_rowcount_lie_is_invariant_bounded() {
    // The manifest lies (claims ROWCOUNT_LIE_CLAIMED fills; the file holds
    // VALID_STEPS). The footer cross-check rejects with Invariant BEFORE any batch,
    // proving `row_counts` never sizes an allocation.
    match load_with("rowcount_lie", ResourceCeilings::default()) {
        Err(BundleError::Invariant(detail)) => {
            assert!(
                detail.contains("row_counts says") && detail.contains("Parquet footer says"),
                "the reject must be the footer/row_counts cross-check (pre-decode): {detail}"
            );
            let claimed = bundle_gen::ROWCOUNT_LIE_CLAIMED.to_string();
            assert!(
                detail.contains(&claimed),
                "the lie ({claimed}) is reported but never used to size a Vec: {detail}"
            );
        }
        other => panic!("a row_counts lie must be Invariant, got {other:?}"),
    }
}

#[test]
fn test_truncated_is_typed_decode_error_not_panic() {
    // A file cut off mid-column has no readable Parquet footer: the reject is a
    // typed Parquet (or Invariant) error, never a panic or a partial Vec.
    match load_with("truncated", ResourceCeilings::default()) {
        Err(BundleError::Parquet(_)) | Err(BundleError::Invariant(_)) => {}
        other => panic!("a truncated table must be a typed Parquet/Invariant error, got {other:?}"),
    }
}

#[test]
fn test_decompression_bomb_is_too_large_bounded() {
    // A tiny ZSTD file whose uncompressed footer size clears the default 20×
    // expansion ratio is rejected by the pre-decode bomb check — the measured
    // budget stops the decode before the 64 KiB uncompressed payload is touched.
    match load_with("decompression_bomb", ResourceCeilings::default()) {
        Err(BundleError::TooLarge(detail)) => assert!(
            detail.contains("decompression bomb"),
            "the reject must be the pre-decode decompression-bomb check: {detail}"
        ),
        other => panic!("a decompression bomb must be TooLarge, got {other:?}"),
    }
}

// =====================================================================
// (4) The #32-flagged referential gap — a dangling position_id degrades
//     gracefully today (ADR-0011). The chain does NOT reject it; the
//     drill-down/detail path reads the fill's own columns and never
//     fabricates a position for a dangling id.
// =====================================================================

#[test]
fn test_dangling_position_id_loads_clean_documenting_the_gap() {
    // The bundle carries one fill whose position_id references no positions row.
    // §5 step 8 does NOT require `fills.position_id ∈ positions.position_id`, so it
    // loads clean — this fixture pins that documented gap (ADR-0011) so a future
    // referential check is a deliberate, coordinated change, not an accident.
    let loaded = load_ok("dangling_position_id");
    let dangling = loaded
        .fills
        .iter()
        .find(|f| f.position_id == bundle_gen::DANGLING_POSITION_ID);
    assert!(
        dangling.is_some(),
        "the dangling fill must be present and loaded verbatim"
    );
    assert!(
        !loaded
            .positions
            .iter()
            .any(|p| p.position_id == bundle_gen::DANGLING_POSITION_ID),
        "no positions row carries the dangling id — the join is genuinely broken"
    );
}

#[test]
fn test_dangling_position_id_drill_down_degrades_gracefully() {
    // Drive the drill-down surface at the scrub head: the dangling fill is visible
    // in the fills history (its own columns intact — the #35 detail panel reads
    // exactly these, never a positions join), and `open_positions` never fabricates
    // a leg for the dangling id. Nothing panics.
    let loaded = load_ok("dangling_position_id");
    let mut cursor = TimelineCursor::new(&loaded);
    cursor.seek(SeekTo::Step(cursor.end_step), &loaded);

    let visible = cursor.visible_fills(&loaded);
    let dangling = visible
        .iter()
        .find(|f| f.position_id == bundle_gen::DANGLING_POSITION_ID);
    match dangling {
        Some(f) => {
            // The fill's OWN columns (the detail panel's inputs) are intact and safe
            // to render — no join to positions is required to display them.
            assert_eq!(
                f.contract_id,
                bundle_gen::CID,
                "contract_id renders from the fill"
            );
            assert_eq!(f.quantity, 1, "quantity renders from the fill");
            assert_eq!(
                i64::try_from(f.strike_cents).unwrap_or(i64::MAX),
                bundle_gen::STRIKE_CENTS
            );
        }
        None => panic!("the dangling fill must be visible at the scrub head"),
    }

    // The open-position set is derived purely from `positions`; a dangling fill
    // contributes nothing and no fabricated leg appears.
    let open: Vec<&PositionRow> = cursor.open_positions(&loaded);
    assert!(
        !open
            .iter()
            .any(|p| p.position_id == bundle_gen::DANGLING_POSITION_ID),
        "open_positions must never fabricate a leg for a dangling position_id"
    );
}

// =====================================================================
// (5) Money discipline — no f64 on the decode path except the analytic
//     drawdown ratio, across the whole replay/decode surface.
// =====================================================================

#[test]
fn test_no_f64_field_on_the_decode_path_except_drawdown() {
    // Scan the replay module sources (the manifest/row types, the typed decoders,
    // and the timeline) for any PUBLIC `f64` field: money is integer cents, so the
    // only one allowed is `EquityPoint::drawdown`. Mirrors the in-crate type-level
    // guard, extended across the decode path per issue #36.
    let sources = [
        ("mod.rs", include_str!("../src/replay/mod.rs")),
        ("tables.rs", include_str!("../src/replay/tables.rs")),
        ("timeline.rs", include_str!("../src/replay/timeline.rs")),
        ("validate.rs", include_str!("../src/replay/validate.rs")),
    ];
    let mut f64_fields = 0_usize;
    for (file, src) in sources {
        for line in src.lines() {
            let trimmed = line.trim_start();
            // A public STRUCT FIELD `pub <name>: f64` — not a `const`/`static`
            // tolerance (the oracle's analytic-ratio tolerances are legitimately
            // f64) and not an `fn` parameter/return.
            let is_pub_field = trimmed.starts_with("pub ")
                && trimmed.contains(": f64")
                && !trimmed.starts_with("pub const")
                && !trimmed.starts_with("pub static")
                && !trimmed.starts_with("pub fn")
                && !trimmed.contains("fn ");
            if is_pub_field {
                assert!(
                    trimmed.contains("drawdown"),
                    "{file}: unexpected public f64 field (money must be integer cents): `{trimmed}`"
                );
                f64_fields += 1;
            }
        }
    }
    assert_eq!(
        f64_fields, 1,
        "exactly one public f64 field (EquityPoint::drawdown) may exist on the decode path"
    );

    // And at runtime, the sole f64 the decode produces is a well-formed analytic
    // ratio in `(−∞, 0]`, never money and never non-finite.
    let loaded = load_ok("valid");
    for (i, e) in loaded.equity.iter().enumerate() {
        assert!(
            e.drawdown.is_finite(),
            "equity row {i}: drawdown must be finite"
        );
        assert!(e.drawdown <= 0.0, "equity row {i}: drawdown must be <= 0");
    }
}
