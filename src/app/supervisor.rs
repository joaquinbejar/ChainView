//! The single task supervisor and the ordered, terminal-restored-last teardown
//! (`docs/02-tui-architecture.md` §12, [ADR-0005]).
//!
//! The async layer spawns several independent tasks around the synchronous
//! render thread — one or more provider tasks (each owning its reconnect loop),
//! the crossterm input reader, the tick task, and in replay the load/seek worker
//! ([ADR-0005]). **One** [`Supervisor`], owned by the application layer, owns
//! **all** of their handles and a process-wide cancellation protocol, so the
//! invariant "every spawned task has a shutdown path" is enforceable across the
//! whole process, not per task.
//!
//! # Cancellation-token tree
//!
//! A root [`CancellationToken`] has one [`child_token`](CancellationToken::child_token)
//! per task ([`Supervisor::child_token`]). Cancelling the root cancels every
//! child; a single provider can be cancelled without touching the others
//! ([`Supervisor::cancel_provider`], used on `Unsubscribe`/`Rediscover`). Every
//! task's loop selects on its child token, so it observes cancellation at the
//! next await point. The supervisor coordinates **by tokens and join handles,
//! never a shared lock across an `.await`** (`rules/global_rules.md` —
//! Concurrency).
//!
//! # Shutdown triggers all converge on one teardown
//!
//! A clean quit (`q`/`Ctrl-C`, via [`Supervisor::request_quit`] wired from
//! `App::should_quit`), a startup failure or a provider failure past its
//! reconnect budget or a channel close (via [`Supervisor::fail`]), and a **panic
//! in any task** (reported through the [`ExitReporter`] seam as
//! [`TaskExit::Panicked`], detected via a [`JoinHandle`](tokio::task::JoinHandle)
//! join result whose [`JoinError::is_panic`](tokio::task::JoinError::is_panic) is
//! true — see [`TokioTask`]) all trip the root token. The panic path is a
//! task-level signal the supervisor treats as **fatal**; it does not rely on the
//! process panic hook alone (that only restores the terminal —
//! `docs/02-tui-architecture.md` §6). No trigger ever leaves an orphan task.
//!
//! # Deterministic join order — terminal restored LAST
//!
//! On teardown the supervisor: (0) drains the **mid-run watch set** (the watched
//! provider handles, [`watch`](Supervisor::watch)), then (1) cancels + joins the
//! **provider** tasks (stop new data), then (2) cancels + joins the
//! **input/tick/replay** tasks, then (3) lets the **render** thread exit its
//! loop, and only then (4) runs the [`FinalTeardown`] that restores the terminal
//! ([`TerminalGuard`] via [`GuardTeardown`], `docs/02-tui-architecture.md` §6).
//! Terminal restore is the **last** step on every path, including panic.
//!
//! Restore has a **single owner**. [`run`](Supervisor::run) claims terminal-
//! restore ownership for the whole supervised lifecycle (via
//! [`crate::terminal::set_supervisor_owns_restore`]); while it is claimed the
//! process panic hook DEFERS its own restore, so a worker-task panic can never
//! restore the terminal out from under a still-live render draw. The supervisor
//! restores LAST (after joining the render task); if its future is unwound by a
//! main-thread panic instead, the [`TerminalGuard`] it owns restores on `Drop`.
//! Either way the "terminal always restored on panic" invariant holds.
//!
//! # Bounded join, then abort
//!
//! Each join has a [`DEFAULT_JOIN_BUDGET`] (2 s) timeout; a task that has not
//! observed cancellation and returned within budget is
//! [`abort`](SupervisedTask::abort)ed so a wedged upstream socket can never hang
//! the exit. Abort is a last resort — a well-behaved task returns on its token
//! well inside the budget.
//!
//! # Error propagation — the seam to `main`
//!
//! The **first** fatal cause (startup, provider-fatal, panic) is recorded and
//! returned as the [`ExitCause`] from [`Supervisor::run`]; a clean quit returns
//! [`ExitCause::Clean`]. The supervisor **never** calls
//! [`std::process::exit`] — that would bypass the [`TerminalGuard`]'s `Drop`.
//! `main` maps the returned [`ExitCause`] to a process exit code
//! ([`ExitCause::exit_code`]) and surfaces its [`ExitCause::failure_message`] on
//! `stderr` **after** the terminal is restored (the one post-restore `stderr`
//! line permitted by CLAUDE.md "Governance precedence" item 3).
//!
//! # Scope of this issue (#11) and the seams left for later
//!
//! This lands the supervisor, the cancellation-token tree, the ordered teardown,
//! the bounded-join-then-abort, error propagation, per-provider cancellation, and
//! the mid-run watch set that wakes the loop on a task that panics or returns
//! mid-run. The [`TerminalGuard`]/panic hook itself is #8 (sequenced last here);
//! the bounded channels are #10; the provider reconnect-loop internals are #16;
//! the render loop that spawns the real tasks and drives the trigger sources is
//! #13. The seams are explicit: [`Supervisor::child_token`] hands each task its
//! cooperative-cancel token, [`Supervisor::watch`] enrols a spawned handle in the
//! supervisor-owned watch set so its mid-run panic/return wakes `supervise` (the
//! seam the #22 registry registers a provider task INTO),
//! [`Supervisor::exit_reporter`] hands a caller-owned join-watcher the channel
//! that reports a panic as fatal, and the
//! [`register_provider`](Supervisor::register_provider) /
//! [`register_ancillary`](Supervisor::register_ancillary) /
//! [`set_render`](Supervisor::set_render) methods take the [`Box<dyn
//! SupervisedTask>`](SupervisedTask) the render loop wires up.
//!
//! [ADR-0005]: https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0005-async-data-sync-render-split.md

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::chain::ProviderId;
use crate::error::ChainViewError;
use crate::terminal::TerminalGuard;

/// The 2 s bounded-join budget: a supervised task that has not observed
/// cancellation and returned within this window is [`abort`](SupervisedTask::abort)ed
/// so a wedged upstream socket can never hang the exit
/// (`docs/02-tui-architecture.md` §12).
pub const DEFAULT_JOIN_BUDGET: Duration = Duration::from_secs(2);

/// Capacity of the small bounded channel that carries task-exit reports from the
/// per-task join-watchers to the supervise loop. Small because a task reports its
/// terminal outcome exactly once, and the loop drains one report per shutdown.
const EXIT_REPORT_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// The supervised-task abstraction (real tokio task vs. a test double).
// ---------------------------------------------------------------------------

