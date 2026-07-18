#!/usr/bin/env bash
#
# scripts/check-perf.sh - the ChainView CI perf-regression gate (issue #52,
# docs/06-performance.md section 5, docs/TESTING.md section 11, NFR-17).
#
# WHAT IT DOES
#   Compares each hot-path benchmark (HP-1..HP-4) against its committed BENCH.md
#   baseline p99 plus a documented per-path noise/headroom threshold, and exits
#   non-zero when a path regresses past its ceiling (baseline + threshold).
#
#   The baselines and thresholds are read from the machine-readable "perf-gate"
#   block in BENCH.md (between the perf-gate:begin / perf-gate:end markers). The
#   gate reads the COMMITTED file - the job can never rewrite the baseline it
#   gates against, so a legitimate perf change re-baselines through a reviewed
#   BENCH.md edit in the same PR.
#
# MODES
#   --run           Run the four benches, parse each p99, compare, enforce
#                   (exit non-zero on any breach). Default mode.
#   --run --report-only
#                   Same, but never exit non-zero: print the comparison and
#                   return 0. Used by the INFORMATIONAL CI step, because a
#                   shared GitHub-hosted runner is a slower, noisier hardware
#                   class than the BENCH.md baseline host (Apple M4 Max), so an
#                   absolute-p99 breach there is expected and is NOT a
#                   regression. Enforcement runs on baseline-class hardware and,
#                   in CI, through the --self-test gate logic below.
#   --run --only <bench>
#                   Run and gate only one named bench (e.g. bench_render_chain).
#                   CI uses `--only bench_render_chain --report-only` to exercise
#                   the REAL-output parser end-to-end on a runner, because that
#                   bench is fast (no per-sample generation), whereas
#                   bench_event_fanin / bench_chain_merge rebuild and normalize
#                   the full leg set through the real seam on every sample and so
#                   run for many minutes - unbounded for a shared runner. The
#                   full four-bench enforcement is `make perf` on baseline-class
#                   hardware, where the multi-minute wall-clock is acceptable.
#   --self-test     Prove the gate FIRES without running a bench: feed the
#                   comparison engine synthetic measured sets derived from the
#                   REAL committed baselines/thresholds - a within-threshold set
#                   (must pass), a deliberately slowed set (must fail on every
#                   path), a missing-measurement set (must fail), and a mixed set
#                   (exactly one fail). Deterministic and hardware-independent,
#                   so it is the CI-BLOCKING proof that the gate is not vacuous.
#
# The gated metric is hdrhistogram p99 (a frame budget is a tail property). p99.9
# and max are indicative, not gated (BENCH.md section 1).
#
# This script is CI TOOLING - it adds no Cargo dependency and never touches a
# live venue. Portable to the repo's default shells (bash 3.2 on macOS, bash 4+
# on the runner): the float comparison lives in awk, no associative arrays.

set -euo pipefail

CARGO="${CARGO:-cargo}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH_MD="${REPO_ROOT}/BENCH.md"

# The four hot-path benches, in HP order (fixed in issue #21 / #36; this gate
# adds none - see the issue #52 scope).
BENCHES="bench_render_chain bench_event_fanin bench_chain_merge bench_replay_decode"

# The GATED metric is the hdrhistogram p99 from each bench's own controlled loop
# (a fixed warmup + sample count in the bench, printed by hdr_report before the
# criterion phase). criterion's mean is only a context cross-check, so we shrink
# its phase to the minimum via CLI args (each bench calls configure_from_args) -
# this does NOT touch the gated hdr number, it just keeps the gate's wall-clock
# bounded. A full criterion cross-check is `cargo bench --features bench`
# (BENCH.md section 5).
CRITERION_ARGS="--warm-up-time 1 --measurement-time 1 --sample-size 10"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
CONFIG_FILE="${TMP}/gate_config.txt"
ENGINE_FILE="${TMP}/engine.awk"

usage() {
  cat <<'EOF'
Usage: scripts/check-perf.sh [--run [--report-only] [--only <bench>] | --self-test | --help]

  --run            Run the four hot-path benches and enforce the BENCH.md gate
                   (exit non-zero on any p99 regression past its ceiling).
  --report-only    With --run: print the comparison but always exit 0
                   (informational; for CI on non-baseline-class runners).
  --only <bench>   With --run: run and gate only one named bench (e.g.
                   bench_render_chain, the fast parser-exercise for CI).
  --self-test      Prove the gate fires against the committed baselines without
                   running a bench (deterministic; the CI-blocking check).
  --help           Show this help.

Baselines and thresholds live in the BENCH.md perf-gate block.
EOF
}

# Extract the machine-readable gate block from BENCH.md as
# "name metric baseline threshold" lines (one per hot path).
read_gate_config() {
  awk '
    /perf-gate:begin/ { on=1; next }
    /perf-gate:end/   { on=0 }
    on && /^bench_[a-z_]+[[:space:]]+p99[[:space:]]/ { print $1, $2, $3, $4 }
  ' "$BENCH_MD"
}

