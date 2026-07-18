//! The order-book depth-ladder screen (`docs/05-views-and-ux.md` §2.1, §6;
//! `docs/03-data-providers.md` §8).
//!
//! The Live `Depth` screen renders the bid/ask ladder for the **selected contract**
//! — the chain cursor's focused strike row and its focused call/put leg — read from
//! the domain [`DepthStore`](crate::chain::DepthStore) on [`LiveState`]. Depth is
//! **instrument-scoped and capability-gated**: it is verified on Deribit option
//! instruments only, so the screen is normally reachable only where the effective
//! [`ProviderCapabilities`](crate::providers::ProviderCapabilities) declare `depth`.
//! It gates on that **capability**, never on a `ProviderId`, so a built-in and an
//! external depth-capable provider render identically.
//!
//! # States first, then the ladder
//!
//! [`draw`] renders — in order — the honest **"depth not available"** body (a
//! defensive capability check; #50 goldens this render), the **loading** spinner, the
//! provider **error** message, the **"select a contract"** empty state (no strike
//! focused yet), the **"no book yet"** empty state (a contract but no book), and only
//! then the populated ladder. A `change_id` discontinuity badges the ladder
//! **"resyncing"** and dims it, never presenting a discontinuous book as trusted.
//! The draw path is **pure** (`&LiveState` + `Copy` [`Theme`]/tick): no I/O, no
//! `.await`, no state mutation (`docs/02-tui-architecture.md` §7).
//!
//! # Honest, `NO_COLOR`-safe, escape-hygienic
//!
//! Each ladder row carries a `bid`/`ask` **text** side label so the side survives a
//! monochrome terminal (color only reinforces, [`Theme::book_side_style`]). Every
//! value the venue supplies is a `Positive`, so there is no fabricated `0`; a
//! non-finite sentinel guards to `—`. The venue-controlled instrument name is routed
//! through the shared [`sanitize`] at the render edge, so a hostile symbol renders as
//! inert text — never a cursor move or a torn layout (`docs/SECURITY.md` §6.4).

use crossterm::event::KeyEvent;
use optionstratlib::OptionStyle;
use optionstratlib::prelude::{Decimal, Positive};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use crate::app::keymap::{DepthAction, KeyChord, resolve_depth};
use crate::app::{LiveState, ScreenLoad};
use crate::chain::{DepthBook, DepthLevel, InstrumentKey, StreamHealth};
use crate::event::AppEvent;
use crate::ui::theme::{Theme, health_span, sanitize, spinner_frame};

/// The em dash rendered for a non-finite sentinel — never a fabricated `0`
/// (`docs/01-domain-model.md` §5).
const EM_DASH: &str = "—";

/// The fixed column width of a price cell.
const PRICE_W: u16 = 12;
/// The fixed column width of a size cell.
const SIZE_W: u16 = 12;
/// The fixed column width of the `bid`/`ask` side cell.
const SIDE_W: u16 = 5;

// ===========================================================================
// The draw entry point + states (states first).
// ===========================================================================

/// Draw the depth ladder for the live `state` into `area` — a pure render over the
/// borrowed [`LiveState`] and the `Copy` [`Theme`]/`tick` (`docs/02-tui-architecture.md`
/// §7). States render before the populated ladder (the states-first rule); the depth
/// store is borrowed, never recomputed, and no book merge happens here.
pub fn draw(state: &LiveState, frame: &mut Frame, area: Rect, theme: Theme, tick: u64) {
    // Resolve the selected contract + its book once (a cheap bounded lookup, off the
    // hot merge path).
    let selected = state.selected_depth_key();
    let book = selected
        .as_ref()
        .and_then(|key| state.depth_store.book(key));

    let block = Block::bordered().title(title_line(state, selected.as_ref(), book, theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 1. Capability gate (defensive). Navigation already skips a depth-less provider
    //    (`Tab` skips it, its number key flashes a hint), so this is normally
    //    unreachable — but the draw honestly renders the "not available" body if
    //    reached, gating on the declared `depth` capability, never on a `ProviderId`
    //    (#50 captures this render as the `depth/depth_unavailable` golden).
    if !state.effective_capabilities().depth {
        draw_unavailable(state, frame, inner, theme);
        return;
    }

    // 2. + 3. The load lifecycle, then the data prerequisites.
    match &state.load {
        ScreenLoad::Loading => draw_loading(state, frame, inner, theme, tick),
        ScreenLoad::Error { message } => draw_error(frame, inner, theme, message),
        ScreenLoad::Ready => match (selected.as_ref(), book) {
            // No strike focused yet — nudge the user to pick one on the chain.
            (None, _) => draw_state_body(
                frame,
                inner,
                Text::from(vec![
                    Line::from(Span::styled("select a contract", theme.dim())),
                    Line::from(Span::styled(
                        "focus a strike on the Chain screen (press 1)",
                        theme.dim(),
                    )),
                ]),
            ),
            // A contract is focused but no book has streamed for it yet.
            (Some(_), None) => draw_state_body(
                frame,
                inner,
                Text::from(vec![
                    Line::from(Span::styled("no book yet", theme.dim())),
                    Line::from(Span::styled("waiting for the order book", theme.dim())),
                ]),
            ),
            // A book with no levels (an empty venue book) — an honest empty, not a
            // fabricated ladder.
            (Some(_), Some(book)) if book.level_count() == 0 => draw_state_body(
                frame,
                inner,
                Text::from(vec![
                    Line::from(Span::styled("no book levels", theme.dim())),
                    Line::from(Span::styled("the venue sent an empty book", theme.dim())),
                ]),
            ),
            (Some(_), Some(book)) => draw_ladder(state, frame, inner, theme, book),
        },
    }
}

/// The framed-block title: `Depth  <contract>` plus the `resyncing` badge and the
/// source stream-health badge when either applies. The contract is the venue
/// instrument name once a book exists (**sanitized** at this render edge), else the
/// domain-formatted `<strike> C/P` of the selected key — no venue bytes in the
/// pre-book states.
#[must_use]
fn title_line(
    state: &LiveState,
    selected: Option<&InstrumentKey>,
    book: Option<&DepthBook>,
    theme: Theme,
) -> Line<'static> {
    let mut spans = vec![Span::styled("Depth", theme.accent())];
    if let Some(book) = book {
        spans.push(Span::raw("  "));
        spans.push(Span::raw(sanitize(&book.ladder().instrument.native_symbol)));
    } else if let Some(key) = selected {
        spans.push(Span::raw("  "));
        spans.push(Span::raw(fmt_contract(key)));
    }
    if book.is_some_and(DepthBook::needs_resync) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("↻ resyncing", theme.warning()));
    }
    if !matches!(state.source.health, StreamHealth::Live) {
        spans.push(Span::raw("  "));
        spans.push(health_span(&state.source.health, theme));
    }
    Line::from(spans)
}

