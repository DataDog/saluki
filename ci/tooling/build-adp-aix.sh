#!/usr/bin/env bash
# Builds agent-data-plane natively on AIX using the known-good IBM Rust SDK and AIX Toolbox GCC toolchain.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

BUILD_PROFILE="${BUILD_PROFILE:-devel}"
AIX_RUST_SDK_DIR="${AIX_RUST_SDK_DIR:-/opt/freeware/lib/RustSDK/1.92}"
ADP_AIX_CC="${ADP_AIX_CC:-/opt/freeware/bin/gcc}"
ADP_AIX_CXX="${ADP_AIX_CXX:-/opt/freeware/bin/g++}"
ADP_AIX_AR="${ADP_AIX_AR:-/usr/bin/ar}"
ADP_AIX_RANLIB="${ADP_AIX_RANLIB:-/usr/bin/ranlib}"
CARGO_HOME="${CARGO_HOME:-/opt/cargo-home}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/opt/saluki/target}"
ADP_AIX_EXPECTED_CARGO_PREFIX="${ADP_AIX_EXPECTED_CARGO_PREFIX:-cargo 1.92.}"
ADP_AIX_EXPECTED_RUSTC_PREFIX="${ADP_AIX_EXPECTED_RUSTC_PREFIX:-rustc 1.92.}"
ADP_AIX_EXPECTED_GCC_PREFIX="${ADP_AIX_EXPECTED_GCC_PREFIX:-gcc (GCC) 13.}"
ADP_AIX_EXPECTED_GXX_PREFIX="${ADP_AIX_EXPECTED_GXX_PREFIX:-g++ (GCC) 13.}"
ADP_AIX_BUILD_DRY_RUN="${ADP_AIX_BUILD_DRY_RUN:-false}"

export PATH="${AIX_RUST_SDK_DIR}/bin:/opt/freeware/bin:/usr/sbin:/usr/bin:/bin:${PATH:-}"
export CC="${ADP_AIX_CC}"
export CXX="${ADP_AIX_CXX}"
export AR="${ADP_AIX_AR}"
export RANLIB="${ADP_AIX_RANLIB}"
export CARGO_HOME
export CARGO_TARGET_DIR

export APP_FULL_NAME="${APP_FULL_NAME:-${ADP_APP_FULL_NAME:-Agent Data Plane}}"
export APP_SHORT_NAME="${APP_SHORT_NAME:-${ADP_APP_SHORT_NAME:-data-plane}}"
export APP_IDENTIFIER="${APP_IDENTIFIER:-${ADP_APP_IDENTIFIER:-adp}}"
export APP_GIT_HASH="${APP_GIT_HASH:-${ADP_APP_GIT_HASH:-unknown}}"
export APP_VERSION="${APP_VERSION:-${ADP_APP_VERSION:-0.0.0}}"
export APP_BUILD_DATE="${APP_BUILD_DATE:-${ADP_APP_BUILD_DATE:-0000-00-00T00:00:00-00:00}}"

require_executable() {
    local path="$1"
    local label="$2"

    if [[ ! -x "${path}" ]]; then
        echo "build-adp-aix: missing ${label} at ${path}" >&2
        exit 1
    fi
}

require_command() {
    local command_name="$1"

    if ! command -v "${command_name}" >/dev/null 2>&1; then
        echo "build-adp-aix: missing required command '${command_name}' in PATH" >&2
        exit 1
    fi
}

check_version_prefix() {
    local executable="$1"
    local label="$2"
    local expected_prefix="$3"
    local version

    if [[ -z "${expected_prefix}" ]]; then
        echo "[*] ${label}: version check skipped"
        return
    fi

    version="$(${executable} --version | head -n 1)"
    if [[ "${version}" != "${expected_prefix}"* ]]; then
        echo "build-adp-aix: ${label} version '${version}' does not match expected prefix '${expected_prefix}'" >&2
        echo "build-adp-aix: set the corresponding ADP_AIX_EXPECTED_*_PREFIX variable if this toolchain change is intentional" >&2
        exit 1
    fi

    echo "[*] ${label}: ${version}"
}

print_environment() {
    echo "AIX_RUST_SDK_DIR=${AIX_RUST_SDK_DIR}"
    echo "CC=${CC}"
    echo "CXX=${CXX}"
    echo "AR=${AR}"
    echo "RANLIB=${RANLIB}"
    echo "CARGO_HOME=${CARGO_HOME}"
    echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR}"
    echo "ADP_AIX_EXPECTED_CARGO_PREFIX=${ADP_AIX_EXPECTED_CARGO_PREFIX}"
    echo "ADP_AIX_EXPECTED_RUSTC_PREFIX=${ADP_AIX_EXPECTED_RUSTC_PREFIX}"
    echo "ADP_AIX_EXPECTED_GCC_PREFIX=${ADP_AIX_EXPECTED_GCC_PREFIX}"
    echo "ADP_AIX_EXPECTED_GXX_PREFIX=${ADP_AIX_EXPECTED_GXX_PREFIX}"
    echo "BUILD_PROFILE=${BUILD_PROFILE}"
    echo "cargo build --profile ${BUILD_PROFILE} --bin agent-data-plane"
}

if [[ "${ADP_AIX_BUILD_DRY_RUN}" == "true" ]]; then
    print_environment
    exit 0
fi

if [[ "$(uname -s)" != "AIX" ]]; then
    echo "build-adp-aix: this script must run on an AIX host." >&2
    exit 1
fi

require_executable "${AIX_RUST_SDK_DIR}/bin/cargo" "IBM Open SDK cargo"
require_executable "${AIX_RUST_SDK_DIR}/bin/rustc" "IBM Open SDK rustc"
require_executable "${CC}" "AIX Toolbox GCC C compiler"
require_executable "${CXX}" "AIX Toolbox GCC C++ compiler"
require_executable "${AR}" "AIX system ar"
require_executable "${RANLIB}" "AIX system ranlib"
require_command protoc

check_version_prefix "${AIX_RUST_SDK_DIR}/bin/cargo" "cargo" "${ADP_AIX_EXPECTED_CARGO_PREFIX}"
check_version_prefix "${AIX_RUST_SDK_DIR}/bin/rustc" "rustc" "${ADP_AIX_EXPECTED_RUSTC_PREFIX}"
check_version_prefix "${CC}" "gcc" "${ADP_AIX_EXPECTED_GCC_PREFIX}"
check_version_prefix "${CXX}" "g++" "${ADP_AIX_EXPECTED_GXX_PREFIX}"

mkdir -p "${CARGO_HOME}" "${CARGO_TARGET_DIR}"

cd "${repo_root}"

print_environment

echo "[*] Fetching Cargo dependencies..."
cargo fetch

echo "[*] Building agent-data-plane for AIX..."
cargo build --profile "${BUILD_PROFILE}" --bin agent-data-plane

echo "[*] AIX ADP binary ready at ${CARGO_TARGET_DIR}/${BUILD_PROFILE}/agent-data-plane"
