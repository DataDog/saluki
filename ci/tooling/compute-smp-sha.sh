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
#
# All cases except the PR comparison need git history. The jobs that run those cases must check
# out the full history (`GIT_DEPTH: "0"`; see .gitlab/benchmark.yml), because the CI git mirror
# does not deepen a shallow checkout: `git fetch --deepen` and `git fetch --unshallow` do not
# change it. This script then fetches main, and stops with an error if the checkout is shallow.

set -euo pipefail

role="${1:?usage: compute-smp-sha.sh <baseline|comparison>}"

# Fetch main and the current branch, so that origin/main and origin/$CI_COMMIT_BRANCH are
# current. On the nightly schedule, the current branch is main.
fetch_history() {
    local refspecs=("+refs/heads/main:refs/remotes/origin/main")
    if [[ "${CI_COMMIT_BRANCH}" != "main" ]]; then
        refspecs+=("+refs/heads/${CI_COMMIT_BRANCH}:refs/remotes/origin/${CI_COMMIT_BRANCH}")
    fi
    if [[ "$(git rev-parse --is-shallow-repository)" == "true" ]]; then
        echo "compute-smp-sha: the checkout is shallow, so the ${role} commit can be incorrect. Set GIT_DEPTH to 0 for this job." >&2
        exit 1
    fi
    git fetch --quiet origin "${refspecs[@]}" >&2
}

# Most recent origin/main commit committed strictly before 00:00:00 UTC of the given
# YYYY-MM-DD date.
last_commit_before_day() {
    git rev-list -n 1 --before="$1 00:00:00 +0000" origin/main
}

if [[ "${CI_PIPELINE_SOURCE:-}" == "schedule" ]]; then
    fetch_history
    run_date="$(date -u -d "${CI_PIPELINE_CREATED_AT}" +%F)"
    day_before="$(date -u -d "${run_date} -1 day" +%F)"
    case "${role}" in
        comparison) sha="$(last_commit_before_day "${run_date}")" ;;
        baseline)   sha="$(last_commit_before_day "${day_before}")" ;;
        *) echo "unknown role: ${role}" >&2; exit 1 ;;
    esac
else
    case "${role}" in
        comparison) sha="${CI_COMMIT_SHA}" ;;
        baseline)
            fetch_history
            sha="$(git merge-base origin/main "origin/${CI_COMMIT_BRANCH}")"
            ;;
        *) echo "unknown role: ${role}" >&2; exit 1 ;;
    esac
fi

if [[ -z "${sha}" ]]; then
    echo "compute-smp-sha: could not resolve the ${role} commit" >&2
    exit 1
fi

echo "compute-smp-sha: ${role} commit is ${sha}" >&2
echo "${sha}"
