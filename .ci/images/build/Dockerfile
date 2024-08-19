FROM registry.ddbuild.io/images/mirror/ubuntu:14.04

ARG RUST_VERSION=1.80.0

# Install basic utilities and an updated compiler/binutils toolchain, which is necessary for compiling.
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential software-properties-common curl ca-certificates git gnupg2 cmake3 unzip && \
    add-apt-repository ppa:ubuntu-toolchain-r/test -y && \
    apt-get update && \
    apt-get install -y --no-install-recommends gcc-8 g++-8 binutils-2.26 && \
    update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-8 60 --slave /usr/bin/g++ g++ /usr/bin/g++-8 && \
    rm -rf /var/lib/apt/lists/*
ENV PATH="/usr/lib/binutils-2.26/bin:${PATH}"

# Install Go, which we need to build AWS-LC in FIPS-compliant mode.
ENV PATH /usr/local/go/bin:$PATH
RUN curl -s -L -o /tmp/go-1.23.0.tar.gz https://go.dev/dl/go1.23.0.linux-amd64.tar.gz
RUN echo "905a297f19ead44780548933e0ff1a1b86e8327bb459e92f9c0012569f76f5e3 /tmp/go-1.23.0.tar.gz" | sha256sum --check
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
    
COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
