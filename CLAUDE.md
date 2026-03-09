# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Saluki is a Rust toolkit for building telemetry data planes. The primary binary is `agent-data-plane` (ADP), which provides a production-grade DogStatsD pipeline and an OTLP pipeline for the Datadog Agent.

**Rust toolchain:** 1.93.0 (pinned in `rust-toolchain.toml`). Formatting requires nightly.

## Repository Structure

```
bin/
  agent-data-plane/     # Primary ADP binary
  correctness/          # Correctness test binaries (airlock, ground-truth, millstone, etc.)
lib/
  saluki-core/          # Topology construction, component traits, data model, pooling, runtime
  saluki-components/    # Concrete implementations of sources, transforms, destinations, etc.
  saluki-io/            # I/O primitives: HTTP client/server, codecs, network transports
  saluki-context/       # Context/tag resolution for metrics
  saluki-config/        # Configuration loading (Figment-based)
  saluki-app/           # Application bootstrap helpers
  saluki-env/           # Environment/host metadata detection
  saluki-error/         # Error handling utilities
  saluki-health/        # Health checking
  saluki-metrics/       # Internal metrics instrumentation
  saluki-api/           # API server primitives
  saluki-tls/           # TLS helpers (rustls)
  saluki-common/        # Shared utility types
  saluki-metadata/      # Metadata handling
  stringtheory/         # Optimized string types (interning, pooling)
  ddsketch/             # DDSketch histogram implementation
  memory-accounting/    # Memory usage tracking
  ottl/                 # OpenTelemetry Transformation Language parser
  protos/               # Generated protobuf code (datadog, containerd, otlp)
```

## Build Commands

All common operations go through `make`. Use `make help` to list targets.

```bash
# Build
make build-adp                  # Debug build (profile: devel)
make build-adp-release          # Release build

# Run locally (requires Datadog Agent running + DD_API_KEY set)
make run-adp                    # Debug, with Agent tagging
make run-adp-standalone         # Debug, standalone mode (no Agent required)
make run-adp-standalone-release # Release, standalone mode

# Direct cargo build
cargo build --profile devel --package agent-data-plane
```

**Prerequisites:** `cargo`, `jq`, `protoc` (verified by `make check-rust-build-tools`).

## Testing

```bash
# Unit tests (preferred - uses cargo-nextest)
make test
cargo nextest run --lib -E 'not test(/property_test_*/)'

# Run a single test
cargo nextest run --lib -p <crate-name> <test_name>
cargo nextest run --lib -p saluki-io codec_tests

# Property tests
make test-property
cargo nextest run --lib --release -E 'test(/property_test_*/)'

# Doctests
make test-docs
cargo test --workspace --exclude containerd-protos --exclude datadog-protos --exclude otlp-protos --doc

# Miri (unsafe memory safety, nightly-2025-06-16)
make test-miri

# Loom (concurrency correctness)
make test-loom

# Correctness / integration tests (require Docker)
make test-correctness
make test-integration
```

## Linting and Formatting

```bash
# Format (requires nightly Rust for rustfmt)
make fmt
cargo +nightly fmt

# Clippy (must pass with -D warnings)
make check-clippy
cargo clippy --all-targets --workspace -- -D warnings

# All checks
make check-all          # fmt + clippy + features + deny + licenses
make check-fmt          # rustfmt + cargo-sort
make check-deny         # dependency advisories/licenses
make check-features     # feature flag compatibility matrix
make check-unused-deps  # cargo-machete

# Full dev cycle (format → lint → test)
make fast-edit-test
```

**Note:** After adding/changing dependencies, run `make sync-licenses` to update the third-party license file, or `check-licenses` will fail in CI.

## Architecture: Topology-Based Pipeline

Saluki's core abstraction is a **topology** — a directed acyclic graph of **components** connected via async channels. Components are registered in a `TopologyBlueprint`, validated at build time, and spawned as Tokio tasks.

### Component Types (in pipeline order)

| Type | Role | Trait |
|------|------|-------|
| **Source** | Ingests data into topology | `SourceBuilder` / `Source` |
| **Relay** | Receives raw payloads, routes without decoding | `RelayBuilder` |
| **Decoder** | Parses raw bytes into `Event`s | `DecoderBuilder` |
| **Transform** | Processes/aggregates/filters events | `TransformBuilder` |
| **Encoder** | Serializes events into payloads | `EncoderBuilder` |
| **Forwarder** | Sends payloads to external systems | `ForwarderBuilder` |
| **Destination** | Combined encode+forward (simpler path) | `DestinationBuilder` |

### Data Flow

- **Event-based path:** `Source → Transform → Encoder → Forwarder`
- **Payload-based path:** `Source/Relay → Decoder → ... → Encoder → Forwarder`
- Components communicate via `EventsBuffer` / `PayloadsBuffer` channels (bounded, async)
- `Consumer` and `Dispatcher` in `saluki-core::topology::interconnect` are the channel endpoints

### Key Crates

- **`saluki-core`**: `TopologyBlueprint`, `RunningTopology`, all component traits and the `Event` data model (metrics, logs, traces, service checks)
- **`saluki-components`**: All concrete component implementations — DogStatsD source, OTLP source, Aggregate transform, Datadog metrics encoder/forwarder, etc.
- **`saluki-io`**: Low-level I/O — HTTP client (hyper + rustls), codec framework, Unix socket/UDP transport, framing
- **`saluki-context`**: Tag/context resolution and interning for metrics

### Shutdown

Shutdown is **ordered**: the topology signals sources first → sources drain and exit → downstream components (transforms, destinations) complete when their input channels close. No explicit signal is sent to non-source components.

### Memory Accounting

All components register with a `ComponentRegistry` (`memory-accounting` crate) to track heap usage. The `Track` allocator wrapper is used on Linux with jemalloc.

## Coding Conventions

- Error handling uses `snafu` with `#[snafu(context(suffix(false)))]` — error context types are named after the error variant, not suffixed with `Snafu`.
- Use `stringtheory` types (`MetaString`, interned strings) for metric names and tag values — avoid allocating plain `String` in hot paths.
- All workspace dependencies are defined in the root `Cargo.toml` `[workspace.dependencies]` section and referenced with `{ workspace = true }` — do not specify versions in individual crates.
- New dependencies must be added to `Cargo.toml` with `default-features = false` and only the required features enabled.
- Clippy `too-many-arguments-threshold` is set to 8 (see `clippy.toml`).
- Property tests are named with the `property_test_*` prefix to distinguish them from unit tests.
