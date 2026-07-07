#!/usr/bin/env bash
set -euo pipefail

# Agent Data Plane boot wrapper for the differential scenario.
#
# first_sample_config writes this timeline's datadog.yaml and a
# ready sentinel under the shared agent-config volume. We block until that
# config exists, copy it into the path ADP reads, then exec one stable ADP
# process.

CONFIG_DIR="${AGENT_CONFIG_DIR:-/agent-config}"

echo "adp: waiting for ${CONFIG_DIR}/ready (released by first_sample_config)" >&2
while [ ! -f "${CONFIG_DIR}/ready" ]; do
  sleep 1
done

cp "${CONFIG_DIR}/datadog.yaml" /etc/datadog-agent/datadog.yaml

exec /usr/local/bin/agent-data-plane "$@"
