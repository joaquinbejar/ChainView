//! The payoff-diagram screen — the multi-leg builder and its states
//! (`docs/05-views-and-ux.md` §3, §4, §6).
//!
//! # This issue's scope (#26): the interaction layer, not the curve
//!
//! This lands the payoff **builder** interaction: [`handle_key`] drives the
//! application-layer [`PayoffBuilder`](crate::app::PayoffBuilder) state machine
//! (append the chain's focused leg, edit the cursor leg, validate + commit,
//! discard, toggle the curve mode), and [`draw`] renders the builder's **states**
//! — the empty hint, the in-progress leg list, and the inline per-leg validation
//! errors — states FIRST, never a blank or a panic. The payoff **curve** itself
//! (expiration + t+0 via the #23 graph adapter) lands in #27; this screen renders
//! the committed strategy's legs, not yet its line chart.
//!
//! # The draw path is pure
//!
//! [`draw`] takes `&LiveState` (never `&mut`) plus the `Copy` resolved [`Theme`],
//! and projects the builder + the borrowed [`OptionChain`](optionstratlib::chains::chain::OptionChain)
//! marks at draw time — no computation, no pricing, no I/O, no state mutation
//! (`docs/02-tui-architecture.md` §7). [`handle_key`] is pure over `&mut LiveState`:
//! it mutates the builder, performs no I/O, and never `.await`s.
//!
//! # Color is never the only signal
//!
//! The side carries a `BUY`/`SELL` text label, the cursor leg a `▸` glyph + bold,
//! a validation error a leading `!`, and the committed strategy a `✓` glyph — all
//! legible under `NO_COLOR` (`docs/05-views-and-ux.md` §7).

use crossterm::event::KeyEvent;
use optionstratlib::OptionStyle;
use optionstratlib::prelude::Positive;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};

use crate::app::atm_index_of;
use crate::app::keymap::{KeyChord, PayoffAction, resolve_payoff};
use crate::app::{BuilderLeg, LegFocus, LiveState, ReplayState, Side};
use crate::event::AppEvent;
use crate::ui::theme::Theme;

// ===========================================================================
// The draw entry point + the builder states (states first).
// ===========================================================================

/// Draw the live multi-leg payoff builder for `state` into `area` — a pure render
/// over the borrowed builder + chain (`docs/02-tui-architecture.md` §7).
///
/// States first (`docs/05-views-and-ux.md` §6): the **empty** hint ("add a leg with
/// `a`"), then the in-progress **leg list** with the cursor leg marked and each
/// leg's current mark, then any inline per-leg **validation errors**, and — once a
/// strategy commits — a `✓` header over the committed legs (the payoff **curve**
/// lands in #27). Never a blank, never a panic.
pub fn draw(state: &LiveState, frame: &mut Frame, area: Rect, theme: Theme) {
    let builder = &state.payoff_builder;
    let chain = state.store.chain();

    let title = Line::from(vec![
        Span::styled("Payoff", theme.accent()),
        Span::styled(format!("  {}", builder.curve().label()), theme.dim()),
    ]);
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Empty state: no legs and nothing committed — the honest "add a leg" hint,
    // vertically centered so it reads as a deliberate state (§6).
    if builder.is_empty() && builder.committed().is_none() {
        draw_empty(frame, inner, theme);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(builder.legs().len() + 3);

    // A committed strategy: a `✓` header over its legs (the curve renders in #27).
    if let Some(committed) = builder.committed() {
        let n = committed.legs().len();
        lines.push(Line::from(vec![
            Span::styled("✓ ", theme.accent()),
            Span::styled(format!("committed {n} {}", leg_word(n)), theme.accent()),
        ]));
    }

    // The in-progress (or committed) leg list, cursor leg marked.
    let cursor = builder.cursor();
    for (idx, leg) in builder.legs().iter().enumerate() {
        lines.push(leg_line(leg, idx == cursor, leg.mark_in(chain), theme));
    }

    // Inline per-leg validation errors from the last commit attempt (§3, §6).
    if !builder.errors().is_empty() {
        lines.push(Line::from(""));
        for err in builder.errors() {
            lines.push(Line::from(vec![
                Span::styled("! ", theme.warning()),
                Span::styled(err.to_string(), theme.warning()),
            ]));
        }
    } else if builder.committed().is_some() {
        // The committed happy path draws its legs above; the line chart is a
        // deliberate deferral (the #27 curve renders in v0.2), not a stub.
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "payoff curve renders in v0.2",
            theme.dim(),
        )));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Draw the empty builder state — the "add a leg with `a`" hint, vertically and
