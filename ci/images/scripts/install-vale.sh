#!/usr/bin/env bash
#
# Installs the vale CLI tool (prose linter) for CI builds.
#
# When bumping VERSION, update VALE_SHA256_* to match the checksums published
# at https://github.com/vale-cli/vale/releases/tag/v<version>.
#
set -euo pipefail
set -x

readonly VERSION="3.14.2"
readonly VALE_SHA256_AMD64="469cf88ec58a374dca14b2564c4391d2c9a1c632210aa0b642758b794082e05f"
readonly VALE_SHA256_ARM64="b11fa9955b93814f993442568b9b922604cc4b574643037b84900e9514860802"

# shellcheck disable=SC2155
readonly TMP_DIR="$(mktemp -d -t "vale_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

# Determine the architecture suffix and expected checksum.
case "${TARGETARCH:-$(uname -m)}" in
    amd64|x86_64)  ARCH="64-bit"; VALE_SHA256="${VALE_SHA256_AMD64}" ;;
    arm64|aarch64) ARCH="arm64";  VALE_SHA256="${VALE_SHA256_ARM64}" ;;
    *) echo "Unsupported architecture: ${TARGETARCH:-$(uname -m)}" >&2; exit 1 ;;
esac

install_vale() {
    local version="$1"
    local install_path="$2"

    local url="https://github.com/vale-cli/vale/releases/download/v${version}/vale_${version}_Linux_${ARCH}.tar.gz"
    local download_path="${TMP_DIR}/vale.tar.gz"

    curl -fsSL "${url}" -o "${download_path}"
    echo "${VALE_SHA256}  ${download_path}" | sha256sum --check
    tar -xzf "${download_path}" -C "${TMP_DIR}" --no-same-owner

    install -m 755 "${TMP_DIR}/vale" "${install_path}"
}

install_vale "${VERSION}" "/usr/local/bin/vale"
