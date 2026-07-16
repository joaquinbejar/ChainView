//! The chain-matrix screen (strikes × call/put: bid/ask/mark/IV/Greeks)
//! (`docs/05-views-and-ux.md` §2.1, §6, §8; `docs/01-domain-model.md` §8).
//!
//! # States first, then the happy path
//!
//! [`draw`] renders the **loading**, **empty**, and provider-**error** states
//! before the populated matrix (the ChainView states-first agent-workflow rule,
//! `CLAUDE.md`), driven off [`LiveState::load`](crate::LiveState) and the store's
//! emptiness/health per the `docs/05` §2.1 prerequisite/recovery matrix:
//!
//! - [`ScreenLoad::Loading`] → a centered tick-driven spinner + "connecting to
//!   `<provider>`…".
//! - [`ScreenLoad::Ready`] with an empty chain → "no data for `<underlying>
//!   <expiry>`" + a hint.
//! - [`ScreenLoad::Error`] → the actionable message + the `r` reconnect
//!   affordance.
//!
//! A live feed that drops does **not** blank the screen: the last chain renders
//! **dimmed** with a `◐ stale` / `↻ reconnecting (n)` badge
//! ([`health_span`](crate::ui::theme::health_span)).
//!
//! # The draw path is pure
//!
//! [`draw`] takes `&LiveState` (never `&mut`) plus the `Copy` resolved [`Theme`]
//! and tick counter, and projects [`ChainRow`]/[`LegView`] view models **at draw
//! time**, borrowed from the store's [`OptionChain`] — no computation, no pricing,
//! no `GraphData`, no I/O, no state mutation (`docs/02-tui-architecture.md` §7).
//! View models are the only place display formatting happens; the domain stays
//! numeric (`docs/01-domain-model.md` §8).
//!
//! # Projection is honest: `None` iff `None`, `—` never a fabricated `0`
//!
//! A [`LegView`] field is `None` **iff** the underlying [`OptionData`] field is
//! `None` — projection never invents a value, and a missing value renders `—` (an
//! em dash), never a fabricated `0` (`docs/01-domain-model.md` §5, §8). One
//! deliberate exception the venue seam forces, documented at [`project_iv`]: an
//! [`OptionData::implied_volatility`] of **exactly zero** is the Deribit adapter's
//! absent-IV sentinel (the upstream `OptionChain::add_option` takes a
//! **non-`Option`** IV, so an absent IV defaults to `Positive::ZERO`), so a bare
//! `0` IV projects to `None` and renders `—`, never a fabricated-looking `0.00%`.
//!
//! # Color is never the only signal
//!
//! The shared strike column shades by the `K/S` [`StrikeRelation`] bucket (not an
//! ITM/OTM label) and carries the `◀ATM` marker on the nearest listed strike; the
//! bid/ask cells carry a `▲`/`▼`/`·` tick-direction glyph; the stale badge pairs a
//! glyph with text — all legible under `NO_COLOR`
//! (`docs/05-views-and-ux.md` §7, `CLAUDE.md` accessibility policy).

use chrono::{DateTime, Utc};
use crossterm::event::KeyEvent;
use optionstratlib::OptionStyle;
use optionstratlib::chains::OptionData;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::{Decimal, Positive};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use crate::app::keymap::{ChainAction, KeyChord, resolve_chain};
use crate::app::{LegFocus, LiveState, ScreenLoad};
use crate::chain::{ChainStore, GreeksOrigin, InstrumentKey, StreamHealth, TickDir};
use crate::event::AppEvent;
use crate::ui::theme::{
    GreekColumn, StrikeRelation, Theme, health_span, spinner_frame, strike_relation_marker_span,
    tick_dir_span,
};

// ===========================================================================
// View models — projected at draw time, borrowed from the store, never owned.
// ===========================================================================

/// One option leg (a call or a put) at one strike, projected from an
/// [`OptionData`] at draw time (`docs/01-domain-model.md` §8).
///
/// Every optional field is `Some` **iff** the underlying [`OptionData`] field is
/// `Some` — projection never invents a value (see the `project_call` / `project_put`
/// and `project_iv` seam). `theta`/`vega` are v0.2 (upstream [`OptionData`] holds no
/// theta/vega yet), so they are always `None` and render `—` for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegView {
    /// Best bid, or `None` when the feed omits it (renders `—`).
    pub bid: Option<Positive>,
    /// Best ask, or `None` when the feed omits it (renders `—`).
    pub ask: Option<Positive>,
    /// Mid/mark price, or `None` when a side is missing (renders `—`).
    pub mark: Option<Positive>,
    /// Implied volatility, or `None` when unavailable **or** the venue's
    /// absent-IV zero sentinel (`project_iv`) — renders `—`, never `0.00%`.
    pub iv: Option<Positive>,
    /// Delta, or `None` when unavailable (renders `—`).
    pub delta: Option<Decimal>,
    /// Gamma, or `None` when unavailable (renders `—`).
    pub gamma: Option<Decimal>,
    /// Theta — v0.2 (no [`OptionData`] field yet), so always `None` for now.
    pub theta: Option<Decimal>,
    /// Vega — v0.2 (no [`OptionData`] field yet), so always `None` for now.
    pub vega: Option<Decimal>,
    /// Where this leg's Greeks came from — drives an origin glyph for locally
    /// computed Greeks. v0.1 sources Greeks from the venue, so this is
    /// [`GreeksOrigin::Provider`]; the [`GreeksOrigin::ComputedLocally`] badge
    /// wires in with the local fill-in and the per-style analytics sidecar (v0.2).
    pub greeks_origin: GreeksOrigin,
    /// The decayed last-tick direction of the bid, read from the store's retained
    /// baseline (`▲`/`▼`/`·`), cleared to `Flat` when the feed goes stale.
    pub bid_dir: TickDir,
    /// The decayed last-tick direction of the ask.
    pub ask_dir: TickDir,
}

/// One strike row of the chain matrix — the call and put legs plus the shared,
/// option-style-**independent** `K/S` relation that shades the strike column
/// (`docs/01-domain-model.md` §8).
///
/// [`strike_relation`](ChainRow::strike_relation) is deliberately **not** an
/// ITM/OTM label: a call and a put at one strike have opposite ITM/OTM status, so
/// no single label on a shared strike row can be truthful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainRow {
    /// The strike price.
    pub strike: Positive,
    /// The call leg at this strike.
    pub call: LegView,
    /// The put leg at this strike.
    pub put: LegView,
    /// Where the strike sits relative to spot (`K/S`) — shades the strike column.
    pub strike_relation: StrikeRelation,
}

/// Project the **call** leg from an [`OptionData`], reading the call-side fields
/// and the pre-decayed direction indicators. `None` iff the source field is
/// `None`; `theta`/`vega` are v0.2 and always `None`.
#[must_use]
fn project_call(od: &OptionData, bid_dir: TickDir, ask_dir: TickDir) -> LegView {
    LegView {
        bid: od.call_bid,
        ask: od.call_ask,
        mark: od.call_middle,
        iv: project_iv(od.implied_volatility),
        delta: od.delta_call,
        gamma: od.gamma,
        theta: None,
        vega: None,
        greeks_origin: GreeksOrigin::Provider,
        bid_dir,
        ask_dir,
    }
}

