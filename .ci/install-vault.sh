#!/usr/bin/env sh
#
# Installs HashiCorp Vault for CI builds.
#
set -euo pipefail
set -x

readonly VERSION="1.21.1"

readonly TMP_DIR="$(mktemp -d -t "vault_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

get_platform() {
    local os
    os="$(uname)"
    case "${os}" in
        Darwin) echo "darwin" ;;
        Linux)  echo "linux" ;;
        *)      echo "Unsupported OS: ${os}" >&2; exit 1 ;;
    esac
}

get_arch() {
    local arch
    arch="$(uname -m)"
    case "${arch}" in
        x86_64)  echo "amd64" ;;
        aarch64) echo "arm64" ;;
        *)       echo "${arch}" ;;
    esac
}

install_vault() {
    local version="$1"
    local install_path="$2"

    local url="https://releases.hashicorp.com/vault/${version}/vault_${version}_$(get_platform)_$(get_arch).zip"
    local download_path="${TMP_DIR}/vault.zip"

    curl -fsSL "${url}" -o "${download_path}"
    unzip -qq "${download_path}" -d "${TMP_DIR}"

    mv "${TMP_DIR}/vault" "${install_path}"
    chmod +x "${install_path}"
}

install_vault "${VERSION}" "/usr/local/bin/vault"
