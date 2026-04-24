ARG BASE_IMAGE=registry.ddbuild.io/images/base/gbi-ubuntu_2204:release

# Create a helper image with all of our CI scripts that we'll bindmount on `RUN` commands as needed.
#
# This lets us avoid having to actually copy them into a layer that makes it into the final image.
FROM scratch AS ci-scripts
COPY --chmod=+x .ci/*.sh /

# Create another small helper image with our Makefile and ADP's `Cargo.toml` in the right layout
# such that we can bindmount it and then run `make cargo-preinstall`.
FROM scratch AS preinstall-helper
COPY Makefile /tmp/Makefile
COPY bin/agent-data-plane/Cargo.toml /tmp/bin/agent-data-plane/Cargo.toml

FROM ${BASE_IMAGE}

ARG RUST_VERSION=1.93.0
ARG TARGETARCH

# Base image defaults to the unprivileged `dog` user; switch to root for all install steps.
USER root

# Install basic build tooling and dependencies.
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential software-properties-common curl ca-certificates git gnupg2 \
      lsb-release make cmake unzip gcc g++ binutils jq bc bzip2 ninja-build python3 python3-pip musl-dev musl-tools && \
    /scripts/clean-temporary-caches.sh

# Install the Docker CLI directly, as well as the CI-specific credential helper.
#
# We intentionally avoid basing our image off of the `docker` image because it's fat and bloated. ¯\_(ツ)_/¯
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-docker-cli.sh && \
    /scripts/clean-temporary-caches.sh

# Install Go, which we need to build AWS-LC in FIPS-compliant mode.
ENV PATH="/usr/local/go/bin:${PATH}"
RUN curl -s -L -o /tmp/go-1.23.0.tar.gz https://go.dev/dl/go1.23.0.linux-${TARGETARCH}.tar.gz && \
    tar -C /usr/local -xzf /tmp/go-1.23.0.tar.gz && \
    rm -f /tmp/go-1.23.0.tar.gz

# Install protoc.
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-protoc.sh

# Install Rust and common Cargo tooling that we depend on.
ENV PATH="/root/.cargo/bin:${PATH}"
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-rust.sh

# Pre-install the relevant Cargo tools we use in the build process.
RUN --mount=type=bind,source=Makefile,target=/tmp/Makefile \
    --mount=type=bind,source=bin/agent-data-plane/Cargo.toml,target=/tmp/bin/agent-data-plane/Cargo.toml \
    --mount=type=bind,from=ci-scripts,target=/scripts \
    CI=true make -C /tmp cargo-preinstall && \
    /scripts/clean-temporary-caches.sh

COPY --chmod=+x .ci/images/build/entrypoint.sh /
ENTRYPOINT [ "/entrypoint.sh" ]
