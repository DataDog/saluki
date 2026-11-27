# Correctness testing tools/utilities

This directory contains a number of tools/utilities that are used in conjunction with one another to form the basis of
the metrics correctness testing framework we use to check Agent Data Plane's behavior against the Datadog Agent. It
consists of the following components:

- `airlock`: helper library for running containers and host processes. Each case's containers get their own network
  and, on Linux, volume, so concurrent cases don't collide. Host processes get no network or volume: they share the
  runner host's network and use one fixed state directory, so `panoramic` runs only one case with host processes at a
  time
- `courier`: a small static binary that `panoramic` runs inside containers to probe listeners, send bytes, make plain
  HTTP requests and follow log files from where the software runs
- `panoramic`: unified test runner for both correctness tests and integration tests. A case is a directory of
  participants and a procedure of steps. For correctness tests, a case drives an identical, deterministic telemetry
  workload into a baseline and a comparison target (for example, Datadog Agent vs ADP), and compares the outputs they
  forward to their intake, highlighting any discrepancies. See [Panoramic cases](../../docs/development/panoramic.md)
- `datadog-intake`: a mock intake in the spirit of [`fakeintake`][fakeintake_gh] that provides a more ergonomic
  approach to dumping the captured data
- `millstone`: a deterministic load generator, in the spirit of [Lading][lading_gh], that allow provides determinism
  around the number of payloads it sends, in addition to the basic determinism of the payloads it generates to send in
  the first place
- `stele`: helper library that established a common, simplified represent for telemetry data, and their values, to be used
  between `datadog-intake` and `panoramic`
- `trace-goldens`: helper library that loads the v1.0 traces golden payload specs recorded by the Go trace-agent
  harness, and compares actual stats and trace output against the golden outputs semantically

## Building

To build any of the tools above, you can use the normal Cargo approach of `cargo build --bin <binary-name>`.
Alternatively, you can build the **correctness tools suite** -- a single container image bundling both `datadog-intake`
and `millstone` -- by running `make build-correctness-tools-image`.

## Running correctness tests

Run all correctness tests with `make test-correctness`, or a single test case with
`make test-correctness-case CASE=<test-name>`. The [Panoramic cases](../../docs/development/panoramic.md) guide
covers writing and running cases.

[fakeintake_gh]: https://github.com/DataDog/datadog-agent/tree/main/test/fakeintake
[lading_gh]: https://github.com/DataDog/lading
