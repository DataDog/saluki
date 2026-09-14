#!/usr/bin/env bash
#
# Runs cargo-deny's bans/licenses/sources checks strictly (any failure blocks), and runs the
# advisories check with the following policy:
#
#   - On `main`, or a tagged release build, or when run outside of CI (i.e. locally), advisories
#     are strict too: any outstanding advisory fails, so regressions on trunk stay visible until
#     fixed.
#   - On any other branch (a pull request), we only fail on advisories that are *new* relative to
#     `main` -- i.e. introduced by this branch. An advisory that already fails on `main` (for
#     example, a freshly-published RUSTSEC entry against a dependency nobody in the PR touched) is
#     reported but does not block the PR, since fixing it isn't something the PR is responsible
#     for.
#
# This avoids blocking unrelated PRs every time a new vulnerability is published against an
# existing dependency, while still catching PRs that introduce a newly-vulnerable dependency.

set -euo pipefail

# Prints one fingerprint per unique error-level diagnostic found by the advisories check in the
# current working directory, one per line, sorted. Covers every advisory-check diagnostic code
# (vulnerability/unsound/unmaintained/notice advisories, plus yanked crates, which have no
# "advisory" field at all), keyed on the diagnostic code, advisory ID (if any), and affected
# package/version (from the diagnostic's primary label span).
advisory_fingerprints() {
    local output status
    # cargo-deny writes its JSON diagnostics to stderr (even with --format json), so swap stdout
    # and stderr to capture them and discard the (empty, for our purposes) real stdout. Exit status
    # 0 means no issues, 1 means it ran fine and found diagnostics to report below -- anything else
    # is an operational failure (e.g. it couldn't fetch the advisory database), which must not be
    # swallowed and silently treated as "no advisories found".
    status=0
    output="$(cargo deny --format json check advisories 2>&1 1>/dev/null)" || status=$?
    if [[ "${status}" -gt 1 ]]; then
        echo "cargo-deny failed to run (exit ${status}):" >&2
        printf '%s\n' "${output}" >&2
        exit "${status}"
    fi
    printf '%s\n' "${output}" | jq -r '
        select(.type == "diagnostic")
        | select(.fields.severity == "error")
        | "\(.fields.code)|\(.fields.advisory.id // "")|\(.fields.labels[0].span // "")"
    ' | sort -u
}

echo "[*] Checking for banned dependencies, license conflicts, and untrusted dependency sources..."
cargo deny check bans licenses sources

strict=0
if [[ "${CI_COMMIT_BRANCH:-}" == "main" || -n "${CI_COMMIT_TAG:-}" || -z "${CI:-}" ]]; then
    strict=1
fi

if [[ "${strict}" -eq 1 ]]; then
    echo "[*] Checking for dependency advisories (strict)..."
    exec cargo deny check advisories --hide-inclusion-graph --show-stats
fi

echo "[*] Checking for dependency advisories (only new advisories relative to main will fail this job)..."

pr_failures="$(advisory_fingerprints)"

# Compare against the current tip of main, not the branch's merge-base with main: if main has
# since fixed an advisory that this (unrebased) branch still carries, merging this branch would
# reintroduce it, so that must still be reported as a new failure rather than waved through as
# "pre-existing".
git fetch --quiet --depth=1 origin main

worktree_dir="$(mktemp -d)"
trap 'git worktree remove --force "${worktree_dir}" >/dev/null 2>&1 || true' EXIT
git worktree add --quiet --detach "${worktree_dir}" origin/main

main_failures="$(cd "${worktree_dir}" && advisory_fingerprints)"

new_failures="$(comm -23 <(printf '%s\n' "${pr_failures}") <(printf '%s\n' "${main_failures}") | sed '/^$/d')"

if [[ -n "${new_failures}" ]]; then
    echo "New advisory failure(s) introduced by this branch (not present on main):"
    echo "${new_failures}"
    echo
    echo "Full advisory report:"
    cargo deny check advisories --hide-inclusion-graph --show-stats || true
    exit 1
fi

pre_existing_count="$(printf '%s\n' "${pr_failures}" | sed '/^$/d' | wc -l | tr -d ' ')"
if [[ "${pre_existing_count}" -gt 0 ]]; then
    echo "${pre_existing_count} pre-existing advisory issue(s) inherited from main; not blocking this PR:"
    echo "${pr_failures}"
else
    echo "No advisory issues found."
fi
