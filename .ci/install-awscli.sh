#!/usr/bin/env bash
#
# Installs AWS CLI v2 for CI builds.
# Note: Hardcoded to Linux x86_64 as this runs in Docker build context.
#
set -euo pipefail
set -x

readonly TMP_DIR="$(mktemp -d -t "awscli_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

install_awscli() {
    local url="https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip"
    local download_path="${TMP_DIR}/awscliv2.zip"

    curl -fsSL "${url}" -o "${download_path}"
    unzip -qq "${download_path}" -d "${TMP_DIR}"

    "${TMP_DIR}/aws/install" --update
}

install_awscli
