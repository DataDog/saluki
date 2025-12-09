#!/bin/bash
set -euo pipefail

VAULT_VERSION="1.21.1"

get_platform() {
  local os
  os=$(uname)
  if [[ "${os}" == "Darwin" ]]; then
    echo "darwin"
  elif [[ "${os}" == "Linux" ]]; then
    echo "linux"
  else
    >&2 echo "unsupported os: ${os}" && exit 1
  fi
}

get_arch() {
  local arch
  arch=$(uname -m)
  # On ARM Macs, uname -m returns "arm64", but in protoc releases this architecture is called "aarch_64"
  if [[ "${arch}" == "x86_64" ]]; then
    echo "amd64"
  elif [[ "${arch}" == "aarch64" ]]; then
    echo "arm64"
  else
    echo "${arch}"
  fi
}

# Download the pre-compiled Vault binary for this platform and extract it.
curl -fsSL "https://releases.hashicorp.com/vault/${VAULT_VERSION}/vault_${VAULT_VERSION}_$(get_platform)_$(get_arch).zip" -o /tmp/vault.zip
mkdir /tmp/vault
cd /tmp/vault
unzip /tmp/vault.zip

# Move the binary into place and ensure it's executable.
mv vault /usr/local/bin/vault
chmod +x /usr/local/bin/vault

# Clean up after ourselves.
rm -f /tmp/vault.zip
rm -rf /tmp/vault
