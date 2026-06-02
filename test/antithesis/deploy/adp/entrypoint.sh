#!/usr/bin/env bash
set -euo pipefail

# Agent Data Plane boot wrapper.
#
# first_sample_config writes this timeline's datadog.yaml + a `ready` sentinel to
# the shared volume; we block on it, copy the config, then `exec` one stable ADP.
# We block indefinitely rather than timing out and exiting non-zero, which would
# be read as an ADP crash. The startup log below makes the wait visible in triage,
# so a missing release shows as "waiting…" with no boot rather than a silent hang.

CONFIG_DIR="${AGENT_CONFIG_DIR:-/agent-config}"

echo "adp: waiting for ${CONFIG_DIR}/ready (released by first_sample_config)" >&2
while [ ! -f "${CONFIG_DIR}/ready" ]; do
  sleep 1
done

cp "${CONFIG_DIR}/datadog.yaml" /etc/datadog-agent/datadog.yaml

exec /usr/local/bin/agent-data-plane "$@"
