//! The v0.4 acceptance-gate **capability-matrix reconcile** (issue #46,
//! `docs/03-data-providers.md` §8, `docs/specs/providers.md` §1).
//!
//! This module is the single executable table that reconciles every bundled
//! adapter's live [`ProviderCapabilities`](crate::ProviderCapabilities) against
//! the documented `docs/03-data-providers.md` §8 row, cell by cell — so a drift
//! between an adapter's `capabilities()` and the published matrix fails a test,
//! not a review. Each adapter also carries its own
//! `test_<id>_capabilities_match_section_8_row` unit test in its module; this is
//! the **cross-adapter** reconcile that names all rows side by side (the matrix
//! shape) plus the IG-deferred cell.
//!
//! It lives in-crate (like `tests_integration`) because it reads each adapter's
//! crate-private `<id>_capabilities()` — none of which is on the public surface.
//! The gated adapters (`tastytrade` / `alpaca` / `dxlink`) are asserted only when
//! their Cargo feature is on; the default build reconciles the always-compiled
//! `deribit` row and the IG-deferred marker.

use crate::{
    AuthKind, ChainCapability, ChainPollCapability, GreeksCapability, OptionStreamCapability,
    ProviderCapabilities, RESERVED_PROVIDER_IDS,
};

/// One `docs/03-data-providers.md` §8 row as an executable expectation. The
/// `chain_poll` cell is the documented "Chain poll: yes/no" (a `Poll` variant vs
/// `None`), never the adapter-internal interval hint.
struct MatrixRow {
    id: &'static str,
    chain: ChainCapability,
    depth: bool,
    greeks: GreeksCapability,
    option_stream: OptionStreamCapability,
    underlying_stream: bool,
    chain_poll: bool,
    trades_tape: bool,
    auth: AuthKind,
}

/// Assert an adapter's live capabilities equal its documented §8 row, cell by
/// cell. Panics with the offending id + cell on any drift (no `assert_eq!`
/// message crate needed — the `{id}` is in every message).
#[track_caller]
fn assert_row(caps: ProviderCapabilities, row: &MatrixRow) {
    let id = row.id;
    assert_eq!(caps.chain, row.chain, "{id}: chain cell drifted from §8");
    assert_eq!(caps.depth, row.depth, "{id}: depth cell drifted from §8");
    assert_eq!(caps.greeks, row.greeks, "{id}: greeks cell drifted from §8");
    assert_eq!(
        caps.option_stream, row.option_stream,
        "{id}: option-stream cell drifted from §8"
    );
    assert_eq!(
        caps.underlying_stream, row.underlying_stream,
        "{id}: underlying-stream cell drifted from §8"
    );
    let polls = matches!(caps.chain_poll, ChainPollCapability::Poll { .. });
    assert_eq!(
        polls, row.chain_poll,
        "{id}: chain-poll cell drifted from §8"
    );
    assert_eq!(
        caps.trades_tape, row.trades_tape,
        "{id}: trades-tape cell drifted from §8"
    );
    assert_eq!(caps.auth, row.auth, "{id}: auth cell drifted from §8");
}

// --- deribit (always compiled; the zero-config default) ----------------------

/// The `deribit` §8 row: Assemble chain, depth yes, Provided Greeks, unverified
/// ChainQuotes overlay, no underlying stream (ADR-0009), polls, no trades tape,
/// no auth.
fn deribit_row() -> MatrixRow {
    MatrixRow {
        id: "deribit",
        chain: ChainCapability::Assemble,
        depth: true,
        greeks: GreeksCapability::Provided,
        option_stream: OptionStreamCapability::ChainQuotes { verified: false },
        underlying_stream: false,
        chain_poll: true,
        trades_tape: false,
        auth: AuthKind::None,
    }
}

#[test]
fn test_deribit_row_reconciles_with_section_8() {
    assert_row(
        crate::providers::deribit::deribit_capabilities(),
        &deribit_row(),
    );
}

// --- tastytrade (feature-gated) ----------------------------------------------

#[cfg(feature = "tastytrade")]
#[test]
fn test_tastytrade_row_reconciles_with_section_8() {
    let row = MatrixRow {
        id: "tastytrade",
        chain: ChainCapability::Native,
        depth: false,
        greeks: GreeksCapability::Provided,
        option_stream: OptionStreamCapability::ChainQuotes { verified: false },
        underlying_stream: true,
        chain_poll: true,
        trades_tape: false,
        auth: AuthKind::UserPass,
    };
    assert_row(
        crate::providers::tastytrade::tastytrade_capabilities(),
        &row,
    );
}

// --- alpaca (feature-gated) --------------------------------------------------

#[cfg(feature = "alpaca")]
#[test]
fn test_alpaca_row_reconciles_with_section_8() {
    // Depth is `false`: Alpaca depth exists only for crypto spot, a class v1 does
    // not select, so it is not an option-chain depth source (§8, depth note).
    // Opt stream is `None`: the Alpaca WebSocket carries only the underlying.
    let row = MatrixRow {
        id: "alpaca",
        chain: ChainCapability::Native,
        depth: false,
        greeks: GreeksCapability::Provided,
        option_stream: OptionStreamCapability::None,
        underlying_stream: true,
        chain_poll: true,
        trades_tape: false,
        auth: AuthKind::KeySecret,
    };
    assert_row(crate::providers::alpaca::alpaca_capabilities(), &row);
}

// --- dxlink (feature-gated) --------------------------------------------------

#[cfg(feature = "dxlink")]
#[test]
fn test_dxlink_row_reconciles_with_section_8() {
    // Overlay-only: no chain, no depth, no chain poll; a SymbolOnly overlay stream.
    let row = MatrixRow {
        id: "dxlink",
        chain: ChainCapability::None,
        depth: false,
        greeks: GreeksCapability::Provided,
        option_stream: OptionStreamCapability::SymbolOnly { verified: false },
        underlying_stream: false,
        chain_poll: false,
        trades_tape: false,
        auth: AuthKind::Token,
    };
    assert_row(crate::providers::dxlink::dxlink_capabilities(), &row);
}

// --- IG: deferred, N/A (issue #39) -------------------------------------------

#[test]
fn test_ig_row_is_deferred_not_shipped() {
    // The `ig` §8 row is a **deferred** built-in (docs/03 §7.4/§8): `ig-client`
    // 0.12.1 exposes no config-injectable constructor, so no adapter ships and
    // there is no `ig_capabilities()` to reconcile. The id stays RESERVED (an
    // external IG integration binds it through the public port), so this row is
    // marked N/A rather than asserted against a live adapter.
    assert!(
        RESERVED_PROVIDER_IDS.contains(&"ig"),
        "ig stays a reserved id while the built-in is deferred"
    );
}

// --- Coverage: every §8 non-IG row has a reconcile ---------------------------

#[test]
fn test_every_reserved_id_is_either_reconciled_or_deferred() {
    // The five reserved ids partition into: reconciled built-in rows above
    // (deribit always; tastytrade/alpaca/dxlink when their feature is on) and the
    // single deferred `ig`. This guards against a reserved id gaining an adapter
    // without a matrix row landing here.
    assert_eq!(RESERVED_PROVIDER_IDS.len(), 5);
    for id in ["deribit", "tastytrade", "dxlink", "ig", "alpaca"] {
        assert!(
            RESERVED_PROVIDER_IDS.contains(&id),
            "{id} is one of the five reserved built-in ids"
        );
    }
}
