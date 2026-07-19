//! CLI entry point for the `chainview` binary.
//!
//! Owns the CLI grammar (`docs/07-configuration.md` §4) and loads the typed
//! [`Config`](chainview::Config). Replay mode is entered by the
//! `chainview replay <dir>` **subcommand** (§4.1), never a `--replay` flag.
//!
//! # The live-loop composition lives here (ADR-0009, `docs/02-tui-architecture.md`
//! §12)
//!
//! The synchronous render loop is in the `ui` layer and the application layer must
//! not import `crate::ui` (the arch fence, `tests/arch.rs`), so the binary is the
//! one place that wires **both** halves together. Startup order
//! (`docs/02-tui-architecture.md` §6): parse the CLI, load `.env`, assemble the
//! config, install the panic hook, then — for a live session — build the tokio
//! runtime and compose the loop:
//!
//! 1. resolve the provider + [`SourceBinding`] via
//!    [`ChainViewApp::builder()…resolve()`](chainview::ChainViewApp);
//! 2. seed the [`ChainStore`](chainview::ChainStore) from the provider's first
//!    `fetch_chain` (best-effort — an initial failure seeds an empty chain and the
//!    supervised reconnect loop retries, so the loop still runs and renders the
//!    honest connecting state);
//! 3. build the bounded, coalescing [`EventBridge`](chainview::EventBridge) and the
//!    [`Supervisor`](chainview::Supervisor) that owns the ordered,
//!    terminal-restored-last teardown;
//! 4. register the provider stream task via
//!    [`spawn_supervised_subscription`](chainview::spawn_supervised_subscription)
//!    (watched, so a mid-run panic/return wakes the supervisor), then the
//!    tick + input tasks (ancillary), then the render loop (the render task) under
//!    an RAII [`TerminalGuard`](chainview::TerminalGuard) whose `Drop` restores the
//!    shell on every exit path;
//! 5. `supervisor.run().await` supervises until the first shutdown trigger, joins
//!    provider → ancillary → render, and restores the terminal **last**.
//!
//! `anyhow` is intentionally NOT used: the typed [`ChainViewError`](chainview::ChainViewError)
//! carries every startup/teardown failure, so the CLAUDE.md `main.rs`-`anyhow`
//! deviation is left unexercised.

#![forbid(unsafe_code)]

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chainview::config::{CliOverrides, Config, ModeSelect};
use chainview::{
    AliasCatalog, App, BundleError, ChainFetch, ChainSource, ChainStore, ChainViewApp,
    ChainViewError, Command as DataCommand, EventBridge, ExitCause, ExpirySource, GuardTeardown,
    Instrument, LiveState, Mode, ReplayState, Resolved, ResourceCeilings, SourceBinding,
    Supervisor, TerminalGuard, TokioTask, event_channel, install_panic_hook, run_render_loop,
    spawn_bundle_load, spawn_input_reader, spawn_supervised_subscription, spawn_tick_task,
};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use optionstratlib::ExpirationDate;
use optionstratlib::chains::chain::OptionChain;
use optionstratlib::prelude::Positive;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// `chainview` — a terminal UI for option chains, Greeks, and volatility.
#[derive(Debug, Parser)]
#[command(name = "chainview", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    live: LiveArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands. The default (none) is Live mode.
#[derive(Debug, Subcommand)]
enum Command {
    /// Render an IronCondor result bundle (replay mode; no network).
    Replay {
        /// The result-bundle directory to open, read-only.
        dir: PathBuf,
    },
}

