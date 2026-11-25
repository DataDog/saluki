#!/usr/bin/env bash
# Reports a Panoramic CI attempt from the job's after_script, so a failed or timed-out attempt still has a report.
#
# Usage: report-panoramic-results.sh convert <integration|correctness> [case]
#        report-panoramic-results.sh upload
#
# Run `convert` first: it writes the JUnit report that GitLab collects. `upload` sends it to Datadog and can take
# longer, so other after_script steps can run between the two.
set -euo pipefail

# How long `convert` waits for a run that was stopped to record its last cases. Panoramic gives each running case
# 30 seconds to return after a signal.
readonly SETTLE_SECONDS=60

case "${1:?expected convert or upload}" in
    convert)
        python3 ci/tooling/panoramic_junit.py "$PANORAMIC_LOG_DIR" "${2:?expected integration or correctness}" \
            ${3:+--case "$3"} --wait "$SETTLE_SECONDS" || true
        exit 0
        ;;
    upload) ;;
    *) echo "WARNING: unknown command '$1'; not reporting Panoramic results." >&2; exit 0 ;;
esac

# The dd-sts policy admits branches, not tags. GitLab collects the XML for both.
if [[ -n "${CI_COMMIT_TAG:-}" ]]; then
    exit 0
fi

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        platform=linux
        arch=x64
        channel=pinned
        if [[ "${AGENT_NIGHTLY:-}" == true ]]; then channel=nightly; fi
        ;;
    Darwin-arm64) platform=darwin; arch=arm64; channel=pinned ;;
    Darwin-x86_64) platform=darwin; arch=x64; channel=pinned ;;
    *) echo "WARNING: unknown Panoramic platform; not uploading test results." >&2; exit 0 ;;
esac
export DD_TAGS="${DD_TAGS:+$DD_TAGS,}os.platform:$platform,os.architecture:$arch,test.configuration.agent_channel:$channel"
bash ci/tooling/upload-test-results.sh "$PANORAMIC_LOG_DIR/panoramic.xml"
