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

To run the correctness tests, you must first build the related container images (the correctness tools suite and ADP
itself) before you can run the tests. This can be done by running `make build-correctness-tools-image
build-datadog-agent-image`. We avoid automatically building the container images when running the test because this can
lead to unnecessary rebuilds, and it's quicker to simply run `make build-datadog-agent-image` after making actual
changes to ADP. Once this is done, you can run the correctness test itself by running `make test-correctness`.

If updates to `datadog-intake` or `millstone` are made, rebuild the suite image by running
`make build-correctness-tools-image`. Panoramic will be rebuilt automatically when running `make test-correctness`.

[fakeintake_gh]: https://github.com/DataDog/datadog-agent/tree/main/test/fakeintake
[lading_gh]: https://github.com/DataDog/lading
