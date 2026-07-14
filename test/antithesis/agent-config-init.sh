#!/usr/bin/env bash
set -euo pipefail

# Converged Datadog Agent boot config, shared by every scenario. first_sample_config
# writes this timeline's datadog.yaml and a `ready` sentinel to the shared
# agent-config volume. Block on the sentinel, then copy the config into place so
# both the Core Agent and the embedded ADP boot under the sampled config.
CONFIG_DIR="${AGENT_CONFIG_DIR:-/agent-config}"

echo "datadog-agent: waiting for ${CONFIG_DIR}/ready" >&2
while [ ! -f "${CONFIG_DIR}/ready" ]; do
  sleep 1
done

cp "${CONFIG_DIR}/datadog.yaml" /etc/datadog-agent/datadog.yaml