/// The loading state: a tick-driven spinner + "connecting to `<provider>`…".
fn draw_loading(state: &LiveState, frame: &mut Frame, inner: Rect, theme: Theme, tick: u64) {
    draw_state_body(
        frame,
        inner,
        Text::from(vec![
            Line::from(Span::styled(
                format!(
                    "{} connecting to {}…",
                    spinner_frame(tick),
                    sanitize(state.source.provider.as_str())
                ),
                theme.accent(),
            )),
            Line::from(Span::styled("waiting for the order book", theme.dim())),
        ]),
    );
}

/// The provider-error state: the actionable message + the `r` reconnect affordance.
fn draw_error(frame: &mut Frame, inner: Rect, theme: Theme, message: &str) {
    draw_state_body(
        frame,
        inner,
        Text::from(vec![
            Line::from(Span::styled(
                format!("! {}", sanitize(message)),
                theme.warning(),
            )),
            Line::from(Span::styled("press r to reconnect", theme.dim())),
        ]),
    );
}

/// The honest "depth not available on `<provider>`" capability state — never a blank
/// or a fabricated ladder (`docs/05-views-and-ux.md` §6). Gated on the declared
/// `depth` capability, so the reason is generic (no `ProviderId` special-casing).
fn draw_unavailable(state: &LiveState, frame: &mut Frame, inner: Rect, theme: Theme) {
    draw_state_body(
        frame,
        inner,
        Text::from(vec![
            Line::from(Span::styled(
                format!(
                    "depth not available on {}",
                    sanitize(state.source.provider.as_str())
                ),
                theme.warning(),
            )),
            Line::from(Span::styled(
                "this venue exposes no option order book",
                theme.dim(),
            )),
        ]),
    );
}

// ===========================================================================
// The populated ladder.
// ===========================================================================

/// Draw the bid/ask ladder for `book`: asks worst-first at the top down to the best
/// ask, then the bids best-first — so the whole ladder reads top-to-bottom as a
/// descending price axis — with a spread footer. `↑↓ / kj` scroll (the offset lives
/// on [`LiveState`], clamped here); before the first scroll the window centers the
/// inside market (#48 P3-02). A `change_id` discontinuity **or** a non-live source
/// health dims the ladder, mirroring the chain's stale idiom; the `resyncing`/health
/// badge in the title carries the honest state (#48 P2-01).
fn draw_ladder(state: &LiveState, frame: &mut Frame, inner: Rect, theme: Theme, book: &DepthBook) {
    let ladder = book.ladder();

    // The ladder body (with a column header row) and the one-line spread footer.
    let (body_area, footer_area) = depth_body_split(inner);

    // The ordered display list: asks reversed (highest → best), then bids
    // (best → lowest). References only — O(levels) to order, and the per-row
    // formatting below runs for the VISIBLE window only (the frame budget).
    let mut order: Vec<(&DepthLevel, bool)> = Vec::with_capacity(book.level_count());
    order.extend(ladder.asks.iter().rev().map(|level| (level, false)));
    order.extend(ladder.bids.iter().map(|level| (level, true)));

    // The visible window: the body height minus the one header row the table draws.
    let visible = visible_rows_of(body_area);
    // The first visible row: the explicit user scroll offset (clamped to the window),
    // or — before the user has scrolled (`None`) — the inside-market anchor that keeps
    // the tradeable levels on-screen without scrolling (#48 P3-02).
    let start = match state.depth_scroll {
        Some(scroll) => clamp_start(scroll, visible, order.len()),
        None => inside_market_anchor(book, visible),
    };

    let widths = [
        Constraint::Length(PRICE_W),
        Constraint::Length(SIZE_W),
        Constraint::Length(SIDE_W),
    ];
    let header = Row::new([
        Cell::from(Line::from("Price").alignment(Alignment::Right)),
        Cell::from(Line::from("Size").alignment(Alignment::Right)),
        Cell::from(Line::from("Side").alignment(Alignment::Left)),
    ])
    .style(theme.accent());

    let rows: Vec<Row> = order
        .iter()
        .skip(start)
        .take(visible)
        .map(|&(level, is_bid)| ladder_row(level, is_bid, theme))
        .collect();

    let mut table = Table::new(rows, widths).header(header).column_spacing(1);
    // A discontinuous book OR a non-live source health dims the ladder — the last
    // values are shown but visibly untrusted, mirroring the chain's stale idiom
    // (`chain::draw_matrix`, `docs/05` §6) so the title's `resyncing`/health badge is
    // never contradicted by a bright body (#48 P2-01). A `change_id` gap and a
    // stale/reconnecting source are both honestly dimmed.
    let stale = book.needs_resync() || !matches!(state.source.health, StreamHealth::Live);
    if stale {
        table = table.style(theme.dim());
    }
    frame.render_widget(table, body_area);

    frame.render_widget(
        Paragraph::new(footer_line(ladder.bids.first(), ladder.asks.first(), theme)),
        footer_area,
    );
}

