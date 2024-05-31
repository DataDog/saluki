# saluki
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

### Saluki

All remaining crates are part of Saluki itself, and all have a name with the prefix `saluki-`:

- `lib/saluki-app`: generic helpers for application bring up (initialization of logging, metrics, etc)
- `lib/saluki-components`: feature-complete implementations of various common components (DogStatsD source, Datadog
  Metrics destination, and so on)
- `lib/saluki-config`: lightweight helpers for both typed and untyped configuration file loading
- `lib/saluki-core`: core primitives for building data planes, such as the topology builder, foundational traits for
  components, buffers, and more
- `lib/saluki-env`: helpers for interacting with the process's environment, such as querying time, hostname, host
  environment (e.g. cloud provider), and so on
- `lib/saluki-event`: the core event model used by Saluki
- `lib/saluki-io`: core I/O primitives for networking (TCP/UDP/UDS), serialization (codecs and framers), compression,
  I/O-specific buffers, as well as some common codec implementations (e.g. DogStatsD)

### Saluki Components
#### Checks
Python checks can be loaded and executed from the active venv via yaml config files in `./dist/conf.d`.

Local python checks can be placed in `./dist/foo.py` and configured via `./dist/conf.d/foo.yaml`.

pyo3 is used to provide python support, which works off of your system's python
install, `sudo apt install libpython3-devel` (todo double check the package name)

#### Getting required python libs into a venv
This is way harder than I expected.

The required python libs are all within `integrations-core` and generally
speaking, `pip install datadog_checks_base pip install datadog_checks_base[deps]` is sufficient to
a "hello-world" style check such as what is committed in this repo.

Once we want to run "standard" checks such as `http` or `mongodb`, what we now
want is to install all the default integrations, just like the Agent does.


In `integrations-core` there is a file
[requirements-agent-release.txt](https://github.com/DataDog/integrations-core/blob/master/requirements-agent-release.txt)
which is a generated `requirements.txt` that records the versions for a given
release.

By combining this and the [wheels index](
https://dd-integrations-core-wheels-build-stable.datadoghq.com/targets/simple/index.html)
, we _should_ be able to install the required packages.


Prepending `--index-url https://dd-integrations-core-wheels-build-stable.datadoghq.com/targets/simple/`
to the `requirements-agent-release.txt` and then handing it to `pip` _should_
work.

However it does not. The above integrations-core-wheels is backed by an s3
bucket and does not have `index_url` set to return the `index.html` when a
directory is requested.

Some amount of this can be hacked around with an `awk` script to process the
requirements.txt, but there are more issues that I was never fully able to
resolve.

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
