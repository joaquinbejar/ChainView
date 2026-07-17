//! Application events and the render-loop -> data-layer command channel
//! (`docs/02-tui-architecture.md` §4).
//!
//! [`AppEvent`] is the **single** closed event set the synchronous render loop
//! folds: terminal input ([`Key`](AppEvent::Key) / [`Resize`](AppEvent::Resize)),
//! the render/animation [`Tick`](AppEvent::Tick), a normalized provider
//! [`Market`](AppEvent::Market) update, a replay
//! [`ReplaySeek`](AppEvent::ReplaySeek) / [`ReplayControl`](AppEvent::ReplayControl),
//! and the off-thread replay-bundle-load result
//! ([`BundleLoaded`](AppEvent::BundleLoaded)). Producers are independent tokio
//! tasks that all push into one `mpsc::Receiver<AppEvent>` (§4); the fan-in that
//! folds them into state is [`App::on_event`](crate::App::on_event).
//!
//! `AppEvent` is a ChainView **closed set**, matched exhaustively with **no
//! wildcard `_` arm** (`CLAUDE.md` "Key Decisions"), so adding a variant forces
//! every fold site to be revisited by the compiler.
//!
//! # Handlers stay pure — I/O is a [`Command`], never inline
//!
//! A fold that needs I/O (subscribe to a new expiry, reconnect, seek within the
//! bundle) emits a [`Command`] back to the async data layer over the app's
//! bounded command channel; it never performs the I/O inline and never `.await`s
//! ([ADR-0005](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0005-async-data-sync-render-split.md)).
//! That keeps `ratatui` off the async executor.

use std::path::PathBuf;

use crossterm::event::KeyEvent;
use optionstratlib::ExpirationDate;

use crate::chain::MarketUpdate;
use crate::replay::LoadedBundle;

/// Every input to the synchronous render loop, as one closed set (§4).
///
/// Matched exhaustively with no wildcard arm, so a new variant forces every fold
/// site — notably [`App::on_event`](crate::App::on_event) — to be revisited by
/// the compiler.
///
/// The `Market` variant is far larger than the input/tick variants, but it rides
/// the **hot** fan-in path: boxing it would add a heap allocation per quote/Greek
/// update — exactly what the bounded coalescing channel (#10) exists to avoid
/// (`docs/06-performance.md`). The transient over-allocation is bounded by the
/// channel capacity and small, so the documented unboxed shape (§4) is kept.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum AppEvent {
    /// A crossterm key press, from the dedicated terminal-input reader task.
    Key(KeyEvent),
    /// The terminal was resized to `(columns, rows)`.
    Resize(u16, u16),
    /// The render/animation cadence fired (default ~250 ms).
    Tick,
    /// A normalized provider update to fold into the live chain store (§4).
    Market(MarketUpdate),
    /// A replay-timeline scrub, in replay mode only
    /// (`docs/04-replay-mode.md` §4). Folds directly into the in-memory timeline
    /// cursor (the whole bundle is loaded, #33), so it performs no I/O.
    ReplaySeek(SeekTo),
    /// A replay playback control (play/pause/speed), in replay mode only
    /// (`docs/04-replay-mode.md` §4). Folds into the [`Playback`](crate::Playback)
    /// state; the tick timer advances the cursor while playing.
    ReplayControl(ReplayControl),
    /// The result of the off-thread replay-bundle load (loaded or failed),
    /// delivered by the replay load worker (`docs/04-replay-mode.md` §3,
    /// `docs/02-tui-architecture.md` §12). Folds the replay
    /// [`BundleLoad`](crate::BundleLoad) state machine from `Loading` to `Ready`
    /// or `Error`; the render loop never blocks on the load.
    BundleLoaded(BundleLoadResult),
}

/// A replay playback control produced by a play/pause/speed key on the replay
/// screen and folded into the [`Playback`](crate::Playback) state
/// (`docs/04-replay-mode.md` §4).
///
/// A ChainView closed set matched exhaustively with no wildcard arm, so a new
/// control forces every fold site to be revisited by the compiler. Fieldless, so
/// `#[repr(u8)]` per the ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReplayControl {
    /// Toggle play / pause (`Space`).
    PlayPause,
    /// Increase the playback speed one step, clamped at the fastest (`+`).
    SpeedFaster,
    /// Decrease the playback speed one step, clamped at the slowest (`-`).
    SpeedSlower,
}

