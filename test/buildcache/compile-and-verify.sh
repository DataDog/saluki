#!/usr/bin/env bash
# End-to-end validation of the buildcache -> S3 round trip.
#
#   Run 1: clean local + remote, compile a small C file through buildcache.
#          Expect: direct/local/remote all MISS, entry written locally and pushed to S3.
#   Run 2: local cache cleared; remote (S3) retained. Recompile.
#          Expect: at least one REMOTE HIT, proving the entry came back from S3.
#
# Requires /tmp/buildcache-aws-env.sh (from configure-buildcache-s3.sh) and gcc on PATH.

set -euo pipefail

# shellcheck disable=SC1091
source /tmp/buildcache-aws-env.sh

command -v gcc        >/dev/null 2>&1 || { echo "gcc not found"        >&2; exit 1; }
command -v buildcache >/dev/null 2>&1 || { echo "buildcache not found" >&2; exit 1; }

WORKDIR="$(mktemp -d -t buildcache-verify-XXXX)"
trap 'rm -rf "${WORKDIR}"' EXIT

# Make the source unique per pipeline run so we never short-circuit on a cache entry left
# behind by a previous pipeline.
cat > "${WORKDIR}/hello.c" <<EOF
/* pipeline_id=${CI_PIPELINE_ID:-local} job_id=${CI_JOB_ID:-local} */
#include <stdio.h>
int main(void) { printf("hello buildcache\n"); return 0; }
EOF

buildcache --clear
buildcache --zero-stats

echo "=== run 1: expect all MISS; entry written locally and pushed to S3 ==="
buildcache gcc -c "${WORKDIR}/hello.c" -o "${WORKDIR}/hello.o"
buildcache --show-stats

# Wipe the local cache only — S3 retains the entry.
buildcache --clear

echo "=== run 2: expect REMOTE HIT served from S3 ==="
buildcache gcc -c "${WORKDIR}/hello.c" -o "${WORKDIR}/hello.o"
stats_out="$(buildcache --show-stats)"
echo "${stats_out}"

remote_hits="$(echo "${stats_out}" | awk -F: '/^[[:space:]]*Remote hits:/ { gsub(/[^0-9]/, "", $2); print $2 }')"
if [[ -z "${remote_hits}" || "${remote_hits}" -lt 1 ]]; then
    echo "FAIL: expected >= 1 remote hit on run 2, got '${remote_hits:-<empty>}'" >&2
    exit 1
fi

echo "SUCCESS: buildcache served ${remote_hits} remote hit(s) from S3."
