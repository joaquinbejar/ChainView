//! The layering **arch test** — a deterministic, build-time enforcement of the
//! acyclic compile-time module graph (issue #22, `docs/03-data-providers.md` §12,
//! `CLAUDE.md` "Module Boundaries", `docs/TESTING.md` §7).
//!
//! # What it enforces
//!
//! The allowed compile-time edges are `ui → application → {domain, port}`,
//! `adapters → {domain, port, dxfeed_decode, upstream}`, with `error`/`config`
//! as leaves. This test greps the crate's own source (production regions only —
//! `#[cfg(test)] mod` blocks are stripped, since they are not part of the shipped
//! dependency graph) and **fails the build on any back-edge**:
//!
//! - **domain → adapter / port / ui / app** — `src/chain/*` (and `src/replay/*`)
//!   import nothing above them; the edge is port → domain, never domain → port.
//! - **adapter → app / ui** — `src/providers/*` never import `crate::app` or
//!   `crate::ui`.
//! - **adapter → adapter** — a concrete adapter (`src/providers/<id>.rs`) never
//!   imports another adapter module; shared decode lives in the neutral
//!   `dxfeed_decode` module both may depend on.
//! - **`src/ui/*` import of a provider or `tokio` I/O** — the draw layer never
//!   imports `crate::providers` nor a `tokio` I/O module (`net`/`fs`/`process`);
//!   the render loop's `tokio::sync`/`task`/`time` primitives are allowed.
//! - **any `ui →` reverse edge** — no lower layer (`app` / `chain` / `replay` /
//!   `providers` / `error` / `config`) imports `crate::ui`.
//!
//! # It is not a vacuous pass
//!
//! [`test_detector_flags_a_synthetic_back_edge`] proves the detector actually
//! fires on a synthetic offending source, so a green run means "no back-edge",
//! not "the check never ran". The suite is filesystem-only — no network, no
//! socket — and finishes in well under a second (`docs/TESTING.md` §7).

use std::fs;
use std::path::{Path, PathBuf};

/// The reserved built-in adapter ids — each is a concrete adapter module under
/// `src/providers/<id>.rs`, so an import of one from another provider file is an
/// adapter→adapter back-edge (`docs/03-data-providers.md` §12).
const ADAPTER_IDS: [&str; 5] = ["deribit", "tastytrade", "dxlink", "ig", "alpaca"];

/// The crate `src/` directory, anchored at the manifest so it resolves from any
/// working directory.
fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect every `.rs` file under `dir`, sorted for deterministic
/// reporting.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_rust_files(dir, &mut out);
    out.sort();
    out
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("failed to read {}: {e}", dir.display()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => panic!("failed to read a dir entry under {}: {e}", dir.display()),
        };
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// A per-line mask: `true` where the original line sits inside a
/// `#[cfg(test)] mod … { … }` (or `#[cfg(any(test,…))]` / `#[cfg(all(test,…))]`)
/// block, so the scanner ignores test-only imports (e.g. a UI test module that
/// builds `ProviderCapabilities` fixtures) while preserving original line
/// numbers for reporting. Only inline `mod … { … }` blocks are masked; a
/// `mod x;` declaration is not.
fn test_module_mask(src: &str) -> Vec<bool> {
    let mut mask = Vec::new();
    let mut armed = false;
    let mut skipping = false;
    let mut depth: i32 = 0;
    for line in src.lines() {
        if skipping {
            mask.push(true);
            depth += brace_delta(line);
            if depth <= 0 {
                skipping = false;
                depth = 0;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]")
            || trimmed.starts_with("#[cfg(any(test")
            || trimmed.starts_with("#[cfg(all(test")
        {
            armed = true;
            mask.push(false);
            continue;
        }
        if armed {
            // Keep the arm across intervening attribute lines.
            if trimmed.starts_with("#[") {
                mask.push(false);
                continue;
            }
            armed = false;
            let is_mod_open = (trimmed.starts_with("mod ")
                || trimmed.starts_with("pub mod ")
                || trimmed.starts_with("pub(crate) mod "))
                && line.contains('{');
            if is_mod_open {
                mask.push(true);
                depth = brace_delta(line);
                skipping = depth > 0;
                if depth <= 0 {
                    depth = 0;
                }
                continue;
            }
        }
        mask.push(false);
    }
    mask
}

/// The net brace balance of a line (`{` minus `}`). Approximate (it does not
/// parse string/char literals), which is safe here: the codebase's test modules
/// are `#[cfg(test)] mod tests { … }` with balanced braces, so the running depth
/// returns to zero at the block's close.
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0;
    for ch in line.chars() {
        if ch == '{' {
            delta += 1;
        } else if ch == '}' {
            delta -= 1;
        }
    }
    delta
}

