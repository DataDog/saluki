# Correctness testing tools/utilities

This directory contains a number of tools/utilities that are used in conjunction with one another to form the basis of
the metrics correctness testing framework we use to check Agent Data Plane's behavior against the Datadog Agent. It
consists of the following components:

- `airlock`: helper library for running containerized applications in "isolated" groups, to allow for spawning
  supporting applications alongside a system under test (SUT) without colliding with other concurrent tests
- `ground-truth`: a test runner designed specifically to drive an identical, deterministic telemetry workload into a
  baseline and comparison target (e.g., Datadog Agent vs ADP), and compare the outputs they forward to their configured
  intake, highlighting any discrepancies
- `datadog-intake`: a mock intake in the spirit of [`fakeintake`][fakeintake_gh] that provides a more ergonomic
  approach to dumping the captured data
- `millstone`: a deterministic load generator, in the spirit of [Lading][lading_gh], that allow provides determinism
  around the number of payloads it sends, in addition to the basic determinism of the payloads it generates to send in
  the first place
- `stele`: helper library that established a common, simplified represent for telemetry data, and their values, to be used
  between `datadog-intake` and `ground-truth`

## Building

To build any of the tools above, you can use the normal Cargo approach of `cargo build --bin <binary-name>`.
Alternatively, you can build container images (necessary to run the correctness tests) of the tools using `make`,
following the pattern of `make build-<binary-name>-image`. For example, to build `millstone`, you could run either
`cargo build --bin millstone` or `make build-millstone-image`.

## Running correctness tests

To run the correctness tests, you must first build the related container images (`datadog-intake`, `millstone`, and ADP
itself) before you can run the tests. This can be done simply by running `make build-datadog-intake-image
build-millstone-image build-datadog-agent-image`. We avoid automatically building the container images when running the
test because this can lead to unnecessary rebuilds, and it's quicker to simply run `make build-datadog-agent-image`
after making actual changes to ADP. Once this is done, you can run the correctness test itself by running `make
test-correctness`.

If updates to any of the required components are made, you can simply rebuild the individual corresponding image
by running the corresponding `make build-<binary-name>-image` command. If changes are made to `ground-truth`, it will be
rebuilt when running `make test-correctness`.

[fakeintake_gh]: https://github.com/DataDog/datadog-agent/tree/main/test/fakeintake
[lading_gh]: https://github.com/DataDog/lading
