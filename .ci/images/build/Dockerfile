FROM registry.ddbuild.io/images/base/gbi-ubuntu_2204:release

ARG RUST_VERSION=1.93.0
ARG TARGETARCH

# Base image defaults to the unprivileged `dog` user; switch to root for all install steps.
USER root

# Install build basics plus docker CLI only. The DinD runner provides the daemon at /var/run/docker.sock,
# so we don't ship dockerd, containerd, buildx, or the full credential-helper bundle.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential software-properties-common curl ca-certificates git gnupg2 \
      lsb-release make cmake unzip gcc g++ binutils jq bc bzip2 ninja-build python3 python3-pip && \
    mkdir -p /etc/apt/keyrings && \
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg && \
    echo "deb [arch=${TARGETARCH} signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu jammy stable" > /etc/apt/sources.list.d/docker.list && \
    apt-get update && \
    apt-get install -y --no-install-recommends docker-ce-cli=5:27.3.1-1~ubuntu.22.04~jammy && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Copy the registry.ddbuild.io credential helper from the upstream docker image so `docker pull`
# against registry.ddbuild.io continues to work. ECR/GCR helpers are intentionally omitted —
# jobs that pull from those registries perform explicit `docker login` beforehand.
COPY --from=registry.ddbuild.io/images/docker:27.3.1 /usr/bin/docker-credential-ci /usr/bin/docker-credential-ci

# Minimal docker client config: only the ci helper for registry.ddbuild.io.
RUN mkdir -p /root/.docker && \
    printf '{\n    "credHelpers": {\n        "registry-staging.ddbuild.io": "ci",\n        "registry.ddbuild.io": "ci"\n    }\n}\n' > /root/.docker/config.json

# Install Go, which we need to build AWS-LC in FIPS-compliant mode.
ENV PATH="/usr/local/go/bin:${PATH}"
RUN curl -s -L -o /tmp/go-1.23.0.tar.gz https://go.dev/dl/go1.23.0.linux-${TARGETARCH}.tar.gz && \
    tar -C /usr/local -xzf /tmp/go-1.23.0.tar.gz && \
    rm -f /tmp/go-1.23.0.tar.gz

COPY .ci/install-protoc.sh /
RUN chmod +x /install-protoc.sh && /install-protoc.sh

# Install Rust and common Cargo tooling that we depend on. Nightly is installed with a minimal profile
# since we only use it to run rustfmt with Edition 2024.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup component add clippy && \
    rustup toolchain install nightly --profile minimal --component rustfmt

# Pre-install the relevant Cargo tools we use in the build process. Drop the Cargo registry cache
# immediately after, since the tools themselves live in /root/.cargo/bin and jobs repopulate the
# registry from Cargo.lock on first build.
WORKDIR /tmp
COPY ./Makefile /tmp
COPY ./bin/agent-data-plane/Cargo.toml /tmp/bin/agent-data-plane/Cargo.toml
RUN CI=true make cargo-preinstall && \
    rm -rf /root/.cargo/registry /tmp/Makefile /tmp/bin

# Final sweep: remove anything leftover in /tmp, apt metadata, docs/man pages, and ephemeral caches.
# The nightly toolchain's rust-std (in lib/rustlib/*/lib) is only used when compiling Rust code;
# since we only use nightly to run rustfmt, we can drop it. rustfmt itself still links dynamically
# against libLLVM / librustc_driver in lib/, so only the rustlib subtree is safe to remove.
RUN find /tmp -mindepth 1 -delete && \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb && \
    rm -rf /root/.cache && \
    rm -rf /root/.cargo/registry && \
    rm -rf /root/.rustup/toolchains/nightly-*/lib/rustlib/*/lib && \
    rm -rf /usr/share/doc/* /usr/share/man/* /usr/share/locale/*

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
