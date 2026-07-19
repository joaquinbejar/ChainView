//! Typed configuration surface for ChainView.
//!
//! This module is the **single canonical configuration contract**
//! (`docs/07-configuration.md` §1): the sources it reads, their precedence, the
//! typed [`Config`] schema, `humantime` durations, range/typo validation, the
//! env-only secret policy, and the reversible provider-id ↔ environment-segment
//! bijection the open-provider model requires (`docs/07-configuration.md` §5.1,
//! ADR-0006).
//!
//! # Precedence
//!
//! ```text
//! CLI flag  >  environment variable  >  config-file key  >  typed default
//! ```
//!
//! A value absent from every source falls to its typed default; a fresh install
//! with no flags, no env, and no file resolves to the zero-config Deribit BTC
//! path (`provider = deribit`, `underlying = BTC`, ADR-0003).
//!
//! # Secrets are the one exception
//!
//! Provider credentials are read from the **environment only** — never from the
//! file and never from a flag. A credential key in a file is a
//! [`ConfigError::InvalidValue`]; there is no credential flag by construction.
//! A missing required credential is [`ConfigError::MissingCredential`], which
//! names the **provider**, never the key or its value. Resolved credential
//! values are wrapped in [`Secret`], whose `Debug` is redacted, so a secret
//! cannot reach a log line, an error, or `Debug` output.
//!
//! # Assembly vs. I/O
//!
//! [`Config::assemble`] is pure: it takes the CLI overrides, an [`EnvSource`],
//! and the optional config-file **contents**, and never touches the filesystem
//! — so precedence and validation are unit-testable without mutating global
//! process state. [`Config::load`] is the startup entry point that performs the
//! I/O (reads the process environment and the XDG-resolved file) and delegates
//! to [`Config::assemble`].
//!
//! # Hand-off to issue #4
//!
//! Provider-id **grammar** validation (`^[a-z][a-z0-9_-]{1,31}$`) is performed
//! locally here by [`validate_provider_id`]. Issue #4 centralizes it in
//! `ProviderId::new` (making that constructor fallible); this module then
//! delegates to it. Registry absence (`UnknownProvider`) is validated at
//! startup by issue #12, not here.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;

use crate::chain::ProviderId;
use crate::error::ConfigError;

// ---------------------------------------------------------------------------
// Ranges and defaults (`docs/07-configuration.md` §3/§4).
// ---------------------------------------------------------------------------

/// Minimum accepted chain-refresh cadence (`docs/07-configuration.md` §4).
pub const REFRESH_MIN: Duration = Duration::from_millis(250);
/// Maximum accepted chain-refresh cadence (`docs/07-configuration.md` §4).
pub const REFRESH_MAX: Duration = Duration::from_secs(300);
/// Minimum accepted UI tick cadence (`docs/07-configuration.md` §4).
pub const TICK_MIN: Duration = Duration::from_millis(50);
/// Maximum accepted UI tick cadence (`docs/07-configuration.md` §4).
pub const TICK_MAX: Duration = Duration::from_secs(5);
/// Minimum accepted bounded-channel capacity, in messages
/// (`docs/07-configuration.md` §4).
pub const CHANNEL_CAP_MIN: i64 = 64;
/// Maximum accepted bounded-channel capacity, in messages
/// (`docs/07-configuration.md` §4).
pub const CHANNEL_CAP_MAX: i64 = 65_536;

/// Default chain-refresh cadence when unset (`2s`).
const DEFAULT_REFRESH: Duration = Duration::from_secs(2);
/// Default UI tick cadence when unset (`250ms`).
const DEFAULT_TICK: Duration = Duration::from_millis(250);
/// Default bounded-channel capacity when unset (`1024` messages).
const DEFAULT_CHANNEL_CAP: usize = 1024;
/// Zero-config default underlying ticker (`BTC`, ADR-0003).
const DEFAULT_UNDERLYING: &str = "BTC";
/// Zero-config default provider id (`deribit`, ADR-0003).
const DEFAULT_PROVIDER: &str = "deribit";

/// The credential field names a provider may declare (per its `AuthKind`).
/// Used to reject a credential key that appears in a config file — the values
/// themselves come from the environment only.
const CREDENTIAL_KEYS: [&str; 5] = ["token", "username", "password", "api_key", "secret"];

// Environment-variable names for the global settings (§4). Per-provider keys
// are built with [`provider_env_var`].
const ENV_PROVIDER: &str = "CHAINVIEW_PROVIDER";
const ENV_UNDERLYING: &str = "CHAINVIEW_UNDERLYING";
const ENV_REFRESH: &str = "CHAINVIEW_REFRESH";
const ENV_TICK: &str = "CHAINVIEW_TICK";
const ENV_CHANNEL_CAP: &str = "CHAINVIEW_CHANNEL_CAP";
const ENV_LOG_FILE: &str = "CHAINVIEW_LOG_FILE";
const ENV_THEME: &str = "CHAINVIEW_THEME";
const ENV_NO_COLOR: &str = "NO_COLOR";

// ---------------------------------------------------------------------------
// Environment source (injectable so assembly stays pure and testable).
// ---------------------------------------------------------------------------

/// A read-only source of environment variables.
///
/// Production uses [`ProcessEnv`] (the real process environment); tests inject a
/// map. [`Config::assemble`] never reads `std::env` directly — it goes through
/// this trait — so config assembly is a pure function of its inputs and needs no
/// global mutation to test (which is `unsafe` on the 2024 edition and forbidden
/// here).
pub trait EnvSource {
    /// Look up a variable by its exact name; `None` when it is unset.
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment, backed by `std::env::var`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    #[inline]
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

// ---------------------------------------------------------------------------
// Secrets.
// ---------------------------------------------------------------------------

/// A resolved credential value, read from the environment.
///
/// Its `Debug` is **redacted** and it has no `Display`, so the raw value cannot
/// reach a log line, a rendered error, or `Debug` output. The value is
/// reachable only through [`Secret::expose`], at the single call site that hands
/// it to the upstream client — the credential guarantee (`docs/SECURITY.md` §1).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a credential value read from the environment.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw secret. Call only where the value is handed to an upstream
    /// client; never log, format, or render the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***redacted***)")
    }
}

