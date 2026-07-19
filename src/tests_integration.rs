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
//! Section 5 (issue #51) adds the v0.5 **behavioural** acceptance the #50 goldens
//! cannot prove: the depth `change_id` gap → resync flows (driven through the REAL
//! `App::on_event` fan-in fold + render), the coalescing burst → no-frozen-frame,
//! the depth-unavailable capability gate, the vol-smile parity against
//! `optionstratlib`'s own `VolatilitySmile::smile()`, and the surface fallible
//! `Err`/degenerate → honest empty-state render. These need `pub(crate)` internals
//! (`fixture_btc_depth_ladder`, `ui::depth::draw`, `ui::surface::draw`) the public
//! `tests/` crate cannot reach, so they live in-crate beside Section 4's goldens.
//!
//! **Spec-text vs reconciled gap semantics.** The 051 spec's scripted "a delta
//! whose `change_id` skips the expected next value triggers a resync" line predates
//! the #48 reconcile. The Deribit adapter subscribes the GROUPED full-snapshot book
//! channel (`book.{i}.none.20.100ms`), where every frame is a complete aggregated
//! book, so under the RECONCILED model (`src/chain/depth.rs` `depth_continues` + its
//! module docs / the #48 hand-off note) a forward `change_id` **skip is BENIGN** (a
//! coalesced snapshot the channel dropped by design) and stays `Fresh`; only a
//! `change_id` **regression** (venue re-seed) or a **lost** sequence (`Some`→`None`)
//! flips `ResyncNeeded` → the "resyncing" badge + the dimmed ladder. Section 5
//! implements the gap tests per the reconciled model, NOT the stale spec line. The
//! 051 spec's other stale line — "a `CurveError`/`SurfaceError` … renders the
//! insufficient-IV empty state" — is likewise reconciled to the #47 refinement
//! (P3-01): an IV-sparse expiry renders "insufficient IV" (`NoData`), a hard build
//! `Err` renders the DISTINGUISHABLE "degenerate geometry" (`Degenerate`).
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
use optionstratlib::prelude::{Decimal, Positive, VolatilitySmile};
use optionstratlib::visualization::{GraphData, Series2D, Surface3D};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use tokio::sync::mpsc;

