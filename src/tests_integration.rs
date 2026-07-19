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
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::Positive;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

use crate::app::{App, EventBridge, LiveScreen, LiveState, Mode, ScreenLoad, SourceBinding};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, MarketUpdate, ProviderId,
    StreamHealth,
};
use crate::config::ThemeChoice;
use crate::event::Command;
use crate::providers::deribit::{
    fixture_btc_chain_fetch_named, fixture_btc_depth_ladder, fixture_btc_stream_updates,
};
use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
use crate::ui::chain::draw as draw_chain;
use crate::ui::golden::{GOLDEN_HEIGHT, GOLDEN_WIDTH, assert_golden, buffer_to_text};
use crate::ui::render;
use crate::ui::theme::Theme;
use crate::ui::view::ViewState;

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

#[track_caller]
fn pos(value: f64) -> Positive {
    match Positive::new(value) {
        Ok(p) => p,
        Err(e) => panic!("invalid test positive `{value}`: {e}"),
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

// =============================================================================
// 4. The #50 vol-surface + depth render goldens at the fixed 120x40
//    (`docs/TESTING.md` §4). Every one is produced through the REAL full-frame
//    render path — state -> `ViewState::sync` (off the draw path) -> `render` — not
//    a hand-drawn buffer: the vol surface's three views (smile / Greek curve /
//    single-expiry surface heat map, #47), its insufficient-IV empty state, the
//    populated Deribit depth ladder assembled from the committed grouped-book
//    fixture through the REAL `normalize_book` (#48), and the honest
//    depth-unavailable state for a depth-less venue. Deterministic: fixed as-of
//    instants + committed fixture data, no wall clock, no socket (tick 0 fixes any
//    spinner frame).
// =============================================================================

/// A `Char` key press (no modifiers), for cycling the surface view through the
/// production `handle_key` seam.
fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// A merged store seeded from `chain` at the fixed as-of instant — the same seed the
/// live path uses, so the surface/depth builds are byte-stable across machines.
fn seed_store(chain: OptionChain) -> ChainStore {
    ChainStore::seed(
        ChainFetch::new(
            chain,
            ExpirySource::new("BTC", utc(AS_OF), pid("deribit")),
            AliasCatalog::new(),
        ),
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    )
}

/// Render a live `state` (forced onto its screen) as a WHOLE frame through the REAL
/// dispatch: build the [`App`], sync the ui view cache off the draw path
/// ([`ViewState::sync`]), and [`render`] into a fixed [`GOLDEN_WIDTH`]x
/// [`GOLDEN_HEIGHT`] `TestBackend` at `tick_count == 0` (so any spinner frame is
/// fixed), returning the buffer as golden text — the exact #19/#37 golden harness.
#[track_caller]
fn render_live_frame(state: LiveState) -> String {
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Live(state), ThemeChoice::Auto, tx);
    app.mark_drawn();
    let mut view = ViewState::new();
    view.sync(&app);
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| render(&app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("live frame render failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

/// A realistic single-expiry smile chain: a put-skewed IV across seven strikes
/// around a 62 000 spot, each strike fully quoted (so the Greek curve / surface
/// price), mids set — the source for the surface smile / curve / heat-map goldens
/// (#47). The skew is authored (lower strikes carry a higher IV), so the smile reads
/// as a genuine curve rather than a flat line.
fn smile_chain() -> OptionChain {
    const SKEW: [(f64, f64); 7] = [
        (56_000.0, 0.72),
        (58_000.0, 0.66),
        (60_000.0, 0.60),
        (62_000.0, 0.55),
        (64_000.0, 0.58),
        (66_000.0, 0.63),
        (68_000.0, 0.69),
    ];
    let mut chain = OptionChain::new("BTC", pos(62_000.0), "2025-06-27".to_owned(), None, None);
    for (strike, iv) in SKEW {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(3_000.0)),
            call_ask: Some(pos(3_100.0)),
            put_bid: Some(pos(2_000.0)),
            put_ask: Some(pos(2_100.0)),
            implied_volatility: pos(iv),
            ..Default::default()
        };
        od.set_mid_prices();
        let _ = chain.options.insert(od);
    }
    chain
}

/// A Surface-screen live state, Ready, on the view reached by `view_toggles` presses
/// of `x` (`0` = smile, `1` = Greek curve, `2` = single-expiry surface). Cycling
/// through the production [`crate::ui::surface::handle_key`] rebuilds the active
/// geometry off the draw path, exactly as a keystroke would.
fn surface_state(chain: OptionChain, view_toggles: u8) -> LiveState {
    let mut live = LiveState::new(
        SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
        seed_store(chain),
    );
    live.screen = LiveScreen::Surface;
    live.load = ScreenLoad::Ready;
    for _ in 0..view_toggles {
        let _ = crate::ui::surface::handle_key(&mut live, key('x'));
    }
    live
}

/// A one-strike chain whose only strike has no reliable IV (zero IV, unquoted) — the
/// surface screen's deliberate "insufficient IV" empty state.
fn empty_iv_chain() -> OptionChain {
    let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
    let _ = chain.options.insert(OptionData {
        strike_price: pos(60_000.0),
        implied_volatility: Positive::ZERO,
        ..Default::default()
    });
    chain
}

/// A one-strike chain at the grouped-book fixture's strike (60 000), so the depth
/// screen's selected-contract key matches the fixture instrument
/// (`BTC-27JUN25-60000-C`).
fn depth_chain() -> OptionChain {
    let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
    let mut od = OptionData {
        strike_price: pos(60_000.0),
        call_bid: Some(pos(0.05)),
        call_ask: Some(pos(0.06)),
        put_bid: Some(pos(0.04)),
        put_ask: Some(pos(0.05)),
        implied_volatility: pos(0.5),
        ..Default::default()
    };
    od.set_mid_prices();
    let _ = chain.options.insert(od);
    chain
}

/// The `deribit` caps with `depth` cleared — a depth-less venue, so the Depth screen
/// renders its honest "not available" body (the capability gate, never a fabricated
/// ladder).
fn no_depth_caps() -> ProviderCapabilities {
    ProviderCapabilities::builder()
        .chain(ChainCapability::Assemble)
        .depth(false)
        .greeks(GreeksCapability::Provided)
        .build()
}

/// A Depth-screen live state, Ready + depth-capable, with the 60 000 contract focused
/// and the grouped-book fixture ladder folded into the depth store under the SAME key
/// the screen selects — the populated ladder golden. The ladder is assembled from the
/// committed grouped fixture through the REAL [`fixture_btc_depth_ladder`] /
/// `normalize_book`, so the rendered levels are the adapter's output, not hand-drawn.
fn depth_ladder_state() -> LiveState {
    let mut live = LiveState::new(
        SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
        seed_store(depth_chain()),
    );
    live.screen = LiveScreen::Depth;
    live.load = ScreenLoad::Ready;
    live.selection.focused_row = Some(0);
    if let Some(depth_key) = live.selected_depth_key() {
        let _ = live
            .depth_store
            .apply(fixture_btc_depth_ladder(depth_key, utc(AS_OF)));
    }
    live
}

/// A Depth-screen live state on a depth-LESS source — the honest capability-
/// unavailable golden (a first-class deliberate state, `docs/05-views-and-ux.md` §6).
fn depth_unavailable_state() -> LiveState {
    let mut live = LiveState::new(
        SourceBinding::new(pid("deribit"), no_depth_caps(), StreamHealth::Live),
        seed_store(depth_chain()),
    );
    live.screen = LiveScreen::Depth;
    live.load = ScreenLoad::Ready;
    live
}

#[test]
fn test_surface_smile_golden() {
    // The populated vol smile (IV vs strike), the skewed curve reading as a genuine
    // smile — the default Surface view.
    assert_golden(
        "surface",
        "deribit_btc_smile.txt",
        &render_live_frame(surface_state(smile_chain(), 0)),
    );
}

#[test]
fn test_surface_curve_golden() {
    // The Greek/IV/Price curve view (`x` once): the axis Greek vs strike.
    assert_golden(
        "surface",
        "deribit_btc_curve.txt",
        &render_live_frame(surface_state(smile_chain(), 1)),
    );
}

#[test]
fn test_surface_heatmap_golden() {
    // The single-expiry surface heat map (`x` twice): the NO_COLOR-safe glyph ramp of
    // the Greek z over strike x volatility.
    let text = render_live_frame(surface_state(smile_chain(), 2));
    assert!(
        text.contains("top") && text.contains("bottom"),
        "the heat-map header marks the vol-axis direction: {text:?}",
    );
    assert_golden("surface", "deribit_btc_surface.txt", &text);
}

#[test]
fn test_surface_empty_golden() {
    // The deliberate insufficient-IV empty state — a first-class body, never a blank.
    let text = render_live_frame(surface_state(empty_iv_chain(), 0));
    assert!(
        text.contains("insufficient IV"),
        "the empty state names the insufficient-IV cause: {text:?}",
    );
    assert_golden("surface", "surface_empty.txt", &text);
}

#[test]
fn test_depth_ladder_golden() {
    // The populated Deribit ladder assembled from the committed grouped-book fixture.
    //
    // NOTE (known #48 limitation, surfaced here — NOT a #50 change): the ladder's
    // `fmt_num` renders prices at a fixed 2 decimals, which UNDER-RESOLVES the
    // sub-unit BTC option prices this grouped fixture carries — the deeper bids
    // 0.049 / 0.048 render as `0.04` (a 2-decimal truncation, not a round). The
    // golden pins this delivered behaviour faithfully so a future precision fix in
    // `src/ui/depth.rs` (widening depth price precision for sub-unit venues) lands as
    // a visible golden diff; #50 makes no production change.
    let text = render_live_frame(depth_ladder_state());
    assert!(
        text.contains("bid") && text.contains("ask"),
        "the ladder shows both sides with text labels (NO_COLOR-safe): {text:?}",
    );
    assert!(
        text.contains("BTC-27JUN25-60000-C"),
        "the venue instrument names the ladder title: {text:?}",
    );
    assert!(
        text.contains("spread"),
        "the spread footer renders: {text:?}"
    );
    assert_golden("depth", "deribit_btc_ladder.txt", &text);
}

#[test]
fn test_depth_unavailable_golden() {
    // The honest capability-unavailable body for a depth-less venue — deliberate,
    // never a blank or a fabricated ladder.
    let text = render_live_frame(depth_unavailable_state());
    assert!(
        text.contains("not available"),
        "the unavailable body names itself: {text:?}",
    );
    assert_golden("depth", "depth_unavailable.txt", &text);
}