/// The outcome of an off-thread replay-bundle load, delivered to the render loop
/// as an [`AppEvent::BundleLoaded`] by the replay load worker
/// (`docs/04-replay-mode.md` §3, `docs/02-tui-architecture.md` §12).
///
/// The load runs on a blocking worker, never the render thread, so a multi-second
/// decode of a large bundle can never freeze a frame; its result is what folds the
/// replay [`BundleLoad`](crate::BundleLoad) state from `Loading` to `Ready`/`Error`.
/// The success payload is boxed so it does not bloat the event, and the failure
/// carries only the non-secret, ChainView-authored
/// [`BundleError`](crate::BundleError) text (`R` retries).
#[derive(Debug, Clone)]
pub enum BundleLoadResult {
    /// The bundle opened, decoded, and validated — the materialised tables.
    Loaded(Box<LoadedBundle>),
    /// The load failed with an actionable, non-secret message.
    Failed(String),
}

/// A replay-timeline seek, expressed against the one integer replay clock — the
/// `step` (`docs/04-replay-mode.md` §4). The display timestamp `ts_ns` is never
/// the seek unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekTo {
    /// Seek to an absolute step (`Home` = 0, `End` = the last step).
    Step(u32),
    /// Seek by a signed delta: `±1` for `←`/`→`, `±quantum` during playback.
    StepBy(i32),
}

/// A render-loop -> data-layer command (§4).
///
/// Emitted by a fold that needs I/O so the handler itself stays pure and fast:
/// the async data layer performs the actual subscribe / reconnect / bundle work.
/// It rides the app's **bounded, small** command channel; a send is non-blocking
/// and never awaited (`docs/02-tui-architecture.md` §4, §5).
///
/// `PartialEq`/`Eq` are intentionally **not** derived: [`ExpirationDate`] does
/// not implement them, and modelling the subscribe payloads exactly as
/// `docs/02-tui-architecture.md` §4 specifies (an `ExpirationDate`, not a
/// pre-resolved instant) is worth more than value-comparability — tests
/// pattern-match on the received command instead.
#[derive(Debug, Clone)]
pub enum Command {
    /// Live: open the streams for one `(underlying, expiration)`.
    Subscribe {
        /// The canonical upper-case underlying ticker.
        underlying: String,
        /// The expiration to subscribe.
        expiration: ExpirationDate,
    },
    /// Live: drop the subscription for one `(underlying, expiration)`.
    Unsubscribe {
        /// The canonical upper-case underlying ticker.
        underlying: String,
        /// The expiration to unsubscribe.
        expiration: ExpirationDate,
    },
    /// Live: provider-driven recovery — reconnect the active stream (`r`).
    Reconnect,
    /// Live: mode reload — re-run discover + fetch for the active provider (`R`).
    Rediscover,
    /// Replay: re-open and revalidate the bundle at the directory (`R`). Seeking
    /// is **not** a command: with the whole bundle loaded (#33) a scrub is an
    /// in-memory cursor move folded directly by [`AppEvent::ReplaySeek`], never a
    /// round-trip to the data layer.
    ReloadBundle(PathBuf),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::prelude::Positive;

    use super::{AppEvent, BundleLoadResult, Command, ReplayControl, SeekTo};
    use crate::chain::{MarketUpdate, ProviderId, StreamHealth};

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    // --- AppEvent construction -----------------------------------------------

    #[test]
    fn test_app_event_market_wraps_health_update() {
        let event = AppEvent::Market(MarketUpdate::Health(pid("deribit"), StreamHealth::Live));
        match event {
            AppEvent::Market(MarketUpdate::Health(provider, StreamHealth::Live)) => {
                assert_eq!(provider.as_str(), "deribit");
            }
            other => panic!("expected Market(Health(_, Live)), got {other:?}"),
        }
    }

