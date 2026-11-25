#!/usr/bin/env bash
# Run in the job's after_script so a failed test still has a report.
set -euo pipefail

python3 ci/tooling/panoramic_junit.py "$PANORAMIC_LOG_DIR" "${1:?expected integration or correctness}" || exit 0

# The dd-sts policy admits branches, not tags. GitLab collects the XML for both.
if [[ -n "${CI_COMMIT_TAG:-}" ]]; then
    exit 0
fi

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        platform=linux
        arch=x64
        # Correctness child jobs use the parent's image but need not inherit its AGENT_NIGHTLY variable.
        channel=$(docker inspect --format '{{ index .Config.Labels "agent.channel" }}' "$BUNDLED_IMAGE") || exit 0
        ;;
    Darwin-arm64) platform=darwin; arch=arm64; channel=pinned ;;
    Darwin-x86_64) platform=darwin; arch=x64; channel=pinned ;;
    *) echo "WARNING: unknown Panoramic platform; not uploading test results." >&2; exit 0 ;;
esac
if [[ "$channel" != pinned && "$channel" != nightly ]]; then
    echo "WARNING: unknown Agent channel '$channel'; not uploading test results." >&2
    exit 0
fi
export DD_TAGS="${DD_TAGS:+$DD_TAGS,}os.platform:$platform,os.architecture:$arch,test.configuration.agent_channel:$channel"
bash ci/tooling/upload-test-results.sh "$PANORAMIC_LOG_DIR/panoramic.xml"
