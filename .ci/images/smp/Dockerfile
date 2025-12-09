FROM registry.ddbuild.io/docker:24.0.4-jammy

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC

# Install basic utilities and an updated compiler/binutils toolchain, which is necessary for compiling.
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates awscli lsb-release git jq bc bzip2 && \
    apt-get clean

RUN curl -L https://s3.amazonaws.com/dd-package-public/dd-package-$(dpkg --print-architecture).deb -o dd-package.deb && \
    dpkg -i dd-package.deb && \
    rm dd-package.deb && \
    apt-get update && \
    dd-package --bucket binaries-ddbuild-io-prod --package devtools/dd-package-dev --distribution "20.04"

# Install pr-commenter which we will need to post comments to Github.
COPY .ci/images/smp/install-pr-commenter.sh /
RUN chmod +x /install-pr-commenter.sh && /install-pr-commenter.sh

# Build and install bloaty for binary size analysis.
COPY .ci/images/smp/install-bloaty.sh /
RUN chmod +x /install-bloaty.sh && /install-bloaty.sh

COPY .ci/images/smp/entrypoint.sh /
RUN chmod +x /entrypoint.sh

ENTRYPOINT [ "/entrypoint.sh" ]