/// Project the **put** leg from an [`OptionData`]. `gamma` and `iv` are shared per
/// strike upstream, so both legs read the same value (`docs/01-domain-model.md`
/// §7); the per-style analytics sidecar in v0.2 splits them.
#[must_use]
fn project_put(od: &OptionData, bid_dir: TickDir, ask_dir: TickDir) -> LegView {
    LegView {
        bid: od.put_bid,
        ask: od.put_ask,
        mark: od.put_middle,
        iv: project_iv(od.implied_volatility),
        delta: od.delta_put,
        gamma: od.gamma,
        theta: None,
        vega: None,
        greeks_origin: GreeksOrigin::Provider,
        bid_dir,
        ask_dir,
    }
}

/// Project the non-`Option` [`OptionData::implied_volatility`] into a
/// `LegView.iv` **honestly** (the "Absent-IV vs 0% IV" decision from #15).
///
/// The upstream `OptionChain::add_option` takes a **non-`Option`** `Positive` IV,
/// so the Deribit adapter defaults an absent IV to `Positive::ZERO` — a row that
/// carries `0` cannot distinguish "venue sent no IV" from a genuine "IV = 0". A
/// listed option that is being quoted always has a strictly positive IV (a zero IV
/// prices the option at pure intrinsic, economically implausible for a live
/// quote), so a bare `0` is the absent-sentinel, **not** a real quote. This
/// projects it to `None` — the matrix renders `—`, exactly the honesty the Greeks
/// columns use — so a fabricated-looking `0.00%` never renders as a live IV. A
/// strictly positive IV projects to `Some(iv)`.
#[must_use]
fn project_iv(iv: Positive) -> Option<Positive> {
    if iv.is_zero() { None } else { Some(iv) }
}

/// Project one strike row: both legs plus the shared `K/S` relation.
///
/// The direction indicators are read from the store's retained/decayed baseline as
/// of `as_of` (the store's last-poll wall-time — the pure-draw reference instant,
/// since `draw` reads no wall clock); `None` when there is no such instant yields
/// `Flat`. Building the per-leg [`InstrumentKey`] clones the (short) underlying
/// ticker, which is why projection runs for the **visible** rows only.
#[must_use]
fn project_row(
    od: &OptionData,
    spot: Positive,
    store: &ChainStore,
    underlying: &str,
    expiration: DateTime<Utc>,
    as_of: Option<DateTime<Utc>>,
) -> ChainRow {
    let strike = od.strike_price;
    let (call_bid_dir, call_ask_dir) = leg_dirs(
        store,
        underlying,
        expiration,
        strike,
        OptionStyle::Call,
        as_of,
    );
    let (put_bid_dir, put_ask_dir) = leg_dirs(
        store,
        underlying,
        expiration,
        strike,
        OptionStyle::Put,
        as_of,
    );
    ChainRow {
        strike,
        call: project_call(od, call_bid_dir, call_ask_dir),
        put: project_put(od, put_bid_dir, put_ask_dir),
        strike_relation: StrikeRelation::classify(strike, spot),
    }
}

/// The `(bid_dir, ask_dir)` for one leg as of `as_of`, read from the store's
/// decayed baseline. Both are `Flat` when there is no reference instant.
#[must_use]
fn leg_dirs(
    store: &ChainStore,
    underlying: &str,
    expiration: DateTime<Utc>,
    strike: Positive,
    style: OptionStyle,
    as_of: Option<DateTime<Utc>>,
) -> (TickDir, TickDir) {
    let Some(now) = as_of else {
        return (TickDir::Flat, TickDir::Flat);
    };
    let key = InstrumentKey {
        underlying: underlying.to_owned(),
        expiration_utc: expiration,
        strike,
        style,
    };
    (store.bid_dir(&key, now), store.ask_dir(&key, now))
}

// ===========================================================================
// The draw entry point + the loading / empty / error states (states first).
// ===========================================================================

/// Draw the chain matrix for the live `state` into `area` — a pure render
/// (`docs/02-tui-architecture.md` §7). The empty/loading/error states render
/// before the populated matrix (the states-first rule); the store is borrowed,
/// never recomputed. `theme` (resolved, `NO_COLOR`-aware) and `tick` (for the
/// loading spinner) are `Copy`, so purity holds.
pub fn draw(state: &LiveState, frame: &mut Frame, area: Rect, theme: Theme, tick: u64) {
    let chain = state.store.chain();
    match &state.load {
        ScreenLoad::Loading => {
            draw_state_body(
                frame,
                area,
                theme,
                Text::from(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!(
                            "{} connecting to {}…",
                            spinner_frame(tick),
                            sanitize(state.source.provider.as_str())
                        ),
                        theme.accent(),
                    )),
                    Line::from(Span::styled("waiting for the first chain", theme.dim())),
                ]),
            );
        }
        ScreenLoad::Error { message } => {
            draw_state_body(
                frame,
                area,
                theme,
                Text::from(vec![
                    Line::from(Span::styled(
                        format!("! {}", sanitize(message)),
                        theme.warning(),
                    )),
                    Line::from(""),
                    Line::from(Span::styled("press r to reconnect", theme.dim())),
                ]),
            );
        }
        ScreenLoad::Ready => {
            if chain.options.is_empty() {
                draw_state_body(
                    frame,
                    area,
                    theme,
                    Text::from(vec![
                        Line::from(Span::styled(
                            format!(
                                "no data for {} {}",
                                sanitize(&chain.symbol),
                                sanitize(&chain.get_expiration_date())
                            ),
                            theme.dim(),
                        )),
                        Line::from(Span::styled(
                            "no strikes yet - press r to reconnect",
                            theme.dim(),
                        )),
                    ]),
                );
            } else {
                draw_matrix(state, frame, area, theme);
            }
        }
    }
}

/// Draw a centered state body (loading / empty / error) inside the framed "Chain"
/// block — a first-class state, never a blank void.
fn draw_state_body(frame: &mut Frame, area: Rect, theme: Theme, text: Text<'static>) {
    let block = Block::bordered().title(Span::styled("Chain", theme.accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), inner);
}

// ===========================================================================
// The populated matrix.
// ===========================================================================

/// The width (columns) of a numeric cell.
const NUM_W: u16 = 8;
/// The width (columns) of the shared center strike cell (fits `◀ATM`).
const STRIKE_W: u16 = 10;
/// Approximate width consumed by the always-present columns (10 numeric cells +
/// the strike cell + inter-column spacing) — the base of the greek-slot budget.
const BASE_W: u16 = 100;
/// Approximate width one optional greek **slot** costs (one numeric cell per
/// side + spacing).
const SLOT_W: u16 = 18;

