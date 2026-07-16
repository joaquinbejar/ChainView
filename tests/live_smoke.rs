//! The manual Deribit live smoke (issue #22, `docs/TESTING.md` §8).
//!
//! `#[ignore]`-by-default and gated on `SMOKE_DERIBIT=1`, so **CI never touches a
//! real venue** — the whole automated suite runs against fixtures
//! (`docs/TESTING.md` §7). An operator runs this before a release cut:
//!
//! ```bash
//! SMOKE_DERIBIT=1 cargo test --test live_smoke -- --ignored
//! ```
//!
//! Deribit public market data needs **no credentials** — it matches the
//! zero-config default (ADR-0003), so this smoke authenticates nothing and logs
//! nothing sensitive.
//!
//! # Current scope through the public surface
//!
//! The eventual smoke subscribes to one underlying, captures a few seconds of
//! updates, and asserts no panic + at least one chain update flowed through
//! (`docs/TESTING.md` §8). That streaming capture rides the render run-loop, whose
//! composition seam (`ChainViewApp::builder()…run()` spinning the loop, #13/#15)
//! is not yet reachable off the **public** surface — the built-in Deribit adapter
//! is crate-internal and `run()` today resolves the live source without spinning
//! the loop. Until that seam lands, this smoke exercises the zero-config Deribit
//! **composition** through the public builder — it asserts the stock
//! `with_builtins()` path resolves the Deribit live source end-to-end — and the
//! streaming-capture assertion is wired in the same change that makes the public
//! run-loop drive a real subscription. This file is honest about that boundary
//! rather than pretending to a venue round-trip the public API cannot yet make.

use std::collections::BTreeMap;
use std::time::Duration;

use chainview::{ChainViewApp, Config, ModeSelect, ProviderId, ThemeChoice};

/// Whether the operator opted into the live smoke.
fn smoke_enabled() -> bool {
    std::env::var_os("SMOKE_DERIBIT").is_some_and(|value| value == "1")
}

#[track_caller]
fn deribit_id() -> ProviderId {
    match ProviderId::new("deribit") {
        Ok(p) => p,
        Err(e) => panic!("`deribit` is a valid, reserved provider id: {e}"),
    }
}

/// The zero-config Deribit live config (no credentials — public data).
fn zero_config_deribit() -> Config {
    Config {
        provider: deribit_id(),
        underlying: "BTC".to_owned(),
        refresh_interval: Duration::from_secs(2),
        tick_interval: Duration::from_millis(250),
        channel_capacity: 1024,
        log_file: None,
        theme: ThemeChoice::Auto,
        no_color: false,
        providers: BTreeMap::new(),
        mode: ModeSelect::Live,
    }
}

#[test]
#[ignore = "manual Deribit smoke; set SMOKE_DERIBIT=1 and run with --ignored"]
fn smoke_deribit_zero_config_composes() {
    if !smoke_enabled() {
        // A guard so `-- --ignored` without the opt-in is a deterministic no-op
        // rather than an accidental venue-adjacent run.
        return;
    }
    // The stock zero-config path: register the gate-clear Deribit built-in and
    // resolve the live source through the PUBLIC builder. This composes the same
    // adapter the streaming smoke will drive once the public run-loop seam lands.
    let result = ChainViewApp::builder()
        .with_builtins()
        .with_config(zero_config_deribit())
        .run();
    assert!(
        result.is_ok(),
        "the zero-config Deribit live source must resolve through with_builtins(): {result:?}"
    );
    // TODO(#13/#15 public run-loop): once the public builder spins the render
    // loop, extend this smoke to subscribe to BTC, capture a few seconds of
    // updates, and assert at least one chain update flowed through with no panic
    // (`docs/TESTING.md` §8).
}
