#!/usr/bin/env sh
#
# Builds the Agent Data Plane binary for the requested target.
#
# This is the single entrypoint for the build. It resolves BUILD_TARGET to a per-target profile under
# `targets/`, sets up the shared build cache and credentials, then runs the one cargo invocation used
# for every target. ALL target-specific knowledge -- `rustup target add`, C/linker toolchain routing,
# CFLAGS, RUSTFLAGS, the output path, and the glibc floor -- lives in the per-target profile, not here.
# To support a new target, add a `targets/<triple>.sh` profile; no change to this script or the
# Dockerfile is needed.

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

# Pull in temporary AWS credentials if the secret was mounted (used to reach the remote build cache).
if [ -f /run/secrets/aws-creds.sh ]; then
    . /run/secrets/aws-creds.sh
fi

# Enable the build cache as a rustc wrapper if 00-install-buildcache.sh installed it.
if command -v buildcache >/dev/null 2>&1; then
    export RUSTC_WRAPPER=buildcache
    buildcache --zero-stats
fi

# Defaults a per-target profile may override. The profile runs its prep (rustup target add, toolchain
# routing) when sourced, and sets the TARGET_* values used by the cargo invocation below.
TARGET_CARGO_ARGS=""
TARGET_CFLAGS=""
TARGET_RUSTFLAGS=""
TARGET_OUTPUT_DIR="/adp/target/${BUILD_PROFILE}"

. "${target_profile}"

mkdir -p /out

# In CI we wrap the build with `cargo auditable` so the binary embeds an SBOM (its dependency tree).
# This is opt-in via USE_CARGO_AUDITABLE because the same Dockerfile builds locally too, where it
# isn't wanted. cargo-auditable is baked into the build image (make cargo-preinstall); if it's
# missing, fail loudly here rather than letting cargo emit a confusing "no such subcommand" error.
cargo_subcmd="build"
if [ "${USE_CARGO_AUDITABLE:-false}" = "true" ]; then
    if ! command -v cargo-auditable >/dev/null 2>&1; then
        echo "ERROR: USE_CARGO_AUDITABLE=true but cargo-auditable is not installed in the build image." >&2
        echo "       Regenerate the build CI image (make cargo-preinstall) before enabling auditable builds." >&2
        exit 1
    fi
    cargo_subcmd="auditable build"
fi

# When a profile sets TARGET_RUSTFLAGS (e.g. -C link-arg=-static-libgcc for the glibc targets), pass
# it via RUSTFLAGS. The RUSTFLAGS env var *overrides* `.cargo/config.toml` `[build] rustflags`, so we
# must re-add `--cfg tokio_unstable` (the only Linux-relevant flag there) to avoid dropping it. When a
# profile leaves TARGET_RUSTFLAGS empty we leave RUSTFLAGS unset so the config file's flags apply as-is.
if [ -n "${TARGET_RUSTFLAGS}" ]; then
    export RUSTFLAGS="--cfg tokio_unstable ${TARGET_RUSTFLAGS}"
fi

# The one cargo invocation shared by every target. TARGET_CFLAGS carries any target-mandated C flags;
# BUILD_CFLAGS lets a caller append extra flags without clobbering the target's own. cargo_subcmd is
# intentionally unquoted so `auditable build` splits into two arguments (same word-splitting style as
# TARGET_CARGO_ARGS below).
CFLAGS="${TARGET_CFLAGS} ${BUILD_CFLAGS:-}" \
    cargo ${cargo_subcmd} \
        --profile "${BUILD_PROFILE}" \
        --package agent-data-plane \
        --features "${BUILD_FEATURES}" \
        ${TARGET_CARGO_ARGS}

cp "${TARGET_OUTPUT_DIR}/agent-data-plane" /out/agent-data-plane

if command -v buildcache >/dev/null 2>&1; then
    buildcache --show-stats
fi