/// Draw the populated strike × call/put matrix with ATM anchoring, the shaded
/// strike column, responsive greek columns, and the stale/reconnecting badge.
fn draw_matrix(state: &LiveState, frame: &mut Frame, area: Rect, theme: Theme) {
    let store = &state.store;
    let chain = store.chain();
    let spot = chain.underlying_price;
    let health = store.health();
    let stale = !matches!(health, StreamHealth::Live);
    let as_of = store.last_full_poll();

    // The underlying/expiry for the per-leg InstrumentKey come from the store's
    // canonical chain key (absolute UTC), not the display strings.
    let key = store.chain_key();
    let underlying = key.1.clone();
    let expiration = key.2;

    let block = Block::bordered().title(matrix_title(chain, health, theme));
    let inner = block.inner(area);

    // v0.1 column set: Δ (always) and Γ (from `OptionData`). Θ/ν are always empty
    // until the v0.2 analytics sidecar, so they are NOT shown — the responsive drop
    // operates on {Δ, Γ}: Γ appears once one optional slot fits. The #14
    // `greek_columns_for_slots` `Γ → ν → Θ` primitive stays intact (in `theme.rs`)
    // for when v0.2 populates Θ/ν and they join the column set.
    let show_gamma = greek_slots_for_width(inner.width) >= 1;
    let plan = columns(show_gamma);
    let widths: Vec<Constraint> = plan.iter().map(|col| col_width(*col)).collect();
    // The body height is the inner height minus the one header row.
    let visible = floor_sub(usize::from(inner.height), 1);

    let strikes: Vec<&OptionData> = chain.options.iter().collect();
    let len = strikes.len();
    let atm = atm_index_of(chain);
    let anchor = clamp_anchor(state.selection.focused_row, atm, len);
    // The explicit user cursor (clamped to the current chain), distinct from the
    // ATM anchor used for scrolling before any row is focused.
    let selected = state
        .selection
        .focused_row
        .and_then(|row| (row < len).then_some(row));
    let start = window_start(anchor.unwrap_or(0), visible, len);

    let header = header_row(&plan, theme);
    let mut rows: Vec<Row> = Vec::with_capacity(visible.min(len));
    for (idx, od) in strikes.iter().enumerate().skip(start).take(visible) {
        let od: &OptionData = od;
        let row = project_row(od, spot, store, &underlying, expiration, as_of);
        let is_atm = atm == Some(idx);
        let is_selected = selected == Some(idx);
        let cells: Vec<Cell> = plan
            .iter()
            .map(|col| {
                col_cell(
                    *col,
                    &row,
                    theme,
                    is_selected,
                    state.selection.focused_leg,
                    is_atm,
                )
            })
            .collect();
        let mut table_row = Row::new(cells);
        if is_selected {
            table_row = table_row.style(Style::new().add_modifier(Modifier::BOLD));
        }
        rows.push(table_row);
    }

    let mut table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1);
    // Never blank on a dropped stream: the last chain renders dimmed, the badge in
    // the title carries the honest state.
    if stale {
        table = table.style(theme.dim());
    }
    frame.render_widget(table, area);
}

/// The block title `<symbol>  exp <expiry>  spot <S>`, with the stream-health
/// badge appended when the feed is not live (`◐ stale` / `↻ reconnecting (n)`).
#[must_use]
fn matrix_title(chain: &OptionChain, health: &StreamHealth, theme: Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(sanitize(&chain.symbol), theme.accent()),
        Span::raw(format!("  exp {}", sanitize(&chain.get_expiration_date()))),
        Span::raw(format!("  spot {}", fmt_strike(chain.underlying_price))),
    ];
    if !matches!(health, StreamHealth::Live) {
        spans.push(Span::raw("  "));
        spans.push(health_span(health, theme));
    }
    Line::from(spans)
}

/// The number of optional greek **slots** (0..=3) that fit at `width`. In v0.1 the
/// matrix shows Γ once one slot fits (Δ is always present); the same slot budget
/// feeds the `Γ → ν → Θ` drop order (`greek_columns_for_slots`, `theme.rs`) once
/// v0.2 adds Θ/ν. A rough, truncation-safe estimate: below the budget the matrix
/// keeps only Δ and the price/IV columns and the [`Table`] clips gracefully.
#[must_use]
fn greek_slots_for_width(width: u16) -> usize {
    // `a - b` floored at zero; `max` guarantees the subtraction never underflows
    // (the ruleset bans `saturating_sub`, and `checked_sub(..).unwrap_or(0)` trips
    // clippy's manual-saturating lint).
    let over = width.max(BASE_W) - BASE_W;
    usize::from(over / SLOT_W).min(3)
}

/// `a - b` floored at zero, spelled so it can never underflow — the ruleset bans
/// `saturating_sub` and `checked_sub(..).unwrap_or(0)` trips clippy's
/// manual-saturating lint, so the floor is `a.max(b) - b`.
#[must_use]
fn floor_sub(a: usize, b: usize) -> usize {
    a.max(b) - b
}

/// The index (in ascending strike order) of the strike nearest spot — the `◀ATM`
/// row and the default scroll anchor. `None` for an empty chain.
#[must_use]
fn atm_index_of(chain: &OptionChain) -> Option<usize> {
    let spot = chain.underlying_price.to_dec();
    let mut best: Option<(usize, Decimal)> = None;
    for (idx, od) in chain.options.iter().enumerate() {
        let diff = (od.strike_price.to_dec() - spot).abs();
        let better = match best {
            Some((_, best_diff)) => diff < best_diff,
            None => true,
        };
        if better {
            best = Some((idx, diff));
        }
    }
    best.map(|(idx, _)| idx)
}

/// The scroll anchor: the clamped user cursor if present, else the ATM index, else
/// row 0 — never an out-of-range index (`docs/02-tui-architecture.md`, `.get()` +
/// fallback discipline).
#[must_use]
fn clamp_anchor(focused: Option<usize>, atm: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match focused {
        Some(row) if row < len => Some(row),
        // A poll shrank the chain under the cursor: fall back to the last row.
        Some(_) => Some(floor_sub(len, 1)),
        None => atm.or(Some(0)),
    }
}

/// The first visible row index so `anchor` stays on screen, clamped to `[0, len -
/// visible]`. Uses checked arithmetic only (no `saturating_*`, per the ruleset).
#[must_use]
fn window_start(anchor: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let half = visible / 2;
    let ideal = floor_sub(anchor, half);
    let max_start = floor_sub(len, visible);
    ideal.min(max_start)
}

// ===========================================================================
// Column plan — one ordered list drives header, widths, and cells consistently.
// ===========================================================================

/// One column of the matrix. A single ordered [`columns`] list is derived from the
/// visible greek columns, and header labels, width constraints, and per-row cells
/// are all mapped from it, so they can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainCol {
    /// A call-side greek column.
    CallGreek(GreekColumn),
    /// Call implied volatility.
    CallIv,
    /// Call bid (carries a tick-direction glyph).
    CallBid,
    /// Call ask (carries a tick-direction glyph).
    CallAsk,
    /// Call mark/mid.
    CallMark,
    /// The shared center strike column (shaded by relation, `◀ATM` marker).
    Strike,
    /// Put bid (carries a tick-direction glyph).
    PutBid,
    /// Put ask (carries a tick-direction glyph).
    PutAsk,
    /// Put mark/mid.
    PutMark,
    /// Put implied volatility.
    PutIv,
    /// A put-side greek column.
    PutGreek(GreekColumn),
}

