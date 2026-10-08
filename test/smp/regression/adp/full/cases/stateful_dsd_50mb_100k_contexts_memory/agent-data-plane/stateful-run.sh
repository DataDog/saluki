#!/bin/sh
# Starts one ack-only stateful intake per port in STATEFUL_INTAKE_PORTS, then runs ADP. The intakes drop
# LD_PRELOAD so SMP's profiler only attaches to ADP.
intake=/opt/datadog-agent/embedded/bin/stateful-metrics-blackhole
if [ -x "$intake" ]; then
  for port in $STATEFUL_INTAKE_PORTS; do
    env -u LD_PRELOAD LISTEN_ADDR="127.0.0.1:$port" "$intake" &
  done
fi
exec /opt/datadog-agent/embedded/bin/agent-data-plane --config /etc/agent-data-plane/empty.yaml run
