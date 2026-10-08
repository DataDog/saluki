#!/usr/bin/env bash
#
# Compute the baseline or comparison commit SHA for an SMP benchmark run, printing the
# resolved SHA on stdout (diagnostics go to stderr).
#
# Usage: compute-smp-sha.sh <baseline|comparison>
#   baseline   - the reference revision SMP compares against
#   comparison - the revision under test
#
# Two contexts are supported, selected by $CI_PIPELINE_SOURCE:
#
#   PR / on-demand (any non-schedule pipeline, e.g. a development-branch pipeline):
#     comparison = the current commit ($CI_COMMIT_SHA)
#     baseline   = the merge base of the branch with main
#
#   Nightly (scheduled pipeline; see .on_smp_nightly): a deterministic, date-anchored
#   "previous nightly" comparison that is immune to divergent/backport release branches.
#   Taking the run day D from $CI_PIPELINE_CREATED_AT (UTC):
#     comparison = last commit on origin/main from D-1 (most recent commit before 00:00 UTC of D)
#     baseline   = last commit on origin/main from D-2 (most recent commit before 00:00 UTC of D-1)
#   i.e. it compares the last commit of yesterday against the last commit of the day before.
#   If a day had no commits, --before falls back to the latest earlier commit; if that makes
#   baseline == comparison the run is simply a no-op delta.

set -euo pipefail

role="${1:?usage: compute-smp-sha.sh <baseline|comparison>}"

# Testing only (#2794): PR runs use saluki#2793's branch as the baseline instead of the merge base, so SMP compares the
# two Foldspace designs head to head. Set SMP_BASELINE_BRANCH to empty to restore the merge base.
SMP_BASELINE_BRANCH="${SMP_BASELINE_BRANCH-andrewq/foldspace-metrics-vendored-rule-store}"

# Most recent origin/main commit committed strictly before 00:00:00 UTC of the given
# YYYY-MM-DD date.
last_commit_before_day() {
    git rev-list -n 1 --before="$1 00:00:00 +0000" origin/main
}

if [[ "${CI_PIPELINE_SOURCE:-}" == "schedule" ]]; then
    run_date="$(date -u -d "${CI_PIPELINE_CREATED_AT}" +%F)"
    day_before="$(date -u -d "${run_date} -1 day" +%F)"
    case "${role}" in
        comparison) last_commit_before_day "${run_date}" ;;
        baseline)   last_commit_before_day "${day_before}" ;;
        *) echo "unknown role: ${role}" >&2; exit 1 ;;
    esac
else
    case "${role}" in
        comparison) echo "${CI_COMMIT_SHA}" ;;
        baseline)
            if [[ -n "${SMP_BASELINE_BRANCH}" ]]; then
                git fetch --quiet origin "+refs/heads/${SMP_BASELINE_BRANCH}:refs/remotes/origin/${SMP_BASELINE_BRANCH}" >&2
                git rev-parse "origin/${SMP_BASELINE_BRANCH}"
            else
                git merge-base origin/main "origin/${CI_COMMIT_BRANCH}"
            fi
            ;;
        *) echo "unknown role: ${role}" >&2; exit 1 ;;
    esac
fi
