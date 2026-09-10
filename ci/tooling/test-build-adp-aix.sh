#!/usr/bin/env bash
# Verifies that the AIX build helper receives valid release metadata from Make and direct callers.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

get_app_build_time() {
    sed -n 's/^APP_BUILD_TIME=//p' <<<"$1"
}

assert_utc_rfc3339_build_time() {
    local app_build_time="$1"

    if [[ ! "${app_build_time}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]]; then
        echo "expected a UTC RFC 3339 APP_BUILD_TIME, got '${app_build_time}'" >&2
        exit 1
    fi

    if [[ "${app_build_time}" == 0000-00-00T00:00:00Z ]]; then
        echo "received the placeholder APP_BUILD_TIME" >&2
        exit 1
    fi
}

local_build_output="$(
    cd "${repo_root}"
    env -u APP_BUILD_TIME -u CI_PIPELINE_CREATED_AT ADP_AIX_BUILD_DRY_RUN=true make --no-print-directory --silent build-adp-aix
)"
assert_utc_rfc3339_build_time "$(get_app_build_time "${local_build_output}")"

ci_build_time="2026-09-10T12:34:56Z"
ci_build_output="$(
    cd "${repo_root}"
    env -u APP_BUILD_TIME CI_PIPELINE_CREATED_AT="${ci_build_time}" ADP_AIX_BUILD_DRY_RUN=true make --no-print-directory --silent build-adp-aix
)"
if [[ "$(get_app_build_time "${ci_build_output}")" != "${ci_build_time}" ]]; then
    echo "make build-adp-aix did not preserve CI_PIPELINE_CREATED_AT" >&2
    exit 1
fi

overridden_build_time="2026-09-10T23:45:01Z"
overridden_build_output="$(
    cd "${repo_root}"
    env -u APP_BUILD_TIME -u CI_PIPELINE_CREATED_AT ADP_AIX_BUILD_DRY_RUN=true make --no-print-directory --silent build-adp-aix APP_BUILD_TIME="${overridden_build_time}"
)"
if [[ "$(get_app_build_time "${overridden_build_output}")" != "${overridden_build_time}" ]]; then
    echo "make build-adp-aix did not preserve explicit APP_BUILD_TIME" >&2
    exit 1
fi

direct_build_output="$(
    cd "${repo_root}"
    env -u APP_BUILD_TIME -u ADP_APP_BUILD_TIME -u CI_PIPELINE_CREATED_AT ADP_AIX_BUILD_DRY_RUN=true ci/tooling/build-adp-aix.sh
)"
assert_utc_rfc3339_build_time "$(get_app_build_time "${direct_build_output}")"
