#!/usr/bin/env bash
# Regenerates the `pcommon.Value.AsString` test vectors (../pdata_as_string_vectors.json).
#
# Uses the version of pdata that the Integration version of the Datadog Agent requires, the Agent commit in
# .datadog-agent-version.json, and runs Go in a digest-pinned stock image. The host needs Docker, curl, Python 3, and
# bash.
set -euo pipefail

# Multi-arch index of golang:1.26.10, the same image that the config recorder uses.
GO_IMAGE="golang@sha256:c1bc3620b0460d97001977fadbe8d0e94d7be5cbb064f4f6b268d95375c3ac71"
GOMOD_VOLUME="saluki-pdata-as-string-gomod"
AGENT_RAW_URL="${AGENT_RAW_URL:-https://raw.githubusercontent.com/DataDog/datadog-agent}"

GENERATOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$GENERATOR_DIR/../../../../../../.." && pwd)"
VECTORS="$GENERATOR_DIR/../pdata_as_string_vectors.json"

die() { echo "[!] $*" >&2; exit 1; }

PIN="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["integration"]["git_revision"])' \
    "$REPO_ROOT/.datadog-agent-version.json")"
[ -n "$PIN" ] || die "the Agent version file has no Integration commit"

AGENT_GO_MOD="$(curl -fsSL "$AGENT_RAW_URL/$PIN/go.mod")"
PDATA_VERSION="$(awk '$1 == "go.opentelemetry.io/collector/pdata" { print $2; exit }' <<<"$AGENT_GO_MOD")"
[ -n "$PDATA_VERSION" ] || die "the Agent's go.mod at $PIN doesn't require go.opentelemetry.io/collector/pdata"
echo "[*] The Agent at $PIN requires pdata $PDATA_VERSION."

GOPROXY_ARGS=()
if [ -n "${GOPROXY:-}" ]; then
    GOPROXY_ARGS=(-e GOPROXY="$GOPROXY")
fi

HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

# The module cache volume starts out owned by root. Give it to the host user, as the container runs as that user.
docker volume create "$GOMOD_VOLUME" >/dev/null
docker run --rm -v "$GOMOD_VOLUME:/gomod" "$GO_IMAGE" \
    sh -c '[ "$(stat -c %u:%g /gomod)" = "'"$HOST_UID:$HOST_GID"'" ] || chown -R "'"$HOST_UID:$HOST_GID"'" /gomod'

# Run as the host user, so that the files written on the host belong to the host user.
docker run --rm \
    --user "$HOST_UID:$HOST_GID" \
    -e HOME=/tmp \
    -e GOWORK=off \
    -e GOFLAGS=-modcacherw \
    -e GOMODCACHE=/gomod \
    ${GOPROXY_ARGS[@]+"${GOPROXY_ARGS[@]}"} \
    -v "$GOMOD_VOLUME:/gomod" \
    -v "$GENERATOR_DIR:/generator" \
    -w /generator \
    "$GO_IMAGE" \
    sh -c "go get go.opentelemetry.io/collector/pdata@$PDATA_VERSION && go mod tidy && go vet . && go run ." \
    > "$VECTORS.tmp"
mv "$VECTORS.tmp" "$VECTORS"
echo "[*] Wrote $VECTORS."
