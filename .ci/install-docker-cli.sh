#!/usr/bin/env bash
#
# Installs the Docker CLI on the host system.
#
set -euo pipefail
set -x

# Pull down Docker's APT GPG key and add it to the keyring.
mkdir -p /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg

# Add Docker's APT repository to the sources list.
cat > /etc/apt/sources.list.d/docker.list <<APT
deb [arch=${TARGETARCH} signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu jammy stable
APT

# Update the package index and install Docker CLI.
apt-get update
apt-get install -y --no-install-recommends docker-ce-cli=5:27.3.1-1~ubuntu.22.04~jammy

# Configure a minimal Docker client configuration to use our CI-specific credential helper
# for our internal Docker registries.
rm -rf /root/.docker
mkdir -p /root/.docker
cat > /root/.docker/config.json <<CREDS
{
  "credHelpers": {
    "registry-staging.ddbuild.io": "ci",
    "registry.ddbuild.io": "ci"
  }
}
CREDS
