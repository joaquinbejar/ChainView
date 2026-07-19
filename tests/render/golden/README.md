# Render goldens

Committed screen snapshots (`docs/TESTING.md` §4). Each golden is a screen drawn
into a `ratatui::backend::TestBackend` at a **fixed 120×40** and captured as text
(the visible symbol of each cell, one row per line, trailing spaces trimmed). A
fixed size plus a deterministic input (a fixed as-of instant / the fixture's own
timestamps, never a wall clock or a live socket) makes the bytes stable across
machines.

The render helper lives in `src/ui/golden.rs` (`#[cfg(test)]`); the tests that
drive each golden live beside the screen they render (e.g. `src/ui/chain.rs`).

## Regeneration discipline

A change to a widget's layout or content **must** update the matching golden in
the **same commit** — a PR that changes a screen but leaves its golden untouched
is 🔴 (`docs/TESTING.md` §4, §10; the golden exists precisely to catch that). The
mismatch fails the test until the golden is refreshed.

To regenerate after a deliberate change, rewrite every golden and review the diff:

```bash
UPDATE_GOLDENS=1 cargo test
```

Then run `cargo test` again to confirm the refreshed goldens pass, and commit the
diff.

## Escape-sequence hygiene

`chain/escape_hygiene.txt` is a security golden (`docs/SECURITY.md` §6.4): a
hostile venue-controlled string carrying an escape / OSC / control sequence flows
through the adapter seam and must render as **inert visible text**. Its committed
bytes contain **no** raw `ESC` (`0x1B`), so a golden that regains an escape byte is
a regression:

```bash
# Must print nothing:
grep -l $'\x1b' tests/render/golden/chain/*
```

## Files

| Path | State |
|------|-------|
| `chain/deribit_btc_atm.txt` | populated matrix (recorded Deribit fixture → adapter normalize → chain matrix) |
| `chain/loading.txt` | pre-first-frame loading state (connecting spinner) |
| `chain/empty.txt` | empty-Ready state ("no data for `<underlying> <expiry>`") |
| `chain/provider_error.txt` | provider-unreachable error state |
| `chain/stale.txt` | stale-feed state (last chain dimmed, `◐ stale` badge — never blanked) |
| `chain/escape_hygiene.txt` | a hostile venue symbol rendered as inert visible text |
