# Builder stage: compile bloaty from source. Its build-only deps (cmake, ninja-build, g++) stay here.
FROM registry.ddbuild.io/docker:24.0.4-jammy AS bloaty-builder

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl cmake ninja-build g++ bzip2 && \
    rm -rf /var/lib/apt/lists/*

COPY .ci/install-bloaty.sh /install-bloaty.sh
RUN chmod +x /install-bloaty.sh && /install-bloaty.sh

# Final stage: runtime-only dependencies. Build toolchain is not present here.
FROM registry.ddbuild.io/docker:24.0.4-jammy

ARG RUST_VERSION=1.93.0

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC

RUN --mount=type=bind,source=.ci/configure-apt-mirror.sh,target=/tmp/configure-apt-mirror.sh \
    sh /tmp/configure-apt-mirror.sh

# Runtime utilities for the benchmark/binary-size-analysis jobs. python3 + binutils are needed by
# analyze-binary-size.py (objcopy / strip). No C/C++ toolchain here — bloaty is built in the
# dedicated builder stage above and copied in below.
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates lsb-release git jq bc bzip2 python3 binutils && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

RUN apt-get update && \
    dd-package --bucket binaries-ddbuild-io-prod --package devtools/dd-package-dev --distribution "20.04" && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Install Rust which is required by the Binary Size Analysis job.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"

# Install pr-commenter for posting analysis reports back to GitHub.
COPY .ci/install-pr-commenter.sh /
RUN chmod +x /install-pr-commenter.sh && /install-pr-commenter.sh

# Install AWS CLI for retrieiving credentials and settings for the SMP image registry location.
COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh

# Copy just the compiled bloaty binary from the builder stage.
COPY --from=bloaty-builder /usr/local/bin/bloaty /usr/local/bin/bloaty

COPY .ci/images/smp/entrypoint.sh /
RUN chmod +x /entrypoint.sh

ENTRYPOINT [ "/entrypoint.sh" ]
