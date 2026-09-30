#!/usr/bin/env bash
# Regenerates the config recorder's corpus (corpus.jsonl) from nothing.
#
# The host needs Docker, git, make and bash. Everything else runs in two digest-pinned stock images:
# Python for the Agent's schema codegen and merge, Go for building, checking and running the
# recorder. See README.md.
set -euo pipefail

GO_IMAGE="golang@sha256:e30143be198ab04cf7ba25fba83ab3a692ca584c994aad0bf131fa0eb32dd8c1"
# Multi-arch index of uv 0.9-python3.12-bookworm-slim (uv 0.9.30, Python 3.12.12).
PYTHON_IMAGE="ghcr.io/astral-sh/uv@sha256:e5b65587bce7de595f299855d7385fe7fca39b8a74baa261ba1b7147afa78e58"
GOMOD_VOLUME="saluki-config-recorder-gomod"
GOBUILD_VOLUME="saluki-config-recorder-gobuild"
UV_VOLUME="saluki-config-recorder-uv"
# `requests` is imported on codegen's import path but is not in the Agent's dev requirements.
REQUESTS_PIN="requests==2.32.5"
AGENT_REPO_URL="${AGENT_REPO_URL:-https://github.com/DataDog/datadog-agent.git}"

RECORDER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$RECORDER_DIR/../../.." && pwd)"
STATE_DIR="$REPO_ROOT/target/config-recorder"
AGENT_DIR="$STATE_DIR/datadog-agent"
OUT_DIR="$STATE_DIR/out"
SCHEMA_FILE="$STATE_DIR/core_schema.merged.yaml"
GENERATED_DIR="$STATE_DIR/generated-cases"
OVERLAY_FILE="$REPO_ROOT/lib/datadog-agent/config/schema/schema_overlay.yaml"
CORPUS="$RECORDER_DIR/corpus.jsonl"

# Paths inside the containers. Both containers see the checkout and the schema at the same paths.
C_STATE="/state"
C_AGENT="$C_STATE/datadog-agent"
C_SCHEMA="$C_STATE/core_schema.merged.yaml"

# Run every container as the host user, so files it writes on the host end up owned by the host
# user rather than root.
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

# Pass the host's Go module proxy through, the same way AGENT_REPO_URL already is.
# Use ${a[@]+...} for empty arrays: macOS bash 3.2 treats "${a[@]}" as unbound under set -u.
GOPROXY_ARGS=()
# Use --platform linux/arm64 for every `docker run` unless CONFIG_RECORDER_PLATFORM overrides it.
# This keeps the corpus host-independent until an amd64 run proves the records byte-identical.
PLATFORM_ARGS=(--platform "${CONFIG_RECORDER_PLATFORM:-linux/arm64}")
if [ -n "${GOPROXY:-}" ]; then
    GOPROXY_ARGS=(-e GOPROXY="$GOPROXY")
fi

step() { echo "[*] $*"; }
warn() { echo "[!] warning: $*" >&2; }
# The pin fetch reads a public repository. Ignore the host's global and system git config, whose
# URL rewrites (for example https to ssh) would otherwise demand SSH credentials.
agent_git() { GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 git -C "$AGENT_DIR" "$@"; }
die() { echo "[!] $*" >&2; exit 1; }

# 1. The pin.
PIN="$(tr -d '[:space:]' < "$REPO_ROOT/lib/datadog-agent/config/schema/core/_version.txt")"
[[ "$PIN" =~ ^[0-9a-f]{40}$ ]] || die "_version.txt does not hold a 40-character lowercase hex commit: '$PIN'"
step "Agent pin is $PIN"

# 2. The checkout: a shallow clone at the pin. Reused only when HEAD is already the pin and the
# checkout has no tracked changes; otherwise fetched fresh, printing why the existing checkout was
# not reused.
mkdir -p "$STATE_DIR"
head=""
dirty=""
if [ -d "$AGENT_DIR/.git" ]; then
    head="$(agent_git rev-parse HEAD 2>/dev/null || true)"
    if [ "$head" = "$PIN" ] && ! agent_git diff --quiet HEAD; then
        dirty=1
    fi