/// How a supervised task ended, as observed at its join point.
///
/// [`Panicked`](TaskExit::Panicked) is the **fatal** outcome: the real
/// [`TokioTask`] maps a [`JoinError::is_panic`](tokio::task::JoinError::is_panic)
/// to it, so a panicking task trips the root token rather than being silently
/// swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskExit {
    /// The task returned normally (including an abort-cancelled task, which is
    /// gone, not a panic).
    Completed,
    /// The task panicked — a fatal signal the supervisor records as the exit
    /// cause.
    Panicked,
}

/// A task the supervisor can join within a budget and, as a last resort,
/// [`abort`](SupervisedTask::abort).
///
/// Cooperative cancellation is driven **out of band** by the task's child
/// [`CancellationToken`] (the supervisor cancels it just before joining), so this
/// trait models only the *join* and the *forced abort*. Abstracting it lets the
/// join-order and the bounded-join-then-abort be unit-tested against a
/// deterministic mock with no real socket and no wall-clock wait; production uses
/// [`TokioTask`].
#[async_trait]
pub trait SupervisedTask: Send {
    /// Await the task's terminal outcome. A well-behaved task resolves promptly
    /// after its child token is cancelled; a wedged one never resolves and is
    /// caught by the join budget.
    async fn join(&mut self) -> TaskExit;

    /// Forcibly abort the task — the last resort after the join budget elapses.
    /// Idempotent and non-blocking.
    fn abort(&mut self);
}

/// The production [`SupervisedTask`]: a tokio [`JoinHandle`](tokio::task::JoinHandle).
///
/// [`join`](SupervisedTask::join) awaits the handle and maps a
/// [`JoinError::is_panic`](tokio::task::JoinError::is_panic) to
/// [`TaskExit::Panicked`] (a cancelled/aborted task is
/// [`TaskExit::Completed`] — gone, not a panic); [`abort`](SupervisedTask::abort)
/// calls [`JoinHandle::abort`](tokio::task::JoinHandle::abort).
///
/// **The handle is retained in `self` across the join await** (the await is over
/// `&mut JoinHandle`, which is a `Future` because
/// [`JoinHandle`](tokio::task::JoinHandle) is `Unpin`). This is load-bearing for
/// the bounded-join-then-abort: if the join budget's `timeout` drops the in-flight
/// join future, the handle stays in `self`, so the follow-up
/// [`abort`](SupervisedTask::abort) truly cancels the task. Awaiting an **owned**
/// (taken) handle would instead let the timeout drop the *owned* handle, which
/// merely **detaches** the task (tokio semantics) and leaves a wedged orphan —
/// the exact failure this supervisor exists to prevent.
#[derive(Debug)]
pub struct TokioTask {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl TokioTask {
    /// Wrap a spawned task's join handle for supervision.
    #[must_use]
    pub fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

#[async_trait]
impl SupervisedTask for TokioTask {
    async fn join(&mut self) -> TaskExit {
        let Some(handle) = self.handle.as_mut() else {
            // Already joined — nothing left to await.
            return TaskExit::Completed;
        };
        // Await `&mut JoinHandle`, NOT a taken owned handle: the handle stays in
        // `self` so a timeout dropping this future leaves `abort()` a live handle
        // to cancel (dropping an owned handle would only detach the task).
        let result = handle.await;
        self.handle = None;
        match result {
            Ok(()) => TaskExit::Completed,
            // A panic is the one fatal join outcome (docs/02 §12).
            Err(error) if error.is_panic() => TaskExit::Panicked,
            // A cancelled/aborted task is gone, not a failure.
            Err(_) => TaskExit::Completed,
        }
    }

    fn abort(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// The final teardown step: restore the terminal, strictly LAST.
// ---------------------------------------------------------------------------

/// The strictly-last shutdown step: restore the terminal
/// (`docs/02-tui-architecture.md` §12, §6).
///
/// Abstracted so the "terminal restore is last" ordering is unit-testable
/// against a recording fake with no real TTY; production is [`GuardTeardown`],
/// which drops the [`TerminalGuard`]. `run` consumes `self` so restore can only
/// run once.
pub trait FinalTeardown: Send {
    /// Run the final teardown (restore the terminal). Called exactly once, as the
    /// last step of [`Supervisor::run`].
    fn run(self: Box<Self>);
}

/// The production [`FinalTeardown`]: owns the [`TerminalGuard`] and restores the
/// terminal by dropping it — the strictly-last shutdown step
/// (`docs/02-tui-architecture.md` §12).
pub struct GuardTeardown {
    guard: TerminalGuard,
}

impl std::fmt::Debug for GuardTeardown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `TerminalGuard` is intentionally not `Debug` (it wraps process stdout);
        // expose only the type name so this stays loggable.
        f.debug_struct("GuardTeardown").finish_non_exhaustive()
    }
}

impl GuardTeardown {
    /// Take ownership of the terminal guard so the supervisor can sequence its
    /// restore last.
    #[must_use]
    pub fn new(guard: TerminalGuard) -> Self {
        Self { guard }
    }
}

impl FinalTeardown for GuardTeardown {
    fn run(self: Box<Self>) {
        // Dropping the guard runs its `Drop` -> terminal restore (docs/02 §6).
        // Explicit for intent; the drop would happen at scope end regardless.
        drop(self.guard);
    }
}

// ---------------------------------------------------------------------------
// Exit cause + the panic-report seam.
// ---------------------------------------------------------------------------

/// Why the supervised process exited — the value [`Supervisor::run`] returns for
/// `main` to map to a process exit code and an optional post-restore `stderr`
/// line (`docs/02-tui-architecture.md` §12).
///
/// The supervisor never calls [`std::process::exit`] (that would bypass the
/// [`TerminalGuard`]'s `Drop`); it returns this instead.
#[derive(Debug)]
pub enum ExitCause {
    /// A clean shutdown (`q`/`Ctrl-C`, or every task ending on its own) — exit
    /// code 0, no `stderr` line.
    Clean,
    /// A supervised task panicked — fatal, non-zero exit.
    TaskPanicked,
    /// A fatal error (startup failure, a provider failure past its reconnect
    /// budget, a channel close) — non-zero exit.
    Failed(ChainViewError),
}

impl ExitCause {
    /// The process exit code `main` should use: `0` for a clean quit, `1` for any
    /// supervised failure.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            ExitCause::Clean => 0,
            ExitCause::TaskPanicked | ExitCause::Failed(_) => 1,
        }
    }

    /// Whether this is a clean (exit-code-0) shutdown.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self, ExitCause::Clean)
    }

    /// The non-secret line `main` may print to `stderr` **after** the terminal is
    /// restored, or `None` for a clean quit. The error `Display` is already
    /// redaction-safe (`docs/01-domain-model.md` §11).
    #[must_use]
    pub fn failure_message(&self) -> Option<String> {
        match self {
            ExitCause::Clean => None,
            ExitCause::TaskPanicked => {
                Some("chainview: a supervised task panicked; see the log".to_owned())
            }
            ExitCause::Failed(error) => Some(format!("chainview: {error}")),
        }
    }
}

