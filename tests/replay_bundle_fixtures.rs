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
    BundleError, BundleReader, LoadedBundle, LoadedReplay, PositionRow, ResourceCeilings, SeekTo,
    TimelineCursor, compare_bundles,
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
        Ok(c) => c.capital_cents().unwrap_or(0),
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
    cursor.seek(SeekTo::Step(cursor.end_step()), &loaded);

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

// =====================================================================
// (6) The WRITER-PRODUCED conformance anchor — the real IronCondor bundle.
//
// `tests/fixtures/bundle/ironcondor_conformance/` holds the five files copied
// BYTE-IDENTICAL from IronCondor's own writer output
// (`IronCondor/tests/fixtures/conformance/`, the `ironcondor.bundle.v1` freeze;
// see that directory's README for the byte-identity contract). Unlike the
// self-authored `valid/` corpus — whose bytes ChainView's own generator writes,
// so it can only prove the reader agrees with ITSELF — this fixture is the
// cross-repo CONFORMANCE anchor: it proves `BundleReader::open + load` reads what
// the writer actually emits. This resolves the second-pass #110/#117 gap ("the
// golden cannot prove bundle compatibility because it is self-authored"): the
// bytes here are writer-produced. Any drift surfaced here is a real reader↔writer
// contract finding, per the PR #95 maintainer ruling that the READER adapts to
// the WRITER.
// =====================================================================

/// The writer's opaque `run_id` (a key, never re-derived) frozen into the real
/// `manifest.json`.
const IC_RUN_ID: &str = "c4cd155fc156f3ed1488661d9cfd448af59b216c962ea4716f76685f92b21459";
/// Opening capital in **integer cents** — `config.initial_capital`, reconciled at
/// IronCondor #29 (`$100,000.00`).
const IC_CAPITAL_CENTS: i64 = 10_000_000;
/// The short-put leg the writer closes mid-run (`manual_close` at step 2).
const IC_CLOSED_PUT_CID: &str = "v1:SPX:1752883200000000000:490000:P";
/// The `position_id` of that closed short put.
const IC_CLOSED_PUT_POSITION_ID: u64 = 3;

#[test]
fn test_ironcondor_conformance_bundle_round_trips_through_the_full_chain() {
    // Open + load the WRITER'S OWN bytes through the whole #30/#31/#32 chain: the
    // schema gate, the resource ceilings, the footer/row_counts cross-check, the
    // typed per-column decode, and every §5 validation step.
    let loaded = load_ok("ironcondor_conformance");

    // The manifest gate accepted the real schema tag and preserved the opaque keys.
    assert_eq!(loaded.manifest.schema, chainview::SUPPORTED_SCHEMA);
    assert_eq!(loaded.manifest.run_id, IC_RUN_ID);
    assert_eq!(
        loaded.manifest.code_version, "0.5.0",
        "the writer's code_version is preserved verbatim (provenance only)"
    );

    // The footer/`row_counts` cross-check (enforced inside `load`) agreed with the
    // decoded lengths: {fills:10, equity_curve:5, positions:18, greeks_attribution:5}.
    assert_eq!(loaded.fills.len(), 10, "fills rows");
    assert_eq!(loaded.equity.len(), 5, "equity_curve rows");
    assert_eq!(loaded.positions.len(), 18, "positions rows");
    assert_eq!(loaded.greeks.len(), 5, "greeks_attribution rows");

    // Every fill's `strategy_run_id` equals the opaque `run_id` key (§5 step 8).
    for (i, f) in loaded.fills.iter().enumerate() {
        assert_eq!(
            f.strategy_run_id, IC_RUN_ID,
            "fills row {i}: strategy_run_id must equal the opaque run_id key"
        );
    }

    // §5 step 5 — the equity identity at exact integer cents on the real numbers.
    for (i, e) in loaded.equity.iter().enumerate() {
        assert_eq!(
            e.equity_cents,
            e.cash_cents + e.position_value_cents,
            "equity row {i}: equity_cents == cash_cents + position_value_cents"
        );
    }

    // The one narrow typed projection over `config` reads `initial_capital` as
    // unsigned integer cents, verbatim from the writer.
    let capital = match loaded.manifest.capital_config() {
        Ok(c) => c.capital_cents().unwrap_or(0),
        Err(e) => panic!("capital_cents must project from the real manifest: {e}"),
    };
    assert_eq!(
        capital, IC_CAPITAL_CENTS,
        "opening capital in integer cents"
    );

    // §5 step 6 — the cross-table attribution identity on the writer's numbers:
    // theta+delta+vega+spread-fees+residual == step_pnl (equity delta; step 0 vs
    // opening capital).
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

    // The equivalence oracle is reflexive on the real writer bundle.
    assert_eq!(
        compare_bundles(&loaded, &loaded),
        Ok(()),
        "the oracle must find the writer bundle equivalent to itself"
    );
}

