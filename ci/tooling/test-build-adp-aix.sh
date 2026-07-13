#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="${repo_root}/ci/tooling/build-adp-aix.sh"

tmpdir="$(python3 - <<'PY'
import tempfile

print(tempfile.mkdtemp())
PY
)"
trap 'rm -rf "${tmpdir}"' EXIT

assert_contains() {
    local file="$1"
    local pattern="$2"

    if ! grep -Fq -- "${pattern}" "${file}"; then
        echo "expected ${file} to contain: ${pattern}" >&2
        echo "--- ${file} ---" >&2
        cat "${file}" >&2
        exit 1
    fi
}

assert_not_contains() {
    local file="$1"
    local pattern="$2"

    if grep -Fq -- "${pattern}" "${file}"; then
        echo "expected ${file} not to contain: ${pattern}" >&2
        echo "--- ${file} ---" >&2
        cat "${file}" >&2
        exit 1
    fi
}

ADP_AIX_BUILD_DRY_RUN=true "${script}" > "${tmpdir}/dry-run.out"
assert_contains "${tmpdir}/dry-run.out" "AIX_RUST_SDK_DIR=/opt/freeware/lib/RustSDK/1.92"
assert_contains "${tmpdir}/dry-run.out" "CC=/opt/freeware/bin/gcc"
assert_contains "${tmpdir}/dry-run.out" "CXX=/opt/freeware/bin/g++"
assert_contains "${tmpdir}/dry-run.out" "AR=/usr/bin/ar"
assert_contains "${tmpdir}/dry-run.out" "RANLIB=/usr/bin/ranlib"
assert_contains "${tmpdir}/dry-run.out" "CARGO_HOME=/opt/cargo-home"
assert_contains "${tmpdir}/dry-run.out" "CARGO_TARGET_DIR=/opt/saluki/target"
assert_contains "${tmpdir}/dry-run.out" "ADP_AIX_EXPECTED_CARGO_PREFIX=cargo 1.92."
assert_contains "${tmpdir}/dry-run.out" "ADP_AIX_EXPECTED_RUSTC_PREFIX=rustc 1.92."
assert_contains "${tmpdir}/dry-run.out" "ADP_AIX_EXPECTED_GCC_PREFIX=gcc (GCC) 13."
assert_contains "${tmpdir}/dry-run.out" "ADP_AIX_EXPECTED_GXX_PREFIX=g++ (GCC) 13."
assert_contains "${tmpdir}/dry-run.out" "BUILD_PROFILE=aix-optimized-release"
assert_contains "${tmpdir}/dry-run.out" "BUILD_FEATURES=default"
assert_contains "${tmpdir}/dry-run.out" "APP_FULL_NAME=Agent Data Plane"
assert_contains "${tmpdir}/dry-run.out" "APP_SHORT_NAME=data-plane"
assert_contains "${tmpdir}/dry-run.out" "APP_IDENTIFIER=adp"
assert_contains "${tmpdir}/dry-run.out" "APP_VERSION=1.3.0"
assert_contains "${tmpdir}/dry-run.out" "APP_DEV_BUILD=false"
assert_contains "${tmpdir}/dry-run.out" "cargo build --profile aix-optimized-release --bin agent-data-plane --features default"
assert_not_contains "${tmpdir}/dry-run.out" "patch"

BUILD_PROFILE=release \
BUILD_FEATURES=fips \
APP_FULL_NAME=CustomName \
APP_SHORT_NAME=custom-short \
APP_IDENTIFIER=custom-id \
APP_VERSION=9.8.7 \
APP_GIT_HASH=abcdef1 \
APP_BUILD_TIME=2026-07-10T00:00:00Z \
APP_DEV_BUILD=true \
AIX_RUST_SDK_DIR=/custom/rust \
ADP_AIX_CC=/custom/gcc \
ADP_AIX_CXX=/custom/g++ \
ADP_AIX_AR=/custom/ar \
ADP_AIX_RANLIB=/custom/ranlib \
CARGO_HOME=/custom/cargo-home \
CARGO_TARGET_DIR=/custom/target \
ADP_AIX_BUILD_DRY_RUN=true \
    "${script}" > "${tmpdir}/custom-dry-run.out"
assert_contains "${tmpdir}/custom-dry-run.out" "AIX_RUST_SDK_DIR=/custom/rust"
assert_contains "${tmpdir}/custom-dry-run.out" "CC=/custom/gcc"
assert_contains "${tmpdir}/custom-dry-run.out" "CXX=/custom/g++"
assert_contains "${tmpdir}/custom-dry-run.out" "AR=/custom/ar"
assert_contains "${tmpdir}/custom-dry-run.out" "RANLIB=/custom/ranlib"
assert_contains "${tmpdir}/custom-dry-run.out" "CARGO_HOME=/custom/cargo-home"
assert_contains "${tmpdir}/custom-dry-run.out" "CARGO_TARGET_DIR=/custom/target"
assert_contains "${tmpdir}/custom-dry-run.out" "ADP_AIX_EXPECTED_CARGO_PREFIX=cargo 1.92."
assert_contains "${tmpdir}/custom-dry-run.out" "ADP_AIX_EXPECTED_RUSTC_PREFIX=rustc 1.92."
assert_contains "${tmpdir}/custom-dry-run.out" "ADP_AIX_EXPECTED_GCC_PREFIX=gcc (GCC) 13."
assert_contains "${tmpdir}/custom-dry-run.out" "ADP_AIX_EXPECTED_GXX_PREFIX=g++ (GCC) 13."
assert_contains "${tmpdir}/custom-dry-run.out" "BUILD_PROFILE=release"
assert_contains "${tmpdir}/custom-dry-run.out" "BUILD_FEATURES=fips"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_FULL_NAME=CustomName"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_SHORT_NAME=custom-short"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_IDENTIFIER=custom-id"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_VERSION=9.8.7"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_GIT_HASH=abcdef1"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_BUILD_TIME=2026-07-10T00:00:00Z"
assert_contains "${tmpdir}/custom-dry-run.out" "APP_DEV_BUILD=true"
assert_contains "${tmpdir}/custom-dry-run.out" "cargo build --profile release --bin agent-data-plane --features fips"

echo "build-adp-aix script tests passed"
