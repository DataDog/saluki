#!/bin/sh

set -eu

# Refreshes the vendored APM semantic mappings from the Datadog Agent and commits the result.
#
# Usage: update-apm-semantics.sh [REF]
#
# REF is the upstream branch, tag or commit to read from, and defaults to `main`. The script finds the last upstream
# commit that touched the mappings on that REF, downloads the file as it was at that commit, and records the commit in
# `_version.txt` beside the snapshot. If the downloaded bytes differ from the vendored copy, the script commits both
# files, and nothing else, in the repository that contains the current directory. If they are equal, it changes nothing.
#
# The upstream file's own `version` and `content_hash` do not change reliably when its content does, so freshness is
# decided by comparing bytes.
#
# Optional environment variables:
# - GITHUB_TOKEN: Sent to `api.github.com` only, to avoid the unauthenticated rate limit. It is not required.

UPSTREAM_REPO="DataDog/datadog-agent"
UPSTREAM_PATH="pkg/trace/semantics/mappings.json"
SEMANTICS_DIR="lib/datadog-agent/apm/src/semantics"
MAPPINGS="${SEMANTICS_DIR}/mappings.json"
VERSION_FILE="${SEMANTICS_DIR}/_version.txt"

fail() {
    echo "[!]: $1" >&2
    exit 1
}

if [ "$#" -gt 1 ]; then
    fail "Usage: $0 [REF]"
fi

REF="${1:-}"
if [ -z "${REF}" ]; then
    REF="main"
fi

# Reject option-like refs and URL query delimiters before inserting REF into the API URL.
case "${REF}" in
    -* | *[!A-Za-z0-9._/-]*) fail "REF '${REF}' must only contain letters, digits, '.', '_', '/' and '-', and must not start with '-'." ;;
esac

ROOT=$(git -C "${PWD}" rev-parse --show-toplevel)
cd "${ROOT}"

# Never overwrite a change the user already has in either file. Changes to any other path are left alone.
if [ -n "$(git -C "${ROOT}" status --porcelain -- "${MAPPINGS}" "${VERSION_FILE}")" ]; then
    fail "${MAPPINGS} or ${VERSION_FILE} has uncommitted changes. Commit or discard them first."
fi

API_TMP=""
JSON_TMP=""
VERSION_TMP=""
trap 'rm -f "${API_TMP}" "${JSON_TMP}" "${VERSION_TMP}"' EXIT
# `sh` skips the EXIT trap when a signal kills it, so turn the signals into an exit.
trap 'exit 1' HUP INT TERM

# Temporary files live beside the targets so that the final `mv` is a rename within one filesystem.
API_TMP=$(mktemp "${SEMANTICS_DIR}/.api.XXXXXX")
JSON_TMP=$(mktemp "${SEMANTICS_DIR}/.mappings.json.XXXXXX")
VERSION_TMP=$(mktemp "${SEMANTICS_DIR}/.version.XXXXXX")

echo "[*] Finding the last upstream commit to touch ${UPSTREAM_PATH} on '${REF}'..."
API_URL="https://api.github.com/repos/${UPSTREAM_REPO}/commits?path=${UPSTREAM_PATH}&sha=${REF}&per_page=1"
if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL --retry 3 -H "Authorization: Bearer ${GITHUB_TOKEN}" -o "${API_TMP}" "${API_URL}"
else
    curl -fsSL --retry 3 -o "${API_TMP}" "${API_URL}"
fi

COMMIT=$(jq -er '.[0].sha' "${API_TMP}") || fail "No upstream commit found for '${REF}'."
if [ "${#COMMIT}" -ne 40 ]; then
    fail "Upstream returned '${COMMIT}', which is not a full commit SHA."
fi
case "${COMMIT}" in
    *[!0-9a-f]*) fail "Upstream returned '${COMMIT}', which is not a full commit SHA." ;;
esac

echo "[*] Downloading ${UPSTREAM_PATH} at ${COMMIT}..."
curl -fsSL --retry 3 -o "${JSON_TMP}" "https://raw.githubusercontent.com/${UPSTREAM_REPO}/${COMMIT}/${UPSTREAM_PATH}"

# Only check the document's shape here. Rust's embedded-registry loader skips unknown concepts and validates mappings
# for known ones; PR CI catches incompatibilities.
if ! jq -e '(.concepts | type == "object" and length > 0) and (.metadata.content_hash | type == "string" and length > 0)' \
    "${JSON_TMP}" >/dev/null; then
    fail "The downloaded file is not a semantic mappings document."
fi

# A newer upstream commit with identical bytes is not worth a change: `_version.txt` must name a commit whose file
# matches the vendored one, and the existing SHA already does.
if cmp -s "${JSON_TMP}" "${MAPPINGS}"; then
    echo "[*] ${MAPPINGS} is already current."
    exit 0
fi

# Lists the concept names selected by the jq filter in `$1`, one per line and sorted. jq runs on its own, because a pipe
# would hide its failure and turn the lists into "none".
concepts() {
    names=$(jq -nr --slurpfile old "${MAPPINGS}" --slurpfile new "${JSON_TMP}" "$1") ||
        fail "Could not compare the concepts in ${MAPPINGS} with the upstream file."
    printf '%s\n' "${names}" | LC_ALL=C sort
}

# Prints a commit message section for the concept names in `$2`.
describe() {
    if [ -z "$2" ]; then
        printf '%s: none\n' "$1"
    else
        printf '%s:\n' "$1"
        printf '%s\n' "$2" | sed 's/^/- /'
    fi
}

ADDED=$(concepts '($new[0].concepts | keys) - ($old[0].concepts | keys) | .[]')
REMOVED=$(concepts '($old[0].concepts | keys) - ($new[0].concepts | keys) | .[]')
CHANGED=$(concepts '$old[0].concepts as $o | $new[0].concepts | to_entries[] | select($o[.key] != null and $o[.key] != .value) | .key')

BODY=$(
    describe "Added concepts" "${ADDED}"
    echo
    describe "Removed concepts" "${REMOVED}"
    echo
    describe "Changed concepts" "${CHANGED}"
    echo
    echo "Source: https://github.com/${UPSTREAM_REPO}/commit/${COMMIT}"
)
SUBJECT="chore(apm): refresh semantic mappings from datadog-agent $(printf '%s' "${COMMIT}" | cut -c1-12)"

# `mktemp` files are private, but the targets are ordinary tracked files.
chmod 644 "${JSON_TMP}"
printf '%s\n' "${COMMIT}" >"${VERSION_TMP}"
chmod 644 "${VERSION_TMP}"
mv "${JSON_TMP}" "${MAPPINGS}"
mv "${VERSION_TMP}" "${VERSION_FILE}"

# Naming the paths on both commands makes the commit contain only these files, whatever else is staged.
git -C "${ROOT}" add -- "${MAPPINGS}" "${VERSION_FILE}"
if ! git -C "${ROOT}" commit --quiet -m "${SUBJECT}" -m "${BODY}" -- "${MAPPINGS}" "${VERSION_FILE}"; then
    fail "The commit failed. ${MAPPINGS} and ${VERSION_FILE} were updated but not committed."
fi

echo "[*] Committed: ${SUBJECT}"
