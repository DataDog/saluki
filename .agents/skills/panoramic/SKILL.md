---
name: panoramic
description: >
  Run Saluki's Panoramic correctness and integration tests non-interactively and investigate their
  machine-readable results. Read this before running `panoramic`, `make test-integration`, or
  `make test-correctness`, or when diagnosing a test from its artifacts.
disable-model-invocation: false
---
# /panoramic

`panoramic` runs correctness cases from `test/correctness/cases` and integration cases from
`test/integration/cases`. Get the user's approval before running these system-level suites.

## Discover the current interface

Check the current `Makefile` recipes and `panoramic --help` before choosing a command. Use
`panoramic run --help` for runner options and `panoramic list --help` to discover selection options.
This keeps commands aligned with the working tree as the harness changes.

Background:

- `docs/development/testing.md`: test types, CI entry points, and how to read results
- `docs/development/testing-patterns.md`: conventions for Rust tests
- `bin/correctness/README.md`: Panoramic and its supporting tools

## Choose the suite and runtime

Prefer the Make targets when test dependencies need rebuilding. Confirm their current recipes in the
`Makefile`; the standard entry points are:

```bash
make test-correctness
make test-correctness-case CASE=<case>
make test-integration
```

`make test-integration-quick` skips image builds. Use it only after verifying that the local images
match the working tree.

`--runtime` scopes integration discovery. On macOS, assume the `linux` runtime through the local
Docker setup unless the user explicitly requests native macOS tests; pass `--runtime linux` because
the CLI defaults to `mac` there. Follow the current Make recipe for native macOS provisioning.
Correctness cases choose their runtime from their case configuration.

Use `panoramic list` with the intended test directory and runtime to discover eligible case names
before selecting them.

## Run non-interactively

The Panoramic test targets write artifacts beneath `{{saluki}}/target/test-output/panoramic/` by
default. This in-tree path is visible to Docker on macOS. Set `SALUKI_TEST_OUTPUT_DIR` to relocate the
shared test-output root or `PANORAMIC_LOG_DIR` to override Panoramic only. Use the same base for direct
runs.

```bash
make build-panoramic
target/release/panoramic run \
  -d "$(pwd)/test/integration/cases" \
  --runtime linux \
  -t <case-name> \
  --no-tui -o json -l "$(pwd)/target/test-output/panoramic"
```

Use comma-separated names with `-t` and `-p 1` when one case needs attributable output. Each run
creates a timestamped `panoramic-*` directory beneath the base. `make clean-test-logs` deletes the
Panoramic directory under the shared test-output root; `make clean` includes that target. Stdout and
`run.json` identify the directory for a run.

## Interpret the result

Read "Reading panoramic results" and "Reading traffic artifacts" in `docs/development/testing.md`
for exit codes, the `run.json` and `result.json` layout, and how to inspect traffic captures. Treat
exit code 2 as an environment or harness problem before investigating assertion differences.
