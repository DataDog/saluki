#!/usr/bin/env bash
# Uploads a JUnit report to Datadog Test Optimization, which powers flaky test detection.
#
# Usage: bash upload-test-results.sh <junit.xml>
#
# This never fails the job: a missing report or a failed upload only prints a warning.

set -uo pipefail

readonly DATADOG_CI_VERSION="v5.26.1"
readonly API_KEY_PARAMETER="ci.saluki.prod_dd_api_key"

junit_file="${1:?usage: $0 <junit.xml>}"

warn() {
    echo "WARNING: $*; not uploading test results." >&2
    exit 0
}

[[ -f "$junit_file" ]] || warn "JUnit report '${junit_file}' not found"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        asset="datadog-ci_linux-x64"
        checksum="6bfead2585e062a4db0fb6f58cc8c34c26f3333e02e68931e18aeaacf141efdb"
        ;;
    Linux-aarch64)
        asset="datadog-ci_linux-arm64"
        checksum="97fce10f9e21a0b749cfc813b983391a2e5fd48f52ef911615f9c033d8443801"
        ;;
    *) warn "unsupported platform $(uname -s)-$(uname -m)" ;;
esac

workdir="$(mktemp -d -t datadog-ci-XXXXXX)"
trap 'rm -rf "$workdir"' EXIT
datadog_ci="${workdir}/datadog-ci"

curl -sSfL --retry 3 -o "$datadog_ci" \
    "https://github.com/DataDog/datadog-ci/releases/download/${DATADOG_CI_VERSION}/${asset}" ||
    warn "failed to download datadog-ci"
echo "${checksum}  ${datadog_ci}" | sha256sum -c --quiet - || warn "datadog-ci checksum mismatch"
chmod +x "$datadog_ci"

DATADOG_API_KEY="$(aws ssm get-parameter --region us-east-1 --name "$API_KEY_PARAMETER" --with-decryption \
    --query "Parameter.Value" --output text)" || warn "failed to fetch the Datadog API key"
export DATADOG_API_KEY
export DATADOG_SITE="datadoghq.com"

"$datadog_ci" junit upload --service saluki "$junit_file" || warn "datadog-ci junit upload failed"
