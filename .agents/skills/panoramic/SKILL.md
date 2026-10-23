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

These human-facing documents provide optional background:

- `docs/development/testing.md`: test types and CI entry points
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

Exit codes distinguish the run-level result:

| Code | Meaning |
|------|---------|
| 0 | Every test passed |
| 1 | At least one assertion failed |
| 2 | Harness error, timeout, or interrupted run |
| 3 | The selection matched no tests |

For code 2, investigate the harness or environment before assertion differences.

Check `run.json` first: it is updated during execution, so passing entries do not prove the run
finished. Inspect completeness before individual results; treat unknown states, outcomes, or
statuses as non-passing.

```bash
jq '{state, interrupted_reason, running, not_run}' run.json
jq -r '.tests[] | select(.outcome != "passed" or (.issues | length) > 0) | "\(.name) \(.outcome) \(.log_dir)"' run.json
```

Follow the selected tests' `log_dir` to `result.json`. Use `outcome` for the test result and
`assertions[].status` for each step. Check `issues` even on passes; they do not change `outcome`.
Open reported artifact paths and `diagnostic_lines` sources for context and uncapped details.

For semantics, read `RunReport`, `TestReport`, and `AssertionReport` in
`bin/correctness/panoramic/src/machine_output.rs` and follow their types. Step-to-outcome rules are
in `TestResult::from_steps` in `reporter.rs`. Check `schema_version` and `run.json`'s `build_revision`
when reports differ from the working tree.

On Unix, send SIGINT or SIGTERM to stop a run with cleanup; a second signal may leave containers.

## Read the traffic artifacts

Traffic captures live under `<log_dir>/traffic/`. Start with `traffic/manifest.json`: it lists each
capture, whether its file is still on disk, and why any capture is missing. Take file names from the
manifest rather than guessing them. Captures are zstd-compressed JSON Lines:

```bash
jq -r '(.input // empty), .decoded[] | select(.file.present) | .file.name' traffic/manifest.json
zstd -dc traffic/<file-name> | head -5
```

The module docs in `bin/correctness/panoramic/src/traffic.rs` explain each artifact, its retention,
and record ordering. For intake sidecar captures, see "Intake artifacts" in
`test/integration/README.md`.