/// horizontally centered in `inner` so it reads as a deliberate state, never a
/// blank void (`docs/05-views-and-ux.md` §6).
fn draw_empty(frame: &mut Frame, inner: Rect, theme: Theme) {
    let text = Text::from(vec![
        Line::from(Span::styled("add a leg with `a`", theme.accent())),
        Line::from(Span::styled(
            "on the chain: focus c / p, then a",
            theme.dim(),
        )),
        Line::from(Span::styled(
            "here: a adds the focused (or ATM) strike",
            theme.dim(),
        )),
    ]);
    let height = u16::try_from(text.height())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

/// One builder-leg line: `[▸] SIDE  qty×  strike C/P   mark <m>`. The cursor leg
/// carries a `▸` glyph and bold weight; the side is a `BUY`/`SELL` text label tinted
/// green (buy) / red (sell), and the mark renders `—` when unknown — all legible
/// under `NO_COLOR`.
#[must_use]
fn leg_line(
    leg: &BuilderLeg,
    is_cursor: bool,
    mark: Option<Positive>,
    theme: Theme,
) -> Line<'static> {
    let cursor_marker = if is_cursor { "▸ " } else { "  " };
    let spans = vec![
        Span::raw(cursor_marker.to_owned()),
        // The side is red for a sell (short), green for a buy (long); the BUY/SELL
        // text carries the signal under NO_COLOR.
        Span::styled(
            format!("{:<4}", leg.side.label()),
            theme.pnl_style(leg.side == Side::Sell),
        ),
        Span::raw(format!(
            " {}× {} {}   mark {}",
            leg.qty,
            fmt_strike(leg.strike),
            style_glyph(leg.style),
            fmt_price(mark),
        )),
    ];
    let line = Line::from(spans);
    if is_cursor {
        line.style(Style::new().add_modifier(Modifier::BOLD))
    } else {
        line
    }
}

/// The singular/plural word for a leg count.
#[must_use]
fn leg_word(count: usize) -> &'static str {
    if count == 1 { "leg" } else { "legs" }
}

/// The single-letter style glyph (`C` call / `P` put), exhaustive with no wildcard.
#[must_use]
fn style_glyph(style: OptionStyle) -> &'static str {
    match style {
        OptionStyle::Call => "C",
        OptionStyle::Put => "P",
    }
}

/// Format a strike, trailing zeros stripped, or `—` for the non-finite sentinel.
#[must_use]
fn fmt_strike(strike: Positive) -> String {
    if strike == Positive::INFINITY {
        return EM_DASH.to_owned();
    }
    format!("{}", strike.round_to(2))
}

/// Format a mark to two decimals, or `—` when absent — the `—`-not-`0` honesty rule
/// (`docs/01-domain-model.md` §8), guarding the `Positive` infinity sentinel so a
/// non-finite value never paints.
#[must_use]
fn fmt_price(value: Option<Positive>) -> String {
    match value {
        Some(price) if price != Positive::INFINITY => format!("{price:.2}"),
        _ => EM_DASH.to_owned(),
    }
}

/// The em dash rendered for an unknown value — never a fabricated `0`.
const EM_DASH: &str = "—";

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a live-payoff-local key, returning any follow-on [`AppEvent`]
/// (`docs/02-tui-architecture.md` §9). Pure over `&mut LiveState` — no I/O, no
/// pricing, no `.await`.
///
/// The chord resolves **through the single keybinding map**
/// ([`resolve_payoff`](crate::resolve_payoff), `src/app/keymap.rs`), so the builder
/// dispatch and the help overlay read one table and cannot drift. Each action drives
/// the application-layer [`PayoffBuilder`](crate::app::PayoffBuilder): `a` appends
/// the chain's focused leg, `x` removes the cursor leg, `+`/`-` change the cursor
/// leg's quantity (the concrete direction read from the shared chord), `s` toggles
/// its side, `Enter` validates + commits, `Esc` discards, and `t` toggles the curve
/// mode. Every mutation is local state, so `handle_key` returns `None`; the render
/// loop detects the builder's revision change and redraws
/// (`docs/02-tui-architecture.md` §8).
#[must_use]
pub fn handle_key(state: &mut LiveState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_payoff(chord)? {
        PayoffAction::AddLeg => {
            append_focused_leg(state);
            None
        }
        PayoffAction::RemoveLeg => {
            state.payoff_builder.remove_cursor();
            None
        }
        PayoffAction::Quantity => {
            adjust_qty(state, chord);
            None
        }
        PayoffAction::ToggleSide => {
            state.payoff_builder.toggle_cursor_side();
            None
        }
        PayoffAction::Commit => {
            commit(state);
            None
        }
        PayoffAction::Cancel => {
            state.payoff_builder.discard();
            None
        }
        PayoffAction::ToggleCurve => {
            state.payoff_builder.toggle_curve();
            None
        }
    }
}