/// The live-mode / global flags (`docs/07-configuration.md` §4). All are
/// optional; an absent flag falls to the env, then the file, then the typed
/// default.
#[derive(Debug, Args)]
struct LiveArgs {
    /// Market-data provider id (default: deribit). Live-only.
    #[arg(long)]
    provider: Option<String>,
    /// Underlying ticker (default: BTC). Live-only.
    #[arg(long)]
    underlying: Option<String>,
    /// Chain refresh cadence, e.g. `2s`; range [250ms, 300s]. Live-only.
    #[arg(long)]
    refresh: Option<String>,
    /// UI tick cadence, e.g. `250ms`; range [50ms, 5s].
    #[arg(long)]
    tick: Option<String>,
    /// Bounded channel capacity in messages; range [64, 65536].
    #[arg(long = "channel-cap")]
    channel_cap: Option<i64>,
    /// Log-file path; never stdout/stderr while the TUI runs.
    #[arg(long = "log-file")]
    log_file: Option<PathBuf>,
    /// Color theme: `auto`, `dark`, or `light`.
    #[arg(long)]
    theme: Option<String>,
    /// Disable color output (also honored via the `NO_COLOR` environment
    /// variable).
    #[arg(long = "no-color")]
    no_color: bool,
    /// Per-provider endpoint override (absolute URL). Live-only.
    #[arg(long)]
    endpoint: Option<String>,
}

impl Cli {
    /// Lower the parsed CLI into the parser-agnostic [`CliOverrides`] the config
    /// loader consumes.
    fn into_overrides(self) -> CliOverrides {
        let mode = match self.command {
            Some(Command::Replay { dir }) => ModeSelect::Replay(dir),
            None => ModeSelect::Live,
        };
        let live = self.live;
        CliOverrides {
            provider: live.provider,
            underlying: live.underlying,
            refresh_interval: live.refresh,
            tick_interval: live.tick,
            channel_capacity: live.channel_cap,
            log_file: live.log_file,
            theme: live.theme,
            no_color: live.no_color,
            endpoint: live.endpoint,
            mode,
        }
    }
}

fn main() -> Result<(), ChainViewError> {
    // Load `.env` from the working directory into the process environment before
    // reading config (startup glue, `docs/07-configuration.md` §2). Absence is
    // not an error.
    let _ = dotenvy::dotenv();

    let overrides = Cli::parse().into_overrides();
    // `ConfigError` folds into `ChainViewError::Config` via `#[from]`, so an
    // early `?` here returns before any terminal setup — stderr is safe.
    let config = Config::load(overrides)?;

    // Replay pre-flight: a bundle directory that does not exist is almost always a
    // typo, so fail fast with a friendly CLI error on the NORMAL terminal, BEFORE
    // entering the alternate screen (`docs/07-configuration.md` §4.1). A malformed
    // but present bundle is NOT rejected here — it becomes a retryable
    // `BundleLoad::Error` inside the TUI (`docs/05-views-and-ux.md` §6), so only a
    // missing/not-a-directory path is a pre-TUI error.
    if let ModeSelect::Replay(dir) = &config.mode {
        validate_replay_dir(dir)?;
    }

    // Install the panic hook BEFORE entering the alternate screen so a panic at any
    // later point restores the terminal before the backtrace prints
    // (`docs/02-tui-architecture.md` §6). While the supervisor drives the ordered
    // teardown it owns the single restore and the hook defers (§12).
    install_panic_hook();

    // Resolve the registry + mode. Resolution is the validation seam: an empty
    // registry / unknown provider / chain-less source surfaces here (stderr-safe,
    // no terminal entered yet). The zero-config Deribit BTC path resolves with no
    // network (capabilities are static).
    match ChainViewApp::builder()
        .with_builtins()
        .with_config(config)
        .resolve()?
    {
        Resolved::Live {
            provider,
            source,
            config,
        } => run_live(provider, source, config),
        Resolved::Replay { dir, config } => run_replay(dir, config),
    }
}

/// Compose and drive the live loop on a fresh tokio runtime, returning once the
/// supervisor has restored the terminal (`docs/02-tui-architecture.md` §12).
fn run_live(
    provider: std::sync::Arc<dyn chainview::Provider>,
    source: SourceBinding,
    config: Config,
) -> Result<(), ChainViewError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ChainViewError::Terminal(format!("tokio runtime: {e}")))?;
    let cause = runtime.block_on(compose_and_run_live(provider, source, config));
    // The terminal has already been restored by the supervisor (LAST step), so a
    // post-restore stderr line is safe (CLAUDE.md "Governance precedence" item 3).
    match cause {
        ExitCause::Clean => Ok(()),
        ExitCause::TaskPanicked => Err(ChainViewError::Terminal(
            "a supervised task panicked; see the log".to_owned(),
        )),
        ExitCause::Failed(error) => Err(error),
    }
}

