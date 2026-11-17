#!/usr/bin/env bash
# Uploads a JUnit report to Datadog Test Optimization, which powers flaky test detection.
#
# Usage: bash upload-test-results.sh <junit.xml>
#
# The Datadog API key comes from dd-sts, which exchanges the job's GitLab ID token (`DD_STS_OIDC_TOKEN`, with
# audience `dd-sts`) for a short-lived key under the `saluki-test-optimization` policy.
#
# `datadog-ci` comes from the CI image when it has one. Otherwise a pinned, checksummed standalone binary is downloaded.
#
# This never fails the job: a missing report or a failed upload only prints a warning.

set -uo pipefail

readonly DD_STS_VERSION="v0.1.9"
readonly DD_STS_CHECKSUM="941c8f7e0dfd6bb91b6389bdaafd7970d67197c8714619630d65048faf99cb9c"
readonly DD_STS_POLICY="saluki-test-optimization"
readonly DATADOG_CI_VERSION="v5.26.1"

junit_file="${1:?usage: $0 <junit.xml>}"

warn() {
    echo "WARNING: $*; not uploading test results." >&2
    exit 0
}

[[ -f "$junit_file" ]] || warn "JUnit report '${junit_file}' not found"

datadog_ci_asset=""
datadog_ci_checksum=""
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        dd_sts_binary="dd-sts-linux-amd64"
        ;;
    Linux-aarch64)
        dd_sts_binary="dd-sts-linux-arm64"
        ;;
    Darwin-x86_64)
        dd_sts_binary="dd-sts-darwin-amd64"
        datadog_ci_asset="datadog-ci_darwin-x64"
        datadog_ci_checksum="628a592fa1a01556388fa24e199a68c3fe5130a21fcb2ab28ba6fba54b56887f"
        ;;
    Darwin-arm64)
        dd_sts_binary="dd-sts-darwin-arm64"
        datadog_ci_asset="datadog-ci_darwin-arm64"
        datadog_ci_checksum="c8d186b94cc73b1e4ea0a2f3f26dd6bd9e6c3043da9a19d364b34e54cf967ff2"
        ;;
    *) warn "unsupported platform $(uname -s)-$(uname -m)" ;;
esac

# macOS has `shasum` but not `sha256sum`.
verify_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        echo "$1  $2" | sha256sum -c --quiet -
    else
        echo "$1  $2" | shasum -a 256 -c --quiet -
    fi
}

workdir="$(mktemp -d "${TMPDIR:-/tmp}/upload-test-results.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

if ! datadog_ci="$(command -v datadog-ci)"; then
    [[ -n "$datadog_ci_asset" ]] || warn "datadog-ci not found"
    datadog_ci="${workdir}/datadog-ci"
    curl -sSfL --retry 3 -o "$datadog_ci" \
        "https://github.com/DataDog/datadog-ci/releases/download/${DATADOG_CI_VERSION}/${datadog_ci_asset}" ||
        warn "failed to download datadog-ci"
    verify_sha256 "$datadog_ci_checksum" "$datadog_ci" || warn "datadog-ci checksum mismatch"
    chmod +x "$datadog_ci"
fi

dd_sts_tarball="${workdir}/dd-sts.tar.gz"
curl -sSfL --retry 3 -o "$dd_sts_tarball" \
    "https://binaries.ddbuild.io/dd-source/dd-sts/${DD_STS_VERSION}/dd-sts-tar.tar.gz" ||
    warn "failed to download dd-sts"
verify_sha256 "$DD_STS_CHECKSUM" "$dd_sts_tarball" || warn "dd-sts checksum mismatch"
tar -xzf "$dd_sts_tarball" -C "$workdir" "$dd_sts_binary" || warn "failed to extract dd-sts"
dd_sts="${workdir}/${dd_sts_binary}"
chmod +x "$dd_sts"

export DATADOG_SITE="datadoghq.com"
"$dd_sts" exchange --policy "$DD_STS_POLICY" -- \
    "$datadog_ci" junit upload --service saluki "$junit_file" ||
    warn "datadog-ci junit upload failed"
