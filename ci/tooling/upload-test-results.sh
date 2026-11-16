#!/usr/bin/env bash
# Uploads a JUnit report to Datadog Test Optimization, which powers flaky test detection.
#
# Usage: bash upload-test-results.sh <junit.xml>
#
# The Datadog API key comes from dd-sts, which exchanges the job's GitLab ID token (`DD_STS_OIDC_TOKEN`, with
# audience `dd-sts`) for a short-lived key under the `saluki-test-optimization` policy.
#
# This never fails the job: a missing report or a failed upload only prints a warning.

set -uo pipefail

readonly DATADOG_CI_VERSION="v5.26.1"
readonly DD_STS_VERSION="v0.1.9"
readonly DD_STS_CHECKSUM="941c8f7e0dfd6bb91b6389bdaafd7970d67197c8714619630d65048faf99cb9c"
readonly DD_STS_POLICY="saluki-test-optimization"

junit_file="${1:?usage: $0 <junit.xml>}"

warn() {
    echo "WARNING: $*; not uploading test results." >&2
    exit 0
}

[[ -f "$junit_file" ]] || warn "JUnit report '${junit_file}' not found"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        datadog_ci_asset="datadog-ci_linux-x64"
        datadog_ci_checksum="6bfead2585e062a4db0fb6f58cc8c34c26f3333e02e68931e18aeaacf141efdb"
        dd_sts_binary="dd-sts-linux-amd64"
        ;;
    Linux-aarch64)
        datadog_ci_asset="datadog-ci_linux-arm64"
        datadog_ci_checksum="97fce10f9e21a0b749cfc813b983391a2e5fd48f52ef911615f9c033d8443801"
        dd_sts_binary="dd-sts-linux-arm64"
        ;;
    *) warn "unsupported platform $(uname -s)-$(uname -m)" ;;
esac

workdir="$(mktemp -d -t upload-test-results-XXXXXX)"
trap 'rm -rf "$workdir"' EXIT

datadog_ci="${workdir}/datadog-ci"
curl -sSfL --retry 3 -o "$datadog_ci" \
    "https://github.com/DataDog/datadog-ci/releases/download/${DATADOG_CI_VERSION}/${datadog_ci_asset}" ||
    warn "failed to download datadog-ci"
echo "${datadog_ci_checksum}  ${datadog_ci}" | sha256sum -c --quiet - || warn "datadog-ci checksum mismatch"
chmod +x "$datadog_ci"

dd_sts_tarball="${workdir}/dd-sts.tar.gz"
curl -sSfL --retry 3 -o "$dd_sts_tarball" \
    "https://binaries.ddbuild.io/dd-source/dd-sts/${DD_STS_VERSION}/dd-sts-tar.tar.gz" ||
    warn "failed to download dd-sts"
echo "${DD_STS_CHECKSUM}  ${dd_sts_tarball}" | sha256sum -c --quiet - || warn "dd-sts checksum mismatch"
tar -xzf "$dd_sts_tarball" -C "$workdir" "$dd_sts_binary" || warn "failed to extract dd-sts"
dd_sts="${workdir}/${dd_sts_binary}"
chmod +x "$dd_sts"

export DATADOG_SITE="datadoghq.com"
"$dd_sts" exchange --policy "$DD_STS_POLICY" -- \
    "$datadog_ci" junit upload --service saluki "$junit_file" ||
    warn "datadog-ci junit upload failed"
