ARG DD_AGENT_VERSION=latest-jmx
ARG DD_AGENT_IMAGE=datadog/agent:${DD_AGENT_VERSION}

FROM ${DD_AGENT_IMAGE} AS agent

FROM registry.ddbuild.io/images/docker:27.3.1

ARG RUST_VERSION=1.90.0
ARG TARGETARCH

# Install the basics for building and compiling software projects.
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential software-properties-common curl ca-certificates git gnupg2 lsb-release make cmake unzip gcc g++ binutils jq bc bzip2 ninja-build && \
    apt-get clean

# Install Go, which we need to build AWS-LC in FIPS-compliant mode.
ENV PATH="/usr/local/go/bin:${PATH}"
RUN curl -s -L -o /tmp/go-1.23.0.tar.gz https://go.dev/dl/go1.23.0.linux-${TARGETARCH}.tar.gz
RUN tar -C /usr/local -xzf /tmp/go-1.23.0.tar.gz

# Install a number of packages/tools by hand, because the versions in Ubuntu are too old, or just aren't available
# from the normal package manager repositories.
RUN curl -L https://s3.amazonaws.com/dd-package-public/dd-package-$(dpkg --print-architecture).deb -o dd-package.deb && \
    dpkg -i dd-package.deb && \
    rm dd-package.deb && \
    apt-get update && \
    dd-package --bucket binaries-ddbuild-io-prod --package devtools/dd-package-dev --distribution "20.04"

COPY .ci/install-pr-commenter.sh /
RUN chmod +x /install-pr-commenter.sh && /install-pr-commenter.sh

COPY .ci/install-bloaty.sh /
RUN chmod +x /install-bloaty.sh && /install-bloaty.sh

COPY .ci/install-protoc.sh /
RUN chmod +x /install-protoc.sh && /install-protoc.sh

COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh

COPY .ci/install-vault.sh /
RUN chmod +x /install-vault.sh && /install-vault.sh

# Install Rust and common Cargo tooling that we depend on.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup toolchain add nightly && \
    rustup component add clippy

# Pre-install the relevant Cargo tools we use in the build process.
WORKDIR /tmp
COPY ./Makefile /tmp
COPY ./bin/agent-data-plane/Cargo.toml /tmp/bin/agent-data-plane/Cargo.toml
RUN CI=true make cargo-preinstall

# Import Python bits from Agent image.
# Do last in the pipeline to avoid polluting the builds of above protobuf/go etc from using these libs
COPY --from=agent /opt/datadog-agent/embedded/ /opt/datadog-agent/embedded/
ENV LD_LIBRARY_PATH="/opt/datadog-agent/embedded/lib:${LD_LIBRARY_PATH}"
ENV PATH="/opt/datadog-agent/embedded/bin:${PATH}"

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
