#!/usr/bin/env bash
# Reproduces the tokio 1.51 OTLP ingest throughput regression caused by
# tokio PR #7431 ("steal tasks from the LIFO slot").
#
# Builds ADP and millstone in release mode, runs the OTLP ingest benchmark
# against two tokio versions, and prints a throughput comparison.
#
# Usage:
#   ./test/bench/bench-tokio-regression.sh
#   ./test/bench/bench-tokio-regression.sh 1.50.0 1.51.0   # explicit versions
#
# Requirements: Rust toolchain, Python 3, openssl, nc
#
# The regression is most visible on Linux with 4+ CPU cores. On macOS the
# effect is smaller (~10-15%) due to better cache coherency between cores.

set -euo pipefail

TOKIO_A=${1:-1.50.0}
TOKIO_B=${2:-1.51.0}
WARMUP=3
RUNS=10

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BENCH_DIR="$ROOT/test/bench"
RESULTS_A=""
RESULTS_B=""

if uname | grep -q Darwin; then SED_INPLACE="sed -i ''"; else SED_INPLACE="sed -i"; fi

cleanup_versions() {
    cd "$ROOT" && git checkout Cargo.toml Cargo.lock 2>/dev/null || true
}
trap cleanup_versions EXIT

run_version() {
    local version=$1
    cd "$ROOT"

    $SED_INPLACE "s/tokio = { version = \"[^\"]*\"/tokio = { version = \"$version\"/" Cargo.toml
    cargo update tokio --precise "$version" -q

    "$BENCH_DIR/otlp-ingest.sh" --warmup "$WARMUP" --runs "$RUNS" 2>&1
}

echo "┌─────────────────────────────────────────────────────────┐"
echo "│  tokio LIFO-stealing throughput regression (PR #7431)   │"
echo "│  OTLP trace ingest via gRPC — $RUNS runs each, $WARMUP warmup       │"
echo "└─────────────────────────────────────────────────────────┘"
echo ""

echo "▶ tokio $TOKIO_A"
OUTPUT_A=$(run_version "$TOKIO_A")
echo "$OUTPUT_A" | grep -E "run [0-9]+:|n=[0-9]"
MEDIAN_A=$(echo "$OUTPUT_A" | grep "median=" | grep -oE 'median=[0-9.]+' | cut -d= -f2)

echo ""
echo "▶ tokio $TOKIO_B"
OUTPUT_B=$(run_version "$TOKIO_B")
echo "$OUTPUT_B" | grep -E "run [0-9]+:|n=[0-9]"
MEDIAN_B=$(echo "$OUTPUT_B" | grep "median=" | grep -oE 'median=[0-9.]+' | cut -d= -f2)

# Compute regression %
REGRESSION=$(python3 -c "
a, b = float('$MEDIAN_A'), float('$MEDIAN_B')
pct = (b - a) / a * 100
sign = '+' if pct >= 0 else ''
print(f'{sign}{pct:.1f}%')
")

echo ""
echo "┌─────────────────────────────────────┐"
printf "│  tokio %-8s  median: %6s MB/s  │\n" "$TOKIO_A" "$MEDIAN_A"
printf "│  tokio %-8s  median: %6s MB/s  │\n" "$TOKIO_B" "$MEDIAN_B"
echo "│                                     │"
printf "│  regression: %24s  │\n" "$REGRESSION"
echo "└─────────────────────────────────────┘"