fi
if [ "$head" = "$PIN" ] && [ -z "$dirty" ]; then
    step "reusing clean Agent checkout at $PIN"
else
    if [ -n "$dirty" ]; then
        step "not reusing Agent checkout: tracked changes present at $PIN"
    elif [ -n "$head" ]; then
        step "not reusing Agent checkout: HEAD is $head, not the pin $PIN"
    else
        step "no existing Agent checkout"
    fi
    step "fetching Agent $PIN"
    if [ ! -d "$AGENT_DIR/.git" ]; then
        rm -rf "$AGENT_DIR"
        mkdir -p "$AGENT_DIR"
        agent_git init -q
    fi
    agent_git fetch -q --depth 1 "$AGENT_REPO_URL" "$PIN"
    agent_git checkout -q --force --detach "$PIN"
    # Keep gitignored codegen output; remove everything else that is untracked.
    agent_git clean -q -fd
fi
AGENT_COMMIT="$(agent_git rev-parse HEAD)"
[ "$AGENT_COMMIT" = "$PIN" ] || die "Agent checkout is at $AGENT_COMMIT, not the pin $PIN"

# 2a. The vendored schema should be the pin's schema. A difference means `_version.txt` and the
# vendored files disagree, which is fatal unless CONFIG_RECORDER_ALLOW_SCHEMA_DIFF=1. diff exit 2 (a
# missing directory, an unreadable file) means the comparison never happened, so it is always fatal.
VENDORED_SCHEMA_DIR="$REPO_ROOT/lib/datadog-agent/config/schema/core"
AGENT_SCHEMA_DIR="$AGENT_DIR/pkg/config/schema/yaml"
vendor_status=0
vendor_diff="$(diff -rq -x _version.txt "$VENDORED_SCHEMA_DIR" "$AGENT_SCHEMA_DIR" 2>&1)" || vendor_status=$?
if [ "$vendor_status" -gt 1 ]; then
    echo "$vendor_diff" >&2
    die "cannot compare lib/datadog-agent/config/schema/core/ with the pin's pkg/config/schema/yaml/"
elif [ "$vendor_status" -eq 1 ]; then
    echo "$vendor_diff" >&2
    if [ "${CONFIG_RECORDER_ALLOW_SCHEMA_DIFF:-}" = "1" ]; then
        warn "lib/datadog-agent/config/schema/core/ differs from the pin's pkg/config/schema/yaml/ (allowed)"
    else
        die "lib/datadog-agent/config/schema/core/ differs from the pin's pkg/config/schema/yaml/; re-vendor the schema, or set CONFIG_RECORDER_ALLOW_SCHEMA_DIFF=1"
    fi
fi

# 3. The recorder source, copied fresh into the checkout.
step "copying the recorder into cmd/config-recorder"
rm -rf "$AGENT_DIR/cmd/config-recorder"
cp -R "$RECORDER_DIR/go" "$AGENT_DIR/cmd/config-recorder"

docker volume create "$GOMOD_VOLUME" >/dev/null
docker volume create "$GOBUILD_VOLUME" >/dev/null
docker volume create "$UV_VOLUME" >/dev/null

# 3a. The three named volumes must be writable by the host user the containers run as. They start
# out root-owned; fix ownership in one short root step, only when it is wrong, so a fixed volume
# costs nothing on later runs.
step "checking cache volume ownership"
docker run --rm ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
    -v "$GOMOD_VOLUME":/vol/gomod \
    -v "$GOBUILD_VOLUME":/vol/gobuild \
    -v "$UV_VOLUME":/vol/uv \
    "$GO_IMAGE" \
    bash -euo pipefail -c '
        for d in /vol/gomod /vol/gobuild /vol/uv; do
            owner="$(stat -c "%u:%g" "$d")"
            if [ "$owner" != "'"$HOST_UID:$HOST_GID"'" ]; then
                echo "[*] fixing ownership of $d ($owner -> '"$HOST_UID:$HOST_GID"')"
                chown -R '"$HOST_UID:$HOST_GID"' "$d"
            fi
        done
    '

