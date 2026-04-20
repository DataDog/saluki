#!/usr/bin/env bash
# Benchmarks OTLP trace ingest throughput by running millstone against a local ADP instance.
#
# Builds ADP and millstone in release mode, starts ADP with a fake HTTP intake to absorb
# forwarded traces, runs a warm-up phase to stabilise JIT/cache effects, then records
# throughput over a configurable number of measurement runs.
#
# Usage:
#   ./test/bench/otlp-ingest.sh [--warmup N] [--runs N]
#
#   --warmup N   warm-up runs before measurement (default: 3, discarded)
#   --runs   N   measurement runs to record     (default: 10)
#
# Typical workflow for comparing two tokio versions:
#   # version A
#   sed -i '' 's/tokio = { version = "[^"]*"/tokio = { version = "1.50.0"/' Cargo.toml
#   cargo update tokio --precise 1.50.0
#   ./test/bench/otlp-ingest.sh
#
#   # version B
#   sed -i '' 's/tokio = { version = "[^"]*"/tokio = { version = "1.51.0"/' Cargo.toml
#   cargo update tokio --precise 1.51.0
#   ./test/bench/otlp-ingest.sh
#
# Or use the Makefile target:
#   make bench-otlp-ingest-compare TOKIO_A=1.50.0 TOKIO_B=1.51.0

set -euo pipefail

WARMUP_RUNS=3
MEASURE_RUNS=10

while [[ $# -gt 0 ]]; do
    case $1 in
        --warmup) WARMUP_RUNS=$2; shift 2 ;;
        --runs)   MEASURE_RUNS=$2; shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BENCH_DIR="$ROOT/test/bench"

ADP_BIN="$ROOT/target/release/agent-data-plane"
MILLSTONE_BIN="$ROOT/target/release/millstone"

ADP_PID=""
INTAKE_PID=""

cleanup() {
    [[ -n "$ADP_PID"    ]] && kill "$ADP_PID"    2>/dev/null || true
    [[ -n "$INTAKE_PID" ]] && kill "$INTAKE_PID" 2>/dev/null || true
    # Belt-and-suspenders: free the ports in case PIDs already exited
    fuser -k 4317/tcp 2>/dev/null || true
    fuser -k 2049/tcp 2>/dev/null || true
}
trap cleanup EXIT

# ── Build ─────────────────────────────────────────────────────────────────────
# Uses --profile release (debug=true, lto=thin, codegen-units=8) to match the
# profile used by SMP regression tests and profile-run-adp in the Makefile.
# Note: SMP tests run on Linux with jemalloc; locally on macOS the system
# allocator is used instead. Relative differences between versions still hold.

echo "Building release binaries..."
cargo build --profile release --bin agent-data-plane --bin millstone -q

TOKIO_VER=$(grep -A2 'name = "tokio"' "$ROOT/Cargo.lock" | grep 'version' | head -1 | awk '{print $3}' | tr -d '"')
echo "tokio $TOKIO_VER"

# ── IPC cert ─────────────────────────────────────────────────────────────────
# ADP requires an IPC cert file for its privileged API even in standalone mode.
# Generate a self-signed dummy cert if one doesn't already exist.

IPC_CERT_FILE="/tmp/adp-bench-ipc-cert.pem"
if [[ ! -f "$IPC_CERT_FILE" ]]; then
    openssl req -x509 -newkey rsa:2048 -keyout "$IPC_CERT_FILE" -out "$IPC_CERT_FILE" \
        -days 3650 -nodes -subj "/CN=adp-bench" 2>/dev/null
fi

# ── Fake intake ───────────────────────────────────────────────────────────────
# Absorbs forwarded traces so ADP's pipeline doesn't back up on a closed socket.

python3 "$BENCH_DIR/intake-blackhole.py" &
INTAKE_PID=$!
sleep 0.5

# ── Start ADP ─────────────────────────────────────────────────────────────────

DD_IPC_CERT_FILE_PATH="$IPC_CERT_FILE" \
"$ADP_BIN" -c "$BENCH_DIR/adp-otlp.yaml" run > /tmp/adp-bench.log 2>&1 &
ADP_PID=$!

echo "Waiting for ADP on port 4317..."
for i in $(seq 1 30); do
    if ! kill -0 "$ADP_PID" 2>/dev/null; then
        echo "ADP exited unexpectedly:" >&2
        cat /tmp/adp-bench.log >&2
        exit 1
    fi
    nc -z 127.0.0.1 4317 2>/dev/null && { echo "ADP ready."; break; }
    [[ $i -eq 30 ]] && { echo "Timed out waiting for ADP:"; cat /tmp/adp-bench.log; exit 1; }
    sleep 1
done
sleep 1  # let gRPC listener fully settle

# ── Warm-up ───────────────────────────────────────────────────────────────────
# Discarded runs that allow ADP's thread pools, allocator, and gRPC connection
# state to reach steady state before we start recording.

echo ""
echo "Warm-up ($WARMUP_RUNS runs, discarded)..."
for i in $(seq 1 "$WARMUP_RUNS"); do
    "$MILLSTONE_BIN" "$BENCH_DIR/millstone-otlp.yaml" > /dev/null 2>&1
done

# ── Measurement ───────────────────────────────────────────────────────────────

echo "Measuring ($MEASURE_RUNS runs)..."
echo ""

THROUGHPUTS_RAW=()  # numeric MB/s values for statistics
THROUGHPUTS_LABEL=()

for run in $(seq 1 "$MEASURE_RUNS"); do
    out=$("$MILLSTONE_BIN" "$BENCH_DIR/millstone-otlp.yaml" 2>&1)
    label=$(echo "$out" | grep -oE '\([0-9.]+ [KMG]?B/s\)' | tr -d '()')
    dur=$(echo   "$out" | grep -oE 'over [0-9.]+[a-zµ]+' | head -1 | sed 's/over //')
    # Extract numeric value in MB/s (convert KB/s if needed)
    num=$(echo "$label" | grep -oE '[0-9.]+')
    unit=$(echo "$label" | grep -oE '[KMG]?B/s')
    case "$unit" in
        KB/s)  num=$(echo "$num / 1000" | bc -l) ;;
        GB/s)  num=$(echo "$num * 1000" | bc -l) ;;
    esac
    printf "  run %2d: %-12s (%s)\n" "$run" "$label" "$dur"
    THROUGHPUTS_RAW+=("$num")
    THROUGHPUTS_LABEL+=("$label")
done

# ── Summary statistics ────────────────────────────────────────────────────────

python3 - "${THROUGHPUTS_RAW[@]}" <<'PYEOF'
import sys, statistics
vals = [float(x) for x in sys.argv[1:]]
print()
print(f"  n={len(vals)}  mean={statistics.mean(vals):.1f} MB/s  "
      f"median={statistics.median(vals):.1f} MB/s  "
      f"stdev={statistics.stdev(vals):.1f} MB/s  "
      f"min={min(vals):.1f}  max={max(vals):.1f}")
PYEOF
