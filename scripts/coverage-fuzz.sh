#!/usr/bin/env bash
set -euo pipefail -o xtrace
cd "$(git rev-parse --show-toplevel)"

# Usage: ./scripts/coverage-fuzz.sh [<num>]
#   <num>  Optional: randomly sample this many corpus inputs for a faster run.
#          Omit to run the full corpus (~30–45 min).

NUM_SAMPLES="${1:-}"
CORPUS_DIR="fuzz/corpus/apd"


# --- Prerequisites ---
rustup component add llvm-tools-preview --toolchain nightly
if ! command -v grcov &>/dev/null; then
  echo "Installing grcov..."
  cargo install grcov
fi


HOST_TRIPLET=$(rustc -vV | sed -n 's|host: ||p')
LLVM_PROFDATA=$(rustup run nightly rustc --print sysroot)/lib/rustlib/${HOST_TRIPLET}/bin/llvm-profdata
if ! command -v $LLVM_PROFDATA &>/dev/null; then
  echo could not find llvm-profdata
  exit 1
fi

# --- Optional: sample a subset of the corpus ---
if [[ -n "$NUM_SAMPLES" ]]; then
  WORK_CORPUS=$(mktemp -d)
  trap 'rm -rf "$WORK_CORPUS"' EXIT
  echo "Sampling $NUM_SAMPLES inputs from $CORPUS_DIR → $WORK_CORPUS"
  ls "$CORPUS_DIR" | shuf -n "$NUM_SAMPLES" | xargs -I{} cp "$CORPUS_DIR/{}" "$WORK_CORPUS/"
  echo "Running $NUM_SAMPLES inputs (~$((NUM_SAMPLES * 14 / 60))m estimated)"
else
  CORPUS_COUNT=$(ls "$CORPUS_DIR" | wc -l)
  echo "Running $CORPUS_COUNT inputs (~$((CORPUS_COUNT * 14 / 60))m estimated)"
  WORK_CORPUS="$CORPUS_DIR"
fi

# --- Step 1: clean stale profraw files, then run corpus ---
echo "Cleaning stale profraw files..."
rm -f fuzz/coverage/apd/raw/*.profraw
echo "Running corpus through coverage-instrumented target..."
RUSTFLAGS="--cfg tokio_unstable" cargo +nightly fuzz coverage apd "$WORK_CORPUS"

# --- Step 2: merge profraw → profdata ---
PROFDATA="coverage-$(date +%Y%m%d-%H%M%S).profdata"
echo "Merging profraw files → $PROFDATA..."
"$LLVM_PROFDATA" merge fuzz/coverage/apd/raw/*.profraw -o "$PROFDATA"

# --- Step 3: generate HTML (ADP only, exclude intake and fuzz harness) ---
echo "Generating HTML coverage report..."
grcov "$PROFDATA" \
  -s . \
  --binary-path "target/${HOST_TRIPLET}/coverage/${HOST_TRIPLET}/release/" \
  -t html \
  --ignore-not-existing \
  --llvm \
  --precision 1 \
  --ignore "bin/correctness/**" \
  --ignore "fuzz/**" \
  --ignore "target/**" \
  -o htmlcoverage \
  --threads 4

echo "Done. Report: htmlcoverage/index.html"