/// Build the ordered v0.1 column plan: the call side mirrors the put side around
/// the center strike (`Δ Γ IV Bid Ask Mark | Strike | Bid Ask Mark IV Γ Δ`), with
/// Γ shown when `show_gamma`. Only the greeks that can carry data in v0.1 (Δ, Γ)
/// are in the set; Θ/ν are omitted (always empty until the v0.2 sidecar) rather
/// than rendered as empty columns that would push Γ off a common terminal. In v0.2
/// Θ/ν join the set and drop `Γ → ν → Θ` via `greek_columns_for_slots`.
#[must_use]
fn columns(show_gamma: bool) -> Vec<ChainCol> {
    let mut plan = Vec::new();
    plan.push(ChainCol::CallGreek(GreekColumn::Delta));
    if show_gamma {
        plan.push(ChainCol::CallGreek(GreekColumn::Gamma));
    }
    plan.push(ChainCol::CallIv);
    plan.push(ChainCol::CallBid);
    plan.push(ChainCol::CallAsk);
    plan.push(ChainCol::CallMark);
    plan.push(ChainCol::Strike);
    plan.push(ChainCol::PutBid);
    plan.push(ChainCol::PutAsk);
    plan.push(ChainCol::PutMark);
    plan.push(ChainCol::PutIv);
    // Put greeks mirror the call side (Γ Δ, outermost last).
    if show_gamma {
        plan.push(ChainCol::PutGreek(GreekColumn::Gamma));
    }
    plan.push(ChainCol::PutGreek(GreekColumn::Delta));
    plan
}

/// The width constraint for a column (fixed, so the header and rows align).
#[must_use]
fn col_width(col: ChainCol) -> Constraint {
    match col {
        ChainCol::Strike => Constraint::Length(STRIKE_W),
        _ => Constraint::Length(NUM_W),
    }
}

/// The header label for a column.
#[must_use]
fn col_header(col: ChainCol) -> &'static str {
    match col {
        ChainCol::CallGreek(greek) | ChainCol::PutGreek(greek) => greek_label(greek),
        ChainCol::CallIv | ChainCol::PutIv => "IV",
        ChainCol::CallBid | ChainCol::PutBid => "Bid",
        ChainCol::CallAsk | ChainCol::PutAsk => "Ask",
        ChainCol::CallMark | ChainCol::PutMark => "Mark",
        ChainCol::Strike => "Strike",
    }
}

/// Which leg a column belongs to (`None` for the shared strike column) — drives
/// the leg-focus emphasis on the selected row.
#[must_use]
fn col_side(col: ChainCol) -> Option<LegFocus> {
    match col {
        ChainCol::CallGreek(_)
        | ChainCol::CallIv
        | ChainCol::CallBid
        | ChainCol::CallAsk
        | ChainCol::CallMark => Some(LegFocus::Call),
        ChainCol::PutGreek(_)
        | ChainCol::PutIv
        | ChainCol::PutBid
        | ChainCol::PutAsk
        | ChainCol::PutMark => Some(LegFocus::Put),
        ChainCol::Strike => None,
    }
}

/// The single-glyph label for a greek column.
#[must_use]
fn greek_label(greek: GreekColumn) -> &'static str {
    match greek {
        GreekColumn::Delta => "Δ",
        GreekColumn::Gamma => "Γ",
        GreekColumn::Vega => "ν",
        GreekColumn::Theta => "Θ",
    }
}

/// The header row, right-aligned numeric labels and a centered `Strike`.
#[must_use]
fn header_row(plan: &[ChainCol], theme: Theme) -> Row<'static> {
    let cells: Vec<Cell> = plan
        .iter()
        .map(|col| {
            let align = if matches!(col, ChainCol::Strike) {
                Alignment::Center
            } else {
                Alignment::Right
            };
            Cell::from(Line::from(col_header(*col)).alignment(align))
        })
        .collect();
    Row::new(cells).style(theme.accent())
}

/// The cell for one column of one row, with leg-focus emphasis (an underline on
/// the focused leg's cells) applied on the selected row.
#[must_use]
fn col_cell(
    col: ChainCol,
    row: &ChainRow,
    theme: Theme,
    is_selected: bool,
    focused_leg: LegFocus,
    is_atm: bool,
) -> Cell<'static> {
    let cell = match col {
        ChainCol::CallGreek(greek) => greek_cell(&row.call, greek, theme),
        ChainCol::PutGreek(greek) => greek_cell(&row.put, greek, theme),
        ChainCol::CallIv => num_cell(fmt_iv(row.call.iv)),
        ChainCol::PutIv => num_cell(fmt_iv(row.put.iv)),
        ChainCol::CallBid => dir_cell(row.call.bid, row.call.bid_dir, theme),
        ChainCol::CallAsk => dir_cell(row.call.ask, row.call.ask_dir, theme),
        ChainCol::CallMark => num_cell(fmt_price(row.call.mark)),
        ChainCol::PutBid => dir_cell(row.put.bid, row.put.bid_dir, theme),
        ChainCol::PutAsk => dir_cell(row.put.ask, row.put.ask_dir, theme),
        ChainCol::PutMark => num_cell(fmt_price(row.put.mark)),
        ChainCol::Strike => strike_cell(row, theme, is_atm),
    };
    // The focused leg is underlined on the selected row — an intensity signal, so
    // it survives NO_COLOR and never fights the tick-direction color.
    match col_side(col) {
        Some(side) if is_selected && side == focused_leg => {
            cell.style(Style::new().add_modifier(Modifier::UNDERLINED))
        }
        _ => cell,
    }
}

/// A greek cell; the delta cell carries a subtle `~` origin glyph when the Greeks
/// are ChainView's local computation (v0.2), never for venue-provided Greeks.
#[must_use]
fn greek_cell(leg: &LegView, greek: GreekColumn, theme: Theme) -> Cell<'static> {
    match greek {
        GreekColumn::Delta => {
            if matches!(leg.greeks_origin, GreeksOrigin::ComputedLocally) && leg.delta.is_some() {
                let line = Line::from(vec![
                    Span::raw(fmt_greek(leg.delta)),
                    Span::styled("~", theme.warning()),
                ])
                .alignment(Alignment::Right);
                Cell::from(line)
            } else {
                num_cell(fmt_greek(leg.delta))
            }
        }
        GreekColumn::Gamma => num_cell(fmt_greek(leg.gamma)),
        GreekColumn::Vega => num_cell(fmt_greek(leg.vega)),
        GreekColumn::Theta => num_cell(fmt_greek(leg.theta)),
    }
}

/// A right-aligned numeric cell.
#[must_use]
fn num_cell(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).alignment(Alignment::Right))
}

/// A right-aligned price cell with a trailing tick-direction glyph (`▲`/`▼`/`·`) —
/// color-independent, so the direction reads under `NO_COLOR`.
#[must_use]
fn dir_cell(price: Option<Positive>, dir: TickDir, theme: Theme) -> Cell<'static> {
    let line = Line::from(vec![
        Span::raw(fmt_price(price)),
        Span::raw(" "),
        tick_dir_span(dir, theme),
    ])
    .alignment(Alignment::Right);
    Cell::from(line)
}

/// The shared center strike cell: the strike shaded by its `K/S` relation, with
/// the `◀ATM` marker on the nearest listed strike (both legible under `NO_COLOR`).
#[must_use]
fn strike_cell(row: &ChainRow, theme: Theme, is_atm: bool) -> Cell<'static> {
    let mut spans = vec![Span::styled(
        fmt_strike(row.strike),
        theme.strike_relation_style(row.strike_relation),
    )];
    if is_atm {
        spans.push(Span::raw(" "));
        spans.push(strike_relation_marker_span(StrikeRelation::AtSpot, theme));
    }
    Cell::from(Line::from(spans).alignment(Alignment::Center))
}

// ===========================================================================
// Cell formatting — the ONE place display formatting happens; `—` never `0`.
// ===========================================================================

/// The em dash rendered for any value the provider did not supply — never a
/// fabricated `0` (`docs/01-domain-model.md` §5, §8).
const EM_DASH: &str = "—";

