#!/bin/bash

if [[ "${DD_DATA_PLANE_ENABLED}" != "true" ]]; then
  exit 0
fi

# Sets an environment variable override for the Core Agent to disable DSD.
mkdir -p /run/agent/env
printf "0" > /run/agent/env/DD_USE_DOGSTATSD
