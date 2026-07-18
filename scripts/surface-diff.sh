#!/usr/bin/env bash
#
# scripts/surface-diff.sh - the ChainView public-surface change annotator
# (issue #55, docs/SEMVER.md "What counts as a public surface" / "CI enforcement").
#
# WHAT IT DOES
#   Flags a PR that touches a public-surface SOURCE file so a rename/removal of a
#   CLI flag, an env var, a default keybinding, a port type, or an exported item
#   is caught in review rather than shipped silently under a minor bump. It is
#   INFORMATIONAL by design - it never fails the build on a mere touch (a
#   surface file changes on almost every additive PR too). The reviewer decides
#   whether the change is additive (minor) or breaking (major) and confirms the
#   CHANGELOG + version classification.
#
#   The gate itself is proven non-vacuous by --self-test (a surface-touching
#   file set is flagged; an internal-only set is not), which CI runs as a
#   BLOCKING step alongside the informational annotation.
#
# THE FROZEN SURFACE SOURCES (docs/SEMVER.md "What counts as a public surface")
#   src/main.rs            CLI grammar (subcommands, flags) + exit codes
#   src/config.rs          configuration environment variables + precedence
#   src/app/keymap.rs      the KEYMAP - the documented keybinding map
#   src/ui/theme.rs        the help-overlay renderer (documented keybindings)
#   src/providers/mod.rs   the provider port (trait + capabilities + enums)
#   src/chain/identity.rs  ProviderId grammar + RESERVED_PROVIDER_IDS
#   src/lib.rs             the exported library API (crate-root re-exports)
#
# INPUTS (environment, for CI)
#   BASE_REF   the base branch the PR targets (default: main). Changed files are
#              computed from `merge-base(BASE_REF, HEAD)`.
#
# MODES
#   (default)     Annotate the surface files touched vs the base (always exit 0).
#   --self-test   Prove the classifier flags a surface touch and ignores an
#                 internal-only change. Deterministic, no git.
#   -h|--help     Usage.
#
# No new dependency: POSIX tools + git + bash only.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The frozen public-surface source files (docs/SEMVER.md). One per line.
SURFACE_FILES='src/main.rs
src/config.rs
src/app/keymap.rs
src/ui/theme.rs
src/providers/mod.rs
src/chain/identity.rs
src/lib.rs'

usage() {
  cat <<'EOF'
Usage: scripts/surface-diff.sh [--self-test]

Informational: lists the public-surface source files a PR touches (docs/SEMVER.md)
so a breaking change to the CLI, config, keybindings, provider port, or exported
API is flagged for review. Never fails on a touch.

Environment: BASE_REF (default: main).
  --self-test   Prove the classifier flags a surface touch, ignores an internal
                change (no git, deterministic).
EOF
}

# --- Pure helper (git-free; unit-testable via --self-test) ------------------

# Given a newline-separated list of changed paths on stdin, print the subset
# that are frozen public-surface sources (in SURFACE_FILES order).
surface_hits() {
  local changed surface
  changed="$(cat)"
  while IFS= read -r surface; do
    [ -n "$surface" ] || continue
    if printf '%s\n' "$changed" | grep -qxF "$surface"; then
      printf '%s\n' "$surface"
    fi
  done <<< "$SURFACE_FILES"
}

# --- git resolution (CI + local real run) -----------------------------------

changed_files() {
  local base_ref="${BASE_REF:-main}" ref mb
  for ref in "origin/${base_ref}" "${base_ref}"; do
    if git -C "$REPO_ROOT" rev-parse --verify --quiet "${ref}^{commit}" >/dev/null 2>&1; then
      mb="$(git -C "$REPO_ROOT" merge-base "${ref}" HEAD 2>/dev/null || echo "${ref}")"
      git -C "$REPO_ROOT" diff --name-only "${mb}" HEAD
      return 0
    fi
  done
}

annotate() {
  local hits
  hits="$(changed_files | surface_hits)"
  if [ -z "$hits" ]; then
    echo "surface-diff: no public-surface source touched (docs/SEMVER.md)."
    return 0
  fi
  echo "surface-diff: this PR touches a FROZEN public surface (docs/SEMVER.md):"
  printf '%s\n' "$hits" | sed 's/^/  * /'
  cat <<'EOF'

  Reviewer checklist (docs/SEMVER.md "Version increment rules"):
    - Additive only (new flag / env var / capability dimension / exported item)?
      -> minor bump, CHANGELOG under ### Added.
    - Rename / removal / re-typed field / changed default keybinding / grammar
      tightening / RESERVED_PROVIDER_IDS change?
      -> MAJOR bump; requires the one-minor deprecation path first.
    - MSRV raised (rust-version)? -> minor bump with a ### Changed callout.
  This message is informational; it never fails the build.
EOF
}

# --- Self-test (deterministic; no git) --------------------------------------

self_test() {
  local fails=0 hits

  echo "== 1/3: a surface-touching change set is flagged =="
  hits="$(printf '%s\n' 'src/config.rs' 'src/chain/store.rs' 'README.md' | surface_hits)"
  if [ "$hits" = "src/config.rs" ]; then
    echo "  OK (flagged: src/config.rs)"
  else
    echo "  UNEXPECTED: expected 'src/config.rs', got '${hits}'"; fails=$((fails + 1))
  fi
  echo

  echo "== 2/3: an internal-only change set is NOT flagged =="
  hits="$(printf '%s\n' 'src/chain/store.rs' 'src/app/registry.rs' 'docs/05-views-and-ux.md' | surface_hits)"
  if [ -z "$hits" ]; then
    echo "  OK (no surface file flagged)"
  else
    echo "  UNEXPECTED: expected none, got '${hits}'"; fails=$((fails + 1))
  fi
  echo

  echo "== 3/3: multiple surface touches are all flagged, in order =="
  hits="$(printf '%s\n' 'src/lib.rs' 'src/main.rs' 'src/providers/mod.rs' 'tests/arch.rs' | surface_hits | tr '\n' ' ')"
  if [ "$hits" = "src/main.rs src/providers/mod.rs src/lib.rs " ]; then
    echo "  OK (${hits})"
  else
    echo "  UNEXPECTED: got '${hits}'"; fails=$((fails + 1))
  fi
  echo

  if [ "$fails" -eq 0 ]; then
    echo "surface-diff --self-test: PASSED - the classifier flags a surface touch"
    echo "and ignores an internal-only change. It is wired, not vacuous."
    return 0
  fi
  echo "surface-diff --self-test: FAILED - ${fails} expectation(s) not met." >&2
  return 1
}

main() {
  case "${1:-}" in
    --self-test) self_test ;;
    -h|--help)   usage; exit 0 ;;
    "")          annotate ;;
    *)           echo "surface-diff: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
  esac
}

main "$@"