/// Which layer a source file belongs to, or `None` for a file outside the
/// layered graph (startup glue / infra / re-export root / bench support).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    Domain,
    Providers,
    Ui,
    App,
    Leaf,
}

/// Classify `rel` (a path relative to `src/`, using `/` separators) into a layer.
fn classify(rel: &str) -> Option<Layer> {
    if rel.starts_with("chain/") || rel.starts_with("replay/") {
        Some(Layer::Domain)
    } else if rel.starts_with("providers/") {
        Some(Layer::Providers)
    } else if rel.starts_with("ui/") {
        Some(Layer::Ui)
    } else if rel == "app.rs" || rel.starts_with("app/") {
        Some(Layer::App)
    } else if rel == "error.rs" || rel == "config.rs" {
        Some(Layer::Leaf)
    } else {
        // main.rs / lib.rs / event.rs / terminal.rs / bench_support.rs /
        // tests_integration.rs are startup glue / infra / test-only, not part of
        // the layered dependency rules.
        None
    }
}

/// The forbidden-edge finding for one line, or `None` when the line is allowed.
fn forbidden_reason(layer: Layer, rel: &str, use_stmt: &str) -> Option<String> {
    match layer {
        // Domain depends on nothing above it (port → domain, never the reverse).
        Layer::Domain => {
            if use_stmt.starts_with("use crate::providers") {
                Some("domain → provider/port (domain must not import crate::providers)".to_owned())
            } else if use_stmt.starts_with("use crate::app") {
                Some("domain → application (domain must not import crate::app)".to_owned())
            } else if use_stmt.starts_with("use crate::ui") {
                Some("ui reverse edge (domain must not import crate::ui)".to_owned())
            } else {
                None
            }
        }
        // Adapters implement the port + normalize into the domain; they never
        // import the application, the ui, or ANOTHER adapter.
        Layer::Providers => {
            if use_stmt.starts_with("use crate::app") {
                Some("adapter → application (a provider must not import crate::app)".to_owned())
            } else if use_stmt.starts_with("use crate::ui") {
                Some("ui reverse edge (a provider must not import crate::ui)".to_owned())
            } else {
                adapter_to_adapter(rel, use_stmt)
            }
        }
        // The draw layer never imports a provider nor a tokio I/O module.
        Layer::Ui => {
            if use_stmt.starts_with("use crate::providers") {
                Some("ui → provider (the draw path must not import crate::providers)".to_owned())
            } else if use_stmt.starts_with("use tokio::net")
                || use_stmt.starts_with("use tokio::fs")
                || use_stmt.starts_with("use tokio::process")
            {
                Some("ui → tokio I/O (no socket/file I/O reachable from the draw path)".to_owned())
            } else {
                None
            }
        }
        // The application sits below the ui: no reverse edge.
        Layer::App => (use_stmt.starts_with("use crate::ui"))
            .then(|| "ui reverse edge (application must not import crate::ui)".to_owned()),
        // Leaves are imported by everyone and import nothing above them.
        Layer::Leaf => {
            if use_stmt.starts_with("use crate::ui") {
                Some("ui reverse edge (a leaf must not import crate::ui)".to_owned())
            } else if use_stmt.starts_with("use crate::app") {
                Some("leaf → application (a leaf must not import crate::app)".to_owned())
            } else if use_stmt.starts_with("use crate::providers") {
                Some("leaf → provider (a leaf must not import crate::providers)".to_owned())
            } else {
                None
            }
        }
    }
}

