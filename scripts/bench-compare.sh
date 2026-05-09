#!/usr/bin/env bash
# Compare current benchmark performance against a saved baseline.
#
# Usage:
#   scripts/bench-compare.sh                    # compare current vs baseline 'main'
#   scripts/bench-compare.sh -b release-0.1     # compare against a specific baseline
#   scripts/bench-compare.sh --update           # save current results as 'main' baseline
#   scripts/bench-compare.sh --threshold 25     # fail on >25% regression (default: 15)
#   scripts/bench-compare.sh --filter traversal # only run benches matching pattern
#
# Requires `cargo install critcmp`. Like the other optional pre-push tools,
# the script no-ops with a hint if critcmp is missing.
#
# This is intended for *local* pre-release checks on consistent hardware.
# GitHub's shared runners are too noisy for a strict threshold — see
# .github/workflows/bench.yml for capturing a runner baseline instead.
set -euo pipefail

baseline="main"
threshold=15
update=0
filter=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -b|--baseline) baseline="$2"; shift 2;;
    --threshold)   threshold="$2"; shift 2;;
    --filter)      filter="$2"; shift 2;;
    --update)      update=1; shift;;
    -h|--help)
      sed -n '2,15p' "$0"; exit 0;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

if ! command -v critcmp &>/dev/null; then
  echo "[skip] critcmp not found — install with: cargo install --locked critcmp" >&2
  exit 0
fi

if (( update )); then
  echo "==> Saving baseline '$baseline'"
  if [[ -n "$filter" ]]; then
    cargo bench --manifest-path benches-crate/Cargo.toml --target-dir target --bench cypher -- --save-baseline "$baseline" "$filter"
  else
    cargo bench --manifest-path benches-crate/Cargo.toml --target-dir target --bench cypher -- --save-baseline "$baseline"
  fi
  echo
  echo "Baseline '$baseline' saved under target/criterion/."
  echo "It is NOT committed — re-run with --update on the same hardware to refresh."
  exit 0
fi

if [[ ! -d "target/criterion" ]] || ! critcmp --baselines | grep -q "^${baseline}\$"; then
  echo "error: baseline '$baseline' not found." >&2
  echo "       Run with --update first to capture it on this machine." >&2
  exit 1
fi

echo "==> Running current benchmarks (saved as baseline 'current')"
if [[ -n "$filter" ]]; then
  cargo bench --manifest-path benches-crate/Cargo.toml --target-dir target --bench cypher -- --save-baseline current "$filter"
else
  cargo bench --manifest-path benches-crate/Cargo.toml --target-dir target --bench cypher -- --save-baseline current
fi

echo
echo "==> Comparing 'current' vs '$baseline' (threshold: ${threshold}% regression)"

# critcmp 'current' '$baseline' prints a table; the last column shows the ratio.
# We re-parse it: rows where the current/baseline ratio exceeds 1 + threshold/100
# count as regressions.
report=$(critcmp current "$baseline")
echo "$report"
echo

ratio_limit=$(awk -v t="$threshold" 'BEGIN { printf "%.4f", 1 + t/100 }')

regressions=$(echo "$report" | awk -v limit="$ratio_limit" '
  # critcmp output rows look like:
  #   <name>     1.00       <T>±<dev>     <ratio>      <T>±<dev>
  # We pick rows where field 2 is "1.00" (current is the reference column),
  # then compare field 4 (the comparison ratio) against the limit.
  NF >= 5 && $2 == "1.00" {
    ratio = $4 + 0
    if (ratio > 0 && ratio < (1 / limit)) {
      # Lower ratio in the comparison column means the comparison side
      # was *faster*, i.e. current is slower. Flag it.
      printf "  %s — current is %.2fx slower than %s\n", $1, 1/ratio, "baseline"
    }
  }
')

if [[ -n "$regressions" ]]; then
  echo "REGRESSION DETECTED (>${threshold}%):"
  echo "$regressions"
  exit 1
fi

echo "OK — no regressions over ${threshold}% threshold."