# The comparison ENGINE (portable awk). Joins the gate config (FILE1) with a
# measured set (FILE2) by bench name, computes ceiling = baseline + threshold,
# prints a table, and exits 1 on any breach OR any missing measurement (a
# missing measurement is a hard fail, never a vacuous pass).
write_engine() {
  cat > "$ENGINE_FILE" <<'AWK'
FNR==NR {
  metric[$1]=$2; base[$1]=$3; thr[$1]=$4; order[++n]=$1; next
}
{ meas[$1]=$2 }
END {
  printf "%-22s %-6s %13s %13s %13s %13s   %s\n", \
         "bench", "metric", "baseline", "threshold", "ceiling", "measured", "result"
  printf "%s\n", "----------------------------------------------------------------------------------------------"
  breaches = 0
  for (i = 1; i <= n; i++) {
    name = order[i]
    ceil = base[name] + thr[name]
    if (name in meas) {
      m = meas[name]
      if (m > ceil) { result = "FAIL"; breaches++ } else { result = "PASS" }
      printf "%-22s %-6s %13.3f %13.3f %13.3f %13.3f   %s\n", \
             name, metric[name], base[name], thr[name], ceil, m, result
    } else {
      breaches++
      printf "%-22s %-6s %13.3f %13.3f %13.3f %13s   %s\n", \
             name, metric[name], base[name], thr[name], ceil, "MISSING", "FAIL(no measurement)"
    }
  }
  printf "%s\n", "----------------------------------------------------------------------------------------------"
  printf "%d/%d paths within budget; %d breach(es). All figures in us (p99).\n", \
         n - breaches, n, breaches
  exit (breaches > 0) ? 1 : 0
}
AWK
}

# evaluate MEASURED_FILE -> prints the table; returns 0 (pass) / 1 (breach).
evaluate() {
  local measured_file="$1"
  local rc=0
  set +e
  awk -f "$ENGINE_FILE" "$CONFIG_FILE" "$measured_file"
  rc=$?
  set -e
  return "$rc"
}

# --run: measure each bench, parse its hdrhistogram p99, compare.
run_gate() {
  local report_only="$1"
  local only="$2"
  local measured_file="${TMP}/measured.txt"
  : > "$measured_file"

  local benches="$BENCHES"
  if [ -n "$only" ]; then
    # Restrict BOTH the benches run and the config rows evaluated to the one
    # named path, so the engine reports a single clean row (not MISSING for the
    # others).
    grep -E "^${only}[[:space:]]" "$CONFIG_FILE" > "${TMP}/gate_config_only.txt" || true
    if [ ! -s "${TMP}/gate_config_only.txt" ]; then
      echo "check-perf: --only '${only}' matches no bench in the BENCH.md perf-gate block" >&2
      return 1
    fi
    mv "${TMP}/gate_config_only.txt" "$CONFIG_FILE"
    benches="$only"
  fi

  echo "check-perf: running hot-path bench(es): ${benches} (cargo bench --features bench) ..." >&2
  local b out p99
  for b in $benches; do
    echo "  measuring ${b} ..." >&2
    # shellcheck disable=SC2086 - CRITERION_ARGS is intentionally word-split.
    if ! out="$("$CARGO" bench --features bench --bench "$b" -- $CRITERION_ARGS 2>&1)"; then
      printf '%s\n' "$out" >&2
      echo "check-perf: bench '${b}' failed to run" >&2
      return 1
    fi
    # The hdr report prints exactly one headline p99 line:  "  p99     : N us".
    # The "p99.9" line does not match (a '.' follows p99, not whitespace/colon).
    p99="$(printf '%s\n' "$out" | awk '/^[[:space:]]*p99[[:space:]]*:/{print $3; exit}')"
    if [ -z "$p99" ]; then
      echo "check-perf: could not parse a p99 from bench '${b}' output" >&2
      return 1
    fi
    printf '%s %s\n' "$b" "$p99" >> "$measured_file"
  done
  echo >&2

  local rc=0
  evaluate "$measured_file" || rc=$?

  if [ "$rc" -ne 0 ] && [ "$report_only" = "1" ]; then
    echo
    echo "check-perf: REPORT-ONLY - a breach above is measured against the BENCH.md"
    echo "baseline host (Apple M4 Max). A shared GitHub-hosted runner is a slower,"
    echo "noisier hardware class, so an absolute-p99 breach here is EXPECTED and is"
    echo "NOT treated as a regression. Enforcement runs on baseline-class hardware"
    echo "and, in CI, through the deterministic --self-test gate logic. Exiting 0."
    return 0
  fi
  return "$rc"
}