/// A handle a task's join-watcher uses to report the task's terminal outcome to
/// the supervisor (`docs/02-tui-architecture.md` §12).
///
/// The render loop (#13) / reconnect loop (#16) spawn one watcher per task:
/// `let exit = task.join().await; reporter.report(exit);`. A
/// [`TaskExit::Panicked`] report trips the root token as **fatal**; a
/// [`TaskExit::Completed`] report is a clean shutdown trigger. Cheap to `Clone`.
#[derive(Debug, Clone)]
pub struct ExitReporter {
    tx: mpsc::Sender<TaskExit>,
}

impl ExitReporter {
    /// Report a task's terminal outcome, non-blocking. On a full or closed
    /// channel the report is dropped — the supervisor is already shutting down,
    /// so a lost report cannot orphan a task.
    pub fn report(&self, exit: TaskExit) {
        let _ = self.tx.try_send(exit);
    }
}

// ---------------------------------------------------------------------------
// The supervisor.
// ---------------------------------------------------------------------------

/// One supervised task: its cooperative-cancel child token and the joinable
/// handle.
struct Supervised {
    child: CancellationToken,
    task: Box<dyn SupervisedTask>,
}

/// The single task supervisor: owns every task's handle and the root
/// cancellation token, and runs the ordered, terminal-restored-last teardown
/// (`docs/02-tui-architecture.md` §12).
///
/// Built with a [`FinalTeardown`] (production: [`GuardTeardown`]); the render loop
/// (#13) registers the spawned tasks via [`register_provider`](Self::register_provider)
/// / [`register_ancillary`](Self::register_ancillary) / [`set_render`](Self::set_render),
/// each with a [`child_token`](Self::child_token), then calls [`run`](Self::run).
pub struct Supervisor {
    root: CancellationToken,
    providers: Vec<(ProviderId, Supervised)>,
    ancillary: Vec<Supervised>,
    render: Option<Supervised>,
    terminal: Box<dyn FinalTeardown>,
    budget: Duration,
    /// The first fatal cause, recorded first-wins; `None` means a clean quit.
    fatal: Option<ExitCause>,
    rx_exit: mpsc::Receiver<TaskExit>,
    tx_exit: mpsc::Sender<TaskExit>,
    /// The supervisor-owned mid-run watch set: one watcher per handle registered
    /// through [`watch`](Supervisor::watch). Each watcher awaits its task's
    /// terminal outcome and yields a [`TaskExit`], so a task that panics or
    /// returns **mid-run** wakes the `supervise` loop via
    /// [`JoinSet::join_next`], and every watcher is reaped (bounded, then aborted)
    /// at teardown — never a detached orphan.
    watchers: JoinSet<TaskExit>,
    /// The [`AbortHandle`] of each **watched task** (not its watcher). Aborting a
    /// watcher would merely drop the task's [`JoinHandle`] and DETACH the task; to
    /// truly stop a wedged watched task at teardown the drain aborts it through
    /// its own handle here.
    watch_aborts: Vec<AbortHandle>,
}

impl Supervisor {
    /// A supervisor with no tasks yet, the [`DEFAULT_JOIN_BUDGET`], and the given
    /// final teardown (production: a [`GuardTeardown`] owning the
    /// [`TerminalGuard`]).
    #[must_use]
    pub fn new(terminal: Box<dyn FinalTeardown>) -> Self {
        Self::with_join_budget(terminal, DEFAULT_JOIN_BUDGET)
    }

    /// A supervisor with an explicit join budget, so a test can drive a tiny
    /// window or a controllable clock instead of the wall-clock 2 s.
    #[must_use]
    pub fn with_join_budget(terminal: Box<dyn FinalTeardown>, budget: Duration) -> Self {
        let (tx_exit, rx_exit) = mpsc::channel(EXIT_REPORT_CAPACITY);
        Self {
            root: CancellationToken::new(),
            providers: Vec::new(),
            ancillary: Vec::new(),
            render: None,
            terminal,
            budget,
            fatal: None,
            rx_exit,
            tx_exit,
            watchers: JoinSet::new(),
            watch_aborts: Vec::new(),
        }
    }

    /// A clone of the **root** token. The render loop holds this and calls
    /// [`cancel`](CancellationToken::cancel) on a clean quit (`App::should_quit`),
    /// which trips the supervise loop.
    #[must_use]
    pub fn root_token(&self) -> CancellationToken {
        self.root.clone()
    }

    /// A fresh **child** token for one task. The task's loop selects on it and
    /// observes cancellation at the next await point; the supervisor cancels it
    /// just before joining (`docs/02-tui-architecture.md` §12).
    ///
    /// The token handed to a task **must** come from this method (or another
    /// descendant of the root) and be the **same** token passed to that task's
    /// `register_*` call, or the root-cascade breaks. Keep a clone to drive a
    /// **mid-run** per-provider cancel yourself (see
    /// [`cancel_provider`](Self::cancel_provider)); the supervisor's own registry
    /// is consumed by [`run`](Self::run) and cannot be reached mid-run. A
    /// correct-by-construction alternative (mint the child inside `register_*` and
    /// return it) was considered but kept as a doc contract to leave the #12/#13
    /// registration seam a plain `(child, task)` pair.
    #[must_use]
    pub fn child_token(&self) -> CancellationToken {
        self.root.child_token()
    }

    /// A cloneable [`ExitReporter`] for a task's join-watcher, so a **mid-run**
    /// panic (or self-completion) is reported to the supervise loop.
    ///
    /// This is the seam that resolves a structural tension: a
    /// [`JoinHandle`](tokio::task::JoinHandle) is not `Clone` and
    /// [`join`](SupervisedTask::join) consumes it, so one handle cannot be **both**
    /// watched for a mid-run panic **and** registered for the teardown join. The
    /// division of labor is: mid-run panic detection is the **caller's** watcher
    /// over its **own** handle (`let exit = task.join().await;
    /// reporter.report(exit);`), while the supervisor's ordered teardown join is
    /// the **fallback** that reaps a panic surfaced at shutdown for the tasks it
    /// holds. [`watch`](Self::watch) closes the loop for handles the supervisor
    /// owns — `supervise` selects over its watch [`JoinSet`], so a registered
    /// handle that panics or returns mid-run wakes the loop directly, with no
    /// separate reporter needed. This `ExitReporter` seam stays for a
    /// caller-owned watcher (e.g. a reconnect loop, #16) that prefers to report
    /// through the channel instead.
    #[must_use]
    pub fn exit_reporter(&self) -> ExitReporter {
        ExitReporter {
            tx: self.tx_exit.clone(),
        }
    }

