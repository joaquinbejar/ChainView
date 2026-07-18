#!/usr/bin/env bash
#
# scripts/check-changelog.sh - the ChainView CHANGELOG-discipline gate
# (issue #55, docs/SEMVER.md "CHANGELOG discipline" / "CI enforcement").
#
# WHAT IT DOES
#   Every user-visible PR must add at least one new line under the
#   `## [Unreleased]` section of CHANGELOG.md (Keep a Changelog 1.1.0). This
#   script is the local + CI enforcement of that rule:
#
#     1. SKIP if the PR title starts with an internal-only prefix
#        (chore: / refactor: / test: / docs: / ci: / bench:) or carries the
#        `[skip changelog]` override token anywhere in the title.
#     2. Otherwise compare the `[Unreleased]` section between the PR base and the
#        PR head. The head must add at least one non-blank line under
#        `[Unreleased]`.
#     3. FAIL closed when neither condition holds.
#
#   The comparison is SECTION-SCOPED (only the `[Unreleased]` block is diffed),
#   so unrelated edits elsewhere in CHANGELOG.md neither satisfy nor break the
#   gate, and line-number churn cannot make it flaky.
#
# INPUTS (environment, for CI)
#   PR_TITLE   the pull-request title (skip-prefix / [skip changelog] detection).
#              Unset/empty -> treated as a user-visible PR (the gate runs).
#   BASE_REF   the base branch the PR targets (default: main). The base
#              CHANGELOG.md is read from `merge-base(BASE_REF, HEAD)`.
#
# MODES
#   (default)     Resolve base + head CHANGELOG.md from git and enforce the gate.
#   --self-test   Prove the gate FIRES without touching git: an added-line diff
#                 passes, a no-added-line diff fails, and both skip paths
#                 (skip-prefix, [skip changelog]) skip. Deterministic.
#   -h|--help     Usage.
#
# No new dependency: POSIX tools + git + bash only.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHANGELOG="CHANGELOG.md"

# The internal-only title prefixes that skip the gate (docs/SEMVER.md
# "CI enforcement"). Anchored at the start of the title.
SKIP_PREFIXES='chore: refactor: test: docs: ci: bench:'
# The explicit per-PR override token (case-insensitive), for genuine edge cases.
SKIP_TOKEN='[skip changelog]'

usage() {
  cat <<'EOF'
Usage: scripts/check-changelog.sh [--self-test]

Enforces the ChainView CHANGELOG rule: a user-visible PR must add a new line
under [Unreleased] in CHANGELOG.md (docs/SEMVER.md). Skips on the internal-only
title prefixes (chore:/refactor:/test:/docs:/ci:/bench:) and the
`[skip changelog]` token.

Environment: PR_TITLE, BASE_REF (default: main).
  --self-test   Prove the gate detects a regression (no git, deterministic).
EOF
}

# --- Pure helpers (git-free; unit-testable via --self-test) -----------------

# Is a PR title internal-only (skippable)?  Returns 0 (skip) / 1 (enforce).
title_is_skippable() {
  local title="$1" prefix lower
  # Case-insensitive `[skip changelog]` token anywhere in the title. `tr` keeps
  # this portable to bash 3.2 (macOS) where `${var,,}` is unavailable.
  lower="$(printf '%s' "$title" | tr '[:upper:]' '[:lower:]')"
  case "$lower" in
    *"$SKIP_TOKEN"*) return 0 ;;
  esac
  for prefix in $SKIP_PREFIXES; do
    case "$title" in
      "$prefix"*) return 0 ;;
    esac
  done
  return 1
}

# Print the body of the `## [Unreleased]` section of a CHANGELOG file: every
# line after the `## [Unreleased]` heading up to (but excluding) the next
# `## [` heading. A missing file / missing section prints nothing.
unreleased_block() {
  local file="$1"
  [ -f "$file" ] || return 0
  awk '
    /^## \[Unreleased\]/ { in_section = 1; next }
    /^## \[/            { in_section = 0 }
    in_section          { print }
  ' "$file"
}

# Count non-blank lines the head adds under [Unreleased] relative to the base.
# Diffs the two extracted blocks and counts added (`>`) non-blank lines.
count_added_unreleased() {
  local base_file="$1" head_file="$2"
  diff <(unreleased_block "$base_file") <(unreleased_block "$head_file") \
    | sed -n 's/^> //p' \
    | grep -c '[^[:space:]]' \
    || true
}

# The core gate over two files + a title. Returns 0 (pass/skip) / 1 (fail).
# Prints a one-line verdict.
check_changelog() {
  local base_file="$1" head_file="$2" title="$3"
  if title_is_skippable "$title"; then
    echo "check-changelog: SKIP - internal-only PR title (\"${title}\")"
    return 0
  fi
  local added
  added="$(count_added_unreleased "$base_file" "$head_file")"
  if [ "${added:-0}" -ge 1 ]; then
    echo "check-changelog: OK - ${added} new [Unreleased] line(s) added"
    return 0
  fi
  {
    echo "check-changelog: FAIL - no new line under [Unreleased] in ${CHANGELOG}."
    echo "  A user-visible PR must add a CHANGELOG entry under one of the"
    echo "  Keep a Changelog headings (Added/Changed/Deprecated/Removed/Fixed/"
    echo "  Security). For an internal-only change, prefix the PR title with one"
    echo "  of: chore: refactor: test: docs: ci: bench:  -- or add the"
    echo "  \`[skip changelog]\` token (docs/SEMVER.md CI enforcement)."
  } >&2
  return 1
}

