#!/bin/bash

# TODO: Remove this once https://github.com/DataDog/datadog-agent/pull/43876 lands and is released,
# since the Core Agent will handle this itself.

# ADP must be baseline enabled to continue.
if [[ "${DD_DATA_PLANE_ENABLED}" != "true" ]]; then
  exit 0
fi

mkdir -p /run/agent/env
mkdir -p /run/adp/env

# Process ADP_DD_* environment variables for ADP-specific config overrides
# Find all env vars starting with ADP_DD_ and strip the ADP_ prefix
env | grep '^ADP_DD_' | while IFS='=' read -r key value; do
    # Strip ADP_ prefix: ADP_DD_OTLP_CONFIG_... → DD_OTLP_CONFIG_...
    new_key="${key#ADP_}"
    printf "%s" "$value" > "/run/adp/env/$new_key"
done

# Keep the Core Agent's OTLP listeners from contending with ADP.
#
# TODO(#2177): Remove this redirect when ADP returns to the canonical schema-provided endpoints and
# OTLP listener ownership prevents port contention.
if [[ "${DD_DATA_PLANE_OTLP_ENABLED}" == "true" ]]; then
    # Agent listens on unused local ports
    printf "127.0.0.1:14317" > /run/agent/env/DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT
    printf "127.0.0.1:14318" > /run/agent/env/DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT
fi
