#!/usr/bin/env sh
#
# A wrapper for invoking a process through `ddprof` if internal profiling is enabled.
#
# TODO: Remove this machinery entirely and rely on the normal LD_PRELOAD approach that SMP utilizes.
#
# We use this to directly run ADP via `ddprof` when profiling is enabled, instead of the `LD_PRELOAD`-based approach SMP
# uses by default. This dates back to when ADP was a statically linked musl binary, which `LD_PRELOAD` could not
# instrument; ADP is now glibc-based/dynamically linked, but we're keeping this script as-is until we remove it
# entirely.

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