/// The async live composition: seed the store, build the bridge + supervisor,
/// register the provider stream (watched) + the tick/input/render tasks, and drive
/// the supervisor to its ordered, terminal-restored-last teardown.
async fn compose_and_run_live(
    provider: std::sync::Arc<dyn chainview::Provider>,
    source: SourceBinding,
    config: Config,
) -> ExitCause {
    let now = now_utc();

    // (2) Seed the store from the provider's first fetch (best-effort). A default
    // near-term expiry is used for the zero-config path; on any failure the store
    // is seeded EMPTY so the loop still runs and renders the honest connecting
    // state while the supervised reconnect loop retries.
    let expiration = ExpirationDate::Days(positive_or_one(7.0));
    let (fetch, instruments) = match provider.fetch_chain(&config.underlying, &expiration).await {
        Ok(fetch) => {
            let instruments: Vec<Instrument> = fetch.aliases.instruments().cloned().collect();
            (fetch, instruments)
        }
        Err(_) => (empty_seed(&config.underlying, &source, now), Vec::new()),
    };
    // The absolute-UTC expiry the subscription is scoped to — resolved by the poll
    // leg (or the empty-seed placeholder), never a relative offset.
    let expiration_utc = fetch.expiry_source.expiration_utc;
    let store = ChainStore::seed(fetch, ChainSource::Merged, config.refresh_interval, now);
    let live = LiveState::new(source, store);

    // (3) The bounded, coalescing bridge + the App (holding the render -> data
    // command sender).
    let (mut bridge, senders) = EventBridge::new(config.channel_capacity);
    let mut app = App::new(Mode::Live(live), config.theme, senders.tx_command.clone())
        .with_no_color(config.no_color);

    // (4) The single supervisor owning the ordered, terminal-restored-last
    // teardown. The TerminalGuard enters raw mode + the alternate screen; the
    // GuardTeardown drops it LAST.
    let guard = match TerminalGuard::new() {
        Ok(guard) => guard,
        Err(error) => return ExitCause::Failed(error),
    };
    let mut supervisor = Supervisor::new(Box::new(GuardTeardown::new(guard)));

    // The provider stream task, registered under the supervisor's `watch` seam so a
    // mid-run panic/return wakes the loop and it is reaped at teardown.
    let _subscription = match spawn_supervised_subscription(
        &provider,
        &config.underlying,
        expiration_utc,
        instruments,
        &senders,
        &mut supervisor,
    )
    .await
    {
        Ok(subscription) => subscription,
        Err(error) => {
            supervisor.fail(ChainViewError::provider(provider.id(), error));
            return supervisor.run().await;
        }
    };

    // The input/tick tasks (ancillary) feed the bounded AppEvent channel; only they
    // hold the sender, so both ending closes the channel and the render loop's
    // `blocking_recv` returns `None`.
    let (tx_events, mut rx_events) = event_channel();
    let tick_child = supervisor.child_token();
    let tick = spawn_tick_task(config.tick_interval, tx_events.clone(), tick_child.clone());
    supervisor.register_ancillary(tick_child, Box::new(TokioTask::new(tick)));
    let input_child = supervisor.child_token();
    let input = spawn_input_reader(tx_events, input_child.clone());
    supervisor.register_ancillary(input_child, Box::new(TokioTask::new(input)));

    // The render loop on a dedicated blocking thread (so `blocking_recv` is legal).
    // On quit it cancels the root so the supervisor tears down.
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            supervisor.fail(ChainViewError::Terminal(error.to_string()));
            return supervisor.run().await;
        }
    };
    let render_child = supervisor.child_token();
    let root = supervisor.root_token();
    let render = tokio::task::spawn_blocking(move || {
        render_thread(terminal, &mut app, &mut bridge, &mut rx_events);
        // The loop returned (quit or channel close): trip the root so the
        // supervisor runs the ordered teardown.
        root.cancel();
    });
    supervisor.set_render(render_child, Box::new(TokioTask::new(render)));

    // Drop the last stray sender clones so the bridge channels close cleanly once
    // the provider + app halves drop.
    drop(senders);

    // (5) Supervise until the first shutdown trigger, then the ordered teardown
    // restores the terminal LAST.
    supervisor.run().await
}

