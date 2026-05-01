#!/usr/bin/env bash
#
# Installs kind (Kubernetes in Docker) on the host system.
#
# When bumping KIND_VERSION, update KIND_SHA256 to match the checksum published
# at https://github.com/kubernetes-sigs/kind/releases/tag/<version>.
#
set -euo pipefail
set -x

KIND_VERSION="v0.31.0"
KIND_SHA256="eb244cbafcc157dff60cf68693c14c9a75c4e6e6fedaf9cd71c58117cb93e3fa"

# Determine the architecture suffix used in the kind release asset name.
case "${TARGETARCH:-$(uname -m)}" in
  amd64|x86_64) ARCH="amd64" ;;
  arm64|aarch64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: ${TARGETARCH:-$(uname -m)}" >&2; exit 1 ;;
esac

curl -sSLo /tmp/kind \
    "https://github.com/kubernetes-sigs/kind/releases/download/${KIND_VERSION}/kind-linux-${ARCH}"
echo "${KIND_SHA256}  /tmp/kind" | sha256sum --check
install -m 755 /tmp/kind /usr/local/bin/kind
rm /tmp/kind
