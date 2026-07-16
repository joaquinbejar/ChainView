//! In-crate Part B integration tests (issue #22) that require `pub(crate)`
//! internals the public `tests/` surface cannot reach.
//!
//! # Why these live in-crate (the public-API seam)
//!
//! A file under `tests/` is a **separate crate** that links `chainview` and sees
//! only its PUBLIC API. The end-to-end live-path golden render needs three
//! `pub(crate)` internals — the assembled [`ChainStore`](crate::chain::ChainStore)
//! poll→stream merge, the `pub` chain-matrix [`draw`](crate::ui::chain::draw)
//! inside the `pub(crate)` `ui` module, and the `pub(crate)` recorded-fixture
//! assembler [`fixture_btc_chain_fetch_named`](crate::providers::deribit::fixture_btc_chain_fetch_named)
//! — plus the `#[cfg(test)]` golden helper
//! ([`assert_golden`](crate::ui::golden::assert_golden) /
//! [`buffer_to_text`](crate::ui::golden::buffer_to_text)). None is on the
//! semver-governed surface, for the **same reason the #19 render goldens live
//! in-crate**: promoting them would widen the public API. So the live-path
//! golden, the id-agnostic render-parity proof, and the draw-path no-I/O
//! assertion live here, `#[cfg(test)]`.
//!
//! The COMPLEMENTARY faux-external-provider CONFORMANCE (fetch → subscribe →
//! forced reconnect resubscribes off the fresh `ChainFetch.aliases` → capability
//! gating → registration → supervision), driven through the PUBLIC port + the
//! `ChainViewApp::builder().register(..)` surface ALONE, lives in
//! `tests/integration.rs`; the arch test (`tests/arch.rs`) proves the faux
//! adapter reaches parity using only the port with **no built-in special-casing**
//! (`docs/TESTING.md` §7, `docs/03-data-providers.md` §11, §12).
//!
//! Every test here is deterministic (recorded fixtures + fixed instants, no
//! socket, no wall-clock wait) and finishes far under the 10 s integration bound
//! (`docs/TESTING.md` §7).

#![cfg(test)]

use std::time::Duration;

use chrono::{DateTime, Utc};
use optionstratlib::OptionStyle;
use optionstratlib::prelude::Positive;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

use crate::app::{App, LiveScreen, LiveState, Mode, ScreenLoad, SourceBinding};
use crate::chain::{
    ChainSource, ChainStore, ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey,
    MergeOutcome, ProviderId, QuoteUpdate, SettlementStyle, StreamHealth,
};
use crate::config::ThemeChoice;
use crate::event::Command;
use crate::providers::deribit::fixture_btc_chain_fetch_named;
use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
use crate::ui::chain::draw as draw_chain;
use crate::ui::golden::{GOLDEN_HEIGHT, GOLDEN_WIDTH, assert_golden, buffer_to_text};
use crate::ui::theme::Theme;

/// The fixed as-of / poll / receipt instant every test uses, so the render is
/// byte-stable across machines (no wall clock). Distinct from the fixture's own
/// contract expiry, exactly as the #19 golden harness seeds it.
const AS_OF: i64 = 1_700_000_000;

// --- Test constructors (no unwrap/expect/indexing per the ruleset) -----------

#[track_caller]
fn pid(id: &str) -> ProviderId {
    match ProviderId::new(id) {
        Ok(p) => p,
        Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
    }
}

#[track_caller]
fn utc(secs: i64) -> DateTime<Utc> {
    match DateTime::<Utc>::from_timestamp(secs, 0) {
        Some(t) => t,
        None => panic!("invalid test timestamp: {secs}"),
    }
}

/// The Deribit-shaped capabilities the live source declares (chain + depth +
/// provided Greeks) — the honest set the UI gates on.
fn caps() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        .depth(true)
        .greeks(GreeksCapability::Provided)
        .build()
}

fn theme() -> Theme {
    Theme::resolve(ThemeChoice::Auto, false)
}

