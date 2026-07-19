# Changelog

All notable changes to `chainview` are documented in this file.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(the full versioning policy lives in the design docs, local until v0.1.0).

> **Status:** `v0.0.1` is a crates.io name reservation — no implementation
> code exists yet. The first real entries land with the v0.1 work from the
> roadmap (local during the design phase).

## [Unreleased]

### Added

- The Deribit `ticker`/`book` streaming overlay and the adapter-owned
  reconnect/resubscribe loop (`src/providers/deribit.rs`, issue #16;
  `docs/03-data-providers.md` §7.1, §5, `docs/01-domain-model.md` §5, §7,
  `docs/02-tui-architecture.md` §5). `Provider::subscribe` now opens the live
  overlay — it replaces the #15 `Unsupported` stub. Key behaviours:
  - **Ticker + book normalization at the seam.** `ticker.{instrument}`
    normalizes into a `QuoteUpdate` (bid/ask/last/sizes, checked at the `f64`
    seam — a crossed quote drops bid/ask, keeping the prior) **and** a
    `GreeksRow` (venue delta/gamma + percentage-form IV divided by 100);
    `book.{instrument}.{group}` normalizes into a `DepthLadder` with the upstream
    `change_id` captured for later gap-detect/resync, best-first levels, and
    per-level `f64` checks that drop an invalid level without dropping the ladder.
    Both the aggregated `[price, amount]` and raw `[action, price, amount]` book
    encodings decode. **Streamed theta/vega/rho are deliberately discarded**
    (`docs/01` §7) — not even deserialized; the `GreeksRow` always emits `None`
    for them. Raw `deribit-websocket` notification DTOs never leave the adapter.
    **`trades.` is not subscribed** (the tape is deferred), so `MarketUpdate`
    carries no trade event.
  - **Producer-side overwrite-on-full staging — completes the two-stage
    coalescing.** The adapter keeps a per-`InstrumentKey` `ProducerStaging` map
    (one slot per instrument, the latest of each kind held independently) and,
    when the bounded `mpsc::Sender<MarketUpdate>` is **full**, **overwrites the
    staged slot with the newest value** — reserving a channel slot *before*
    taking the staged value, so a full channel never drops it — and flushes on
    space. This is the producer mirror of #10's consumer `EventBridge`,
    completing the NFR-15 latest-value-wins guarantee under sustained saturation.
    The map is O(N subscribed) and reuses its allocation.
  - **Adapter-owned reconnect/resubscribe loop.** `deribit-websocket` (0.3.1)
    ships no auto-reconnect, so ChainView drives it behind the
    `SubscriptionHandle`: connect → resubscribe the `ticker`/`book` channels →
    drain updates; on a drop it emits `Health(id, Reconnecting{attempt})` —
    control-class updates (`Health`/`Chain`) are **await-sent** (never
    coalesced/dropped) on the **one** bounded `mpsc::Sender` the port provides,
    while coalesced-class updates use overwrite-on-full staging on the same
    sender; the single-sender port cannot physically separate a control channel,
    so the true two-class priority drain is the consumer bridge's concern and the
    port→bridge two-sender routing is reconciled at the composition seam (#22, per
    ADR-0009). It backs off with jittered exponential backoff
    (`BASE = 250 ms`, `MAX = 30 s`, `jitter ∈ [-0.2, 0.2]`, `attempt` reset to 0
    on a successful (re)subscribe — never a hot-loop, respecting the upstream
    token-bucket limiter), then **re-`fetch_chain`** (#15) to reconcile drift and
    resubscribes off the **fresh** `ChainFetch.aliases` (backfill = current
    state, no bare `OptionChain`, no symbol re-derivation; a fresh `Chain`
    snapshot is emitted to reconcile structure). Cancellation (handle drop) is
    observed at every `.await` via a `biased` `select!`, so the loop never opens
    a socket after cancellation; dropping the handle cancels the token and aborts
    the task (no fire-and-forget). `install_default_crypto_provider()` installs
    the rustls provider once before the first WS TLS handshake.
  - **Backoff as a pure, injectable-jitter kernel.** `backoff_delay(attempt,
    jitter)` is pure — the jitter is **injected** (the loop samples it from the
    process clock; tests pass a fixed value) — so the formula, the bounds (never
    above `MAX * 1.2` = 36 s, never below `BASE * 0.8` = 200 ms), the jitter
    range, and the `attempt = 0` → `BASE` reset are unit-tested with **no**
    wall-clock wait and no unseeded RNG in the kernel.
  - **`AliasCatalog::instruments()`** (new, `src/chain/fetch.rs`) enumerates
    every feed alias so the reconnect resubscribe walks the **fresh** aliases
    without re-deriving symbols from strikes.
  - Tested with CONSTRUCTED payloads, NO real socket, NO wall clock: the backoff
    kernel (bounds / jitter range / reset), ticker → `QuoteUpdate`/`GreeksRow`
    (incl. discarding theta/vega/rho and the percentage-form IV), book →
    `DepthLadder` with `change_id` (both level encodings), the producer staging
    (overwrite-on-full + flush-on-space + O(N) bound + closed-channel), frame
    routing (ticker/book publish, a ticker channel with a trailing interval
    suffix still routing to the right key, unknown-symbol guard, non-subscription
    / malformed frames ignored), the reconnect backfill snapshot, and property
    tests that `backoff_delay` / `normalize_ticker` / `normalize_book` are total
    (a malformed payload is a valid update or a dropped field, never a panic).
    The `subscribe` test spawns the loop on a current-thread runtime and drops
    the handle before it is polled, so no socket opens. The full mock-transport
    lifecycle (socket close/error/resubscribe/saturation/lag/shutdown) lands in
    #17. 27 new deribit tests plus the `AliasCatalog::instruments()` test in
    `src/chain/fetch.rs`.
- Deribit normalization fixtures and mock-transport lifecycle tests
  (`src/providers/deribit.rs`, `tests/fixtures/deribit/`, issue #17;
  `docs/TESTING.md` §5, §9, `docs/03-data-providers.md` §5, §3). Adds the
  recorded fixture corpus and the deterministic reconnect-lifecycle coverage #16
  deferred — **no real socket, no wall clock**. Key pieces:
  - **A minimal transport seam so the loop is testable.** The #16 reconnect loop
    reached straight for `DeribitWebSocketClient`, so a crate-internal
    `DeribitTransport` trait (private — the public `Provider` API is unchanged)
    now lifts the three impure loop operations (connect + subscribe, receive a
    frame, re-`fetch_chain` for the backfill) behind one seam. The production
    `LiveTransport` wraps the upstream WebSocket client plus the REST backfill,
    exactly as before; a test `MockTransport` yields scripted frames/errors and
    records connects/refetches/subscription-sets. No raw upstream DTO crosses the
    seam, and #16's tests stay green.
  - **Constructed-to-wire-shape fixtures** under `tests/fixtures/deribit/`
    (`include_str!`-baked, so byte-stable across machines): `instruments_btc`,
    `ticker_normal`, `book_snapshot`, `book_delta`, plus degraded shapes —
    zero-bid, crossed (`ask < bid`), negative, non-finite, and a
    missing-strike/unknown-style payload. Each is pinned to `deribit-http` 0.7.1 /
    `deribit-websocket` 0.3.1 (recorded in `docs/specs/providers.md` §0). JSON
    carries no `NaN`/`Inf` literal, so the non-finite fixture uses a non-numeric
    string field the adapter refuses at deserialization (the frame is dropped, no
    fabricated value); the `f64` `NaN`/`Inf` guards themselves stay covered by the
    property tests.
  - **Fixture → `OptionChain` / update assertions.** Each fixture normalizes to
    its recorded chain/update: the instrument list assembles a two-strike chain
    (perpetual filtered, IV / 100 reaches the leg), the ticker normalizes to a
    `QuoteUpdate` + `GreeksRow` (theta/vega/rho discarded), the book to a
    `DepthLadder` with `change_id`; the degraded fixtures prove a
    crossed/zero/negative field outcome and a row-fatal `Normalize` reject with no
    panic and no fabricated value.
  - **Mock-transport lifecycle tests (a)–(f):** socket close and stream error →
    `Health(Reconnecting)` + no panic; resubscribe → the reconnect re-issues
    `fetch_chain` **and** resubscribes off the fresh aliases (the new 61000-C leg
    appears), with the backoff **attempt reset-on-success** asserted at the loop
    level (both reconnects surface `attempt: 1`) — the assertion #16 deferred;
    saturation → a burst far beyond a cap-1 channel keeps the producer staging
    O(N instruments) (flat memory); lag → a slow consumer still receives the
    await-sent `Health`/`Chain`; shutdown → dropping the real `SubscriptionHandle`
    stops the loop. All run under `#[tokio::test(start_paused = true)]` with
    scripted frames and virtual-clock drains, deterministic and well under 10 s.
  - **Fixture corpus as a property seed.** The committed fixtures also feed a
    totality test (each normalizes to a valid update or a typed reject, never a
    panic), complementing #16's `normalize_total` property tests.
  - 16 new deribit tests (10 fixture-normalization + 6 lifecycle); the transport
    seam refactor keeps all 65 existing #16 deribit tests green.
- **`deribit-websocket` `0.3.1`** (`[dependencies]`, issue #16) — the upstream
  Deribit WebSocket client ChainView wraps for the streaming overlay; the
  JSON-RPC 2.0 over WebSocket protocol lives upstream and is never reimplemented
  here.
  - **Audit note (supply-chain).** An explicit-approval dependency addition
    (CLAUDE.md "Coding Rules"). Delta over #15's `deribit-http`: adds
    `tokio-tungstenite` (WS framing) and the default `rustls-aws-lc` TLS backend
    — `rustls` + the `aws-lc-rs` crypto provider, installed **once** via
    `install_default_crypto_provider()` before the first WS TLS handshake (this
    differs from #15, where the REST client used `reqwest`'s default TLS and
    needed no provider install) — plus `futures-util`. It shares `tokio`
    (feature-unified toward `full`), `serde`/`serde_json`, `url`, and `dotenv`
    with the existing tree; `aws-lc-rs` requires a C/ASM toolchain at build time.
    `RUSTSEC`-clean at this revision. The public data path needs no credential
    and logs none; the public endpoints send none.
- The Deribit adapter chain assembly, normalization, and honest capabilities
  (`src/providers/deribit.rs`, issue #15; `docs/03-data-providers.md` §7.1, §3,
  §8, ADR-0003) — the zero-config, public-data poll leg and the first provider
  wired end-to-end. Key behaviours:
  - **Chain assembly from an instrument list (no chain endpoint).** `fetch_chain`
    wraps `deribit-http` `get_instruments(currency, "option")` for structure and
    `get_ticker(instrument)` for mark/IV/Greeks, filters to the requested expiry
    day, and assembles one `optionstratlib::OptionChain` — call and put at each
    strike collapse into one `OptionData` row — returning the named `ChainFetch`
    with its per-leg `AliasCatalog` (native `instrument_name` + the Deribit
    `ContractSpecFingerprint`) and absolute-UTC `ExpirySource`. Per-contract
    tickers are hydrated with **bounded concurrency** (a `tokio::task::JoinSet`
    capped at 16 in-flight requests), so a large expiry meets the
    startup-to-first-chain budget without a sequential round-trip per instrument
    and without hammering the venue rate limiter (ADR-0007,
    `docs/06-performance.md`); assembly stays order-independent (grouped by
    strike). A per-ticker failure degrades that leg only, never the whole chain.
    `discover` lists the venue's currencies as underlyings. Public data needs
    **no credentials** (the adapter drives `HttpConfig::production()`), so it is
    the zero-config default.
  - **Field-specific numeric normalization at the `f64` seam.** Prices/IV/sizes
    become `Positive`, Greeks become `Decimal`, each checked before it enters the
    domain (CLAUDE.md "Governance precedence" item 2): Deribit IV is
    percentage-form and divided by 100 (`49.22` → `0.4922`); a zero bid is a real
    zero; a zero ask on a non-zero bid or any `ask < bid` is crossed and rejects
    the whole quote; a `NaN`/`Inf`/negative price/IV/Greek is dropped, never
    becoming a fabricated value; and only a payload that cannot yield a valid
    strike/style/expiry rejects the row as a typed `ProviderError::Normalize`
    naming the field.
  - **Symbol + direct-UTC expiry mapping.** A Deribit `instrument_name`
    (`BTC-27JUN25-60000-C`) maps to the provider-agnostic `InstrumentKey`; expiry
    is the direct UTC instant from the instrument's millisecond timestamp (or the
    `DDMMMYY` date code resolved to 08:00 UTC settlement), never a relative
    offset. Upstream errors map to a redaction-safe `ProviderError` by category
    only — no URL, body, or token is interpolated.
  - **Honest capabilities + honest streaming stub.** `capabilities()` matches the
    §8 Deribit row exactly (`chain: Assemble`, `depth: true`, `greeks: Provided`,
    `option_stream: ChainQuotes { verified: false }`, `underlying_stream: true`,
    `chain_poll: Poll`, `trades_tape: false`, `auth: None`). `subscribe` returns
    `Unsupported` — the streaming overlay + reconnect loop is issue #16. Raw
    `deribit-http` DTOs never leave the adapter.
  - **Registered through `with_builtins`.** `ChainViewAppBuilder::with_builtins`
    now registers the real Deribit adapter under the reserved `deribit` id (via a
    new `register_builtin` helper that expects the reserved id), so
    `builder().with_builtins().run()` resolves the Deribit live source instead of
    reporting an empty registry.
- **`deribit-http` `0.7.1`** (`[dependencies]`, issue #15) — the upstream Deribit
  REST client ChainView wraps for the poll leg; the JSON-RPC-over-HTTP protocol
  lives upstream and is never reimplemented here.
  - **Audit note (supply-chain).** An explicit-approval dependency addition
    (CLAUDE.md "Coding Rules"). It pulls `reqwest` (with its default TLS),
    `tokio` (feature-unified toward `full`), `serde_json`, request-signing crates
    (`hmac`/`sha2`/`base64`, unused on the public path), `url`, `serde_with`,
    `rand`, and `dotenv`. No `rustls` crypto-provider install is needed — the
    HTTP client relies on `reqwest`'s default TLS and exposes no
    `install_default_crypto_provider`; any provider install belongs to the
    websocket path (issue #16), and no live TLS handshake runs in the test suite.
    `deribit-websocket` (streaming) is a separate, deferred addition (#16). The
    public data path requires no credential and logs none; the public endpoints
    send none.
- The single-source keybinding map, the modal help overlay, the auto/dark/light
  theme, the truthful status bar / keybar, and the `NO_COLOR` fallback
  (`src/app/keymap.rs` + `src/ui/theme.rs`, issue #14; `docs/05-views-and-ux.md`
  §3, §7, §8). The terminal layer's interaction + accessibility seam. Key
  behaviours:
  - **One map both dispatch and the overlay read, so they cannot drift.** `KEYMAP`
    is a single declarative `(key, context, action)` table living in the
    **application** layer (`src/app/keymap.rs`, pure data + resolution, no
    `ratatui`). The key dispatch reads it — `App::dispatch_key_global` resolves
    globals through `keymap::resolve_global` (a `GlobalCommand`), and
    `ui::replay::handle_key` resolves scrub keys through `keymap::resolve_replay` —
    and the ui help overlay (`src/ui/theme.rs`) is **generated from the same table**
    via `keymap::help_sections` (`ui → application`, the mandated direction), so a
    key that does something appears in the overlay by construction. A cross-check
    test proves every dispatched global/replay chord is documented (a key not in the
    overlay is a 🔴). The full v0.1 table lands: `q`/`Ctrl-C`, `?`, `1`–`4`,
    `Tab`/`S-Tab`, `r`, `R`, the chain/depth/surface/payoff/replay keys — keys whose
    bodies land later (chain #18, surface/payoff v0.2, depth v0.5, replay
    playback/speed/fill v0.3) are declared now with their context and resolve to a
    documented no-op placeholder. No key is handled outside the map.
  - **Modal help precedence, ordered correctly.** While the overlay is open the
    dispatch honors only `?`/`Esc` (both close it); every other key is swallowed —
    including keys **outside** the keymap vocabulary (F-keys, PageUp/Down,
    Insert/Delete), because the modal intercept runs **before** the vocabulary check,
    so no key can reach the hidden screen behind the overlay. `Ctrl-C` is the one
    documented carve-out: it stays a hard terminal-interrupt quit even behind the
    modal.
  - **Two-column help overlay, readable on 80x24.** The overlay is laid out in two
    height-balanced columns (globals + a screen group per side) generated from the
    map, so every documented key — including the last (Payoff) section — is visible
    on a standard trader terminal instead of clipping off the bottom; help text is
    terse so nothing truncates mid-phrase. Every screen is listed even when it has no
    bindings yet: the v0.5 replay Payoff screen appears as a titled section with a
    "not available yet" note rather than being dropped.
  - **Reachability skip / one-line hint, capability-driven, never a `ProviderId`
    match.** `Tab`/`S-Tab` cycle only the reachable screens for the active
    mode+provider (reading `is_screen_reachable` / `is_replay_screen_reachable`), so
    they never land on an unavailable body; an unavailable number key flashes a
    transient keybar hint ("Depth not available on deribit" / "Payoff is v0.5") and
    does **not** switch, so `App.screen` stays reachable and `render` (#13) stays
    total. The hint decays on the next key or after `HINT_TICKS` ticks (~2 s), so it
    is never a near-zero flash.
  - **`ThemeChoice` (Auto/Dark/Light) resolution + `NO_COLOR` fallback; color is
    never the only signal.** `Theme::resolve` maps the choice to a variant painted
    from the 16 ANSI-named colors (legible on both dark and light terminals, zero
    config). Every color-encoded state pairs color with a color-independent
    marker — `◀ATM` at-spot, `+`/`−` P&L sign, `▲`/`▼`/`·` tick direction, glyph +
    text health badges — and when `NO_COLOR` is set the `Theme` drops every
    foreground/background color and keeps only intensity + the markers (asserted by
    tests that the rendered span carries no `fg`/`bg` but the glyph survives).
  - **Truthful one-line status bar + generated keybar, tick-driven animation.** The
    status bar shows provider / health badge / mode plus a braille spinner in motion
    states (loading / reconnecting / playing), driven by an `App.tick_count`
    advanced on every tick and read **purely in `draw`** — never a wall-clock read.
    A tick sets `dirty` **iff** the app is in a motion state (`App::is_in_motion`) or
    a hint decayed, so the spinner actually animates during the initial connect /
    reconnect / playback while a truly idle, non-motion app still parks and never
    redraws on a tick. Venue/user strings on the status line are stripped of
    control/escape characters at the render edge.
  - **Responsive chain column-drop order + cross-screen too-small guard.**
    `greek_columns_for_slots` fixes the drop **order** `Γ → ν → Θ` (Θ retained
    first, Γ dropped first) for the chain matrix (the columns themselves land in
    #18); below the minimum size (`MIN_WIDTH`×`MIN_HEIGHT`), `render` shows the
    cross-screen "widen the terminal" state instead of a corrupt layout, on any
    screen.
  - **Layering respected: keymap in the application layer, rendering in ui.** The
    ratatui-free keymap data + resolution (`KeyChord`/`Context`/`Action`/`Binding`/
    `KEYMAP`, `GlobalCommand`/`resolve_global`/`resolve_replay`/`help_bindings`)
    lives in `src/app/keymap.rs`; the `ratatui`-dependent rendering (`Theme`,
    `StrikeRelation` + its marker spans, `GreekColumn`/`greek_columns_for_slots`,
    the markers/spans, `MIN_WIDTH`/`MIN_HEIGHT`/`is_too_small`, and the status/keybar/
    overlay renderers) stays in `src/ui/theme.rs`. So `ui → application` holds and no
    application/domain/provider module imports `ui` — the single-source-of-truth
    guarantee is preserved with dispatch and overlay reading one table. Both surfaces
    are re-exported from the crate root (keymap from `app::keymap`, theme from
    `ui::theme`) for the chain matrix (#18) and the render goldens (#19). `App` gains
    `no_color`, `tick_count`, a transient `status_hint` with a `hint_ttl`, and an
    `is_in_motion` predicate; `App::dispatch_key_global` is refactored to read the map
    without breaking #9's / #13's tests. **No new dependency.** Tests: 5 in
    `src/app/keymap.rs` (map↔overlay cross-check for globals and replay, screen-switch
    slot binding, unmapped-key, chord normalization) + 19 in `src/ui/theme.rs`
    (`NO_COLOR` strips color but keeps every marker, theme resolution, `StrikeRelation`
    K/S bucketing, the `Γ→ν→Θ` drop order, the too-small guard + the widen state
    through `render`, the number-key hint / `Tab` skip, modal precedence, the overlay
    fitting 80x24 with every section, and the deferred replay-Payoff listing) + the
    dispatch/tick regressions in `src/app.rs` (out-of-vocab modal swallow, motion-tick
    animates while idle-tick parks, hint decays after N ticks) — 325 lib tests total.
- The synchronous render loop, the pure total draw dispatch, and the two-level
  key input (`src/ui/mod.rs`, `src/ui/driver.rs`, `src/ui/{chain,depth,surface,payoff,replay}.rs`,
  issue #13; `docs/02-tui-architecture.md` §7, §8, §9, §12,
  `docs/05-views-and-ux.md` §2, §8). The terminal layer's render seam: a draw path
  that is a pure function of app state, an event-driven loop that parks and redraws
  only when dirty, and the tick/input task seams the supervisor (#11) owns. Key
  behaviours:
  - **`render(&App, &mut Frame)` is pure and the dispatch is total, wildcard-free.**
    `render` takes `&App` (never `&mut`), so a draw cannot mutate state or perform
    I/O — the purity guarantee is enforced by the signature. It lays out the root
    (`layout_root`: status bar + body + hint line, via `Layout::areas` so there is
    no unchecked index and a zero-size area yields empty regions, never a panic),
    draws a minimal status placeholder, then the **mode-first, screen-exhaustive**
    match with **no `_` arm** — `Mode::Live(s) => match s.screen { Chain | Depth |
    Surface | Payoff }`, `Mode::Replay(s) => match s.screen { Replay | Payoff }` —
    then the help overlay when open. Adding a screen variant forces the matching
    mode arm to be revisited by the compiler.
  - **Screen-shaped module boundary.** Each screen (`chain`/`depth`/`surface`/
    `payoff`/`replay`) exposes a pure `draw(&State, &mut Frame, Rect)` and
    `handle_key(&mut State, KeyEvent) -> Option<AppEvent>` with honest placeholder
    bodies (a titled block — the real chain matrix is #18, the others v0.2/v0.3/
    v0.5); no I/O, no `.await`, no `GraphData` build in `draw`. The replay screen's
    `handle_key` demonstrates the seam: a scrub key returns `AppEvent::ReplaySeek`
    the loop folds back, so the widget emits an event rather than seeking inline.
  - **Event-driven render loop, redraw only when dirty.** `run_render_loop` runs on
    a dedicated blocking thread and **parks** on the bounded `AppEvent` channel via
    `blocking_recv` — no busy-poll. Per event it folds it (two-level key dispatch),
    **pumps the #10 `EventBridge` between frames** (draining coalesced quotes/Greeks/
    depth + the priority control channel and routing commands), and redraws **only
    when `App::dirty`**, clearing `dirty` after the draw; it breaks on
    `App::should_quit` and returns when the channel closes. The tick (default ~250 ms
    from `config.tick_interval`) is the bridge's flush cadence, so market updates are
    folded at least every tick with zero spinning.
  - **Two-level key dispatch, closed sets wildcard-free.** `App::dispatch_key_global`
    handles the globals (`q`/`Ctrl-C` quit, `?` help, `r` reconnect, `R` rediscover)
    and the **modal-help intercept** (only `?`/`Esc` close it; every other key is
    swallowed, never reaching the screen behind the overlay), returning a `KeyRoute`
    (`Consumed`/`ToScreen`); a `ToScreen` key is forwarded to the active screen's
    `handle_key`, whose follow-on `AppEvent` is folded back. Both the `AppEvent` fold
    and the mode→screen forwarding are exhaustive with no `_` arm (crossterm
    `KeyCode`/`Event` are the only open vocabularies). Extends `App::on_key` without
    breaking #9's tests.
  - **Tick + input tasks are supervisor-owned seams (§12).** `spawn_tick_task`
    (`tokio::spawn` + `interval` with `MissedTickBehavior::Skip`, `select!` on its
    child `CancellationToken`, non-blocking `try_send` so a full channel drops a
    harmless tick and a closed one ends the task) and `spawn_input_reader`
    (`spawn_blocking` polling with a bounded 100 ms timeout so cancellation is
    observed, `blocking_send` so a slow render never drops a keystroke, ignoring
    mouse/focus/paste per the v1 keyboard-only model) each return a `JoinHandle` the
    composition wraps in `TokioTask` and registers with the `Supervisor` (ancillary
    tasks; the render loop is the render task on `spawn_blocking`). The composition
    recipe is documented in `src/ui/driver.rs`.
  - **Tested with `TestBackend` and mocks; no socket, no real clock.** `render`,
    `layout_root`, the loop `step`, the two-level dispatch, and the crossterm-event
    normalization are unit-tested with a `ratatui::backend::TestBackend` and a
    crate-internal `App` test-support builder; the tick task is asserted on a paused
    virtual clock (zero real wait). `render`, `layout_root`, `RootLayout`,
    `run_render_loop`, `event_channel`, `EVENT_CHANNEL_CAPACITY`, `spawn_tick_task`,
    `spawn_input_reader`, and `KeyRoute` are re-exported from the crate root. **No new
    dependency and no new `tokio`/`crossterm` feature** — the render loop uses the
    `sync`/`rt`/`macros`/`time` features already present from #11 and crossterm's
    default `events`. Tests: 19 in-module (`src/ui/mod.rs` layout/purity/reachable-
    screen + `prop_render_never_panics` over both modes × every reachable screen ×
    help × every live load state × terminal sizes from 1x1; `src/ui/driver.rs`
    dirty-gated `step`, parked-loop drain/quit, two-level dispatch incl. modal-swallow
    and replay-scrub forward, event normalization, and the two tick-task lifecycle
    tests) plus 4 in `src/app.rs` (`dispatch_key_global` route/modal/non-press) —
    23 new; 298 lib tests total.
- The application-owned `ProviderRegistry` and the `ChainViewApp` builder
  (`src/app/registry.rs`, issue #12; `docs/02-tui-architecture.md` §11,
  `docs/03-data-providers.md` §9, ADR-0006). The open provider-extension entry
  point: the stock binary and any external thin binary compose the app through
  `ChainViewApp::builder()` and drive it with `run()`, so a developer plugs in
  their own venue with no fork and no central enum to edit. Key behaviours:
  - **Collision is a typed startup error, never a panic or silent last-wins.**
    `register(impl Provider + 'static)` reads `provider.id()`: a **reserved**
    built-in id used through the external path records
    `RegistryError::ReservedId`, a **duplicate** id records
    `RegistryError::DuplicateId`, a **gated** built-in requested via
    `with_gated_builtin(id)` records `RegistryError::Gated(id)`, and an **empty**
    registry at `run()` is `RegistryError::Empty` — all surface as
    `ChainViewError::Registry`. Every builder method returns `Self`; build-phase
    errors are accumulated first-error-wins and reported by `run()`, so no method
    returns a mid-chain `Result`.
  - **`with_builtins()` is an honest no-op in v0.1.** The only gate-clear
    built-in is Deribit (public, no auth), whose adapter lands in #15/#16 — so
    **no fake provider is fabricated** and `builder().with_builtins().run()`
    reports `RegistryError::Empty` until Deribit is registered here. The external
    `register()` path and the collision/empty validation are fully exercised
    today.
  - **`with_gated_builtin` fails while the gate holds.** No gated adapter ships
    in v0.1 (`docs/SECURITY.md` §2.3–§2.4), so the gate always holds and the opt-in
    records `RegistryError::Gated`; this is the *mechanism*, exercised in v0.4. It
    also resolves the CV-CODEX-051 drift: the concrete typed error is
    `RegistryError::Gated` (a runtime hard gate; gated adapter code absent), and
    the unattached `ChainViewError::ProviderGated` sketch is removed from
    `docs/02-tui-architecture.md` §11.
  - **`--provider` resolution and the capability-driven composite-source guard.**
    `run()` resolves `config.provider` against the registry: an absent id is
    `ConfigError::UnknownProvider` (a syntactically invalid id is
    `ConfigError::InvalidValue` at the `ProviderId::new` grammar gate, before it
    can reach the registry). The selected provider's capabilities are read
    **once** and wired into `App`'s `SourceBinding`; a **chain-less** provider
    (standalone dxlink) selected as the live *source* is `ConfigError::InvalidValue`
    — the composite-source guard, which reads the declared `ChainCapability`,
    **never** matches a `ProviderId`. Replay mode needs no live provider and skips
    resolution.
  - **`Arc<dyn Provider>`, immutable after validation; registry is UI-unreachable.**
    Each adapter is stored behind an `Arc` so one adapter is shared read-only
    across the poll + stream tasks (#13/#16) without re-fetching; the registry is
    immutable once `run()` validates it. `ProviderRegistry` is opaque
    (private field, assembled only through the builder) and **not** re-exported —
    the UI-facing gating seam is the `SourceBinding`'s `ProviderCapabilities` +
    `ProviderId`, never the registry or a `dyn Provider`.
  - **The composition seam is documented, not spun.** `run()` validates the
    registry and resolves the live source, then returns `Ok(())`; the tokio
    runtime, the `Supervisor` (#11), the bounded channels (#10), the seeded
    `ChainStore` (from the provider's first fetch, #15), and the render loop (#13)
    are assembled at the documented seam. A test-only `FakeProvider` implementing
    the public `Provider` port exercises `register()` without a real adapter
    (prefiguring the #22 faux provider). `ChainViewApp` and `ChainViewAppBuilder`
    are re-exported from the crate root. Adds `RegistryError::Gated` to `src/error.rs`
    (pre-v0.1 addition to an unshipped surface — no SemVer event). **No new
    dependency.** Tests: 13 unit + 3 property (`prop_registry_rejects_reserved_id`,
    `prop_registry_rejects_duplicate_id`, `prop_capabilities_total` — gating total
    over declared capabilities, never id) in `src/app/registry.rs`, plus the
    `RegistryError::Gated` display test in `src/error.rs`.
- The single task supervisor, cancellation-token tree, and ordered teardown
  (`src/app/supervisor.rs`, issue #11; `docs/02-tui-architecture.md` §12,
  ADR-0005). One `Supervisor`, owned by the application layer, owns **all** task
  handles and a root `tokio_util::sync::CancellationToken` so the invariant
  "every spawned task has a shutdown path" is enforceable process-wide. Key
  behaviours:
  - **Cancellation-token tree.** A root token has one `child_token` per task
    (`Supervisor::child_token`); cancelling the root cascades to every child,
    and `cancel_provider(id)` cancels a **single** provider's child without
    touching the others or the root (used on `Unsubscribe`/`Rediscover`). The
    supervisor coordinates by tokens + join handles, **never a lock across an
    `.await`** (`rules/global_rules.md` — Concurrency).
  - **All triggers converge on one teardown.** A clean quit (`request_quit`,
    wired from `App::should_quit`), a startup / provider-past-budget / channel
    close failure (`fail`), and a **panic in any task** (reported through the
    `ExitReporter` seam as `TaskExit::Panicked`, detected via a `JoinHandle`
    join result whose `JoinError::is_panic()` is true — `TokioTask::join`) all
    trip the root token. The panic path is a task-level fatal signal the
    supervisor records itself; it does **not** rely on the process panic hook
    alone (that only restores the terminal). No trigger leaves an orphan.
  - **Deterministic join order, terminal restored LAST.** Teardown (1) cancels +
    joins the **provider** tasks, then (2) the **input/tick/replay** tasks, then
    (3) lets the **render** task exit, and only then (4) runs the `FinalTeardown`
    that restores the terminal (`GuardTeardown` drops the #8 `TerminalGuard`) —
    the strictly-last step on every path, including panic.
  - **Bounded join, then abort.** Each join has a `DEFAULT_JOIN_BUDGET` (2 s)
    `tokio::time::timeout`; a task that ignores cancellation past the budget is
    `abort()`ed so a wedged upstream socket can never hang exit. The budget is
    asserted with a **controllable virtual clock** (`#[tokio::test(start_paused
    = true)]`), so the 2 s window is honored in virtual time with **zero real
    wall-clock wait**.
  - **Error propagation seam.** `run()` returns the first fatal `ExitCause`
    (`Clean` / `TaskPanicked` / `Failed(ChainViewError)`, first-fatal-wins), with
    `exit_code()` (0 clean, 1 failure) and a redaction-safe `failure_message()`.
    The supervisor **never** calls `std::process::exit` (that would bypass the
    guard `Drop`); `main` maps the returned cause to an exit code and prints the
    message on `stderr` **after** the terminal is restored (CLAUDE.md governance
    item 3).
  - **Testable with mocks, no socket / no real clock.** The `SupervisedTask`
    trait (real `TokioTask` vs. a recording mock) and the `FinalTeardown` trait
    (real `GuardTeardown` vs. a recorder) make the join-order and
    bounded-join-then-abort deterministic. `Supervisor`, `ExitCause`,
    `ExitReporter`, `FinalTeardown`, `GuardTeardown`, `SupervisedTask`,
    `TaskExit`, `TokioTask`, and `DEFAULT_JOIN_BUDGET` are re-exported from the
    crate root for the builder (#12) / render loop (#13) / `main.rs` to wire.
    Sequencing the guard last (#8), the channels (#10), the render loop (#13),
    and the provider reconnect internals (#16) are left as clean seams.
    Tests: 13 in-module (exit-code/message, request-quit cascade,
    cancel-provider isolation, bounded-join wedged-abort + cooperative-in-budget
    on a paused clock, a **real-`TokioTask`** wedged regression proving the
    timeout-then-abort truly cancels rather than detaching an orphan — it fails
    against a `take()`-based join, ordered provider→ancillary→render→restore,
    normal-quit all-join, reported-panic non-zero + restore-last + no-orphan,
    panic-at-join-over-clean-trigger, first-fatal-only, wedged-run
    abort-still-clean) plus 2 integration (`tests/supervisor_shutdown.rs`,
    real tokio tasks: normal-quit every-task-joined + terminal-restored, and an
    injected real provider panic → non-zero `TaskPanicked` exit, terminal
    restored, no orphan). Adds one dependency and extends `tokio` (audit notes):
  - `tokio-util` `0.7` (`default-features = false`) — only
    `tokio_util::sync::CancellationToken` for the root + per-task child-token
    tree (no codec/io features). RUSTSEC-clean; MSRV 1.71 (below our 1.85).
    Explicit-approval addition (CLAUDE.md "Coding Rules").
  - `tokio` gains `rt` / `macros` / `time` on top of the existing `sync`. This
    **supersedes** the two earlier notes that pinned tokio to `["sync"]`-only —
    the #6 provider-port entry ("no runtime / macros / net yet — the full runtime
    features land with the adapters and app loop in later issues") and the #10
    bridge entry ("no new `tokio` features … no `rt`/`macros`/`time` are pulled");
    #11 is that "later issue". `rt` for `JoinHandle`/`abort`/`JoinError::is_panic`,
    `macros` for the supervise `tokio::select!`, `time` for the bounded-join
    `tokio::time::timeout`. Still no `net`/`fs`/`rt-multi-thread` — the render loop
    (#13) picks the runtime flavor. A **dev-only** `test-util` feature (in
    `[dev-dependencies]`, never in the release binary) enables the paused virtual
    clock for the no-wall-clock budget tests. RUSTSEC-clean.
- The two-class bounded, coalescing provider -> app bridge (`src/app/bridge.rs`,
  issue #10; `docs/02-tui-architecture.md` §5, `docs/06-performance.md` §3.2,
  `docs/03-data-providers.md` §5). `EventBridge` is the seam that joins the async
  data layer to the synchronous render-loop fan-in (`App::on_event`), draining
  only over **bounded** `tokio::sync::mpsc` channels — no unbounded channel exists
  on the provider -> app path. Key behaviours:
  - **Two-class backpressure.** A bounded **coalesced** channel carries
    `Quote`/`Greeks`/`Depth` (capacity from `config.channel_capacity`); a
    **separate, small** bounded **control** channel carries `Chain`/`Health`. The
    fan-in wakeup (`pump`/`pump_into`) drains the control channel **first**
    (priority) so a `Health(Reconnecting)` or a fresh `Chain` is delivered
    promptly even while the coalesced channel is saturated with stale quote
    traffic — health can never sit behind a quote burst.
  - **Consumer-side conflation, O(N) not O(burst).** The coalesced channel drains
    into a per-instrument staging map keyed by `InstrumentKey` — one slot per
    instrument, last-value-wins — then the current values flush into
    `App::on_event`. The map is bounded by the subscribed instrument count N,
    never by burst rate or session length; a dropped intermediate quote is not a
    correctness loss (the chain shows the freshest price). Within a slot a quote,
    a Greeks row, and a depth ladder are held independently, so a Greeks refresh
    never clobbers a pending quote and the slot count stays exactly N. This is the
    **consumer-side** stage of the two-stage coalescing design (`docs/02` §5); the
    **producer-side** overwrite-on-full staging that preserves the freshest value
    when the bounded channel is *full* landed with the Deribit adapter in #16
    (`ProducerStaging`), so the two-stage coalescing is now complete and the
    NFR-15 latest-value-wins guarantee holds even under sustained channel
    saturation.
  - **HP-3 allocation discipline.** The staging map **reuses** its allocation
    across bursts — a flush drains via `HashMap::drain` and an unsubscribe prunes
    via `HashMap::retain`, both of which retain the bucket allocation, and a
    repeat update for an already-staged instrument clones no key — so once grown
    to fit N it performs zero steady-state per-burst allocation.
  - **Lifecycle tracks the subscription set.** A slot is created on the first
    update, overwritten on subsequent ones, and **removed** when the instrument
    is unsubscribed: `pump` drains the render -> data command channel and prunes
    the staging map on each `Command::Unsubscribe` (an absolute-expiry unsubscribe
    prunes the matching expiry precisely; a relative-days one, which cannot be
    resolved without a wall clock the fan-in deliberately lacks, prunes the whole
    underlying), while forwarding every drained command to a caller `route`
    closure — the clean seam the task supervisor (#11) fills to reach the provider
    layer.
  - **Testable with no socket and no wall clock.** The drain is `try_recv`-based
    and reads no clock; `EventBridge::new(coalesced_capacity)` returns the bridge
    plus a `BridgeSenders` bundle (the producer halves the supervisor wires to the
    adapters and to `App`). The adapter that produces updates (#16), the
    supervisor that owns the channel ends (#11), and the render loop that pumps
    between frames (#13) are separate issues; the seams for each are explicit.
    `EventBridge`, `BridgeSenders`, and the `CONTROL_CHANNEL_CAPACITY` /
    `COMMAND_CHANNEL_CAPACITY` constants are re-exported from the crate root.
    Tests: 17 in-module — staging-map overwrite / lossless-per-kind (quote+greeks,
    depth+quote) / control-not-staged / remove-on-unsubscribe (absolute expiry,
    relative days, other-underlying no-op), capacity-reuse and latest-value-over-
    burst (HP-3, deterministic memory-flatness by asserting `capacity()` is stable
    and `len() <= N` across 1000 bursts), plus `EventBridge` priority-drain,
    health-delivered-while-saturated, burst-beyond-channel-capacity-keeps-memory-
    flat, every-instrument-receives-latest, unsubscribe-prunes, command-routed,
    and two `pump`-into-a-live-`App` folds. **No new dependency and no new `tokio`
    features**: `tokio::sync::mpsc` channels with `try_send`/`try_recv` are
    runtime-free, so the existing `["sync"]` feature suffices — no `rt`/`macros`/
    `time` are pulled, keeping runtime features minimal.
- The application state machine and synchronous event fan-in (`src/app.rs`,
  `src/event.rs`, issue #9; `docs/02-tui-architecture.md` §3, §4). `App` owns all
  render-loop state as a `Live | Replay` `Mode` machine; the fan-in folds every
  event into state and keeps `ratatui` off the async executor. Key behaviours:
  - **Mode-scoped screens make out-of-mode pairs unrepresentable.** `LiveScreen
    { Chain, Depth, Surface, Payoff }` and `ReplayScreen { Replay, Payoff }` are
    owned by their mode's state, so `Replay` + `Chain` cannot be constructed — the
    type system, not a runtime fallback, prevents it, and the render dispatch (#13)
    stays a total, wildcard-free match.
  - **One exhaustive, wildcard-free fan-in.** `App::on_event` folds each
    `AppEvent { Key, Resize, Tick, Market, ReplaySeek }` in a single match with no
    `_` arm and sets `dirty` on any mutation; adding a variant forces every fold
    site to be revisited by the compiler. `Market(MarketUpdate)` folds into the
    `ChainStore` (`Quote`/`Greeks` → the merge path, `Chain` → a snapshot-driven
    `apply_poll`, `Health` → the correct side's badge); an idle tick does not set
    `dirty`.
  - **I/O never runs inline.** `on_event` is synchronous and never `.await`s; a
    handler that needs I/O (reconnect, re-discover, seek/reload the bundle,
    subscribe) emits a typed `Command { Subscribe, Unsubscribe, Reconnect,
    Rediscover, SeekBundle, ReloadBundle }` on a bounded command channel via a
    non-blocking `try_send`.
  - **Per-side composite health.** `LiveState` binds a `SourceBinding` plus an
    optional `OverlayBinding`, each carrying its own `ProviderCapabilities` and
    `StreamHealth`; a health transition routes to the matching side by id equality,
    so either side failing degrades only that side.
  - **Capability-driven reachability, never a `ProviderId` match.** The
    `is_screen_reachable(screen, caps)` helper and `LiveState::set_screen` gate on
    declared `ProviderCapabilities` (source ∪ overlay), so a screen is only ever
    set to a reachable value and a built-in and an external provider are gated
    identically. The `Tab` skip / number-key hint mechanics land in #13/#14.
  - **Documented stubs with stable shapes.** `ReplayState`/`BundleLoad`/
    `LoadedReplay`/`Playback` (v0.3) and `PayoffBuilder` (v0.2) are typed
    placeholders whose enum/struct shapes are fixed now so later work fills the
    internals without a breaking change; `StatusLine`/`Selection`/`ScreenLoad` are
    minimal, typed state the render loop (#13/#14) drives.
- The terminal lifecycle: the RAII `TerminalGuard` and the panic-hook restore
  (`src/terminal.rs`, issue #8; `docs/02-tui-architecture.md` §6, ADR-0001). The
  guard's constructor enables raw mode, enters the alternate screen, and hides the
  cursor; its `Drop` runs the exact inverse, so the terminal is restored on
  **every** exit path — a normal return, an early `?`, or a panic. Key behaviours:
  - **Transactional setup, tolerant teardown.** Setup records each applied step;
    a mid-sequence failure rolls back exactly the applied prefix and returns
    `ChainViewError::Terminal`, so a rejected setup (e.g. no TTY) leaves the shell
    clean. Teardown is best-effort, infallible, and idempotent — a
    partially-initialized guard and a double teardown both restore without
    panicking (a `restored` latch guarantees at-most-once).
  - **Panic hook chains, never swallows.** `install_panic_hook` captures the
    previously installed hook via `std::panic::take_hook`, restores the terminal
    first (show cursor, leave alternate screen, disable raw mode — synchronous,
    allocation-light, errors ignored), then invokes the captured hook, so the
    backtrace prints on a normal (non-raw) screen and is never lost.
  - **TTY-less testability.** The low-level operations are abstracted over a
    crate-internal `TerminalOps` trait; unit tests drive a recording fake to
    assert the setup order, the inverse teardown order, idempotent double-restore,
    partial-setup tolerance, setup-failure rollback, and teardown error tolerance
    — all deterministic, with no real terminal. The restore-before-chain ordering
    is proved by a small `restore_then_chain` primitive tested with fakes (no
    process-global hook). The concrete `crossterm` path (`CrosstermOps`) is
    exercised end to end by a `harness = false` subprocess in
    `tests/terminal_restore.rs`: the child installs the real hook and panics; the
    parent asserts the leave-alternate-screen + show-cursor escapes reach the
    child's stdout (restore ran) and the panic marker reaches stderr (chained hook
    not swallowed), and that the child exits non-zero without hanging.
  - **`main.rs` wiring.** Startup installs the panic hook, then enters the guard,
    so the render loop (#13) will be wrapped by a guaranteed restore. `main`
    returns the typed `ChainViewError` (`ConfigError` folds in via `#[from]`), so
    the `main.rs`-only `anyhow` deviation gate (`clippy.toml`) is left untouched.
    No `std::process::exit` bypasses `Drop`; the supervised, ordered teardown that
    sequences the guard last lands in #11. `TerminalGuard` and `install_panic_hook`
    are re-exported from the crate root so an external thin binary (ADR-0006) can
    drive the same restore.
  - Tests: 8 unit (`src/terminal.rs`) plus the subprocess integration harness.
  Adds the first two TUI dependencies (audit notes):
  - `ratatui` `0.29` (`features = ["crossterm"]`) — the widget/layout library
    (ADR-0001), first TUI pull approved by this issue. `ratatui` `0.30` requires
    rustc 1.88, above the crate's 1.85 MSRV, so the resolver pins `0.29`, which
    re-exports `crossterm` `0.28.1`. `RUSTSEC`-clean at this revision; the
    de-facto standard Rust TUI toolkit.
  - `crossterm` `0.28` — the terminal backend (raw mode, alternate screen, cursor
    control) named by ADR-0001; cross-platform including Windows. Pinned to the
    same `0.28` line `ratatui` `0.29` drives, so cargo unifies to a **single**
    `crossterm` instance — the one ChainView calls is exactly the one `ratatui`
    drives, with no two-version mismatch. `RUSTSEC`-clean.
- The live `ChainStore` (`src/chain/store.rs`, issue #7): the deterministic
  poll -> stream merge over the `optionstratlib` chain
  (`docs/01-domain-model.md` §5.1, §6, `docs/03-data-providers.md` §3, §4).
  Assembled from a `ChainFetch` via `ChainStore::seed`, carrying the same
  `AliasCatalog` forward with no re-derivation; `apply_poll` / `apply_quote` /
  `apply_greeks` / `apply_health` mutate it **only on the market/tick event**,
  never in draw, and `snapshot()` emits a `ChainSnapshot`. Key behaviours:
  - **Strike-keyed clone/patch/re-insert row update.** A `QuoteUpdate` /
    `GreeksRow` takes the row at its strike out of the upstream
    `BTreeSet<OptionData>` (via a strike-only probe, since `OptionData`'s `Ord`
    is its `strike_price`), clone-patches only the update's `OptionStyle` side,
    recomputes that side's `*_middle` (upstream's `(bid+ask)/2` rounded to 4 dp),
    and re-inserts — opposite leg and untouched fields preserved, set ordering
    unchanged.
  - **Field-fold rules.** A rejected (absent) field keeps its prior value; a
    **crossed** quote (`ask < bid`, or a zero ask on a non-zero bid) rejects the
    whole update and keeps the prior row (a zero bid is valid). `theta`/`vega`/
    `rho` have no `OptionData` field and are intentionally not folded — the
    per-style analytics sidecar lands in v0.2 (`docs/01` §7).
  - **Bounded-generation merge.** A monotonic snapshot generation stamps each
    poll; a de-listed strike is tombstoned (and never resurrected), a re-listed
    strike clears its tombstone. A stream update for an unknown strike is held in
    a bounded `MAX_PENDING` (256) FIFO buffer with a `pending_ttl` per-entry TTL
    (`refresh_interval` + slack); on overflow the oldest entry is dropped
    (counted via `dropped_overflow`); on the next poll a pending update whose
    strike is now present is applied, a tombstoned or past-TTL one is dropped.
    A stream update for a tombstoned strike is dropped immediately.
  - **Two-clock freshness (§5.1).** A per-instrument watermark = `max(event_time)`
    drops an out-of-order update (event time below the watermark) for value and
    direction and counts it (`dropped_stale`); a `None`-`event_time` update
    orders by receipt and never advances the watermark. Per-component staleness
    (`quote_freshness` / `greeks_freshness` / `chain_freshness`) against
    `QUOTE_STALE_AFTER` / `GREEKS_STALE_AFTER` / `chain_stale_after`, plus a
    feed-delay `Delayed` classification past `FEED_DELAY_WARN` with negative skew
    clamped to zero — surfaced as the new `Freshness` enum.
  - **Retained/decayed price direction.** Per-instrument prev bid/ask + last
    change time drive a `TickDir` (Up on strictly higher, Down on strictly lower,
    an equal value keeps the prior, first-ever `Flat`), decayed to `Flat` after
    `DIRECTION_DECAY` (3 s) and cleared to `Flat` immediately on a
    stale/reconnecting `apply_health` — mutated on the event, read pure in draw.
  - **Cross-provider overlay gate wired.** A leg whose overlay feed differs from
    the source provider merges only when `AliasCatalog::overlay_compatible`
    passes; a `ContractSpecFingerprint` mismatch refuses the leg
    (`MergeOutcome::OverlayRefused`), keeps the source leg, and badges it
    (`is_overlay_refused`); the within-provider merge is a no-op for the gate.
  - The `ChainStore`, `Freshness`, `MergeOutcome`, `TickDir`, `MAX_PENDING`, and
    `pending_ttl` are re-exported from the crate root; `ChainSnapshot` /
    `ChainSource` / `StreamHealth` stay in `src/chain/events.rs` with unchanged
    re-export paths (the forward declarations already matched the store's needs).
    Tests: 32 unit (clone/patch both legs and orders, crossed/zero/missing folds,
    staleness/delay/negative-skew, out-of-order + watermark, direction
    up/down/equal/decay/stale-clear, tombstone no-resurrection, pending
    new-listing/TTL/overflow, overlay gate) plus 4 property
    (`prop_chain_merge_idempotent`, `prop_overlay_spec_gate`,
    `prop_no_resurrection_and_bounded_memory` over scripted poll/stream
    interleavings, `prop_freshness_out_of_order_keeps_max_event_value`). No new
    dependency.
- The PUBLIC, semver-governed **provider port** (`src/providers/mod.rs`, issue #6):
  the `#[async_trait] Provider: Send + Sync` trait (`id` / `capabilities` /
  `discover` / `fetch_chain` / `subscribe`) an external adapter compiles against
  to plug in its own venue (`docs/03-data-providers.md` §2, §11.1, ADR-0006). The
  `#[non_exhaustive] ProviderCapabilities` capability self-declaration with its
  `ProviderCapabilitiesBuilder` — the ONLY cross-crate construction path — plus
  the `#[non_exhaustive]` dimension enums `ChainCapability` (Native / Assemble /
  Partial / None), `GreeksCapability` (Provided / ComputedLocally / None),
  `OptionStreamCapability` (None / SymbolOnly{verified} / ChainQuotes{verified}),
  `ChainPollCapability` (None / Poll{interval_hint_secs: u32}), and `AuthKind`
  (None / Token / KeySecret / UserPass). Streaming is **three independent
  dimensions** (`option_stream` / `underlying_stream` / `chain_poll`) so a
  real-time underlying is never mis-badged as a real-time option chain; every
  dimension defaults to its least-capable variant, so a future field lands with a
  safe default and keeps external adapters compiling and honest
  (`docs/SEMVER.md`). The port helper types `UnderlyingRef`,
  `SubscriptionRequest`, and the drop-cancels `SubscriptionHandle` (a `Send`
  cancel closure so the port stays agnostic to the adapter's cancellation
  mechanism). The `async_trait` per-call allocation is accepted and doc-noted —
  provider methods are cold-path, the hot render loop holds no `dyn Provider`.
- The named fetch artifact (`src/chain/fetch.rs`, issue #6): `ChainFetch { chain,
  expiry_source, aliases }` — the artifact `Provider::fetch_chain` returns,
  **never** a bare `OptionChain`, so the poll leg preserves the absolute-UTC
  expiry/source identity (`ExpirySource { underlying, expiration_utc, provider }`)
  and the per-leg `AliasCatalog` the merge/subscription/resubscription/DXLink
  overlay joins need (`docs/01-domain-model.md` §6, `docs/03-data-providers.md`
  §2, §4). `AliasCatalog` maps the provider-agnostic `InstrumentKey` to each
  feed's `Instrument` (native + stream symbols + spec fingerprint) with
  `instrument()`, `resolve_symbol()` (native AND stream symbol → shared key), and
  `overlay_compatible()` — the cross-provider economic-equivalence gate returning
  `Result<(), OverlayError>` with the **real `ContractSpecFingerprint`
  comparison** wired (first disagreeing dimension → `OverlayError::SpecMismatch`;
  the within-provider merge is a no-op; the store-level *invocation* lands in #7).
  These are DOMAIN types (the trait emits them, the future `ChainStore` consumes
  them) defined in `src/chain/*` and re-exported through the port surface so the
  module graph stays acyclic (port → domain, never domain → port,
  `docs/03-data-providers.md` §12). Now that `AliasCatalog` and a trivial
  `ChainSource` (Poll / Stream / Merged) enum exist, the forward-declared
  `ChainSnapshot` (issue #5) gains its documented `aliases: AliasCatalog` and
  `source: ChainSource` fields (the store LOGIC that drives them still lands in
  #7). The full port surface — trait, capabilities + builder + enums,
  `ChainFetch`/`ExpirySource`/`AliasCatalog`, and the helper types — is
  re-exported from the crate root. Adds two runtime dependencies (audit notes):
  - `async-trait` `0.1` — object-safe `async fn` methods on the `Provider` trait
    (the port must be `dyn`-dispatched via the `ProviderRegistry`, issue #12).
    The per-call box allocation is cold-path only. Ubiquitous, `RUSTSEC`-clean;
    the standard way to express an object-safe async trait on stable Rust.
  - `tokio` `1` (`default-features = false`, `features = ["sync"]`) — only
    `tokio::sync::mpsc::Sender<MarketUpdate>` for the `subscribe` bounded fan-in
    channel (`docs/03-data-providers.md` §5). Minimal features: **no** runtime /
    macros / net yet — the full runtime features land with the adapters and app
    loop in later issues. `RUSTSEC`-clean; the mandated async runtime
    (`rules/global_rules.md` "Concurrency").
- Normalized streaming update events and freshness clocks
  (`src/chain/events.rs`, issue #5): the DOMAIN payloads a provider emits across
  the seam (`docs/01-domain-model.md` §5 and §5.1). `QuoteUpdate` (bid/ask/last/
  bid_size/ask_size), `GreeksRow` (iv + delta/gamma/theta/vega/rho + a
  `GreeksOrigin` tag), and `DepthLadder`/`DepthLevel` (best-first bids/asks +
  an `Option<u64>` `change_id` for Deribit gap-detect/resync) — **every numeric
  field is `Option`** so a value the feed omits stays `None` and renders as an
  em dash, never a fabricated zero; quotes/IV/Greeks are `Positive`/`Decimal`
  **display analytics**, never accounting values. Each event carries the **two
  clocks** of §5.1: `event_time` (venue timestamp, optional) and `received_time`
  (normalization time, always present). The closed `MarketUpdate` fan-in enum
  (`Quote`/`Greeks`/`Depth`/`Chain`/`Health`) matched exhaustively downstream
  with no wildcard `_` arm, plus **thin forward declarations** of the store
  types `ChainSnapshot` and `StreamHealth` (self-contained fields only —
  `aliases: AliasCatalog` and `source: ChainSource` are **completed with the
  chain store in issues #6/#7**) so the enum can be closed now. The named
  freshness thresholds with no magic numbers — `QUOTE_STALE_AFTER` (5 s),
  `GREEKS_STALE_AFTER` (10 s), `FEED_DELAY_WARN` (2 s), `DIRECTION_DECAY` (3 s),
  and `CHAIN_STALE_SLACK` (2 s) with the `chain_stale_after(refresh_interval)`
  helper that fixes §5.1's `CHAIN_STALE_AFTER = refresh_interval + slack`
  formula (the store applies the comparison in #7). The event types, the
  `MarketUpdate` enum, the forward-declared `ChainSnapshot`/`StreamHealth`, and
  the threshold constants/helper are re-exported from the crate root. **No new
  dependency**: Greeks use the `Decimal` (`rust_decimal`) that `optionstratlib`
  already re-exports through its prelude.
- Normalized instrument identity (`src/chain/identity.rs`, issue #4): the
  provider-agnostic `InstrumentKey` (`underlying`, absolute-UTC
  `expiration_utc`, `strike`, `style`) with `Eq`/`Hash` over all four fields and
  deliberately **no** `ProviderId` — so a REST snapshot row and a stream overlay
  for the same option collapse to one map entry; the `ContractSpecFingerprint`
  economic-equivalence spec (`contract_multiplier`, `settlement`, `exercise`,
  `quote_currency`, `venue_product_code`) with the `SettlementStyle`/
  `ExerciseStyle` (`#[repr(u8)]`) enums, deriving `Eq`/`Hash` so the
  cross-provider overlay gate (issue #7) can compare it by value; and the
  `Instrument` view (key + owning `ProviderId` + native/stream aliases + spec)
  with **hand-written** `PartialEq`/`Eq`/`Hash` delegating to `key` only. The
  open, validated `ProviderId` newtype is completed from its issue #2/#3
  placeholder into the full form: a fallible `new()` → `ConfigError::InvalidValue`,
  `as_str()`, `is_reserved()`, `serde` via `try_from = "String"` / `into =
  "String"` (re-validates on the way in), `PartialOrd`/`Ord` retained for
  `Config`'s `BTreeMap`, not `Copy`; plus `RESERVED_PROVIDER_IDS` (the five
  built-in ids). `validate_provider_id` in `src/config.rs` now delegates to
  `ProviderId::new`. The identity types, the style enums, and
  `RESERVED_PROVIDER_IDS` are re-exported from the crate root, alongside
  `optionstratlib`'s `Positive` and `OptionStyle` (the domain numeric
  vocabulary on the public identity surface). Nothing may `match` on a
  `ProviderId` (documented; arch test lands in issue #22).
  Adds two runtime dependencies (audit notes):
  - `optionstratlib` `0.18.0` — the chain model and options math; supplies
    `Positive` (non-negative price/strike) and `OptionStyle` (call/put) on the
    identity surface. Default features are empty (`default = []`), so no
    tokio/reqwest/plotly is pulled; `RUSTSEC`-clean at this revision, first-party
    ecosystem crate. Named by `CLAUDE.md` "Key Decisions" as the mandated chain/
    math library.
  - `chrono` `0.4` (`default-features = false`, `features = ["std"]`) —
    `DateTime<Utc>` for the absolute-UTC expiry in `InstrumentKey`; the ecosystem
    timestamp type (`rules/global_rules.md` "Type Safety"). Minimal features (no
    `clock`/`serde`) requested; feature unification with `optionstratlib` adds no
    obligation. `RUSTSEC`-clean.
- Typed configuration surface (`src/config.rs`, issue #3): the immutable
  `Config` with `ProviderSettings`, `ThemeChoice`, and `ModeSelect`, assembled
  once at startup and validated into typed `ConfigError`s. A layered loader with
  `CLI flag > env (CHAINVIEW_*) > optional TOML file > typed default` precedence
  (`Config::assemble` is pure over an injectable `EnvSource`; `Config::load`
  does the XDG-aware file read, while `.env` is loaded by `main.rs` startup
  glue before config assembly); `humantime` durations with range checks
  (refresh `[250ms,300s]`, tick `[50ms,5s]`, channel-cap `[64,65536]`);
  `deny_unknown_fields` typo protection on the file. Env-only secrets: a
  credential key in a file is rejected, a missing required credential is
  `MissingCredential(provider)` (names the provider, never the key), and
  resolved values are wrapped in a redacted `Secret`. The reversible provider-id
  ↔ env-segment bijection (`encode_segment`/`decode_segment`/`provider_env_var`,
  `'_'→'__'`, `'-'→'_'`). The zero-config default resolves to Deribit BTC. The
  CLI grammar in `src/main.rs` (clap derive) including the `chainview replay
  <dir>` subcommand → `ModeSelect::Replay`. The `config` module and the headline
  types (`Config`, `CliOverrides`, `ModeSelect`, `ProviderSettings`,
  `ThemeChoice`) are re-exported as the public config surface; `ProviderId` gains
  `PartialOrd`/`Ord` so it can key `Config::providers`.
  Adds five runtime dependencies and one dev dependency, each named by
  `docs/07-configuration.md` §4 or required by the spec (audit notes):
  - `serde` (derive) — typed deserialize of the optional TOML file and the
    config enums; ubiquitous, `RUSTSEC`-clean, already ecosystem-standard.
  - `toml` — parse `~/.config/chainview/config.toml`; the canonical file format
    named in §2/§4.
  - `humantime` — the duration grammar (`250ms`/`2s`/`5m`) mandated by §4.
  - `clap` (derive) — the CLI grammar in `main.rs`; §4 names no CLI crate, so
    clap-with-derive is chosen and recorded here as the decision.
  - `dotenvy` — load `.env` at startup (§2); the maintained successor to the
    unmaintained `dotenv` crate.
  - `proptest` (dev) — property tests for the id↔env bijection and the
    humantime-parse ⇄ range gate (`docs/TESTING.md` §3).
  Design note: the documented §5.1 transliteration is a bijection only over ids
  with non-adjacent separators (realistic ids); it is not injective for the full
  grammar (`encode("a--") == encode("a_")`). Implemented verbatim, documented,
  and handed to issue #4 (owner of the `ProviderId` grammar) to resolve.
- Boundary error types (`src/error.rs`): `ChainViewError` and its per-boundary
  source enums — `ProviderError`, `BundleError`, `ConfigError`, `RegistryError`,
  and `OverlayError` — via `thiserror`, plus the `Redacted` trait and
  `TransportDetail`/`TransportKind` and the closed `NormalizeKind`. Redaction-safe
  by construction: no upstream error type reaches a widget and no secret can be
  interpolated into any `Display` (transport detail is a category + status only;
  normalize failures name a field, never a value). `ProviderError` converts into
  `ChainViewError` only through the explicit `ChainViewError::provider(id, source)`
  helper (the `Provider` variant carries the `ProviderId`); the other
  sub-boundaries convert via `#[from]`. A minimal `ProviderId` placeholder lands
  in `src/chain/mod.rs`, completed by #4. Re-exported from the crate root. Adds
  `thiserror` as the first runtime dependency.
- Bootstrap the single-crate (binary + lib) skeleton for v0.1: MSRV Rust 1.85
  on the 2024 edition, `#![forbid(unsafe_code)]` at both crate roots, the
  `[lints]` table (deny warnings, deny `unsafe_code`, clippy restriction
  lints), module stubs for `error` / `config` / `app` / `event` /
  `providers` / `chain` / `ui`, `rustfmt.toml` + `clippy.toml` (with `anyhow`
  scoped to `main.rs`/startup glue), the `make pre-push` toolchain skeleton,
  and `.env.example`. No runtime dependency added.

### Changed

- Tightened the `ProviderId` grammar from `^[a-z][a-z0-9_-]{1,31}$` to
  `^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$` (2–32 chars, `-`/`_` isolated between
  alphanumerics — no leading/trailing/adjacent separator), a strict superset
  check (issue #4, [ADR-0008](docs/adr/0008-provider-id-grammar-and-env-bijection.md)).
  This resolves the issue #3 non-injectivity defect in the
  `docs/07-configuration.md` §5.1 id ↔ env-segment transliteration
  (`encode("a--") == encode("a_") == "A__"`): under the tightened grammar the
  map is a **total bijection over the full valid-id space**, proved by property
  test (round-trip + no-collision) in `tests/property.rs`, which replaces the
  pinned-limitation test. All five built-ins and the documented examples
  (`my-broker`, `my_broker`, `td-ameritrade`) stay valid; `encode_segment`/
  `decode_segment` are unchanged and stay in `src/config.rs`. Pre-v0.1 narrowing
  of an unshipped surface — no SemVer event.

### Deprecated

### Removed

### Fixed

### Security

## [0.0.1] - 2026-07-12

### Added

- Reserve the `chainview` crate name on crates.io.
- Design documentation under `docs/` (PRD, roadmap, architecture, data
  providers, replay mode, views/UX, ADRs, and specs).

[Unreleased]: https://github.com/joaquinbejar/ChainView/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/joaquinbejar/ChainView/releases/tag/v0.0.1
