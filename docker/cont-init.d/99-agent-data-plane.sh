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
    # Strip ADP_ prefix: ADP_DD_AGGREGATE_CONTEXT_LIMIT → DD_AGGREGATE_CONTEXT_LIMIT
    new_key="${key#ADP_}"
    printf "%s" "$value" > "/run/adp/env/$new_key"
done
