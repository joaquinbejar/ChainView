//! Task-supervisor shutdown integration tests (issue #11,
//! `docs/TESTING.md` §7).
//!
//! These drive the PUBLIC supervisor surface with **real** tokio tasks (via
//! [`chainview::TokioTask`] wrapping a real `JoinHandle`) so the ordered
//! teardown, the panic-as-fatal detection
//! (`JoinError::is_panic` via `TokioTask::join`), the non-zero exit cause, and
//! the terminal-restored-last / no-orphan guarantees are exercised end to end —
//! deterministically, with no real terminal and no wall-clock wait.
//!
//! A local [`RecordingTeardown`] stands in for the production `GuardTeardown` so
//! the "terminal restore ran, last" assertion needs no TTY.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chainview::{
    ExitCause, FinalTeardown, ProviderId, SupervisedTask, Supervisor, TaskExit, TokioTask,
};
use tokio_util::sync::CancellationToken;

/// A `FinalTeardown` that records that the (final, last) terminal restore ran.
struct RecordingTeardown {
    restored: Arc<AtomicBool>,
}

impl FinalTeardown for RecordingTeardown {
    fn run(self: Box<Self>) {
        self.restored.store(true, Ordering::SeqCst);
    }
}

#[track_caller]
fn provider_id(id: &str) -> ProviderId {
    match ProviderId::new(id) {
        Ok(provider) => provider,
        Err(error) => panic!("expected a valid provider id `{id}`, got: {error}"),
    }
}

/// Spawn a cooperative task that returns (setting `done`) once its child token is
/// cancelled, and return the child token the supervisor cancels plus the wrapped
/// handle.
fn cooperative_task(done: Arc<AtomicBool>, child: CancellationToken) -> TokioTask {
    let task_child = child.clone();
    let handle = tokio::spawn(async move {
        task_child.cancelled().await;
        done.store(true, Ordering::SeqCst);
    });
    TokioTask::new(handle)
}

#[tokio::test]
async fn test_supervisor_normal_quit_joins_every_task_and_restores_terminal() {
    let restored = Arc::new(AtomicBool::new(false));
    let mut supervisor = Supervisor::new(Box::new(RecordingTeardown {
        restored: restored.clone(),
    }));

    let provider_done = Arc::new(AtomicBool::new(false));
    let provider_child = supervisor.child_token();
    supervisor.register_provider(
        provider_id("deribit"),
        provider_child.clone(),
        Box::new(cooperative_task(provider_done.clone(), provider_child)),
    );

    let input_done = Arc::new(AtomicBool::new(false));
    let input_child = supervisor.child_token();
    supervisor.register_ancillary(
        input_child.clone(),
        Box::new(cooperative_task(input_done.clone(), input_child)),
    );

    supervisor.request_quit();
    let cause = supervisor.run().await;

    assert!(cause.is_clean(), "a normal quit exits clean");
    assert_eq!(cause.exit_code(), 0);
    assert!(
        provider_done.load(Ordering::SeqCst),
        "the provider task joined"
    );
    assert!(input_done.load(Ordering::SeqCst), "the input task joined");
    assert!(
        restored.load(Ordering::SeqCst),
        "the terminal was restored (last step)"
    );
}

#[tokio::test]
async fn test_supervisor_reported_provider_panic_exits_nonzero_with_no_orphan() {
    let restored = Arc::new(AtomicBool::new(false));
    let mut supervisor = Supervisor::new(Box::new(RecordingTeardown {
        restored: restored.clone(),
    }));

    // A cooperative provider + input that must still be joined (no orphan) when
    // another task panics.
    let provider_done = Arc::new(AtomicBool::new(false));
    let provider_child = supervisor.child_token();
    supervisor.register_provider(
        provider_id("deribit"),
        provider_child.clone(),
        Box::new(cooperative_task(provider_done.clone(), provider_child)),
    );
    let input_done = Arc::new(AtomicBool::new(false));
    let input_child = supervisor.child_token();
    supervisor.register_ancillary(
        input_child.clone(),
        Box::new(cooperative_task(input_done.clone(), input_child)),
    );

    // A provider task that PANICS; its join-watcher detects the panic
    // (`JoinError::is_panic` via `TokioTask::join`) and reports it as fatal —
    // exactly the #13/#16 wiring. This is what trips the root token.
    let panic_handle = tokio::spawn(async move {
        panic!("injected provider panic");
    });
    let reporter = supervisor.exit_reporter();
    let watcher = tokio::spawn(async move {
        let mut task = TokioTask::new(panic_handle);
        let exit = task.join().await;
        reporter.report(exit);
        exit
    });

    let cause = supervisor.run().await;

    // The watcher observed a real panic (fatal), not a clean completion.
    match watcher.await {
        Ok(exit) => assert_eq!(exit, TaskExit::Panicked, "the watcher saw a panic"),
        Err(error) => panic!("the watcher task itself failed: {error}"),
    }
    assert!(!cause.is_clean(), "a supervised panic is not a clean exit");
    assert_eq!(cause.exit_code(), 1, "a supervised panic exits non-zero");
    assert!(matches!(cause, ExitCause::TaskPanicked));
    assert!(
        provider_done.load(Ordering::SeqCst),
        "the cooperative provider still joined — no orphan"
    );
    assert!(
        input_done.load(Ordering::SeqCst),
        "the input task still joined — no orphan"
    );
    assert!(
        restored.load(Ordering::SeqCst),
        "the terminal was restored on the failure path"
    );
}
