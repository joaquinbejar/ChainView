# `ironcondor_conformance` — the writer-produced replay conformance anchor

These five files are the **cross-repo conformance anchor** for the IronCondor to
ChainView replay contract (`ironcondor.bundle.v1`). They are **copied
byte-identical** from the real IronCondor bundle writer's output.

## Provenance

- **Source repo:** IronCondor (`https://github.com/joaquinbejar/IronCondor`).
- **Source path:** `IronCondor/tests/fixtures/conformance/`.
- **Produced by:** IronCondor's real bundle writer (a realistic thin-book
  iron-condor run, `tests/conformance.rs`, feature `orderbook`), frozen at the
  v0.3 schema freeze.

```
manifest.json               # run metadata + metrics + row_counts (schema ironcondor.bundle.v1)
fills.parquet               # 10 rows — 4 legs opened multi-level at step 0 + short-put close at step 2
equity_curve.parquet        #  5 rows — one per step, 0..4
positions.parquet           # 18 rows — per-step leg marks + the short-put manual_close terminal row
greeks_attribution.parquet  #  5 rows — one per step, 0..4
```

## Byte-identity IS the contract

The bytes here MUST be identical to the IronCondor source path — that identity is
the whole point of the anchor: ChainView's reader (`src/replay/{mod,tables,
validate}.rs`) is proven to consume exactly what the writer emits, not a
ChainView-authored guess. Do **not** hand-edit or regenerate these files in
ChainView. Any change is a schema event: it is produced on the IronCondor side,
re-copied here verbatim, and reconciled across both repos in the same week (see
`docs/TESTING.md` §6, `docs/04-replay-mode.md` §2.4).

To refresh after an IronCondor writer change, re-copy the five files from the
source path above and re-run the conformance tests:

```
cp IronCondor/tests/fixtures/conformance/{manifest.json,fills.parquet,\
equity_curve.parquet,positions.parquet,greeks_attribution.parquet} \
   ChainView/tests/fixtures/bundle/ironcondor_conformance/
cargo test --test replay_bundle_fixtures ironcondor_conformance
```

## What consumes it here (ChainView, reader side)

`tests/replay_bundle_fixtures.rs` (`test_ironcondor_conformance_*`) drives
`BundleReader::open + load` end-to-end over these exact bytes: the schema /
row-count gate, the resource ceilings, the typed per-column decode, and the whole
§5 validation chain (equity identity, cross-table attribution identity against
`config.initial_capital`, step domain, referential integrity, `contract_id`
grammar), plus the timeline drill-down (the `manual_close` short put leaves the
open set after step 2). This is the anchor that resolves the second-pass #110 /
#117 finding that a self-authored golden cannot prove writer compatibility.
