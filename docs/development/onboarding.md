# Saluki Onboarding Guide

This guide covers what Saluki does, how the code is structured, how to build and test it, and the domain vocabulary you need to navigate the codebase confidently.

---

## 1. What Is Saluki?

**Saluki** is a Rust toolkit for building **telemetry data planes** — systems that ingest, process, and forward observability data (metrics, logs, traces). It is the foundation for **Agent Data Plane (ADP)**, the next-generation data plane inside the Datadog Agent.

Key properties:
- High-performance, async-first (Tokio)
- Resource-efficient (deterministic memory accounting)
- Correctness-oriented (dedicated end-to-end test framework)
- Modular component model (sources, transforms, destinations)

---

## 2. Repository Structure

```
saluki/
├── bin/
│   ├── agent-data-plane/        # Main production binary (ADP)
│   └── correctness/             # End-to-end testing tools
│       ├── ground-truth/        # Test orchestrator (runs ADP vs baseline)
│       ├── millstone/           # Deterministic load generator
│       ├── datadog-intake/      # Mock Datadog API intake
│       ├── panoramic/           # Integration (smoke) test runner
│       ├── airlock/             # Container isolation library
│       └── stele/               # Simplified telemetry data types
├── lib/
│   ├── saluki-core/             # Topology engine, component traits, runtime
│   ├── saluki-components/       # Concrete sources, transforms, destinations
│   ├── saluki-app/              # App bootstrap, logging, memory management
│   ├── saluki-config/           # Configuration (Figment-based)
│   ├── saluki-context/          # Metric context (name + tags) resolution
│   ├── saluki-io/               # Network I/O primitives and codecs
│   ├── saluki-metrics/          # Internal observability helpers
│   ├── saluki-health/           # Health check primitives
│   ├── saluki-error/            # Error types
│   ├── saluki-env/              # Environment utilities
│   ├── saluki-tls/              # TLS support
│   ├── saluki-api/              # Public traits for component development
│   ├── saluki-common/           # Shared utilities
│   ├── protos/                  # Protobuf definitions (Datadog, OTLP, containerd)
│   ├── ddsketch/                # DDSketch quantile sketch algorithm
│   ├── memory-accounting/       # Memory tracking primitives
│   ├── stringtheory/            # String utilities
│   └── ottl/                    # OpenTelemetry Transformation Language
├── test/
│   ├── correctness/             # Correctness test YAML configs
│   ├── integration/             # Integration/smoke test YAML configs
│   └── smp/                     # Single Machine Performance benchmark configs
├── docker/                      # Dockerfiles for ADP, test tools, mock services
├── docs/
│   ├── reference/architecture/  # Architecture guide (topologies, components, etc.)
│   └── development/             # Contributing, testing, style, common-language
├── .gitlab/                     # CI pipeline stage definitions
├── Makefile                     # All build/test commands (single source of truth)
└── Cargo.toml                   # Workspace manifest
```

---

## 3. Architecture

### 3.1 Topologies

A **topology** is a directed, acyclic graph (DAG) of connected **components**. Think of it like a Unix pipeline: data enters at one end, is processed in the middle, and exits at the other.

Rules:
- Must have at least one input component (source or relay) and one output component (destination or forwarder).
- Components are identified by unique string names.
- Connections are declared explicitly by name.
- Validated at build time before any data flows.

**Simple topology:**
```
DogStatsD (source) → Aggregate (transform) → DD Metrics (destination)
```

**Complex topology with named outputs:**
```
OpenTelemetry (source)
  ├─[otel.metrics]→ Aggregate (transform) → DD Metrics (destination)
  ├─[otel.logs]──→ Redact (transform)    → DD Logs (destination)
  └─[otel.traces]→                         DD Traces (destination)
```

For a deeper treatment, see [`docs/reference/architecture/index.md`](../reference/architecture/index.md).

### 3.2 Component Types

| Type | Role | Examples |
|------|------|---------|
| **Source** | Gets data into the topology (push or pull) | DogStatsD receiver, internal metrics, file reader |
| **Relay** | Like a source but emits raw `Payload` bytes instead of decoded `Event`s | Network relay for direct byte forwarding |
| **Decoder** | Converts `Payload` → `Event` | DogStatsD decoder, OTLP decoder |
| **Transform** | Processes events (filter, enrich, aggregate, route) | Aggregate, origin enrichment, sampler, router |
| **Encoder** | Converts `Event` → `Payload` | Protocol-specific serialization |
| **Forwarder** | Sends `Payload` bytes to an external system | HTTP forwarder, gRPC forwarder |
| **Destination** | Sends `Event`s to an external system (encoder + forwarder combined) | DD Metrics, Prometheus scrape endpoint |

