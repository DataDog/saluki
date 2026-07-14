#!/usr/bin/env sh

# Verifies the built Agent Data Plane binary imports no glibc symbol newer than the target's floor, so it runs on the
# oldest Datadog Agent-supported OS for that platform.
#
# The floor is declared per target in `build/targets/<triple>.sh` (TARGET_MAX_GLIBC) which should be set to the oldest
# supported glibc version for that platform, but can potentially become out-of-date. Ultimately, the glibc version used
# is dependent on the Datadog Agent buildimage we base our `build-ci` image off of.
#
# "Weak" undefined references are ignored: the dynamic loader leaves an absent weak symbol NULL and the referencing code
# falls back. In essence, weak references won't lead to the resulting binary failing to start if the GLIBC version they
# target is not present on the system.

set -eu

binary="/out/agent-data-plane"

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
build_target="${BUILD_TARGET:-default}"
target_profile="${script_dir}/targets/${build_target}.sh"

if [ ! -f "${target_profile}" ]; then
    echo "ERROR: unsupported build target '${build_target}'." >&2
    exit 1
fi

TARGET_MAX_GLIBC=""
. "${target_profile}"
max_glibc="${TARGET_MAX_GLIBC}"

if [ -z "${max_glibc}" ]; then
    echo "[*] No glibc floor set for this target; skipping the glibc-floor check."
    exit 0
fi

if ! command -v readelf >/dev/null 2>&1; then
    echo "ERROR: readelf not found; cannot verify the glibc floor (expected <= ${max_glibc})." >&2
    exit 1
fi

floor_major="${max_glibc%%.*}"
floor_minor="${max_glibc#*.}"

# List non-weak dynamic symbols whose required glibc version exceeds the floor. readelf's Bind column
# ($5) distinguishes WEAK from GLOBAL; the version is the `@GLIBC_x.y` suffix on the symbol name ($8).
too_new="$(readelf --dyn-syms --wide "${binary}" \
    | awk -v maj="${floor_major}" -v min="${floor_minor}" '
        $5 == "WEAK" { next }
        /@GLIBC_/ {
            v = $0; sub(/.*@GLIBC_/, "", v); sub(/[^0-9.].*/, "", v);
            split(v, a, ".");
            if (a[1] > maj || (a[1] == maj && a[2] > min)) print $8
        }' | sort -u)"

if [ -n "${too_new}" ]; then
    echo "ERROR: agent-data-plane imports non-weak glibc symbols newer than the GLIBC_${max_glibc} floor:" >&2
    echo "${too_new}" | sed 's/^/  /' >&2
    exit 1
fi

echo "[*] glibc floor OK: agent-data-plane imports no non-weak symbol newer than GLIBC_${max_glibc}."
