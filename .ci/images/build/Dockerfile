FROM registry.ddbuild.io/images/mirror/alpine:3.20

ARG RUST_VERSION=1.90.0
ARG BUILDCACHE_VERSION=0.31.5
ARG SCCACHE_VERSION=0.12.0

# Install some basic dependencies required for building Agent Data Plane / Saluki.
RUN apk update && \
    apk add --no-cache curl jq rustup make cmake gcc g++ perl go protobuf aws-cli \
    musl-dev linux-headers libgcc openssl openssl-dev openssl-libs-static

# Install rustup and install the pinned Rust version, setting it as our default toolchain.
RUN rustup-init -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"

# Build and install buildcache.
RUN curl -s -L -o /tmp/buildcache.tar.gz https://gitlab.com/bits-n-bites/buildcache/-/archive/v${BUILDCACHE_VERSION}/buildcache-v${BUILDCACHE_VERSION}.tar.gz \
    && tar -C /tmp -xzf /tmp/buildcache.tar.gz \
    && cd /tmp/buildcache-v${BUILDCACHE_VERSION} \
    && mkdir build \
    && cd build \
    && cmake -DCMAKE_BUILD_TYPE=Release -DCMAKE_CXX_FLAGS="-include cstdint" ../src \
    && cmake --build . --config Release \
    && cp buildcache /usr/local/bin \
    && cd ~ \
    && rm -rf /tmp/buildcache-v${BUILDCACHE_VERSION} /tmp/buildcache.tar.gz

# Build and install sccache.
RUN curl -s -L -o /tmp/sccache.tar.gz https://github.com/mozilla/sccache/archive/refs/tags/v${SCCACHE_VERSION}.tar.gz \
    && tar -C /tmp -xzf /tmp/sccache.tar.gz \
    && cd /tmp/sccache-${SCCACHE_VERSION} \
    && cargo build --release \
    && cp target/release/sccache /usr/local/bin \
    && cd ~ \
    && rm -rf /tmp/sccache-${SCCACHE_VERSION} /tmp/sccache.tar.gz

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