**Two pipeline styles:**
- **Event-based**: `Source → Transform(s) → Destination` — most common
- **Payload-based**: `Relay → Decoder → Transform(s) → Encoder → Forwarder` — for protocol translation

### 3.3 Data Model

The core data type is `Event`, an enum that wraps:
- **Metric** — a measurement identified by context (name + tags) + value + metadata
- **EventD** — Datadog event (alert-style)
- **ServiceCheck** — service health status
- **Log** — a log entry
- **Trace** / **TraceStats** — distributed trace data

**Metric structure:**
```
Metric
  ├── Context: name + tag set  ("who is this metric?")
  ├── Values: MetricValues enum
  │     ├── Counter / Gauge / Rate / Set
  │     ├── Histogram
  │     └── Distribution (DDSketch)
  └── Metadata: timestamp, sample rate, hostname, origin info
```

### 3.4 Outputs

- **Default output** — single output; referenced by the component's ID alone (e.g., `"dsd"`)
- **Named output** — multiple typed outputs; referenced as `"component_id.output_name"` (e.g., `"otel.metrics"`)

### 3.5 Shutdown

Ordered shutdown: topology signals all **sources** to stop → sources drain and close their output channels → transforms and destinations naturally complete when their input channels are closed. No in-flight events are dropped.

### 3.6 Runtime

Built on **Tokio**: each component is an async task. Tasks run concurrently across OS threads using Tokio's work-stealing scheduler. Components can spawn sub-tasks (e.g., one per client connection).

**Topology lifecycle:** `build()` (validate config + connections) → `spawn()` (start all tasks).

---

## 4. Build & Test Guide

### 4.1 Prerequisites

```bash
# Rust toolchain — version is pinned in rust-toolchain.toml
rustup

# System tools
protoc   # protobuf compiler
jq

# All required Cargo tools in one shot
make cargo-preinstall
# Installs: cargo-nextest, cargo-deny, cargo-hack, cargo-machete, etc.

# Verify everything is in place
make check-rust-build-tools
```

### 4.2 Building

```bash
# Debug build of Agent Data Plane
make build-adp

# Release build
make build-adp-release

# Docker image builds (requires Docker with buildx)
make build-adp-image
make build-adp-image-release
```

### 4.3 Running ADP Locally

```bash
# Requires a running Datadog Agent and DD_API_KEY env var
make run-adp

# Standalone mode (no agent needed)
make run-adp-standalone
```

### 4.4 Running Tests

The project has four testing tiers:

#### Tier 1 — Unit Tests (fast, local)

```bash
make test                  # All unit tests via cargo-nextest
make test-property         # Property-based tests (excluded from `test`)
make test-docs             # Doc tests
make test-all              # Unit + property + docs + miri + loom
```

#### Tier 2 — Integration / Smoke Tests (panoramic)

```bash
make test-integration              # Build images + run all smoke tests
make test-integration-quick        # Skip image builds (assumes images exist)
make list-integration-tests        # List available test cases
```

Smoke tests verify: startup stability, DogStatsD availability, OTLP endpoint, telemetry endpoint, config streaming, exit behavior.

#### Tier 3 — Correctness Tests (ground-truth)

```bash
# Build required images first
make build-datadog-intake-image build-millstone-image

# Build the test orchestrator
make build-ground-truth

# Run all correctness tests
make test-correctness

# Run a specific test case
make test-correctness-dsd-plain
```

Correctness tests run ADP and the Datadog Agent side-by-side in containers, drive identical telemetry workloads through both, and compare outputs semantically. Test case configurations live in `test/correctness/`.

#### Tier 4 — Specialized Tests

```bash
make test-miri             # Miri — undefined behavior / memory safety (requires nightly)
make test-loom             # Loom — concurrency correctness
make check-features        # Verify all feature flag combinations compile
```

### 4.5 Linting & Code Quality

