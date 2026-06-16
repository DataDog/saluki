#!/usr/bin/env sh
#
# Installs the `buildcache` binary from the optional buildcache image, if one was provided.
#
# The Dockerfile bind-mounts the buildcache image at /mnt/bcsrc. When no image is provided (the
# default `scratch`), there is no /mnt/bcsrc/buildcache and this is a no-op -- the build then runs
# without a compiler cache. When present, 10-build-adp.sh picks it up as the rustc wrapper.

set -eu

if [ -f /mnt/bcsrc/buildcache ]; then
    install -m 0755 /mnt/bcsrc/buildcache /usr/local/bin/buildcache
    /usr/local/bin/buildcache --version
fi