# --- git resolution (CI + local real run) -----------------------------------

# Print the base CHANGELOG.md contents (empty when unresolvable / absent).
resolve_base_changelog() {
  local base_ref="${BASE_REF:-main}" ref mb
  for ref in "origin/${base_ref}" "${base_ref}"; do
    if git -C "$REPO_ROOT" rev-parse --verify --quiet "${ref}^{commit}" >/dev/null 2>&1; then
      mb="$(git -C "$REPO_ROOT" merge-base "${ref}" HEAD 2>/dev/null || echo "${ref}")"
      if git -C "$REPO_ROOT" cat-file -e "${mb}:${CHANGELOG}" 2>/dev/null; then
        git -C "$REPO_ROOT" show "${mb}:${CHANGELOG}"
      fi
      return 0
    fi
  done
}

run_gate() {
  local title="${PR_TITLE:-}" base_file head_file rc=0
  base_file="$(mktemp)"
  head_file="${REPO_ROOT}/${CHANGELOG}"
  # shellcheck disable=SC2064
  trap "rm -f '${base_file}'" EXIT
  resolve_base_changelog > "$base_file"
  if [ ! -f "$head_file" ]; then
    echo "check-changelog: FAIL - ${CHANGELOG} not found in the working tree" >&2
    return 1
  fi
  check_changelog "$base_file" "$head_file" "$title" || rc=$?
  return "$rc"
}

# --- Self-test (deterministic; no git) --------------------------------------

self_test() {
  local dir base added noadd fails=0
  dir="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '${dir}'" EXIT

  base="${dir}/base.md"
  added="${dir}/added.md"
  noadd="${dir}/noadd.md"

  cat > "$base" <<'EOF'
# Changelog

## [Unreleased]

### Added

- An existing entry.

## [0.0.1] - 2026-01-01

### Added

- Name reservation.
EOF

  # Head that ADDS a new line under [Unreleased].
  cat > "$added" <<'EOF'
# Changelog

## [Unreleased]

### Added

- An existing entry.
- A brand new user-visible entry (#55).

## [0.0.1] - 2026-01-01

### Added

- Name reservation.
EOF

  # Head that adds NOTHING under [Unreleased] (an edit to an old section only).
  cat > "$noadd" <<'EOF'
# Changelog

## [Unreleased]

### Added

- An existing entry.

## [0.0.1] - 2026-01-01

### Added

- Name reservation, reworded but still one section down.
EOF

  echo "== 1/4: an added [Unreleased] line must PASS =="
  local rc1=0
  check_changelog "$base" "$added" "Freeze CLI/config surfaces (#55)" || rc1=$?
  if [ "$rc1" -eq 0 ]; then echo "  OK"; else echo "  UNEXPECTED: should pass"; fails=$((fails + 1)); fi
  echo

  echo "== 2/4: no new [Unreleased] line must FAIL =="
  local rc2=0
  check_changelog "$base" "$noadd" "Reword an old changelog entry" 2>/dev/null || rc2=$?
  if [ "$rc2" -ne 0 ]; then echo "  OK (exit ${rc2})"; else echo "  UNEXPECTED: should fail"; fails=$((fails + 1)); fi
  echo

  echo "== 3/4: a skip-prefix title must SKIP even with no new line =="
  local rc3=0
  check_changelog "$base" "$noadd" "chore: bump a dev tool" || rc3=$?
  if [ "$rc3" -eq 0 ]; then echo "  OK (skipped)"; else echo "  UNEXPECTED: skip-prefix should skip"; fails=$((fails + 1)); fi
  echo

  echo "== 4/4: the [skip changelog] token must SKIP even with no new line =="
  local rc4=0
  check_changelog "$base" "$noadd" "Emergency infra fix [skip changelog]" || rc4=$?
  if [ "$rc4" -eq 0 ]; then echo "  OK (skipped)"; else echo "  UNEXPECTED: token should skip"; fails=$((fails + 1)); fi
  echo

  # A control: the skip-prefix path must NOT mask a real regression on a
  # user-visible title (proves case 2 is not a vacuous always-fail).
  echo "== control: a user-visible title with a new line still PASSES =="
  local rc5=0
  check_changelog "$base" "$added" "Add a real feature" || rc5=$?
  if [ "$rc5" -eq 0 ]; then echo "  OK"; else echo "  UNEXPECTED"; fails=$((fails + 1)); fi
  echo

  if [ "$fails" -eq 0 ]; then
    echo "check-changelog --self-test: PASSED - the gate requires a real [Unreleased]"
    echo "entry and honors both skip paths. It is wired, not vacuous."
    return 0
  fi
  echo "check-changelog --self-test: FAILED - ${fails} expectation(s) not met." >&2
  return 1
}

main() {
  case "${1:-}" in
    --self-test) self_test ;;
    -h|--help)   usage; exit 0 ;;
    "")          run_gate ;;
    *)           echo "check-changelog: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
  esac
}

main "$@"
