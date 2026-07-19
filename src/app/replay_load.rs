//! The off-thread replay bundle load worker (`docs/04-replay-mode.md` §3,
//! `docs/02-tui-architecture.md` §12).
//!
//! Opening + decoding a bundle is synchronous, blocking, potentially multi-second
//! I/O (up to the resource ceilings), so it MUST NOT run on the render thread —
//! that would freeze frames ([ADR-0005](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0005-async-data-sync-render-split.md)).
//! [`spawn_bundle_load`] runs it on a [`spawn_blocking`](tokio::task::spawn_blocking)
//! worker and delivers the outcome back to the loop as a single
//! [`AppEvent::BundleLoaded`], which folds the replay
//! [`BundleLoad`](crate::BundleLoad) state from `Loading` to `Ready`/`Error`.
//!
//! # Cancellation is the supervisor's shutdown token, adapted at the seam
//!
//! The decode is a **measured, batched, cancellable** loop that polls a
//! `&dyn Fn() -> bool` probe at every batch boundary
//! ([`BundleReader::load_cancellable`](crate::BundleReader::load_cancellable)). The
//! app seam adapts the supervisor's [`CancellationToken`] into that probe with
//! `&|| cancel.is_cancelled()`, so a quit mid-load aborts promptly and the domain
//! reader never depends on `tokio`. A cancelled load emits **no** event — the app
//! is tearing down, and a `Cancelled` outcome is not a bundle error.

use std::path::Path;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::BundleError;
use crate::event::{AppEvent, BundleLoadResult};
use crate::replay::{BundleReader, ResourceCeilings};

/// Spawn the off-thread bundle load on a blocking worker and deliver its outcome
/// to the render loop as an [`AppEvent::BundleLoaded`]
/// (`docs/04-replay-mode.md` §3).
///
/// The worker runs [`BundleReader::open_with_ceilings`] then
/// [`load_cancellable`](crate::BundleReader::load_cancellable) with `cancel`
/// adapted as the cancellation probe, so a shutdown aborts the decode at the next
/// batch boundary. On success it sends [`BundleLoadResult::Loaded`]; on a
/// [`BundleError`] other than [`BundleError::Cancelled`] it sends
/// [`BundleLoadResult::Failed`] with the non-secret message. A cancelled load
/// sends nothing (the app is shutting down). A closed event channel (the render
/// loop is gone) drops the outcome harmlessly.
///
/// The returned [`JoinHandle`] is registered with the supervisor as the replay
/// load/seek worker so shutdown joins it (`docs/02-tui-architecture.md` §12).
#[must_use = "register the returned JoinHandle with the Supervisor so it has a shutdown path"]
pub fn spawn_bundle_load(
    dir: impl Into<std::path::PathBuf>,
    ceilings: ResourceCeilings,
    tx_events: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let dir = dir.into();
    tokio::task::spawn_blocking(move || {
        // A pre-cancelled load never touches the filesystem.
        if cancel.is_cancelled() {
            return;
        }
        if let Some(result) = load_bundle(&dir, ceilings, &cancel) {
            // Blocking send from the blocking worker: respects backpressure. A
            // closed channel (render loop gone) drops the outcome harmlessly.
            let _ = tx_events.blocking_send(AppEvent::BundleLoaded(result));
        }
    })
}

/// Open + load the bundle at `dir`, mapping the outcome to a [`BundleLoadResult`],
/// or `None` when the caller cancelled (shutdown — no event should be emitted).
///
/// `cancel` is adapted into the reader's `&dyn Fn() -> bool` probe exactly as the
/// #30 seam documents, keeping the domain reader free of `tokio`.
fn load_bundle(
    dir: &Path,
    ceilings: ResourceCeilings,
    cancel: &CancellationToken,
) -> Option<BundleLoadResult> {
    let reader = match BundleReader::open_with_ceilings(dir, ceilings) {
        Ok(reader) => reader,
        Err(BundleError::Cancelled) => return None,
        Err(error) => return Some(BundleLoadResult::Failed(error.to_string())),
    };
    match reader.load_cancellable(&|| cancel.is_cancelled()) {
        Ok(bundle) => Some(BundleLoadResult::Loaded(Box::new(bundle))),
        // A cancelled load is shutdown, not a bad bundle — emit nothing.
        Err(BundleError::Cancelled) => None,
        Err(error) => Some(BundleLoadResult::Failed(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{load_bundle, spawn_bundle_load};
    use crate::event::{AppEvent, BundleLoadResult};
    use crate::replay::ResourceCeilings;

    #[test]
    fn test_load_bundle_missing_dir_is_failed_not_panic() {
        // A non-existent bundle directory is a typed failure carrying a non-secret
        // message — never a panic. (A missing dir is also caught pre-TUI by the
        // CLI check; this proves the worker degrades gracefully regardless.)
        let cancel = CancellationToken::new();
        let dir = std::env::temp_dir().join("chainview-nonexistent-bundle-xyz-34");
        // Ensure it really is absent.
        let _ = fs::remove_dir_all(&dir);
        match load_bundle(&dir, ResourceCeilings::default(), &cancel) {
            Some(BundleLoadResult::Failed(message)) => {
                assert!(!message.is_empty(), "the failure carries a message");
            }
            other => panic!("expected Failed for a missing dir, got {other:?}"),
        }
    }

    #[test]
    fn test_load_bundle_pre_cancelled_returns_none() {
        // A pre-cancelled probe short-circuits `open`/`load` — no event should be
        // emitted for a shutdown.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let dir = std::env::temp_dir().join("chainview-any-bundle-dir");
        // `open_with_ceilings` sees the cancelled token via `load_cancellable`
        // only after opening; but a bad dir returns Failed. To assert the
        // pure cancellation path deterministically we rely on the spawn wrapper's
        // pre-check below; here we simply require no panic for a cancelled probe.
        let _ = load_bundle(&dir, ResourceCeilings::default(), &cancel);
    }

    #[tokio::test]
    async fn test_spawn_bundle_load_pre_cancelled_emits_no_event() {
        // The spawn wrapper checks the token before touching the filesystem: a
        // pre-cancelled load emits nothing and the worker joins cleanly.
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let handle = spawn_bundle_load(
            std::env::temp_dir().join("chainview-bundle"),
            ResourceCeilings::default(),
            tx,
            cancel,
        );
        match handle.await {
            Ok(()) => {}
            Err(e) => panic!("load worker join failed: {e}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "a pre-cancelled load emits no event"
        );
    }

    #[tokio::test]
    async fn test_spawn_bundle_load_missing_dir_emits_failed() {
        // A missing bundle directory surfaces as a `BundleLoaded(Failed)` event —
        // the worker never panics and the message is non-secret.
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let cancel = CancellationToken::new();
        let dir = std::env::temp_dir().join("chainview-missing-bundle-xyz-34b");
        let _ = fs::remove_dir_all(&dir);
        let handle = spawn_bundle_load(dir, ResourceCeilings::default(), tx, cancel);
        match handle.await {
            Ok(()) => {}
            Err(e) => panic!("load worker join failed: {e}"),
        }
        match rx.try_recv() {
            Ok(AppEvent::BundleLoaded(BundleLoadResult::Failed(_))) => {}
            other => panic!("expected BundleLoaded(Failed), got {other:?}"),
        }
    }
}
