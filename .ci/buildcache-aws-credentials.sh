#!/bin/sh

# Don't run the script if we're not in CI.
if [ "${CI}" != "true" ]; then
  exit 0
fi

# Get our credentials from IMDS and write them out so we can source them into the environment during the build step.
eval $(aws configure export-credentials --format env)
