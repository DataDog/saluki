#!/bin/sh

set -eu

# Updates Protocol Buffers definitions from their source repositories.
#
# Expected environment variables:
# - DD_AGENT_GIT_TAG: Git tag, branch, or full commit SHA for the `DataDog/datadog-agent` repository to pull Protocol Buffers definitions from.
# - AGENT_PAYLOAD_GIT_TAG: Git tag, branch, or full commit SHA for the `DataDog/agent-payload` repository to pull Protocol Buffers definitions from.
# - CONTAINERD_GIT_TAG: Git tag, branch, or full commit SHA for the `containerd/containerd` repository to pull Protocol Buffers definitions from.
# - SKETCHES_GO_GIT_TAG: Git tag, branch, or full commit SHA for the `DataDog/sketches-go` repository to pull Protocol Buffers definitions from.

DD_AGENT_GIT_URL="https://github.com/DataDog/datadog-agent.git"
AGENT_PAYLOAD_GIT_URL="https://github.com/DataDog/agent-payload.git"
CONTAINERD_GIT_URL="https://github.com/containerd/containerd.git"
SKETCHES_GO_GIT_URL="https://github.com/DataDog/sketches-go.git"

# Ensure that the required environment variables are set
if [ -z "${DD_AGENT_GIT_TAG:-}" ] || [ -z "${AGENT_PAYLOAD_GIT_TAG:-}" ] || [ -z "${CONTAINERD_GIT_TAG:-}" ] || [ -z "${SKETCHES_GO_GIT_TAG:-}" ]; then
    echo "[!]: DD_AGENT_GIT_TAG, AGENT_PAYLOAD_GIT_TAG, CONTAINERD_GIT_TAG, and SKETCHES_GO_GIT_TAG environment variables must be set. Exiting."
    exit 1
fi

# Create a temporary directory to clone the repositories into.
TMP_DIR=$(mktemp -d)

# Clones `url` into `${TMP_DIR}` at `ref`, which is a branch, a tag, or a full commit SHA. Does a shallow clone.
#
# A commit SHA can't be passed to `git clone --branch`, so those are fetched directly instead. This lets a pin
# reference a commit that no branch or tag contains yet.
clone_at_ref() {
    url="$1"
    ref="$2"
    name=$(basename "${url}" .git)

    if printf '%s' "${ref}" | grep -Eq '^[0-9a-f]{40}$'; then
        git -C "${TMP_DIR}" init --quiet "${name}"
        git -C "${TMP_DIR}/${name}" remote add origin "${url}"
        git -C "${TMP_DIR}/${name}" fetch --quiet --depth 1 origin "${ref}"
        git -C "${TMP_DIR}/${name}" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
    else
        git -C "${TMP_DIR}" -c advice.detachedHead=false clone --quiet --depth 1 --branch "${ref}" "${url}"
    fi
}

# Clone the repositories at their respective refs.
echo "[*] Checking out the Datadog Agent repository (${DD_AGENT_GIT_TAG})..."
clone_at_ref ${DD_AGENT_GIT_URL} ${DD_AGENT_GIT_TAG}

echo "[*] Checking out the agent-payload repository (${AGENT_PAYLOAD_GIT_TAG})..."
clone_at_ref ${AGENT_PAYLOAD_GIT_URL} ${AGENT_PAYLOAD_GIT_TAG}

echo "[*] Checking out the containerd repository (${CONTAINERD_GIT_TAG})..."
clone_at_ref ${CONTAINERD_GIT_URL} ${CONTAINERD_GIT_TAG}

echo "[*] Checking out the sketches-go repository (${SKETCHES_GO_GIT_TAG})..."
clone_at_ref ${SKETCHES_GO_GIT_URL} ${SKETCHES_GO_GIT_TAG}

# Copy the relevant Protocol Buffers definitions to the appropriate locations.
echo "[*] Copying Protocol Buffers definitions to lib/protos/*..."

## Datadog Agent and Agent-related definitions.
##
## We drop all non-proto files afterwards: the Agent's tree carries `BUILD.bazel` files that are
## meaningless here, and which break downstream Bazel builds of this crate by introducing extra
## Bazel package boundaries that hide the definitions from the crate's own glob.
mkdir -p lib/protos/datadog/proto/datadog-agent
cp -R ${TMP_DIR}/datadog-agent/pkg/proto/datadog lib/protos/datadog/proto/datadog-agent/

find lib/protos/datadog/proto/datadog-agent/datadog/ -type f ! -name "*.proto" -delete

mkdir -p lib/protos/datadog/proto/agent-payload
cp ${TMP_DIR}/agent-payload/proto/metrics/agent_payload.proto lib/protos/datadog/proto/agent-payload/

## The Datadog Agent's `api.proto` imports `datadog/healthplatform/healthplatform.proto`, which the Agent takes from this
## repository. Keep the import path so that it resolves with `proto/agent-payload` as an include path.
mkdir -p lib/protos/datadog/proto/agent-payload/datadog/healthplatform
cp ${TMP_DIR}/agent-payload/proto/healthplatform/healthplatform.proto lib/protos/datadog/proto/agent-payload/datadog/healthplatform/

## containerd and containerd-related definitions.
##
## We do a cleanup step at the end to drop all non-proto files and any empty directories after
# removing those non-proto files.
mkdir -p lib/protos/containerd/proto/github.com/containerd/containerd/
cp -R ${TMP_DIR}/containerd/api lib/protos/containerd/proto/github.com/containerd/containerd/

find lib/protos/containerd/proto/ -type f  ! -name "*.proto" -delete

## sketches-go DDSketch definitions.
mkdir -p lib/protos/datadog/proto/sketches-go/ddsketch/pb
cp ${TMP_DIR}/sketches-go/ddsketch/pb/ddsketch.proto lib/protos/datadog/proto/sketches-go/ddsketch/pb/

# Update the README.md files to specify what version the definitions are based on.
cat <<EOF > lib/protos/datadog/proto/datadog-agent/README.md
# Datadog Agent Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for the Datadog Agent.

## Source

**Repository:** ${DD_AGENT_GIT_URL}
**Branch / Tag / Commit**: ${DD_AGENT_GIT_TAG}
EOF

cat <<EOF > lib/protos/datadog/proto/agent-payload/README.md
# Agent Payload Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for the public Datadog APIs that the Datadog
Agent/Agent Data Plane send telemetry payloads to.

## Source

**Repository:** ${AGENT_PAYLOAD_GIT_URL}
**Branch / Tag / Commit**: ${AGENT_PAYLOAD_GIT_TAG}
EOF

cat <<EOF > lib/protos/containerd/proto/github.com/containerd/containerd/README.md
# Containerd Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for containerd.

## Source

**Repository:** ${CONTAINERD_GIT_URL}
**Branch / Tag / Commit**: ${CONTAINERD_GIT_TAG}
EOF

cat <<EOF > lib/protos/datadog/proto/sketches-go/README.md
# DDSketch Protocol Buffers Definitions

This directory contains Protocol Buffers definitions for DDSketch.

## Source

**Repository:** ${SKETCHES_GO_GIT_URL}
**Branch / Tag / Commit**: ${SKETCHES_GO_GIT_TAG}
EOF

# Clean up the temporary directory.
rm -rf ${TMP_DIR}

echo "[*] Done."
