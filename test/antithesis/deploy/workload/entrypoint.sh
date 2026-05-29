#!/usr/bin/env bash
set -euo pipefail

# Workload client entrypoint.
#
# By the time this runs, docker-compose has gated startup on the `adp` and `intake` services being
# healthy (depends_on: condition: service_healthy). We re-confirm reachability defensively, emit the
# Antithesis `setup_complete` signal, then idle so Antithesis can run test commands from the test
# template at /opt/antithesis/test/v1/.

ADP_HOST="${ADP_HOST:-adp}"
ADP_API_PORT="${ADP_API_PORT:-5100}"
DSD_SOCKET="${DSD_SOCKET:-/var/run/datadog/dsd.socket}"
INTAKE_HOST="${INTAKE_HOST:-intake}"
INTAKE_PORT="${INTAKE_PORT:-2049}"

wait_for_tcp() {
  local host="$1" port="$2" name="$3" tries=60
  echo "Waiting for ${name} (${host}:${port})..."
  while (( tries-- > 0 )); do
    if (exec 3<>"/dev/tcp/${host}/${port}") 2>/dev/null; then
      echo "${name} is reachable."
      return 0
    fi
    sleep 1
  done
  echo "Timed out waiting for ${name} (${host}:${port})." >&2
  return 1
}

wait_for_socket() {
  local path="$1" name="$2" tries=60
  echo "Waiting for ${name} (${path})..."
  while (( tries-- > 0 )); do
    if [[ -S "${path}" ]]; then
      echo "${name} is reachable."
      return 0
    fi
    sleep 1
  done
  echo "Timed out waiting for ${name} (${path})." >&2
  return 1
}

wait_for_tcp "${ADP_HOST}" "${ADP_API_PORT}" "agent-data-plane API"
wait_for_socket "${DSD_SOCKET}" "agent-data-plane DogStatsD socket"
wait_for_tcp "${INTAKE_HOST}" "${INTAKE_PORT}" "datadog-intake"

echo "System is ready. Emitting setup_complete."
/opt/antithesis/setup-complete.sh

echo "Workload client idle; awaiting Antithesis test commands."
exec tail -f /dev/null
