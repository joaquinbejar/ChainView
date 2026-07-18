//! The `GraphData` → ratatui dataset adapter (`docs/05-views-and-ux.md` §4,
//! `docs/02-tui-architecture.md` §7, [ADR-0001]).
//!
//! # Why an adapter exists
//!
//! `optionstratlib` describes payoff, smile, and Greek-curve geometry as its own
//! [`GraphData`] type; ratatui's [`Chart`](ratatui::widgets::Chart) does **not**
//! consume `GraphData` — the Plotly backend is deliberately unused
//! ([ADR-0001], `docs/05-views-and-ux.md` §4). This module is the single seam that
//! projects a `GraphData` into the shape a ratatui chart consumes: a borrowed
//! `&[(f64, f64)]` point series (what [`Dataset::data`](ratatui::widgets::Dataset::data)
//! takes) plus `[f64; 2]` x/y axis bounds (what [`Axis::bounds`](ratatui::widgets::Axis::bounds)
//! takes) and precomputed numeric endpoint labels.
//!
//! ChainView never computes payoff/curve/surface geometry itself — it consumes a
//! `GraphData` built in the domain layer and lays it out
//! (`docs/05-views-and-ux.md` §4). This adapter performs **no** domain computation.
//!
//! # The projection is fallible and first-class-empty
//!
//! [`project`] returns a [`GraphProjection`]: [`Ready`](GraphProjection::Ready) with
//! a [`ProjectedSeries`], or [`Empty`](GraphProjection::Empty) with a machine
//! [`EmptyReason`]. An empty, malformed, or not-yet-supported `GraphData` becomes an
//! **explicit** empty projection — never a panic and never a fabricated series. This
//! is what lets the payoff screen (#27) render its "add a leg" state and the vol
//! surface (#47) render its "insufficient IV" state deliberately, rather than
//! blanking the body.
//!
//! # The projection is cached on state, never built in `draw`
//!
//! The render loop is synchronous and pure over `&App`; building a `GraphData`
//! inside `draw` is a 🔴 (`docs/02-tui-architecture.md` §7). The [`GraphCache`]
//! handle encodes that discipline structurally: it owns the domain-built
//! `GraphData` and its cached `GraphProjection`, and it computes the projection at
//! construction / [`update`](GraphCache::update) time — **off** the draw path. A
//! screen holds a `GraphCache` on its state and, in `draw`, reads only
//! [`projection`](GraphCache::projection) (a borrow). Because `draw` receives `&State`
//! (never `&mut`) and `update` needs `&mut self`, a draw cannot **build, mutate, or
//! replace** the cached `GraphData` (the retained input is unchanged across a draw —
//! asserted by a purity test), and reads only the cached `projection()`. Re-projecting
//! the retained input in `draw` (`project(cache.input())`) is possible but is the exact
//! per-frame allocation the cache exists to avoid — a review-caught anti-pattern, not a
//! shape-enforced impossibility.
//!
//! # Display floats are guarded before they reach a dataset
//!
//! `Series2D` carries its coordinates as `Decimal` (a fixed-point type that cannot
//! itself be `NaN`/`Inf`), which the projection converts to plot `f64` **here at the
//! UI edge only** — the domain stays numeric (`CLAUDE.md` numeric policy). Every
//! converted point passes through the single `finite_xy` gate: a non-finite or
//! non-representable `x`/`y` is **dropped**, never rendered as `NaN`; if no point
//! survives (or the series was empty) the projection is [`Empty`](GraphProjection::Empty).
//! This gate is the enforced invariant for the ratatui dataset and for the future
//! `f64`-sourced curves/surfaces that route through it.
//!
//! # The three projected shapes (#27 payoff, #47 surface)
//!
//! [`project`] matches **every** `GraphData` variant with no wildcard, so adding an
//! upstream variant forces this module to be revisited by the compiler. A
//! `GraphData::Series` projects to a [`ProjectedSeries`] (payoff, the vol smile, one
//! Greek curve). A `GraphData::GraphSurface` projects to a [`ProjectedSurface`] — the
//! #47 single-expiry Greek/Price-over-(strike, volatility) heat map (the `Series`
//! projection path is untouched by that arm). `GraphData::MultiSeries` (overlaid
//! Greek curves) stays a deliberate [`Empty(Unsupported)`](EmptyReason::Unsupported):
//! #47 cycles **one** Greek curve at a time (`g`/`G`), so the overlay shape is not
//! needed and is never fabricated.
//!
//! [ADR-0001]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0001-ratatui-crossterm.md
//! [`Chart`]: ratatui::widgets::Chart

use optionstratlib::prelude::{Decimal, ToPrimitive};
use optionstratlib::visualization::{GraphData, Series2D, Surface3D};

use crate::ui::theme::sanitize;

/// The maximum number of volatility rows / strike columns a surface grid retains
/// off the draw path. A single-expiry surface is `strikes × vol-samples`, both
/// small (the vol vector is deliberately tiny), so this cap is a defensive ceiling
/// on a pathological chain, keeping the grid bounded regardless of strike count.
const MAX_SURFACE_CELLS: usize = 4096;

// ===========================================================================
// The projected dataset shape a ratatui chart consumes.
// ===========================================================================

/// The minimum half-width added to a degenerate (`min == max`) axis interval: at
/// least this many units of the axis' own coordinate space, so a flat line near
/// zero still spans a visible range. See [`AxisBounds::padded`].
const DEGENERATE_PAD_MIN: f64 = 1.0;

/// The relative half-width added to a degenerate axis interval: 0.5% of the
/// (absolute) endpoint value, so a flat line at a large coordinate (e.g. a 60000
/// strike) spans a proportionate range rather than a sliver. See
/// [`AxisBounds::padded`].
const DEGENERATE_PAD_FRACTION: f64 = 0.005;

/// The `[min, max]` range of one axis, in the plot's `f64` coordinate space.
///
/// The **units are those of the source `GraphData`**, which the adapter is generic
/// over: for a payoff (#27) the x range is the underlying price and the y range is
/// P&L; for a vol smile (#47) x is the strike and y is implied volatility; for a
/// Greek curve x is the strike and y is the selected Greek. The adapter carries the
/// numeric range only — the semantic axis title is supplied by the screen.
///
/// # Invariant
///
/// Both endpoints are **finite and ordered** (`min <= max`) by construction: the
/// fields are private and [`new`](Self::new) is the only constructor, rejecting any
/// `NaN`/`Inf` or inverted range before it can reach a ratatui axis. Read them
/// through [`min`](Self::min) / [`max`](Self::max) / [`to_array`](Self::to_array).
///
/// # Degenerate intervals are padded at the seam
///
/// A single-point or flat series would otherwise yield `min == max` — a zero-width
/// range ratatui's chart rejects entirely, painting a BLANK chart. The projection
/// pads such an interval (the internal `padded` step), so the axis always has real
/// width and a flat line stays visible; the pad size is documented on that step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisBounds {
    /// The axis minimum (finite, `<= max`).
    min: f64,
    /// The axis maximum (finite, `>= min`).
    max: f64,
}