use crate::app::{
    App, EventBridge, LiveScreen, LiveState, Mode, ScreenLoad, SourceBinding, is_screen_reachable,
};
use crate::chain::{
    AliasCatalog, ChainFetch, ChainSource, ChainStore, DepthLadder, DepthLevel, DepthStatus,
    ExpirySource, InstrumentKey, MarketUpdate, ProviderId, StreamHealth, depth_continues,
};
use crate::config::ThemeChoice;
use crate::event::{AppEvent, Command};
use crate::providers::deribit::{
    fixture_btc_chain_fetch_named, fixture_btc_depth_ladder, fixture_btc_stream_updates,
};
use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
use crate::ui::chain::draw as draw_chain;
use crate::ui::golden::{GOLDEN_HEIGHT, GOLDEN_WIDTH, assert_golden, buffer_to_text};
use crate::ui::graph::{EmptyReason, GraphProjection, project};
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
    // #109/#118 (closed here): the ladder's `fmt_num` renders each sub-unit BTC option
    // price at its OWN decimal scale, so the deeper bids 0.049 / 0.048 keep their
    // tradeable digits (rendering `0.049` / `0.048`, not the old 2-decimal `0.04`
    // truncation), and two distinct prices near one never collapse to a shared
    // 3-significant-figure string. The golden is regenerated in the same commit as the
    // precision fix (a visible diff), and stays NO_COLOR-safe.
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
    // #109/#118: sub-unit BTC prices keep their tradeable digits (no `0.04` truncation).
    assert!(
        text.contains("0.049") && text.contains("0.048"),
        "sub-unit venue prices render at their own decimal scale, not truncated: {text:?}",
    );
    assert!(
        !text.contains(" 0.04 "),
        "the old 2-decimal truncation is gone: {text:?}",
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

// =============================================================================
// 5. The #51 v0.5 acceptance-gate BEHAVIOURAL flows (`docs/TESTING.md` §7,
//    `docs/03-data-providers.md` §8/§5, ROADMAP §v0.5). Distinct from Section 4's
//    snapshot goldens: these assert BEHAVIOUR — a change_id discontinuity resyncs
//    (no torn book), a book burst never freezes a frame, the depth screen is
//    capability-gated, the vol smile passes `VolatilitySmile::smile()` through, and
//    the surface fallible path degrades to an honest empty state. Every flow drives
//    the REAL fan-in fold (`App::on_event`) and/or the REAL draw; no socket, no wall
//    clock, each far under the 10 s integration bound.
//
//    RECONCILED gap semantics (binding — the spec text is stale, see the module
//    header): the adapter subscribes the GROUPED full-snapshot book channel, so a
//    forward change_id SKIP is a benign coalesced snapshot (stays Fresh); only a
//    change_id REGRESSION (venue re-seed) or a LOST sequence (Some→None) resyncs.
// =============================================================================

/// A depth-capable Live state on the [`LiveScreen::Depth`] screen (Ready), with the
/// 60 000 call contract focused but **no book yet** — the scripted-gap tests fold the
/// book sequence through the real fan-in from this starting point.
fn depth_focused_state() -> LiveState {
    let mut live = LiveState::new(
        SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
        seed_store(depth_chain()),
    );
    live.screen = LiveScreen::Depth;
    live.load = ScreenLoad::Ready;
    live.selection.focused_row = Some(0);
    live
}

/// Wrap a Live `state` in an [`App`] (Live mode) whose command channel needs no
/// runtime — the fan-in fold ([`App::on_event`]) and the pure draw never touch it.
fn depth_app(state: LiveState) -> App {
    let (tx, _rx) = mpsc::channel::<Command>(16);
    App::new(Mode::Live(state), ThemeChoice::Auto, tx)
}

/// A full grouped-snapshot book ladder for `key` at `change_id`, assembled from the
/// committed grouped fixture through the REAL [`fixture_btc_depth_ladder`] /
/// `normalize_book` (so the ladder shape is the adapter's output), with the sequence
/// number scripted.
fn depth_ladder_cid(key: &InstrumentKey, change_id: Option<u64>) -> DepthLadder {
    let mut ladder = fixture_btc_depth_ladder(key.clone(), utc(AS_OF));
    ladder.change_id = change_id;
    ladder
}

/// A grouped-snapshot book ladder for `key` at `change_id` with **distinctive**
/// hand-authored levels (best-first) — used to prove a wholesale snapshot swap, never
/// a delta stitched across a gap.
fn depth_ladder_levels(
    key: &InstrumentKey,
    change_id: Option<u64>,
    bids: &[(f64, f64)],
    asks: &[(f64, f64)],
) -> DepthLadder {
    let mut ladder = fixture_btc_depth_ladder(key.clone(), utc(AS_OF));
    ladder.change_id = change_id;
    ladder.bids = bids
        .iter()
        .map(|&(p, s)| DepthLevel {
            price: pos(p),
            size: pos(s),
        })
        .collect();
    ladder.asks = asks
        .iter()
        .map(|&(p, s)| DepthLevel {
            price: pos(p),
            size: pos(s),
        })
        .collect();
    ladder
}

/// Fold one grouped book frame for `key` at `change_id` through the REAL fan-in
/// (`AppEvent::Market(MarketUpdate::Depth(..))` → `on_market` → `apply_depth` →
/// `DepthStore::apply`).
fn fold_depth(app: &mut App, key: &InstrumentKey, change_id: Option<u64>) {
    app.on_event(AppEvent::Market(MarketUpdate::Depth(depth_ladder_cid(
        key, change_id,
    ))));
}

/// The selected-contract key of a depth state (the tests focus a real strike, so it
/// resolves).
#[track_caller]
fn depth_key(state: &LiveState) -> InstrumentKey {
    match state.selected_depth_key() {
        Some(key) => key,
        None => panic!("the focused depth state resolves a selected contract"),
    }
}

/// Render the Depth screen for `live` through the REAL [`crate::ui::depth::draw`] into
/// the fixed 120x40 backend and return the cloned buffer (for a modifier/dim probe the
/// row-major text projection cannot carry).
#[track_caller]
fn render_depth_buffer(live: &LiveState) -> Buffer {
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| crate::ui::depth::draw(live, frame, frame.area(), theme(), 0)) {
        Ok(_) => {}
        Err(e) => panic!("depth draw failed: {e}"),
    }
    term.backend().buffer().clone()
}

/// The row-major text of a depth-screen buffer.
fn depth_text(live: &LiveState) -> String {
    buffer_to_text(&render_depth_buffer(live))
}

/// Whether any digit-bearing cell in the ladder BODY band (below the `y=1` header,
/// above the always-dim spread footer) is `DIM`-styled — the honest stale/resync dim
/// (#48 P2-01). Mirrors the proven `src/ui/depth.rs` probe: the band `2..height/2`
/// excludes the unconditionally-dim footer so it never false-positives.
fn body_has_dimmed_digit(buffer: &Buffer, height: u16) -> bool {
    let width = buffer.area().width;
    for y in 2..(height / 2) {
        for x in 0..width {
            if let Some(cell) = buffer.cell((x, y))
                && cell.symbol().chars().any(|c| c.is_ascii_digit())
                && cell.modifier.contains(Modifier::DIM)
            {
                return true;
            }
        }
    }
    false
}

