#!/usr/bin/env bash
# scripts/changelog-section.sh <version> — print just one release's CHANGELOG body.
#
# The SINGLE release-notes extractor used everywhere notes are needed: the
# release-cut playbook (docs/RELEASE-PROCESS.md §4 tag message) and the
# tag-triggered release workflow (.github/workflows/release.yml §6 release body),
# so the tag message and the GitHub Release body are byte-identical.
#
# It is a portable awk range — starts printing AFTER the version heading and stops
# at the NEXT `## [` heading. No `head -n -1` (BSD head rejects a negative count),
# and the start heading can never be re-matched as the end heading because printing
# begins on the line after it. Behaves identically on macOS (BSD) and Linux (GNU).
#
# Usage:
#   bash scripts/changelog-section.sh 1.0.0            # reads ./CHANGELOG.md
#   test -n "$(bash scripts/changelog-section.sh 1.0.0)" || exit 1   # non-empty guard
set -euo pipefail

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  echo "usage: scripts/changelog-section.sh <version>" >&2
  exit 2
fi

CHANGELOG="${CHANGELOG_FILE:-CHANGELOG.md}"
if [ ! -f "$CHANGELOG" ]; then
  echo "changelog not found: $CHANGELOG" >&2
  exit 2
fi

awk -v ver="$VERSION" '
  $0 ~ ("^## \\[" ver "\\]") { inblock = 1; next }   # match heading, skip it, start
  inblock && /^## \[/        { exit }                 # next release heading -> stop
  inblock                    { print }                # body lines in between
' "$CHANGELOG"
