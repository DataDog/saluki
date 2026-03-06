# saluki

[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/DataDog/saluki/blob/main/LICENSE)

Saluki is a toolkit for building telemetry data planes in Rust.

## Structure

Everything under `lib/` contains reusable/common code, and everything under `bin/` contains dedicated crates for
building application-specific binaries.

### Binaries

- `bin/agent-data-plane`: the primary data plane binary, which provides a production-grade DogStatsD pipeline and an
  experimental OTLP pipeline
- `bin/correctness`: binaries used to run correctness tests against ADP and standalone DogStatsD

### Libraries

The `lib/` directory contains two groups of crates:

**Reusable and general-purpose** — Implementations of features/capabilities that are required for Saluki or Agent Data
Plane but aren't specific to Saluki. Examples include `ddsketch`, generated code for Protocol Buffers definitions, and
so on.

**Saluki** (`saluki-*`) — Foundational crates that make up Saluki itself, covering topology construction, component
traits, I/O primitives, context resolution, configuration, and more.

## Contributing

If you find an issue with this package and have a fix, or simply want to report it, please review our
[contributing](https://datadoghq.dev/saluki/development/contributing) guide.

## Documentation

Procedural documentation — architecture, releasing, etc — can be found [here](https://datadoghq.dev/saluki/).

## Security

Please refer to our [Security Policy](SECURITY.md) if you believe you have found a security vulnerability.
