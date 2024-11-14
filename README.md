# tanooki
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/DataDog/datadog-agent/blob/master/LICENSE)

Saluki is an experimental toolkit for building telemetry data planes in Rust.

## Please Read: experimental repository

This repository contains experimental code that is **UNSUPPORTED** and may cease to receive further updates at **ANY TIME**.
In the event that we cease further development, the repository will be put into a read-only archive state.

## Structure

Everything under `lib` contains reusable/common code, and everything under `bin` contains dedicated crates for building
application-specific binaries.

### Binaries (data planes)

- `bin/agent-data-plane`: data plane used for testing Saluki, which emulates the [standalone DogStatsD server][standalone-dsd]
  in the Datadog Agent

### Helper crates

- `lib/datadog-protos`: Rust bindings generated from Protocol Buffers definitions for metrics/trace intake APIs
- `lib/ddsketch-agent`: Rust implementation of the [DDSketch][ddsketch] algorithm matched to the implementation
  [used][ddsketch-agent] in the Datadog Agent
- `lib/memory-accounting`: foundational traits and helpers for declaring memory bounds on components, and partitioning
  memory grants based on those components
- `lib/process-memory`: cross-platform library for querying the RSS of the current process with few to no allocations
- `lib/stringtheory`: custom string types and string interning implementations for high-performance string handling

### Saluki

All remaining crates are part of Saluki itself, and all have a name with the prefix `saluki-`:

- `lib/saluki-api`: minimal interface for defining API endpoints (to avoid circular crate dependencies)
- `lib/saluki-app`: generic helpers for application bring up (initialization of logging, metrics, etc)
- `lib/saluki-components`: feature-complete implementations of various common components (DogStatsD source, Datadog
  Metrics destination, and so on)
- `lib/saluki-config`: lightweight helpers for both typed and untyped configuration file loading
- `lib/saluki-context`: core primitives for metric contexts (unique name and tag combination), including zero-allocation
  resolving and caching
- `lib/saluki-core`: core primitives for building data planes, such as the topology builder, foundational traits for
  components, buffers, and more
- `lib/saluki-env`: helpers for interacting with the process's environment, such as querying time, hostname, host
  environment (e.g. cloud provider), and so on
- `lib/saluki-error`: generic error type and helpers for error handling based on `anyhow`
- `lib/saluki-event`: the core event model used by Saluki
- `lib/saluki-health`: lightweight library for defining components and checking/exposing the health of those components
- `lib/saluki-io`: core I/O primitives for networking (TCP/UDP/UDS), serialization (codecs and framers), compression,
  I/O-specific buffers, as well as some common codec implementations (e.g. DogStatsD)
- `lib/saluki-metrics`: helper macros for generating statically-defined metric structs to ease creating/holding
  registered metric handles
- `lib/saluki-tls`: lightweight library for initializing global TLS primitives, as well as build client/server TLS
  configurations, in a centralized way that is amenable to ensuring certain aspects of usage (such as operating in
  FIPS mode) are controlled and conforming

## Contributing

If you find an issue with this package and have a fix, or simply want to report it, please review our
[contributing][contributing] guide.

## Security

Please refer to our [Security Policy][security-policy] if you believe you have found a security vulnerability.

[standalone-dsd]: https://github.com/DataDog/datadog-agent/tree/main/cmd/dogstatsd
[ddsketch]: https://www.vldb.org/pvldb/vol12/p2195-masson.pdf
[ddsketch-agent]: https://github.com/DataDog/opentelemetry-mapping-go/blob/main/pkg/quantile/sparse.go
[contributing]: CONTRIBUTING.md
[security-policy]: SECURITY.md
