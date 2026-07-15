//! Panic-injection restore harness (`docs/TESTING.md` §7).
//!
//! This is a `harness = false` integration test: a single `main` that plays two
//! roles. Run plainly by `cargo test`, it takes the PARENT role — it re-execs
//! itself with `CHAINVIEW_PANIC_CHILD` set, captures the child's output, and
//! asserts the terminal was restored. Run with that env var set, it takes the
//! CHILD role — it installs the real panic hook and panics, so the restore path
//! runs on a bare process with the default hook as the chained tail (no libtest
//! `catch_unwind` in the way).
//!
//! The child needs no TTY: the `crossterm` cursor-show and leave-alternate-screen
//! commands write their ANSI escapes to stdout even over a pipe, so the parent
//! can assert they were emitted. `disable_raw_mode` fails without a TTY and is
//! ignored by the best-effort restore, exactly as designed.

use std::process::Command;

/// Marker the child panics with; the parent asserts it survives to stderr,
/// proving the chained (previous) hook was invoked and the message not swallowed.
const PANIC_MARKER: &str = "chainview-panic-harness-sentinel";

/// The ANSI escape `crossterm` emits to leave the alternate screen.
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// The ANSI escape `crossterm` emits to show the cursor.
const SHOW_CURSOR: &str = "\x1b[?25h";

/// Env var that selects the child role.
const CHILD_ENV: &str = "CHAINVIEW_PANIC_CHILD";

fn main() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        return;
    }
    run_parent();
}

/// Child role: install the panic hook and panic. The hook restores the terminal
/// (leave alternate screen + show cursor to stdout) before the chained default
/// hook prints the message to stderr. The process then exits non-zero.
fn run_child() {
    chainview::install_panic_hook();
    panic!("{PANIC_MARKER}");
}

/// Parent role: re-exec this binary as the child, capture its output, and assert
/// the restore-before-print + no-swallow + no-hang guarantees.
fn run_parent() {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => panic!("could not resolve current_exe: {err}"),
    };

    // `output()` waits for the child to finish; the child has no loop and always
    // panics, so it exits promptly — a hang would surface as a stuck test, well
    // inside the < 10 s integration budget (`docs/TESTING.md` §7).
    let output = match Command::new(&exe)
        .env(CHILD_ENV, "1")
        .env("RUST_BACKTRACE", "0")
        .output()
    {
        Ok(out) => out,
        Err(err) => panic!("could not spawn panic-harness child: {err}"),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The child panicked, so it exited non-zero (and, since `output()` returned,
    // it did not hang).
    assert!(
        !output.status.success(),
        "panic-harness child must exit non-zero; status: {:?}",
        output.status
    );

    // The panic hook restored the terminal: both restore escapes reach stdout.
    assert!(
        stdout.contains(LEAVE_ALT_SCREEN),
        "expected the leave-alternate-screen restore on stdout; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains(SHOW_CURSOR),
        "expected the show-cursor restore on stdout; stdout={stdout:?} stderr={stderr:?}"
    );

    // The chained previous hook still printed the panic message (not swallowed).
    assert!(
        stderr.contains(PANIC_MARKER),
        "expected the chained panic message on stderr; stdout={stdout:?} stderr={stderr:?}"
    );
}