/// One ladder row (`Price  Size  Side`), styled by side — green bid / red ask — with
/// the `bid`/`ask` text label carrying the side under `NO_COLOR`
/// ([`Theme::book_side_style`]).
#[must_use]
fn ladder_row(level: &DepthLevel, is_bid: bool, theme: Theme) -> Row<'static> {
    let side_label = if is_bid { "bid" } else { "ask" };
    Row::new([
        Cell::from(Line::from(fmt_num(level.price)).alignment(Alignment::Right)),
        Cell::from(Line::from(fmt_num(level.size)).alignment(Alignment::Right)),
        Cell::from(Line::from(side_label).alignment(Alignment::Left)),
    ])
    .style(theme.book_side_style(is_bid))
}

/// The spread footer: the best bid / best ask and their spread, or an honest note
/// when a side is empty — every price the em-dash-guarded [`fmt_num`], never a `0`.
#[must_use]
fn footer_line(
    best_bid: Option<&DepthLevel>,
    best_ask: Option<&DepthLevel>,
    theme: Theme,
) -> Line<'static> {
    let text = match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => format!(
            "bid {} / ask {}   spread {}",
            fmt_num(bid.price),
            fmt_num(ask.price),
            fmt_spread(ask.price, bid.price),
        ),
        (Some(bid), None) => format!("bid {}   (no asks)", fmt_num(bid.price)),
        (None, Some(ask)) => format!("ask {}   (no bids)", fmt_num(ask.price)),
        (None, None) => "no levels".to_owned(),
    };
    Line::from(Span::styled(text, theme.dim()))
}

/// Draw a centered two-line state body (loading / error / empty / unavailable)
/// inside the framed block — a first-class, deliberate-looking state, never a blank
/// void or a top-anchored fragment (`docs/05-views-and-ux.md` §6). Mirrors the
/// chain/surface state bodies.
fn draw_state_body(frame: &mut Frame, inner: Rect, text: Text<'static>) {
    let height = u16::try_from(text.height())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let [centered] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), centered);
}

// ===========================================================================
// Formatting helpers — `—` never a fabricated `0`.
// ===========================================================================

/// The decimals a value at or above `1.0` renders — the familiar money precision
/// for an index/underlying-scale price (`60000.00`), where two decimals is right.
const WHOLE_DECIMALS: usize = 2;
/// The ceiling on rendered decimals, so a pathological sub-cent venue price never
/// widens the price cell past its column ([`PRICE_W`]).
const MAX_DECIMALS: usize = 8;

/// Format a `Positive` price/size with **venue-scale-aware** precision (issue #109,
/// #118), guarding the non-finite [`Positive::INFINITY`] sentinel to `—` so it never
/// paints (rule: guard `f64` `NaN`/`Inf` before a widget).
///
/// The decimal count scales to the value's magnitude: a value `>= 1` renders at the
/// familiar [`WHOLE_DECIMALS`] cents precision, while a **sub-unit** value renders at
/// its own exact decimal scale (capped at [`MAX_DECIMALS`]) — so a fractional-coin BTC
/// option price (`0.049`) keeps its tradeable digits instead of truncating to `0.04`,
/// and two distinct executable prices near one (`0.9994` vs `0.9995`) never collapse to
/// the same string. This is a **render-edge** display precision only; the underlying
/// value stays the exact `Positive` the venue supplied.
#[must_use]
fn fmt_num(value: Positive) -> String {
    if value == Positive::INFINITY {
        return EM_DASH.to_owned();
    }
    let places = price_decimals(value.to_dec());
    format!("{value:.places$}")
}

/// The render-edge decimal count for a price/size, scaled to its magnitude
/// (issue #109, #118). A value at or above `1.0` (or a non-positive degenerate value)
/// renders at [`WHOLE_DECIMALS`]; a **sub-unit** value renders at its OWN decimal scale
/// (the exact scale of the `Positive`/[`Decimal`] the venue supplied), capped at
/// [`MAX_DECIMALS`].
///
/// Preserving the value's own scale — rather than counting significant figures from the
/// first non-zero digit — is what keeps two distinct executable prices near one
/// (`0.9994` and `0.9995`, both scale `4`) rendering distinctly instead of collapsing to
/// a shared 3-significant-figure `0.999`; the column budget still caps the width.
#[must_use]
fn price_decimals(value: Decimal) -> usize {
    if value >= Decimal::ONE || value <= Decimal::ZERO {
        return WHOLE_DECIMALS;
    }
    usize::try_from(value.scale())
        .unwrap_or(MAX_DECIMALS)
        .min(MAX_DECIMALS)
}

/// The bid/ask spread (`ask − bid`) at the same venue-scale-aware precision as the
/// ladder prices ([`price_decimals`]), or `—` when non-finite or crossed — computed
/// with checked [`Decimal`] arithmetic so it never panics.
#[must_use]
fn fmt_spread(ask: Positive, bid: Positive) -> String {
    if ask == Positive::INFINITY || bid == Positive::INFINITY {
        return EM_DASH.to_owned();
    }
    match ask.to_dec().checked_sub(bid.to_dec()) {
        Some(spread) if spread >= Decimal::ZERO => {
            let places = price_decimals(spread);
            format!("{spread:.places$}")
        }
        _ => EM_DASH.to_owned(),
    }
}