# --self-test: prove the engine fires, using synthetic measured sets derived
# from the REAL committed baselines/thresholds (also proves BENCH.md parses).
self_test() {
  local fails=0
  local npaths within over missing mixed
  npaths="$(wc -l < "$CONFIG_FILE" | tr -d '[:space:]')"
  within="${TMP}/within.txt"; over="${TMP}/over.txt"
  missing="${TMP}/missing.txt"; mixed="${TMP}/mixed.txt"

  # within: baseline + 0.5*threshold (strictly under the ceiling) -> PASS.
  awk '{ printf "%s %.6f\n", $1, $3 + $4 * 0.5 }' "$CONFIG_FILE" > "$within"
  # over: baseline + 3*threshold + 1 (clearly over the ceiling) -> FAIL.
  awk '{ printf "%s %.6f\n", $1, $3 + $4 * 3 + 1 }' "$CONFIG_FILE" > "$over"
  # missing: drop the first path entirely (a missing measurement must FAIL).
  awk 'NR > 1 { printf "%s %.6f\n", $1, $3 }' "$CONFIG_FILE" > "$missing"
  # mixed: first path over, the rest within (exactly one FAIL).
  awk 'NR == 1 { printf "%s %.6f\n", $1, $3 + $4 * 3 + 1; next }
       { printf "%s %.6f\n", $1, $3 + $4 * 0.5 }' "$CONFIG_FILE" > "$mixed"

  echo "check-perf --self-test: proving the gate FIRES against the committed"
  echo "BENCH.md baselines (${npaths} hot paths). No bench is run here."
  echo

  echo "== 1/4: a within-threshold run must PASS =="
  local rc1=0
  evaluate "$within" || rc1=$?
  if [ "$rc1" -eq 0 ]; then echo "  OK (exit 0)"; else echo "  UNEXPECTED: within-threshold run did not pass"; fails=$((fails + 1)); fi
  echo

  echo "== 2/4: a deliberately slowed run must FAIL on every path =="
  local out2 rc2=0 fc2
  set +e
  out2="$(evaluate "$over")"; rc2=$?
  set -e
  printf '%s\n' "$out2"
  fc2="$(printf '%s\n' "$out2" | grep -c 'FAIL' || true)"
  if [ "$rc2" -ne 0 ] && [ "$fc2" -ge "$npaths" ]; then
    echo "  OK (exit ${rc2}, ${fc2} FAIL row(s))"
  else
    echo "  UNEXPECTED: slowed run should exit non-zero and flag every path (exit ${rc2}, ${fc2} FAIL)"
    fails=$((fails + 1))
  fi
  echo

  echo "== 3/4: a missing measurement must FAIL (never a vacuous pass) =="
  local out3 rc3=0
  set +e
  out3="$(evaluate "$missing")"; rc3=$?
  set -e
  printf '%s\n' "$out3"
  if [ "$rc3" -ne 0 ] && printf '%s\n' "$out3" | grep -q 'MISSING'; then
    echo "  OK (exit ${rc3}, MISSING flagged)"
  else
    echo "  UNEXPECTED: a missing measurement should exit non-zero and be flagged"
    fails=$((fails + 1))
  fi
  echo

  echo "== 4/4: one slow path among healthy ones must FAIL exactly once =="
  local out4 rc4=0 fc4
  set +e
  out4="$(evaluate "$mixed")"; rc4=$?
  set -e
  printf '%s\n' "$out4"
  fc4="$(printf '%s\n' "$out4" | grep -c 'FAIL' || true)"
  if [ "$rc4" -ne 0 ] && [ "$fc4" -eq 1 ]; then
    echo "  OK (exit ${rc4}, exactly 1 FAIL row)"
  else
    echo "  UNEXPECTED: mixed run should exit non-zero with exactly one FAIL (exit ${rc4}, ${fc4} FAIL)"
    fails=$((fails + 1))
  fi
  echo

  if [ "$fails" -eq 0 ]; then
    echo "check-perf --self-test: PASSED - the gate detects a synthetic regression"
    echo "and accepts a within-threshold run. It is wired, not vacuous."
    return 0
  fi
  echo "check-perf --self-test: FAILED - ${fails} expectation(s) not met." >&2
  return 1
}

main() {
  local mode="run" report_only=0 only=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --run) mode="run" ;;
      --self-test) mode="self-test" ;;
      --report-only) report_only=1 ;;
      --only)
        shift
        [ "$#" -gt 0 ] || { echo "check-perf: --only needs a bench name" >&2; exit 2; }
        only="$1" ;;
      -h|--help) usage; exit 0 ;;
      *) echo "check-perf: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
    esac
    shift
  done

  [ -f "$BENCH_MD" ] || { echo "check-perf: BENCH.md not found at ${BENCH_MD}" >&2; exit 1; }
  read_gate_config > "$CONFIG_FILE"
  [ -s "$CONFIG_FILE" ] || {
    echo "check-perf: no perf-gate block parsed from BENCH.md (expected the" >&2
    echo "perf-gate:begin / perf-gate:end markers around the threshold table)." >&2
    exit 1
  }
  write_engine

  case "$mode" in
    run)       run_gate "$report_only" "$only" ;;
    self-test) self_test ;;
  esac
}

main "$@"
