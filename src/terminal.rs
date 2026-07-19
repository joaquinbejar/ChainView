//! Terminal lifecycle: the RAII [`TerminalGuard`] and the panic-hook restore.
//!
//! The terminal is a shared resource the process borrows and must return on
//! **every** exit path — a clean return, an early `?`, or a panic
//! (`docs/02-tui-architecture.md` §6, ADR-0001). Setup enables raw mode, enters
//! the alternate screen, and hides the cursor; teardown runs the exact inverse
//! and is driven by [`Drop`], so the shell is restored even when the caller
//! forgets. [`install_panic_hook`] adds a hook that restores the terminal
//! **before** the chained previous hook prints, so a backtrace never lands on a
//! raw-mode screen and is never swallowed.
//!
//! Restore is best-effort and never panics: a partially-initialized or an
//! already-restored guard tears down cleanly (idempotent and tolerant). The
//! low-level operations are abstracted over the crate-internal `TerminalOps`
//! trait so the restore ordering and idempotency are unit-testable **without a
//! real TTY** — the deterministic sequencing proofs live in this module's
//! `#[cfg(test)]` block, and the real end-to-end panic path is exercised by the
//! subprocess harness in `tests/terminal_restore.rs` (`docs/TESTING.md` §7).

use std::io::{self, Stdout};
use std::panic;

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::error::ChainViewError;

/// The low-level terminal operations the guard drives, in setup order:
/// enable raw mode, enter the alternate screen, hide the cursor — and their
/// inverses for teardown.
///
/// Abstracting these behind a trait lets the guard's restore ordering,
/// idempotency, and partial-setup tolerance be asserted deterministically
/// against a recording fake, with no real terminal attached. Production uses
/// [`CrosstermOps`]; tests use an in-memory recorder.
pub(crate) trait TerminalOps {
    /// Put the terminal into raw mode.
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    /// Return the terminal from raw mode to cooked mode.
    fn disable_raw_mode(&mut self) -> io::Result<()>;
    /// Switch to the alternate screen buffer.
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    /// Return to the primary screen buffer.
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    /// Hide the cursor.
    fn hide_cursor(&mut self) -> io::Result<()>;
    /// Show the cursor.
    fn show_cursor(&mut self) -> io::Result<()>;
}

/// The production [`TerminalOps`], driving `crossterm` over the process stdout.
///
/// A fresh [`Stdout`] handle is acquired per call (each is a cheap clone of the
/// global handle) so teardown holds no long-lived borrow and stays allocation-
/// light for the panic path.
pub(crate) struct CrosstermOps;

impl TerminalOps for CrosstermOps {
    #[inline]
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    #[inline]
    fn disable_raw_mode(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }

    #[inline]
    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        let mut out: Stdout = io::stdout();
        execute!(out, EnterAlternateScreen)
    }

    #[inline]
    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        let mut out: Stdout = io::stdout();
        execute!(out, LeaveAlternateScreen)
    }

    #[inline]
    fn hide_cursor(&mut self) -> io::Result<()> {
        let mut out: Stdout = io::stdout();
        execute!(out, Hide)
    }

    #[inline]
    fn show_cursor(&mut self) -> io::Result<()> {
        let mut out: Stdout = io::stdout();
        execute!(out, Show)
    }
}

/// Map a terminal-backend I/O failure into the shared boundary error.
///
/// The message is a non-secret `crossterm`/`io` string — terminal operations
/// never touch a credential (`docs/01-domain-model.md` §11).
#[cold]
#[inline(never)]
fn terminal_error(err: io::Error) -> ChainViewError {
    ChainViewError::Terminal(err.to_string())
}

/// The generic guard core: owns the backend and the record of which setup steps
/// are currently applied, so teardown undoes exactly those, in inverse order,
/// at most once.
///
/// Generic over [`TerminalOps`] purely for testability; the public surface is
/// the concrete [`TerminalGuard`] below.
pub(crate) struct Guard<O: TerminalOps> {
    ops: O,
    raw_enabled: bool,
    alt_screen: bool,
    cursor_hidden: bool,
    restored: bool,
}

