#!/usr/bin/env bash
#
# Installs buildcache (compiler cache) for CI builds.
#
# TODO: Switch back to upstream (https://gitlab.com/bits-n-bites/buildcache) once the HTTPS-support change (currently
# carried on the `tobz/https-support` branch of the fork below) is merged upstream. When that happens, swap `REPO_URL`
# to the upstream release tarball pattern and pin `VERSION` to a released tag.
set -euo pipefail
set -x

readonly REPO_URL="https://gitlab.com/tobz1/buildcache.git"
readonly REPO_REF="tobz/https-support"
readonly REPO_COMMIT="921c88108819fa3c2b9dc35083f8215c081e826e"

readonly TMP_DIR="$(mktemp -d -t "buildcache_XXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

install_buildcache() {
    local install_path="$1"
    local src_dir="${TMP_DIR}/buildcache"

    git clone --filter=blob:none --no-checkout "${REPO_URL}" "${src_dir}"
    git -C "${src_dir}" fetch --depth=1 origin "${REPO_COMMIT}"
    git -C "${src_dir}" checkout --detach "${REPO_COMMIT}"

    local cmake_args=(
        -S "${src_dir}/src"
        -B "${src_dir}/build"
        -G Ninja
        -DCMAKE_BUILD_TYPE=Release
    )

    # Statically link the build if enabled.
    #
    # This is purely to make things easier in terms of not needing to include OpenSSL (or, specifically, the same/right
    # version of OpenSSL) in build containers as the one used to compile `buildcache`.
    if [[ "${BUILDCACHE_STATIC:-0}" == "1" ]]; then
        cmake_args+=(
            -DOPENSSL_USE_STATIC_LIBS=TRUE
            "-DCMAKE_EXE_LINKER_FLAGS=-static-libgcc -static-libstdc++"
        )
    fi

    cmake "${cmake_args[@]}"
    cmake --build "${src_dir}/build" --target buildcache

    cp "${src_dir}/build/buildcache" "${install_path}"
    chmod +x "${install_path}"
}

install_buildcache "/usr/local/bin/buildcache"