    /// Enrol a spawned task's [`JoinHandle`] in the supervisor's **mid-run** watch
    /// set, returning its [`AbortHandle`] for an optional targeted abort.
    ///
    /// This is the seam that closes Codex's "supervisor never notices a task that
    /// panics or returns mid-run" gap: the supervisor spawns a watcher over
    /// `handle` into its owned [`JoinSet`], and the `supervise` loop
    /// selects on that set — so a Deribit provider loop that PANICS or RETURNS
    /// mid-run wakes it (a panic is fatal; a self-completion is a
    /// clean shutdown trigger) instead of leaving the UI stale forever. A watched
    /// handle is single-owned by the watcher (a [`JoinHandle`] is not `Clone` and
    /// awaiting consumes it), and is reaped at teardown by the bounded watch drain
    /// (`docs/02-tui-architecture.md` §12) — never a detached orphan.
    ///
    /// The registration path (#22, `spawn_supervised_subscription`) registers a
    /// **provider** task INTO this: it keeps a clone of the task's
    /// [`child_token`](Self::child_token) for a per-provider `Unsubscribe`/
    /// `Rediscover` cancel, and hands the task's `JoinHandle` here so the mid-run
    /// panic/return is observed. Watched tasks are the data producers, so the
    /// teardown drains them **first** (before the ancillary and render groups),
    /// keeping the "stop new data first" order.
    ///
    /// Must be called from within a tokio runtime (it spawns the watcher).
    pub fn watch(&mut self, handle: JoinHandle<()>) -> AbortHandle {
        // The WATCHED task's own abort handle (not the watcher's): aborting the
        // watcher would only drop this `JoinHandle` and detach the task. Kept so
        // the teardown drain can truly stop a wedged watched task, and returned so
        // the caller (#22) can drive a targeted abort.
        let task_abort = handle.abort_handle();
        self.watch_aborts.push(task_abort.clone());
        self.watchers.spawn(async move {
            match handle.await {
                Ok(()) => TaskExit::Completed,
                // A panic is the one fatal join outcome (docs/02 §12); it surfaces
                // here as the watcher's returned value, not a `JoinError`.
                Err(error) if error.is_panic() => TaskExit::Panicked,
                // A cancelled/aborted task is gone, not a failure.
                Err(_) => TaskExit::Completed,
            }
        });
        task_abort
    }

    /// Register a **provider** task under its id (joined first). `child` **must**
    /// be a token from [`child_token`](Self::child_token) that the task selects
    /// on, and the caller **must** keep a clone of it to drive a mid-run
    /// per-provider cancel (see [`cancel_provider`](Self::cancel_provider)).
    pub fn register_provider(
        &mut self,
        id: ProviderId,
        child: CancellationToken,
        task: Box<dyn SupervisedTask>,
    ) {
        self.providers.push((id, Supervised { child, task }));
    }

    /// Register an **input/tick/replay** task (joined after the providers).
    /// `child` **must** come from [`child_token`](Self::child_token).
    pub fn register_ancillary(&mut self, child: CancellationToken, task: Box<dyn SupervisedTask>) {
        self.ancillary.push(Supervised { child, task });
    }

    /// Register the **render** task (joined after the ancillary tasks, before the
    /// terminal restore). At most one; a second call replaces the first. `child`
    /// **must** come from [`child_token`](Self::child_token).
    pub fn set_render(&mut self, child: CancellationToken, task: Box<dyn SupervisedTask>) {
        self.render = Some(Supervised { child, task });
    }

    /// Cancel a **single** provider's child token without touching the others or
    /// the root, returning whether a provider with that id was registered
    /// (`docs/02-tui-architecture.md` §12).
    ///
    /// **Pre-startup convenience only.** [`run`](Self::run) takes `self` by value,
    /// so once the supervise loop is running this method is unreachable. The
    /// documented mid-run use — a per-`(underlying, expiration)` `Unsubscribe` or a
    /// `Rediscover` — is therefore driven **not** through here but through a
    /// **caller-held clone** of the provider's [`child_token`](Self::child_token)
    /// (the render loop keeps one per provider and calls `cancel` on it), which
    /// cancels exactly that provider's subtree without touching the others; #13
    /// owns wiring that per-provider cancel path (a control message or a held
    /// token). This method stays for building/adjusting the registry before
    /// `run`, and for tests.
    pub fn cancel_provider(&self, id: &ProviderId) -> bool {
        for (provider_id, supervised) in &self.providers {
            if provider_id == id {
                supervised.child.cancel();
                return true;
            }
        }
        false
    }

    /// Request a **clean** quit: trip the root token (cascading to every child)
    /// without recording a fatal cause, so [`run`](Self::run) returns
    /// [`ExitCause::Clean`]. Wired from `App::should_quit` (`q`/`Ctrl-C`).
    pub fn request_quit(&self) {
        self.root.cancel();
    }

    /// Record a **fatal** error (startup failure, a provider failure past its
    /// reconnect budget, a channel close) as the exit cause and trip the root
    /// token. First-wins: a later fatal never overwrites the first.
    pub fn fail(&mut self, error: ChainViewError) {
        self.trip_fatal(ExitCause::Failed(error));
    }

    /// Record `cause` as the fatal exit cause (first-wins) and trip the root
    /// token so every child observes cancellation.
    fn trip_fatal(&mut self, cause: ExitCause) {
        if self.fatal.is_none() {
            self.fatal = Some(cause);
        }
        self.root.cancel();
    }

    /// Supervise until the first shutdown trigger, then run the ordered,
    /// terminal-restored-last teardown, returning the recorded [`ExitCause`]
    /// (`docs/02-tui-architecture.md` §12).
    ///
    /// This is the process's shutdown seam: `main` awaits it, maps the returned
    /// cause to a process exit code **after** the terminal is restored, and never
    /// lets the supervisor call [`std::process::exit`].
    #[must_use = "the returned ExitCause is main's exit code + post-restore stderr line"]
    pub async fn run(mut self) -> ExitCause {
        // Claim single ownership of the terminal restore for the whole supervised
        // lifecycle. While this is set, the panic hook DEFERS its own restore to
        // the supervisor (docs/02 §12), which cancels + joins the render task and
        // restores LAST — so a worker-task panic can never restore the terminal
        // out from under a still-live render draw (the two-restore-owners race).
        crate::terminal::set_supervisor_owns_restore(true);
        self.supervise().await;
        self.teardown().await
    }