/// Render an [`App`] full-frame through the REAL public [`render`] entry (ui view
/// cache synced off the draw path) into the fixed 120x40 backend, as golden text —
/// the render loop's own path.
#[track_caller]
fn render_app_text(app: &App) -> String {
    let mut view = ViewState::new();
    view.sync(app);
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| render(app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("full-frame render failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

// --- 5a. A change_id REGRESSION resyncs: badge + dimmed ladder, no torn book ---

#[test]
fn test_depth_regression_flips_resync_badge_and_dims_ladder() {
    // Seed a grouped snapshot (change_id 10), then fold a REGRESSING frame
    // (change_id 3 — a venue re-seed) through the REAL fan-in. Under the reconciled
    // grouped-snapshot model that is a genuine discontinuity, so the visible book
    // flips ResyncNeeded: the "resyncing" badge renders and the ladder body dims.
    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));
    fold_depth(&mut app, &key, Some(10));
    fold_depth(&mut app, &key, Some(3));

    let live = live_of(&app);
    let book = match live.depth_store.book(&key) {
        Some(b) => b,
        None => panic!("the folded book is retrievable by the selected key"),
    };
    assert_eq!(
        book.status(),
        DepthStatus::ResyncNeeded,
        "a change_id regression flips the visible book to ResyncNeeded",
    );

    let buffer = render_depth_buffer(live);
    let text = buffer_to_text(&buffer);
    assert!(
        text.contains("resyncing"),
        "the resync badge renders through the real draw: {text:?}",
    );
    assert!(
        body_has_dimmed_digit(&buffer, GOLDEN_HEIGHT),
        "the ladder body dims under resync — never a bright, trusted-looking book",
    );
    // Not a frozen/blank frame: the last snapshot is still rendered (badged), so the
    // side labels remain on-screen.
    assert!(
        text.contains("bid") && text.contains("ask"),
        "the dimmed ladder still renders both sides: {text:?}",
    );
}

#[test]
fn test_depth_lost_sequence_flips_resync() {
    // A tracked sequence that goes Some→None (the feed lost its change_id) is a resync
    // boundary too (`depth_continues(Some(_), None) == false`).
    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));
    fold_depth(&mut app, &key, Some(10));
    fold_depth(&mut app, &key, None);

    let live = live_of(&app);
    match live.depth_store.book(&key) {
        Some(book) => assert_eq!(
            book.status(),
            DepthStatus::ResyncNeeded,
            "losing the change_id sequence flags a resync",
        ),
        None => panic!("book present"),
    }
    assert!(
        depth_text(live).contains("resyncing"),
        "the lost-sequence resync badges the ladder",
    );
}

// --- 5b. A forward SKIP is BENIGN (the reconciled model) ----------------------

#[test]
fn test_depth_forward_skip_stays_fresh_and_bright() {
    // THE spec-text divergence, made executable. The 051 spec's stale line says "a
    // delta whose change_id skips the expected next value triggers a resync". Under
    // the RECONCILED grouped-snapshot semantics a forward skip is a benign coalesced
    // full snapshot (the channel dropped intermediate books by design), so it stays
    // Fresh: NO resync badge, and the ladder stays bright (trusted).
    //
    // Bind the reconciled classifier directly so the intent is unambiguous.
    assert!(
        depth_continues(Some(10), Some(50)),
        "a forward skip continues (benign coalesced snapshot) — the reconciled model",
    );
    assert!(
        !depth_continues(Some(10), Some(9)),
        "only a regression is a discontinuity",
    );
    assert!(
        !depth_continues(Some(10), None),
        "only a lost sequence is a discontinuity",
    );

    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));
    fold_depth(&mut app, &key, Some(10));
    fold_depth(&mut app, &key, Some(50)); // a big forward skip

    let live = live_of(&app);
    match live.depth_store.book(&key) {
        Some(book) => assert_eq!(
            book.status(),
            DepthStatus::Fresh,
            "a forward change_id skip stays Fresh — no false resync",
        ),
        None => panic!("book present"),
    }
    let buffer = render_depth_buffer(live);
    let text = buffer_to_text(&buffer);
    assert!(
        !text.contains("resyncing"),
        "a benign skip shows no resync badge: {text:?}",
    );
    assert!(
        !body_has_dimmed_digit(&buffer, GOLDEN_HEIGHT),
        "a benign skip leaves the ladder body bright (only the footer is dim)",
    );
}

// --- 5c. A gap swaps a WHOLE snapshot — never a delta stitched across the gap ---