    #[test]
    fn test_app_event_key_wraps_key_event() {
        let event = AppEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        match event {
            AppEvent::Key(key) => assert_eq!(key.code, KeyCode::Char('q')),
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    fn test_app_event_resize_carries_columns_and_rows() {
        let event = AppEvent::Resize(120, 40);
        match event {
            AppEvent::Resize(cols, rows) => {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            other => panic!("expected Resize, got {other:?}"),
        }
    }

    // --- SeekTo --------------------------------------------------------------

    #[test]
    fn test_seek_to_step_and_step_by_are_distinct() {
        assert_ne!(SeekTo::Step(0), SeekTo::StepBy(0));
        assert_eq!(SeekTo::Step(3), SeekTo::Step(3));
        // `Copy`: a value used twice compiles without a move error.
        let seek = SeekTo::StepBy(-1);
        let _first = seek;
        let _second = seek;
    }

    // --- Command variants ----------------------------------------------------

    #[test]
    fn test_command_reconnect_and_rediscover_construct() {
        match Command::Reconnect {
            Command::Reconnect => {}
            other => panic!("expected Reconnect, got {other:?}"),
        }
        match Command::Rediscover {
            Command::Rediscover => {}
            other => panic!("expected Rediscover, got {other:?}"),
        }
    }

    #[test]
    fn test_command_subscribe_carries_underlying_and_expiration() {
        let command = Command::Subscribe {
            underlying: "BTC".to_owned(),
            expiration: optionstratlib::ExpirationDate::Days(pos(30.0)),
        };
        match command {
            Command::Subscribe {
                underlying,
                expiration,
            } => {
                assert_eq!(underlying, "BTC");
                match expiration {
                    optionstratlib::ExpirationDate::Days(days) => assert_eq!(days, pos(30.0)),
                    other => panic!("expected Days, got {other:?}"),
                }
            }
            other => panic!("expected Subscribe, got {other:?}"),
        }
    }

    #[test]
    fn test_command_reload_bundle_constructs() {
        // Reload is the ONLY replay command: seeking folds into the in-memory
        // cursor (#33), so no `SeekBundle` command exists.
        match Command::ReloadBundle(PathBuf::from("/bundle")) {
            Command::ReloadBundle(dir) => assert_eq!(dir, PathBuf::from("/bundle")),
            other => panic!("expected ReloadBundle, got {other:?}"),
        }
    }

    #[test]
    fn test_app_event_replay_control_and_bundle_loaded_construct() {
        match AppEvent::ReplayControl(ReplayControl::PlayPause) {
            AppEvent::ReplayControl(ReplayControl::PlayPause) => {}
            other => panic!("expected ReplayControl(PlayPause), got {other:?}"),
        }
        match AppEvent::BundleLoaded(BundleLoadResult::Failed("bad bundle".to_owned())) {
            AppEvent::BundleLoaded(BundleLoadResult::Failed(message)) => {
                assert_eq!(message, "bad bundle");
            }
            other => panic!("expected BundleLoaded(Failed), got {other:?}"),
        }
    }

    // --- Compile-fence: the AppEvent closed set has no wildcard fold ----------

    #[test]
    fn test_app_event_match_is_wildcard_free() {
        // This exhaustive, wildcard-free match mirrors `on_event`'s discipline:
        // adding an `AppEvent` variant breaks THIS match at compile time, forcing
        // every fold site (including `on_event`) to be revisited.
        fn label(event: &AppEvent) -> &'static str {
            match event {
                AppEvent::Key(_) => "key",
                AppEvent::Resize(_, _) => "resize",
                AppEvent::Tick => "tick",
                AppEvent::Market(_) => "market",
                AppEvent::ReplaySeek(_) => "seek",
                AppEvent::ReplayControl(_) => "control",
                AppEvent::BundleLoaded(_) => "loaded",
            }
        }
        assert_eq!(label(&AppEvent::Tick), "tick");
    }

    #[test]
    fn test_replay_control_match_is_wildcard_free() {
        // `ReplayControl` is a ChainView closed set (like `AppEvent`): this
        // exhaustive, wildcard-free match mirrors every fold site (`apply_control`),
        // so adding a control variant breaks THIS match at compile time and forces
        // every fold site to be revisited.
        fn label(control: ReplayControl) -> &'static str {
            match control {
                ReplayControl::PlayPause => "playpause",
                ReplayControl::SpeedFaster => "faster",
                ReplayControl::SpeedSlower => "slower",
            }
        }
        assert_eq!(label(ReplayControl::PlayPause), "playpause");
        assert_eq!(label(ReplayControl::SpeedFaster), "faster");
        assert_eq!(label(ReplayControl::SpeedSlower), "slower");
    }
}