/// The domain-formatted contract label for the title before a book exists:
/// `<strike> C/P` — a domain value (no venue bytes), the strike em-dash-guarded.
#[must_use]
fn fmt_contract(key: &InstrumentKey) -> String {
    let style = match key.style {
        OptionStyle::Call => "C",
        OptionStyle::Put => "P",
    };
    format!("{} {}", fmt_num(key.strike), style)
}

/// `a − b` floored at zero, spelled so it can never underflow — the ruleset bans
/// `saturating_sub` and `checked_sub(..).unwrap_or(0)` trips clippy's
/// manual-saturating lint, so the floor is `a.max(b) − b` (mirrors `ui::chain`).
#[must_use]
fn floor_sub(a: usize, b: usize) -> usize {
    a.max(b) - b
}

/// Split the framed block `inner` into the ladder body and the one-line spread
/// footer — the single layout both the draw ([`draw_ladder`]) and the render loop's
/// off-draw geometry stash ([`body_visible_rows`]) read, so the two can never drift
/// (#48 P2-02).
#[must_use]
fn depth_body_split(inner: Rect) -> (Rect, Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    (body, footer)
}

/// The number of ladder rows a `body_area` fits: its height minus the one header row
/// the table draws. The one place the header-row reservation is spelled, shared by
/// the draw and the geometry stash.
#[must_use]
fn visible_rows_of(body_area: Rect) -> usize {
    floor_sub(usize::from(body_area.height), 1)
}

/// The number of ladder rows visible in the depth body for the depth screen's outer
/// `area` (the root body): a bordered block, minus the one-line spread footer, minus
/// the one header row. The render loop calls this **after** a draw to stash the last
/// viewport height onto [`LiveState`](crate::app::LiveState) off the pure draw
/// (`src/ui/driver.rs`), so the scroll clamp can couple to the viewport without the
/// draw path mutating state (#48 P2-02).
#[must_use]
pub fn body_visible_rows(area: Rect) -> usize {
    let (body_area, _footer) = depth_body_split(Block::bordered().inner(area));
    visible_rows_of(body_area)
}

/// The first visible ladder row so `scroll` stays in range: `0` when everything
/// fits, else `scroll` clamped to `[0, total − visible]` — a top-anchored scroll,
/// never an out-of-range index.
#[must_use]
fn clamp_start(scroll: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    scroll.min(floor_sub(total, visible))
}

/// The default window start that centers the **inside market** (best bid/ask) in a
/// `visible`-row viewport, so the tradeable levels are on-screen without scrolling
/// (#48 P3-02). The best-ask row sits at index `asks.len() − 1` in the
/// asks-reversed-then-bids display order; anchoring half a viewport above it puts the
/// best-ask/best-bid boundary near the middle. Floored at `0` and clamped to
/// `[0, total − visible]` — the same bound [`clamp_start`] and `scroll` use — so the
/// anchor reconciles with the scroll clamp and never over-scrolls; `0` when the whole
/// book fits or the viewport is unknown.
#[must_use]
fn inside_market_anchor(book: &DepthBook, visible: usize) -> usize {
    let total = book.level_count();
    if visible == 0 || total <= visible {
        return 0;
    }
    let best_ask_index = floor_sub(book.ladder().asks.len(), 1);
    floor_sub(best_ask_index, visible / 2).min(floor_sub(total, visible))
}

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a depth-local key, returning any follow-on [`AppEvent`]
/// (`docs/02-tui-architecture.md` §9). Pure over `&mut LiveState` — no I/O, no
/// `.await`.
///
/// The chord resolves **through the single keybinding map**
/// ([`resolve_depth`](crate::app::keymap::resolve_depth), `src/app/keymap.rs`), so
/// the dispatch and the help overlay read one table and cannot drift. `↑`/`↓` scroll
/// the ladder (the concrete chord chooses the direction), mutating the
/// application-layer scroll offset on [`LiveState`]; `handle_key` returns `None` and
/// the render loop diffs the offset (its view signature) to redraw
/// (`docs/02-tui-architecture.md` §8).
#[must_use]
pub fn handle_key(state: &mut LiveState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_depth(chord)? {
        DepthAction::Scroll => {
            // `↓`/`j` scroll down, `↑`/`k` up — the chain's `kj` idiom (#48 P3-01).
            scroll(state, matches!(chord, KeyChord::Down | KeyChord::Char('j')));
            None
        }
    }
}

