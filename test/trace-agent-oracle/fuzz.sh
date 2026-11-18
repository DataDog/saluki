#!/usr/bin/env bash
# Fuzzes Saluki's trace decoders against the trace-agent's own receiver code.
#
#   test/trace-agent-oracle/fuzz.sh [DATADOG_AGENT_CHECKOUT] [CASES] [TEST_FILTER]
#
# Builds oracle_test.go into a Go test binary, as regenerate.sh runs it, and runs the ignored `parity_*` tests of
# saluki-components with it: CASES payloads per decoder (1000 by default), for the decoders whose test names match
# TEST_FILTER (all by default, for example `parity_v05`). A test fails on the first payload that the decoder and the
# trace-agent decode differently, and prints it, shrunk. To compare one printed body again, set PARITY_BODY to it. Each
# test prints the seed of its run: set PARITY_SEED to it to repeat the run.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
saluki="$(cd "$here/../.." && pwd)"
agent="$(cd "${1:-$HOME/dd/datadog-agent}" && pwd)"
cases="${2:-1000}"
filter="${3:-parity_}"
go_version="$(cat "$agent/.go-version")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [[ "$(uname)" == "Darwin" ]]; then
  export SDKROOT="$(xcrun --show-sdk-path)"
fi

printf '{"Replace": {"%s": "%s"}}\n' "$agent/pkg/trace/api/oracle_test.go" "$here/oracle_test.go" >"$work/overlay.json"
(
  cd "$agent/pkg/trace"
  GOTOOLCHAIN="go$go_version" GOFLAGS=-mod=readonly \
    go test -c -tags test -overlay "$work/overlay.json" -o "$work/oracle.test" ./api
)

# A test that runs past 15 minutes is ended, which prints its output, and so its PARITY_SEED: a run of 50,000 payloads
# per decoder takes under two minutes, and shrinking a failure is bounded at two.
cat >"$work/nextest.toml" <<'TOML'
[profile.default]
slow-timeout = { period = "60s", terminate-after = 15 }
TOML

cd "$saluki"
TRACE_AGENT_ORACLE="$work/oracle.test" PARITY_CASES="$cases" PARITY_BODY="${PARITY_BODY:-}" \
  PARITY_SEED="${PARITY_SEED:-}" \
  cargo nextest run --config-file "$work/nextest.toml" -p saluki-components --run-ignored only --no-fail-fast \
  --success-output final \
  -E "test(/decoders::datadog::parity::${filter}/)"
