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
# The benchmark jobs use a shallow checkout (see `.smp-checkout` in .gitlab/benchmark.yml), so
# this script fetches the history it needs. It fetches only the last `history_depth` commits of
# main and of the branch. If the commit is not in those commits, it fetches the full history and
# tries again.

set -euo pipefail
# Keep `set -e` in command substitutions, so that a failed fetch stops the script.
shopt -s inherit_errexit

role="${1:?usage: compute-smp-sha.sh <baseline|comparison>}"

# The number of commits to fetch from each branch tip before we fetch the full history. This is
# enough for a branch that is up to this many commits behind main, and for some weeks of commits
# on main. Set `SMP_HISTORY_DEPTH` to change it, for example to test the full-history fallback.
history_depth="${SMP_HISTORY_DEPTH:-1000}"

# The refspecs for main and for the current branch. On the nightly schedule, the current branch
# is main.
history_refspecs() {
    echo "+refs/heads/main:refs/remotes/origin/main"
    if [[ "${CI_COMMIT_BRANCH}" != "main" ]]; then
        echo "+refs/heads/${CI_COMMIT_BRANCH}:refs/remotes/origin/${CI_COMMIT_BRANCH}"
    fi
}

fetch_history() {
    local refspecs
    mapfile -t refspecs < <(history_refspecs)
    # TEMPORARY diagnostics.
    echo "compute-smp-sha: before fetch: shallow=$(git rev-parse --is-shallow-repository) count=$(git rev-list --count HEAD) refs:" >&2
    git for-each-ref >&2
    git config --list --show-origin | grep -iv "token\|auth\|header" >&2 || true
    git fetch --verbose --deepen="${history_depth}" origin "${refspecs[@]}" >&2
    echo "compute-smp-sha: after fetch: shallow=$(git rev-parse --is-shallow-repository) count=$(git rev-list --count HEAD) main=$(git rev-list --count origin/main || true)" >&2
    git for-each-ref >&2
}

# Fetch the full history. Returns 1 if the repository has the full history already.
fetch_full_history() {
    local refspecs
    if [[ "$(git rev-parse --is-shallow-repository)" != "true" ]]; then
        return 1
    fi
    echo "compute-smp-sha: the commit is not in the last ${history_depth} commits, fetching the full history" >&2
    mapfile -t refspecs < <(history_refspecs)
    git fetch --verbose --unshallow origin "${refspecs[@]}" >&2
    # TEMPORARY diagnostics.
    echo "compute-smp-sha: after unshallow: shallow=$(git rev-parse --is-shallow-repository) count=$(git rev-list --count HEAD) main=$(git rev-list --count origin/main || true)" >&2
}

# Most recent origin/main commit committed strictly before 00:00:00 UTC of the given
# YYYY-MM-DD date.
last_commit_before_day() {
    git rev-list -n 1 --before="$1 00:00:00 +0000" origin/main
}

# Fetch the history, then run the given command and print its output. If the command gives no
# output, fetch the full history and run the command again.
#
# A shallow history cannot give an incorrect result: if the correct commit is not in the shallow
# history, then no older commit that would also match is in it.
with_history() {
    local sha
    fetch_history
    sha="$("$@" || true)"
    if [[ -z "${sha}" ]] && fetch_full_history; then
        sha="$("$@" || true)"
    fi
    if [[ -z "${sha}" ]]; then
        echo "compute-smp-sha: could not resolve the ${role} commit with: $*" >&2
        return 1
    fi
    echo "${sha}"
}

if [[ "${CI_PIPELINE_SOURCE:-}" == "schedule" ]]; then
    run_date="$(date -u -d "${CI_PIPELINE_CREATED_AT}" +%F)"
    day_before="$(date -u -d "${run_date} -1 day" +%F)"
    case "${role}" in
        comparison) sha="$(with_history last_commit_before_day "${run_date}")" ;;
        baseline)   sha="$(with_history last_commit_before_day "${day_before}")" ;;
        *) echo "unknown role: ${role}" >&2; exit 1 ;;
    esac
else
    case "${role}" in
        comparison) sha="${CI_COMMIT_SHA}" ;;
        baseline)   sha="$(with_history git merge-base origin/main "origin/${CI_COMMIT_BRANCH}")" ;;
        *) echo "unknown role: ${role}" >&2; exit 1 ;;
    esac
fi

echo "compute-smp-sha: ${role} commit is ${sha}" >&2
echo "${sha}"