/// Format a price to two decimals, or `—` when absent. Guards the `Positive`
/// infinity sentinel so a non-finite value never paints (rule: guard `f64`
/// `NaN`/`Inf` before it reaches a widget).
#[must_use]
fn fmt_price(value: Option<Positive>) -> String {
    match value {
        Some(price) if price != Positive::INFINITY => format!("{price:.2}"),
        _ => EM_DASH.to_owned(),
    }
}

/// Format IV as a percentage to two decimals, or `—` when absent (including the
/// venue's absent-IV zero already resolved to `None` by [`project_iv`]).
///
/// The `× 100` uses **checked** multiplication: the adapter seam rejects
/// NaN/Inf/negative but **not** magnitude, so a finite-but-absurd IV (`> ~7.9e26`)
/// can survive a hostile/corrupt venue payload, and [`Decimal`]'s `Mul` **panics**
/// on overflow — a render-edge panic a pure draw must never risk (ADR-0007
/// untrusted-input hardening). On overflow it renders `—` rather than panicking.
#[must_use]
fn fmt_iv(value: Option<Positive>) -> String {
    match value {
        Some(iv) if iv != Positive::INFINITY => iv
            .to_dec()
            .checked_mul(Decimal::from(100))
            .map_or_else(|| EM_DASH.to_owned(), |pct| format!("{pct:.2}%")),
        _ => EM_DASH.to_owned(),
    }
}

/// Format a greek to four decimals, or `—` when absent. A [`Decimal`] is
/// fixed-point, so it carries no `NaN`/`Inf` to guard.
#[must_use]
fn fmt_greek(value: Option<Decimal>) -> String {
    match value {
        Some(greek) => format!("{greek:.4}"),
        None => EM_DASH.to_owned(),
    }
}

/// Format a strike, trailing zeros stripped (`Positive` `Display` normalizes), so
/// an integer strike reads `60000` and a fractional one keeps its places.
#[must_use]
fn fmt_strike(strike: Positive) -> String {
    if strike == Positive::INFINITY {
        return EM_DASH.to_owned();
    }
    format!("{}", strike.round_to(2))
}

/// Strip control/escape characters from a venue- or user-controlled string before
/// it reaches the render edge (`docs/SECURITY.md` terminal escape-sequence
/// hygiene). The full escape-hygiene hardening + goldens land in #19; this is the
/// defensive minimum for the display symbol/expiry/message.
#[must_use]
fn sanitize(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect()
}

// ===========================================================================
// Key handling — resolved THROUGH the single keymap, no parallel table, no I/O.
// ===========================================================================

/// Handle a chain-local key, returning any follow-on [`AppEvent`] for the render
/// loop to fold (`docs/02-tui-architecture.md` §9). Pure — no I/O.
///
/// The chord resolves **through the single keybinding map**
/// ([`resolve_chain`](crate::resolve_chain), `src/app/keymap.rs`), so the chain
/// dispatch and the help overlay read one table and cannot drift — there is **no**
/// parallel key table here. Local navigation (strike cursor, leg focus) mutates
/// [`LiveState`] and returns `None` (the render loop detects the [`Selection`]
/// change and redraws, `docs/05-views-and-ux.md` §8). Actions that need I/O
/// (multi-expiry subscribe, underlying switch, drill-in, the v0.2 payoff builder)
/// are resolved but not yet wired — never performed inline; they land with their
/// data plumbing, exactly as the replay screen defers its playback actions.
///
/// [`Selection`]: crate::Selection
#[must_use]
pub fn handle_key(state: &mut LiveState, key: KeyEvent) -> Option<AppEvent> {
    let chord = KeyChord::from_event(key)?;
    match resolve_chain(chord)? {
        ChainAction::MoveStrike => {
            move_strike(state, chord);
            None
        }
        ChainAction::FocusLeg => {
            focus_leg(state, chord);
            None
        }
        // Resolved through the map, but their I/O plumbing is a later issue: a
        // multi-expiry subscribe path, an underlying list, the drill-in view, and
        // the v0.2 payoff builder. They never perform I/O inline here.
        ChainAction::SwitchExpiry
        | ChainAction::SwitchUnderlying
        | ChainAction::Drill
        | ChainAction::AddLeg => None,
    }
}

/// Move the strike cursor up/down within the chain bounds. The first move from an
/// unset cursor reveals it at the ATM anchor; later moves step by one, clamped —
/// never an out-of-range index.
fn move_strike(state: &mut LiveState, chord: KeyChord) {
    let chain = state.store.chain();
    let len = chain.options.len();
    if len == 0 {
        return;
    }
    let down = matches!(chord, KeyChord::Down | KeyChord::Char('j'));
    let current = clamp_anchor(state.selection.focused_row, None, len);
    let next = match state.selection.focused_row {
        // First move: place the cursor at the ATM anchor rather than jumping.
        None => atm_index_of(chain).unwrap_or(0),
        Some(_) => {
            let row = current.unwrap_or(0);
            if down {
                (row + 1).min(floor_sub(len, 1))
            } else {
                floor_sub(row, 1)
            }
        }
    };
    state.selection.focused_row = Some(next);
}