#[test]
fn test_depth_gap_never_renders_a_torn_book() {
    // The grouped channel delivers a COMPLETE aggregated book every frame; the store
    // overwrites its slot wholesale and never merges a delta. So even across a
    // change_id regression the rendered book is exactly ONE venue snapshot (the latest
    // full frame), badged resyncing — never A's levels stitched onto B's. This is the
    // "never a torn book from a delta applied across the gap" acceptance, proven under
    // the reconciled grouped model (where no delta-merge exists to tear a book).
    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));

    // Frame A: change_id 10, distinctive levels around 60 000.
    app.on_event(AppEvent::Market(MarketUpdate::Depth(depth_ladder_levels(
        &key,
        Some(10),
        &[(60_000.0, 1.0), (59_990.0, 2.0)],
        &[(60_010.0, 1.0)],
    ))));
    // Frame B: a REGRESSING change_id 3, a completely different book around 50 000.
    app.on_event(AppEvent::Market(MarketUpdate::Depth(depth_ladder_levels(
        &key,
        Some(3),
        &[(50_000.0, 3.0)],
        &[(50_010.0, 4.0)],
    ))));

    let live = live_of(&app);
    let book = match live.depth_store.book(&key) {
        Some(b) => b,
        None => panic!("book present"),
    };
    let ladder = book.ladder();
    // The shown book is EXACTLY frame B, wholesale — none of A's levels survive.
    assert_eq!(ladder.bids.len(), 1, "the book is B's complete snapshot");
    assert_eq!(
        ladder.bids.first().map(|l| l.price),
        Some(pos(50_000.0)),
        "the best bid is B's, not A's 60 000 (no stitch)",
    );
    assert_eq!(ladder.asks.first().map(|l| l.price), Some(pos(50_010.0)));
    assert!(
        !ladder.bids.iter().any(|l| l.price == pos(60_000.0)),
        "A's levels are gone — the frame is not torn across the gap",
    );
    assert!(
        book.needs_resync(),
        "the discontinuity is honestly flagged for the badge/dim, not silently trusted",
    );
    // And the rendered ladder shows B's price, badged resyncing.
    let text = depth_text(live);
    assert!(text.contains("50000.00"), "B's best bid renders: {text:?}");
    assert!(text.contains("resyncing"), "badged resyncing");
}

// --- 5d. No-frozen-frame: the render loop keeps producing frames through resync -

#[test]
fn test_depth_resync_state_keeps_rendering_frames() {
    // Through a resync-needed state the full-frame render keeps producing frames —
    // never a stale-bright frame (the badge is present), never a blank or a panic (the
    // #48 ux fix, pinned). Interleaving regressing folds and renders shows the draw
    // path never blocks on the update path: each fold keeps the book discontinuous
    // (a strictly decreasing change_id re-seed), and each render still succeeds.
    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));
    fold_depth(&mut app, &key, Some(1_000)); // seed, Fresh

    for round in 0..5u64 {
        // Each fold regresses from the previous stored change_id → stays ResyncNeeded.
        let cid = 1_000 - (round + 1) * 10; // 990, 980, 970, 960, 950
        fold_depth(&mut app, &key, Some(cid));
        // Advance the tick (the spinner counter) between draws — the loop is alive.
        app.on_event(AppEvent::Tick);
        let text = render_app_text(&app);
        assert!(
            text.contains("resyncing"),
            "round {round}: the resync badge is never dropped for a stale-bright frame: {text:?}",
        );
        assert!(
            text.contains("bid") && text.contains("ask"),
            "round {round}: the ladder still renders (never blank): {text:?}",
        );
    }
}

// --- 5e. A book burst folds without freezing a frame, memory stays bounded ------

#[test]
fn test_depth_burst_through_the_real_bridge_coalesces_latest_value_wins() {
    // The SAME burst driven through the REAL bounded coalescing path - the
    // producer sink staging -> the bounded market channel -> EventBridge::pump ->
    // the fold - instead of direct on_event folds, so a regression in the actual
    // bridge/coalescing machinery is caught here (the #111 review point). Under
    // saturation the coalescing collapses intermediates latest-value-wins: after
    // the pump the store holds ONE bounded book at the NEWEST change_id, and a
    // frame still renders.
    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));

    let (mut bridge, senders) = EventBridge::new(64);
    let mut sink = senders.market_update_sink();
    const BURST: u64 = 512; // deliberately above the channel capacity
    for cid in 1..=BURST {
        // publish_coalesced never blocks: overflow stages latest-value-wins.
        let _ = sink.publish_coalesced(MarketUpdate::Depth(depth_ladder_cid(&key, Some(cid))));
    }
    let _ = sink.flush();
    bridge.pump(&mut app, |_command| {});
    // A saturated burst may leave the freshest value staged behind a full
    // channel; one more flush+pump drains it (the production tick does this).
    let _ = sink.flush();
    bridge.pump(&mut app, |_command| {});

    let live = live_of(&app);
    assert_eq!(
        live.depth_store.len(),
        1,
        "one bounded book after the real-path burst"
    );
    match live.depth_store.book(&key) {
        Some(book) => assert_eq!(
            book.ladder().change_id,
            Some(BURST),
            "coalescing is latest-value-wins: the newest change_id survives the saturation",
        ),
        None => panic!("the burst book exists"),
    }
    let text = render_app_text(&app);
    assert!(
        !text.is_empty(),
        "a frame renders after the real-path burst"
    );
}

