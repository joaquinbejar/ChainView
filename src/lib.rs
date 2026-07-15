//! # ChainView
//!
//! Terminal UI for option chains, Greeks and volatility — real-time market
//! data and backtest replay, rendered in your terminal.
//!
//! **Status:** early development — the crate skeleton is in place; the
//! modules below are planned surfaces and carry no runtime behavior yet.
//! Follow progress at <https://github.com/joaquinbejar/ChainView>.

#![forbid(unsafe_code)]

pub(crate) mod app;
pub(crate) mod chain;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod event;
pub(crate) mod providers;
pub(crate) mod ui;
