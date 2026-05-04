#!/usr/bin/env bash
#
# Installs AWS CLI v2 for CI builds.
#
set -euo pipefail
set -x

readonly TMP_DIR="$(mktemp -d -t "awscli_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

install_awscli() {
    local arch
    arch="$(uname -m)"
    case "${arch}" in
        x86_64|aarch64) ;;
        *)
            echo "Unsupported architecture: ${arch}" >&2
            exit 1
            ;;
    esac

    local url="https://awscli.amazonaws.com/awscli-exe-linux-${arch}.zip"
    local download_path="${TMP_DIR}/awscliv2.zip"

    curl -fsSL "${url}" -o "${download_path}"
    unzip -qq "${download_path}" -d "${TMP_DIR}"

    "${TMP_DIR}/aws/install" --update
}

install_awscli