/// Append the chain's currently-focused leg (`a`): the cursor strike + focused
/// call/put, long by default. A leg with no focused row yet falls back to the
/// nearest-spot strike ([`atm_index_of`]), so a fresh Payoff screen still builds a
/// sensible leg; an empty chain appends nothing.
///
/// Shared with the **chain** screen (`src/ui/chain.rs`): the chain-side `a`
/// (`ChainAction::AddLeg`) calls this same helper so the headline gesture — focus a
/// strike with `c`/`p`, press `a` to add it to the builder — appends through one code
/// path (ui→ui is allowed). A successful append bumps the builder revision (via
/// [`PayoffBuilder::append`](crate::app::PayoffBuilder)), which the driver's
/// `live_view_sig` diffs to mark the frame dirty.
pub(crate) fn append_focused_leg(state: &mut LiveState) {
    let chain = state.store.chain();
    let len = chain.options.len();
    if len == 0 {
        return;
    }
    let row = match state.selection.focused_row {
        Some(row) if row < len => Some(row),
        // No cursor yet (or one stale past a shrunk chain): fall back to ATM.
        _ => atm_index_of(chain),
    };
    let Some(row) = row else {
        return;
    };
    let Some(od) = chain.options.iter().nth(row) else {
        return;
    };
    let strike = od.strike_price;
    let style = style_of(state.selection.focused_leg);
    state.payoff_builder.append(strike, style);
}

/// Increment or decrement the cursor leg's quantity, read from the concrete chord —
/// the shared [`PayoffAction::Quantity`] binds both `+` and `-`.
fn adjust_qty(state: &mut LiveState, chord: KeyChord) {
    match chord {
        KeyChord::Char('+') => state.payoff_builder.increment_qty(),
        // The Quantity action binds only `+`/`-`; `-` (and any defensive fallback)
        // decrements.
        _ => state.payoff_builder.decrement_qty(),
    }
}

/// Validate + commit the built strategy (`Enter`) against the current chain
/// snapshot. Uses disjoint field borrows (`store` read immutably, `payoff_builder`
/// mutated) so the validation stays a pure read off the in-memory chain — no I/O.
fn commit(state: &mut LiveState) {
    let LiveState {
        store,
        payoff_builder,
        ..
    } = state;
    // Freshness reaches the validation as data (#26): the store's stream-quote
    // receipt clocks + the analytics reference instant, so a leg whose feed died
    // is rejected with StaleMark instead of committing a cached midpoint. Still a
    // pure read - no I/O, no wall clock in the draw path.
    let clocks = store.quote_clocks();
    let as_of = store.analytics_as_of();
    let _ = payoff_builder.commit(store.chain(), &clocks, as_of);
}

/// The [`OptionStyle`] of a focused leg, exhaustive with no wildcard.
#[must_use]
fn style_of(leg: LegFocus) -> OptionStyle {
    match leg {
        LegFocus::Call => OptionStyle::Call,
        LegFocus::Put => OptionStyle::Put,
    }
}

// ===========================================================================
// Replay payoff (v0.5) — unchanged pure seams.
// ===========================================================================

/// Draw the replay payoff (the open position at the head) for `state` into `area`
/// — a pure render. Placeholder body until v0.5 (`docs/ROADMAP.md`).
pub fn draw_replay(_state: &ReplayState, frame: &mut Frame, area: Rect) {
    super::placeholder_body(
        frame,
        area,
        "Payoff",
        "replay payoff at the head lands in v0.5",
    );
}