/// An adapter→adapter finding: a provider file `<stem>.rs` importing a DIFFERENT
/// concrete adapter module. The neutral `dxfeed_decode` and the port `mod`/`sink`
/// are not adapters, so importing them is allowed.
fn adapter_to_adapter(rel: &str, use_stmt: &str) -> Option<String> {
    let stem = file_stem(rel);
    // Only the segment(s) under `use crate::providers::…` can name a sibling
    // adapter. Take the tail so a grouped import
    // `use crate::providers::{alpaca::Foo, sink::Bar}` is caught as well as the
    // direct `use crate::providers::alpaca::…` form.
    let tail = use_stmt.strip_prefix("use crate::providers::")?;
    for adapter in ADAPTER_IDS {
        if adapter == stem {
            continue;
        }
        // The adapter module named as a path/group segment: `alpaca::…`,
        // `alpaca,`, `alpaca}`, `alpaca ` or exactly `alpaca` (self-import of a
        // sibling module). `dxfeed_decode`/`mod`/`sink` are not in ADAPTER_IDS,
        // so importing them stays allowed.
        let hit = tail
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|seg| seg == adapter);
        if hit {
            return Some(format!(
                "adapter → adapter (`{stem}` must not import the `{adapter}` adapter; \
                 shared decode belongs in the neutral dxfeed_decode module)"
            ));
        }
    }
    None
}

/// The file stem of a `src/`-relative path (`providers/deribit.rs` → `deribit`).
fn file_stem(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.strip_suffix(".rs").unwrap_or(name)
}

/// Strip a leading visibility on a `use` line and return the `use …` remainder,
/// so a re-export is scanned like a plain import. Returns `None` for any line
/// that is not an import.
fn normalize_use(trimmed: &str) -> Option<&str> {
    if trimmed.starts_with("use ") {
        return Some(trimmed);
    }
    // `pub use …`, `pub(crate) use …`, `pub(in some::path) use …`.
    let after_pub = trimmed.strip_prefix("pub")?;
    let rest = after_pub.trim_start();
    // Skip an optional `(…)` scope on `pub`.
    let rest = match rest.strip_prefix('(') {
        Some(inner) => inner.split_once(')').map(|(_, tail)| tail.trim_start())?,
        None => rest,
    };
    rest.starts_with("use ").then_some(rest)
}

/// Every forbidden-edge finding for one source file (production regions only),
/// formatted `rel:line — reason — <use stmt>`.
fn violations_in(rel: &str, src: &str) -> Vec<String> {
    let Some(layer) = classify(rel) else {
        return Vec::new();
    };
    let mask = test_module_mask(src);
    let mut out = Vec::new();
    for (idx, (line, masked)) in src.lines().zip(mask.iter()).enumerate() {
        if *masked {
            continue;
        }
        let trimmed = line.trim_start();
        // Normalize any visibility prefix so a re-export (`pub use …`,
        // `pub(crate) use …`, `pub(in path) use …`) is scanned the same as a
        // plain `use …` — a `pub use crate::providers::Foo;` from a domain/ui
        // file creates the identical compile edge and must not evade detection.
        let Some(use_stmt) = normalize_use(trimmed) else {
            continue;
        };
        if let Some(reason) = forbidden_reason(layer, rel, use_stmt) {
            let lineno = idx.checked_add(1).unwrap_or(idx);
            out.push(format!(
                "{rel}:{lineno} — {reason} — `{}`",
                use_stmt.trim_end()
            ));
        }
    }
    out
}

/// The `src/`-relative, `/`-separated path for a file.
fn relative_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

#[test]
fn test_module_graph_has_no_back_edges() {
    let root = src_root();
    let mut violations = Vec::new();
    for path in rust_files(&root) {
        let src = match fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => panic!("failed to read {}: {e}", path.display()),
        };
        let rel = relative_path(&root, &path);
        violations.extend(violations_in(&rel, &src));
    }
    assert!(
        violations.is_empty(),
        "layering back-edges detected (docs/03-data-providers.md §12, CLAUDE.md \
         \"Module Boundaries\"):\n{}",
        violations.join("\n"),
    );
}

