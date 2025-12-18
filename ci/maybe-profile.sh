#!/usr/bin/env sh
#
# A wrapper for invoking a process through `ddprof` if internal profiling is enabled.
#
# We use this to directly run ADP via `ddprof` when profiling is enabled, as the `LD_PRELOAD`-based approach taken by
# SMP itself is not compatible with statically linked binaries like ADP.

set -eu

env

# Check if internal profiling is enabled.
if [ "${SMP_PROFILING_ENABLED:-""}" = "true" ]; then
    # Run through `ddprof`.
    #
    # We specifically pass in any SMP-provided profiling tags so that we get things like `experiment`, `variant`, and
    # so on for profile tags which we need to actually be able to filter the profiles and split them apart, etc.
    /ddprof \
        --tags "${SMP_PROFILING_EXTRA_TAGS:-"smp_tags_missing:true"}" \
        "$@"
else
    # Run directly.
    exec "$@"
fi