#[test]
fn test_ironcondor_conformance_drill_down_closes_the_short_put() {
    // The realistic condor: four legs sharing trade_id 1 open at step 0, the short
    // put (position_id 3, contract 490000:P) is manual_close'd at step 2, and the
    // other three legs are left open_at_end. Drive the drill-down over the writer's
    // own rows.
    let loaded = load_ok("ironcondor_conformance");
    let mut cursor = TimelineCursor::new(&loaded);

    // Before the close (step 1), all four legs are open.
    cursor.seek(SeekTo::Step(1), &loaded);
    let ids_before: Vec<u64> = cursor
        .open_positions(&loaded)
        .iter()
        .map(|p| p.position_id)
        .collect();
    assert_eq!(
        ids_before,
        vec![1, 2, 3, 4],
        "all four condor legs are open before the mid-run close"
    );

    // At the last step, the manual_close short put is excluded; the three legs left
    // open_at_end remain (ordered by position_id).
    cursor.seek(SeekTo::Step(cursor.end_step()), &loaded);
    let open = cursor.open_positions(&loaded);
    let open_ids: Vec<u64> = open.iter().map(|p| p.position_id).collect();
    assert_eq!(
        open_ids,
        vec![1, 2, 4],
        "the closed short put must not appear open at the end of the tape"
    );
    assert!(
        !open
            .iter()
            .any(|p| p.position_id == IC_CLOSED_PUT_POSITION_ID),
        "open_positions must exclude the manual_close short put"
    );

    // The writer's terminal row for the short put carries its `exit_reason` and its
    // contract identity, closed at step 2.
    let terminal = loaded
        .positions
        .iter()
        .find(|p| p.position_id == IC_CLOSED_PUT_POSITION_ID && p.exit_reason.is_some());
    match terminal {
        Some(p) => {
            assert_eq!(p.exit_reason.as_deref(), Some("manual_close"));
            assert_eq!(p.contract_id, IC_CLOSED_PUT_CID);
            assert_eq!(p.step, 2, "the writer closes the short put at step 2");
        }
        None => panic!("the short put must carry a terminal manual_close row"),
    }

    // The close is also visible as the buy-back fills at step 2 (position_id 3 flips
    // to the `long` side to close). The fill's own columns render without any join.
    let close_fills: Vec<&chainview::Fill> = loaded
        .fills
        .iter()
        .filter(|f| f.step == 2 && f.position_id == IC_CLOSED_PUT_POSITION_ID)
        .collect();
    assert!(
        !close_fills.is_empty(),
        "the short put close must appear as fills at step 2"
    );
    for f in &close_fills {
        assert_eq!(f.contract_id, IC_CLOSED_PUT_CID, "close fill contract_id");
    }
}

#[test]
fn test_ironcondor_conformance_head_mtm_sums_writer_unrealized() {
    // Second-pass #108 fix: the payoff-at-head MTM is the SUM of the writer's OWN per-row
    // `unrealized_cents` — which already applies the SPX 100x contract multiplier and the
    // writer's fee conventions — never a reader recompute of `(mark − avg) · qty` that
    // drops the multiplier and reads 100x too small.
    let loaded = load_ok("ironcondor_conformance");

    // At the load head (step 0) all four condor legs are open; sum the WRITER's field.
    let cursor = TimelineCursor::new(&loaded);
    let open = cursor.open_positions(&loaded);
    let expected = open
        .iter()
        .try_fold(0_i64, |acc, p| acc.checked_add(p.unrealized_cents));
    assert_eq!(
        expected,
        Some(-14_000),
        "four open legs × −3500c = −$140.00 (the writer's multiplier-applied unrealized)"
    );
    drop(open);

    // The reader's payoff-at-head, built by `LoadedReplay::new` at the same head, must
    // equal that writer figure — and must NOT be the 100x-too-small recompute.
    let replay = LoadedReplay::new(loaded);
    assert_eq!(
        replay.payoff_head().mark_pnl_cents(),
        expected,
        "the head MTM equals the writer's summed unrealized_cents"
    );
    assert_ne!(
        replay.payoff_head().mark_pnl_cents(),
        Some(-140),
        "never the 100x-too-small `(mark − avg) · qty` recompute"
    );
}

#[test]
fn test_ironcondor_conformance_load_is_deterministic() {
    // Two loads of the writer's committed bytes yield identical decoded tables — the
    // reader never sorts and carries no hidden state.
    let a = load_ok("ironcondor_conformance");
    let b = load_ok("ironcondor_conformance");
    assert_eq!(a, b, "loading the writer bytes twice must be identical");
    assert_eq!(compare_bundles(&a, &b), Ok(()));
}