/// The synchronous render loop body on the blocking render thread. A command from
/// the fold is currently a no-op route (per-provider recovery routing lands with
/// the navigation layer); the loop is driven purely by the bounded channels.
fn render_thread(
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    bridge: &mut EventBridge,
    rx_events: &mut tokio::sync::mpsc::Receiver<chainview::AppEvent>,
) {
    // A draw failure ends the loop; the supervisor's teardown still restores the
    // terminal. The route closure is a no-op in v0.1 (commands are surfaced, not
    // yet acted on beyond the seam). The ViewState is the render-loop-owned
    // projection cache (#27): geometry projects off-draw in its sync step.
    let mut view = chainview::ViewState::new();
    let _ = run_render_loop(
        &mut terminal,
        app,
        bridge,
        &mut view,
        rx_events,
        |_command| {},
    );
}

/// Compose and drive the replay loop on a fresh tokio runtime, returning once the
/// supervisor has restored the terminal (`docs/02-tui-architecture.md` §12).
///
/// Mirrors [`run_live`] but with **no** provider/bridge market wiring: replay
/// renders a static IronCondor bundle, so the only data task is the off-thread
/// bundle load ([`spawn_bundle_load`]). The coalescing [`EventBridge`] is still
/// built — the render loop pumps it and drains the render -> data command channel
/// through it — but nothing ever streams onto its market channels.
fn run_replay(dir: PathBuf, config: Config) -> Result<(), ChainViewError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ChainViewError::Terminal(format!("tokio runtime: {e}")))?;
    let cause = runtime.block_on(compose_and_run_replay(dir, config));
    // The terminal has already been restored by the supervisor (LAST step), so a
    // post-restore stderr line is safe (CLAUDE.md "Governance precedence" item 3).
    match cause {
        ExitCause::Clean => Ok(()),
        ExitCause::TaskPanicked => Err(ChainViewError::Terminal(
            "a supervised task panicked; see the log".to_owned(),
        )),
        ExitCause::Failed(error) => Err(error),
    }
}