impl AxisBounds {
    /// Construct validated bounds, or `None` when the "finite, ordered" invariant
    /// fails: both `min` and `max` must be finite (no `NaN`/`Inf`) and ordered
    /// (`min <= max`). This is the ONLY way to build an `AxisBounds`, so a
    /// downstream caller can never hand a non-finite or inverted range to a ratatui
    /// axis. A degenerate `min == max` is accepted here (it is finite and ordered);
    /// the projection expands it via the internal `padded` step.
    #[must_use]
    pub fn new(min: f64, max: f64) -> Option<Self> {
        (min.is_finite() && max.is_finite() && min <= max).then_some(Self { min, max })
    }

    /// The axis minimum — finite and `<= max` by construction.
    #[must_use]
    pub fn min(self) -> f64 {
        self.min
    }

    /// The axis maximum — finite and `>= min` by construction.
    #[must_use]
    pub fn max(self) -> f64 {
        self.max
    }

    /// The bounds as the `[f64; 2]` array [`Axis::bounds`](ratatui::widgets::Axis::bounds)
    /// consumes.
    #[must_use]
    pub fn to_array(self) -> [f64; 2] {
        [self.min, self.max]
    }

    /// The bounds of a non-empty iterator of finite values, or `None` when the
    /// iterator is empty. Callers pass only finite values (post-`finite_xy`), so
    /// the `<`/`>` comparisons need no `NaN` handling. A degenerate result
    /// (`min == max`) is expanded via [`padded`](Self::padded) so the axis has real
    /// width.
    #[must_use]
    fn from_values(values: impl Iterator<Item = f64>) -> Option<Self> {
        let mut values = values;
        let first = values.next()?;
        let mut min = first;
        let mut max = first;
        for value in values {
            if value < min {
                min = value;
            }
            if value > max {
                max = value;
            }
        }
        Self::new(min, max).map(Self::padded)
    }

    /// Expand a degenerate (`min == max`) interval symmetrically so the axis has
    /// real width; a non-degenerate interval is returned unchanged.
    ///
    /// A flat or single-point series collapses one (or both) axes to `min == max`.
    /// ratatui's [`Chart`](ratatui::widgets::Chart) rejects every point when an axis
    /// range has zero width, so the projection would claim `Ready` yet paint a BLANK
    /// chart. Padding gives the axis a visible span, so a flat line renders as a
    /// flat line. The half-width is `max(|v| * 0.005, 1.0)` — the larger of 0.5% of
    /// the endpoint value ([`DEGENERATE_PAD_FRACTION`]) or one axis unit
    /// ([`DEGENERATE_PAD_MIN`]). If padding would overflow to a non-finite endpoint,
    /// the original (degenerate) interval is kept rather than panicking.
    #[must_use]
    fn padded(self) -> Self {
        // The invariant guarantees `min <= max`, so `!(min < max)` means `min == max`
        // (a degenerate interval) without an `==` float comparison.
        if self.min < self.max {
            return self;
        }
        let value = self.min;
        let pad = (value.abs() * DEGENERATE_PAD_FRACTION).max(DEGENERATE_PAD_MIN);
        Self::new(value - pad, value + pad).unwrap_or(self)
    }

    /// The `[min, mid, max]` numeric labels for the axis, formatted for
    /// [`Axis::labels`](ratatui::widgets::Axis::labels). Precomputed off the draw
    /// path (stored on [`ProjectedSeries`]), so `draw` allocates nothing.
    #[must_use]
    fn endpoint_labels(self) -> Vec<String> {
        let mid = self.min + (self.max - self.min) / 2.0;
        vec![fmt_coord(self.min), fmt_coord(mid), fmt_coord(self.max)]
    }
}

/// A `GraphData::Series` projected into the ratatui chart shape: the borrowed point
/// series, the x/y axis bounds, precomputed axis labels, and the series name.
///
/// A screen builds a [`Dataset`](ratatui::widgets::Dataset) from [`points`](Self::points)
/// and feeds [`x_bounds`](Self::x_bounds)/[`y_bounds`](Self::y_bounds) to its
/// [`Axis`](ratatui::widgets::Axis)es. Everything here is precomputed off the draw
/// path and only borrowed at draw time — no per-frame allocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedSeries {
    /// The `(x, y)` point series in input order — the exact shape
    /// [`Dataset::data`](ratatui::widgets::Dataset::data) borrows. Every coordinate
    /// is finite (non-finite points were dropped by `finite_xy`).
    points: Vec<(f64, f64)>,
    /// The x-axis range (see [`AxisBounds`] for units).
    x: AxisBounds,
    /// The y-axis range (see [`AxisBounds`] for units).
    y: AxisBounds,
    /// Precomputed x-axis endpoint labels.
    x_labels: Vec<String>,
    /// Precomputed y-axis endpoint labels.
    y_labels: Vec<String>,
    /// The series name (sanitized for the render edge), used for a chart legend.
    name: String,
}

impl ProjectedSeries {
    /// The `(x, y)` point series to hand
    /// [`Dataset::data`](ratatui::widgets::Dataset::data) — a borrow, so `draw`
    /// allocates nothing. Non-empty (an all-dropped series projects to
    /// [`Empty`](GraphProjection::Empty)).
    #[must_use]
    pub fn points(&self) -> &[(f64, f64)] {
        &self.points
    }

    /// The x-axis bounds `[min, max]` for
    /// [`Axis::bounds`](ratatui::widgets::Axis::bounds). **Units** are the source
    /// GraphData's x axis — e.g. underlying price (payoff) or strike (smile / Greek
    /// curve); see [`AxisBounds`].
    #[must_use]
    pub fn x_bounds(&self) -> [f64; 2] {
        self.x.to_array()
    }

    /// The y-axis bounds `[min, max]` for
    /// [`Axis::bounds`](ratatui::widgets::Axis::bounds). **Units** are the source
    /// GraphData's y axis — e.g. P&L (payoff) or implied volatility / Greek value
    /// (smile / Greek curve); see [`AxisBounds`].
    #[must_use]
    pub fn y_bounds(&self) -> [f64; 2] {
        self.y.to_array()
    }

    /// The precomputed `[min, mid, max]` x-axis labels for
    /// [`Axis::labels`](ratatui::widgets::Axis::labels) — a borrow.
    #[must_use]
    pub fn x_labels(&self) -> &[String] {
        &self.x_labels
    }

