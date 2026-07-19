//! Application events and the render-loop -> data-layer command channel
//! (`docs/02-tui-architecture.md` §4).
//!
//! [`AppEvent`] is the **single** closed event set the synchronous render loop
//! folds: terminal input ([`Key`](AppEvent::Key) / [`Resize`](AppEvent::Resize)),
//! the render/animation [`Tick`](AppEvent::Tick), a normalized provider
//! [`Market`](AppEvent::Market) update, and a replay
//! [`ReplaySeek`](AppEvent::ReplaySeek). Producers are independent tokio tasks
//! that all push into one `mpsc::Receiver<AppEvent>` (§4); the fan-in that folds
//! them into state is [`App::on_event`](crate::App::on_event).
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
    /// (`docs/04-replay-mode.md` §4).
    ReplaySeek(SeekTo),
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
    /// Replay: advance/rewind the table indices by the given seek
    /// (`docs/04-replay-mode.md` §4).
    SeekBundle(SeekTo),
    /// Replay: re-open and revalidate the bundle at the directory (`R`).
    ReloadBundle(PathBuf),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use optionstratlib::prelude::Positive;

    use super::{AppEvent, Command, SeekTo};
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
    fn test_command_seek_bundle_and_reload_bundle_construct() {
        match Command::SeekBundle(SeekTo::Step(5)) {
            Command::SeekBundle(SeekTo::Step(5)) => {}
            other => panic!("expected SeekBundle(Step(5)), got {other:?}"),
        }
        match Command::ReloadBundle(PathBuf::from("/bundle")) {
            Command::ReloadBundle(dir) => assert_eq!(dir, PathBuf::from("/bundle")),
            other => panic!("expected ReloadBundle, got {other:?}"),
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
            }
        }
        assert_eq!(label(&AppEvent::Tick), "tick");
    }
}