#[test]
fn test_depth_burst_folds_without_freezing_a_frame_and_stays_bounded() {
    // A bursty feed folds a stream of grouped snapshots for ONE contract; each fold
    // overwrites the slot latest-value-wins (the coalescing discipline), so the store
    // stays bounded at one book while the render loop keeps producing frames. Proven
    // by interleaving folds and full-frame renders — not a wall-clock latency
    // measurement (that is a BENCH.md concern).
    let mut app = depth_app(depth_focused_state());
    let key = depth_key(live_of(&app));

    const BURST: u64 = 128;
    for cid in 1..=BURST {
        fold_depth(&mut app, &key, Some(cid)); // advancing → stays Fresh
        let text = render_app_text(&app);
        assert!(
            !text.is_empty(),
            "cid {cid}: a frame renders during the burst — the draw never stalls",
        );
    }

    let live = live_of(&app);
    assert_eq!(
        live.depth_store.len(),
        1,
        "the bounded store holds ONE book for the single contract — memory does not grow",
    );
    match live.depth_store.book(&key) {
        Some(book) => {
            assert_eq!(
                book.status(),
                DepthStatus::Fresh,
                "an advancing burst stays Fresh (no false resync)",
            );
            assert_eq!(
                book.ladder().change_id,
                Some(BURST),
                "latest-value-wins: the newest frame is shown",
            );
        }
        None => panic!("book present"),
    }
    assert!(
        !body_has_dimmed_digit(&render_depth_buffer(live), GOLDEN_HEIGHT),
        "a continuous burst renders bright (dim is reserved for the honest resync)",
    );
}

// --- 5f. The Depth screen is capability-gated, never fabricated ----------------

#[test]
fn test_depth_screen_is_capability_gated_and_unavailable_renders_honestly() {
    // Reachability reads the declared caps ONLY (the gate signature takes `&caps`,
    // never a ProviderId): depth:false → unreachable; depth:true → reachable.
    assert!(
        !is_screen_reachable(LiveScreen::Depth, &no_depth_caps()),
        "a depth-less venue cannot reach the Depth screen",
    );
    assert!(
        is_screen_reachable(LiveScreen::Depth, &caps()),
        "a depth-capable venue reaches the Depth screen",
    );

    // `set_screen` honours the gate: a depth-less state on the default Chain screen
    // refuses the switch to Depth, so the screen is genuinely unreachable, not just
    // hidden.
    let mut depthless = LiveState::new(
        SourceBinding::new(pid("deribit"), no_depth_caps(), StreamHealth::Live),
        seed_store(depth_chain()),
    );
    assert_eq!(depthless.screen, LiveScreen::Chain);
    assert!(
        !depthless.set_screen(LiveScreen::Depth),
        "set_screen refuses the unreachable Depth screen (capability-gated)",
    );
    assert_eq!(
        depthless.screen,
        LiveScreen::Chain,
        "the screen never switched to the gated Depth",
    );

    // The defensive draw (if reached) renders the honest state, never a fabricated
    // ladder: no bid/ask side labels, no spread footer.
    let text = render_live_frame(depth_unavailable_state());
    assert!(
        text.contains("not available"),
        "the unavailable body names itself: {text:?}",
    );
    assert!(
        !text.contains("resyncing") && !text.contains("spread"),
        "no fabricated ladder machinery leaks into the unavailable state: {text:?}",
    );
}

// --- 5g. The vol smile passes optionstratlib's VolatilitySmile::smile() through -

/// A parity smile chain: strikes carrying a reliable per-strike venue IV but NO
/// quotes. With no premium the #24 sidecar inverts nothing (its IV clears to `None`),
/// so `build_smile`'s IV overlay resolves each strike to the chain's OWN set IV — the
/// same `(strike, IV)` pairs `OptionChain::smile()` reads. That makes
/// `store.chain().smile()` an EXACT oracle for the panel's built geometry (the panel's
/// only transform, the frozen-`Days` re-stamp, does not touch the smile's inputs).
fn parity_smile_chain() -> OptionChain {
    const SKEW: [(f64, f64); 5] = [
        (56_000.0, 0.72),
        (58_000.0, 0.66),
        (60_000.0, 0.60),
        (62_000.0, 0.55),
        (64_000.0, 0.58),
    ];
    let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
    for (strike, iv) in SKEW {
        let _ = chain.options.insert(OptionData {
            strike_price: pos(strike),
            implied_volatility: pos(iv),
            ..Default::default()
        });
    }
    chain
}

/// The `(x, y)` decimal vectors of a `GraphData::Series` (the smile geometry) — panics
/// on any other shape so a regression cannot slip through as a different variant.
#[track_caller]
fn series_xy(graph: &GraphData) -> (Vec<Decimal>, Vec<Decimal>) {
    match graph {
        GraphData::Series(series) => (series.x.clone(), series.y.clone()),
        other => panic!("expected a smile Series, got {other:?}"),
    }
}

