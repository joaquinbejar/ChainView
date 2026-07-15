//! CLI entry point for the `chainview` binary.
//!
//! Owns the CLI grammar (`docs/07-configuration.md` §4) and loads the typed
//! [`Config`](chainview::Config). Replay mode is entered by the
//! `chainview replay <dir>` **subcommand** (§4.1), never a `--replay` flag.
//!
//! This is deliberately minimal: parse the CLI, load `.env` into the
//! environment, assemble the config. The terminal setup with guaranteed
//! restore, the tokio runtime, and the render loop land in later issues.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use chainview::ConfigError;
use chainview::config::{CliOverrides, Config, ModeSelect};
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

fn main() -> Result<(), ConfigError> {
    // Load `.env` from the working directory into the process environment before
    // reading config (startup glue, `docs/07-configuration.md` §2). Absence is
    // not an error.
    let _ = dotenvy::dotenv();

    let overrides = Cli::parse().into_overrides();
    let _config = Config::load(overrides)?;

    // No runtime behavior yet — the terminal and render loop land in later
    // issues. `_config` is assembled and validated, then dropped.
    Ok(())
}
