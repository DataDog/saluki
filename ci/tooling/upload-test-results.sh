#!/usr/bin/env bash
# Uploads a JUnit report to Datadog Test Optimization, which powers flaky test detection.
#
# Usage: bash upload-test-results.sh <junit.xml>
#
# The Datadog API key comes from dd-sts, which exchanges the job's GitLab ID token (`DD_STS_OIDC_TOKEN`, with
# audience `dd-sts`) for a short-lived key under the `saluki-test-optimization` policy.
#
# `datadog-ci` comes from the CI image.
#
# This never fails the job: a missing report or a failed upload only prints a warning.

set -uo pipefail

readonly DD_STS_VERSION="v0.1.9"
readonly DD_STS_CHECKSUM="941c8f7e0dfd6bb91b6389bdaafd7970d67197c8714619630d65048faf99cb9c"
readonly DD_STS_POLICY="saluki-test-optimization"

junit_file="${1:?usage: $0 <junit.xml>}"

warn() {
    echo "WARNING: $*; not uploading test results." >&2
    exit 0
}

[[ -f "$junit_file" ]] || warn "JUnit report '${junit_file}' not found"
datadog_ci="$(command -v datadog-ci)" || warn "datadog-ci not found"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        dd_sts_binary="dd-sts-linux-amd64"
        ;;
    Linux-aarch64)
        dd_sts_binary="dd-sts-linux-arm64"
        ;;
    *) warn "unsupported platform $(uname -s)-$(uname -m)" ;;
esac

workdir="$(mktemp -d -t upload-test-results-XXXXXX)"
trap 'rm -rf "$workdir"' EXIT

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