#[test]
fn test_surface_smile_renders_optionstratlib_smile_output() {
    // The parity acceptance (`docs/TESTING.md` §9, ROADMAP §v0.5): ChainView RENDERS
    // `VolatilitySmile::smile()`'s output — it does not recompute or "improve" it. The
    // panel's built smile GraphData (the geometry BEFORE the ratatui projection) must
    // equal `optionstratlib`'s own `chain.smile()` output for a known chain. ChainView
    // deliberately does NOT re-test the smile MATH — only that it passes it through.
    let store = seed_store(parity_smile_chain());
    let live = LiveState::new(
        SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
        store,
    );

    // ChainView's built smile (the Smile view is the panel default).
    let built = live.surface.active_graph_data();
    let (built_x, built_y) = series_xy(built);
    assert!(
        !built_x.is_empty(),
        "the parity chain builds a non-empty smile (not two compared empties)",
    );

    // The upstream oracle: optionstratlib's own smile() over the SAME chain, wrapped by
    // the SAME `GraphData::from` conversion ChainView uses.
    let oracle: GraphData = live.store.chain().smile().into();
    let (oracle_x, oracle_y) = series_xy(&oracle);

    // Bit-exact Decimal parity — both sides pass through the identical `to_dec()` path,
    // so the "stated exactness" is exact equality (a pure pass-through, no recompute).
    assert_eq!(
        built_x, oracle_x,
        "the rendered smile's strike axis is exactly smile()'s x",
    );
    assert_eq!(
        built_y, oracle_y,
        "the rendered smile's IV axis is exactly smile()'s y (pass-through, not recomputed)",
    );
    assert_eq!(
        built_x.len(),
        5,
        "one smile point per reliable strike (the five seeded)",
    );
}

// --- 5h. The surface fallible path degrades to an honest empty state -----------

#[test]
fn test_surface_insufficient_iv_renders_empty_state_off_the_draw_path() {
    // An IV-sparse expiry (one strike, zero IV) has no reliable IV to build from, so
    // `build_smile` yields the empty series and the projection routes to
    // `Empty(NoData)` OFF the draw path — the deliberate "insufficient IV" state, never
    // a fabricated curve.
    let state = surface_state(empty_iv_chain(), 0);
    assert_eq!(
        project(state.surface.active_graph_data()).empty_reason(),
        Some(EmptyReason::NoData),
        "the IV-sparse expiry routes to the insufficient-IV empty projection",
    );
    let text = render_live_frame(state);
    assert!(
        text.contains("insufficient IV"),
        "the empty state names the insufficient-IV cause: {text:?}",
    );
}

/// The production degenerate `Series` sentinel shape (`src/app/surface_build.rs`
/// `degenerate_series`): a single `x`, no `y` (mismatched lengths). A hard
/// `CurveError` produces exactly this, and the #23 adapter projects it to
/// `Empty(Degenerate)`.
fn degenerate_series() -> GraphData {
    GraphData::Series(Series2D {
        x: vec![Decimal::ZERO],
        y: Vec::new(),
        ..Series2D::default()
    })
}

/// The production degenerate `GraphSurface` sentinel shape (`degenerate_surface`): a
/// single `x`, no `y`/`z`. A hard `SurfaceError` produces exactly this.
fn degenerate_surface() -> GraphData {
    GraphData::GraphSurface(Surface3D {
        x: vec![Decimal::ZERO],
        y: Vec::new(),
        z: Vec::new(),
        ..Surface3D::default()
    })
}

/// Render the surface screen for `state` with an injected `projection` through the
/// REAL [`crate::ui::surface::draw`] at 120x40, returning the frame as text.
#[track_caller]
fn render_surface_with(state: &LiveState, projection: &GraphProjection) -> String {
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| {
        crate::ui::surface::draw(state, projection, frame, frame.area(), theme(), 0);
    }) {
        Ok(_) => {}
        Err(e) => panic!("surface draw failed: {e}"),
    }
    buffer_to_text(term.backend().buffer())
}

#[test]
fn test_surface_curve_err_renders_degenerate_state_not_a_corrupt_chart() {
    // A hard `CurveError` build produces the degenerate series sentinel, which the REAL
    // `project()` routes to `Empty(Degenerate)`. The REAL surface draw must render the
    // DISTINGUISHABLE "degenerate geometry" body — never a corrupt chart, never a panic,
    // never folded into the insufficient-IV state (the #47 P3-01 refinement; a spec-text
    // divergence, see the module header).
    let projection = project(&degenerate_series());
    assert_eq!(
        projection.empty_reason(),
        Some(EmptyReason::Degenerate),
        "the curve-Err sentinel projects degenerate geometry",
    );
    let state = surface_state(smile_chain(), 1); // the Curve view
    let text = render_surface_with(&state, &projection);
    assert!(
        text.contains("degenerate geometry"),
        "the Err path renders the distinguishable degenerate-geometry state: {text:?}",
    );
    assert!(
        !text.contains("insufficient IV"),
        "the Err path is NOT folded into the insufficient-IV state (P3-01)",
    );
}

