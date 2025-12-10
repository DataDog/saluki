FROM registry.ddbuild.io/images/mirror/alpine:3.20

ARG RUST_VERSION=1.90.0
ARG TARGETARCH

# Install Rust and other build dependencies.
RUN apk update && \
    apk add --no-cache rustup musl-dev linux-headers make cmake mold gcc libgcc perl go python3 py3-pip protobuf bloaty aws-cli curl && \
   rustup-init -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"

# Manually download/install Vault since there's no existing package for it in Alpine.
COPY .ci/install-vault.sh /
RUN chmod +x /install-vault.sh && /install-vault.sh

# Install the relevant toolchains and Cargo-based tools that we use in the build process.
WORKDIR /tmp
COPY ./Makefile /tmp
COPY ./bin/agent-data-plane/Cargo.toml /tmp/bin/agent-data-plane/Cargo.toml
RUN rustup toolchain add nightly && rustup component add clippy
RUN CI=true make cargo-preinstall

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
