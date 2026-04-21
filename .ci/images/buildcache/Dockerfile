# Builder stage: compile buildcache from source with static linking (OpenSSL + libstdc++/libgcc_s)
# so the resulting binary has libc as its only remaining runtime dependency and is safe to drop
# into a `FROM scratch` final stage for consumption as a `COPY --from=` source in other builds.
FROM registry.ddbuild.io/docker:24.0.4-jammy AS builder

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl git cmake ninja-build g++ libssl-dev && \
    rm -rf /var/lib/apt/lists/*

COPY .ci/install-buildcache.sh /install-buildcache.sh
RUN chmod +x /install-buildcache.sh && BUILDCACHE_STATIC=1 /install-buildcache.sh

# Final stage: scratch base holding only the buildcache binary. This image is not meant to be
# run directly — its sole purpose is to serve as a `COPY --from=${SALUKI_BUILDCACHE_CI_IMAGE}`
# source in other Dockerfiles (e.g. docker/Dockerfile.agent-data-plane) so that cargo can use
# buildcache as RUSTC_WRAPPER during the build.
FROM scratch
COPY --from=builder /usr/local/bin/buildcache /buildcache
