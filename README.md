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

todo update instructions with full integrations-core package installation
```
#pip install datadog_checks_base # maybe not needed bc its a dependency
#pip install datadog_checks_base[deps] # maybe not needed bc its a dependency
cp $HOME/dev/integrations-core/requirements-agent-release.txt .
pip install -r $(awk -v local_path_base="$HOME/dev/integrations-core" '{sub(/^datadog-/, "", $1); gsub(/-/, "_", $1); split($1, parts, "=="); package_name = parts[1]; if (index(package_name, "checks_") != 1) {local_path = local_path_base "/" package_name "/"; print local_path}}' requirements-agent-release.txt)
# One error
# ERROR: Package 'datadog-tokumx' requires a different Python: 3.10.12 not in '==2.7.*'
# Not bad.
```

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
