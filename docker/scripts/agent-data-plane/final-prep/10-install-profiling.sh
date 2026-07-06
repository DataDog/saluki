#!/usr/bin/env sh
#
# Stages the profiling helpers into the mirrored output tree for internal benchmarking builds.
#
# For internal builds (INTERNAL_BUILD=true), downloads `ddprof` and stages the `maybe-profile.sh` wrapper (bind-mounted
# at /tooling/maybe-profile.sh) into /rootfs so the final image can run ADP under the profiler in SMP. For non-internal
# builds this is a no-op -- the mirror still has everything else, and the final stage copies the whole tree regardless.

set -eu

ddprof_version="v0.22.1"

if [ "${INTERNAL_BUILD:-}" = "true" ]; then
    apt-get update
    apt-get install --no-install-recommends -y ca-certificates=20260601~26.04.1 curl=8.18.0-1ubuntu2.2
    curl -s -L -o /rootfs/ddprof \
        "https://github.com/DataDog/ddprof/releases/download/${ddprof_version}/ddprof-${TARGETARCH}"
    chmod +x /rootfs/ddprof
    cp /tooling/maybe-profile.sh /rootfs/maybe-profile.sh
    chmod +x /rootfs/maybe-profile.sh
fi
