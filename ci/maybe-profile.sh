#!/usr/bin/env sh
#
# A wrapper for invoking a process through `ddprof` if internal profiling is enabled.
#
# We use this to directly run ADP via `ddprof` when profiling is enabled, as the `LD_PRELOAD`-based approach taken by
# SMP itself is not compatible with statically linked binaries like ADP.

set -eu

# Check if internal profiling is enabled.
if [ "${SMP_PROFILING_ENABLED:-""}" = "true" ]; then
    # Run through `ddprof`.
    /ddprof "$@"
else
    # Run directly.
    exec "$@"
fi
