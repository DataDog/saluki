ARG DD_AGENT_VERSION=latest-jmx
ARG DD_AGENT_IMAGE=datadog/agent:${DD_AGENT_VERSION}

FROM ${DD_AGENT_IMAGE} AS agent

FROM registry.ddbuild.io/images/mirror/ubuntu:24.04

ARG RUST_VERSION=1.90.0
ARG TARGETARCH

# Install the basics for building and compiling software projects.
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential software-properties-common curl ca-certificates git gnupg2 cmake unzip gcc g++ binutils jq && \
    apt-get clean

# Install Go, which we need to build AWS-LC in FIPS-compliant mode.
ENV PATH="/usr/local/go/bin:${PATH}"
RUN curl -s -L -o /tmp/go-1.23.0.tar.gz https://go.dev/dl/go1.23.0.linux-${TARGETARCH}.tar.gz
RUN tar -C /usr/local -xzf /tmp/go-1.23.0.tar.gz

# Install a number of packages/tools by hand, because the versions in Ubuntu 14.04 are too old or not available at all.
COPY .ci/install-protoc.sh /
RUN chmod +x /install-protoc.sh && /install-protoc.sh

COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh

RUN VAULT_VERSION=1.21.1 && \
    curl -fsSL "https://releases.hashicorp.com/vault/${VAULT_VERSION}/vault_${VAULT_VERSION}_linux_amd64.zip" -o vault.zip && \
    unzip vault.zip && \
    mv vault /usr/local/bin/vault && \
    rm vault.zip && \
    chmod +x /usr/local/bin/vault

# Install Rust and common Cargo tooling that we depend on.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup toolchain add nightly && \
    rustup component add clippy

# Pre-install the relevant Cargo tools we use in the build process.
COPY ./Makefile /
RUN make cargo-preinstall

# Import Python bits from Agent image.
# Do last in the pipeline to avoid polluting the builds of above protobuf/go etc from using these libs
COPY --from=agent /opt/datadog-agent/embedded/ /opt/datadog-agent/embedded/
ENV LD_LIBRARY_PATH="/opt/datadog-agent/embedded/lib:${LD_LIBRARY_PATH}"
ENV PATH="/opt/datadog-agent/embedded/bin:${PATH}"

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
