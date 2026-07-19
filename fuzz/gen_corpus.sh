#!/usr/bin/env bash
# Generate the fuzz seed corpora from the committed shared fixtures (issue #53,
# docs/TESTING.md §13.4). Seeds are DERIVED from the real provider + replay
# fixtures, not hand-typed bytes, so the fuzzer explores from valid shapes and
# mutates outward. The generated corpus dirs are gitignored (reproducible from
# the fixtures via this script); CI and the local smoke run this before fuzzing.
#
# Each seed is a one-byte SEAM/MEMBER selector followed by a fixture payload,
# matching the leading-byte protocol both fuzz targets use:
#   * fuzz_provider_normalize — byte 0 ticker, 1 book, 2 instrument-name
#   * fuzz_replay_decode       — byte 0 manifest, 1 fills, 2 equity, 3 positions,
#                                4 greeks (the overridden member; rest stay valid)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
fixtures="$root/tests/fixtures"

pn="$here/corpus/fuzz_provider_normalize"
rd="$here/corpus/fuzz_replay_decode"
mkdir -p "$pn" "$rd"

# Prepend selector byte $1 to file $2, writing seed $3.
seed() {
  local sel="$1" src="$2" dst="$3"
  { printf "$(printf '\\x%02x' "$sel")"; cat "$src"; } > "$dst"
}

# --- fuzz_provider_normalize -------------------------------------------------
# Ticker seams (selector 0) from every recorded ticker fixture.
for f in "$fixtures"/deribit/ticker/*.json; do
  seed 0 "$f" "$pn/ticker_$(basename "$f" .json)"
done
# Book seams (selector 1) from every recorded book fixture.
for f in "$fixtures"/deribit/book/*.json; do
  seed 1 "$f" "$pn/book_$(basename "$f" .json)"
done
# Instrument-name seams (selector 2) — a valid name and a malformed one.
printf '\x02BTC-27JUN25-60000-C' > "$pn/name_valid"
printf '\x02ETH-3JAN25-2000-P'   > "$pn/name_valid_put"
printf '\x02garbage-name'        > "$pn/name_malformed"

# --- fuzz_replay_decode ------------------------------------------------------
# For every committed bundle fixture (the valid + adversarial set), seed each
# member with the matching selector byte, so the fuzzer starts from each real
# member shape (valid, oversized_footer, rowcount_lie, truncated, ...).
members=(manifest.json fills.parquet equity_curve.parquet positions.parquet greeks_attribution.parquet)
for dir in "$fixtures"/bundle/*/; do
  bundle="$(basename "$dir")"
  sel=0
  for m in "${members[@]}"; do
    if [ -f "$dir$m" ]; then
      seed "$sel" "$dir$m" "$rd/${bundle}_${m//./_}"
    fi
    sel=$((sel + 1))
  done
done

echo "seeded fuzz_provider_normalize: $(find "$pn" -type f | wc -l | tr -d ' ') files"
echo "seeded fuzz_replay_decode:      $(find "$rd" -type f | wc -l | tr -d ' ') files"