    /// The precomputed `[min, mid, max]` y-axis labels for
    /// [`Axis::labels`](ratatui::widgets::Axis::labels) — a borrow.
    #[must_use]
    pub fn y_labels(&self) -> &[String] {
        &self.y_labels
    }

    /// The series name (sanitized), for a chart legend.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A `GraphData::GraphSurface` projected into a character-grid heat map — the
/// v0.5 (#47) single-expiry Greek/Price-over-(strike, volatility) surface.
///
/// The upstream [`Surface3D`] is a scattered `(strike, vol, z)` point set on a
/// strike × vol grid, where **`z` is a Greek or a Price — never IV**
/// (`BasicSurfaces` rejects the volatility axis; the only true IV artifact is the
/// 2D smile). This projection bins those points into a dense row-major intensity
/// grid the surface screen paints as a glyph ramp: the top row is the **highest**
/// volatility and the left column the **lowest** strike, so the layout matches a
/// chart's `y`-up / `x`-right convention. Each cell is the `z` value normalized to
/// `[0, 1]` across the surface's `z` range, so the screen maps it to a
/// `NO_COLOR`-safe glyph (light → dense) whose intensity survives a monochrome
/// terminal. Every contributing point passed the same finite gate the `Series` path
/// uses, so no `NaN`/`Inf` coordinate or `z` reaches the grid.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedSurface {
    /// Row-major normalized-`z` grid: `rows[r][c]` is the `z` intensity in
    /// `[0, 1]` for vol-row `r` (row `0` = the **highest** vol) and strike-column
    /// `c` (ascending strike), or `None` when that `(strike, vol)` cell produced no
    /// finite `z` (a gap the screen paints blank, never a fabricated `0`).
    rows: Vec<Vec<Option<f64>>>,
    /// The strike (`x`) axis range.
    x: AxisBounds,
    /// The volatility (`y`) axis range (a fraction; the screen formats it as a
    /// percent at the edge).
    y: AxisBounds,
    /// The `z` (Greek / Price) value range — the legend scale for the glyph ramp.
    z: AxisBounds,
    /// Precomputed strike-axis endpoint labels.
    x_labels: Vec<String>,
    /// Precomputed vol-axis endpoint labels (raw fractions; the screen renders the
    /// percent form).
    y_labels: Vec<String>,
    /// Precomputed `z`-axis endpoint labels (the Greek/Price legend).
    z_labels: Vec<String>,
    /// The surface name (sanitized for the render edge).
    name: String,
}

impl ProjectedSurface {
    /// The row-major normalized-`z` grid (`[0, 1]` per cell, `None` for a gap) —
    /// row `0` is the highest vol, column `0` the lowest strike. A borrow, so the
    /// screen allocates nothing at draw time.
    #[must_use]
    pub fn rows(&self) -> &[Vec<Option<f64>>] {
        &self.rows
    }

    /// The strike-axis bounds `[min, max]`.
    #[must_use]
    pub fn x_bounds(&self) -> [f64; 2] {
        self.x.to_array()
    }

    /// The volatility-axis bounds `[min, max]` (a fraction).
    #[must_use]
    pub fn y_bounds(&self) -> [f64; 2] {
        self.y.to_array()
    }

    /// The `z` (Greek / Price) value bounds `[min, max]` — the glyph-ramp legend.
    #[must_use]
    pub fn z_bounds(&self) -> [f64; 2] {
        self.z.to_array()
    }

    /// The precomputed `[min, mid, max]` strike-axis labels.
    #[must_use]
    pub fn x_labels(&self) -> &[String] {
        &self.x_labels
    }

    /// The precomputed `[min, mid, max]` vol-axis labels (raw fractions).
    #[must_use]
    pub fn y_labels(&self) -> &[String] {
        &self.y_labels
    }

    /// The precomputed `[min, mid, max]` `z`-axis (Greek/Price) labels.
    #[must_use]
    pub fn z_labels(&self) -> &[String] {
        &self.z_labels
    }

    /// The surface name (sanitized).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Why a `GraphData` produced no renderable series — the reason attached to
/// [`GraphProjection::Empty`], so a screen can pick the right empty-state message
/// and a diagnostic can distinguish the causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EmptyReason {
    /// The series carried no points — an unbuilt payoff, or a chain with no IV
    /// samples yet. The screen renders its deliberate empty state ("add a leg" /
    /// "insufficient IV"), never a blank void.
    NoData,
    /// The `GraphData` was malformed (mismatched `x`/`y` lengths) or every point was
    /// non-finite after the `finite_xy` guard — nothing trustworthy to plot. The
    /// screen renders its error/empty state rather than an invented series.
    Degenerate,
    /// A `GraphData` variant this adapter does not project (`MultiSeries` — the
    /// overlaid-Greek-curve overlay). #47 renders one Greek curve at a time
    /// (`g`/`G` cycles the axis) so the overlaid `MultiSeries` shape is not needed;
    /// it stays a deliberate `Unsupported` rather than a fabricated overlay. The
    /// single-expiry `GraphSurface` **is** projected now (to [`ProjectedSurface`]).
    Unsupported,
}

/// The outcome of projecting a `GraphData`: a renderable series, or an explicit,
/// first-class empty state with its reason.
///
/// This is intentionally not `Result` with a stringly error: the "empty" outcome is
/// a normal, expected state a screen renders deliberately (`docs/05-views-and-ux.md`
/// §4, §6), not an exceptional failure.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphProjection {
    /// A renderable single series (payoff / smile / one Greek curve).
    Ready(ProjectedSeries),
    /// A renderable single-expiry surface heat map (the #47 Greek/Price surface).
    ReadySurface(ProjectedSurface),
    /// No renderable geometry — render the screen's empty/error state.
    Empty(EmptyReason),
}

impl GraphProjection {
    /// The [`ProjectedSeries`] when [`Ready`](Self::Ready), else `None` — a `.get()`
    /// style accessor so a widget never has to `match` at the call site.
    #[must_use]
    pub fn ready(&self) -> Option<&ProjectedSeries> {
        match self {
            Self::Ready(series) => Some(series),
            Self::ReadySurface(_) | Self::Empty(_) => None,
        }
    }

    /// The [`ProjectedSurface`] when [`ReadySurface`](Self::ReadySurface), else
    /// `None` — the surface-screen accessor (#47).
    #[must_use]
    pub fn ready_surface(&self) -> Option<&ProjectedSurface> {
        match self {
            Self::ReadySurface(surface) => Some(surface),
            Self::Ready(_) | Self::Empty(_) => None,
        }
    }

    /// The [`EmptyReason`] when [`Empty`](Self::Empty), else `None`.
    #[must_use]
    pub fn empty_reason(&self) -> Option<EmptyReason> {
        match self {
            Self::Empty(reason) => Some(*reason),
            Self::Ready(_) | Self::ReadySurface(_) => None,
        }
    }
}