/// Handle a replay-payoff-local key, returning any follow-on [`AppEvent`]. Pure —
/// no I/O. Lands with the replay payoff (v0.5).
#[must_use]
pub fn handle_key_replay(_state: &mut ReplayState, _key: KeyEvent) -> Option<AppEvent> {
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::OptionStyle;
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::Positive;
    use proptest::prelude::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::{draw, handle_key};
    use crate::app::{BuilderLeg, LegError, LegFocus, LiveState, Side, SourceBinding};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, ProviderId, StreamHealth,
    };
    use crate::config::ThemeChoice;
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crate::ui::theme::Theme;

    const EXP: i64 = 1_700_000_000;
    /// A strike present in the chain WITH a mark.
    const FULL_A: f64 = 60_000.0;
    /// A second strike present in the chain WITH a mark.
    const FULL_B: f64 = 62_000.0;
    /// A strike present in the chain but WITHOUT a mark (no bids/asks).
    const BARE: f64 = 64_000.0;

    // --- Constructors (no unwrap/expect/indexing per the ruleset) ------------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("invalid provider id `{id}`: {e}"),
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

    /// A strike row with both call/put mids populated.
    fn full_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    /// A strike row with no quotes, so `call_middle`/`put_middle` stay `None` — the
    /// "no mark" case validation rejects.
    fn bare_row(strike: f64) -> OptionData {
        OptionData {
            strike_price: pos(strike),
            ..Default::default()
        }
    }

    fn chain() -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(FULL_A), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(full_row(FULL_A));
        let _ = chain.options.insert(full_row(FULL_B));
        let _ = chain.options.insert(bare_row(BARE));
        chain
    }

    fn store() -> ChainStore {
        ChainStore::seed(
            ChainFetch::new(
                chain(),
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        )
    }

    fn caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .greeks(GreeksCapability::Provided)
            .build()
    }

    fn live_state() -> LiveState {
        LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store(),
        )
    }

    fn press(state: &mut LiveState, code: KeyCode) {
        let _ = handle_key(state, KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// The leg at `idx` (a `Copy` [`BuilderLeg`]), via `.get()` — never an unchecked
    /// index (per the ruleset).
    #[track_caller]
    fn nth_leg(state: &LiveState, idx: usize) -> BuilderLeg {
        match state.payoff_builder.legs().get(idx) {
            Some(leg) => *leg,
            None => panic!("expected a leg at index {idx}"),
        }
    }

    /// Focus `row` on the chain so `a` appends that strike.
    fn focus(state: &mut LiveState, row: usize) {
        state.selection.focused_row = Some(row);
    }

    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        }
    }

    #[track_caller]
    fn render(state: &LiveState, width: u16, height: u16) {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let mut term = terminal(width, height);
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, frame, area, theme)) {
            Ok(_) => {}
            Err(e) => panic!("payoff draw failed: {e}"),
        }
    }

    // --- `a` appends the focused leg, in order -------------------------------

    #[test]
    fn test_add_leg_appends_focused_strike_and_style_in_order() {
        let mut state = live_state();
        // Focus the first strike as a call, add it.
        focus(&mut state, 0);
        state.selection.focused_leg = LegFocus::Call;
        press(&mut state, KeyCode::Char('a'));
        // Focus the second strike as a put, add it.
        focus(&mut state, 1);
        state.selection.focused_leg = LegFocus::Put;
        press(&mut state, KeyCode::Char('a'));

        assert_eq!(state.payoff_builder.legs().len(), 2, "two legs appended");
        assert_eq!(
            nth_leg(&state, 0).strike,
            pos(FULL_A),
            "first leg is the first focus"
        );
        assert_eq!(nth_leg(&state, 0).style, OptionStyle::Call);
        assert_eq!(
            nth_leg(&state, 1).strike,
            pos(FULL_B),
            "second leg is the second focus"
        );
        assert_eq!(nth_leg(&state, 1).style, OptionStyle::Put);
        // The cursor tracks the most recently added leg.
        assert_eq!(state.payoff_builder.cursor(), 1);
        // A fresh leg is long, quantity one.
        assert_eq!(nth_leg(&state, 1).side, Side::Buy);
        assert_eq!(nth_leg(&state, 1).qty, 1);
    }

    #[test]
    fn test_add_leg_with_no_focus_falls_back_to_atm() {
        let mut state = live_state();
        // No focused row: `a` still appends the nearest-spot strike (FULL_A = spot).
        assert!(state.selection.focused_row.is_none());
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(state.payoff_builder.legs().len(), 1);
        assert_eq!(nth_leg(&state, 0).strike, pos(FULL_A));
    }

    // --- `x`/`+`/`-`/`s` edit ONLY the cursor leg ----------------------------

    #[test]
    fn test_edits_touch_only_the_cursor_leg() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // leg 0
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // leg 1 (cursor here)

        // `+` twice, `s` once — all on the cursor leg (index 1).
        press(&mut state, KeyCode::Char('+'));
        press(&mut state, KeyCode::Char('+'));
        press(&mut state, KeyCode::Char('s'));

        assert_eq!(nth_leg(&state, 0).qty, 1, "leg 0 quantity is untouched");
        assert_eq!(
            nth_leg(&state, 0).side,
            Side::Buy,
            "leg 0 side is untouched"
        );
        assert_eq!(
            nth_leg(&state, 1).qty,
            3,
            "only the cursor leg's quantity changed"
        );
        assert_eq!(
            nth_leg(&state, 1).side,
            Side::Sell,
            "only the cursor leg's side toggled"
        );

        // `-` once brings the cursor leg back to 2.
        press(&mut state, KeyCode::Char('-'));
        assert_eq!(nth_leg(&state, 1).qty, 2);
    }

    #[test]
    fn test_remove_cursor_leg_keeps_cursor_in_bounds() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // leg 0
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // leg 1 (cursor)
        assert_eq!(state.payoff_builder.cursor(), 1);

        // Remove the cursor leg: it steps back to the new last leg.
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.payoff_builder.legs().len(), 1);
        assert_eq!(
            state.payoff_builder.cursor(),
            0,
            "cursor clamps into bounds"
        );
        assert_eq!(nth_leg(&state, 0).strike, pos(FULL_A));

        // Remove the last remaining leg → empty, cursor at 0.
        press(&mut state, KeyCode::Char('x'));
        assert!(state.payoff_builder.is_empty());
        assert_eq!(state.payoff_builder.cursor(), 0);
        // A further remove on the empty builder is a safe no-op.
        press(&mut state, KeyCode::Char('x'));
        assert!(state.payoff_builder.is_empty());
    }

    // --- `Enter` validation: valid commits, invalid does not -----------------

    #[test]
    fn test_enter_commits_a_valid_strategy() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // FULL_A, has a mark
        focus(&mut state, 1);
        press(&mut state, KeyCode::Char('a')); // FULL_B, has a mark
        press(&mut state, KeyCode::Enter);

        match state.payoff_builder.committed() {
            Some(committed) => assert_eq!(committed.legs().len(), 2),
            None => panic!("a valid strategy must commit"),
        }
        assert!(
            state.payoff_builder.errors().is_empty(),
            "no errors on commit"
        );
    }

    #[test]
    fn test_validate_rejects_a_stale_stream_mark_with_stale_mark() {
        use crate::chain::{InstrumentKey, QuoteClocks};
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // FULL_A, has a mark
        let chain = state.store.chain();
        let (strike, style) = match state.payoff_builder.legs().first() {
            Some(leg) => (leg.strike, leg.style),
            None => panic!("a leg was appended"),
        };
        // The leg's stream quote was received 120s before as_of - far past
        // QUOTE_STALE_AFTER (5s) - so validation must reject it with StaleMark
        // rather than committing a cached midpoint from a dead feed.
        let as_of = utc(EXP);
        let mut clocks = QuoteClocks::new();
        clocks.insert(
            InstrumentKey {
                underlying: chain.symbol.clone(),
                expiration_utc: match chain.get_expiration() {
                    Some(optionstratlib::ExpirationDate::DateTime(dt)) => dt,
                    other => panic!("fixture chain must carry an absolute expiry, got {other:?}"),
                },
                strike,
                style,
            },
            as_of - chrono::Duration::seconds(120),
        );
        match state.payoff_builder.validate(chain, &clocks, as_of) {
            Err(errors) => assert!(
                errors
                    .iter()
                    .any(|e| matches!(e, LegError::StaleMark { idx: 0 })),
                "the stale leg reports StaleMark, got {errors:?}"
            ),
            Ok(_) => panic!("a stale-marked leg must not validate"),
        }
        // The SAME leg with no recorded clock (poll-seeded) still validates - the
        // #24 ungated convention.
        let empty = QuoteClocks::new();
        assert!(
            state.payoff_builder.validate(chain, &empty, as_of).is_ok(),
            "a leg with no stream clock is not gated"
        );
    }

    #[test]
    fn test_enter_on_empty_reports_error_and_does_not_commit() {
        let mut state = live_state();
        press(&mut state, KeyCode::Enter);
        assert!(
            state.payoff_builder.committed().is_none(),
            "empty never commits"
        );
        assert_eq!(state.payoff_builder.errors(), &[LegError::Empty]);
    }

    #[test]
    fn test_enter_on_zero_qty_reports_error_and_does_not_commit() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a')); // qty 1, has a mark
        press(&mut state, KeyCode::Char('-')); // qty 0
        assert_eq!(nth_leg(&state, 0).qty, 0);
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_none());
        assert_eq!(
            state.payoff_builder.errors(),
            &[LegError::ZeroQty { idx: 0 }]
        );
    }

    #[test]
    fn test_enter_on_missing_mark_reports_error_and_does_not_commit() {
        let mut state = live_state();
        // Index 2 is the bare strike with no mark.
        focus(&mut state, 2);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_none());
        assert_eq!(
            state.payoff_builder.errors(),
            &[LegError::NoMark { idx: 0 }]
        );
    }

    #[test]
    fn test_edit_after_failed_commit_clears_stale_errors() {
        let mut state = live_state();
        focus(&mut state, 2);
        press(&mut state, KeyCode::Char('a')); // bare strike
        press(&mut state, KeyCode::Enter); // fails: no mark
        assert!(!state.payoff_builder.errors().is_empty());
        // Any edit clears the stale errors (and any stale commit).
        press(&mut state, KeyCode::Char('s'));
        assert!(state.payoff_builder.errors().is_empty());
    }

    #[test]
    fn test_edit_after_commit_clears_the_committed_strategy() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_some());
        // Editing invalidates the committed snapshot so #27 never draws a stale curve.
        press(&mut state, KeyCode::Char('+'));
        assert!(state.payoff_builder.committed().is_none());
    }

    // --- `Esc` discards → empty ----------------------------------------------

    #[test]
    fn test_esc_discards_to_the_empty_state() {
        let mut state = live_state();
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(state.payoff_builder.legs().len(), 2);
        press(&mut state, KeyCode::Esc);
        assert!(state.payoff_builder.is_empty(), "Esc clears the strategy");
        assert!(state.payoff_builder.committed().is_none());
        assert!(state.payoff_builder.errors().is_empty());
        assert_eq!(state.payoff_builder.cursor(), 0);
    }

    // --- `t` toggles the curve mode (state lives here; render in #27) ---------

    #[test]
    fn test_toggle_curve_flips_the_mode() {
        let mut state = live_state();
        let before = state.payoff_builder.curve();
        press(&mut state, KeyCode::Char('t'));
        assert_ne!(
            state.payoff_builder.curve(),
            before,
            "`t` toggles the curve mode"
        );
        press(&mut state, KeyCode::Char('t'));
        assert_eq!(state.payoff_builder.curve(), before, "`t` toggles back");
    }

    // --- Every builder state renders without panic ----------------------------

    #[test]
    fn test_render_empty_partial_invalid_committed_states() {
        // Empty.
        let mut state = live_state();
        render(&state, 80, 24);
        // Partial (in progress).
        focus(&mut state, 0);
        press(&mut state, KeyCode::Char('a'));
        render(&state, 80, 24);
        // Invalid (a failed commit with an inline error).
        focus(&mut state, 2);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        render(&state, 80, 24);
        // Committed (edit the bare leg out, then commit the valid remainder).
        press(&mut state, KeyCode::Char('x')); // drop the bare leg (cursor on it)
        press(&mut state, KeyCode::Enter);
        assert!(state.payoff_builder.committed().is_some());
        render(&state, 80, 24);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// Any sequence of builder keys, over any focused row, leaves a builder state
        /// that draws into a `TestBackend` without panic (`docs/TESTING.md` §3,
        /// `render_never_panics`) — empty, partial, invalid, or committed.
        #[test]
        fn prop_payoff_builder_any_key_sequence_renders(
            keys in proptest::collection::vec(0u8..8, 0..24),
            focus_row in 0usize..4,
            width in 40u16..120,
            height in 8u16..40,
        ) {
            let mut state = live_state();
            state.selection.focused_row = Some(focus_row);
            for k in keys {
                let code = match k {
                    0 => KeyCode::Char('a'),
                    1 => KeyCode::Char('x'),
                    2 => KeyCode::Char('+'),
                    3 => KeyCode::Char('-'),
                    4 => KeyCode::Char('s'),
                    5 => KeyCode::Enter,
                    6 => KeyCode::Esc,
                    _ => KeyCode::Char('t'),
                };
                press(&mut state, code);
            }
            let theme = Theme::resolve(ThemeChoice::Auto, false);
            let mut term = terminal(width, height);
            let area = Rect::new(0, 0, width, height);
            match term.draw(|frame| draw(&state, frame, area, theme)) {
                Ok(_) => {}
                Err(e) => prop_assert!(false, "payoff draw failed at {width}x{height}: {e}"),
            }
        }
    }
}