#[test]
fn test_surface_surface_err_renders_degenerate_state_not_a_corrupt_chart() {
    // The 3D analogue: a hard `SurfaceError` (the degenerate surface sentinel) on a
    // non-Volatility axis renders the honest degenerate-geometry state through the REAL
    // draw — never a fabricated heat map.
    let projection = project(&degenerate_surface());
    assert_eq!(
        projection.empty_reason(),
        Some(EmptyReason::Degenerate),
        "the surface-Err sentinel projects degenerate geometry",
    );
    let state = surface_state(smile_chain(), 2); // the Surface view (Delta axis)
    let text = render_surface_with(&state, &projection);
    assert!(
        text.contains("degenerate geometry"),
        "the surface Err path renders the degenerate-geometry state: {text:?}",
    );
}

// =============================================================================
// 6. The #57 cross-screen polish pass: NO_COLOR golden pair + color-stripping
//    proof, the cross-screen too-small state, the surface stale badge, and the
//    mode-correct Live retry key on every error state (`docs/05-views-and-ux.md`
//    §6, §7, §8). Every render goes through the REAL draw path at the fixed
//    golden size (or an explicit small size for the too-small state), tick 0.
// =============================================================================

/// Render the chain BODY (the flagship screen) into the fixed golden backend under a
/// resolved theme (`no_color` on/off), returning the raw `Buffer` so a test can probe
/// BOTH the row-major symbols (marker parity) and the per-cell styles (color
/// stripping).
#[track_caller]
fn render_chain_buffer_themed(live: &LiveState, no_color: bool) -> Buffer {
    let theme = Theme::resolve(ThemeChoice::Auto, no_color);
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| draw_chain(live, frame, frame.area(), theme, 0, utc(AS_OF))) {
        Ok(_) => {}
        Err(e) => panic!("chain themed draw failed: {e}"),
    }
    term.backend().buffer().clone()
}

/// Whether ANY cell in the buffer sets a non-default foreground or background color —
/// the render-edge probe that distinguishes a colored frame from a `NO_COLOR` one
/// (which resolves every semantic style to intensity/markers only, leaving `Reset`).
fn any_cell_colored(buffer: &Buffer) -> bool {
    buffer
        .content()
        .iter()
        .any(|cell| cell.fg != Color::Reset || cell.bg != Color::Reset)
}

#[test]
fn test_chain_no_color_golden_pair_is_marker_legible_and_color_is_stripped() {
    // The NO_COLOR golden PAIR (`docs/05-views-and-ux.md` §7): the flagship chain matrix
    // rendered with color and with NO_COLOR. The bytes (symbols) are IDENTICAL — every
    // color-encoded state (the ◀ATM marker, the ▲/▼/· tick glyphs, the `~` computed-Greek
    // glyph, the stale badge) is carried by a glyph/text marker, never color alone — so
    // the UI is fully legible with markers + intensity only. A style-level probe then
    // proves color is GENUINELY stripped (not that both happen to match): the color
    // buffer sets a semantic foreground, the NO_COLOR buffer sets none.
    let store = ChainStore::seed(
        fixture_btc_chain_fetch_named("BTC"),
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    );
    let live = live_ready(store, pid("deribit"));

    let color = render_chain_buffer_themed(&live, false);
    let no_color = render_chain_buffer_themed(&live, true);
    let color_text = buffer_to_text(&color);
    let no_color_text = buffer_to_text(&no_color);

    // The golden pair — both committed (byte-identical, which IS the marker-parity proof).
    assert_golden("chain", "deribit_btc_atm.txt", &color_text);
    assert_golden("chain", "deribit_btc_atm_no_color.txt", &no_color_text);
    assert_eq!(
        color_text, no_color_text,
        "NO_COLOR carries no glyph the color mode lacks: the visible marker set is identical",
    );

    // Color is genuinely stripped under NO_COLOR (intensity + markers only).
    assert!(
        any_cell_colored(&color),
        "the color render sets a semantic foreground somewhere",
    );
    assert!(
        !any_cell_colored(&no_color),
        "NO_COLOR strips every foreground/background color (falls back to markers + intensity)",
    );
}

#[test]
fn test_too_small_terminal_golden_widen_hint() {
    // The cross-screen too-small state (`docs/05-views-and-ux.md` §8): below the minimum
    // size EVERY screen shows the "widen the terminal" hint through the REAL render
    // dispatch — never a corrupt layout, never a panic. A first-class state with its own
    // golden at an explicit small size (the golden harness handles any size).
    let store = ChainStore::seed(
        fixture_btc_chain_fetch_named("BTC"),
        ChainSource::Merged,
        Duration::from_secs(2),
        utc(AS_OF),
    );
    let live = live_ready(store, pid("deribit"));
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Live(live), ThemeChoice::Auto, tx);
    app.mark_drawn();
    let mut view = ViewState::new();
    view.sync(&app);
    let mut term = terminal(30, 6);
    match term.draw(|frame| render(&app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("too-small render failed: {e}"),
    }
    let text = buffer_to_text(term.backend().buffer());
    assert!(
        text.contains("widen the terminal"),
        "the too-small state names its recovery: {text:?}",
    );
    assert_golden("common", "too_small.txt", &text);
}