/// The async replay composition: build the bridge + supervisor, start the
/// off-thread bundle load (Loading -> Ready/Error), spawn the tick/input/render
/// tasks, and drive the supervisor to its ordered, terminal-restored-last teardown.
///
/// Unlike [`compose_and_run_live`] there is no provider stream to seed or supervise;
/// the [`App`] starts in [`Mode::Replay`] at [`BundleLoad::Loading`] and the initial
/// [`spawn_bundle_load`] fills it. The render loop's route closure respawns the load
/// on [`chainview::Command::ReloadBundle`] so the `R` retry works end to end.
async fn compose_and_run_replay(dir: PathBuf, config: Config) -> ExitCause {
    // (1) The bounded, coalescing bridge (replay never streams market data onto it,
    // but the render loop still pumps it and drains its command channel) + the App
    // in Replay mode, which starts at `BundleLoad::Loading`.
    let (mut bridge, senders) = EventBridge::new(config.channel_capacity);
    let replay = ReplayState::new(dir.clone());
    let mut app = App::new(
        Mode::Replay(replay),
        config.theme,
        senders.tx_command.clone(),
    )
    .with_no_color(config.no_color);

    // (2) The single supervisor owning the ordered, terminal-restored-last teardown.
    // The TerminalGuard enters raw mode + the alternate screen; the GuardTeardown
    // drops it LAST.
    let guard = match TerminalGuard::new() {
        Ok(guard) => guard,
        Err(error) => return ExitCause::Failed(error),
    };
    let mut supervisor = Supervisor::new(Box::new(GuardTeardown::new(guard)));

    // (3) The bounded AppEvent channel the render loop parks on. The tick/input tasks
    // and every bundle-load worker feed it; when every producer half drops, the
    // loop's `blocking_recv` returns `None`.
    let (tx_events, mut rx_events) = event_channel();

    // (4) The initial off-thread bundle load, registered as an ancillary task so
    // shutdown cancels it (at the next decode batch boundary) and joins it. It opens,
    // decodes, and validates the bundle on a blocking worker and posts a single
    // `AppEvent::BundleLoaded` (Ready or a retryable Error) back up the channel.
    let ceilings = ResourceCeilings::default();
    let load_child = supervisor.child_token();
    let load = spawn_bundle_load(dir, ceilings, tx_events.clone(), load_child.clone());
    supervisor.register_ancillary(load_child, Box::new(TokioTask::new(load)));

    // (5) The tick + input tasks (ancillary): the tick animates the loading spinner
    // and advances playback; the input reader feeds keys (scrub, `R` reload, quit).
    let tick_child = supervisor.child_token();
    let tick = spawn_tick_task(config.tick_interval, tx_events.clone(), tick_child.clone());
    supervisor.register_ancillary(tick_child, Box::new(TokioTask::new(tick)));
    // A dedicated sender clone for the render loop's reload route (below); the input
    // reader takes the original.
    let reload_tx = tx_events.clone();
    let input_child = supervisor.child_token();
    let input = spawn_input_reader(tx_events, input_child.clone());
    supervisor.register_ancillary(input_child, Box::new(TokioTask::new(input)));

    // (6) The render loop on a dedicated blocking thread (so `blocking_recv` is
    // legal). Its route closure respawns `spawn_bundle_load` on
    // `Command::ReloadBundle` (`R`), so the reload retry works end to end; each
    // reload runs under a fresh child of the root token, so teardown aborts an
    // in-flight one. On quit the loop cancels the root so the supervisor tears down.
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            supervisor.fail(ChainViewError::Terminal(error.to_string()));
            return supervisor.run().await;
        }
    };
    let render_child = supervisor.child_token();
    let root = supervisor.root_token();
    // A second root clone: the render loop mints per-reload child tokens from it,
    // while `root` (above) is the one it cancels on quit.
    let reload_cancel = supervisor.root_token();
    let render = tokio::task::spawn_blocking(move || {
        replay_render_thread(
            terminal,
            &mut app,
            &mut bridge,
            &mut rx_events,
            ceilings,
            reload_tx,
            reload_cancel,
        );
        // The loop returned (quit): trip the root so the supervisor runs the ordered
        // teardown.
        root.cancel();
    });
    supervisor.set_render(render_child, Box::new(TokioTask::new(render)));

    // Drop the stray bridge sender clones so the market/command channels close
    // cleanly once the app half drops.
    drop(senders);

    // (7) Supervise until the first shutdown trigger, then the ordered teardown
    // restores the terminal LAST.
    supervisor.run().await
}

/// The synchronous replay render loop body on the blocking render thread. Unlike
/// the live [`render_thread`], its route closure is **not** a no-op: it respawns the
/// off-thread bundle load on [`chainview::Command::ReloadBundle`] via
/// [`route_replay_command`] so the `R` retry re-opens the bundle end to end. The
/// render thread holds a
/// `tx_events` sender (for reloads) for the loop's lifetime, which is why the loop
/// ends on [`App::should_quit`](chainview::App) rather than on channel close — in
/// replay the only shutdown trigger is the quit key, since there are no provider or
/// watched tasks to trip the root mid-loop.
fn replay_render_thread(
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    bridge: &mut EventBridge,
    rx_events: &mut tokio::sync::mpsc::Receiver<chainview::AppEvent>,
    ceilings: ResourceCeilings,
    tx_events: tokio::sync::mpsc::Sender<chainview::AppEvent>,
    cancel: tokio_util::sync::CancellationToken,
) {
    // The ViewState is the render-loop-owned projection cache (#27). Replay does not
    // project a payoff series today, but the render loop still owns and syncs it.
    let mut view = chainview::ViewState::new();
    let _ = run_render_loop(
        &mut terminal,
        app,
        bridge,
        &mut view,
        rx_events,
        |command| route_replay_command(command, ceilings, &tx_events, &cancel),
    );
}