// ===========================================================================
// The pure projection (off the draw path).
// ===========================================================================

/// Project a domain-built [`GraphData`] into a ratatui chart [`GraphProjection`].
///
/// **Pure and fallible**: it borrows an already-built `GraphData` (owned by screen
/// state, see [`GraphCache`]) and performs no domain computation and no I/O. An
/// empty, malformed, or not-yet-supported `GraphData` yields
/// [`Empty`](GraphProjection::Empty) — never a panic, never a fabricated series.
///
/// This must run **off** the draw path (at [`GraphCache`] construction/update time),
/// not inside `terminal.draw` (`docs/02-tui-architecture.md` §7).
///
/// The match is wildcard-free over all `GraphData` variants, so an upstream variant
/// addition forces this function to be revisited by the compiler. `Series` and
/// `GraphSurface` project to real geometry (#27/#47); `MultiSeries` stays the
/// deliberate `Empty(Unsupported)` (#47 cycles one curve at a time, no overlay).
#[must_use]
pub fn project(graph: &GraphData) -> GraphProjection {
    match graph {
        GraphData::Series(series) => project_series(series),
        // The #47 single-expiry Greek/Price surface — a real heat-map projection.
        // The `Series` path above is untouched by this arm.
        GraphData::GraphSurface(surface) => project_surface(surface),
        // Overlaid Greek curves: not built (#47 cycles one curve at a time via
        // `g`/`G`), so a deliberate `Empty(Unsupported)`, never a fabricated overlay.
        GraphData::MultiSeries(_) => GraphProjection::Empty(EmptyReason::Unsupported),
    }
}

/// Project a single [`Series2D`] (the payoff / smile / single Greek-curve variant).
#[must_use]
fn project_series(series: &Series2D) -> GraphProjection {
    // A mismatched x/y length is malformed: pairing `x[i]` with `y[i]` across
    // differing lengths would silently invent a truncated series, so refuse it.
    if series.x.len() != series.y.len() {
        return GraphProjection::Empty(EmptyReason::Degenerate);
    }
    if series.x.is_empty() {
        return GraphProjection::Empty(EmptyReason::NoData);
    }
    // Convert the Decimal domain coordinates to plot `f64` at the UI edge, then pass
    // every point through the SINGLE finite gate. A Decimal that cannot be
    // represented as `f64` becomes a non-finite sentinel the gate drops — so no
    // non-finite coordinate can ever reach a dataset.
    let mut points: Vec<(f64, f64)> = Vec::with_capacity(series.x.len());
    for (x, y) in series.x.iter().zip(series.y.iter()) {
        if let Some(point) = finite_xy(coord(x), coord(y)) {
            points.push(point);
        }
    }
    if points.is_empty() {
        // Every point was dropped by the finite gate — malformed geometry, not the
        // deliberate "no data yet" state.
        return GraphProjection::Empty(EmptyReason::Degenerate);
    }
    let (Some(x), Some(y)) = (
        AxisBounds::from_values(points.iter().map(|(x, _)| *x)),
        AxisBounds::from_values(points.iter().map(|(_, y)| *y)),
    ) else {
        // Unreachable given `points` is non-empty, but resolved without an
        // `.unwrap()`: a bounds failure degrades to the empty state, never a panic.
        return GraphProjection::Empty(EmptyReason::Degenerate);
    };
    let x_labels = x.endpoint_labels();
    let y_labels = y.endpoint_labels();
    GraphProjection::Ready(ProjectedSeries {
        points,
        x,
        y,
        x_labels,
        y_labels,
        name: sanitize(&series.name),
    })
}

/// Project a [`Surface3D`] (the #47 single-expiry Greek/Price-over-(strike, vol)
/// surface) into the character-grid [`ProjectedSurface`].
///
/// **Pure and fallible**, exactly like [`project_series`]: mismatched `x`/`y`/`z`
/// lengths are [`Degenerate`](EmptyReason::Degenerate), an empty surface is
/// [`NoData`](EmptyReason::NoData), and a surface whose every point is non-finite
/// (or that collapses to no axis) is [`Degenerate`](EmptyReason::Degenerate) — never
/// a panic and never a fabricated cell. The `z` values are normalized to `[0, 1]`
/// across the surface `z` range for the screen's glyph ramp.
#[must_use]
fn project_surface(surface: &Surface3D) -> GraphProjection {
    if surface.x.len() != surface.y.len() || surface.x.len() != surface.z.len() {
        return GraphProjection::Empty(EmptyReason::Degenerate);
    }
    if surface.x.is_empty() {
        return GraphProjection::Empty(EmptyReason::NoData);
    }
    // Keep only fully-finite triples (the belt-and-suspenders finite gate: every
    // coordinate is a `Decimal` and finite by construction, but a Decimal outside
    // the `f64` range maps to `NaN` and is dropped here rather than painted).
    let mut points: Vec<(f64, f64, f64)> = Vec::with_capacity(surface.x.len());
    for ((x, y), z) in surface.x.iter().zip(&surface.y).zip(&surface.z) {
        let (xf, yf, zf) = (coord(x), coord(y), coord(z));
        if xf.is_finite() && yf.is_finite() && zf.is_finite() {
            points.push((xf, yf, zf));
        }
    }
    if points.is_empty() {
        return GraphProjection::Empty(EmptyReason::Degenerate);
    }
    // The distinct strike (x) and volatility (y) coordinates — the grid axes.
    let strikes = distinct_sorted(points.iter().map(|p| p.0));
    let vols = distinct_sorted(points.iter().map(|p| p.1));
    // A checked cell-count guard (no banned `saturating_*`): an overflow — or a grid
    // above the ceiling — degrades to the empty state rather than allocating unbounded.
    let too_large = strikes
        .len()
        .checked_mul(vols.len())
        .is_none_or(|cells| cells > MAX_SURFACE_CELLS);
    if strikes.is_empty() || vols.is_empty() || too_large {
        return GraphProjection::Empty(EmptyReason::Degenerate);
    }
    let (Some(x), Some(y), Some(z)) = (
        AxisBounds::from_values(strikes.iter().copied()),
        AxisBounds::from_values(vols.iter().copied()),
        AxisBounds::from_values(points.iter().map(|p| p.2)),
    ) else {
        return GraphProjection::Empty(EmptyReason::Degenerate);
    };
    // Rows top-to-bottom = highest vol first (the chart `y`-up convention).
    let vols_desc: Vec<f64> = vols.iter().rev().copied().collect();
    let span = z.max - z.min;
    let mut rows: Vec<Vec<Option<f64>>> = vec![vec![None; strikes.len()]; vols_desc.len()];
    for (px, py, pz) in &points {
        let col = strikes.iter().position(|s| s == px);
        let row = vols_desc.iter().position(|v| v == py);
        if let (Some(col), Some(row)) = (col, row)
            && let Some(cell) = rows.get_mut(row).and_then(|r| r.get_mut(col))
        {
            // Normalize `z` into the glyph-ramp band; a flat surface reads mid.
            let norm = if span > 0.0 {
                ((pz - z.min) / span).clamp(0.0, 1.0)
            } else {
                0.5
            };
            *cell = Some(norm);
        }
    }
    let x_labels = x.endpoint_labels();
    let y_labels = y.endpoint_labels();
    let z_labels = z.endpoint_labels();
    GraphProjection::ReadySurface(ProjectedSurface {
        rows,
        x,
        y,
        z,
        x_labels,
        y_labels,
        z_labels,
        name: sanitize(&surface.name),
    })
}