```bash
make check-clippy          # Clippy lints
make check-fmt             # rustfmt formatting check
make check-deny            # Dependency audit (licenses, advisories)
make check-licenses        # License header check
make check-unused-deps     # Unused dependency check (cargo-machete)

# All-in-one dev cycle shortcut
make fast-edit-test        # fmt + sync-licenses + clippy + deny + test-all
```

---

## 5. Domain Vocabulary

| Term | Definition |
|------|-----------|
| **ADP / Agent Data Plane** | The production binary built on Saluki; the next-gen data plane for the Datadog Agent |
| **Topology** | A DAG of connected components; the fundamental unit of a Saluki data plane |
| **Component** | A discrete processing unit in a topology (source, transform, destination, etc.) |
| **Source** | A component that brings data *into* a topology |
| **Transform** | A component that processes/modifies data in the middle of a topology |
| **Destination** | A component that sends data *out of* a topology to an external system |
| **Relay** | Like a source, but outputs raw `Payload` bytes instead of decoded `Event`s |
| **Decoder** | A component that converts `Payload` → `Event` |
| **Encoder** | A component that converts `Event` → `Payload` |
| **Forwarder** | A component that sends `Payload` bytes to an external system |
| **Event** | The unified in-topology data type (enum: Metric, Log, Trace, EventD, ServiceCheck) |
| **Payload** | Raw serialized bytes for transport, used in relay/encoder/forwarder pipelines |
| **Metric** | An `Event` variant representing a measurement; composed of Context + Values + Metadata |
| **Context** | The identity of a metric: its name + tag set; used as the aggregation key |
| **MetricValues** | The measurement part of a Metric (Counter, Gauge, Rate, Set, Histogram, Distribution) |
| **DDSketch** | A mergeable quantile sketch algorithm used for Distribution metrics |
| **DogStatsD** | Datadog's extension of the StatsD protocol; the primary metric ingestion protocol |
| **OTLP** | OpenTelemetry Protocol; used to ingest OTel metrics, logs, and traces |
| **Default output** | A component's single primary output; referenced by the component's name alone |
| **Named output** | One of multiple typed outputs on a component; referenced as `"component.output_name"` |
| **Output ID** | The string identifier used to wire components together in a topology |
| **Figment** | The configuration library used by Saluki (layered config from files + env vars) |
| **Tokio** | The async runtime Saluki uses; provides work-stealing multi-threaded task scheduling |
| **Task** | An async unit of work in Tokio; each component runs as a task |
| **Work-stealing** | Tokio scheduler behavior: idle threads steal tasks from busy threads |
| **cargo-nextest** | Faster Rust test runner used instead of `cargo test` |
| **ground-truth** | The correctness test orchestrator: runs ADP vs Datadog Agent side-by-side |
| **millstone** | Deterministic load generator used in correctness and benchmark tests |
| **datadog-intake** | Mock Datadog API server used to capture output in correctness tests |
| **airlock** | Library that manages isolated groups of containers via containerd gRPC |
| **panoramic** | Integration/smoke test runner |
| **stele** | Helper library providing simplified telemetry types for test comparison |
| **SMP** | Single Machine Performance — the benchmark and regression testing framework |
| **FIPS** | Federal Information Processing Standards; ADP supports FIPS-compliant TLS builds |
| **RAR** | Remote Agent Registration — ADP feature for registering with a parent Datadog Agent |
| **OTTL** | OpenTelemetry Transformation Language — filter/transform language for OTel pipelines |
| **ADR** | Architectural Decision Record — short doc explaining *why* a key design choice was made |
| **MUST / SHOULD / MAY** | RFC 2119 requirement levels used in Saluki docs to clarify expectations |

---

## 6. Key Files to Read First

| File | Why |
|------|-----|
| `README.md` | 2-minute project overview |
| `docs/reference/architecture/index.md` | Core concepts: topologies, components, events, outputs, shutdown, runtime |
| `docs/development/contributing.md` | PR process and standards |
| `docs/development/testing.md` | Four-tier testing philosophy in depth |
| `docs/development/style-guide.md` | Code and documentation style expectations |
| `AGENTS.md` | Guidelines for AI-assisted development on this repo |
| `Makefile` | All available commands — the single source of truth for build and test |
| `lib/saluki-core/src/` | Core topology engine and component trait definitions |
| `lib/saluki-components/src/` | Concrete component implementations |
| `bin/agent-data-plane/src/` | Wiring that assembles ADP's topologies from the above |
