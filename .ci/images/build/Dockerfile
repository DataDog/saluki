FROM registry.ddbuild.io/images/mirror/ubuntu:14.04

ARG RUST_VERSION=1.81.0
ARG TARGETARCH

# Install basic utilities and an updated compiler/binutils toolchain, which is necessary for compiling.
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential software-properties-common curl ca-certificates git gnupg2 cmake3 unzip && \
    add-apt-repository ppa:ubuntu-toolchain-r/test -y && \
    apt-get update && \
    apt-get install -y --no-install-recommends gcc-8 g++-8 binutils-2.26 && \
    update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-8 60 --slave /usr/bin/g++ g++ /usr/bin/g++-8 && \
    apt-get clean
ENV PATH="/usr/lib/binutils-2.26/bin:${PATH}"

# Install Go, which we need to build AWS-LC in FIPS-compliant mode.
ENV PATH="/usr/local/go/bin:${PATH}"
RUN curl -s -L -o /tmp/go-1.23.0.tar.gz https://go.dev/dl/go1.23.0.linux-${TARGETARCH}.tar.gz
RUN tar -C /usr/local -xzf /tmp/go-1.23.0.tar.gz

# Install Protocol Buffers compiler by hand, since Ubuntu 14.04 does not have a recent enough version.
COPY .ci/install-protoc.sh /
RUN chmod +x /install-protoc.sh && /install-protoc.sh

# Install awscli by hand, for similar reasons to the Protocol Buffers compiler.
COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh

# Install Rust and common Cargo tooling that we depend on.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup toolchain add nightly && \
    rustup component add clippy

# Pre-install the relevant Cargo tools we use in the build process.
COPY ./Makefile /
RUN make cargo-preinstall

# Install mold, an alternative linker that is drastically faster than GNU ld. This is not strictly necessary, but it
# helps speed up builds in CI which is important for reducing iteration times / resource consumption.
RUN export ARCH=$(echo "${TARGETARCH}" | sed s/amd64/x86_64/g | sed s/arm64/aarch64/g) && \
    curl -s -L -o /tmp/mold-2.36.0.tar.gz https://github.com/rui314/mold/releases/download/v2.36.0/mold-2.36.0-${ARCH}-linux.tar.gz && \
    tar -C /tmp -xzf /tmp/mold-2.36.0.tar.gz && \
    cp -a /tmp/mold-2.36.0-${ARCH}-linux/. /usr/local/ && \
    rm -rf /tmp/mold-2.36.0-${ARCH}-linux

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
