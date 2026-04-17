FROM registry.ddbuild.io/docker:24.0.4-jammy

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC

RUN --mount=type=bind,source=.ci/configure-apt-mirror.sh,target=/tmp/configure-apt-mirror.sh \
    sh /tmp/configure-apt-mirror.sh

# Install basic utilities and an updated compiler/binutils toolchain, which is necessary for compiling.
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates awscli lsb-release git jq bc bzip2 python3 binutils \
      cmake ninja-build g++ && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

RUN apt-get update && \
    dd-package --bucket binaries-ddbuild-io-prod --package devtools/dd-package-dev --distribution "20.04"

# Install pr-commenter which we will need to post comments to Github.
COPY .ci/install-pr-commenter.sh /
RUN chmod +x /install-pr-commenter.sh && /install-pr-commenter.sh

# Build and install bloaty for binary size analysis.
COPY .ci/install-bloaty.sh /
RUN chmod +x /install-bloaty.sh && /install-bloaty.sh

COPY .ci/images/smp/entrypoint.sh /
RUN chmod +x /entrypoint.sh

ENTRYPOINT [ "/entrypoint.sh" ]
