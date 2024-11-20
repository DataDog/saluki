#!/bin/bash

if [[ "${DD_ADP_ENABLED}" != "true" ]]; then
  exit 0
fi

# Enables the agent-data-plane s6 service.
mkdir -p /etc/datadog-agent/adp
touch /etc/datadog-agent/adp/enabled

# Sets an environment variable override for the core agent to disable DSD.
mkdir -p /run/agent/env
printf "0" > /run/agent/env/DD_USE_DOGSTATSD
