# syntax=docker/dockerfile:1.24@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89
#
# Antithesis harness image for Agent Data Plane (ADP).
#
# Build context is the repository root. Three named targets:
#   - datadog-agent : converged Datadog Agent with our instrumented ADP embedded, data plane ON (the SUT)
#   - intake        : antithesis-intake mock Datadog intake + W-property assertions (dependency)
#   - workload      : DogStatsD driver + test templates + setup-complete (the client)
#
# ADP is built native x86_64-unknown-linux-gnu (glibc), so no musl cross-compile headers are needed.

ARG BUILD_IMAGE=ubuntu:26.04@sha256:b7f48194d4d8b763a478a621cdc81c27be222ba2206ca3ca6bc42b49685f3d9e
ARG APP_IMAGE=ubuntu:26.04@sha256:b7f48194d4d8b763a478a621cdc81c27be222ba2206ca3ca6bc42b49685f3d9e
# Project-pinned converged Datadog Agent image (Make-built from `.datadog-agent-version`).
# Do not pin an upstream Agent tag here.
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
# Build the antithesis-intake mock and the test-command binaries, uninstrumented.
# These are supporting harness components, not the SUT, so they need no coverage instrumentation.
# ---------------------------------------------------------------------------
FROM build-base AS tools-builder
WORKDIR /tools
COPY . /tools
RUN --mount=type=cache,target=/tools/target,id=antithesis-tools-target \
    --mount=type=cache,target=/root/.cargo/registry,id=cargo-registry \
    --mount=type=cache,target=/root/.cargo/git,id=cargo-git \
    cargo build --release --package antithesis-intake --bin antithesis-intake && \
    cargo build --release --package harness \
        --bin first_sample_config \
        --bin eventually_adp_alive && \
    cargo build --release --package antithesis-scenario-general --bins && \
    cp /tools/target/release/antithesis-intake /usr/local/bin/antithesis-intake && \
    cp /tools/target/release/parallel_driver_send_dogstatsd /usr/local/bin/parallel_driver_send_dogstatsd && \
    cp /tools/target/release/parallel_driver_sketchburst /usr/local/bin/parallel_driver_sketchburst && \
    cp /tools/target/release/parallel_driver_poll_stats /usr/local/bin/parallel_driver_poll_stats && \
    cp /tools/target/release/eventually_adp_alive /usr/local/bin/eventually_adp_alive && \
    cp /tools/target/release/first_sample_config /usr/local/bin/first_sample_config

# ---------------------------------------------------------------------------
# Runtime: converged Datadog Agent with our instrumented ADP embedded, data
# plane ON. The general scenario's single SUT, so the workload exercises the
# real Agent+ADP path rather than a standalone ADP.
# ---------------------------------------------------------------------------
FROM ${DD_AGENT_IMAGE} AS datadog-agent
ENV NO_COLOR=1
# Wait for and install the per-timeline sampled config for both the Core Agent and ADP.
COPY --chmod=755 test/antithesis/agent-config-init.sh /etc/cont-init.d/00-general-agent-config.sh
# Overlay our instrumented ADP over the release binary the converged image ships,
# so the embedded data plane carries Antithesis coverage. Expose its symbols.
COPY --from=adp-builder /usr/local/bin/agent-data-plane /opt/datadog-agent/embedded/bin/agent-data-plane
RUN mkdir -p /symbols && ln -s /opt/datadog-agent/embedded/bin/agent-data-plane /symbols/agent-data-plane
# Bring the container down on an ADP crash instead of respawning, so a SIGABRT is
# recorded as a fault rather than masked by an s6 restart loop.
COPY --chmod=755 test/antithesis/data-plane-finish /etc/services.d/data-plane/finish

# ---------------------------------------------------------------------------
# Runtime: antithesis-intake (mock Datadog intake + W-property assertions).
# ---------------------------------------------------------------------------
FROM ${APP_IMAGE} AS intake
ENV NO_COLOR=1
# W17 resolves each series' host resource against this hostname. Keep it in sync
# with the DD_HOSTNAME the datadog-agent service sets and the harness default.
ENV DD_HOSTNAME=antithesis-adp
COPY --from=tools-builder /usr/local/bin/antithesis-intake /usr/local/bin/antithesis-intake
ENTRYPOINT ["/usr/local/bin/antithesis-intake"]

# ---------------------------------------------------------------------------
# Fetch the Datadog Agent debug symbols. The release binary is stripped; the
# DWARF is published as a separate build-id-matched `-dbg` package, so Antithesis
# can symbolize the off-the-shelf Core Agent without touching the agent image.
# ---------------------------------------------------------------------------
FROM build-base AS agent-symbols
# Version bumped by .github/workflows/update-agent-version.yml alongside .datadog-agent-version.
ARG DD_AGENT_VERSION=7.81.1
ARG TARGETARCH=amd64
RUN curl -fsSL -o /tmp/agent-dbg.deb \
        "https://s3.amazonaws.com/apt.datadoghq.com/pool/d/da/datadog-agent-dbg_${DD_AGENT_VERSION}-1_${TARGETARCH}.deb" && \
    dpkg-deb --raw-extract /tmp/agent-dbg.deb /tmp/dbg && \
    mkdir -p /agent-symbols && \
    dbg="$(find /tmp/dbg -type f -name agent.dbg -path '*bin/agent/*')" && \
    [ "$(printf '%s\n' "$dbg" | grep -c .)" = "1" ] || { echo "expected exactly one agent.dbg, found: [$dbg]" >&2; exit 1; } && \
    cp "$dbg" /agent-symbols/agent.dbg

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
COPY --chmod=755 test/antithesis/scenarios/general/workload/setup-complete.sh /opt/antithesis/setup-complete.sh
COPY test/antithesis/scenarios/general/workload/test/ /opt/antithesis/test/
# Inject the compiled test-command binaries into the "main" test template.
COPY --from=tools-builder --chmod=755 /usr/local/bin/first_sample_config /opt/antithesis/test/v1/main/first_sample_config
COPY --from=tools-builder --chmod=755 /usr/local/bin/parallel_driver_send_dogstatsd /opt/antithesis/test/v1/main/parallel_driver_send_dogstatsd
COPY --from=tools-builder --chmod=755 /usr/local/bin/parallel_driver_sketchburst /opt/antithesis/test/v1/main/parallel_driver_sketchburst
COPY --from=tools-builder --chmod=755 /usr/local/bin/parallel_driver_poll_stats /opt/antithesis/test/v1/main/parallel_driver_poll_stats
COPY --from=tools-builder --chmod=755 /usr/local/bin/eventually_adp_alive /opt/antithesis/test/v1/main/eventually_adp_alive
# Datadog Agent debug symbols. Antithesis scans /symbols in every pushed image and
# matches by build-id, so this symbolizes the Core Agent running in the SUT.
COPY --from=agent-symbols /agent-symbols/agent.dbg /symbols/agent.dbg
COPY --chmod=755 test/antithesis/scenarios/general/workload/entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
