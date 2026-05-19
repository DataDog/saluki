# Correctness testing tools/utilities

This directory contains a number of tools/utilities that are used in conjunction with one another to form the basis of
the metrics correctness testing framework we use to check Agent Data Plane's behavior against the Datadog Agent. It
consists of the following components:

- `airlock`: helper library for running containerized applications in "isolated" groups, to allow for spawning
  supporting applications alongside a system under test (SUT) without colliding with other concurrent tests
- `panoramic`: unified test runner for both correctness tests and integration tests. For correctness tests, it drives
  an identical, deterministic telemetry workload into a baseline and comparison target (for example, Datadog Agent vs
  ADP), and compares the outputs they forward to their configured intake, highlighting any discrepancies
- `datadog-intake`: a mock intake in the spirit of [`fakeintake`][fakeintake_gh] that provides a more ergonomic
  approach to dumping the captured data
- `millstone`: a deterministic load generator, in the spirit of [Lading][lading_gh], that allow provides determinism
  around the number of payloads it sends, in addition to the basic determinism of the payloads it generates to send in
  the first place
- `stele`: helper library that established a common, simplified represent for telemetry data, and their values, to be used
  between `datadog-intake` and `panoramic`

## Building

To build any of the tools above, you can use the normal Cargo approach of `cargo build --bin <binary-name>`.
Alternatively, you can build the **correctness tools suite** -- a single container image bundling both `datadog-intake`
and `millstone` -- by running `make build-correctness-tools-image`.

## Running correctness tests

Run all correctness tests with `make test-correctness`, or a single test case with
`make test-correctness-case CASE=<test-name>`. Both targets build required images automatically
before running (`panoramic`, `correctness-tools-image`, `datadog-agent-image-release`).

After making changes to `datadog-intake` or `millstone`, run `make build-correctness-tools-image`
before the next test run to pick up the changes, or let `make test-correctness` rebuild it as part
of the normal prerequisite chain.

[fakeintake_gh]: https://github.com/DataDog/datadog-agent/tree/main/test/fakeintake
[lading_gh]: https://github.com/DataDog/lading
