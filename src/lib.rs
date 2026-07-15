//! # ChainView
//!
//! Terminal UI for option chains, Greeks and volatility — real-time market
//! data and backtest replay, rendered in your terminal.
//!
//! **Status:** early development — the crate skeleton is in place. The first
//! runtime surface to land is the boundary error type [`ChainViewError`]; the
//! remaining modules are planned surfaces and carry no runtime behavior yet.
//! Follow progress at <https://github.com/joaquinbejar/ChainView>.

#![forbid(unsafe_code)]

pub(crate) mod app;
pub(crate) mod chain;
pub mod config;
pub(crate) mod error;
pub(crate) mod event;
pub(crate) mod providers;
pub(crate) mod ui;

pub use chain::{
    CHAIN_STALE_SLACK, ChainSnapshot, ContractSpecFingerprint, DIRECTION_DECAY, DepthLadder,
    DepthLevel, ExerciseStyle, FEED_DELAY_WARN, GREEKS_STALE_AFTER, GreeksOrigin, GreeksRow,
    Instrument, InstrumentKey, MarketUpdate, ProviderId, QUOTE_STALE_AFTER, QuoteUpdate,
    RESERVED_PROVIDER_IDS, SettlementStyle, StreamHealth, chain_stale_after,
};
pub use config::{CliOverrides, Config, ModeSelect, ProviderSettings, ThemeChoice};
pub use error::{
    BundleError, ChainViewError, ConfigError, NormalizeKind, OverlayError, ProviderError, Redacted,
    RegistryError, TransportDetail, TransportKind,
};
// The domain speaks `optionstratlib`'s numeric vocabulary
// (`docs/01-domain-model.md` §3–§4); re-export the two types that appear on the
// public identity surface so downstream callers can name them without depending
// on `optionstratlib` directly.
pub use optionstratlib::OptionStyle;
pub use optionstratlib::prelude::Positive;