// ---------------------------------------------------------------------------
// Provider-id ↔ environment-segment bijection (`docs/07-configuration.md` §5.1).
// ---------------------------------------------------------------------------

/// Transliterate a provider id into its shell-safe environment segment
/// (`docs/07-configuration.md` §5.1).
///
/// The mapping is `uppercase`, then `'_' → '__'` (escape existing underscores)
/// and `'-' → '_'` (hyphen becomes a single underscore), applied in one pass.
/// The output is always a legal POSIX variable segment (`[A-Z0-9_]`, starting
/// with a letter), so `CHAINVIEW_<SEG>_<KEY>` is always assignable in a shell.
///
/// | id          | segment      |
/// |-------------|--------------|
/// | `deribit`   | `DERIBIT`    |
/// | `my-broker` | `MY_BROKER`  |
/// | `my_broker` | `MY__BROKER` |
///
/// # Reversibility (a bijection only over the collision-free id space)
///
/// The map round-trips through [`decode_segment`] for every id whose separators
/// are **not adjacent** to each other (no `--`, `-_`, or `_-` substring) — which
/// covers `deribit`, `my-broker`, `my_broker`, `td-ameritrade`, and every
/// realistic id. It is **not** a total bijection over the full placeholder
/// grammar `^[a-z][a-z0-9_-]{1,31}$`: a per-character `'-' → '_'` and
/// `'_' → '__'` cannot be injective when hyphens are adjacent, because
/// `encode("a--") == encode("a_") == "A__"`. This is a defect in the documented
/// §5.1 scheme, not this implementation. **Hand-off:** issue #4 owns the
/// `ProviderId` grammar and must resolve it (tighten the grammar to forbid
/// adjacent separators, or revise the escape) under an ADR; the collision-free
/// contract above is what callers may rely on until then.
#[must_use]
pub fn encode_segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + 2);
    for c in id.chars() {
        match c {
            '_' => out.push_str("__"),
            '-' => out.push('_'),
            other => out.push(other.to_ascii_uppercase()),
        }
    }
    out
}

/// Recover a provider id from its environment segment — the inverse of
/// [`encode_segment`] over the collision-free id space (see that function's
/// reversibility note). Lowercase, then `'__' → '_'` and a remaining single
/// `'_' → '-'`. For diagnostics, this reports which id a `CHAINVIEW_<SEG>_*`
/// variable belongs to.
#[must_use]
pub fn decode_segment(seg: &str) -> String {
    let lower = seg.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                // Escaped underscore: consume the pair, emit one underscore.
                let _ = chars.next();
                out.push('_');
            } else {
                out.push('-');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Build the `CHAINVIEW_<SEG>_<KEY>` variable name for a provider credential or
/// setting, where `<SEG>` is [`encode_segment`] of `id` and `<KEY>` is the
/// upper-cased field name.
#[must_use]
pub fn provider_env_var(id: &str, key: &str) -> String {
    format!(
        "CHAINVIEW_{}_{}",
        encode_segment(id),
        key.to_ascii_uppercase()
    )
}

/// Read a provider's required credentials from the environment only.
///
/// Each key resolves `CHAINVIEW_<SEG>_<KEY>`; a missing (or empty) required key
/// is [`ConfigError::MissingCredential`] naming the **provider**, never the key.
/// The returned values are wrapped in [`Secret`].
///
/// # Errors
///
/// [`ConfigError::MissingCredential`] when any requested key is absent or empty.
pub fn require_credentials(
    env: &dyn EnvSource,
    provider: &ProviderId,
    keys: &[&str],
) -> Result<BTreeMap<String, Secret>, ConfigError> {
    let mut out = BTreeMap::new();
    for key in keys {
        let var = provider_env_var(provider.as_str(), key);
        match env.get(&var).filter(|v| !v.is_empty()) {
            Some(value) => {
                let _ = out.insert((*key).to_ascii_uppercase(), Secret::new(value));
            }
            None => return Err(ConfigError::MissingCredential(provider.clone())),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Public config enums.
// ---------------------------------------------------------------------------

/// The color theme selection (`docs/07-configuration.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ThemeChoice {
    /// Detect a dark or light terminal automatically (the default).
    #[default]
    Auto,
    /// Force the dark theme.
    Dark,
    /// Force the light theme.
    Light,
}

impl FromStr for ThemeChoice {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            other => Err(ConfigError::InvalidValue {
                field: "theme".to_owned(),
                reason: format!("must be one of auto|dark|light, got `{other}`"),
            }),
        }
    }
}

/// The selected run mode. `Live` is the default; `Replay` is entered by the
/// `chainview replay <dir>` subcommand (`docs/07-configuration.md` §4.1), never
/// a `--replay` flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModeSelect {
    /// Live mode: stream a real-time option chain from the selected provider.
    #[default]
    Live,
    /// Replay mode: render the IronCondor result bundle at the given directory,
    /// read-only and with no network.
    Replay(PathBuf),
}

/// Per-provider, non-secret settings. Credentials are **never** here — they come
/// from the environment (`docs/07-configuration.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderSettings {
    /// Override the upstream base URL where meaningful (absolute URL).
    pub endpoint: Option<String>,
    /// Per-provider override of the global refresh cadence.
    pub refresh_interval: Option<Duration>,
}