/// A Surface-screen state on the populated smile chain, Ready, whose stream health is
/// **stale** — the dropped-stream state the surface must badge (never blank). `toggles`
/// selects the view (`0` smile / `1` Greek curve / `2` single-expiry heat surface).
fn surface_stale_state(toggles: u8) -> LiveState {
    let mut live = surface_state(smile_chain(), toggles);
    live.source.health = StreamHealth::Stale { since: utc(AS_OF) };
    live
}

/// The heat-map ramp glyphs (`src/ui/surface.rs` `RAMP`) — used to probe whether the
/// 3D surface's heat GRID (not its dim legend/footer) carries color.
const RAMP_GLYPHS: [char; 7] = ['·', ':', '+', '*', '#', '%', '@'];

/// Whether any ramp-glyph cell in the buffer carries a non-default foreground — the
/// probe that distinguishes a **bright** (live, intensity-tinted) heat grid from a
/// **dimmed** (stale) one, where every present cell resolves to the color-less dim
/// style (`src/ui/surface.rs` `cell_span`, #57 P2-01).
fn any_ramp_cell_colored(buffer: &Buffer) -> bool {
    buffer.content().iter().any(|cell| {
        cell.symbol()
            .chars()
            .next()
            .is_some_and(|c| RAMP_GLYPHS.contains(&c))
            && cell.fg != Color::Reset
    })
}

/// Render a live `state` as a WHOLE frame through the REAL dispatch into the fixed
/// golden backend, returning the raw `Buffer` (for a per-cell style probe the text
/// projection cannot carry). Mirrors [`render_live_frame`].
#[track_caller]
fn render_live_buffer(state: LiveState) -> Buffer {
    let (tx, _rx) = mpsc::channel::<Command>(16);
    let mut app = App::new(Mode::Live(state), ThemeChoice::Auto, tx);
    app.mark_drawn();
    let mut view = ViewState::new();
    view.sync(&app);
    let mut term = terminal(GOLDEN_WIDTH, GOLDEN_HEIGHT);
    match term.draw(|frame| render(&app, &view, frame)) {
        Ok(_) => {}
        Err(e) => panic!("live buffer render failed: {e}"),
    }
    term.backend().buffer().clone()
}

#[test]
fn test_surface_stale_golden() {
    // #57: a dropped stream never blanks the surface — the last-known smile renders with
    // the stream-health badge in the title (glyph + text, NO_COLOR-safe), the §6 stale
    // state the surface previously lacked. The golden pins the deliberate stale render.
    let text = render_live_frame(surface_stale_state(0));
    assert!(
        text.contains("stale"),
        "the surface title badges the dropped stream: {text:?}",
    );
    assert!(
        text.contains("strike"),
        "the last-known smile still renders (never blanked): {text:?}",
    );
    assert_golden("surface", "deribit_btc_smile_stale.txt", &text);
}

#[test]
fn test_surface_heat_view_stale_golden_dims_the_grid() {
    // #57 P2-01: the THIRD surface view (the 3D single-expiry heat map) must also dim on
    // a dropped stream — not just the 2D smile/curve. The stale heat render badges the
    // title and dims every present heat cell (the ramp glyph shape still carries the
    // structure, NO_COLOR-safe), so the grid never reads bright/trusted under `◐ stale`.
    let stale_text = render_live_frame(surface_stale_state(2));
    assert!(
        stale_text.contains("stale"),
        "the heat-view title badges the dropped stream: {stale_text:?}",
    );
    assert_golden("surface", "deribit_btc_surface_stale.txt", &stale_text);

    // bright → dimmed proof: the LIVE heat grid tints high-intensity cells (a colored
    // ramp cell exists); the STALE grid resolves every cell to the color-less dim style.
    let live_grid = render_live_buffer(surface_state(smile_chain(), 2));
    let stale_grid = render_live_buffer(surface_stale_state(2));
    assert!(
        any_ramp_cell_colored(&live_grid),
        "the live heat grid tints high-intensity cells (bright)",
    );
    assert!(
        !any_ramp_cell_colored(&stale_grid),
        "the stale heat grid dims every cell — no colored ramp cell survives",
    );
}

#[test]
fn test_live_error_states_offer_the_r_reconnect_key_not_reload() {
    // #57: every Live error state offers the MODE-CORRECT retry key — Live → `r`
    // (reconnect/refetch) — across the chain, depth, and surface screens. A Live provider
    // error must never tell the user to press the Replay `R` bundle-reload key (§6).
    let store = || {
        ChainStore::seed(
            fixture_btc_chain_fetch_named("BTC"),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(AS_OF),
        )
    };
    for screen in [LiveScreen::Chain, LiveScreen::Depth, LiveScreen::Surface] {
        let mut live = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store(),
        );
        live.screen = screen;
        live.load = ScreenLoad::Error {
            message: "provider unreachable".to_owned(),
        };
        let text = render_live_frame(live);
        assert!(
            text.contains("press r to reconnect"),
            "the {screen:?} Live error offers the `r` reconnect key: {text:?}",
        );
    }
}
