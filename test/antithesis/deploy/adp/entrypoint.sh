#!/usr/bin/env bash
set -euo pipefail

# Agent Data Plane boot wrapper.
#
# The datadog-yaml-config-gen service generates a per-replay datadog.yaml (post-snapshot) onto
# the shared `agent-config` volume and touches a `ready` sentinel. We block on
# that sentinel, copy the config into place, then exec ADP. docker-compose also
# gates this container on datadog-yaml-config-gen completing successfully, so the wait below
# is a defensive belt-and-suspenders for the file actually landing.

CONFIG_DIR="${AGENT_CONFIG_DIR:-/agent-config}"

tries=120
while [ ! -f "${CONFIG_DIR}/ready" ]; do
  tries=$((tries - 1))
  if [ "${tries}" -le 0 ]; then
    echo "timeout waiting for ${CONFIG_DIR}/ready" >&2
    exit 1
  fi
  sleep 1
done

cp "${CONFIG_DIR}/datadog.yaml" /etc/datadog-agent/datadog.yaml

exec /usr/local/bin/agent-data-plane "$@"