/// The distinct, ascending finite values of an iterator — the grid axis coordinates.
/// Uses [`f64::total_cmp`] (a total order over the finite inputs, so no `NaN`
/// handling) and exact dedup: the surface's repeated strike / vol coordinates come
/// from the same `Decimal` set, so equal values collapse exactly.
#[must_use]
fn distinct_sorted(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut out: Vec<f64> = values.collect();
    out.sort_by(f64::total_cmp);
    out.dedup();
    out
}

/// Convert a `Decimal` coordinate to plot `f64`. A value that cannot be represented
/// as `f64` maps to `NaN`, so the single `finite_xy` gate drops it — the failure
/// never fabricates a `0` or a truncated coordinate.
#[must_use]
fn coord(value: &Decimal) -> f64 {
    value.to_f64().unwrap_or(f64::NAN)
}

/// The single finite gate every point crosses before it can enter a dataset: `Some`
/// only when **both** `x` and `y` are finite, else `None` (dropped). A `NaN`/`Inf`
/// coordinate can never reach a ratatui [`Dataset`](ratatui::widgets::Dataset), so a
/// chart axis or cell never renders `NaN` (`CLAUDE.md`: guard `f64` `NaN`/`Inf`
/// before it paints).
#[must_use]
fn finite_xy(x: f64, y: f64) -> Option<(f64, f64)> {
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// Format an (already-finite) coordinate for an axis label — two decimals, the
/// house numeric style. `draw`-free (labels are precomputed on [`ProjectedSeries`]).
#[must_use]
fn fmt_coord(value: f64) -> String {
    format!("{value:.2}")
}

// ===========================================================================
// The cache handle — GraphData + its projection live on state, projected off draw.
// ===========================================================================

/// The cache a screen holds on its state: the domain-built [`GraphData`] and its
/// cached [`GraphProjection`], projected **off** the draw path.
///
/// This handle is how ChainView keeps `GraphData` construction out of `draw`
/// (`docs/02-tui-architecture.md` §7). A screen builds it once from the domain
/// geometry ([`new`](Self::new)), rebuilds it when that geometry changes
/// ([`update`](Self::update), which requires `&mut self` and a fresh `GraphData`),
/// and in `draw` reads only [`projection`](Self::projection) — a borrow. Because
/// `draw` takes `&State` (never `&mut`) and `update` needs `&mut self`, a draw
/// cannot build, mutate, or replace the cached `GraphData`; it reads the cached
/// `projection()`. (Re-projecting the retained input in `draw`,
/// `project(cache.input())`, is possible but is the review-caught anti-pattern the
/// cache exists to avoid.) The payoff screen (#27) holds one of these on its
/// payoff state.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphCache {
    /// The domain-built input geometry, retained so the cache can be diffed/rebuilt.
    input: GraphData,
    /// The cached projection, recomputed only in [`new`](Self::new)/[`update`](Self::update).
    projection: GraphProjection,
}

impl GraphCache {
    /// Build the cache from a domain-built `GraphData`, projecting it **once** here
    /// (off the draw path).
    #[must_use]
    pub fn new(input: GraphData) -> Self {
        let projection = project(&input);
        Self { input, projection }
    }

    /// Replace the input geometry and re-project, off the draw path. Called by a
    /// screen when the domain rebuilds the `GraphData` (e.g. a leg is added to the
    /// payoff builder, or the smile expiry changes) — never from `draw`.
    pub fn update(&mut self, input: GraphData) {
        self.projection = project(&input);
        self.input = input;
    }

    /// The cached projection — the **only** thing `draw` reads. A borrow, so drawing
    /// builds no `GraphData` and does no geometry work.
    #[must_use]
    pub fn projection(&self) -> &GraphProjection {
        &self.projection
    }

    /// The retained input `GraphData` — for a screen that needs to diff before
    /// deciding whether to [`update`](Self::update). Not read by `draw`.
    #[must_use]
    pub fn input(&self) -> &GraphData {
        &self.input
    }
}

#[cfg(test)]
mod tests {
    use optionstratlib::prelude::Decimal;
    use optionstratlib::visualization::{GraphData, Series2D, Surface3D};
    use proptest::prelude::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::{Axis, Chart, Dataset, GraphType};

    use super::{
        AxisBounds, EmptyReason, GraphCache, GraphProjection, ProjectedSeries, coord, finite_xy,
        project,
    };

    // --- Constructors (no unwrap/expect/indexing per the ruleset) ------------

