# syntax=docker/dockerfile:1.24@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89
#
# Differential Antithesis harness image.
#
# The Datadog Agent lane wraps the project-wide Agent image built by Make from
# `.datadog-agent-version`. Do not pin an upstream Agent tag in this file.

ARG BUILD_IMAGE=ubuntu:24.04@sha256:c4a8d5503dfb2a3eb8ab5f807da5bc69a85730fb49b5cfca2330194ebcc41c7b
ARG APP_IMAGE=ubuntu:24.04@sha256:c4a8d5503dfb2a3eb8ab5f807da5bc69a85730fb49b5cfca2330194ebcc41c7b
ARG DD_AGENT_IMAGE=saluki-images/datadog-agent:testing-release

# ---------------------------------------------------------------------------
# Shared build environment: Rust toolchain + native build dependencies.
# ---------------------------------------------------------------------------
FROM ${BUILD_IMAGE} AS build-base
ENV DEBIAN_FRONTEND=noninteractive \
    NO_COLOR=1 \
    CARGO_TERM_COLOR=never
RUN apt-get update && \
    apt-get install --no-install-recommends -y \
        build-essential ca-certificates make cmake gcc g++ perl protobuf-compiler curl unzip rustup && \
    rm -rf /var/lib/apt/lists/*
RUN rustup set profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN --mount=type=bind,source=rust-toolchain.toml,target=/tmp/rust-toolchain.toml \
    cd /tmp && rustup show active-toolchain

# ---------------------------------------------------------------------------
# Build the instrumented Agent Data Plane.
# ---------------------------------------------------------------------------
FROM build-base AS adp-builder
ENV APP_FULL_NAME="Agent Data Plane" \
    APP_SHORT_NAME="agent-data-plane" \
    APP_IDENTIFIER="adp" \
    CARGO_PROFILE_RELEASE_LTO=off
WORKDIR /adp
COPY . /adp
RUN --mount=type=cache,target=/adp/target,id=antithesis-differential-adp-target \
    --mount=type=cache,target=/root/.cargo/registry,id=cargo-registry \
    --mount=type=cache,target=/root/.cargo/git,id=cargo-git \
    cargo build --release --package agent-data-plane --features antithesis \
        --target x86_64-unknown-linux-gnu \
        --config 'profile.release.panic="abort"' \
        --config 'target.x86_64-unknown-linux-gnu.rustflags=["--cfg","tokio_unstable","-Ccodegen-units=1","-Cpasses=sancov-module","-Cllvm-args=-sanitizer-coverage-level=3","-Cllvm-args=-sanitizer-coverage-trace-pc-guard","-Clink-args=-Wl,--build-id"]' && \
    cp /adp/target/x86_64-unknown-linux-gnu/release/agent-data-plane /usr/local/bin/agent-data-plane && \
    echo "Validating Antithesis instrumentation symbols..." && \
    nm /usr/local/bin/agent-data-plane | grep -q "antithesis_load_libvoidstar" && \
    nm /usr/local/bin/agent-data-plane | grep -q "sanitizer_cov_trace_pc_guard" && \
    echo "Instrumentation symbols present."

# ---------------------------------------------------------------------------
# Build the intake and differential test-command binaries, uninstrumented.
# ---------------------------------------------------------------------------
FROM build-base AS tools-builder
WORKDIR /tools
COPY . /tools
RUN --mount=type=cache,target=/tools/target,id=antithesis-differential-tools-target \
    --mount=type=cache,target=/root/.cargo/registry,id=cargo-registry \
    --mount=type=cache,target=/root/.cargo/git,id=cargo-git \
        cargo build --release --package antithesis-intake --bin antithesis-intake && \
    cargo build --release --package antithesis-scenario-differential --bins && \
    cargo build --release --package harness --bin first_sample_config --bin eventually_adp_alive && \
    cp /tools/target/release/antithesis-intake /usr/local/bin/antithesis-intake && \
    cp /tools/target/release/parallel_driver_send_dogstatsd_differential /usr/local/bin/parallel_driver_send_dogstatsd_differential && \
    cp /tools/target/release/eventually_differential_contexts /usr/local/bin/eventually_differential_contexts && \
    cp /tools/target/release/finally_differential_contexts /usr/local/bin/finally_differential_contexts && \
    cp /tools/target/release/eventually_adp_alive /usr/local/bin/eventually_adp_alive && \
    cp /tools/target/release/first_sample_config /usr/local/bin/first_sample_config

# ---------------------------------------------------------------------------
# Runtime: Agent Data Plane (SUT).
# ---------------------------------------------------------------------------
FROM ${APP_IMAGE} AS adp
ENV NO_COLOR=1
RUN apt-get update && \
    apt-get install --no-install-recommends -y ca-certificates openssl && \
    rm -rf /var/lib/apt/lists/*
COPY --from=adp-builder /usr/local/bin/agent-data-plane /usr/local/bin/agent-data-plane
RUN mkdir -p /symbols && ln -s /usr/local/bin/agent-data-plane /symbols/agent-data-plane
COPY --chmod=755 test/antithesis/scenarios/differential/adp/entrypoint.sh /entrypoint.sh
RUN mkdir -p /etc/datadog-agent
RUN openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=agent-data-plane" \
        -keyout /tmp/ipc_key.pem -out /tmp/ipc_cert.pem && \
    cat /tmp/ipc_cert.pem /tmp/ipc_key.pem > /etc/datadog-agent/ipc_cert.pem && \
    rm -f /tmp/ipc_cert.pem /tmp/ipc_key.pem && \
    touch /etc/datadog-agent/auth_token
ENTRYPOINT ["/entrypoint.sh"]
CMD ["run"]

# ---------------------------------------------------------------------------
# Runtime: Datadog Agent normative lane.
# ---------------------------------------------------------------------------
FROM ${DD_AGENT_IMAGE} AS datadog-agent
ENV NO_COLOR=1
COPY --chmod=755 test/antithesis/scenarios/differential/agent-config-init.sh /etc/cont-init.d/00-differential-agent-config.sh

# ---------------------------------------------------------------------------
# Runtime: antithesis-intake.
# ---------------------------------------------------------------------------
FROM ${APP_IMAGE} AS intake
ENV NO_COLOR=1
ENV DD_HOSTNAME=antithesis-differential
COPY --from=tools-builder /usr/local/bin/antithesis-intake /usr/local/bin/antithesis-intake
ENTRYPOINT ["/usr/local/bin/antithesis-intake"]

# ---------------------------------------------------------------------------
# Runtime: differential workload client.
# ---------------------------------------------------------------------------
FROM ${APP_IMAGE} AS workload-differential
ENV NO_COLOR=1
RUN test -d /usr/share/ca-certificates || ( \
    apt-get update && \
    apt-get install --no-install-recommends -y ca-certificates && \
    rm -rf /var/lib/apt/lists/* )
RUN mkdir -p /opt/antithesis/test/v1/main
COPY --chmod=755 test/antithesis/scenarios/differential/workload/setup-complete.sh /opt/antithesis/setup-complete.sh
COPY --from=tools-builder --chmod=755 /usr/local/bin/first_sample_config /opt/antithesis/test/v1/main/first_sample_config
COPY --from=tools-builder --chmod=755 /usr/local/bin/parallel_driver_send_dogstatsd_differential /opt/antithesis/test/v1/main/parallel_driver_send_dogstatsd_differential
COPY --from=tools-builder --chmod=755 /usr/local/bin/eventually_differential_contexts /opt/antithesis/test/v1/main/eventually_differential_contexts
COPY --from=tools-builder --chmod=755 /usr/local/bin/finally_differential_contexts /opt/antithesis/test/v1/main/finally_differential_contexts
COPY --from=tools-builder --chmod=755 /usr/local/bin/eventually_adp_alive /opt/antithesis/test/v1/main/eventually_adp_alive
COPY --chmod=755 test/antithesis/scenarios/differential/workload/entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