/// Scroll the ladder one row down (`down`) or up, clamped to the **viewport** so it
/// can never scroll past the last screenful — a no-op when no contract/book is
/// selected. Off the draw path.
///
/// The max offset is `total − visible_rows` (the last screenful, `0` when the book
/// fits), reading the viewport height the render loop stashed after the last draw
/// ([`LiveState::depth_visible_rows`](crate::app::LiveState)); so an at-limit press
/// on a deep book, or **any** press on a book that fits, leaves the offset unchanged
/// — no [`ViewSig`](crate::app) flip, no busy redraw (#48 P2-02). Before the first
/// scroll (`None`) the offset seeds from the inside-market anchor, so `↓` first steps
/// down from the centered view rather than jumping to the top (#48 P3-02).
fn scroll(state: &mut LiveState, down: bool) {
    let visible = state.depth_visible_rows;
    let Some((total, anchor)) = state
        .selected_depth_key()
        .as_ref()
        .and_then(|key| state.depth_store.book(key))
        .map(|book| (book.level_count(), inside_market_anchor(book, visible)))
    else {
        // No contract/book selected — nothing to scroll.
        return;
    };
    let max = floor_sub(total, visible);
    let current = state.depth_scroll.unwrap_or(anchor);
    let next = if down {
        current.checked_add(1).unwrap_or(current).min(max)
    } else {
        floor_sub(current, 1)
    };
    // Only record a real move: leaving an unscrolled (`None`) ladder that cannot move
    // as `None` keeps its view signature stable, so a dead-end press never flips it
    // and forces a redraw (#48 P2-02).
    if next != current {
        state.depth_scroll = Some(next);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyModifiers};
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::{Decimal, Positive};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;

    use super::{
        body_visible_rows, clamp_start, draw, fmt_num, fmt_spread, handle_key, inside_market_anchor,
    };
    use crate::app::{LiveScreen, LiveState, ScreenLoad, SourceBinding};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ContractSpecFingerprint, DepthLadder,
        DepthLevel, ExerciseStyle, ExpirySource, Instrument, InstrumentKey, ProviderId,
        SettlementStyle, StreamHealth,
    };
    use crate::config::ThemeChoice;
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crate::ui::golden::buffer_to_text;
    use crate::ui::theme::Theme;

    const EXP: i64 = 1_700_000_000;

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

    /// A `Positive` from an exact `mantissa × 10^-scale` decimal, so a sub-unit price
    /// assertion (`0.049`) is byte-exact and free of `f64` conversion drift.
    #[track_caller]
    fn posd(mantissa: i64, scale: u32) -> Positive {
        match Positive::new_decimal(Decimal::new(mantissa, scale)) {
            Ok(p) => p,
            Err(e) => panic!("invalid test decimal positive: {e}"),
        }
    }

    fn row(strike: f64) -> OptionData {
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

    fn store() -> ChainStore {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(row(60_000.0));
        let _ = chain.options.insert(row(62_000.0));
        ChainStore::seed(
            ChainFetch::new(
                chain,
                ExpirySource::new("BTC", utc(EXP), pid("deribit")),
                AliasCatalog::new(),
            ),
            ChainSource::Merged,
            Duration::from_secs(2),
            utc(EXP),
        )
    }

    fn caps(depth: bool) -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(depth)
            .greeks(GreeksCapability::Provided)
            .build()
    }

    /// A live state on the Depth screen (Ready), with a two-strike chain. Depth is
    /// capable by default; the caller focuses a contract and seeds a book.
    fn depth_state(load: ScreenLoad, depth_capable: bool) -> LiveState {
        let mut state = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(depth_capable), StreamHealth::Live),
            store(),
        );
        state.screen = LiveScreen::Depth;
        state.load = load;
        state
    }

    /// A ladder for the given contract with `n_bids` / `n_asks` levels and a
    /// venue-controlled `native_symbol` (for the escape-hygiene probe).
    fn ladder_for(
        key: &InstrumentKey,
        native_symbol: &str,
        n_bids: usize,
        n_asks: usize,
        change_id: Option<u64>,
    ) -> DepthLadder {
        let instrument = Instrument {
            key: key.clone(),
            provider: pid("deribit"),
            native_symbol: native_symbol.to_owned(),
            stream_symbol: None,
            spec: ContractSpecFingerprint {
                contract_multiplier: 1,
                settlement: SettlementStyle::Cash,
                exercise: ExerciseStyle::European,
                quote_currency: "USD".to_owned(),
                venue_product_code: "BTC".to_owned(),
            },
        };
        let bids = (0..n_bids)
            .map(|i| DepthLevel {
                price: pos(60_000.0 - i as f64 * 10.0),
                size: pos(1.0 + i as f64),
            })
            .collect();
        let asks = (0..n_asks)
            .map(|i| DepthLevel {
                price: pos(60_010.0 + i as f64 * 10.0),
                size: pos(1.0 + i as f64),
            })
            .collect();
        DepthLadder {
            instrument,
            bids,
            asks,
            event_time: Some(utc(EXP)),
            received_time: utc(EXP + 1),
            change_id,
        }
    }

    #[track_caller]
    fn render_text(state: &LiveState, width: u16, height: u16) -> String {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let mut term = match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        };
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, frame, area, theme, 0)) {
            Ok(_) => {}
            Err(e) => panic!("depth draw failed: {e}"),
        }
        buffer_to_text(term.backend().buffer())
    }

    fn press(state: &mut LiveState, code: KeyCode) {
        let _ = handle_key(
            state,
            crossterm::event::KeyEvent::new(code, KeyModifiers::NONE),
        );
    }

    /// A Ready, depth-capable state with a focused contract and a seeded book of
    /// `n_bids`/`n_asks` levels — the common populated fixture the scroll/dim tests
    /// share.
    fn populated_with(n_bids: usize, n_asks: usize) -> LiveState {
        let mut state = depth_state(ScreenLoad::Ready, true);
        state.selection.focused_row = Some(0);
        if let Some(key) = state.selected_depth_key() {
            let _ = state.depth_store.apply(ladder_for(
                &key,
                "BTC-27JUN25-60000-C",
                n_bids,
                n_asks,
                Some(1),
            ));
        }
        state
    }

    /// Whether any digit-bearing cell in the ladder BODY band (below the `y=1` header,
    /// above the always-dim spread footer near `y=height-2`) is `DIM`-styled — the
    /// honest stale/resync dim (#48 P2-01). The band `2..height/2` excludes the footer
    /// so its unconditional dim never false-positives.
    #[track_caller]
    fn body_is_dimmed(state: &LiveState, width: u16, height: u16) -> bool {
        let theme = Theme::resolve(ThemeChoice::Auto, false);
        let mut term = match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        };
        let area = Rect::new(0, 0, width, height);
        match term.draw(|frame| draw(state, frame, area, theme, 0)) {
            Ok(_) => {}
            Err(e) => panic!("depth draw failed: {e}"),
        }
        let buffer = term.backend().buffer();
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

    // --- Every state renders without panic -----------------------------------

    #[test]
    fn test_render_states_never_panic() {
        // Loading / error / select-a-contract / no-book / populated / unavailable, at
        // full and tiny sizes.
        render_text(&depth_state(ScreenLoad::Loading, true), 120, 40);
        render_text(
            &depth_state(
                ScreenLoad::Error {
                    message: "provider unreachable".to_owned(),
                },
                true,
            ),
            120,
            40,
        );
        // Ready, no contract focused → "select a contract".
        render_text(&depth_state(ScreenLoad::Ready, true), 120, 40);
        // Ready + focused contract but no book → "no book yet".
        let mut focused = depth_state(ScreenLoad::Ready, true);
        focused.selection.focused_row = Some(0);
        render_text(&focused, 120, 40);
        // Populated.
        let populated = populated_with(3, 3);
        render_text(&populated, 120, 40);
        render_text(&populated, 40, 8);
        // A non-Live health render (the stale-dim path, #48 P2-01/P3-03) — dimmed body
        // + badge, at full and tiny sizes.
        let mut stale = populated_with(3, 3);
        stale.source.health = StreamHealth::Stale { since: utc(EXP) };
        render_text(&stale, 120, 40);
        render_text(&stale, 40, 8);
        // A 0/0 ladder (an empty venue book) renders its honest empty, never a panic.
        let mut empty_book = depth_state(ScreenLoad::Ready, true);
        empty_book.selection.focused_row = Some(0);
        if let Some(key) = empty_book.selected_depth_key() {
            let _ = empty_book.depth_store.apply(ladder_for(
                &key,
                "BTC-27JUN25-60000-C",
                0,
                0,
                Some(1),
            ));
        }
        render_text(&empty_book, 120, 40);
        render_text(&empty_book, 40, 8);
        // A resync-dimmed tiny (40×8) render — a change_id regression flags the resync
        // dim on a near-minimal frame.
        let mut resync = populated_with(3, 3);
        if let Some(key) = resync.selected_depth_key() {
            let _ =
                resync
                    .depth_store
                    .apply(ladder_for(&key, "BTC-27JUN25-60000-C", 3, 3, Some(0)));
        }
        render_text(&resync, 40, 8);
        // Unavailable.
        render_text(&depth_state(ScreenLoad::Ready, false), 120, 40);
    }

    #[test]
    fn test_unavailable_body_renders_honestly() {
        // A depth-less provider's screen (defensive gate) names the honest state,
        // gating on the capability, never a fabricated ladder.
        let text = render_text(&depth_state(ScreenLoad::Ready, false), 120, 40);
        assert!(
            text.contains("not available"),
            "the unavailable body names itself: {text:?}",
        );
        assert!(text.contains("deribit"), "it names the provider");
    }

    #[test]
    fn test_select_a_contract_empty_state() {
        // Ready + depth-capable but no strike focused → the "select a contract" nudge,
        // not a blank or a fabricated ladder.
        let text = render_text(&depth_state(ScreenLoad::Ready, true), 120, 40);
        assert!(
            text.contains("select a contract"),
            "the empty state nudges the user: {text:?}",
        );
    }

    #[test]
    fn test_no_book_yet_empty_state() {
        let mut state = depth_state(ScreenLoad::Ready, true);
        state.selection.focused_row = Some(0);
        let text = render_text(&state, 120, 40);
        assert!(
            text.contains("no book yet"),
            "a focused contract with no book shows the awaiting state: {text:?}",
        );
    }

    #[test]
    fn test_populated_ladder_shows_bid_ask_and_prices() {
        let mut state = depth_state(ScreenLoad::Ready, true);
        state.selection.focused_row = Some(0);
        if let Some(key) = state.selected_depth_key() {
            let _ = state
                .depth_store
                .apply(ladder_for(&key, "BTC-27JUN25-60000-C", 2, 2, Some(1)));
        }
        let text = render_text(&state, 120, 40);
        assert!(text.contains("bid"), "the bid side label renders: {text:?}");
        assert!(text.contains("ask"), "the ask side label renders");
        assert!(text.contains("60000.00"), "a best-bid price renders");
        assert!(text.contains("spread"), "the spread footer renders");
        assert!(
            text.contains("BTC-27JUN25-60000-C"),
            "the instrument name is in the title",
        );
    }

    #[test]
    fn test_resync_badge_renders_on_change_id_gap() {
        let mut state = depth_state(ScreenLoad::Ready, true);
        state.selection.focused_row = Some(0);
        if let Some(key) = state.selected_depth_key() {
            // Seed, then a regressing change_id flags a resync.
            let _ =
                state
                    .depth_store
                    .apply(ladder_for(&key, "BTC-27JUN25-60000-C", 2, 2, Some(10)));
            let _ = state
                .depth_store
                .apply(ladder_for(&key, "BTC-27JUN25-60000-C", 2, 2, Some(3)));
        }
        let text = render_text(&state, 120, 40);
        assert!(
            text.contains("resyncing"),
            "a change_id gap badges the ladder resyncing: {text:?}",
        );
    }

    // --- `↑↓ / kj` scrolls, clamped to the viewport (#48 P2-02) --------------

    #[test]
    fn test_scroll_moves_and_clamps_to_the_viewport() {
        // 5 bids + 5 asks = 10 levels; a 4-row viewport → max offset 10 − 4 = 6.
        let mut state = populated_with(5, 5);
        state.depth_visible_rows = 4;
        // Anchor the scroll to the top so the step assertions are absolute (the
        // inside-market seed is exercised by its own test).
        state.depth_scroll = Some(0);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.depth_scroll, Some(1), "down scrolls one row");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.depth_scroll, Some(0), "up scrolls back");
        // Up at the top clamps at 0.
        press(&mut state, KeyCode::Up);
        assert_eq!(state.depth_scroll, Some(0), "up at the top clamps");
        // Down past the end clamps at the last screenful (total − visible = 6), NOT
        // total − 1 (the old phantom-scroll dead-zone).
        for _ in 0..50 {
            press(&mut state, KeyCode::Down);
        }
        assert_eq!(
            state.depth_scroll,
            Some(6),
            "down clamps at total − visible (the window lands exactly at the bottom)",
        );
    }

    #[test]
    fn test_j_k_scroll_like_the_chain_idiom() {
        // `j` scrolls down, `k` up — the chain's `kj` idiom (#48 P3-01).
        let mut state = populated_with(5, 5);
        state.depth_visible_rows = 4;
        state.depth_scroll = Some(0);
        press(&mut state, KeyCode::Char('j'));
        assert_eq!(state.depth_scroll, Some(1), "j scrolls down");
        press(&mut state, KeyCode::Char('k'));
        assert_eq!(state.depth_scroll, Some(0), "k scrolls up");
    }

    #[test]
    fn test_at_limit_press_leaves_the_offset_unchanged() {
        // At the bottom limit, another `↓` must not bump the offset — otherwise the
        // ViewSig flips and forces a phantom redraw with no visual movement (#48 P2-02).
        let mut state = populated_with(5, 5);
        state.depth_visible_rows = 4; // max offset 6
        state.depth_scroll = Some(6); // already at the bottom
        press(&mut state, KeyCode::Down);
        assert_eq!(
            state.depth_scroll,
            Some(6),
            "an at-limit down press is a no-op (no ViewSig flip)",
        );
    }

    #[test]
    fn test_a_fitting_book_never_scrolls() {
        // A book that fits entirely (10 levels ≤ a 20-row viewport): no press moves the
        // offset, and an unscrolled ladder stays `None` — so the view signature never
        // flips and no redraw fires (#48 P2-02).
        let mut state = populated_with(5, 5);
        state.depth_visible_rows = 20; // the whole book fits → max offset 0
        assert_eq!(state.depth_scroll, None, "starts unscrolled");
        press(&mut state, KeyCode::Down);
        assert_eq!(state.depth_scroll, None, "a fitting book stays unscrolled");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.depth_scroll, None, "still unscrolled");
    }

    #[test]
    fn test_scroll_is_a_noop_without_a_selected_book() {
        // No contract focused: scrolling changes nothing (no book to scroll).
        let mut state = depth_state(ScreenLoad::Ready, true);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.depth_scroll, None, "no book → scroll is a no-op");
    }

    #[test]
    fn test_clamp_start_windows_without_over_scroll() {
        assert_eq!(clamp_start(0, 5, 3), 0, "everything fits → start 0");
        assert_eq!(clamp_start(9, 5, 3), 0, "over-scroll a short book → 0");
        assert_eq!(clamp_start(2, 5, 20), 2, "in-range scroll passes through");
        assert_eq!(clamp_start(999, 5, 20), 15, "clamps to total − visible");
        assert_eq!(clamp_start(3, 0, 20), 0, "a zero window never indexes");
    }

    // --- The inside-market anchor centers the tradeable levels (#48 P3-02) ----

    #[test]
    fn test_inside_market_anchor_centers_and_reconciles_with_the_clamp() {
        // 20 asks + 20 bids = 40 levels. The best ask sits at display index 19; a
        // 4-row viewport anchors half a viewport above it: 19 − 2 = 17.
        let state = populated_with(20, 20);
        let book = match state
            .selected_depth_key()
            .as_ref()
            .and_then(|key| state.depth_store.book(key))
        {
            Some(b) => b.clone(),
            None => panic!("the seeded book must be retrievable"),
        };
        assert_eq!(
            inside_market_anchor(&book, 4),
            17,
            "centers the inside market"
        );
        // A book that fits, or an unknown (0) viewport, anchors at the top.
        assert_eq!(
            inside_market_anchor(&book, 40),
            0,
            "a fitting book anchors at 0"
        );
        assert_eq!(
            inside_market_anchor(&book, 0),
            0,
            "an unknown viewport anchors at 0"
        );
        // The anchor never over-scrolls: it is clamped to total − visible.
        assert!(
            inside_market_anchor(&book, 4) <= super::floor_sub(40, 4),
            "the anchor reconciles with the scroll clamp",
        );
    }

    #[test]
    fn test_initial_view_centers_inside_market_on_a_deep_book() {
        // A deep book with no user scroll (`None`) centers the inside market so the
        // best bid/ask are on-screen without scrolling — the worst ask is NOT shown.
        let state = populated_with(20, 20);
        assert_eq!(state.depth_scroll, None, "unscrolled by default");
        // 40×8 → a 4-row body window; centered on the best-ask/best-bid boundary.
        let text = render_text(&state, 40, 8);
        assert!(
            text.contains("60010.00"),
            "the best ask is on-screen: {text:?}"
        );
        assert!(
            text.contains("60000.00"),
            "the best bid is on-screen: {text:?}"
        );
        assert!(
            !text.contains("60200.00"),
            "the worst ask is NOT shown (the view is centered, not top-anchored): {text:?}",
        );
    }

    #[test]
    fn test_body_visible_rows_matches_the_drawn_window() {
        // The off-draw geometry helper the render loop stashes agrees with the body a
        // draw affords: a bordered block minus the footer minus the header row.
        // 40×8 → inner 6 rows → body 5 → minus header = 4.
        assert_eq!(body_visible_rows(Rect::new(0, 0, 40, 8)), 4);
        // A degenerate (too-short) area never underflows.
        assert_eq!(body_visible_rows(Rect::new(0, 0, 40, 1)), 0);
        assert_eq!(body_visible_rows(Rect::new(0, 0, 40, 0)), 0);
    }

    // --- Non-live health dims the ladder body (#48 P2-01) --------------------

    #[test]
    fn test_stale_health_dims_the_ladder_and_shows_the_badge() {
        // A non-Live source health dims the body (mirroring the chain) AND badges the
        // title — never a bright, trusted-looking body over a stale badge (#48 P2-01).
        let mut state = populated_with(3, 3);
        state.source.health = StreamHealth::Stale { since: utc(EXP) };
        assert!(
            body_is_dimmed(&state, 120, 40),
            "a stale feed dims the ladder body",
        );
        let text = render_text(&state, 120, 40);
        assert!(
            text.contains("stale"),
            "the stale badge renders in the title"
        );
    }

    #[test]
    fn test_live_health_leaves_the_ladder_bright() {
        // The control: a Live, continuous book is NOT dimmed — the dim is reserved for
        // the honest stale/resync state.
        let state = populated_with(3, 3);
        assert!(
            !body_is_dimmed(&state, 120, 40),
            "a live, continuous book renders bright",
        );
    }

    // --- Venue-controlled instrument name renders inert (escape hygiene) ------

    #[test]
    fn test_venue_instrument_name_renders_inert() {
        // A hostile instrument name (an OSC clipboard-write + a CSI clear) must render
        // as inert text at the depth title edge — no raw ESC survives.
        let mut state = depth_state(ScreenLoad::Ready, true);
        state.selection.focused_row = Some(0);
        if let Some(key) = state.selected_depth_key() {
            let hostile = "BTC\u{1b}]52;c;pwn\u{7}\u{1b}[2J-C";
            let _ = state
                .depth_store
                .apply(ladder_for(&key, hostile, 2, 2, Some(1)));
        }
        let text = render_text(&state, 120, 40);
        assert!(!text.contains('\u{1b}'), "no raw ESC survives to the title");
        assert!(
            text.contains("52;c;pwn"),
            "the OSC params render as inert text",
        );
    }

    // --- fmt_spread ----------------------------------------------------------

    #[test]
    fn test_fmt_spread_guards_crossed_and_infinite() {
        assert_eq!(fmt_spread(pos(60_010.0), pos(60_000.0)), "10.00");
        assert_eq!(fmt_spread(pos(60_000.0), pos(60_010.0)), "—", "crossed → —");
        assert_eq!(fmt_spread(Positive::INFINITY, pos(1.0)), "—", "inf → —");
        // #118: a sub-unit spread (0.06 − 0.05 = 0.01, scale 2) renders at its own
        // decimal scale, keeping its tradeable digits.
        assert_eq!(fmt_spread(posd(6, 2), posd(5, 2)), "0.01");
    }

    // --- fmt_num venue-scale-aware precision (#109, #118) --------------------

    #[test]
    fn test_fmt_num_scales_precision_to_magnitude() {
        // #109/#118: a sub-unit BTC option price renders at its OWN decimal scale,
        // keeping the tradeable digits instead of collapsing to two decimals (the old
        // `0.049 → 0.04` truncation); an index/underlying-scale price stays at the
        // familiar two-decimal cents.
        assert_eq!(fmt_num(posd(49, 3)), "0.049", "0.049 keeps its 3rd digit");
        assert_eq!(fmt_num(posd(48, 3)), "0.048");
        assert_eq!(fmt_num(posd(5, 2)), "0.05", "0.05 renders at scale 2");
        assert_eq!(fmt_num(posd(61, 3)), "0.061");
        assert_eq!(
            fmt_num(posd(5, 3)),
            "0.005",
            "0.005 keeps its scale-3 digit"
        );
        assert_eq!(fmt_num(pos(1.0)), "1.00", "at 1.0 → cents");
        assert_eq!(fmt_num(pos(60_000.0)), "60000.00", "index scale → cents");
        assert_eq!(fmt_num(Positive::INFINITY), "—", "non-finite → em dash");
    }

    #[test]
    fn test_fmt_num_distinguishes_near_one_sub_unit_prices() {
        // #118: two distinct executable prices just below one (both scale 4) must render
        // distinctly — the old 3-significant-figure scheme collapsed both to `0.999`.
        assert_eq!(fmt_num(posd(9994, 4)), "0.9994");
        assert_eq!(fmt_num(posd(9995, 4)), "0.9995");
        assert_ne!(
            fmt_num(posd(9994, 4)),
            fmt_num(posd(9995, 4)),
            "distinct near-one prices render distinctly, not a shared 0.999",
        );
        // The column budget still caps a pathologically deep sub-unit scale: a
        // scale-9 value renders at most MAX_DECIMALS (8) places, never widening the
        // price cell past its column.
        let capped = fmt_num(posd(123_456_789, 9));
        let decimals = capped
            .split_once('.')
            .map_or(0, |(_, frac)| frac.chars().count());
        assert!(
            decimals <= 8,
            "a deep sub-unit scale is capped at MAX_DECIMALS: {capped:?}",
        );
    }

    // --- Draw purity: draw does not mutate the scroll or store ---------------

    #[test]
    fn test_draw_does_not_mutate_scroll() {
        let mut state = populated_with(3, 3);
        state.depth_scroll = Some(2);
        let _ = render_text(&state, 120, 40);
        assert_eq!(
            state.depth_scroll,
            Some(2),
            "a draw must not move the scroll offset",
        );
    }
}
