# syntax=docker/dockerfile:1.28
#
# A slim build base for the SMP baseline and comparison image builds (see `.gitlab/benchmark.yml`).
#
# The `build-ci` image is about 2.8 GB compressed. It carries tooling for every job: cross toolchains for each platform,
# a nightly toolchain, Python, the Docker CLI, and more. An SMP job builds the Linux x86_64 binary only, and pulling and
# extracting all of `build-ci` takes about 105 seconds of each job. This image keeps only the parts of `build-ci` that
# the ADP build uses, on a plain Ubuntu base: the pinned Rust toolchain, the crosstool-NG toolchain that sets the glibc
# floor, and `protoc`. The SMP jobs build it on demand and tag it with a hash of its inputs, so a job builds it only when
# this file, the base image, or `build-ci` changes.
#
# The binary must stay the same as a build on `build-ci`. Thus we copy the toolchains from `build-ci` and do not
# install them again, and we set the same environment that the build reads.
ARG BASE_IMAGE=ubuntu:24.04
ARG TOOLCHAIN_IMAGE=registry.ddbuild.io/saluki/build-ci:latest
# The crosstool-NG toolchain directory under /opt/toolchains: `x86_64` or `aarch64`.
ARG TOOLCHAIN_ARCH=x86_64

FROM ${TOOLCHAIN_IMAGE} AS toolchain-src

FROM ${BASE_IMAGE}

ARG TOOLCHAIN_ARCH

# The same Cargo and rustup layout as `build-ci`. `PKG_CONFIG_LIBDIR` is empty there (it comes from the Datadog Agent
# build image), so pkg-config finds no system libraries. We keep that, so that no build script links a library from
# this image.
ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    CARGO_INSTALL_ROOT=/root/.cargo \
    PKG_CONFIG_LIBDIR= \
    PATH="/root/.cargo/bin:/opt/toolchains/${TOOLCHAIN_ARCH}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

# The host C toolchain (for build scripts and procedural macros), and the tools that native dependencies use to build.
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential ca-certificates cmake linux-libc-dev make perl && \
    rm -rf /var/lib/apt/lists/*

# The crosstool-NG toolchain that targets the old glibc. See
# docker/scripts/agent-data-plane/build/targets/x86_64-unknown-linux-gnu.sh.
COPY --from=toolchain-src /opt/toolchains/${TOOLCHAIN_ARCH} /opt/toolchains/${TOOLCHAIN_ARCH}

# The pinned Rust toolchain and the rustup proxies. We do not copy the nightly toolchain: it holds only the nightly
# `rustfmt`, which the ADP build does not use.
COPY --from=toolchain-src --exclude=toolchains/nightly-* /root/.rustup /root/.rustup
COPY --from=toolchain-src /root/.cargo/bin /root/.cargo/bin

# protoc, with the well-known types. The generated code depends on the protoc version, so we use the same binary.
COPY --from=toolchain-src /usr/bin/protoc /usr/bin/protoc
COPY --from=toolchain-src /usr/include/google /usr/include/google
