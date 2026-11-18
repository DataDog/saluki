#!/usr/bin/env bash
# Regenerates Saluki's trace decoder fixtures from the trace-agent's own receiver code.
#
#   test/trace-agent-oracle/regenerate.sh [DATADOG_AGENT_CHECKOUT]
#
# The checkout defaults to ~/dd/datadog-agent and is never written to: oracle_test.go is overlaid into its
# pkg/trace/api package with `go test -overlay`. The Go toolchain is the one the checkout pins in .go-version,
# downloaded by `GOTOOLCHAIN` if needed. Requires python3 and, on macOS, Xcode's SDK for cgo.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
saluki="$(cd "$here/../.." && pwd)"
agent="$(cd "${1:-$HOME/dd/datadog-agent}" && pwd)"
go_version="$(cat "$agent/.go-version")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [[ "$(uname)" == "Darwin" ]]; then
  export SDKROOT="$(xcrun --show-sdk-path)"
fi

printf '{"Replace": {"%s": "%s"}}\n' "$agent/pkg/trace/api/oracle_test.go" "$here/oracle_test.go" >"$work/overlay.json"

fixtures="$saluki/lib/saluki-components/src/decoders/datadog/testdata"
for cases in v04_json v01_json; do
  python3 -I "$here/${cases}_cases.py" cases >"$work/${cases}_in.json"
  (
    cd "$agent/pkg/trace"
    GOTOOLCHAIN="go$go_version" GOFLAGS=-mod=readonly \
      ORACLE_IN="$work/${cases}_in.json" ORACLE_OUT="$work/${cases}_out.json" \
      go test -tags test -count=1 -overlay "$work/overlay.json" -run '^TestOracle$' ./api
  )
  # Written whole, then moved into place, so that a failure leaves the committed fixture as it was.
  python3 -I "$here/${cases}_cases.py" fixture "$work/${cases}_out.json" >"$work/${cases}_cases.json"
  mv "$work/${cases}_cases.json" "$fixtures/${cases}_cases.json"
  echo "Regenerated ${cases}_cases.json with the trace-agent at $(git -C "$agent" rev-parse --short HEAD), Go $go_version."
done
