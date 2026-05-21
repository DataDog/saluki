#!/usr/bin/env bash
#
# Run a command inside an ephemeral Tart macOS VM with the Saluki repo mounted.
#
# Usage:
#     tooling/tart/run-in-vm.sh <command> [args...]
#
# Examples:
#     tooling/tart/run-in-vm.sh make test-integration-macos
#     tooling/tart/run-in-vm.sh sh -c 'uname -a && ls'
#
# Environment variables:
#     TART_BASE_IMAGE   Base image to clone from. Default:
#                       ghcr.io/cirruslabs/macos-sequoia-base:latest
#     TART_VM_NAME      Override the ephemeral VM name. Default: a per-PID name.
#     TART_SHARED_NAME  Mount tag used to share the repo into the VM. Default: "saluki".
#                       Inside the VM the repo lives at
#                       /Volumes/My Shared Files/$TART_SHARED_NAME
#     TART_BOOT_TIMEOUT Seconds to wait for the VM to become reachable. Default: 180.
#     TART_KEEP_VM      If set to "1", do not delete the VM after the command exits.
#                       The VM is still stopped. Useful for debugging.
#
# Prerequisites:
#     - macOS host (Apple Silicon recommended)
#     - tart installed (https://tart.run/)
#
# This script is intentionally minimal — it manages a VM lifecycle and shells
# out to `tart exec` to run commands. It does not know or care what runs inside
# the VM.

set -euo pipefail

BASE_IMAGE="${TART_BASE_IMAGE:-ghcr.io/cirruslabs/macos-sequoia-base:latest}"
VM_NAME="${TART_VM_NAME:-saluki-integration-$$}"
SHARED_NAME="${TART_SHARED_NAME:-saluki}"
BOOT_TIMEOUT="${TART_BOOT_TIMEOUT:-180}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ---- helpers ---------------------------------------------------------------

log() {
    printf '[tart] %s\n' "$*" >&2
}

error() {
    printf '[tart] error: %s\n' "$*" >&2
}

ensure_tart() {
    if ! command -v tart >/dev/null 2>&1; then
        error "tart is not installed. Install it from https://tart.run/ or via 'brew install cirruslabs/cli/tart'."
        exit 1
    fi
}

ensure_macos() {
    if [[ "$(uname -s)" != "Darwin" ]]; then
        error "This script must be run on a macOS host. Tart requires macOS."
        exit 1
    fi
}

# Returns 0 if a local VM with the given name exists.
vm_exists() {
    tart list --format=json 2>/dev/null \
        | grep -E "\"Name\"\s*:\s*\"$1\"" >/dev/null
}

cleanup() {
    local exit_code=$?
    # Do not let cleanup failures mask the real exit code.
    set +e

    if [[ -n "${TART_RUN_PID:-}" ]]; then
        kill -TERM "$TART_RUN_PID" 2>/dev/null
    fi

    if vm_exists "$VM_NAME"; then
        log "Stopping VM '$VM_NAME'..."
        tart stop "$VM_NAME" 2>/dev/null
        # tart stop returns before the VM is fully down; give it a moment.
        sleep 1
    fi

    if [[ "${TART_KEEP_VM:-0}" == "1" ]]; then
        log "TART_KEEP_VM=1 set; leaving VM '$VM_NAME' on disk (use 'tart delete $VM_NAME' to remove)."
    elif vm_exists "$VM_NAME"; then
        log "Deleting VM '$VM_NAME'..."
        tart delete "$VM_NAME" 2>/dev/null
    fi

    exit "$exit_code"
}

# ---- main ------------------------------------------------------------------

ensure_macos
ensure_tart

if [[ $# -eq 0 ]]; then
    error "missing command to run inside the VM"
    cat <<EOF >&2
Usage: $0 <command> [args...]
Example: $0 make test-integration-macos
EOF
    exit 2
fi

trap cleanup EXIT INT TERM

# Pull the base image only if it isn't already present locally. The first pull
# is ~30 GB and may take a while; subsequent runs reuse the cached image.
if ! vm_exists "$BASE_IMAGE"; then
    log "Base image '$BASE_IMAGE' not present locally."
    log "Pulling now (this can take 10+ minutes on a first run, downloads ~30 GB)..."
    tart pull "$BASE_IMAGE"
fi

# If the ephemeral VM somehow already exists (previous abort?), wipe it before
# cloning fresh. Cloning into an existing name fails otherwise.
if vm_exists "$VM_NAME"; then
    log "Removing stale VM '$VM_NAME' before cloning..."
    tart stop "$VM_NAME" 2>/dev/null || true
    sleep 1
    tart delete "$VM_NAME"
fi

log "Cloning '$BASE_IMAGE' -> ephemeral VM '$VM_NAME'..."
tart clone "$BASE_IMAGE" "$VM_NAME"

log "Booting VM. Repo at '$REPO_ROOT' will be mounted as '/Volumes/My Shared Files/$SHARED_NAME' inside the VM."
tart run --no-graphics \
    --dir="${SHARED_NAME}:${REPO_ROOT}" \
    "$VM_NAME" \
    >/dev/null 2>&1 &
TART_RUN_PID=$!

log "Waiting up to ${BOOT_TIMEOUT}s for the VM to become reachable via tart exec..."
deadline=$(( SECONDS + BOOT_TIMEOUT ))
ready=0
while (( SECONDS < deadline )); do
    if tart exec "$VM_NAME" -- true >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 2
done

if (( ready == 0 )); then
    error "VM did not become reachable within ${BOOT_TIMEOUT}s. Inspect with 'tart list' / 'tart ip $VM_NAME' before retrying."
    exit 1
fi

log "VM ready. Running command: $*"

# `tart exec <name> -- cmd args...` passes args verbatim. We wrap in `sh -c` so
# we can cd into the shared mount before running the user's command.
guest_repo="/Volumes/My Shared Files/${SHARED_NAME}"
inner_cmd=$(printf 'cd %q && %s' "$guest_repo" "$*")

tart exec "$VM_NAME" -- sh -c "$inner_cmd"
