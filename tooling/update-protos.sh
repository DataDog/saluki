#!/bin/sh

set -eu

# Updates Protocol Buffers definitions from their source repositories.
#
# Expected environment variables:
# - DD_AGENT_GIT_TAG: Git tag for the `DataDog/datadog-agent` repository to pull Protocol Buffers definitions from.
# - AGENT_PAYLOAD_GIT_TAG: Git tag for the `DataDog/agent-payload` repository to pull Protocol Buffers definitions from.
# - CONTAINERD_GIT_TAG: Git tag for the `containerd/containerd` repository to pull Protocol Buffers definitions from.

DD_AGENT_GIT_URL="https://github.com/DataDog/datadog-agent.git"
AGENT_PAYLOAD_GIT_URL="https://github.com/DataDog/agent-payload.git"
CONTAINERD_GIT_URL="https://github.com/containerd/containerd.git"

# Ensure that the required environment variables are set
if [ -z "${DD_AGENT_GIT_TAG:-}" ] || [ -z "${AGENT_PAYLOAD_GIT_TAG:-}" ] || [ -z "${CONTAINERD_GIT_TAG:-}" ]; then
    echo "[!]: DD_AGENT_GIT_TAG, AGENT_PAYLOAD_GIT_TAG, and CONTAINERD_GIT_TAG environment variables must be set. Exiting."
    exit 1
fi

# Create a temporary directory to clone the repositories into.
TMP_DIR=$(mktemp -d)

# Clone the repositories at their respective tags. Do a shallow clone.
echo "[*] Checking out the Datadog Agent repository (${DD_AGENT_GIT_TAG})..."
git -C ${TMP_DIR} -c advice.detachedHead=false clone --quiet --depth 1 --branch ${DD_AGENT_GIT_TAG} ${DD_AGENT_GIT_URL}

echo "[*] Checking out the agent-payload repository (${AGENT_PAYLOAD_GIT_TAG})..."
git -C ${TMP_DIR} -c advice.detachedHead=false clone --quiet --depth 1 --branch ${AGENT_PAYLOAD_GIT_TAG} ${AGENT_PAYLOAD_GIT_URL}

echo "[*] Checking out the containerd repository (${CONTAINERD_GIT_TAG})..."
git -C ${TMP_DIR} -c advice.detachedHead=false clone --quiet --depth 1 --branch ${CONTAINERD_GIT_TAG} ${CONTAINERD_GIT_URL}

# Copy the relevant Protocol Buffers definitions to the appropriate locations.
echo "[*] Copying Protocol Buffers definitions to lib/protos/*..."

## Datadog Agent and Agent-related definitions.
mkdir -p lib/protos/datadog/proto/datadog-agent
cp -R ${TMP_DIR}/datadog-agent/pkg/proto/datadog lib/protos/datadog/proto/datadog-agent/

mkdir -p lib/protos/datadog/proto/agent-payload
cp ${TMP_DIR}/agent-payload/proto/metrics/agent_payload.proto lib/protos/datadog/proto/agent-payload/

## containerd and containerd-related definitions.
##
## We do a cleanup step at the end to drop all non-proto files and any empty directories after
# removing those non-proto files.
mkdir -p lib/protos/containerd/proto/github.com/containerd/containerd/
cp -R ${TMP_DIR}/containerd/api lib/protos/containerd/proto/github.com/containerd/containerd/

find lib/protos/containerd/proto/ -type f  ! -name "*.proto" -delete

# Update the README.md files to specify what version the definitions are based on.
cat <<EOF > lib/protos/datadog/proto/datadog-agent/README.md
# Datadog Agent Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for the Datadog Agent.

## Source

**Repository:** ${DD_AGENT_GIT_URL}
**Branch / Tag**: ${DD_AGENT_GIT_TAG}
EOF

cat <<EOF > lib/protos/datadog/proto/agent-payload/README.md
# Agent Payload Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for the public Datadog APIs that the Datadog
Agent/Agent Data Plane send telemetry payloads to.

## Source

**Repository:** ${AGENT_PAYLOAD_GIT_URL}
**Branch / Tag**: ${AGENT_PAYLOAD_GIT_TAG}
EOF

cat <<EOF > lib/protos/containerd/proto/github.com/containerd/containerd/README.md
# Containerd Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for containerd.

## Source

**Repository:** ${CONTAINERD_GIT_URL}
**Branch / Tag**: ${CONTAINERD_GIT_TAG}
EOF

# Clean up the temporary directory.
rm -rf ${TMP_DIR}

echo "[*] Done."