    fn dec(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    /// A `Series2D` from parallel `(x, y)` decimals — the real 0.18.0 field shape
    /// (parallel `Vec<Decimal>`, not a `Vec<(f64, f64)>`).
    fn series(name: &str, xs: &[Decimal], ys: &[Decimal]) -> Series2D {
        Series2D {
            x: xs.to_vec(),
            y: ys.to_vec(),
            name: name.to_owned(),
            ..Default::default()
        }
    }

    /// Assert two finite floats are equal within a tight tolerance (avoids exact
    /// float equality while proving the numeric result).
    #[track_caller]
    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}",
        );
    }

    #[track_caller]
    fn ready(projection: &GraphProjection) -> &ProjectedSeries {
        match projection.ready() {
            Some(series) => series,
            None => panic!("expected a Ready projection, got {projection:?}"),
        }
    }

    // --- A known Series2D projects to the expected dataset + bounds ----------

    #[test]
    fn test_project_series_known_points_projects_expected_dataset_and_bounds() {
        // x = {1, 2, 3}, y = {10, -5, 7}: the points project in order and the bounds
        // are the true min/max per axis.
        let graph = GraphData::Series(series(
            "payoff",
            &[dec(1, 0), dec(2, 0), dec(3, 0)],
            &[dec(10, 0), dec(-5, 0), dec(7, 0)],
        ));
        let projection = project(&graph);
        let s = ready(&projection);
        assert_eq!(s.points().len(), 3, "all three points survive");
        let expected = [(1.0, 10.0), (2.0, -5.0), (3.0, 7.0)];
        for (got, want) in s.points().iter().zip(expected.iter()) {
            assert_close(got.0, want.0);
            assert_close(got.1, want.1);
        }
        assert_close(s.x_bounds()[0], 1.0);
        assert_close(s.x_bounds()[1], 3.0);
        assert_close(s.y_bounds()[0], -5.0);
        assert_close(s.y_bounds()[1], 10.0);
        assert_eq!(s.name(), "payoff", "the series name projects through");
    }

    #[test]
    fn test_project_series_preserves_input_point_order() {
        // Points must not be reordered (a line chart connects them in order).
        let graph = GraphData::Series(series(
            "s",
            &[dec(5, 0), dec(1, 0), dec(9, 0)],
            &[dec(0, 0), dec(0, 0), dec(0, 0)],
        ));
        let s = ready(&project(&graph)).clone();
        let xs: Vec<f64> = s.points().iter().map(|(x, _)| *x).collect();
        assert_close(xs.first().copied().unwrap_or_default(), 5.0);
        assert_close(xs.get(1).copied().unwrap_or_default(), 1.0);
        assert_close(xs.get(2).copied().unwrap_or_default(), 9.0);
    }

    // --- Empty / degenerate GraphData -> the explicit empty state ------------

    #[test]
    fn test_project_empty_series_yields_empty_no_data() {
        // No points at all -> the deliberate "no data yet" state, not a panic.
        let graph = GraphData::Series(series("empty", &[], &[]));
        assert_eq!(
            project(&graph).empty_reason(),
            Some(EmptyReason::NoData),
            "an empty series projects Empty(NoData)",
        );
    }

    #[test]
    fn test_project_mismatched_lengths_yields_empty_degenerate() {
        // Malformed geometry (x and y differ in length) -> Empty(Degenerate), never
        // a silently truncated (invented) series.
        let graph = GraphData::Series(series("bad", &[dec(1, 0), dec(2, 0)], &[dec(3, 0)]));
        assert_eq!(
            project(&graph).empty_reason(),
            Some(EmptyReason::Degenerate),
            "a length mismatch projects Empty(Degenerate)",
        );
    }

    // --- MultiSeries stays Unsupported; GraphSurface now projects (#47) -------

    #[test]
    fn test_project_multiseries_variant_yields_empty_unsupported() {
        // MultiSeries (overlaid Greek curves) is NOT built — #47 cycles one Greek
        // curve at a time (g/G), so the overlay shape stays a deliberate
        // Empty(Unsupported), never a fabricated overlay.
        let graph = GraphData::MultiSeries(vec![series("a", &[dec(1, 0)], &[dec(2, 0)])]);
        assert_eq!(
            project(&graph).empty_reason(),
            Some(EmptyReason::Unsupported),
        );
    }

    #[test]
    fn test_project_graphsurface_builds_a_normalized_grid() {
        // A 2 strike × 2 vol GraphSurface projects to a 2×2 normalized-z heat map:
        // z ∈ {1, 2, 3, 4} → normalized {0, 1/3, 2/3, 1}. Row 0 is the HIGHEST vol
        // (chart y-up), column 0 the lowest strike.
        // Points (strike, vol, z): (10,0.2,1) (20,0.2,2) (10,0.4,3) (20,0.4,4).
        let graph = GraphData::GraphSurface(Surface3D {
            x: vec![dec(10, 0), dec(20, 0), dec(10, 0), dec(20, 0)],
            y: vec![dec(2, 1), dec(2, 1), dec(4, 1), dec(4, 1)],
            z: vec![dec(1, 0), dec(2, 0), dec(3, 0), dec(4, 0)],
            name: "delta".to_owned(),
        });
        let projection = project(&graph);
        let surface = match projection.ready_surface() {
            Some(s) => s,
            None => panic!("expected a ReadySurface, got {projection:?}"),
        };
        assert_eq!(surface.rows().len(), 2, "two vol rows");
        // Row 0 = highest vol (0.4): z = {3, 4} → normalized {2/3, 1}.
        let top = surface.rows().first().cloned().unwrap_or_default();
        assert_close(
            top.first().copied().flatten().unwrap_or_default(),
            2.0 / 3.0,
        );
        assert_close(top.get(1).copied().flatten().unwrap_or_default(), 1.0);
        // Row 1 = lowest vol (0.2): z = {1, 2} → normalized {0, 1/3}.
        let bottom = surface.rows().get(1).cloned().unwrap_or_default();
        assert_close(bottom.first().copied().flatten().unwrap_or_default(), 0.0);
        assert_close(
            bottom.get(1).copied().flatten().unwrap_or_default(),
            1.0 / 3.0,
        );
        assert_close(surface.x_bounds()[0], 10.0);
        assert_close(surface.x_bounds()[1], 20.0);
        assert_close(surface.z_bounds()[0], 1.0);
        assert_close(surface.z_bounds()[1], 4.0);
        assert_eq!(surface.name(), "delta");
    }

    #[test]
    fn test_project_empty_graphsurface_yields_empty_no_data() {
        let graph = GraphData::GraphSurface(Surface3D::default());
        assert_eq!(
            project(&graph).empty_reason(),
            Some(EmptyReason::NoData),
            "an empty surface projects Empty(NoData)",
        );
    }

    #[test]
    fn test_project_mismatched_graphsurface_yields_empty_degenerate() {
        // Mismatched x/y/z lengths are malformed geometry, never a truncated grid.
        let graph = GraphData::GraphSurface(Surface3D {
            x: vec![dec(1, 0), dec(2, 0)],
            y: vec![dec(1, 0)],
            z: vec![dec(1, 0)],
            name: "bad".to_owned(),
        });
        assert_eq!(
            project(&graph).empty_reason(),
            Some(EmptyReason::Degenerate),
        );
    }

    // --- The NaN/Inf finite gate ---------------------------------------------

    #[test]
    fn test_finite_xy_drops_nan_and_inf_keeps_finite() {
        // The single gate every point crosses: any non-finite coordinate is dropped
        // (None); a finite pair passes through unchanged.
        assert_eq!(finite_xy(f64::NAN, 1.0), None, "NaN x is dropped");
        assert_eq!(finite_xy(1.0, f64::NAN), None, "NaN y is dropped");
        assert_eq!(finite_xy(f64::INFINITY, 1.0), None, "+Inf x is dropped");
        assert_eq!(finite_xy(1.0, f64::NEG_INFINITY), None, "-Inf y is dropped");
        match finite_xy(2.0, -3.0) {
            Some((x, y)) => {
                assert_close(x, 2.0);
                assert_close(y, -3.0);
            }
            None => panic!("a finite pair must pass the gate"),
        }
    }

    #[test]
    fn test_coord_finite_decimal_converts_and_is_finite() {
        // A representable Decimal converts to a finite f64 (Series2D's Decimal domain
        // cannot itself be NaN/Inf, so the gate's enforced invariant lives at the f64
        // edge, where NaN/Inf is representable and tested above).
        assert!(coord(&dec(12345, 2)).is_finite());
        assert_close(coord(&dec(12345, 2)), 123.45);
    }

    // --- Axis bounds + labels precomputed off the draw path ------------------

    #[test]
    fn test_axis_bounds_from_values_computes_min_and_max() {
        match AxisBounds::from_values([3.0, -1.0, 7.5, 2.0].into_iter()) {
            Some(bounds) => {
                assert_close(bounds.min(), -1.0);
                assert_close(bounds.max(), 7.5);
                assert_eq!(bounds.to_array(), [-1.0, 7.5]);
            }
            None => panic!("non-empty values must yield bounds"),
        }
        assert_eq!(
            AxisBounds::from_values(std::iter::empty()),
            None,
            "empty values yield no bounds",
        );
    }

    #[test]
    fn test_axis_bounds_new_rejects_non_finite_and_unordered() {
        // The only constructor enforces "finite, ordered": NaN/Inf endpoints and an
        // inverted (max < min) range are rejected, so no bad range reaches a ratatui
        // axis or an axis label.
        assert_eq!(AxisBounds::new(f64::NAN, 1.0), None, "NaN min is rejected");
        assert_eq!(AxisBounds::new(1.0, f64::NAN), None, "NaN max is rejected");
        assert_eq!(
            AxisBounds::new(f64::INFINITY, 1.0),
            None,
            "+Inf min is rejected",
        );
        assert_eq!(
            AxisBounds::new(1.0, f64::NEG_INFINITY),
            None,
            "-Inf max is rejected",
        );
        assert_eq!(AxisBounds::new(5.0, 3.0), None, "max < min is rejected");
    }

    #[test]
    fn test_axis_bounds_new_accepts_valid_and_round_trips() {
        // Valid finite, ordered bounds round-trip through the accessors and to_array;
        // a degenerate min == max is accepted at the type level (padding is a
        // projection-seam concern, not a constructor one).
        match AxisBounds::new(-1.0, 7.5) {
            Some(bounds) => {
                assert_close(bounds.min(), -1.0);
                assert_close(bounds.max(), 7.5);
                assert_eq!(bounds.to_array(), [-1.0, 7.5]);
            }
            None => panic!("valid finite, ordered bounds must construct"),
        }
        assert!(
            AxisBounds::new(3.0, 3.0).is_some(),
            "a degenerate min == max is a valid (finite, ordered) interval",
        );
    }

    #[test]
    fn test_project_single_point_yields_ready_with_padded_bounds() {
        // A single point is renderable: the projection pads the degenerate min == max
        // interval so each axis has real width (a zero-width axis paints BLANK). The
        // point itself is unchanged; the pad is max(|v| * 0.005, 1.0) per axis.
        let graph = GraphData::Series(series("one", &[dec(42, 0)], &[dec(7, 0)]));
        let s = ready(&project(&graph)).clone();
        assert_eq!(s.points().len(), 1);
        let point = s.points().first().copied().unwrap_or_default();
        assert_close(point.0, 42.0);
        assert_close(point.1, 7.0);
        // x pad = max(42 * 0.005, 1.0) = 1.0 -> [41, 43]; y pad = max(7 * 0.005, 1.0)
        // = 1.0 -> [6, 8].
        assert_close(s.x_bounds()[0], 41.0);
        assert_close(s.x_bounds()[1], 43.0);
        assert_close(s.y_bounds()[0], 6.0);
        assert_close(s.y_bounds()[1], 8.0);
        assert!(
            s.x_bounds()[0] < s.x_bounds()[1],
            "the x axis has real width"
        );
        assert!(
            s.y_bounds()[0] < s.y_bounds()[1],
            "the y axis has real width"
        );
    }

    // --- A degenerate series still paints ink, never a blank chart ------------

    /// Render a projected series into a `Chart` on a `TestBackend` and count the
    /// non-blank cells — the "plot ink" a real screen would paint. Zero means the
    /// chart drew nothing (a blank void), which a zero-width axis range causes.
    #[track_caller]
    fn plot_ink_cells(series: &ProjectedSeries) -> usize {
        let backend = TestBackend::new(40, 12);
        let mut terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend terminal construction failed: {e}"),
        };
        let draw = |series: &ProjectedSeries, frame: &mut ratatui::Frame| {
            let dataset = Dataset::default()
                .name(series.name().to_owned())
                .marker(ratatui::symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .data(series.points());
            let chart = Chart::new(vec![dataset])
                .x_axis(Axis::default().bounds(series.x_bounds()))
                .y_axis(Axis::default().bounds(series.y_bounds()));
            frame.render_widget(chart, frame.area());
        };
        match terminal.draw(|frame| draw(series, frame)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count()
    }

    #[test]
    fn test_project_single_point_series_renders_non_blank_chart() {
        // Regression: a single-point series, padded to a real axis width, plots a
        // visible dot instead of a blank chart (a zero-width axis would reject it).
        let graph = GraphData::Series(series("one", &[dec(42, 0)], &[dec(7, 0)]));
        let s = ready(&project(&graph)).clone();
        assert!(
            plot_ink_cells(&s) > 0,
            "a single-point series must paint some plot ink, not a blank chart",
        );
    }

    #[test]
    fn test_project_flat_series_renders_non_blank_chart() {
        // Regression: a flat series (constant y across several x) collapses the y axis
        // to min == max; padding gives it width so the flat line is visible ink.
        let graph = GraphData::Series(series(
            "flat",
            &[dec(1, 0), dec(2, 0), dec(3, 0), dec(4, 0)],
            &[dec(5, 0), dec(5, 0), dec(5, 0), dec(5, 0)],
        ));
        let s = ready(&project(&graph)).clone();
        assert!(
            s.y_bounds()[0] < s.y_bounds()[1],
            "the flat y axis was padded to real width",
        );
        assert!(
            plot_ink_cells(&s) > 0,
            "a flat series must paint a visible flat line, not a blank chart",
        );
    }

    #[test]
    fn test_projected_series_labels_are_precomputed_min_mid_max() {
        // Labels are precomputed on the projection (a borrow at draw time, no
        // per-frame format!): [min, mid, max] for each axis.
        let graph = GraphData::Series(series(
            "s",
            &[dec(0, 0), dec(10, 0)],
            &[dec(-4, 0), dec(4, 0)],
        ));
        let s = ready(&project(&graph)).clone();
        assert_eq!(s.x_labels(), ["0.00", "5.00", "10.00"]);
        assert_eq!(s.y_labels(), ["-4.00", "0.00", "4.00"]);
    }

    // --- The cache handle: projection built off draw, only read in draw ------

    #[test]
    fn test_graph_cache_new_projects_once_and_caches() {
        // Construction projects off the draw path; projection() borrows the cached
        // result, and input() retains the source geometry.
        let graph = GraphData::Series(series("s", &[dec(1, 0)], &[dec(2, 0)]));
        let cache = GraphCache::new(graph.clone());
        assert_eq!(cache.input(), &graph, "the source GraphData is retained");
        assert!(
            cache.projection().ready().is_some(),
            "the projection is cached and Ready",
        );
    }

    #[test]
    fn test_graph_cache_update_reprojects_new_input() {
        // update() swaps the input and re-projects, off the draw path.
        let mut cache = GraphCache::new(GraphData::Series(series("empty", &[], &[])));
        assert_eq!(
            cache.projection().empty_reason(),
            Some(EmptyReason::NoData),
            "the initial empty series projects Empty(NoData)",
        );
        let next = GraphData::Series(series(
            "filled",
            &[dec(1, 0), dec(2, 0)],
            &[dec(3, 0), dec(4, 0)],
        ));
        cache.update(next.clone());
        assert_eq!(cache.input(), &next);
        assert!(
            cache.projection().ready().is_some(),
            "after update the projection is Ready",
        );
    }

    // --- Draw-purity: a draw reads the cached projection, builds no GraphData -

    #[test]
    fn test_draw_reads_cached_projection_builds_no_graphdata() {
        // A draw closure that receives only `&GraphCache` renders a ratatui Chart
        // from the CACHED projection: it constructs no GraphData and does no geometry
        // work, and the retained input GraphData is byte-for-byte unchanged across
        // the draw (the purity guarantee the cache enforces structurally).
        let graph = GraphData::Series(series(
            "payoff",
            &[dec(1, 0), dec(2, 0), dec(3, 0)],
            &[dec(-2, 0), dec(0, 0), dec(5, 0)],
        ));
        let cache = GraphCache::new(graph.clone());
        let before = cache.input().clone();

        let backend = TestBackend::new(80, 24);
        let mut terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => panic!("TestBackend terminal construction failed: {e}"),
        };
        let draw = |cache: &GraphCache, frame: &mut ratatui::Frame| {
            // The whole draw path reads ONLY the cached projection.
            if let Some(series) = cache.projection().ready() {
                let dataset = Dataset::default()
                    .name(series.name().to_owned())
                    .graph_type(GraphType::Line)
                    .data(series.points());
                let chart = Chart::new(vec![dataset])
                    .x_axis(Axis::default().bounds(series.x_bounds()))
                    .y_axis(Axis::default().bounds(series.y_bounds()));
                frame.render_widget(chart, frame.area());
            }
        };
        match terminal.draw(|frame| draw(&cache, frame)) {
            Ok(_) => {}
            Err(e) => panic!("draw failed: {e}"),
        }
        assert_eq!(
            cache.input(),
            &before,
            "a draw must not mutate or rebuild the cached GraphData",
        );
    }

    // --- Property: projecting any accepted GraphData never panics -------------

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Projecting an ARBITRARY `Series2D` (arbitrary decimals, possibly mismatched
        /// x/y lengths) never panics, and any Ready projection is well-formed: as many
        /// points as the paired inputs, all coordinates finite, and `min <= max` on
        /// both axes (`docs/TESTING.md` §3 `render_never_panics`, the projection side).
        #[test]
        fn test_project_arbitrary_series_never_panics_and_is_well_formed(
            xs in proptest::collection::vec((any::<i32>(), 0u32..6), 0..24),
            ys in proptest::collection::vec((any::<i32>(), 0u32..6), 0..24),
        ) {
            let to_decimals = |raw: &[(i32, u32)]| -> Vec<Decimal> {
                raw.iter().map(|(m, s)| Decimal::new(i64::from(*m), *s)).collect()
            };
            let xd = to_decimals(&xs);
            let yd = to_decimals(&ys);
            let graph = GraphData::Series(series("prop", &xd, &yd));

            // The projection must not panic for any input.
            let projection = project(&graph);
            if let GraphProjection::Ready(s) = &projection {
                prop_assert!(!s.points().is_empty(), "a Ready projection is non-empty");
                prop_assert!(
                    s.points().len() <= xd.len().min(yd.len()),
                    "no point is invented beyond the paired inputs",
                );
                for (x, y) in s.points() {
                    prop_assert!(x.is_finite() && y.is_finite(), "no non-finite coord survives");
                }
                prop_assert!(s.x_bounds()[0] <= s.x_bounds()[1], "x min <= max");
                prop_assert!(s.y_bounds()[0] <= s.y_bounds()[1], "y min <= max");
            }
        }

        /// Projecting an ARBITRARY `Surface3D` (arbitrary decimals, possibly
        /// mismatched lengths) never panics, and a ReadySurface is well-formed: a
        /// rectangular grid, every cell in `[0, 1]` (or a gap), and `min <= max` on
        /// all three axes.
        #[test]
        fn test_project_arbitrary_surface_never_panics_and_is_well_formed(
            xs in proptest::collection::vec((0i32..8, 0u32..3), 0..24),
            ys in proptest::collection::vec((0i32..4, 0u32..2), 0..24),
            zs in proptest::collection::vec((any::<i32>(), 0u32..3), 0..24),
        ) {
            let to_decimals = |raw: &[(i32, u32)]| -> Vec<Decimal> {
                raw.iter().map(|(m, s)| Decimal::new(i64::from(*m), *s)).collect()
            };
            let graph = GraphData::GraphSurface(Surface3D {
                x: to_decimals(&xs),
                y: to_decimals(&ys),
                z: to_decimals(&zs),
                name: "prop".to_owned(),
            });
            let projection = project(&graph);
            if let GraphProjection::ReadySurface(s) = &projection {
                let cols = s.rows().first().map_or(0, Vec::len);
                for row in s.rows() {
                    prop_assert_eq!(row.len(), cols, "the grid is rectangular");
                    for v in row.iter().flatten() {
                        prop_assert!((0.0..=1.0).contains(v), "z normalized into [0,1]");
                    }
                }
                prop_assert!(s.x_bounds()[0] <= s.x_bounds()[1], "x min <= max");
                prop_assert!(s.y_bounds()[0] <= s.y_bounds()[1], "y min <= max");
                prop_assert!(s.z_bounds()[0] <= s.z_bounds()[1], "z min <= max");
            }
        }
    }
}
