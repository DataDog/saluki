#!/usr/bin/env sh

# Verifies the built Agent Data Plane binary carries a GNU build ID (`NT_GNU_BUILD_ID` in `.note.gnu.build-id`).
#
# Profilers and symbolizers use the build ID to identify the exact binary a process is running, and to match it against
# its uploaded debug symbols. Whether the linker emits one depends on how the toolchain's GCC was configured, not on
# rustc, so a toolchain or base image change can silently drop it. The glibc target profiles request it explicitly via
# `-Wl,--build-id` in `build/targets/<triple>.sh`; this check ensures that keeps working.

set -eu

binary="/out/agent-data-plane"

if ! command -v readelf >/dev/null 2>&1; then
    echo "ERROR: readelf not found; cannot verify that agent-data-plane has a GNU build ID." >&2
    exit 1
fi

build_id="$(readelf --notes --wide "${binary}" | awk '/Build ID:/ { sub(/.*Build ID: */, ""); print $1; exit }')"

if [ -z "${build_id}" ]; then
    echo "ERROR: agent-data-plane has no GNU build ID (missing .note.gnu.build-id)." >&2
    echo "       Make sure the target profile in build/targets/ passes '-C link-arg=-Wl,--build-id' via TARGET_RUSTFLAGS." >&2
    exit 1
fi

echo "[*] GNU build ID OK: agent-data-plane has build ID ${build_id}."
