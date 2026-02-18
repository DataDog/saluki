#!/bin/bash

# ADP must be baseline enabled to continue.
if [[ "${DD_DATA_PLANE_ENABLED}" != "true" ]]; then
  exit 0
fi

mkdir -p /run/agent/env

# When ADP is handling OTLP, redirect Agent's OTLP receivers to unused localhost ports
# so ADP can bind to the actual ports (4317, 4318)
if [[ "${DD_DATA_PLANE_OTLP_ENABLED}" == "true" ]]; then
    # Agent listens on unused local ports
    printf "127.0.0.1:14317" > /run/agent/env/DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT
    printf "127.0.0.1:14318" > /run/agent/env/DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT
fi
