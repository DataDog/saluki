#!/bin/bash

# ADP must be baseline enabled to continue.
if [[ "${DD_DATA_PLANE_ENABLED}" != "true" ]]; then
  exit 0
fi

mkdir -p /run/agent/env

# When ADP is handling DSD, disable DSD in the Core Agent.
if [[ "${DD_DATA_PLANE_DOGSTATSD_ENABLED}" == "true" ]]; then
    printf "0" > /run/agent/env/DD_USE_DOGSTATSD
fi