impl<O: TerminalOps> Guard<O> {
    /// Run the setup sequence, rolling back any partial progress on failure so a
    /// rejected setup leaves the terminal clean and returns the failure.
    fn new(ops: O) -> Result<Self, ChainViewError> {
        let mut guard = Self {
            ops,
            raw_enabled: false,
            alt_screen: false,
            cursor_hidden: false,
            restored: false,
        };
        if let Err(err) = guard.enter() {
            // Undo whatever succeeded before the failing step, then surface the
            // error. The subsequent `Drop` sees `restored == true` and is a
            // no-op, so setup failure never double-tears-down.
            guard.restore();
            return Err(err);
        }
        Ok(guard)
    }

    /// Apply the setup steps in order, recording each success so a mid-sequence
    /// failure rolls back precisely the applied prefix.
    fn enter(&mut self) -> Result<(), ChainViewError> {
        self.ops.enable_raw_mode().map_err(terminal_error)?;
        self.raw_enabled = true;
        self.ops.enter_alternate_screen().map_err(terminal_error)?;
        self.alt_screen = true;
        self.ops.hide_cursor().map_err(terminal_error)?;
        self.cursor_hidden = true;
        Ok(())
    }

    /// Restore the terminal: undo the applied setup steps in inverse order, at
    /// most once.
    ///
    /// Best-effort and infallible — a backend error on one step is ignored so
    /// the remaining steps still run and `Drop` never panics. (`Drop` cannot
    /// propagate an error, and stdout must not carry a failure while the TUI
    /// owns it; the tracing WARN sink lands with the supervisor in #11.)
    fn restore(&mut self) {
        if self.restored {
            return;
        }
        // Continue through every step even if one fails, but clear a state flag
        // ONLY when its inverse actually returned `Ok` — a failed step leaves its
        // flag set so the recorded state stays truthful (never "forgets" that raw
        // mode / the alternate screen is still applied). `restored` is latched
        // LAST, after the work, so the flags drive the teardown, not a flag set
        // before the ops ran.
        if self.cursor_hidden && self.ops.show_cursor().is_ok() {
            self.cursor_hidden = false;
        }
        if self.alt_screen && self.ops.leave_alternate_screen().is_ok() {
            self.alt_screen = false;
        }
        if self.raw_enabled && self.ops.disable_raw_mode().is_ok() {
            self.raw_enabled = false;
        }
        self.restored = true;
    }
}

impl<O: TerminalOps> Drop for Guard<O> {
    fn drop(&mut self) {
        self.restore();
    }
}

/// An RAII guard for the terminal: on construction it enables raw mode, enters
/// the alternate screen, and hides the cursor; on [`Drop`] it runs the exact
/// inverse.
///
/// Because teardown is driven by `Drop`, the terminal is restored on **every**
/// exit path — a normal return, an early `?`, or a panic (paired with
/// [`install_panic_hook`], which restores before the backtrace prints). Hold the
/// guard for the whole lifetime of the TUI; dropping it early restores the
/// terminal immediately.
///
/// Restore is idempotent and tolerant: a partially-initialized guard (setup
/// failed midway) and a double teardown both restore cleanly without panicking.
#[must_use = "hold the guard for the terminal's lifetime; dropping it restores the terminal"]
pub struct TerminalGuard {
    inner: Guard<CrosstermOps>,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen and hide the cursor, returning a
    /// guard whose [`Drop`] restores the terminal.
    ///
    /// # Errors
    ///
    /// Returns [`ChainViewError::Terminal`] if the terminal backend rejects a
    /// setup step (for example, stdout is not a TTY). Any partially-applied
    /// setup is rolled back before returning, so the terminal is left clean.
    pub fn new() -> Result<Self, ChainViewError> {
        Ok(Self {
            inner: Guard::new(CrosstermOps)?,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Restore explicitly here (and again when `inner` drops — restore is
        // idempotent) so the guard's whole point, returning the terminal, is
        // driven by its own `Drop`.
        self.inner.restore();
    }
}

/// Install a panic hook that restores the terminal **before** chaining to the
/// previously installed hook.
///
/// The prior hook (captured via [`std::panic::take_hook`]) is invoked *after*
/// the restore, so the panic message and any backtrace print on a normal
/// (non-raw) screen and are never swallowed (`docs/02-tui-architecture.md` §6).
/// Install this once at startup, before the [`TerminalGuard`] enters the
/// alternate screen.
pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_then_chain(restore_on_panic, |i| previous(i), info);
    }));
}

