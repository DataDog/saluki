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
# All cases except the PR comparison need git history, and the benchmark jobs use a shallow
# checkout (see `.smp-checkout` in .gitlab/benchmark.yml). We cannot deepen the checkout: the CI
# git mirror ignores `git fetch --deepen` and `git fetch --unshallow` for a ref that the checkout
# has already. So this script fetches the full history of main and of the branch into a separate
# repository, and resolves the commit there. A full fetch from the mirror takes some seconds,
# but a full checkout takes about a minute.
#
# The script then adds the objects of that repository to the checkout as alternates, so that the
# job can check out the resolved commit.

set -euo pipefail

role="${1:?usage: compute-smp-sha.sh <baseline|comparison>}"

history_repo=""

# Fetch the full history of main and of the current branch into a separate bare repository, and
# set `history_repo` to its path. On the nightly schedule, the current branch is main.
fetch_history() {
    local refspecs=("+refs/heads/main:refs/remotes/origin/main")
    if [[ "${CI_COMMIT_BRANCH}" != "main" ]]; then
        refspecs+=("+refs/heads/${CI_COMMIT_BRANCH}:refs/remotes/origin/${CI_COMMIT_BRANCH}")
    fi

    history_repo="$(mktemp -d -t smp-history.XXXXXX)"
    git init --quiet --bare "${history_repo}"
    # Use the remote and the HTTP settings (for example, authentication) of the checkout.
    git -C "${history_repo}" remote add origin "$(git remote get-url origin)"
    git config --local --get-regexp '^http\.' | while read -r key value; do
        git -C "${history_repo}" config --add "${key}" "${value}"
    done || true
    git -C "${history_repo}" fetch --quiet --no-tags origin "${refspecs[@]}" >&2

    if [[ "$(git -C "${history_repo}" rev-parse --is-shallow-repository)" == "true" ]]; then
        echo "compute-smp-sha: the history fetch is shallow, so the ${role} commit can be incorrect" >&2
        exit 1
    fi
}

# Make the objects of the history repository available in the checkout.
share_history() {
    echo "${history_repo}/objects" >> "$(git rev-parse --git-path objects/info/alternates)"
}

# Most recent origin/main commit committed strictly before 00:00:00 UTC of the given
# YYYY-MM-DD date.
last_commit_before_day() {
    git -C "${history_repo}" rev-list -n 1 --before="$1 00:00:00 +0000" origin/main
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
    share_history
else
    case "${role}" in
        comparison) sha="${CI_COMMIT_SHA}" ;;
        baseline)
            fetch_history
            sha="$(git -C "${history_repo}" merge-base origin/main "origin/${CI_COMMIT_BRANCH}")"
            share_history
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