    /// Await the first shutdown trigger: the root token tripped externally
    /// (`request_quit`/`fail`, or the render loop cancelling its `root_token`
    /// clone), or a task-exit report (a panic is fatal; a clean completion is a
    /// clean shutdown).
    ///
    /// Mid-run detection arrives two ways, both handled here: a watcher the
    /// supervisor owns for a handle registered through [`watch`](Self::watch)
    /// (the [`JoinSet`] arm — a handle that panics or returns mid-run wakes this
    /// loop directly), and an [`ExitReporter`] **report** from a caller-owned
    /// watcher (the channel arm — see [`exit_reporter`](Self::exit_reporter)).
    /// Either way, a panic is fatal and a self-completion is a clean shutdown.
    async fn supervise(&mut self) {
        // A cloned (owned) future so the select borrows neither `self.root` nor
        // entangles with the mutable `rx_exit`/`watchers` borrows below.
        let root = self.root.clone();
        tokio::select! {
            () = root.cancelled_owned() => {}
            report = self.rx_exit.recv() => match report {
                Some(TaskExit::Panicked) => self.trip_fatal(ExitCause::TaskPanicked),
                // A task completing on its own, or every reporter dropping, is a
                // clean shutdown trigger.
                Some(TaskExit::Completed) | None => self.request_quit(),
            },
            // A supervisor-owned watched handle finishing mid-run. The `if` guard
            // disables this arm while the set is empty (an empty `JoinSet` yields
            // `None` immediately, which must not spuriously trip a shutdown).
            watched = self.watchers.join_next(), if !self.watchers.is_empty() => match watched {
                Some(Ok(TaskExit::Panicked)) => self.trip_fatal(ExitCause::TaskPanicked),
                // A clean self-completion, an aborted watcher, or the set draining
                // to empty is a clean shutdown trigger — no task left unobserved.
                Some(Ok(TaskExit::Completed)) | Some(Err(_)) | None => self.request_quit(),
            },
        }
    }

    /// Run the deterministic teardown and return the exit cause
    /// (`docs/02-tui-architecture.md` §12): (1) providers, (2) input/tick/replay,
    /// (3) render, (4) terminal restore **last**. Each join is bounded then
    /// aborted; a panic discovered at any join is recorded as the fatal cause
    /// (first-wins) even when the trigger was a clean quit.
    async fn teardown(mut self) -> ExitCause {
        let budget = self.budget;
        // Ensure every child observes cancellation before we join (idempotent if
        // a trigger already tripped the root).
        self.root.cancel();

        // The fatal cause is threaded through a local (not `self`) so the group
        // loops can borrow the task vectors mutably without a disjoint-field
        // dance; it is seeded with any cause a trigger already recorded.
        let mut fatal = self.fatal.take();

        // (0) Mid-run watched handles (the provider/producer tasks registered via
        // `watch`, #22): drained FIRST so new data stops before anything else, and
        // aborted-then-AWAITED so no watched task outlives the terminal restore.
        drain_watchers(&mut self.watchers, &self.watch_aborts, budget, &mut fatal).await;

        // (1) Provider tasks: stop new data first.
        for (_id, supervised) in &mut self.providers {
            let outcome = join_supervised(supervised, budget).await;
            record_group_outcome(&mut fatal, outcome);
        }
        // (2) Input / tick / replay tasks.
        for supervised in &mut self.ancillary {
            let outcome = join_supervised(supervised, budget).await;
            record_group_outcome(&mut fatal, outcome);
        }
        // (3) Render thread: let it exit its loop.
        if let Some(render) = self.render.as_mut() {
            let outcome = join_supervised(render, budget).await;
            record_group_outcome(&mut fatal, outcome);
        }

        // (4) Terminal restore — the LAST step on every path, and the SINGLE
        // owner: the panic hook has been deferring to us since `run` claimed
        // ownership, so no restore raced a live draw. Moved out of `self` so
        // `run` on the boxed teardown consumes it exactly once.
        let terminal = self.terminal;
        terminal.run();
        // Ownership done: a post-teardown panic (if any) falls back to the panic
        // hook's own restore.
        crate::terminal::set_supervisor_owns_restore(false);

        fatal.unwrap_or(ExitCause::Clean)
    }
}

/// Cancel a task's child token, then join it within the budget (aborting on
/// timeout). Cancelling immediately before the join is what lets a cooperative
/// task return well inside the budget.
async fn join_supervised(supervised: &mut Supervised, budget: Duration) -> JoinBudget {
    supervised.child.cancel();
    join_bounded(supervised.task.as_mut(), budget).await
}

/// Join `task` within `budget`; on timeout, [`abort`](SupervisedTask::abort) it
/// so a wedged upstream socket can never hang the exit
/// (`docs/02-tui-architecture.md` §12).
///
/// The timeout uses [`tokio::time`], so under a paused test clock the budget is
/// honored in **virtual** time with zero real wall-clock wait.
async fn join_bounded(task: &mut dyn SupervisedTask, budget: Duration) -> JoinBudget {
    match tokio::time::timeout(budget, task.join()).await {
        Ok(exit) => JoinBudget::Returned(exit),
        Err(_elapsed) => {
            // `timeout` has dropped the inner `join` future (releasing its `&mut
            // task` borrow) by the time it yields `Err`. Because [`TokioTask::join`]
            // awaits `&mut JoinHandle` and keeps the handle IN `self`, that drop
            // does not detach the task — so this `abort()` reaches a live handle
            // and truly cancels it, and a wedged upstream socket cannot orphan.
            task.abort();
            // AWAIT the aborted task so it is truly finished before the caller
            // proceeds to the terminal restore — an aborted-but-not-joined task
            // could still be running when the terminal is handed back, racing the
            // restore. Bounded again by the SAME budget so a task that somehow
            // ignores the abort still cannot hang the exit: on a second timeout we
            // simply proceed, the abort having already been issued.
            let _ = tokio::time::timeout(budget, task.join()).await;
            JoinBudget::AbortedAfterBudget
        }
    }
}

/// Drain the mid-run watch set within the budget, folding any panic into the
/// first-fatal-wins cause. A straggler whose task ignores cancellation past the
/// budget has its WATCHED task aborted (through `watch_aborts`) and is then
/// AWAITED, so no watched task outlives the terminal restore
/// (`docs/02-tui-architecture.md` §12).
///
/// The root token is cancelled before this runs (cascading to every watched
/// task's child), so a cooperative watcher resolves well inside the budget.
async fn drain_watchers(
    watchers: &mut JoinSet<TaskExit>,
    watch_aborts: &[AbortHandle],
    budget: Duration,
    fatal: &mut Option<ExitCause>,
) {
    while !watchers.is_empty() {
        match tokio::time::timeout(budget, watchers.join_next()).await {
            Ok(Some(Ok(exit))) => record_group_outcome(fatal, JoinBudget::Returned(exit)),
            // A cancelled/aborted watcher is gone, not a failure; the watcher maps
            // the WATCHED task's panic to `TaskExit::Panicked` as a value, so a
            // panic never arrives here as a `JoinError`.
            Ok(Some(Err(_))) => {}
            // Set drained to empty (a race with the loop guard).
            Ok(None) => break,
            Err(_elapsed) => {
                // A watched task ignored cancellation past the budget: abort the
                // WATCHED tasks (not the watchers — aborting a watcher would only
                // detach its task), then AWAIT every watcher so nothing races the
                // restore. Bounded so even that final drain cannot hang the exit.
                for task_abort in watch_aborts {
                    task_abort.abort();
                }
                let _ = tokio::time::timeout(budget, async {
                    while watchers.join_next().await.is_some() {}
                })
                .await;
                break;
            }
        }
    }
}