# 3b. The inputs digest (record.md §2): the recorder's inputs, hashed by the same recipe a
# developer can run by hand with `shasum`, computed here with `sha256sum` in the Go image, over
# the recorder directory mounted read-only.
step "computing the inputs digest"
INPUTS_DIGEST_HEX="$(docker run --rm ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
    --user "$HOST_UID:$HOST_GID" \
    -v "$RECORDER_DIR":/recorder:ro \
    -w /recorder \
    "$GO_IMAGE" \
    bash -euo pipefail -c '
        find go cases agent_codegen.py regenerate.sh -type f ! -path "*/.*" \
            | LC_ALL=C sort \
            | xargs sha256sum \
            | sha256sum \
            | cut -d" " -f1
    ')"
INPUTS_DIGEST="sha256:$INPUTS_DIGEST_HEX"
step "inputs digest is $INPUTS_DIGEST"

# 4. The Agent's schema codegen and core schema merge, with the Agent's pinned Python packages.
# Codegen always starts from a checkout with no gitignored files under pkg/config, so a reused
# checkout builds exactly what a fresh checkout builds.
agent_git clean -fdXq -- pkg/config
step "running the Agent's schema codegen and merge"
docker run --rm ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
    --user "$HOST_UID:$HOST_GID" \
    -e HOME=/tmp/home \
    -v "$STATE_DIR":"$C_STATE" \
    -v "$RECORDER_DIR/agent_codegen.py":/tools/agent_codegen.py:ro \
    -v "$UV_VOLUME":/uv-cache \
    -e UV_CACHE_DIR=/uv-cache \
    -e UV_PYTHON_DOWNLOADS=never \
    -w "$C_AGENT" \
    "$PYTHON_IMAGE" \
    bash -euo pipefail -c '
        mkdir -p "$HOME"
        uv_run() {
            uv run --quiet --no-project --with-requirements deps/py_dev_requirements.txt --with "$1" python3 "${@:2}"
        }
        uv_run "$0" /tools/agent_codegen.py
        uv_run "$0" tasks/schema/merge_schema.py pkg/config/schema/yaml/core_schema.yaml "$1"
    ' "$REQUESTS_PIN" "$C_SCHEMA"
[ -s "$SCHEMA_FILE" ] || die "schema merge did not write $SCHEMA_FILE"

# 5. Build, check, test and run the recorder over the hand-written and generated cases. The
# overlay is read only to choose the generated cases' keys; it is not a digest input.
step "checking, testing, building and running the recorder"
# The drive workdir keeps the written case files and first-snapshot dumps on the host.
WORK_DIR="$STATE_DIR/work"
rm -rf "$OUT_DIR" "$GENERATED_DIR" "$WORK_DIR"
mkdir -p "$OUT_DIR" "$GENERATED_DIR" "$WORK_DIR"
# The checkout is mounted read-only, but Go in workspace mode must write go.work.sum (gitignored,
# absent at the pin). Create it on the host so the mountpoint exists, and bind it read-write.
[ -f "$AGENT_DIR/go.work.sum" ] || : > "$AGENT_DIR/go.work.sum"
CHECKOUT_MOUNTS=(-v "$AGENT_DIR":"$C_AGENT":ro -v "$AGENT_DIR/go.work.sum":"$C_AGENT/go.work.sum")