#[test]
fn test_detector_flags_a_synthetic_back_edge() {
    // The detector is not a vacuous pass: it fires on a synthetic offending source
    // for EACH forbidden edge in the list.
    let ui_provider = violations_in("ui/depth.rs", "use crate::providers::deribit;\n");
    assert!(
        ui_provider.iter().any(|v| v.contains("ui → provider")),
        "a ui→provider import must be flagged, got {ui_provider:?}"
    );

    let ui_tokio_io = violations_in("ui/driver.rs", "use tokio::net::TcpStream;\n");
    assert!(
        ui_tokio_io.iter().any(|v| v.contains("ui → tokio I/O")),
        "a ui→tokio-I/O import must be flagged, got {ui_tokio_io:?}"
    );

    let domain_adapter = violations_in("chain/store.rs", "use crate::providers::deribit;\n");
    assert!(
        domain_adapter.iter().any(|v| v.contains("domain →")),
        "a domain→provider import must be flagged, got {domain_adapter:?}"
    );

    let adapter_app = violations_in("providers/deribit.rs", "use crate::app::App;\n");
    assert!(
        adapter_app
            .iter()
            .any(|v| v.contains("adapter → application")),
        "an adapter→app import must be flagged, got {adapter_app:?}"
    );

    let adapter_adapter =
        violations_in("providers/deribit.rs", "use crate::providers::alpaca::X;\n");
    assert!(
        adapter_adapter
            .iter()
            .any(|v| v.contains("adapter → adapter")),
        "an adapter→adapter import must be flagged, got {adapter_adapter:?}"
    );

    // A GROUPED adapter→adapter import (not just the direct `::alpaca::` form)
    // is caught — the neutral `dxfeed_decode` sibling in the same group stays
    // allowed.
    let grouped_adapter = violations_in(
        "providers/dxlink.rs",
        "use crate::providers::{dxfeed_decode::decode, alpaca::X};\n",
    );
    assert!(
        grouped_adapter
            .iter()
            .any(|v| v.contains("adapter → adapter")),
        "a grouped adapter→adapter import must be flagged, got {grouped_adapter:?}"
    );

    // A `pub use` RE-EXPORT back-edge creates the same compile edge as a plain
    // `use` and must be flagged too (not skipped for its visibility prefix).
    let reexport_edge = violations_in(
        "chain/store.rs",
        "pub(crate) use crate::providers::deribit;\n",
    );
    assert!(
        reexport_edge.iter().any(|v| v.contains("domain →")),
        "a `pub(crate) use` re-export back-edge must be flagged, got {reexport_edge:?}"
    );

    let ui_reverse = violations_in("app/registry.rs", "use crate::ui::render;\n");
    assert!(
        ui_reverse.iter().any(|v| v.contains("ui reverse edge")),
        "a ui reverse edge must be flagged, got {ui_reverse:?}"
    );

    // Allowed edges are NOT flagged: application → port, ui → domain state, a leaf
    // naming the domain ProviderId, an adapter importing the neutral dxfeed_decode.
    assert!(
        violations_in(
            "app/registry.rs",
            "use crate::providers::deribit::DeribitAdapter;\n"
        )
        .is_empty(),
        "application → port (registering a built-in adapter) is an allowed edge"
    );
    assert!(
        violations_in(
            "providers/dxlink.rs",
            "use crate::providers::dxfeed_decode::decode;\n"
        )
        .is_empty(),
        "an adapter importing the neutral dxfeed_decode module is allowed"
    );
    assert!(
        violations_in("error.rs", "use crate::chain::ProviderId;\n").is_empty(),
        "a leaf naming the domain ProviderId newtype is allowed"
    );
}

#[test]
fn test_test_modules_are_stripped_before_scanning() {
    // A `use crate::providers::…` INSIDE a `#[cfg(test)] mod tests { … }` block is
    // a fixture import, not a production back-edge, so it is masked out (mirrors
    // the real src/ui/theme.rs + src/ui/chain.rs test modules).
    let src = "\
use crate::chain::ChainStore;

#[cfg(test)]
mod tests {
    use crate::providers::deribit::DeribitAdapter;
    fn helper() {}
}
";
    assert!(
        violations_in("ui/chain.rs", src).is_empty(),
        "a provider import inside a #[cfg(test)] mod is not a production back-edge"
    );
    // But the SAME import in production code IS flagged.
    let production = "use crate::providers::deribit::DeribitAdapter;\n";
    assert!(
        !violations_in("ui/chain.rs", production).is_empty(),
        "the same import in production code must still be flagged"
    );
}
