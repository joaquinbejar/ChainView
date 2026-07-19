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
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

use crate::app::{App, EventBridge, LiveScreen, LiveState, Mode, ScreenLoad, SourceBinding};
use crate::chain::{ChainSource, ChainStore, MarketUpdate, ProviderId, StreamHealth};
use crate::config::ThemeChoice;
use crate::event::Command;
use crate::providers::deribit::{fixture_btc_chain_fetch_named, fixture_btc_stream_updates};
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
    match term.draw(|frame| draw_chain(live, frame, frame.area(), theme(), 0, utc(AS_OF))) {
        Ok(_) => {}
        Err(e) => panic!("chain golden draw failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

/// The live [`LiveState`] borrowed out of a live [`App`], for rendering.
#[track_caller]
fn live_of(app: &App) -> &LiveState {
    match &app.mode {
        Mode::Live(live) => live,
        Mode::Replay(_) => panic!("expected a live app"),
    }
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
    let mut store = ChainStore::seed(
        fixture_btc_chain_fetch_named("BTC"),
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    );
    // POLL -> a second identical poll exercises the bounded-generation merge
    // (generation bump, tombstone reconciliation with NO de-listing, pending
    // drain) deterministically and idempotently.
    store.apply_poll(fixture_btc_chain_fetch_named("BTC"), utc(AS_OF));

    // Assemble the live App on the ready chain screen — the fan-in target.
    let live = live_ready(store, pid("deribit"));
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
    app.mark_drawn();

    // -> STREAM leg through the PRODUCTION seams (issue #22, finding 5): the
    // recorded `ticker.` frames are normalized by the real `normalize_ticker`, sent
    // through the two-class `MarketUpdateSink`, drained by the bounded coalescing
    // `EventBridge`, and folded into the store by `App::on_event` — NOT a hand-built
    // `QuoteUpdate` applied directly. Because the overlay values come from the SAME
    // recorded tickers the poll seed assembled from, the fold is idempotent and the
    // render stays byte-stable against the seed-only #19 golden, so a regression in
    // the real streaming normalize/route/merge path is caught here.
    let updates = fixture_btc_stream_updates(utc(AS_OF));
    assert!(
        !updates.is_empty(),
        "the recorded fixture yields streaming overlay updates"
    );
    // The recorded tickers normalize to BOTH a quote and a Greeks row (the full
    // `normalize_ticker` output is exercised). The QUOTE class is the mergeable
    // overlay here: its bid/ask come from the SAME tickers the poll seed assembled,
    // so folding it is idempotent and the render stays byte-stable. The Greeks
    // class carries the venue `mark_iv` (49.22%), which by design overrides the
    // seed's per-strike IV — folding it would (correctly) change the render, so it
    // is proven by the Deribit unit tests and left out of this byte-golden overlay.
    let quotes: Vec<MarketUpdate> = updates
        .into_iter()
        .filter(|u| matches!(u, MarketUpdate::Quote(_)))
        .collect();
    assert!(!quotes.is_empty(), "the overlay carries quotes to merge");

    let (mut bridge, senders) = EventBridge::new(64);
    let mut sink = senders.market_update_sink();
    for update in quotes {
        // Coalesced-class overlay updates ride the sink's producer staging (the real
        // NFR-15 path). The synchronous test drives the sync coalesced publish.
        assert_eq!(
            sink.publish_coalesced(update),
            crate::providers::SendState::Open,
            "the overlay update is accepted by the bounded coalesced channel",
        );
    }
    // Fold the coalesced result into the App through the real bridge pump.
    bridge.pump(&mut app, |_command| {});
    assert!(
        app.dirty,
        "the real-seam overlay fold changed store rows and marked the frame dirty"
    );
    app.mark_drawn();
    // A second pump with no pending updates is a no-op — the coalescer delivered the
    // whole overlay in one wakeup.
    bridge.pump(&mut app, |_command| {});
    assert!(
        !app.dirty,
        "no residual staged updates after the coalesced flush"
    );

    // RENDER the merged chain and assert the #19 end-to-end golden (byte-stable).
    assert_golden(
        "chain",
        "deribit_btc_atm.txt",
        &render_chain_body(live_of(&app)),
    );
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
    let mut view = crate::ui::view::ViewState::new();
    view.sync(&app);
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| crate::ui::render(&app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("full-frame render failed: {e}"),
    }
    let after = (app.dirty, app.help_open, app.should_quit, app.tick_count);
    assert_eq!(
        before, after,
        "the draw path must not mutate app state (purity enforced by the &App signature)"
    );
}
