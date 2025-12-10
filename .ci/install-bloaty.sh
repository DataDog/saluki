#!/usr/bin/env bash
#
# Installs bloaty (binary size profiler) for CI builds.
# Bloaty does not provide pre-built binaries, so we build from source.
#
set -euo pipefail
set -x

readonly VERSION="1.1"

readonly TMP_DIR="$(mktemp -d -t "bloaty_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

install_bloaty() {
    local version="$1"
    local install_path="$2"

    local url="https://github.com/google/bloaty/releases/download/v${version}/bloaty-${version}.tar.bz2"
    local download_path="${TMP_DIR}/bloaty.tar.bz2"
    local src_dir="${TMP_DIR}/bloaty-${version}"

    curl -fsSL "${url}" -o "${download_path}"
    tar -xf "${download_path}" -C "${TMP_DIR}" --no-same-owner

    cmake -S "${src_dir}" -B "${src_dir}/build" -G Ninja -DCMAKE_BUILD_TYPE=Release
    cmake --build "${src_dir}/build" --target bloaty

    cp "${src_dir}/build/bloaty" "${install_path}"
    chmod +x "${install_path}"
}

install_bloaty "${VERSION}" "/usr/local/bin/bloaty"
