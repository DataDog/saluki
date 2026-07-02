#!/usr/bin/env bash
set -euo pipefail

CONFIG_DIR="${AGENT_CONFIG_DIR:-/agent-config}"

echo "datadog-agent: waiting for ${CONFIG_DIR}/ready" >&2
while [ ! -f "${CONFIG_DIR}/ready" ]; do
  sleep 1
done

cp "${CONFIG_DIR}/datadog.yaml" /etc/datadog-agent/datadog.yaml
