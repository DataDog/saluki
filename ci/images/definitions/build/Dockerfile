# We base our build image on the Datadog Agent's own Linux build image. It is Ubuntu-based
# (`ubuntu:noble`) but ships crosstool-NG cross-toolchains under `/opt/toolchains/<arch>/bin` that
# target an old glibc (x86_64 -> glibc 2.17, aarch64 -> glibc 2.23). ADP is built against those
# toolchains (see docker/scripts/agent-data-plane/build/targets/*-unknown-linux-gnu.sh) so its glibc
# floor matches the Agent's supported-OS matrix by construction, and tracks it automatically over time.
ARG BASE_IMAGE=registry.ddbuild.io/ci/datadog-agent-buildimages/linux:v120029388-43933fee

# Create a helper image with all of our CI scripts that we'll bindmount on `RUN` commands as needed.
#
# This lets us avoid having to actually copy them into a layer that makes it into the final image.
FROM scratch AS ci-scripts
COPY --chmod=+x ci/images/scripts/*.sh /

# Create another small helper image with our Makefile and ADP's `Cargo.toml` in the right layout
# such that we can bindmount it and then run `make cargo-preinstall`.
FROM scratch AS preinstall-helper
COPY Makefile /tmp/Makefile
COPY bin/agent-data-plane/Cargo.toml /tmp/bin/agent-data-plane/Cargo.toml

FROM ${BASE_IMAGE}

ARG RUST_VERSION=1.94.0
ARG TARGETARCH

# The Agent build image runs as a non-root `dd` user by default; switch to root for all install steps.
USER root

# The Agent build image sets an opinionated Cargo/rustup layout (XDG-style CARGO_HOME/RUSTUP_HOME and a
# separate CARGO_INSTALL_ROOT). Reset it to the plain `/root/.cargo` layout our install scripts and
# Makefile assume, and which docker/Dockerfile.agent-data-plane's build caches are mounted at
# (`/root/.cargo/registry`, `/root/.cargo/git`) -- otherwise those cache mounts silently miss.
ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    CARGO_INSTALL_ROOT=/root/.cargo

# Install basic build tooling and dependencies. Most are already present on the Agent base (apt is
# idempotent); we keep the explicit list so the image's contract does not depend on the base's.
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential software-properties-common curl ca-certificates git gnupg2 \
      lsb-release make cmake unzip gcc g++ binutils jq bc bzip2 ninja-build python3 python3-pip python3-yaml && \
    /scripts/clean-temporary-caches.sh

# Install the Docker CLI directly, as well as the CI-specific credential helper.
# Also install kind (Kubernetes in Docker), used for kind-based correctness tests.
#
# We intentionally avoid basing our image off of the `docker` image because it's fat and bloated. ¯\_(ツ)_/¯
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-docker-cli.sh && \
    /scripts/install-kind.sh && \
    /scripts/clean-temporary-caches.sh

# Go (needed to build AWS-LC in FIPS mode) is provided by the Datadog Agent build image at
# /usr/local/go and is already on PATH, so we deliberately do not install our own.

# Install protoc.
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-protoc.sh

# Install vale (prose linter), used for documentation lint checks.
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-vale.sh

# Install Rust and common Cargo tooling that we depend on.
ENV PATH="/root/.cargo/bin:${PATH}"
RUN --mount=type=bind,from=ci-scripts,target=/scripts \
    /scripts/install-rust.sh && \
    /scripts/clean-temporary-caches.sh

# Pre-install the relevant Cargo tools we use in the build process.
RUN --mount=type=bind,source=Makefile,target=/tmp/Makefile \
    --mount=type=bind,source=bin/agent-data-plane/Cargo.toml,target=/tmp/bin/agent-data-plane/Cargo.toml \
    --mount=type=bind,from=ci-scripts,target=/scripts \
    CI=true make -C /tmp cargo-preinstall && \
    /scripts/clean-temporary-caches.sh

COPY --chmod=+x ci/images/definitions/build/entrypoint.sh /
ENTRYPOINT [ "/entrypoint.sh" ]