#[track_caller]
fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    match Terminal::new(TestBackend::new(width, height)) {
        Ok(t) => t,
        Err(e) => panic!("TestBackend construction failed: {e}"),
    }
}

/// A live chain-screen state on [`ScreenLoad::Ready`], bound to `provider`'s
/// declared capabilities and the merged `store`.
fn live_ready(store: ChainStore, provider: ProviderId) -> LiveState {
    let mut live = LiveState::new(
        SourceBinding::new(provider, caps(), StreamHealth::Live),
        store,
    );
    live.screen = LiveScreen::Chain;
    live.load = ScreenLoad::Ready;
    live
}

/// Draw the chain body for `live` into a fixed [`GOLDEN_WIDTH`]×[`GOLDEN_HEIGHT`]
/// `TestBackend` (tick 0, so the loading spinner frame is fixed) and return the
/// buffer as golden text — the exact #19 golden harness, so the bytes match the
/// committed `chain/deribit_btc_atm` golden.
#[track_caller]
fn render_chain_body(live: &LiveState) -> String {
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| draw_chain(live, frame, frame.area(), theme(), 0)) {
        Ok(_) => {}
        Err(e) => panic!("chain golden draw failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

/// Build an **idempotent** streaming [`QuoteUpdate`] for one leg from the store's
/// OWN current row values, so folding it patches the row to identical values (a
/// real `apply_quote` merge) while the rendered chain stays byte-stable. The
/// `provider` equals the chain's source provider, so this is a within-provider
/// poll→stream merge (the overlay gate is a no-op).
fn idempotent_quote(
    provider: &ProviderId,
    underlying: &str,
    expiry: DateTime<Utc>,
    strike: Positive,
    style: OptionStyle,
    bid: Positive,
    ask: Positive,
) -> QuoteUpdate {
    QuoteUpdate {
        instrument: Instrument {
            key: InstrumentKey {
                underlying: underlying.to_owned(),
                expiration_utc: expiry,
                strike,
                style,
            },
            provider: provider.clone(),
            native_symbol: format!("{underlying}-{strike}-{}", style.as_str()),
            stream_symbol: None,
            spec: ContractSpecFingerprint {
                contract_multiplier: 1,
                settlement: SettlementStyle::Cash,
                exercise: ExerciseStyle::European,
                quote_currency: "USD".to_owned(),
                venue_product_code: underlying.to_owned(),
            },
        },
        bid: Some(bid),
        ask: Some(ask),
        last: None,
        bid_size: None,
        ask_size: None,
        event_time: None,
        received_time: utc(AS_OF),
    }
}

/// Every priced leg (a call and/or a put with BOTH sides present) currently in
/// the store, as owned `(strike, style, bid, ask)` tuples — the stream-mergeable
/// set. Collected owned so the store's immutable borrow ends before the merge.
fn priced_legs(store: &ChainStore) -> Vec<(Positive, OptionStyle, Positive, Positive)> {
    let mut legs = Vec::new();
    for od in store.chain().options.iter() {
        if let (Some(bid), Some(ask)) = (od.call_bid, od.call_ask) {
            legs.push((od.strike_price, OptionStyle::Call, bid, ask));
        }
        if let (Some(bid), Some(ask)) = (od.put_bid, od.put_ask) {
            legs.push((od.strike_price, OptionStyle::Put, bid, ask));
        }
    }
    legs
}

// =============================================================================
// 1. The end-to-end live-path integration test (Part B task 1).
//    Deribit #17 fixture -> adapter normalize (#15/#16) -> ChainStore poll->stream
//    merge (#7) -> chain::draw -> the chain/deribit_btc_atm golden (#19), with NO
//    network and a fixed as-of instant. This is the zero-config Deribit acceptance
//    proven against fixtures (`docs/TESTING.md` §7).
// =============================================================================

#[test]
fn test_live_path_deribit_fixture_poll_stream_merge_renders_btc_atm_golden() {
    // POLL leg: the recorded #17 fixtures through the REAL normalize/assemble seam.
    let fetch = fixture_btc_chain_fetch_named("BTC");
    let mut store = ChainStore::seed(
        fetch,
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    );

    // POLL -> a second identical poll exercises the bounded-generation merge
    // (generation bump, tombstone reconciliation with NO de-listing, pending
    // drain) deterministically and idempotently.
    store.apply_poll(fixture_btc_chain_fetch_named("BTC"), utc(AS_OF));

    // -> STREAM leg: fold an idempotent quote into EVERY priced leg, reconstructed
    // from the store's own current values, so each `apply_quote` patches its row to
    // identical values (a real merge fold, `MergeOutcome::Applied`) while the
    // rendered chain stays byte-identical to the seed-only #19 golden.
    let key = store.chain_key().clone();
    let (provider, underlying, expiry) = (key.0, key.1, key.2);
    let legs = priced_legs(&store);
    assert!(
        !legs.is_empty(),
        "the assembled fixture chain must have priced legs to stream-merge"
    );
    for (strike, style, bid, ask) in legs {
        let update = idempotent_quote(&provider, &underlying, expiry, strike, style, bid, ask);
        assert_eq!(
            store.apply_quote(&update),
            MergeOutcome::Applied,
            "the streaming quote folds into its row at strike {strike} {style:?}",
        );
    }

    // RENDER the merged chain and assert the #19 end-to-end golden.
    let live = live_ready(store, pid("deribit"));
    assert_golden("chain", "deribit_btc_atm.txt", &render_chain_body(&live));
}

// =============================================================================
// 2. The render path gates on data + declared capabilities, NEVER the ProviderId
//    (Part B task 4, render half). The SAME assembled fixture chain, but under a
//    non-reserved EXTERNAL ProviderId("faux"), renders byte-identically to the
//    built-in — proving no built-in special-casing at the render edge.
// =============================================================================

#[test]
fn test_render_is_provider_id_agnostic_faux_renders_identical_golden() {
    assert!(
        !pid("faux").is_reserved(),
        "`faux` is an external, non-reserved provider id (parity with a built-in)"
    );
    let fetch = fixture_btc_chain_fetch_named("BTC");
    let store = ChainStore::seed(
        fetch,
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    );
    // The SourceBinding carries the external id "faux"; the chain-matrix draw reads
    // store state + the declared capabilities, never `source.provider`, so the
    // rendered bytes equal the built-in golden — the id-agnostic render contract.
    let live = live_ready(store, pid("faux"));
    assert_golden("chain", "deribit_btc_atm.txt", &render_chain_body(&live));
}

// =============================================================================
// 3. The draw path is reachable with NO async runtime and mutates nothing
//    (Part B task 3, the #13 draw-purity check elevated). `render` is a plain
//    synchronous fn over `&App` (never `&mut`, never `async`), so no `.await`/I/O
//    is reachable from the draw path; this test runs OUTSIDE any tokio runtime (a
//    plain `#[test]`, NOT `#[tokio::test]`) to demonstrate it. The layering half —
//    no provider/`tokio` I/O import in `src/ui/*` — is enforced by `tests/arch.rs`.
// =============================================================================

#[test]
fn test_draw_path_runs_without_async_runtime_and_mutates_nothing() {
    let fetch = fixture_btc_chain_fetch_named("BTC");
    let store = ChainStore::seed(
        fetch,
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    );
    let live = live_ready(store, pid("deribit"));
    // Creating the command channel needs no runtime (only awaited send/recv would);
    // the receiver is dropped — `render` never touches it.
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
    app.mark_drawn();

    let before = (app.dirty, app.help_open, app.should_quit, app.tick_count);
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| crate::ui::render(&app, frame)) {
        Ok(_) => {}
        Err(e) => panic!("full-frame render failed: {e}"),
    }
    let after = (app.dirty, app.help_open, app.should_quit, app.tick_count);
    assert_eq!(
        before, after,
        "the draw path must not mutate app state (purity enforced by the &App signature)"
    );
}
