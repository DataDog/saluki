#!/usr/bin/env bash
#
# Print the Linux integration cases that one CI job runs, as the comma-separated list that `panoramic run -t` takes.
#
# Usage: integration-cases.sh <otlp|shared>
#   otlp   - the cases that check the answers the OTLP/HTTP receivers give to clients, in the `test-integration-otlp` job
#   shared - every other Linux integration case, which runs in the `test-integration` job
#
# OTLP_CASES is the only list. It holds the cases that send requests to the receivers of ADP and of the Agent and check
# the answers that clients get, such as status, headers and body, not every case that sends data through a receiver. The
# other OTLP cases, such as `otlp-tls`, `otlp-traces-enabled` and `otlp-routing-mixed-http`, stay in the shared job.
# The shared job takes each case that the list does not name, so each case runs in one job.
# A name in the list that no case has fails the `test-integration-otlp` job, because `panoramic run -t` rejects it.
#
# Run it from the repository root, after `make build-panoramic`.

set -euo pipefail

OTLP_CASES=(otlp-rcv-http)

case "${1:?expected otlp or shared}" in
    otlp)
        (IFS=,; echo "${OTLP_CASES[*]}")
        ;;
    shared)
        ./target/release/panoramic list -d test/integration/cases --json --subject-os linux \
            | jq -r 'keys[]' \
            | grep -vxFf <(printf '%s\n' "${OTLP_CASES[@]}") \
            | paste -sd, -
        ;;
    *)
        echo "expected otlp or shared, not '$1'" >&2
        exit 1
        ;;
esac
