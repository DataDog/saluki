#!/bin/sh

set -eu

# Updates Protocol Buffers definitions from their source repositories for both Datadog Agent and `agent-payload`.
#
# Expected environment variables:
# - DD_AGENT_GIT_TAG: Git tag for the `DataDog/datadog-agent` repository to pull Protocol Buffers definitions from.
# - AGENT_PAYLOAD_GIT_TAG: Git tag for the `DataDog/agent-payload` repository to pull Protocol Buffers definitions from.

DD_AGENT_GIT_URL="https://github.com/DataDog/datadog-agent.git"
AGENT_PAYLOAD_GIT_URL="https://github.com/DataDog/agent-payload.git"

# Ensure that the required environment variables are set
if [ -z "${DD_AGENT_GIT_TAG:-}" ] || [ -z "${AGENT_PAYLOAD_GIT_TAG:-}" ]; then
    echo "[!]: DD_AGENT_GIT_TAG and AGENT_PAYLOAD_GIT_TAG environment variables must be set. Exiting."
    exit 1
fi

# Create a temporary directory to clone the repositories into.
TMP_DIR=$(mktemp -d)

# Clone the repositories at their respective tags. Do a shallow clone.
echo "[*] Checking out the Datadog Agent repository (${DD_AGENT_GIT_TAG})..."
git -C ${TMP_DIR} -c advice.detachedHead=false clone --quiet --depth 1 --branch ${DD_AGENT_GIT_TAG} ${DD_AGENT_GIT_URL}

echo "[*] Checking out the agent-payload repository (${AGENT_PAYLOAD_GIT_TAG})..."
git -C ${TMP_DIR} -c advice.detachedHead=false clone --quiet --depth 1 --branch ${AGENT_PAYLOAD_GIT_TAG} ${AGENT_PAYLOAD_GIT_URL}

# Copy the relevant Protocol Buffers definitions to the appropriate locations.
echo "[*] Copying Protocol Buffers definitions to lib/datadog-protos..."

mkdir -p lib/datadog-protos/proto/datadog-agent
cp -R ${TMP_DIR}/datadog-agent/pkg/proto/datadog lib/datadog-protos/proto/datadog-agent/

mkdir -p lib/datadog-protos/proto/agent-payload
cp ${TMP_DIR}/agent-payload/proto/metrics/agent_payload.proto lib/datadog-protos/proto/agent-payload/

# Update the README.md files to specify what version the definitions are based on.
cat <<EOF > lib/datadog-protos/proto/datadog-agent/README.md
# Datadog Agent Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for the Datadog Agent.

## Source

**Repository:** ${DD_AGENT_GIT_URL}
**Branch / Tag**: ${DD_AGENT_GIT_TAG}
EOF

cat <<EOF > lib/datadog-protos/proto/agent-payload/README.md
# Agent Payload Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for the public Datadog APIs that the Datadog
Agent/Agent Data Plane send telemetry payloads to.

## Source

**Repository:** ${AGENT_PAYLOAD_GIT_URL}
**Branch / Tag**: ${AGENT_PAYLOAD_GIT_TAG}
EOF

# Clean up the temporary directory.
rm -rf ${TMP_DIR}

echo "[*] Done."