/// Route one render-loop -> data-layer [`chainview::Command`] in replay mode. Only
/// [`chainview::Command::ReloadBundle`] is actionable here: it respawns the
/// off-thread bundle load under a fresh child of `cancel` (so a shutdown-time root
/// cancel aborts an in-flight reload at the next decode batch boundary), and the
/// worker posts the outcome back as an [`AppEvent::BundleLoaded`] on `tx_events` —
/// the same channel the render loop parks on. The live-only subscription/recovery
/// commands never arise in replay (there is no provider), so they are matched
/// explicitly and ignored — no wildcard arm, so a new [`chainview::Command`] variant
/// forces this site to be revisited by the compiler.
///
/// Must run within a tokio runtime context (it spawns the load); the render thread
/// is a [`spawn_blocking`](tokio::task::spawn_blocking) task, which carries that
/// context.
fn route_replay_command(
    command: DataCommand,
    ceilings: ResourceCeilings,
    tx_events: &tokio::sync::mpsc::Sender<chainview::AppEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) {
    match command {
        DataCommand::ReloadBundle(dir) => {
            // The reload's shutdown path is its `cancel` child (cascaded by the root
            // at teardown), not the returned handle: this render-thread route cannot
            // reach the supervisor (it is owned by the concurrent `run`), so the
            // handle is dropped and the token is the honest shutdown path. A closed
            // event channel drops the worker's outcome harmlessly.
            let _load = spawn_bundle_load(dir, ceilings, tx_events.clone(), cancel.child_token());
        }
        DataCommand::Subscribe { .. }
        | DataCommand::Unsubscribe { .. }
        | DataCommand::Reconnect
        | DataCommand::Rediscover => {}
    }
}

/// A [`Positive`] from a controlled startup value, degrading a non-positive/`NaN`
/// input to `1.0` rather than panicking (no `unwrap`/`expect` in `main`).
fn positive_or_one(value: f64) -> Positive {
    Positive::new(value).unwrap_or_else(|_| positive_one())
}

/// The constant `1.0` as a [`Positive`] — the empty-seed spot placeholder.
fn positive_one() -> Positive {
    Positive::new(1.0).unwrap_or(Positive::ZERO)
}

/// An empty [`ChainFetch`] used to seed the store when the initial fetch fails, so
/// the loop still runs and renders the connecting/empty state while the reconnect
/// loop retries. Carries the source provider + a placeholder expiry so the
/// [`ExpirySource`] identity is well-formed.
fn empty_seed(underlying: &str, source: &SourceBinding, now: DateTime<Utc>) -> ChainFetch {
    let chain = OptionChain::new(underlying, positive_one(), now.to_rfc3339(), None, None);
    ChainFetch::new(
        chain,
        ExpirySource::new(underlying, now, source.provider.clone()),
        AliasCatalog::new(),
    )
}

