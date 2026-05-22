#!/usr/bin/env bash
#
# Installs protoc (Protocol Buffers compiler) for CI builds.
# No guard because we want to override Ubuntu's old version in case it is
# already installed by a dependency.
#
# Basis of script copied from:
# https://github.com/paxosglobal/asdf-protoc/blob/46c2f9349b8420144b197cfd064a9677d21cfb0c/bin/install
#
set -euo pipefail
set -x

readonly VERSION="29.3"

# shellcheck disable=SC2155
readonly TMP_DIR="$(mktemp -d -t "protoc_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

get_platform() {
    local os
    os="$(uname)"
    case "${os}" in
        Darwin) echo "osx" ;;
        Linux)  echo "linux" ;;
        *)      echo "Unsupported OS: ${os}" >&2; exit 1 ;;
    esac
}

get_arch() {
    local os arch
    os="$(uname)"
    arch="$(uname -m)"
    # On ARM Macs, uname -m returns "arm64", but in protoc releases this
    # architecture is called "aarch_64".
    if [[ "${os}" == "Darwin" && "${arch}" == "arm64" ]]; then
        echo "aarch_64"
    elif [[ "${os}" == "Linux" && "${arch}" == "aarch64" ]]; then
        echo "aarch_64"
    else
        echo "${arch}"
    fi
}

install_protoc() {
    local version="$1"
    local install_path="$2"

    local base_url="https://github.com/protocolbuffers/protobuf/releases/download"
    local url="${base_url}/v${version}/protoc-${version}-$(get_platform)-$(get_arch).zip"
    local download_path="${TMP_DIR}/protoc.zip"

    curl -fsSL "${url}" -o "${download_path}"
    unzip -qq "${download_path}" -d "${TMP_DIR}"

    mv "${TMP_DIR}/bin/protoc" "${install_path}"
    chmod +x "${install_path}"
}

install_protoc "${VERSION}" "/usr/bin/protoc"
