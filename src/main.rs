//! CLI entry point for the `chainview` binary.
//!
//! Owns the CLI grammar (`docs/07-configuration.md` §4) and loads the typed
//! [`Config`](chainview::Config). Replay mode is entered by the
//! `chainview replay <dir>` **subcommand** (§4.1), never a `--replay` flag.
//!
//! Startup order (`docs/02-tui-architecture.md` §6): parse the CLI, load `.env`,
//! assemble the config, install the panic hook, then enter the terminal under an
//! RAII [`TerminalGuard`](chainview::TerminalGuard) whose `Drop` restores the
//! shell on every exit path. The tokio runtime, event loop, and render loop land
//! in later issues (#9/#11/#13); this entry point returns the typed
//! [`ChainViewError`](chainview::ChainViewError), so `anyhow` is not needed here
//! and the `main.rs` `anyhow` deviation (CLAUDE.md "Governance precedence") is
//! left untouched.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use chainview::config::{CliOverrides, Config, ModeSelect};
use chainview::{ChainViewError, TerminalGuard, install_panic_hook};
use clap::{Args, Parser, Subcommand};

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
    let _config = Config::load(overrides)?;

    // Install the panic hook BEFORE entering the alternate screen so a panic at
    // any later point restores the terminal before the backtrace prints
    // (`docs/02-tui-architecture.md` §6). The TUI-safe tracing file/pane sink
    // (CLAUDE.md "Governance precedence" item 3) is wired in #3/#11; until then
    // nothing competes with the TUI for stdout.
    install_panic_hook();

    // Enter raw mode + the alternate screen under an RAII guard. Its `Drop`
    // restores the terminal on EVERY exit path from here on — a normal return,
    // an early `?`, or a panic. No `std::process::exit` runs while the guard is
    // held, so `Drop` is never bypassed; the supervised, ordered teardown that
    // sequences this last lands in #11.
    let _guard = TerminalGuard::new()?;

    // The event fan-in, provider tasks, and render loop land in #9/#11/#13. Today
    // the assembled config and the guard prove the lifecycle end to end: on
    // return `_guard` drops and restores the terminal.
    Ok(())
}
