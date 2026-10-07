#!/usr/bin/env bash
# Reproducible CARD-10 parity runs. Not run by CI; kept with the comparator so the
# evidence in /tmp/aad-parity/ can be regenerated.
#
# Prerequisites (see README.md):
#   * /tmp/aad-parity/bin/anomalydetection-testbench-patched (instrumented Go build)
#   * target/release/anomaly-detection-testbench            (Rust build)
#   * tools/aad-parity/configs/*.json
#   * scenario recordings under $SCENARIOS_DIR
set -euo pipefail

SCENARIOS_DIR="${SCENARIOS_DIR:-/home/bits/Documents/observer-scenarios}"
ROOT="${ROOT:-/tmp/aad-parity}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GO_BIN="${GO_BIN:-$ROOT/bin/anomalydetection-testbench-patched}"
RUST_BIN="${RUST_BIN:-$REPO_ROOT/target/release/anomaly-detection-testbench}"

mkdir -p "$ROOT/go" "$ROOT/rust" "$ROOT/comparison"

run_go() { # scenario mode config
  local scenario="$1" mode="$2" config="$3"
  "$GO_BIN" --headless "$scenario" --scenarios-dir "$SCENARIOS_DIR" \
    --config "$REPO_ROOT/tools/aad-parity/configs/$config" \
    --baseline-duration 0 --include-detector-anomalies --retain-parquet \
    --output "$ROOT/go/$scenario-$mode.json" \
    --score-state-output "$ROOT/go/$scenario-$mode-scorestate.json"
}

run_rust() { # scenario mode config
  local scenario="$1" mode="$2" config="$3"
  "$RUST_BIN" --headless "$scenario" --scenarios-dir "$SCENARIOS_DIR" \
    --config "$REPO_ROOT/tools/aad-parity/configs/$config" \
    --baseline-duration 0 --include-detector-anomalies --retain-parquet \
    --output "$ROOT/rust/$scenario-$mode.json"
}

for scenario in kafka-partition-saturation dns-upstream-outage; do
  run_go "$scenario" metricsonly metrics-only.json
  run_rust "$scenario" metricsonly metrics-only.json
  run_go "$scenario" default all-detectors.json
  run_rust "$scenario" default all-detectors.json
done

python3 "$REPO_ROOT/tools/aad-parity/compare.py" --root "$ROOT" \
  --out-json "$ROOT/summary.json" --out-md "$ROOT/summary.md"

for scenario in kafka-partition-saturation dns-upstream-outage; do
  python3 "$REPO_ROOT/tools/aad-parity/crosscheck_artifacts.py" \
    --scenario "$scenario" --scenarios-dir "$SCENARIOS_DIR" \
    --rust "$ROOT/rust/$scenario-default.json" \
    --out "$ROOT/comparison/$scenario-crosscheck.json"
done
