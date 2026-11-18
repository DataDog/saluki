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

python3 -I "$here/v04_json_cases.py" cases >"$work/v04_json_in.json"
(
  cd "$agent/pkg/trace"
  GOTOOLCHAIN="go$go_version" ORACLE_IN="$work/v04_json_in.json" ORACLE_OUT="$work/v04_json_out.json" \
    go test -tags test -count=1 -overlay "$work/overlay.json" -run '^TestOracle$' ./api
)
python3 -I "$here/v04_json_cases.py" fixture "$work/v04_json_out.json" \
  >"$saluki/lib/saluki-components/src/decoders/datadog/testdata/v04_json_cases.json"
echo "Regenerated v04_json_cases.json with the trace-agent at $(git -C "$agent" rev-parse --short HEAD), Go $go_version."