run_go_step() {
    docker run --rm ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
        --user "$HOST_UID:$HOST_GID" \
        -e HOME=/tmp/home \
        "${CHECKOUT_MOUNTS[@]}" \
        -v "$SCHEMA_FILE":"$C_SCHEMA":ro \
        -v "$RECORDER_DIR/cases":/cases:ro \
        -v "$OVERLAY_FILE":/overlay.yaml:ro \
        -v "$GENERATED_DIR":/generated \
        -v "$OUT_DIR":/out \
        -v "$WORK_DIR":/work \
        -v "$GOMOD_VOLUME":/go/pkg/mod \
        -v "$GOBUILD_VOLUME":/tmp/go-build-cache \
        -e GOMODCACHE=/go/pkg/mod \
        -e GOPATH=/tmp/gopath \
        -e GOCACHE=/tmp/go-build-cache \
        -e GOFLAGS="-mod=readonly -buildvcs=false" \
        -e GOTOOLCHAIN=local \
        ${GOPROXY_ARGS[@]+"${GOPROXY_ARGS[@]}"} \
        -w "$C_AGENT" \
        "$GO_IMAGE" \
        bash -euo pipefail -c '
            mkdir -p "$HOME"
            echo "[*] checking workspace mode"
            work="$(go env GOWORK)"
            [ "$work" = "'"$C_AGENT"'/go.work" ] || {
                echo "[!] GOWORK is \"$work\", expected '"$C_AGENT"'/go.work" >&2
                exit 1
            }
            echo "[*] gofmt"
            unformatted="$(gofmt -l cmd/config-recorder/)"
            if [ -n "$unformatted" ]; then
                echo "[!] gofmt found unformatted files:" >&2
                echo "$unformatted" >&2
                exit 1
            fi
            # The otlp tag enables the Agent'"'"'s OTLP read (configcheck.ReadConfigSection).
            # It selects no file under pkg/config, comp/core/config or comp/core/configstream,
            # leaving config construction and streaming unchanged. Compare the recorder'"'"'s
            # dependency graph with and without the tag; fail if any Agent package other than
            # comp/otelcol/otlp/configcheck changes its file set or appears only with the tag.
            echo "[*] checking the otlp build tag against the rest of the dependency graph"
            go list -e -deps -f "{{.ImportPath}} {{.GoFiles}}" ./cmd/config-recorder | sort > /tmp/deps-no-tag.txt
            go list -e -deps -tags otlp -f "{{.ImportPath}} {{.GoFiles}}" ./cmd/config-recorder | sort > /tmp/deps-otlp.txt
            agent_pkgs="$(cut -d" " -f1 /tmp/deps-no-tag.txt /tmp/deps-otlp.txt \
                | grep "^github.com/DataDog/datadog-agent/" \
                | grep -v "^github.com/DataDog/datadog-agent/comp/otelcol/otlp/configcheck$" \
                | sort -u)"
            echo "[*] Agent packages compared for build-tag drift:"
            echo "$agent_pkgs"
            tag_fail=0
            for pkg in $agent_pkgs; do
                no_tag="$(grep -F "$pkg " /tmp/deps-no-tag.txt || true)"
                with_tag="$(grep -F "$pkg " /tmp/deps-otlp.txt || true)"
                if [ -z "$no_tag" ]; then
                    echo "[!] $pkg appears only with -tags otlp" >&2
                    tag_fail=1
                    continue
                fi
                if [ "$no_tag" != "$with_tag" ]; then
                    echo "[!] $pkg has a different file set with -tags otlp" >&2
                    echo "    without: $no_tag" >&2
                    echo "    with:    $with_tag" >&2
                    tag_fail=1
                fi
            done
            [ "$tag_fail" -eq 0 ] || exit 1
            echo "[*] go vet"
            go vet -tags otlp ./cmd/config-recorder/...
            echo "[*] go test"
            # CONFIG_RECORDER_TEST_SCHEMA lets the integration test (go/integration_test.go) build
            # the recorder binary and run its real drive/run-case subcommands on go/testdata/.
            CONFIG_RECORDER_TEST_SCHEMA="$0" go test -tags otlp -count=1 ./cmd/config-recorder/...
            echo "[*] go build"
            go build -tags otlp -o /tmp/config-recorder ./cmd/config-recorder
            echo "[*] generate"
            /tmp/config-recorder generate --schema "$0" --overlay /overlay.yaml --out /generated
            echo "[*] drive"
            /tmp/config-recorder drive --schema "$0" --cases /cases --cases /generated --workdir /work --out /out/corpus.jsonl \
                --agent-commit "$1" --container-image "$2" --inputs-digest "$3" --jobs "$4"
        ' "$C_SCHEMA" "$AGENT_COMMIT" "$GO_IMAGE" "$INPUTS_DIGEST" "${CONFIG_RECORDER_JOBS:-0}"
}

run_go_step || die "the Go step failed"

# 6. Replace the corpus only after a successful run.
[ -s "$OUT_DIR/corpus.jsonl" ] || die "the recorder wrote no corpus"
mv "$OUT_DIR/corpus.jsonl" "$CORPUS"
step "wrote ${CORPUS#"$REPO_ROOT"/}"