/// The assembled, validated, immutable configuration for the process
/// (`docs/07-configuration.md` §3). Built once at startup by [`Config::load`];
/// a bad value is a typed [`ConfigError`], never a runtime surprise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The selected market-data provider id.
    pub provider: ProviderId,
    /// The underlying ticker, upper-cased.
    pub underlying: String,
    /// Chain-refresh cadence; within `[250ms, 300s]`.
    pub refresh_interval: Duration,
    /// UI tick cadence; within `[50ms, 5s]`.
    pub tick_interval: Duration,
    /// Bounded-channel capacity in messages; within `[64, 65536]`.
    pub channel_capacity: usize,
    /// Optional log-file sink; never stdout/stderr while the TUI runs.
    pub log_file: Option<PathBuf>,
    /// The color theme selection.
    pub theme: ThemeChoice,
    /// Whether color is disabled (from `--no-color`, `NO_COLOR`, or the file).
    pub no_color: bool,
    /// Per-provider non-secret settings, keyed by provider id.
    pub providers: BTreeMap<ProviderId, ProviderSettings>,
    /// The selected run mode (`Live` by default, `Replay` via the subcommand).
    pub mode: ModeSelect,
}

/// The overrides parsed from the CLI in `src/main.rs`, handed to
/// [`Config::assemble`]. Kept free of `clap` types so config assembly is
/// testable without the parser. In `Replay` mode the live-only fields
/// (`provider`, `underlying`, `refresh_interval`, `endpoint`) are ignored
/// (`docs/07-configuration.md` §4.1).
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--provider <id>` (live-only).
    pub provider: Option<String>,
    /// `--underlying <sym>` (live-only).
    pub underlying: Option<String>,
    /// `--refresh <dur>` (live-only).
    pub refresh_interval: Option<String>,
    /// `--tick <dur>`.
    pub tick_interval: Option<String>,
    /// `--channel-cap <n>`.
    pub channel_capacity: Option<i64>,
    /// `--log-file <path>`.
    pub log_file: Option<PathBuf>,
    /// `--theme <auto|dark|light>`.
    pub theme: Option<String>,
    /// `--no-color` (presence).
    pub no_color: bool,
    /// `--endpoint <url>` for the selected provider (live-only).
    pub endpoint: Option<String>,
    /// The mode selected by the (absent) subcommand.
    pub mode: ModeSelect,
}

// ---------------------------------------------------------------------------
// File deserialization (typo-protected; credential keys rejected).
// ---------------------------------------------------------------------------

/// The optional TOML config file, strictly deserialized. `deny_unknown_fields`
/// turns a misspelled key (`refresh_intervall`) into a typed error rather than a
/// silent default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    provider: Option<String>,
    underlying: Option<String>,
    refresh_interval: Option<String>,
    tick_interval: Option<String>,
    channel_capacity: Option<i64>,
    log_file: Option<PathBuf>,
    theme: Option<String>,
    no_color: Option<bool>,
    providers: Option<BTreeMap<String, RawProviderSettings>>,
}

/// A per-provider settings block in the config file. `deny_unknown_fields`
/// rejects any key other than `endpoint`/`refresh_interval` — including a
/// credential key, which is caught earlier with a clearer message.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviderSettings {
    endpoint: Option<String>,
    refresh_interval: Option<String>,
}

// ---------------------------------------------------------------------------
// Validators.
// ---------------------------------------------------------------------------

/// True when `key` is a well-known credential field name (case-insensitive).
fn is_credential_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    CREDENTIAL_KEYS.contains(&lower.as_str())
}

/// Build the "credential in a file" rejection, naming the key (not a value).
#[cold]
fn credential_in_file_err(key: &str) -> ConfigError {
    ConfigError::InvalidValue {
        field: "config file".to_owned(),
        reason: format!(
            "credential key `{key}` must be set via the environment \
             (CHAINVIEW_<ID>_*), never a config file"
        ),
    }
}

/// Reject any credential key present at the top level or under a
/// `providers.<id>` table before typed deserialization.
fn reject_file_credentials(table: &toml::Table) -> Result<(), ConfigError> {
    for key in table.keys() {
        if is_credential_key(key) {
            return Err(credential_in_file_err(key));
        }
    }
    if let Some(toml::Value::Table(providers)) = table.get("providers") {
        for settings in providers.values() {
            if let toml::Value::Table(inner) = settings {
                for key in inner.keys() {
                    if is_credential_key(key) {
                        return Err(credential_in_file_err(key));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Parse and validate the config-file contents into a [`RawFile`].
fn parse_file(contents: &str) -> Result<RawFile, ConfigError> {
    let table: toml::Table =
        contents
            .parse::<toml::Table>()
            .map_err(|e| ConfigError::InvalidValue {
                field: "config file".to_owned(),
                reason: format!("invalid TOML: {e}"),
            })?;
    // Reject credential keys first, so their clean message wins over the generic
    // `deny_unknown_fields` error from the typed parse below.
    reject_file_credentials(&table)?;
    let raw: RawFile = toml::from_str(contents).map_err(|e| ConfigError::InvalidValue {
        field: "config file".to_owned(),
        reason: e.message().to_owned(),
    })?;
    Ok(raw)
}

/// Validate the provider-id grammar `^[a-z][a-z0-9_-]{1,31}$` locally and wrap
/// it into a [`ProviderId`]. Issue #4 moves this into `ProviderId::new`.
///
/// # Errors
///
/// [`ConfigError::InvalidValue`] with `field: "provider id"` for a malformed id.
#[must_use = "the validated provider id must be used"]
pub fn validate_provider_id(id: &str) -> Result<ProviderId, ConfigError> {
    if is_valid_provider_id(id) {
        Ok(ProviderId::new(id))
    } else {
        Err(ConfigError::InvalidValue {
            field: "provider id".to_owned(),
            reason: format!("`{id}` must match ^[a-z][a-z0-9_-]{{1,31}}$"),
        })
    }
}

/// The `^[a-z][a-z0-9_-]{1,31}$` grammar check: 2–32 chars, first a lowercase
/// letter, the rest lowercase letters, digits, `-`, or `_`.
fn is_valid_provider_id(id: &str) -> bool {
    let len = id.chars().count();
    if !(2..=32).contains(&len) {
        return false;
    }
    let mut chars = id.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase());
    first_ok && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Validate the underlying: non-empty after trimming, then upper-cased.
fn validate_underlying(raw: &str) -> Result<String, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidValue {
            field: "underlying".to_owned(),
            reason: "must be non-empty".to_owned(),
        });
    }
    Ok(trimmed.to_ascii_uppercase())
}

/// Parse a `humantime` duration and check it is within `[min, max]`.
fn parse_duration_in_range(
    raw: &str,
    field: &str,
    min: Duration,
    max: Duration,
) -> Result<Duration, ConfigError> {
    let value = humantime::parse_duration(raw.trim()).map_err(|_| ConfigError::InvalidValue {
        field: field.to_owned(),
        reason: format!("`{raw}` is not a duration (use 250ms, 2s, 5m)"),
    })?;
    if value < min || value > max {
        return Err(ConfigError::InvalidValue {
            field: field.to_owned(),
            reason: format!(
                "must be within [{}, {}]",
                humantime::format_duration(min),
                humantime::format_duration(max)
            ),
        });
    }
    Ok(value)
}

/// Parse an integer, reporting a typed error rather than a panic.
fn parse_i64(raw: &str, field: &str) -> Result<i64, ConfigError> {
    raw.trim()
        .parse::<i64>()
        .map_err(|_| ConfigError::InvalidValue {
            field: field.to_owned(),
            reason: format!("`{raw}` is not an integer"),
        })
}

/// Validate the channel capacity is within `[64, 65536]`.
fn validate_channel_capacity(n: i64) -> Result<usize, ConfigError> {
    if !(CHANNEL_CAP_MIN..=CHANNEL_CAP_MAX).contains(&n) {
        return Err(ConfigError::InvalidValue {
            field: "channel_capacity".to_owned(),
            reason: format!("must be within [{CHANNEL_CAP_MIN}, {CHANNEL_CAP_MAX}], got {n}"),
        });
    }
    usize::try_from(n).map_err(|_| ConfigError::InvalidValue {
        field: "channel_capacity".to_owned(),
        reason: "capacity out of range".to_owned(),
    })
}

/// Validate a log-file path: non-empty and never a stdout/stderr sink. Actual
/// parent-directory writability is checked when the sink is created (issues
/// #008/#011), not here.
fn validate_log_file(path: PathBuf) -> Result<PathBuf, ConfigError> {
    let shown = path.to_string_lossy();
    if shown.is_empty() {
        return Err(ConfigError::InvalidValue {
            field: "log_file".to_owned(),
            reason: "must be a non-empty path".to_owned(),
        });
    }
    if matches!(
        shown.as_ref(),
        "-" | "/dev/stdout" | "/dev/stderr" | "/dev/fd/1" | "/dev/fd/2"
    ) {
        return Err(ConfigError::InvalidValue {
            field: "log_file".to_owned(),
            reason: "must be a file path, never stdout/stderr".to_owned(),
        });
    }
    Ok(path)
}

/// Validate a minimally-absolute URL (`scheme://authority`), without a URL
/// dependency. Rejects a relative or scheme-less value.
fn validate_endpoint(raw: &str) -> Result<String, ConfigError> {
    let ok = match raw.split_once("://") {
        Some((scheme, rest)) => {
            !rest.is_empty()
                && matches!(scheme.chars().next(), Some(c) if c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    };
    if ok {
        Ok(raw.to_owned())
    } else {
        Err(ConfigError::InvalidValue {
            field: "endpoint".to_owned(),
            reason: "must be an absolute URL (scheme://host)".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Providers-map assembly.
// ---------------------------------------------------------------------------

/// Build the per-provider settings map: the file entries, then the selected
/// provider's env/CLI overrides layered on with `CLI > env > file` precedence.
fn assemble_providers(
    file_providers: BTreeMap<String, RawProviderSettings>,
    selected: &ProviderId,
    cli_endpoint: Option<String>,
    env: &dyn EnvSource,
) -> Result<BTreeMap<ProviderId, ProviderSettings>, ConfigError> {
    let mut out = BTreeMap::new();
    for (raw_id, raw) in file_providers {
        let id = validate_provider_id(&raw_id)?;
        let endpoint = match raw.endpoint {
            Some(e) => Some(validate_endpoint(&e)?),
            None => None,
        };
        let refresh_interval = match raw.refresh_interval {
            Some(d) => Some(parse_duration_in_range(
                &d,
                "providers.<id>.refresh_interval",
                REFRESH_MIN,
                REFRESH_MAX,
            )?),
            None => None,
        };
        let _ = out.insert(
            id,
            ProviderSettings {
                endpoint,
                refresh_interval,
            },
        );
    }

    let seg = encode_segment(selected.as_str());
    let env_endpoint = env.get(&format!("CHAINVIEW_{seg}_ENDPOINT"));
    let env_refresh = env.get(&format!("CHAINVIEW_{seg}_REFRESH"));
    let endpoint_override = cli_endpoint.or(env_endpoint);

    if endpoint_override.is_some() || env_refresh.is_some() || out.contains_key(selected) {
        let entry = out.entry(selected.clone()).or_default();
        if let Some(e) = endpoint_override {
            entry.endpoint = Some(validate_endpoint(&e)?);
        }
        if let Some(d) = env_refresh {
            entry.refresh_interval = Some(parse_duration_in_range(
                &d,
                "providers.<id>.refresh_interval",
                REFRESH_MIN,
                REFRESH_MAX,
            )?);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Assembly and loading.
// ---------------------------------------------------------------------------

impl Config {
    /// Assemble the config from its three sources, applying `CLI > env > file >
    /// default` precedence and validating every value. Pure — it performs no
    /// I/O and reads the environment only through `env`.
    ///
    /// `file_contents` is the raw text of the optional config file (already read
    /// from disk by the caller), or `None` when there is no file.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for any malformed, out-of-range, unknown, or
    /// credential-bearing value.
    pub fn assemble(
        cli: CliOverrides,
        env: &dyn EnvSource,
        file_contents: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let file = match file_contents {
            Some(contents) => parse_file(contents)?,
            None => RawFile::default(),
        };

        // In replay mode the live-only flags are ignored (§4.1).
        let replay = matches!(cli.mode, ModeSelect::Replay(_));
        let cli_provider = if replay { None } else { cli.provider.clone() };
        let cli_underlying = if replay { None } else { cli.underlying.clone() };
        let cli_refresh = if replay {
            None
        } else {
            cli.refresh_interval.clone()
        };
        let cli_endpoint = if replay { None } else { cli.endpoint.clone() };

        // Provider.
        let provider_raw = cli_provider
            .or_else(|| env.get(ENV_PROVIDER))
            .or_else(|| file.provider.clone());
        let provider = match provider_raw {
            Some(s) => validate_provider_id(&s)?,
            None => ProviderId::new(DEFAULT_PROVIDER),
        };

        // Underlying.
        let underlying_raw = cli_underlying
            .or_else(|| env.get(ENV_UNDERLYING))
            .or_else(|| file.underlying.clone())
            .unwrap_or_else(|| DEFAULT_UNDERLYING.to_owned());
        let underlying = validate_underlying(&underlying_raw)?;

        // Refresh interval.
        let refresh_raw = cli_refresh
            .or_else(|| env.get(ENV_REFRESH))
            .or_else(|| file.refresh_interval.clone());
        let refresh_interval = match refresh_raw {
            Some(s) => parse_duration_in_range(&s, "refresh_interval", REFRESH_MIN, REFRESH_MAX)?,
            None => DEFAULT_REFRESH,
        };

        // Tick interval.
        let tick_raw = cli
            .tick_interval
            .clone()
            .or_else(|| env.get(ENV_TICK))
            .or_else(|| file.tick_interval.clone());
        let tick_interval = match tick_raw {
            Some(s) => parse_duration_in_range(&s, "tick_interval", TICK_MIN, TICK_MAX)?,
            None => DEFAULT_TICK,
        };

        // Channel capacity (CLI > env > file > default).
        let cap_raw: Option<i64> = match cli.channel_capacity {
            Some(n) => Some(n),
            None => match env.get(ENV_CHANNEL_CAP) {
                Some(s) => Some(parse_i64(&s, "channel_capacity")?),
                None => file.channel_capacity,
            },
        };
        let channel_capacity = match cap_raw {
            Some(n) => validate_channel_capacity(n)?,
            None => DEFAULT_CHANNEL_CAP,
        };

        // Log file.
        let log_raw = cli
            .log_file
            .clone()
            .or_else(|| env.get(ENV_LOG_FILE).map(PathBuf::from))
            .or_else(|| file.log_file.clone());
        let log_file = match log_raw {
            Some(p) => Some(validate_log_file(p)?),
            None => None,
        };

        // Theme.
        let theme_raw = cli
            .theme
            .clone()
            .or_else(|| env.get(ENV_THEME))
            .or_else(|| file.theme.clone());
        let theme = match theme_raw {
            Some(s) => s.parse::<ThemeChoice>()?,
            None => ThemeChoice::default(),
        };

        // No-color: CLI flag, else NO_COLOR presence, else file, else off.
        let no_color = cli.no_color
            || env.get(ENV_NO_COLOR).is_some_and(|v| !v.is_empty())
            || file.no_color.unwrap_or(false);

        // Per-provider settings.
        let providers = assemble_providers(
            file.providers.unwrap_or_default(),
            &provider,
            cli_endpoint,
            env,
        )?;

        Ok(Self {
            provider,
            underlying,
            refresh_interval,
            tick_interval,
            channel_capacity,
            log_file,
            theme,
            no_color,
            providers,
            mode: cli.mode,
        })
    }

    /// Load the config at startup: read the process environment and the optional
    /// XDG-resolved config file, then [`assemble`](Config::assemble). This is the
    /// only method that performs I/O.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for an unreadable file or any invalid value.
    pub fn load(cli: CliOverrides) -> Result<Self, ConfigError> {
        let env = ProcessEnv;
        let contents = read_config_file(&env)?;
        Self::assemble(cli, &env, contents.as_deref())
    }
}

/// Resolve the config-file path, XDG-aware: `$XDG_CONFIG_HOME/chainview/
/// config.toml` when `XDG_CONFIG_HOME` is set and non-empty, else
/// `$HOME/.config/chainview/config.toml`, else `None`.
#[must_use]
pub fn config_file_path(env: &dyn EnvSource) -> Option<PathBuf> {
    if let Some(xdg) = env.get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("chainview").join("config.toml"));
    }
    let home = env.get("HOME").filter(|s| !s.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("chainview")
            .join("config.toml"),
    )
}

/// Read the config file if it exists. An absent file is `Ok(None)` (the
/// zero-config path); an unreadable one is a typed error that never leaks the
/// path.
fn read_config_file(env: &dyn EnvSource) -> Result<Option<String>, ConfigError> {
    let Some(path) = config_file_path(env) else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ConfigError::InvalidValue {
            field: "config file".to_owned(),
            reason: "config file exists but could not be read".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map-backed [`EnvSource`] for deterministic tests — the process
    /// environment is never mutated (which is `unsafe` on the 2024 edition).
    struct MapEnv(BTreeMap<String, String>);

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn empty_env() -> MapEnv {
        MapEnv(BTreeMap::new())
    }

    #[track_caller]
    fn assembled(cli: CliOverrides, env: &dyn EnvSource, file: Option<&str>) -> Config {
        match Config::assemble(cli, env, file) {
            Ok(config) => config,
            Err(e) => panic!("expected a valid Config, got error: {e}"),
        }
    }

    fn is_invalid_value(result: &Result<Config, ConfigError>) -> bool {
        matches!(result, Err(ConfigError::InvalidValue { .. }))
    }

    // --- Zero-config default -------------------------------------------------

    #[test]
    fn test_config_zero_config_resolves_deribit_btc() {
        let config = assembled(CliOverrides::default(), &empty_env(), None);
        assert_eq!(config.provider.as_str(), "deribit");
        assert_eq!(config.underlying, "BTC");
        assert_eq!(config.refresh_interval, Duration::from_secs(2));
        assert_eq!(config.tick_interval, Duration::from_millis(250));
        assert_eq!(config.channel_capacity, 1024);
        assert_eq!(config.theme, ThemeChoice::Auto);
        assert!(!config.no_color);
        assert_eq!(config.mode, ModeSelect::Live);
        assert!(config.log_file.is_none());
        assert!(config.providers.is_empty());
    }

    // --- Precedence (CLI > env > file > default) -----------------------------

    #[test]
    fn test_config_provider_precedence_cli_over_env_over_file() {
        let cli = CliOverrides {
            provider: Some("alpaca".to_owned()),
            ..Default::default()
        };
        let env = env(&[("CHAINVIEW_PROVIDER", "ig")]);
        let file = "provider = \"deribit\"\n";
        let config = assembled(cli, &env, Some(file));
        assert_eq!(config.provider.as_str(), "alpaca");
    }

    #[test]
    fn test_config_provider_precedence_env_over_file() {
        let env = env(&[("CHAINVIEW_PROVIDER", "ig")]);
        let file = "provider = \"deribit\"\n";
        let config = assembled(CliOverrides::default(), &env, Some(file));
        assert_eq!(config.provider.as_str(), "ig");
    }

    #[test]
    fn test_config_provider_from_file_when_no_higher_source() {
        let file = "provider = \"tastytrade\"\n";
        let config = assembled(CliOverrides::default(), &empty_env(), Some(file));
        assert_eq!(config.provider.as_str(), "tastytrade");
    }

    #[test]
    fn test_config_underlying_precedence_and_uppercased() {
        let cli = CliOverrides {
            underlying: Some("eth".to_owned()),
            ..Default::default()
        };
        let env = env(&[("CHAINVIEW_UNDERLYING", "sol")]);
        let config = assembled(cli, &env, Some("underlying = \"btc\"\n"));
        assert_eq!(config.underlying, "ETH");
    }

    #[test]
    fn test_config_refresh_precedence_cli_over_env_over_file() {
        let cli = CliOverrides {
            refresh_interval: Some("3s".to_owned()),
            ..Default::default()
        };
        let env = env(&[("CHAINVIEW_REFRESH", "10s")]);
        let config = assembled(cli, &env, Some("refresh_interval = \"30s\"\n"));
        assert_eq!(config.refresh_interval, Duration::from_secs(3));
    }

    #[test]
    fn test_config_tick_precedence_env_over_file() {
        let env = env(&[("CHAINVIEW_TICK", "500ms")]);
        let config = assembled(
            CliOverrides::default(),
            &env,
            Some("tick_interval = \"1s\"\n"),
        );
        assert_eq!(config.tick_interval, Duration::from_millis(500));
    }

    #[test]
    fn test_config_channel_capacity_precedence_cli_over_env() {
        let cli = CliOverrides {
            channel_capacity: Some(2048),
            ..Default::default()
        };
        let env = env(&[("CHAINVIEW_CHANNEL_CAP", "4096")]);
        let config = assembled(cli, &env, None);
        assert_eq!(config.channel_capacity, 2048);
    }

    #[test]
    fn test_config_channel_capacity_from_file() {
        let config = assembled(
            CliOverrides::default(),
            &empty_env(),
            Some("channel_capacity = 256\n"),
        );
        assert_eq!(config.channel_capacity, 256);
    }

    #[test]
    fn test_config_theme_precedence_env_over_file() {
        let env = env(&[("CHAINVIEW_THEME", "dark")]);
        let config = assembled(CliOverrides::default(), &env, Some("theme = \"light\"\n"));
        assert_eq!(config.theme, ThemeChoice::Dark);
    }

    #[test]
    fn test_config_no_color_from_env_presence() {
        let env = env(&[("NO_COLOR", "1")]);
        let config = assembled(CliOverrides::default(), &env, None);
        assert!(config.no_color);
    }

    #[test]
    fn test_config_no_color_empty_env_value_does_not_enable() {
        let env = env(&[("NO_COLOR", "")]);
        let config = assembled(CliOverrides::default(), &env, None);
        assert!(!config.no_color);
    }

    #[test]
    fn test_config_no_color_cli_flag_enables() {
        let cli = CliOverrides {
            no_color: true,
            ..Default::default()
        };
        let config = assembled(cli, &empty_env(), None);
        assert!(config.no_color);
    }

    #[test]
    fn test_config_log_file_precedence_cli_over_env() {
        let cli = CliOverrides {
            log_file: Some(PathBuf::from("/tmp/cli.log")),
            ..Default::default()
        };
        let env = env(&[("CHAINVIEW_LOG_FILE", "/tmp/env.log")]);
        let config = assembled(cli, &env, None);
        assert_eq!(config.log_file, Some(PathBuf::from("/tmp/cli.log")));
    }

    // --- Range rejections ----------------------------------------------------

    #[test]
    fn test_config_refresh_below_min_rejected() {
        let cli = CliOverrides {
            refresh_interval: Some("100ms".to_owned()),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_refresh_above_max_rejected() {
        let cli = CliOverrides {
            refresh_interval: Some("301s".to_owned()),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_tick_out_of_range_rejected() {
        let cli = CliOverrides {
            tick_interval: Some("10s".to_owned()),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_channel_capacity_out_of_range_rejected() {
        let cli = CliOverrides {
            channel_capacity: Some(10),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_channel_capacity_negative_rejected() {
        let cli = CliOverrides {
            channel_capacity: Some(-5),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_invalid_duration_string_rejected() {
        let cli = CliOverrides {
            refresh_interval: Some("soon".to_owned()),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_channel_capacity_bounds_inclusive() {
        for cap in [CHANNEL_CAP_MIN, CHANNEL_CAP_MAX] {
            let cli = CliOverrides {
                channel_capacity: Some(cap),
                ..Default::default()
            };
            let config = assembled(cli, &empty_env(), None);
            assert_eq!(config.channel_capacity as i64, cap);
        }
    }

    // --- Unknown / misspelled file key --------------------------------------

    #[test]
    fn test_config_rejects_unknown_file_key() {
        let file = "refresh_intervall = \"2s\"\n";
        let result = Config::assemble(CliOverrides::default(), &empty_env(), Some(file));
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_rejects_unknown_provider_settings_key() {
        let file = "[providers.deribit]\nunexpected = \"x\"\n";
        let result = Config::assemble(CliOverrides::default(), &empty_env(), Some(file));
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_rejects_malformed_toml() {
        let result = Config::assemble(
            CliOverrides::default(),
            &empty_env(),
            Some("not = valid = toml"),
        );
        assert!(is_invalid_value(&result));
    }

    // --- Secret-from-file rejection -----------------------------------------

    #[test]
    fn test_config_rejects_credential_key_at_top_level() {
        let file = "password = \"hunter2\"\n";
        let result = Config::assemble(CliOverrides::default(), &empty_env(), Some(file));
        match result {
            Err(ConfigError::InvalidValue { field, reason }) => {
                assert_eq!(field, "config file");
                assert!(reason.contains("password"));
                assert!(!reason.contains("hunter2"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn test_config_rejects_credential_key_in_provider_table() {
        let file = "[providers.ig]\napi_key = \"topsecret-value\"\n";
        let result = Config::assemble(CliOverrides::default(), &empty_env(), Some(file));
        match result {
            Err(ConfigError::InvalidValue { field, reason }) => {
                assert_eq!(field, "config file");
                assert!(reason.contains("api_key"));
                assert!(!reason.contains("topsecret-value"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    // --- Provider grammar (hand-off to #4) ----------------------------------

    #[test]
    fn test_config_rejects_syntactically_invalid_provider_id() {
        let cli = CliOverrides {
            provider: Some("Deribit".to_owned()),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        match result {
            Err(ConfigError::InvalidValue { field, .. }) => assert_eq!(field, "provider id"),
            other => panic!("expected InvalidValue on provider id, got {other:?}"),
        }
    }

    #[test]
    fn test_config_accepts_valid_custom_provider_id() {
        let cli = CliOverrides {
            provider: Some("my-broker".to_owned()),
            ..Default::default()
        };
        let config = assembled(cli, &empty_env(), None);
        assert_eq!(config.provider.as_str(), "my-broker");
    }

    #[test]
    fn test_validate_provider_id_rejects_leading_digit() {
        assert!(validate_provider_id("1broker").is_err());
    }

    #[test]
    fn test_validate_provider_id_rejects_too_short() {
        assert!(validate_provider_id("a").is_err());
    }

    // --- Replay subcommand ---------------------------------------------------

    #[test]
    fn test_config_replay_subcommand_selects_replay_mode() {
        let cli = CliOverrides {
            mode: ModeSelect::Replay(PathBuf::from("./run-2026-07-01/")),
            ..Default::default()
        };
        let config = assembled(cli, &empty_env(), None);
        assert_eq!(
            config.mode,
            ModeSelect::Replay(PathBuf::from("./run-2026-07-01/"))
        );
    }

    #[test]
    fn test_config_replay_ignores_live_only_flags() {
        let cli = CliOverrides {
            provider: Some("alpaca".to_owned()),
            underlying: Some("spy".to_owned()),
            refresh_interval: Some("5s".to_owned()),
            endpoint: Some("https://example.test".to_owned()),
            mode: ModeSelect::Replay(PathBuf::from("./bundle/")),
            ..Default::default()
        };
        let config = assembled(cli, &empty_env(), None);
        // Live-only flags are ignored, so defaults hold.
        assert_eq!(config.provider.as_str(), "deribit");
        assert_eq!(config.underlying, "BTC");
        assert_eq!(config.refresh_interval, Duration::from_secs(2));
        assert!(config.providers.is_empty());
    }

    // --- Per-provider endpoint ----------------------------------------------

    #[test]
    fn test_config_endpoint_precedence_cli_over_env_over_file() {
        let cli = CliOverrides {
            provider: Some("deribit".to_owned()),
            endpoint: Some("https://cli.example".to_owned()),
            ..Default::default()
        };
        let env = env(&[("CHAINVIEW_DERIBIT_ENDPOINT", "https://env.example")]);
        let file = "[providers.deribit]\nendpoint = \"https://file.example\"\n";
        let config = assembled(cli, &env, Some(file));
        let settings = match config.providers.get(&ProviderId::new("deribit")) {
            Some(s) => s,
            None => panic!("expected deribit provider settings"),
        };
        assert_eq!(settings.endpoint.as_deref(), Some("https://cli.example"));
    }

    #[test]
    fn test_config_rejects_relative_endpoint() {
        let cli = CliOverrides {
            endpoint: Some("example.com/api".to_owned()),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }

    #[test]
    fn test_config_per_provider_env_refresh_override() {
        let env = env(&[("CHAINVIEW_DERIBIT_REFRESH", "45s")]);
        let config = assembled(CliOverrides::default(), &env, None);
        let settings = match config.providers.get(&ProviderId::new("deribit")) {
            Some(s) => s,
            None => panic!("expected deribit provider settings"),
        };
        assert_eq!(settings.refresh_interval, Some(Duration::from_secs(45)));
    }

    // --- Bijection -----------------------------------------------------------

    #[test]
    fn test_encode_segment_maps_hyphen_and_underscore() {
        assert_eq!(encode_segment("deribit"), "DERIBIT");
        assert_eq!(encode_segment("my-broker"), "MY_BROKER");
        assert_eq!(encode_segment("my_broker"), "MY__BROKER");
    }

    #[test]
    fn test_decode_segment_inverts_encode() {
        assert_eq!(decode_segment("DERIBIT"), "deribit");
        assert_eq!(decode_segment("MY_BROKER"), "my-broker");
        assert_eq!(decode_segment("MY__BROKER"), "my_broker");
    }

    #[test]
    fn test_encode_decode_no_collision_between_hyphen_and_underscore() {
        // The two ids that a naive uppercase would collide are distinct segments.
        assert_ne!(encode_segment("my-broker"), encode_segment("my_broker"));
    }

    #[test]
    fn test_encode_segment_known_limitation_adjacent_hyphens_pending_issue_4() {
        // Documented defect in docs/07 §5.1: the per-character `'-' → '_'` /
        // `'_' → '__'` scheme is NOT injective when hyphens are adjacent, so the
        // round-trip does not hold there. This test pins the known limitation so
        // any future change to the scheme (issue #4, under an ADR) must revisit
        // it. Realistic ids with isolated separators are unaffected.
        assert_eq!(encode_segment("a--"), encode_segment("a_"));
        assert_ne!(decode_segment(&encode_segment("a--")), "a--");
    }

    #[test]
    fn test_provider_env_var_builds_shell_safe_name() {
        assert_eq!(provider_env_var("ig", "username"), "CHAINVIEW_IG_USERNAME");
        assert_eq!(
            provider_env_var("my-broker", "token"),
            "CHAINVIEW_MY_BROKER_TOKEN"
        );
    }

    // --- Credentials ---------------------------------------------------------

    #[test]
    fn test_require_credentials_present_reads_from_env() {
        let env = env(&[
            ("CHAINVIEW_IG_USERNAME", "alice"),
            ("CHAINVIEW_IG_PASSWORD", "correct horse"),
            ("CHAINVIEW_IG_API_KEY", "key-123"),
        ]);
        let provider = ProviderId::new("ig");
        let result = require_credentials(&env, &provider, &["username", "password", "api_key"]);
        match result {
            Ok(map) => {
                assert_eq!(map.get("USERNAME").map(Secret::expose), Some("alice"));
                assert_eq!(
                    map.get("PASSWORD").map(Secret::expose),
                    Some("correct horse")
                );
                assert_eq!(map.get("API_KEY").map(Secret::expose), Some("key-123"));
            }
            Err(e) => panic!("expected credentials, got {e}"),
        }
    }

    #[test]
    fn test_require_credentials_missing_returns_missing_credential() {
        let env = env(&[("CHAINVIEW_IG_USERNAME", "alice")]);
        let provider = ProviderId::new("ig");
        let result = require_credentials(&env, &provider, &["username", "password"]);
        match result {
            Err(ConfigError::MissingCredential(p)) => assert_eq!(p.as_str(), "ig"),
            other => panic!("expected MissingCredential, got {other:?}"),
        }
    }

    #[test]
    fn test_require_credentials_empty_value_treated_as_missing() {
        let env = env(&[("CHAINVIEW_IG_PASSWORD", "")]);
        let provider = ProviderId::new("ig");
        let result = require_credentials(&env, &provider, &["password"]);
        assert!(matches!(result, Err(ConfigError::MissingCredential(_))));
    }

    // --- Security: no secret in Debug / error / Config -----------------------

    #[test]
    fn test_secret_debug_is_redacted() {
        let secret = Secret::new("super-secret-value");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "Secret(***redacted***)");
        assert!(!rendered.contains("super-secret-value"));
    }

    #[test]
    fn test_no_credential_value_leaks_when_present() {
        // A credential is present in the environment; assemble a full Config and
        // resolve the credential, then assert the secret string appears in no
        // Debug output and in no ConfigError produced on the credential path.
        const SECRET_VALUE: &str = "tRoub4dor&3-do-not-log";
        let env = env(&[
            ("CHAINVIEW_PROVIDER", "ig"),
            ("CHAINVIEW_IG_PASSWORD", SECRET_VALUE),
        ]);
        let config = assembled(CliOverrides::default(), &env, None);

        // The assembled Config never stores or renders the secret.
        assert!(!format!("{config:?}").contains(SECRET_VALUE));

        // The resolved secret map redacts the value in Debug.
        let provider = ProviderId::new("ig");
        match require_credentials(&env, &provider, &["password"]) {
            Ok(map) => {
                assert!(!format!("{map:?}").contains(SECRET_VALUE));
                // The value is still recoverable for the upstream client.
                assert_eq!(map.get("PASSWORD").map(Secret::expose), Some(SECRET_VALUE));
            }
            Err(e) => panic!("expected credentials, got {e}"),
        }

        // A MissingCredential error names the provider, never a secret.
        let missing = require_credentials(&env, &provider, &["username"]);
        match missing {
            Err(e) => {
                let rendered = e.to_string();
                assert!(rendered.contains("ig"));
                assert!(!rendered.contains(SECRET_VALUE));
                assert!(!format!("{e:?}").contains(SECRET_VALUE));
            }
            Ok(_) => panic!("expected MissingCredential"),
        }
    }

    // --- Config-file path resolution ----------------------------------------

    #[test]
    fn test_config_file_path_prefers_xdg_config_home() {
        let env = env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/user")]);
        assert_eq!(
            config_file_path(&env),
            Some(PathBuf::from("/xdg/chainview/config.toml"))
        );
    }

    #[test]
    fn test_config_file_path_falls_back_to_home() {
        let env = env(&[("HOME", "/home/user")]);
        assert_eq!(
            config_file_path(&env),
            Some(PathBuf::from("/home/user/.config/chainview/config.toml"))
        );
    }

    #[test]
    fn test_config_file_path_none_without_home() {
        assert_eq!(config_file_path(&empty_env()), None);
    }

    // --- Theme parsing -------------------------------------------------------

    #[test]
    fn test_theme_choice_from_str_accepts_known() {
        assert_eq!("auto".parse::<ThemeChoice>().ok(), Some(ThemeChoice::Auto));
        assert_eq!("DARK".parse::<ThemeChoice>().ok(), Some(ThemeChoice::Dark));
        assert_eq!(
            " light ".parse::<ThemeChoice>().ok(),
            Some(ThemeChoice::Light)
        );
    }

    #[test]
    fn test_theme_choice_from_str_rejects_unknown() {
        assert!("neon".parse::<ThemeChoice>().is_err());
    }

    // --- Log-file guard ------------------------------------------------------

    #[test]
    fn test_config_rejects_stdout_log_file() {
        let cli = CliOverrides {
            log_file: Some(PathBuf::from("/dev/stdout")),
            ..Default::default()
        };
        let result = Config::assemble(cli, &empty_env(), None);
        assert!(is_invalid_value(&result));
    }
}