/// The outcome of a bounded join: the task returned in time, or it blew the
/// budget and was aborted.
enum JoinBudget {
    Returned(TaskExit),
    AbortedAfterBudget,
}

/// Fold one group member's join outcome into the first-fatal-wins cause: a
/// panic recorded at any join is fatal even when the trigger was a clean quit.
fn record_group_outcome(fatal: &mut Option<ExitCause>, outcome: JoinBudget) {
    if let JoinBudget::Returned(TaskExit::Panicked) = outcome {
        if fatal.is_none() {
            *fatal = Some(ExitCause::TaskPanicked);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        ExitCause, FinalTeardown, JoinBudget, SupervisedTask, Supervisor, TaskExit, TokioTask,
        join_bounded,
    };
    use crate::chain::ProviderId;
    use crate::error::ChainViewError;

    // --- A recording log shared by the mock tasks and the mock teardown -------

    type Log = Arc<Mutex<Vec<String>>>;

    fn new_log() -> Log {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// Push a step onto the log without holding the lock across any await (the
    /// callers lock, push, and drop synchronously).
    fn record(log: &Log, step: String) {
        if let Ok(mut guard) = log.lock() {
            guard.push(step);
        }
    }

    fn steps(log: &Log) -> Vec<String> {
        match log.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => Vec::new(),
        }
    }

    #[track_caller]
    fn position(steps: &[String], target: &str) -> usize {
        match steps.iter().position(|step| step == target) {
            Some(index) => index,
            None => panic!("missing step `{target}` in {steps:?}"),
        }
    }

    fn has_step(steps: &[String], target: &str) -> bool {
        steps.iter().any(|step| step == target)
    }

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(provider) => provider,
            Err(error) => panic!("expected a valid provider id `{id}`, got: {error}"),
        }
    }

    // --- The mock task + teardown --------------------------------------------

    enum Behavior {
        /// Returns once its child token is cancelled (records `join:<label>`).
        Cooperative,
        /// Never returns — models a wedged upstream socket.
        Wedged,
    }

    struct MockTask {
        label: String,
        child: tokio_util::sync::CancellationToken,
        behavior: Behavior,
        exit: TaskExit,
        log: Log,
    }

    impl MockTask {
        fn cooperative(label: &str, child: tokio_util::sync::CancellationToken, log: Log) -> Self {
            Self {
                label: label.to_owned(),
                child,
                behavior: Behavior::Cooperative,
                exit: TaskExit::Completed,
                log,
            }
        }

        fn panicking(label: &str, child: tokio_util::sync::CancellationToken, log: Log) -> Self {
            Self {
                label: label.to_owned(),
                child,
                behavior: Behavior::Cooperative,
                exit: TaskExit::Panicked,
                log,
            }
        }

        fn wedged(label: &str, child: tokio_util::sync::CancellationToken, log: Log) -> Self {
            Self {
                label: label.to_owned(),
                child,
                behavior: Behavior::Wedged,
                exit: TaskExit::Completed,
                log,
            }
        }
    }

    #[async_trait::async_trait]
    impl SupervisedTask for MockTask {
        async fn join(&mut self) -> TaskExit {
            match self.behavior {
                Behavior::Cooperative => {
                    // Observe cancellation FIRST, so a recorded `join` proves the
                    // child token was cancelled before the join returned.
                    self.child.cancelled().await;
                    record(&self.log, format!("join:{}", self.label));
                    self.exit
                }
                Behavior::Wedged => std::future::pending::<TaskExit>().await,
            }
        }

        fn abort(&mut self) {
            record(&self.log, format!("abort:{}", self.label));
        }
    }

    struct RecordingTeardown {
        log: Log,
    }

    impl FinalTeardown for RecordingTeardown {
        fn run(self: Box<Self>) {
            record(&self.log, "terminal_restore".to_owned());
        }
    }

    fn recording_supervisor(log: &Log) -> Supervisor {
        Supervisor::new(Box::new(RecordingTeardown { log: log.clone() }))
    }

    // === ExitCause ============================================================

    #[test]
    fn test_exit_cause_exit_code_is_zero_for_clean_nonzero_for_failure() {
        assert_eq!(ExitCause::Clean.exit_code(), 0);
        assert_eq!(ExitCause::TaskPanicked.exit_code(), 1);
        let failed = ExitCause::Failed(ChainViewError::Terminal("boom".to_owned()));
        assert_eq!(failed.exit_code(), 1);
    }

    #[test]
    fn test_exit_cause_failure_message_absent_for_clean_present_for_failure() {
        assert!(ExitCause::Clean.failure_message().is_none());
        assert!(ExitCause::TaskPanicked.failure_message().is_some());
        let failed = ExitCause::Failed(ChainViewError::Terminal("boom".to_owned()));
        assert!(
            failed
                .failure_message()
                .is_some_and(|message| message.contains("boom"))
        );
    }

    // === Cancellation-token tree ==============================================

    #[test]
    fn test_supervisor_request_quit_cascades_to_children_without_fatal() {
        let log = new_log();
        let supervisor = recording_supervisor(&log);
        let child = supervisor.child_token();
        supervisor.request_quit();
        assert!(supervisor.root_token().is_cancelled());
        assert!(
            child.is_cancelled(),
            "cancelling the root cascades to a child"
        );
    }

    #[tokio::test]
    async fn test_supervisor_cancel_provider_cancels_only_that_child() {
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let first = supervisor.child_token();
        supervisor.register_provider(
            pid("deribit"),
            first.clone(),
            Box::new(MockTask::cooperative("deribit", first.clone(), log.clone())),
        );
        let second = supervisor.child_token();
        supervisor.register_provider(
            pid("dxlink"),
            second.clone(),
            Box::new(MockTask::cooperative("dxlink", second.clone(), log.clone())),
        );

        assert!(supervisor.cancel_provider(&pid("deribit")));
        assert!(first.is_cancelled(), "the targeted provider is cancelled");
        assert!(!second.is_cancelled(), "the other provider is untouched");
        assert!(
            !supervisor.root_token().is_cancelled(),
            "a per-provider cancel must not trip the root"
        );
        assert!(
            !supervisor.cancel_provider(&pid("alpaca")),
            "an unregistered provider id reports not-found"
        );
        // Cancel the remaining task so `run` can join cleanly and drop cleanly.
        supervisor.request_quit();
        let _ = supervisor.run().await;
    }

    // === Bounded join then abort (paused virtual clock, no real wait) =========

    #[tokio::test(start_paused = true)]
    async fn test_join_bounded_wedged_task_is_aborted_after_budget() {
        let log = new_log();
        let child = tokio_util::sync::CancellationToken::new();
        let mut task = MockTask::wedged("provider", child, log.clone());

        let start = tokio::time::Instant::now();
        let outcome = join_bounded(&mut task, super::DEFAULT_JOIN_BUDGET).await;
        let elapsed = start.elapsed();

        assert!(
            matches!(outcome, JoinBudget::AbortedAfterBudget),
            "a task ignoring cancellation past the budget is aborted"
        );
        assert!(
            elapsed >= super::DEFAULT_JOIN_BUDGET,
            "the budget was honored in virtual time (zero real wait): {elapsed:?}"
        );
        assert!(has_step(&steps(&log), "abort:provider"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_join_bounded_cooperative_task_returns_within_budget_without_abort() {
        let log = new_log();
        let child = tokio_util::sync::CancellationToken::new();
        // Cancel first, so the cooperative join returns immediately.
        child.cancel();
        let mut task = MockTask::cooperative("provider", child, log.clone());

        let outcome = join_bounded(&mut task, super::DEFAULT_JOIN_BUDGET).await;

        assert!(matches!(outcome, JoinBudget::Returned(TaskExit::Completed)));
        let recorded = steps(&log);
        assert!(has_step(&recorded, "join:provider"));
        assert!(
            !recorded.iter().any(|step| step.starts_with("abort:")),
            "a task that returns in time is never aborted"
        );
    }

    /// Sets its flag on `Drop`, so a spawned task carrying it can prove whether
    /// its future was **dropped** (aborted/cancelled) versus left running.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_tokio_task_bounded_join_wedged_real_task_is_aborted_not_detached() {
        // A REAL wedged `TokioTask` (not the mock, which can never exhibit the
        // detach bug). It never completes on its own and carries a drop-guard that
        // fires ONLY if its future is dropped — i.e. it was truly aborted, not
        // detached as an orphan. This test FAILS against a `take()`-based `join`
        // (the timeout would drop the owned handle and DETACH the task, so
        // `abort()` no-ops, `dropped` stays false, and `is_finished()` is false).
        let completed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let ran = completed.clone();
        let handle = tokio::spawn(async move {
            // Hold the drop-guard across the wedge: it drops iff the task future
            // is dropped (aborted), never if the task is merely detached.
            let _guard = guard;
            std::future::pending::<()>().await;
            ran.store(true, Ordering::SeqCst);
        });
        let abort_handle = handle.abort_handle();
        let mut task = TokioTask::new(handle);

        let outcome = join_bounded(&mut task, super::DEFAULT_JOIN_BUDGET).await;
        assert!(matches!(outcome, JoinBudget::AbortedAfterBudget));

        // Let the current-thread runtime process the queued abort (drop the task
        // future). A few yields is deterministic and adds no real wall-clock wait.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        assert!(
            !completed.load(Ordering::SeqCst),
            "the wedged task must never run to completion"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "the real task was TRULY aborted (its future dropped), not detached as an orphan"
        );
        assert!(
            abort_handle.is_finished(),
            "the real task handle reports finished after a genuine abort"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_join_bounded_awaits_aborted_task_before_returning() {
        // Fix 2: a REAL wedged task carrying a drop-guard. `join_bounded` AWAITS
        // the handle AFTER aborting, so the task is truly finished (its future
        // dropped) by the time `join_bounded` returns — with NO extra yields,
        // unlike a fire-and-forget abort that could still be running when the
        // caller proceeds to the terminal restore.
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let mut task = TokioTask::new(handle);

        let outcome = join_bounded(&mut task, super::DEFAULT_JOIN_BUDGET).await;

        assert!(matches!(outcome, JoinBudget::AbortedAfterBudget));
        assert!(
            dropped.load(Ordering::SeqCst),
            "the aborted task was AWAITED to completion before join_bounded returned"
        );
    }

    // === Ordered teardown =====================================================

    #[tokio::test]
    async fn test_supervisor_teardown_joins_providers_before_ancillary_then_render_then_restore() {
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let provider_child = supervisor.child_token();
        supervisor.register_provider(
            pid("deribit"),
            provider_child.clone(),
            Box::new(MockTask::cooperative(
                "provider",
                provider_child,
                log.clone(),
            )),
        );
        let input_child = supervisor.child_token();
        supervisor.register_ancillary(
            input_child.clone(),
            Box::new(MockTask::cooperative("input", input_child, log.clone())),
        );
        let render_child = supervisor.child_token();
        supervisor.set_render(
            render_child.clone(),
            Box::new(MockTask::cooperative("render", render_child, log.clone())),
        );

        supervisor.request_quit();
        let cause = supervisor.run().await;

        assert!(cause.is_clean());
        let recorded = steps(&log);
        let provider = position(&recorded, "join:provider");
        let input = position(&recorded, "join:input");
        let render = position(&recorded, "join:render");
        let restore = position(&recorded, "terminal_restore");
        assert!(
            provider < input && input < render && render < restore,
            "order must be provider -> ancillary -> render -> terminal restore: {recorded:?}"
        );
    }

    // === Normal quit ==========================================================

    #[tokio::test]
    async fn test_supervisor_run_normal_quit_joins_every_task_and_restores_terminal() {
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let provider_child = supervisor.child_token();
        supervisor.register_provider(
            pid("deribit"),
            provider_child.clone(),
            Box::new(MockTask::cooperative(
                "provider",
                provider_child,
                log.clone(),
            )),
        );
        let tick_child = supervisor.child_token();
        supervisor.register_ancillary(
            tick_child.clone(),
            Box::new(MockTask::cooperative("tick", tick_child, log.clone())),
        );

        supervisor.request_quit();
        let cause = supervisor.run().await;

        assert!(cause.is_clean(), "a normal quit exits clean");
        let recorded = steps(&log);
        assert!(has_step(&recorded, "join:provider"), "the provider joined");
        assert!(has_step(&recorded, "join:tick"), "the tick task joined");
        assert_eq!(
            recorded.last().map(String::as_str),
            Some("terminal_restore"),
            "terminal restore is the last step: {recorded:?}"
        );
    }

    // === Failed task: a panic report trips the root, non-zero exit ============

    #[tokio::test]
    async fn test_supervisor_run_reported_panic_exits_nonzero_and_restores_terminal_last() {
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let provider_child = supervisor.child_token();
        supervisor.register_provider(
            pid("deribit"),
            provider_child.clone(),
            Box::new(MockTask::panicking("provider", provider_child, log.clone())),
        );
        let input_child = supervisor.child_token();
        supervisor.register_ancillary(
            input_child.clone(),
            Box::new(MockTask::cooperative("input", input_child, log.clone())),
        );

        // A panic reported through the join-watcher seam trips the root as fatal.
        supervisor.exit_reporter().report(TaskExit::Panicked);
        let cause = supervisor.run().await;

        assert!(matches!(cause, ExitCause::TaskPanicked));
        assert_eq!(cause.exit_code(), 1, "a supervised panic exits non-zero");
        let recorded = steps(&log);
        assert!(
            has_step(&recorded, "join:input"),
            "the other task still joined — no orphan: {recorded:?}"
        );
        assert_eq!(
            recorded.last().map(String::as_str),
            Some("terminal_restore"),
            "terminal restore is last even on a supervised failure: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn test_supervisor_teardown_panic_at_join_records_fatal_over_clean_trigger() {
        // The trigger is a CLEAN quit, but a task turns out to have panicked at
        // its join: the first fatal is recorded, so the exit is non-zero.
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let provider_child = supervisor.child_token();
        supervisor.register_provider(
            pid("deribit"),
            provider_child.clone(),
            Box::new(MockTask::panicking("provider", provider_child, log.clone())),
        );

        supervisor.request_quit();
        let cause = supervisor.run().await;

        assert!(
            matches!(cause, ExitCause::TaskPanicked),
            "a panic discovered at join is fatal even under a clean trigger"
        );
        assert_eq!(
            steps(&log).last().map(String::as_str),
            Some("terminal_restore")
        );
    }

    #[tokio::test]
    async fn test_supervisor_fail_records_first_fatal_only() {
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        supervisor.fail(ChainViewError::Terminal("first".to_owned()));
        supervisor.fail(ChainViewError::Terminal("second".to_owned()));

        let cause = supervisor.run().await;

        match cause {
            ExitCause::Failed(error) => assert!(
                error.to_string().contains("first"),
                "the FIRST fatal cause is the recorded exit cause, got: {error}"
            ),
            other => panic!("expected the first Failed cause, got {other:?}"),
        }
    }

    // === Wedged task under a full run: aborted, exit still clean ==============

    #[tokio::test(start_paused = true)]
    async fn test_supervisor_run_wedged_provider_is_aborted_and_exit_stays_clean() {
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let provider_child = supervisor.child_token();
        supervisor.register_provider(
            pid("deribit"),
            provider_child.clone(),
            Box::new(MockTask::wedged("provider", provider_child, log.clone())),
        );

        supervisor.request_quit();
        let cause = supervisor.run().await;

        assert!(
            cause.is_clean(),
            "a wedged (non-panicking) task is aborted but does not fail the exit"
        );
        let recorded = steps(&log);
        assert!(
            has_step(&recorded, "abort:provider"),
            "the wedged task was aborted so the exit cannot hang: {recorded:?}"
        );
        assert_eq!(
            recorded.last().map(String::as_str),
            Some("terminal_restore"),
            "terminal restore is still last after an abort: {recorded:?}"
        );
    }

    // === Mid-run watch: a registered handle that ends on its own wakes the loop =

    #[tokio::test]
    async fn test_supervisor_watched_task_panic_mid_run_wakes_supervise_as_fatal() {
        // A REAL task that panics on its own, MID-RUN, with NO external trigger
        // and NO manual reporter: `watch` is the only thing observing it. Its
        // panic must wake `supervise` and record a fatal cause — closing the
        // "supervisor never notices a task that panics mid-run" gap (the UI would
        // otherwise run stale forever).
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let handle = tokio::spawn(async { panic!("mid-run provider panic") });
        let _abort = supervisor.watch(handle);

        let cause = supervisor.run().await;

        assert!(
            matches!(cause, ExitCause::TaskPanicked),
            "a watched task panicking mid-run wakes the supervisor as fatal"
        );
        assert_eq!(
            steps(&log).last().map(String::as_str),
            Some("terminal_restore"),
            "terminal restore is still last on the watched-panic path"
        );
    }

    #[tokio::test]
    async fn test_supervisor_watched_task_return_mid_run_triggers_clean_shutdown() {
        // A REAL task that RETURNS on its own, mid-run: a clean self-completion
        // that must wake `supervise` and trigger an ordered, clean shutdown
        // (not a stale UI).
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let handle = tokio::spawn(async {});
        let _abort = supervisor.watch(handle);

        let cause = supervisor.run().await;

        assert!(
            cause.is_clean(),
            "a watched task self-completing mid-run is a clean shutdown trigger"
        );
        assert_eq!(
            steps(&log).last().map(String::as_str),
            Some("terminal_restore")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_supervisor_watched_wedged_task_is_aborted_at_teardown() {
        // A REAL watched task that ignores cancellation (wedged) is aborted AND
        // AWAITED by the teardown drain, so it cannot outlive the terminal
        // restore. A drop-guard proves the task future was truly dropped.
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let _abort = supervisor.watch(handle);

        // A clean quit trigger so `supervise` returns and teardown drains the
        // wedged watcher within the budget, then aborts + awaits it.
        supervisor.request_quit();
        let cause = supervisor.run().await;

        assert!(
            cause.is_clean(),
            "a wedged (non-panicking) watched task is aborted but does not fail the exit"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "the wedged watched task was aborted AND awaited before restore"
        );
        assert_eq!(
            steps(&log).last().map(String::as_str),
            Some("terminal_restore")
        );
    }

    // === Single restore owner under a worker panic ============================

    #[tokio::test]
    async fn test_supervisor_worker_panic_restores_terminal_exactly_once() {
        // Single restore owner: under a worker-task panic the supervisor performs
        // the ONE terminal restore (the panic hook defers to it while supervised,
        // see `terminal::should_hook_restore`), so restore is recorded exactly
        // once and is the last step — no second owner racing it.
        let log = new_log();
        let mut supervisor = recording_supervisor(&log);
        let handle = tokio::spawn(async { panic!("worker panic") });
        let _abort = supervisor.watch(handle);

        let cause = supervisor.run().await;

        assert!(matches!(cause, ExitCause::TaskPanicked));
        let recorded = steps(&log);
        assert_eq!(
            recorded
                .iter()
                .filter(|step| step.as_str() == "terminal_restore")
                .count(),
            1,
            "the terminal is restored by a single owner (exactly once): {recorded:?}"
        );
        assert_eq!(
            recorded.last().map(String::as_str),
            Some("terminal_restore"),
            "the single restore is the last step: {recorded:?}"
        );
    }
}