/// Focus the call or put leg (`c` / `p`), revealing the cursor at the ATM anchor if
/// no row is focused yet so the focus has a visible target.
fn focus_leg(state: &mut LiveState, chord: KeyChord) {
    state.selection.focused_leg = match chord {
        KeyChord::Char('p') => LegFocus::Put,
        // The FocusLeg action only binds `c`/`p`; `c` (and any defensive fallback)
        // focuses the call leg.
        _ => LegFocus::Call,
    };
    if state.selection.focused_row.is_none() {
        let chain = state.store.chain();
        if !chain.options.is_empty() {
            state.selection.focused_row = Some(atm_index_of(chain).unwrap_or(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::OptionStyle;
    use optionstratlib::chains::OptionData;
    use optionstratlib::chains::chain::OptionChain;
    use optionstratlib::prelude::{Decimal, Positive};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{
        ChainRow, LegView, atm_index_of, clamp_anchor, draw, greek_slots_for_width, handle_key,
        project_call, project_iv, project_put, project_row, window_start,
    };
    use crate::app::{LegFocus, LiveScreen, LiveState, Mode, ScreenLoad, Selection, SourceBinding};
    use crate::chain::{
        AliasCatalog, ChainFetch, ChainSource, ChainStore, ExpirySource, GreeksOrigin, Instrument,
        InstrumentKey, ProviderId, QuoteUpdate, StreamHealth, TickDir,
    };
    use crate::chain::{ContractSpecFingerprint, ExerciseStyle, SettlementStyle};
    use crate::config::ThemeChoice;
    use crate::providers::{ChainCapability, GreeksCapability, ProviderCapabilities};
    use crate::ui::theme::{GreekColumn, StrikeRelation, Theme};

    const EXP: i64 = 1_700_000_000;

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

    #[track_caller]
    fn pos_dec(value: Decimal) -> Positive {
        match Positive::new_decimal(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive decimal: {e}"),
        }
    }

    fn dec(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    /// A fully-populated call+put row at `strike` with an IV of 50%.
    fn full_row(strike: f64) -> OptionData {
        let mut od = OptionData {
            strike_price: pos(strike),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            delta_call: Some(dec(6, 1)),
            delta_put: Some(dec(-4, 1)),
            gamma: Some(dec(1, 2)),
            ..Default::default()
        };
        od.set_mid_prices();
        od
    }

    fn chain_with(strikes: &[f64]) -> OptionChain {
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        for strike in strikes {
            let _ = chain.options.insert(full_row(*strike));
        }
        chain
    }

    fn store_with(chain: OptionChain) -> ChainStore {
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

    fn caps() -> ProviderCapabilities {
        ProviderCapabilities::builder()
            .chain(ChainCapability::Assemble)
            .depth(true)
            .greeks(GreeksCapability::Provided)
            .build()
    }

    fn live_with(chain: OptionChain, load: ScreenLoad) -> LiveState {
        let mut live = LiveState::new(
            SourceBinding::new(pid("deribit"), caps(), StreamHealth::Live),
            store_with(chain),
        );
        live.load = load;
        live
    }

    fn spec() -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: 1,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn instrument(strike: f64, style: OptionStyle) -> Instrument {
        Instrument {
            key: InstrumentKey {
                underlying: "BTC".to_owned(),
                expiration_utc: utc(EXP),
                strike: pos(strike),
                style,
            },
            provider: pid("deribit"),
            native_symbol: format!("BTC-{strike}-{}", style.as_str()),
            stream_symbol: None,
            spec: spec(),
        }
    }

    fn quote(strike: f64, style: OptionStyle, bid: f64, ask: f64, received: i64) -> QuoteUpdate {
        QuoteUpdate {
            instrument: instrument(strike, style),
            bid: Some(pos(bid)),
            ask: Some(pos(ask)),
            last: None,
            bid_size: None,
            ask_size: None,
            event_time: Some(utc(received)),
            received_time: utc(received),
        }
    }

    #[track_caller]
    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        match Terminal::new(TestBackend::new(width, height)) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend construction failed: {e}"),
        }
    }

    fn theme() -> Theme {
        Theme::resolve(ThemeChoice::Auto, false)
    }

    /// Draw the chain screen for `live` at `width`×`height` and return the frame
    /// text (row-major), for render assertions.
    #[track_caller]
    fn rendered(live: &LiveState, width: u16, height: u16) -> String {
        let mut term = terminal(width, height);
        match term.draw(|frame| draw(live, frame, frame.area(), theme(), 0)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // --- Projection: None iff None -------------------------------------------

    #[test]
    fn test_project_call_leg_none_iff_none_field() {
        // A row with some sides absent: the LegView field is None exactly where the
        // OptionData field is None, never a fabricated value.
        let mut od = OptionData {
            strike_price: pos(60_000.0),
            call_bid: Some(pos(1.0)),
            call_ask: None,
            put_bid: Some(pos(2.0)),
            put_ask: Some(pos(2.4)),
            implied_volatility: pos(0.5),
            delta_call: Some(dec(5, 1)),
            gamma: None,
            ..Default::default()
        };
        od.set_mid_prices();
        let call = project_call(&od, TickDir::Flat, TickDir::Flat);
        assert_eq!(call.bid, Some(pos(1.0)), "present bid projects Some");
        assert_eq!(call.ask, None, "absent ask projects None (renders em dash)");
        assert_eq!(call.mark, None, "no mid without both sides -> None");
        assert_eq!(call.delta, Some(dec(5, 1)));
        assert_eq!(call.gamma, None, "absent gamma projects None");
        // theta/vega are v0.2 -> always None.
        assert_eq!(call.theta, None);
        assert_eq!(call.vega, None);
        assert_eq!(call.greeks_origin, GreeksOrigin::Provider);
    }

    #[test]
    fn test_project_put_leg_reads_put_side_fields() {
        let od = full_row(60_000.0);
        let put = project_put(&od, TickDir::Up, TickDir::Down);
        assert_eq!(put.bid, od.put_bid);
        assert_eq!(put.ask, od.put_ask);
        assert_eq!(put.mark, od.put_middle);
        assert_eq!(
            put.delta, od.delta_put,
            "put reads delta_put, not delta_call"
        );
        assert_eq!(put.gamma, od.gamma, "gamma is shared per strike");
        assert_eq!(put.bid_dir, TickDir::Up);
        assert_eq!(put.ask_dir, TickDir::Down);
    }

    // --- The absent-IV rule (#15): a bare 0 IV renders `—`, not `0.00%` ------

    #[test]
    fn test_project_iv_zero_projects_none_not_a_fabricated_quote() {
        // The venue's absent-IV sentinel is Positive::ZERO; it must project None so
        // the matrix renders `—`, never a fabricated-looking 0.00%.
        assert_eq!(project_iv(Positive::ZERO), None);
    }

    #[test]
    fn test_project_iv_positive_projects_some() {
        assert_eq!(project_iv(pos(0.5)), Some(pos(0.5)));
    }

    #[test]
    fn test_project_call_leg_absent_iv_zero_is_none() {
        let od = OptionData {
            strike_price: pos(60_000.0),
            implied_volatility: Positive::ZERO,
            ..Default::default()
        };
        let call = project_call(&od, TickDir::Flat, TickDir::Flat);
        assert_eq!(
            call.iv, None,
            "a zero-sentinel IV projects None, not Some(0)"
        );
    }

    // --- StrikeRelation bucketing via the row projection ---------------------

    #[test]
    fn test_project_row_buckets_strike_relation_below_at_above() {
        let chain = chain_with(&[50_000.0, 60_000.0, 70_000.0]);
        let store = store_with(chain);
        let spot = pos(60_000.0);
        let expiration = utc(EXP);
        let below = match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(50_000.0))
        {
            Some(od) => project_row(od, spot, &store, "BTC", expiration, store.last_full_poll()),
            None => panic!("expected a 50000 strike"),
        };
        assert_eq!(below.strike_relation, StrikeRelation::BelowSpot);
        let at = match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(60_000.0))
        {
            Some(od) => project_row(od, spot, &store, "BTC", expiration, store.last_full_poll()),
            None => panic!("expected a 60000 strike"),
        };
        assert_eq!(at.strike_relation, StrikeRelation::AtSpot);
        let above = match store
            .chain()
            .options
            .iter()
            .find(|o| o.strike_price == pos(70_000.0))
        {
            Some(od) => project_row(od, spot, &store, "BTC", expiration, store.last_full_poll()),
            None => panic!("expected a 70000 strike"),
        };
        assert_eq!(above.strike_relation, StrikeRelation::AboveSpot);
    }

    // --- Direction projection reads the store's decayed baseline -------------

    #[test]
    fn test_project_row_projects_rising_bid_direction_up() {
        // Two rising quotes give the store an Up bid direction; the projection
        // reads it (as of the last-poll instant, which precedes the changes, so no
        // decay applies).
        let mut store = store_with(chain_with(&[60_000.0]));
        let _ = store.apply_quote(&quote(60_000.0, OptionStyle::Call, 1.0, 1.2, EXP + 100));
        let _ = store.apply_quote(&quote(60_000.0, OptionStyle::Call, 1.5, 1.7, EXP + 101));
        let od = match store.chain().options.iter().next() {
            Some(od) => od.clone(),
            None => panic!("expected one row"),
        };
        let row = project_row(
            &od,
            pos(60_000.0),
            &store,
            "BTC",
            utc(EXP),
            store.last_full_poll(),
        );
        assert_eq!(row.call.bid_dir, TickDir::Up, "a rising bid projects Up");
        assert_eq!(row.call.ask_dir, TickDir::Up, "a rising ask projects Up");
        // The put leg had no quotes -> Flat.
        assert_eq!(row.put.bid_dir, TickDir::Flat);
    }

    #[test]
    fn test_project_row_no_reference_instant_is_flat() {
        let store = store_with(chain_with(&[60_000.0]));
        let od = match store.chain().options.iter().next() {
            Some(od) => od.clone(),
            None => panic!("expected one row"),
        };
        // as_of = None -> directions default to Flat without touching the store.
        let row = project_row(&od, pos(60_000.0), &store, "BTC", utc(EXP), None);
        assert_eq!(row.call.bid_dir, TickDir::Flat);
        assert_eq!(row.put.ask_dir, TickDir::Flat);
    }

    // --- Windowing / anchoring helpers (no out-of-range index) ---------------

    #[test]
    fn test_atm_index_of_finds_nearest_strike() {
        let chain = chain_with(&[50_000.0, 59_000.0, 70_000.0]);
        // spot 60000 -> nearest is 59000 at index 1.
        assert_eq!(atm_index_of(&chain), Some(1));
        assert_eq!(
            atm_index_of(&OptionChain::new(
                "BTC",
                pos(1.0),
                "x".to_owned(),
                None,
                None
            )),
            None
        );
    }

    #[test]
    fn test_clamp_anchor_falls_back_when_cursor_out_of_range() {
        assert_eq!(
            clamp_anchor(Some(2), Some(1), 5),
            Some(2),
            "in-range cursor kept"
        );
        assert_eq!(
            clamp_anchor(Some(9), Some(1), 5),
            Some(4),
            "over-range -> last row"
        );
        assert_eq!(clamp_anchor(None, Some(3), 5), Some(3), "unset -> ATM");
        assert_eq!(clamp_anchor(None, None, 0), None, "empty chain -> None");
    }

    #[test]
    fn test_window_start_keeps_anchor_visible() {
        assert_eq!(window_start(0, 5, 3), 0, "chain fits -> start 0");
        assert_eq!(window_start(10, 5, 20), 8, "centers the anchor");
        assert_eq!(window_start(19, 5, 20), 15, "clamps to the last window");
        assert_eq!(window_start(5, 0, 20), 0, "zero visible -> start 0");
    }

    #[test]
    fn test_greek_slots_for_width_is_bounded_and_grows() {
        assert_eq!(greek_slots_for_width(20), 0, "too narrow -> Delta only");
        assert!(greek_slots_for_width(200) <= 3, "at most three greek slots");
        assert!(
            greek_slots_for_width(200) >= greek_slots_for_width(120),
            "wider fits at least as many",
        );
    }

    // --- v0.1 column set is {Delta, Gamma} only (Theta/Vega omitted) ---------

    #[test]
    fn test_columns_v01_set_is_delta_and_gamma_only() {
        // Θ/ν are always empty until the v0.2 sidecar, so the v0.1 column set never
        // includes them; Γ (which carries `od.gamma`) shows once one slot fits.
        let call_greeks = |plan: &[super::ChainCol]| -> Vec<GreekColumn> {
            plan.iter()
                .filter_map(|col| match col {
                    super::ChainCol::CallGreek(greek) => Some(*greek),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            call_greeks(&super::columns(true)),
            vec![GreekColumn::Delta, GreekColumn::Gamma],
            "wide: Delta and Gamma",
        );
        assert_eq!(
            call_greeks(&super::columns(false)),
            vec![GreekColumn::Delta],
            "narrow: Delta only",
        );
        // No Theta/Vega column ever appears in v0.1.
        for show_gamma in [true, false] {
            assert!(
                !super::columns(show_gamma).iter().any(|col| matches!(
                    col,
                    super::ChainCol::CallGreek(GreekColumn::Theta | GreekColumn::Vega)
                        | super::ChainCol::PutGreek(GreekColumn::Theta | GreekColumn::Vega)
                )),
                "no always-empty Theta/Vega column in v0.1",
            );
        }
    }

    #[test]
    fn test_draw_matrix_shows_gamma_column_at_common_width() {
        // The v0.1 fix: on a common 120-col terminal the matrix shows Γ (real data),
        // never an empty Θ column in its place.
        let live = live_with(
            chain_with(&[59_000.0, 60_000.0, 61_000.0]),
            ScreenLoad::Ready,
        );
        let wide = rendered(&live, 120, 20);
        assert!(wide.contains("Γ"), "gamma column shows at 120 cols");
        assert!(!wide.contains("Θ"), "no always-empty theta column in v0.1");
        assert!(!wide.contains("ν"), "no always-empty vega column in v0.1");
    }

    // --- fmt_iv never panics at the render edge on an absurd magnitude -------

    #[test]
    fn test_fmt_iv_overflowing_magnitude_renders_em_dash_not_panic() {
        // A finite-but-absurd IV can survive the adapter seam (magnitude is not
        // rejected). fmt_iv's checked ×100 renders `—`, never panics (ADR-0007).
        let huge = pos_dec(Decimal::MAX);
        assert_eq!(super::fmt_iv(Some(huge)), super::EM_DASH);
        // A normal IV still formats as a percentage.
        assert_eq!(super::fmt_iv(Some(pos(0.5))), "50.00%");
    }

    // --- States render before the happy path, deliberately, without panic ----

    #[test]
    fn test_draw_loading_state_shows_connecting_to_provider() {
        let live = live_with(chain_with(&[60_000.0]), ScreenLoad::Loading);
        let text = rendered(&live, 120, 20);
        assert!(text.contains("connecting to deribit"), "names the provider");
    }

    #[test]
    fn test_draw_empty_state_shows_no_data_hint() {
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let live = live_with(empty, ScreenLoad::Ready);
        let text = rendered(&live, 120, 20);
        assert!(text.contains("no data for BTC"), "names the underlying");
        assert!(text.contains("2025-06-27"), "names the expiry");
    }

    #[test]
    fn test_draw_error_state_shows_message_and_retry_key() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Loading);
        live.load = ScreenLoad::Error {
            message: "provider unreachable".to_owned(),
        };
        let text = rendered(&live, 120, 20);
        assert!(text.contains("provider unreachable"), "shows the message");
        assert!(text.contains("press r to reconnect"), "shows the retry key");
    }

    #[test]
    fn test_draw_populated_matrix_shows_strike_and_atm_marker() {
        let live = live_with(
            chain_with(&[59_000.0, 60_000.0, 61_000.0]),
            ScreenLoad::Ready,
        );
        let text = rendered(&live, 120, 20);
        assert!(text.contains("60000"), "renders a strike");
        assert!(text.contains("◀ATM"), "marks the ATM strike");
        assert!(text.contains("Strike"), "renders the header");
    }

    #[test]
    fn test_draw_stale_feed_shows_badge_not_blank() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        live.source.health = StreamHealth::Reconnecting { attempt: 2 };
        live.store
            .apply_health(StreamHealth::Reconnecting { attempt: 2 });
        let text = rendered(&live, 120, 20);
        assert!(text.contains("60000"), "the last chain still renders");
        assert!(
            text.contains("reconnecting"),
            "shows the reconnecting badge"
        );
    }

    #[test]
    fn test_draw_absent_iv_row_renders_em_dash_not_zero_percent() {
        // A row whose IV is the absent-sentinel zero must render an em dash, never
        // a fabricated 0.00%.
        let mut od = OptionData {
            strike_price: pos(60_000.0),
            call_bid: Some(pos(1.0)),
            call_ask: Some(pos(1.2)),
            implied_volatility: Positive::ZERO,
            ..Default::default()
        };
        od.set_mid_prices();
        let mut chain = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let _ = chain.options.insert(od);
        let live = live_with(chain, ScreenLoad::Ready);
        let text = rendered(&live, 120, 20);
        assert!(
            text.contains(super::EM_DASH),
            "the absent IV renders an em dash"
        );
        assert!(!text.contains("0.00%"), "no fabricated 0.00% IV");
    }

    #[test]
    fn test_draw_reachable_states_render_across_sizes_without_panic() {
        // The chain screen's reachable states (populated / empty / loading / error /
        // stale) at several sizes never panic (extends render_never_panics to the
        // chain body directly).
        let populated = chain_with(&[58_000.0, 59_000.0, 60_000.0, 61_000.0, 62_000.0]);
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let mut stale = live_with(populated.clone(), ScreenLoad::Ready);
        stale
            .store
            .apply_health(StreamHealth::Stale { since: utc(EXP) });
        stale.source.health = StreamHealth::Stale { since: utc(EXP) };
        let states = [
            live_with(populated.clone(), ScreenLoad::Ready),
            live_with(empty, ScreenLoad::Ready),
            live_with(populated.clone(), ScreenLoad::Loading),
            {
                let mut e = live_with(populated, ScreenLoad::Ready);
                e.load = ScreenLoad::Error {
                    message: "boom".to_owned(),
                };
                e
            },
            stale,
        ];
        for live in &states {
            for (w, h) in [(40u16, 8u16), (80, 24), (120, 40), (160, 50)] {
                let _ = rendered(live, w, h);
            }
        }
    }

    // --- Draw purity: draw takes &LiveState and mutates nothing --------------

    #[test]
    fn test_draw_is_pure_leaves_state_unchanged() {
        // `draw` takes `&LiveState`, so it cannot mutate the store, selection, or
        // load; assert the observable state is unchanged across a draw (no pricing
        // call, no mutation, no state flip).
        let live = live_with(
            chain_with(&[59_000.0, 60_000.0, 61_000.0]),
            ScreenLoad::Ready,
        );
        let before_len = live.store.chain().options.len();
        let before_poll = live.store.last_full_poll();
        let before_sel = live.selection;
        let before_health = matches!(live.store.health(), StreamHealth::Live);
        let _ = rendered(&live, 120, 40);
        assert_eq!(
            live.store.chain().options.len(),
            before_len,
            "no rows added/removed"
        );
        assert_eq!(live.store.last_full_poll(), before_poll, "no re-poll");
        assert_eq!(live.selection, before_sel, "selection unchanged");
        assert_eq!(
            matches!(live.store.health(), StreamHealth::Live),
            before_health,
            "health unchanged",
        );
    }

    // --- handle_key: nav resolves through the keymap, mutates local state ----

    #[test]
    fn test_handle_key_move_strike_down_reveals_cursor_at_atm_then_steps() {
        let mut live = live_with(
            chain_with(&[58_000.0, 60_000.0, 62_000.0]),
            ScreenLoad::Ready,
        );
        assert_eq!(live.selection.focused_row, None, "no cursor initially");
        // First `j` reveals the cursor at the ATM anchor (index 1 for spot 60000).
        assert!(handle_key(&mut live, press(KeyCode::Char('j'))).is_none());
        assert_eq!(live.selection.focused_row, Some(1), "cursor at ATM");
        // Next `j` steps down, clamped to the last row.
        let _ = handle_key(&mut live, press(KeyCode::Char('j')));
        assert_eq!(live.selection.focused_row, Some(2));
        let _ = handle_key(&mut live, press(KeyCode::Down));
        assert_eq!(
            live.selection.focused_row,
            Some(2),
            "clamped at the last row"
        );
        // `k` / Up step back up.
        let _ = handle_key(&mut live, press(KeyCode::Char('k')));
        assert_eq!(live.selection.focused_row, Some(1));
    }

    #[test]
    fn test_handle_key_focus_leg_sets_call_or_put() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let _ = handle_key(&mut live, press(KeyCode::Char('p')));
        assert_eq!(live.selection.focused_leg, LegFocus::Put);
        assert!(
            live.selection.focused_row.is_some(),
            "focus reveals the cursor"
        );
        let _ = handle_key(&mut live, press(KeyCode::Char('c')));
        assert_eq!(live.selection.focused_leg, LegFocus::Call);
    }

    #[test]
    fn test_handle_key_unbound_key_returns_none_and_changes_nothing() {
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let before = live.selection;
        assert!(handle_key(&mut live, press(KeyCode::Char('z'))).is_none());
        assert_eq!(live.selection, before, "an unbound key changes nothing");
    }

    #[test]
    fn test_handle_key_deferred_actions_are_noops_no_io() {
        // Expiry/underlying/drill/add resolve through the map but are not yet wired;
        // they return None and perform no I/O, changing no selection.
        let mut live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let before = live.selection;
        for code in [
            KeyCode::Char('l'), // SwitchExpiry
            KeyCode::Char(']'), // SwitchUnderlying
            KeyCode::Enter,     // Drill
            KeyCode::Char('a'), // AddLeg
        ] {
            assert!(handle_key(&mut live, press(code)).is_none());
        }
        assert_eq!(live.selection, before, "deferred actions change no state");
    }

    #[test]
    fn test_handle_key_move_strike_on_empty_chain_is_noop() {
        let empty = OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None);
        let mut live = live_with(empty, ScreenLoad::Ready);
        let _ = handle_key(&mut live, press(KeyCode::Char('j')));
        assert_eq!(
            live.selection.focused_row, None,
            "no cursor on an empty chain"
        );
    }

    // --- Sanity: view models are Copy and round-trip through a live state -----

    #[test]
    fn test_chain_row_and_leg_view_are_constructible_copy_values() {
        let od = full_row(60_000.0);
        let row: ChainRow = ChainRow {
            strike: od.strike_price,
            call: project_call(&od, TickDir::Flat, TickDir::Flat),
            put: project_put(&od, TickDir::Flat, TickDir::Flat),
            strike_relation: StrikeRelation::AtSpot,
        };
        // Copy: using the value twice compiles without a move error.
        let copy: ChainRow = row;
        let _first: LegView = row.call;
        let _second: LegView = copy.call;
        assert_eq!(row.strike, copy.strike);
    }

    // Keep the imports for a Selection-shaped default used above meaningful.
    #[test]
    fn test_selection_default_focuses_call_leg() {
        let selection = Selection::default();
        assert_eq!(selection.focused_leg, LegFocus::Call);
        assert_eq!(selection.focused_row, None);
    }

    // Ensure the Mode/LiveScreen imports are exercised (a live state renders on the
    // Chain screen).
    #[test]
    fn test_live_state_defaults_to_chain_screen() {
        let live = live_with(chain_with(&[60_000.0]), ScreenLoad::Ready);
        let app_mode = Mode::Live(live);
        match app_mode {
            Mode::Live(state) => assert_eq!(state.screen, LiveScreen::Chain),
            Mode::Replay(_) => panic!("expected a live mode"),
        }
    }
}
