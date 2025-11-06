FROM registry.ddbuild.io/images/mirror/alpine:3.22.1

ARG RUST_VERSION=1.90.0
ARG BUILDCACHE_VERSION=0.31.5

# Install some basic dependencies required for building Agent Data Plane / Saluki.
RUN apk update && \
    apk add --no-cache rustup musl-dev linux-headers make cmake mold gcc libgcc perl go protobuf

# Install rustup and install the pinned Rust version, setting it as our default toolchain.
RUN rustup-init -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"

# Build and install buildcache.
RUN curl -s -L -o buildcache.tar.gz https://gitlab.com/bits-n-bites/buildcache/-/archive/v${BUILDCACHE_VERSION}/buildcache-v${BUILDCACHE_VERSION}.tar.gz \
    && tar -xzf buildcache.tar.gz \
    && cd buildcache-v${BUILDCACHE_VERSION} \
    && mkdir build \
    && cd build \
    && cmake -DCMAKE_BUILD_TYPE=Release ../src \
    && cmake --build . --config Release \
    && cp buildcache /usr/local/bin \
    && rm -rf buildcache-v${BUILDCACHE_VERSION} buildcache.tar.gz

COPY .ci/images/general/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