/// The current wall-clock instant from `std`'s clock (chrono's `clock` feature is
/// off), clamped to the representable range, never `unwrap`ping.
fn now_utc() -> DateTime<Utc> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp(secs, since.subsec_nanos()).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Validate a replay bundle directory before the TUI starts: it must exist and be
/// a directory (`docs/07-configuration.md` §4.1). A missing or non-directory path
/// is a friendly, pre-TUI [`ChainViewError::Bundle`] naming only the path the user
/// passed — never any other filesystem detail. A present-but-malformed bundle is
/// handled inside the TUI as a retryable load error, not here.
fn validate_replay_dir(dir: &Path) -> Result<(), ChainViewError> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(
            BundleError::Io(format!("replay path is not a directory: {}", dir.display())).into(),
        ),
        Err(_) => Err(BundleError::Io(format!(
            "replay bundle directory not found: {}",
            dir.display()
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, route_replay_command, validate_replay_dir};
    use chainview::config::ModeSelect;
    use chainview::{
        AppEvent, BundleLoadResult, ChainViewError, Command as DataCommand, ResourceCeilings,
    };
    use clap::Parser;
    use std::path::PathBuf;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Parse an argv into the lowered [`ModeSelect`], or a clap error.
    fn mode_of(args: &[&str]) -> Result<ModeSelect, clap::Error> {
        Cli::try_parse_from(args).map(|cli| cli.into_overrides().mode)
    }

    #[test]
    fn test_cli_no_subcommand_selects_live() {
        match mode_of(&["chainview"]) {
            Ok(mode) => assert_eq!(mode, ModeSelect::Live),
            Err(e) => panic!("expected Live, got parse error: {e}"),
        }
    }

    #[test]
    fn test_cli_replay_subcommand_selects_replay_with_dir() {
        match mode_of(&["chainview", "replay", "./run-2026-07-01/"]) {
            Ok(mode) => assert_eq!(mode, ModeSelect::Replay(PathBuf::from("./run-2026-07-01/"))),
            Err(e) => panic!("expected Replay, got parse error: {e}"),
        }
    }

    #[test]
    fn test_cli_replay_ignores_live_only_flags() {
        // The live-only flags parse alongside `replay` but are lowered as a no-op
        // (the config layer drops them) — never a live/replay hybrid.
        match mode_of(&[
            "chainview",
            "--provider",
            "ig",
            "--underlying",
            "SPY",
            "replay",
            "./bundle/",
        ]) {
            Ok(mode) => assert_eq!(mode, ModeSelect::Replay(PathBuf::from("./bundle/"))),
            Err(e) => panic!("expected Replay, got parse error: {e}"),
        }
    }

    #[test]
    fn test_cli_replay_requires_a_directory() {
        // `replay` with no positional is a clap error, not a silent Live fallback.
        assert!(mode_of(&["chainview", "replay"]).is_err());
    }

    #[test]
    fn test_cli_replay_rejects_extra_positional() {
        assert!(mode_of(&["chainview", "replay", "./a", "./b"]).is_err());
    }

    #[test]
    fn test_validate_replay_dir_accepts_an_existing_directory() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("failed to make a temp dir: {e}"),
        };
        assert!(validate_replay_dir(dir.path()).is_ok());
    }

    #[test]
    fn test_validate_replay_dir_rejects_a_missing_directory() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("failed to make a temp dir: {e}"),
        };
        let missing = dir.path().join("does-not-exist");
        match validate_replay_dir(&missing) {
            Err(ChainViewError::Bundle(_)) => {}
            other => panic!("expected a friendly Bundle error, got {other:?}"),
        }
    }

    #[test]
    fn test_validate_replay_dir_rejects_a_file() {
        use std::io::Write;
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("failed to make a temp dir: {e}"),
        };
        let file_path = dir.path().join("manifest.json");
        match std::fs::File::create(&file_path).and_then(|mut f| f.write_all(b"{}")) {
            Ok(()) => {}
            Err(e) => panic!("failed to write a temp file: {e}"),
        }
        match validate_replay_dir(&file_path) {
            Err(ChainViewError::Bundle(_)) => {}
            other => panic!("expected a friendly Bundle error for a file, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_route_replay_command_reload_respawns_load_and_emits_bundle_loaded() {
        // Finding 1 (#34): `Command::ReloadBundle` from the render loop's route
        // closure respawns the off-thread bundle load, which posts an
        // `AppEvent::BundleLoaded` back up the event channel — the seam that makes
        // the `R` retry work end to end. A missing directory decodes to a retryable
        // `Failed`, proving the wiring without a TTY.
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let cancel = CancellationToken::new();
        let dir = std::env::temp_dir().join("chainview-route-missing-bundle-34");
        let _ = std::fs::remove_dir_all(&dir);
        route_replay_command(
            DataCommand::ReloadBundle(dir),
            ResourceCeilings::default(),
            &tx,
            &cancel,
        );
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Some(AppEvent::BundleLoaded(BundleLoadResult::Failed(message)))) => {
                assert!(
                    !message.is_empty(),
                    "the failure carries a non-secret message"
                );
            }
            other => {
                panic!("expected BundleLoaded(Failed) from the respawned load, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn test_route_replay_command_ignores_live_only_commands() {
        // The live-only recovery commands are meaningless in replay: the route
        // neither spawns a load nor emits any event for them.
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let cancel = CancellationToken::new();
        for command in [DataCommand::Reconnect, DataCommand::Rediscover] {
            route_replay_command(command, ResourceCeilings::default(), &tx, &cancel);
        }
        assert!(
            rx.try_recv().is_err(),
            "live-only commands produce no replay event and spawn no load",
        );
    }
}
