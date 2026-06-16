#!/usr/bin/env sh
#
# Builds the Agent Data Plane binary for the requested target.
#
# This is the single entrypoint for the build. It resolves BUILD_TARGET to a per-target profile under
# `targets/`, sets up the shared build cache and credentials, then runs the one cargo invocation used
# for every target. ALL target-specific knowledge -- cross-compilation kernel headers, CFLAGS,
# `rustup target add`, and the output path -- lives in the per-target profile, not here. To support a
# new target, add a `targets/<triple>.sh` profile; no change to this script or the Dockerfile is
# needed.

set -eu

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
targets_dir="${script_dir}/targets"

build_target="${BUILD_TARGET:-default}"
target_profile="${targets_dir}/${build_target}.sh"

if [ ! -f "${target_profile}" ]; then
    echo "ERROR: unsupported build target '${build_target}'." >&2
    echo "Supported targets:" >&2
    for profile in "${targets_dir}"/*.sh; do
        echo "  - $(basename "${profile}" .sh)" >&2
    done
    exit 1
fi

# Copies the host kernel headers into the musl target include directory. AWS-LC (and any other crate
# that needs kernel headers) requires these when cross-compiling <arch>-unknown-linux-musl. Invoked
# by the per-target profiles that need it.
#
#   $1: the GNU triple whose headers we copy from (e.g. x86_64-linux-gnu)
#   $2: the musl cross triple we copy into        (e.g. x86_64-linux-musl)
copy_kernel_headers() {
    gnu_triple="$1"
    musl_triple="$2"

    if ! [ -d /usr/include/linux ] || ! [ -d /usr/include/asm-generic ] || ! [ -d "/usr/include/${gnu_triple}" ]; then
        echo "ERROR: kernel headers not found (need /usr/include/linux, asm-generic, and ${gnu_triple})." >&2
        exit 1
    fi

    mkdir -p "/usr/include/${musl_triple}"
    cp -R /usr/include/linux "/usr/include/${musl_triple}/linux"
    cp -R /usr/include/asm-generic "/usr/include/${musl_triple}/asm-generic"
    cp -R "/usr/include/${gnu_triple}/asm" "/usr/include/${musl_triple}/asm"
}

# Pull in temporary AWS credentials if the secret was mounted (used to reach the remote build cache).
if [ -f /run/secrets/aws-creds.sh ]; then
    . /run/secrets/aws-creds.sh
fi

# Enable the build cache as a rustc wrapper if 10-install-buildcache.sh installed it.
if command -v buildcache >/dev/null 2>&1; then
    export RUSTC_WRAPPER=buildcache
    buildcache --zero-stats
fi

# Defaults a per-target profile may override. The profile runs its prep (rustup target add, header
# copying) when sourced, and sets the three TARGET_* values used by the cargo invocation below.
TARGET_CARGO_ARGS=""
TARGET_CFLAGS=""
TARGET_OUTPUT_DIR="/adp/target/${BUILD_PROFILE}"

. "${target_profile}"

mkdir -p /out

# The one cargo invocation shared by every target. TARGET_CFLAGS carries any target-mandated flags
# (e.g. -mno-outline-atomics for aarch64 musl); BUILD_CFLAGS lets a caller append extra flags without
# clobbering the target's own.
CFLAGS="${TARGET_CFLAGS} ${BUILD_CFLAGS:-}" \
    cargo build \
        --profile "${BUILD_PROFILE}" \
        --package agent-data-plane \
        --features "${BUILD_FEATURES}" \
        ${TARGET_CARGO_ARGS}

cp "${TARGET_OUTPUT_DIR}/agent-data-plane" /out/agent-data-plane

if command -v buildcache >/dev/null 2>&1; then
    buildcache --show-stats
fi
