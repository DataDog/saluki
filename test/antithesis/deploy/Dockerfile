# syntax=docker/dockerfile:1.24@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89
#
# Antithesis harness image for Agent Data Plane (ADP).
#
# Build context is the repository root. Three named targets:
#   - adp      : agent-data-plane built WITH Antithesis coverage instrumentation + SDK (the SUT)
#   - intake   : datadog-intake mock Datadog intake (dependency)
#   - workload : DogStatsD driver + test templates + setup-complete (the client)
#
# ADP is built native x86_64-unknown-linux-gnu (glibc), so no musl cross-compile headers are needed.

ARG BUILD_IMAGE=ubuntu:24.04@sha256:c4a8d5503dfb2a3eb8ab5f807da5bc69a85730fb49b5cfca2330194ebcc41c7b
ARG APP_IMAGE=ubuntu:24.04@sha256:c4a8d5503dfb2a3eb8ab5f807da5bc69a85730fb49b5cfca2330194ebcc41c7b

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
# Pre-install the pinned toolchain (cache key is just rust-toolchain.toml).
RUN --mount=type=bind,source=rust-toolchain.toml,target=/tmp/rust-toolchain.toml \
    cd /tmp && rustup show active-toolchain

# ---------------------------------------------------------------------------
# Build the instrumented Agent Data Plane.
#
# Coverage instrumentation uses the modern Antithesis Rust flow
# (post-2026-05-22): the `antithesis-instrumentation` crate (referenced once in
# main.rs behind the `antithesis` feature) provides the runtime shim, and these
# RUSTFLAGS enable LLVM sancov coverage. `--build-id` is required for
# symbolization; the release profile sets `debug = true`, so the binary keeps
# DWARF for /symbols. LTO is disabled to keep sancov instrumentation
# predictable. `panic = "abort"` (antithesis build only) turns any ADP panic
# into SIGABRT, caught as a hard crash.
# ---------------------------------------------------------------------------
FROM build-base AS adp-builder
ENV APP_FULL_NAME="Agent Data Plane" \
    APP_SHORT_NAME="agent-data-plane" \
    APP_IDENTIFIER="adp" \
    CARGO_PROFILE_RELEASE_LTO=off
WORKDIR /adp
COPY . /adp
# The sancov RUSTFLAGS are passed via `--config target.<triple>.rustflags` with an explicit
# `--target`, NOT via the RUSTFLAGS env var. With an explicit target, Cargo builds host artifacts
# (build scripts, proc-macros) for the host and does NOT apply the target rustflags to them — so they
# are not instrumented and link cleanly. Using the RUSTFLAGS env var instead instruments build
# scripts too, which then fail to link (`undefined symbol: __sanitizer_cov_trace_pc_guard_init`).
RUN --mount=type=cache,target=/adp/target,id=antithesis-adp-target \
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
# Build the correctness tools (datadog-intake) and the test-command binaries, uninstrumented.
# These are supporting harness components, not the SUT, so they need no coverage instrumentation.
# ---------------------------------------------------------------------------
FROM build-base AS tools-builder
WORKDIR /tools
COPY . /tools
RUN --mount=type=cache,target=/tools/target,id=antithesis-tools-target \
    --mount=type=cache,target=/root/.cargo/registry,id=cargo-registry \
    --mount=type=cache,target=/root/.cargo/git,id=cargo-git \
    cargo build --release \
        --bin datadog-intake \
        --bin parallel_driver_send_dogstatsd --bin parallel_driver_sketchburst \
        --bin finally_verify_delivery --bin eventually_adp_alive \
        --bin first_sample_config && \
    cp /tools/target/release/datadog-intake /usr/local/bin/datadog-intake && \
    cp /tools/target/release/parallel_driver_send_dogstatsd /usr/local/bin/parallel_driver_send_dogstatsd && \
    cp /tools/target/release/parallel_driver_sketchburst /usr/local/bin/parallel_driver_sketchburst && \
    cp /tools/target/release/finally_verify_delivery /usr/local/bin/finally_verify_delivery && \
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
# Expose DWARF/build-id symbols to Antithesis for symbolization (one-hop symlink to the unstripped binary).
RUN mkdir -p /symbols && ln -s /usr/local/bin/agent-data-plane /symbols/agent-data-plane
# main.rs requires a config file at the default path. Ship a minimal standalone config as a
# fallback. The boot wrapper overwrites it with the per-timeline config that first_sample_config
# samples onto the shared `agent-config` volume.
COPY test/antithesis/deploy/adp/datadog.yaml /etc/datadog-agent/datadog.yaml
# Boot wrapper waits for the config sentinel, copies the config into place, then execs ADP.
COPY --chmod=755 test/antithesis/deploy/adp/entrypoint.sh /entrypoint.sh
# ADP's control-plane secure API requires an IPC TLS cert (a single PEM holding both certificate and
# private key) that the Core Agent normally generates. In standalone mode there is no Core Agent, so
# generate a self-signed cert+key. An empty auth_token satisfies the IPC auth config at startup.
RUN openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=agent-data-plane" \
        -keyout /tmp/ipc_key.pem -out /tmp/ipc_cert.pem && \
    cat /tmp/ipc_cert.pem /tmp/ipc_key.pem > /etc/datadog-agent/ipc_cert.pem && \
    rm -f /tmp/ipc_cert.pem /tmp/ipc_key.pem && \
    touch /etc/datadog-agent/auth_token
ENTRYPOINT ["/entrypoint.sh"]
CMD ["run"]

# ---------------------------------------------------------------------------
# Runtime: datadog-intake (mock Datadog intake dependency).
# ---------------------------------------------------------------------------
FROM ${APP_IMAGE} AS intake
ENV NO_COLOR=1
COPY --from=tools-builder /usr/local/bin/datadog-intake /usr/local/bin/datadog-intake
ENTRYPOINT ["/usr/local/bin/datadog-intake"]

# ---------------------------------------------------------------------------
# Runtime: workload client (DogStatsD driver + test templates).
# ---------------------------------------------------------------------------
FROM ${APP_IMAGE} AS workload
ENV NO_COLOR=1
RUN test -d /usr/share/ca-certificates || ( \
    apt-get update && \
    apt-get install --no-install-recommends -y ca-certificates && \
    rm -rf /var/lib/apt/lists/* )
# Antithesis setup-complete helper and test templates (helper files + the "main" template dir).
COPY --chmod=755 test/antithesis/deploy/workload/setup-complete.sh /opt/antithesis/setup-complete.sh
COPY test/antithesis/deploy/workload/test/ /opt/antithesis/test/
# Inject the compiled test-command binaries into the "main" test template.
COPY --from=tools-builder --chmod=755 /usr/local/bin/first_sample_config /opt/antithesis/test/v1/main/first_sample_config
COPY --from=tools-builder --chmod=755 /usr/local/bin/parallel_driver_send_dogstatsd /opt/antithesis/test/v1/main/parallel_driver_send_dogstatsd
COPY --from=tools-builder --chmod=755 /usr/local/bin/parallel_driver_sketchburst /opt/antithesis/test/v1/main/parallel_driver_sketchburst
COPY --from=tools-builder --chmod=755 /usr/local/bin/finally_verify_delivery /opt/antithesis/test/v1/main/finally_verify_delivery
COPY --from=tools-builder --chmod=755 /usr/local/bin/eventually_adp_alive /opt/antithesis/test/v1/main/eventually_adp_alive
COPY --chmod=755 test/antithesis/deploy/workload/entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