/// Run `restore` first, then invoke `next` with `payload`.
///
/// Factored out of [`install_panic_hook`] so the ordering guarantee — the
/// restore runs strictly before the chained hook — is unit-testable without
/// installing a process-global hook or constructing a `PanicHookInfo`.
#[inline]
fn restore_then_chain<T>(restore: impl FnOnce(), next: impl FnOnce(&T), payload: &T) {
    restore();
    next(payload);
}

/// Best-effort, synchronous terminal restore for the panic path: show the
/// cursor, leave the alternate screen, and disable raw mode.
///
/// Errors are intentionally ignored — nothing actionable remains inside a panic,
/// and stdout must not carry a second failure. This runs before the chained
/// previous hook prints, so the terminal is already normal when the backtrace
/// lands.
fn restore_on_panic() {
    let mut out: Stdout = io::stdout();
    let _ = execute!(out, Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// A recorded terminal operation, in the exact order it was applied.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        EnableRaw,
        DisableRaw,
        EnterAlt,
        LeaveAlt,
        HideCursor,
        ShowCursor,
    }

    /// A recording [`TerminalOps`] fake. Records each *successful* op into a
    /// shared log and can be configured to fail exactly one op (to inject a
    /// setup failure or a teardown-step failure). Failed ops are not recorded,
    /// so the log reflects the terminal's actual state changes.
    struct FakeOps {
        log: Rc<RefCell<Vec<Op>>>,
        fail_on: Option<Op>,
    }

    impl FakeOps {
        fn new(log: Rc<RefCell<Vec<Op>>>) -> Self {
            Self { log, fail_on: None }
        }

        fn failing(log: Rc<RefCell<Vec<Op>>>, fail_on: Op) -> Self {
            Self {
                log,
                fail_on: Some(fail_on),
            }
        }

        fn run(&mut self, op: Op) -> io::Result<()> {
            if self.fail_on == Some(op) {
                return Err(io::Error::other("injected terminal failure"));
            }
            self.log.borrow_mut().push(op);
            Ok(())
        }
    }

    impl TerminalOps for FakeOps {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            self.run(Op::EnableRaw)
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.run(Op::DisableRaw)
        }
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.run(Op::EnterAlt)
        }
        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.run(Op::LeaveAlt)
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.run(Op::HideCursor)
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.run(Op::ShowCursor)
        }
    }

    fn new_log() -> Rc<RefCell<Vec<Op>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn test_guard_new_records_setup_sequence_in_order() {
        let log = new_log();
        let guard = match Guard::new(FakeOps::new(Rc::clone(&log))) {
            Ok(g) => g,
            Err(e) => panic!("expected setup to succeed, got: {e}"),
        };
        // Setup order: raw mode, then alternate screen, then hide cursor.
        assert_eq!(
            *log.borrow(),
            vec![Op::EnableRaw, Op::EnterAlt, Op::HideCursor]
        );
        drop(guard);
    }

    #[test]
    fn test_guard_drop_restores_inverse_sequence() {
        let log = new_log();
        let guard = match Guard::new(FakeOps::new(Rc::clone(&log))) {
            Ok(g) => g,
            Err(e) => panic!("expected setup to succeed, got: {e}"),
        };
        drop(guard);
        // Full lifecycle: setup forwards, teardown the exact inverse.
        assert_eq!(
            *log.borrow(),
            vec![
                Op::EnableRaw,
                Op::EnterAlt,
                Op::HideCursor,
                Op::ShowCursor,
                Op::LeaveAlt,
                Op::DisableRaw,
            ]
        );
    }

    #[test]
    fn test_guard_restore_continues_past_a_failed_step_and_keeps_the_flag_truthful() {
        // Inject a failure on the FIRST teardown step (show_cursor). The remaining
        // steps must still run, and the failed step's flag must stay set so the
        // recorded state never "forgets" that the cursor is still hidden.
        let log = new_log();
        let mut guard = match Guard::new(FakeOps::failing(Rc::clone(&log), Op::ShowCursor)) {
            Ok(g) => g,
            Err(e) => panic!("expected setup to succeed, got: {e}"),
        };
        guard.restore();
        // show_cursor failed (not recorded), but leave_alt + disable_raw still ran.
        assert_eq!(
            *log.borrow(),
            vec![
                Op::EnableRaw,
                Op::EnterAlt,
                Op::HideCursor,
                Op::LeaveAlt,
                Op::DisableRaw,
            ]
        );
        // The failed step's flag stays TRUE (state truthful); the succeeded ones
        // are cleared; restore is latched.
        assert!(
            guard.cursor_hidden,
            "a failed show_cursor must not clear the flag"
        );
        assert!(!guard.alt_screen);
        assert!(!guard.raw_enabled);
        assert!(guard.restored);
    }

    #[test]
    fn test_guard_double_restore_is_idempotent() {
        let log = new_log();
        let mut guard = match Guard::new(FakeOps::new(Rc::clone(&log))) {
            Ok(g) => g,
            Err(e) => panic!("expected setup to succeed, got: {e}"),
        };
        guard.restore();
        guard.restore();
        drop(guard);
        // Teardown appears exactly once despite three restore attempts.
        assert_eq!(
            *log.borrow(),
            vec![
                Op::EnableRaw,
                Op::EnterAlt,
                Op::HideCursor,
                Op::ShowCursor,
                Op::LeaveAlt,
                Op::DisableRaw,
            ]
        );
    }

    #[test]
    fn test_guard_partial_setup_teardown_undoes_only_applied_steps() {
        // A half-set-up guard: only raw mode was applied. Teardown must undo just
        // that step and must not panic.
        let log = new_log();
        let mut guard = Guard {
            ops: FakeOps::new(Rc::clone(&log)),
            raw_enabled: true,
            alt_screen: false,
            cursor_hidden: false,
            restored: false,
        };
        guard.restore();
        drop(guard);
        assert_eq!(*log.borrow(), vec![Op::DisableRaw]);
    }

    #[test]
    fn test_guard_new_setup_failure_rolls_back_applied_prefix() {
        // Failure at "enter alternate screen": raw mode was applied and must be
        // rolled back; the alternate screen and cursor were never touched.
        let log = new_log();
        let err = match Guard::new(FakeOps::failing(Rc::clone(&log), Op::EnterAlt)) {
            Err(e) => e,
            Ok(_) => panic!("expected setup to fail at the alternate screen"),
        };
        assert!(matches!(err, ChainViewError::Terminal(_)));
        assert_eq!(*log.borrow(), vec![Op::EnableRaw, Op::DisableRaw]);
    }

    #[test]
    fn test_guard_restore_tolerates_backend_error_and_continues() {
        // The cursor-show step fails during teardown; restore must swallow it and
        // still leave the alternate screen and disable raw mode.
        let log = new_log();
        let mut guard = Guard {
            ops: FakeOps::failing(Rc::clone(&log), Op::ShowCursor),
            raw_enabled: true,
            alt_screen: true,
            cursor_hidden: true,
            restored: false,
        };
        guard.restore();
        drop(guard);
        // ShowCursor failed (unrecorded), yet the remaining teardown ran.
        assert_eq!(*log.borrow(), vec![Op::LeaveAlt, Op::DisableRaw]);
    }

    #[test]
    fn test_restore_then_chain_runs_restore_before_chained_hook() {
        let order: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
        restore_then_chain(
            || order.borrow_mut().push("restore"),
            |_payload: &u8| order.borrow_mut().push("chained"),
            &0u8,
        );
        assert_eq!(*order.borrow(), vec!["restore", "chained"]);
    }

    #[test]
    fn test_restore_then_chain_always_invokes_chained_hook() {
        // The chained (previous) hook is never swallowed — proving the panic
        // message still prints after the restore.
        let chained = RefCell::new(false);
        restore_then_chain(|| {}, |_p: &()| *chained.borrow_mut() = true, &());
        assert!(*chained.borrow());
    }
}
