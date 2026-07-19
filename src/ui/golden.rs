//! Render-golden test support (#19, `docs/TESTING.md` §4).
//!
//! A render golden captures a screen drawn into a [`ratatui::backend::TestBackend`]
//! at a **fixed** [`GOLDEN_WIDTH`]×[`GOLDEN_HEIGHT`] as text — the visible symbol
//! of each cell, one row per line — and compares it against a committed golden
//! under `tests/render/golden/<category>/<name>`. A fixed size plus a deterministic
//! input (a fixed as-of instant / the fixture's own timestamps, never a wall clock
//! or a live socket) make the bytes stable across machines.
//!
//! A deliberate screen change regenerates the golden in the same commit
//! ([`assert_golden`] rewrites it when `UPDATE_GOLDENS` is set); a screen change
//! that leaves the golden untouched is caught by the mismatch — that is exactly
//! what the golden exists for (`docs/TESTING.md` §4, the PR checklist).

use std::fs;
use std::path::PathBuf;

use ratatui::buffer::Buffer;

/// The fixed golden terminal width, in columns (`docs/TESTING.md` §4).
pub(crate) const GOLDEN_WIDTH: u16 = 120;

/// The fixed golden terminal height, in rows (`docs/TESTING.md` §4).
pub(crate) const GOLDEN_HEIGHT: u16 = 40;

/// Render a `Buffer` to deterministic text: one line per row (each cell's visible
/// symbol, in column order), trailing spaces trimmed, every line newline-terminated.
///
/// Trailing spaces carry no information (an empty cell is a space), so trimming
/// keeps the golden compact and editor-friendly without losing a rendered glyph. A
/// wide grapheme is stored in one cell with the following continuation cell(s)
/// carrying an empty symbol, so concatenating symbols reconstructs the visible
/// text faithfully.
#[must_use]
pub(crate) fn buffer_to_text(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                line.push_str(cell.symbol());
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The absolute path of the golden `tests/render/golden/<category>/<name>`,
/// anchored at the crate root via `CARGO_MANIFEST_DIR` so it resolves the same
/// from any working directory.
#[must_use]
fn golden_path(category: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("render")
        .join("golden")
        .join(category)
        .join(name)
}

/// Compare `actual` against the committed golden for `category`/`name`, or — when
/// the `UPDATE_GOLDENS` environment variable is set — rewrite the golden and return.
///
/// The update path is the documented regeneration mechanism: after a deliberate
/// screen change, `UPDATE_GOLDENS=1 cargo test` refreshes every golden, and the
/// diff is reviewed and committed. Without it, a missing golden and a mismatch both
/// fail with a message pointing at the regeneration command.
#[track_caller]
pub(crate) fn assert_golden(category: &str, name: &str, actual: &str) {
    let path = golden_path(category, name);
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                panic!(
                    "failed to create golden directory {}: {e}",
                    parent.display()
                );
            }
        }
        if let Err(e) = fs::write(&path, actual) {
            panic!("failed to write golden {}: {e}", path.display());
        }
        return;
    }
    let expected = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => panic!(
            "missing render golden {} ({e}); regenerate with `UPDATE_GOLDENS=1 cargo test`",
            path.display(),
        ),
    };
    assert_eq!(
        actual, expected,
        "render golden {category}/{name} mismatch; if this screen change is \
         intended, regenerate with `UPDATE_GOLDENS=1 cargo test` and commit the diff",
    );
}
