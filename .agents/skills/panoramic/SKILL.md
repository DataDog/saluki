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

Build with the current Make recipe, then use `--no-tui -o json` for an agent-readable run. Set
`-l DIR` when the artifacts need a predictable base directory.

```bash
make build-panoramic
target/release/panoramic run \
  -d "$(pwd)/test/integration/cases" \
  --runtime linux \
  -t <case-name> \
  --no-tui -o json -l /tmp/panoramic-logs
```

Use comma-separated names with `-t` and `-p 1` when one case needs attributable output. Each run
creates a timestamped directory beneath `-l`; stdout and `run.json` identify that directory.

## Interpret the result

Exit codes distinguish the run-level result:

| Code | Meaning |
|------|---------|
| 0 | Every test passed |
| 1 | At least one assertion failed |
| 2 | The harness could not reach a verdict for at least one test |
| 3 | The selection matched no tests |

Treat code 2 as an environment or harness problem before investigating assertion differences.

A completed run writes `run.json`; each test directory contains `result.json`, `result.log`, and its
captured artifacts. Start with the non-passing entries in `run.json`, follow each entry's `log_dir`,
then use the paths in `result.json` rather than constructing artifact paths yourself.

```bash
jq -r '.tests[] | select(.outcome != "passed") | "\(.name) \(.outcome) \(.log_dir)"' run.json
jq -r '.assertions[] | select(.passed == false) | .kind + ": " + .message' <case-log-dir>/result.json
jq -r '.diagnostic_lines[]? | "\(.source):\(.line_number) \(.text)"' <case-log-dir>/result.json
```

`outcome` is the verdict: `passed`, `failed`, `errored`, or `timed_out`. For a timeout, read the
`timeout` object to identify the deadline, active phase, and configured duration. For other failures,
read `error`, failed `assertions`, and `diagnostic_lines`, then open the referenced artifacts for
context. Correctness assertion details may point to uncapped files under `details/`.

`run.json` includes `build_revision`: the source revision used to build Panoramic, with `-dirty` for
a modified checkout and `unknown` when it cannot be determined.

## Read the traffic artifacts

A correctness test records what it sent and what each side decoded under `<log_dir>/traffic/`. Start
with `traffic/manifest.json`: it names each capture, reports its compressed size, and says whether
the file is still on disk (`file.present`). A passing test keeps only the manifest; every other
outcome keeps the captures. When `input` is `null`, read `input_unavailable_reason` - the
`kubernetes_in_docker` runtime never produces an input capture.

The captures are zstd-compressed JSON Lines, one record per line, so decompress before reading:

```bash
zstd -dc traffic/input.jsonl.zst | head -5
zstd -dc traffic/baseline-decoded.jsonl.zst | jq -c 'select(.kind == "metric") | .value.context'
```

Input records are in send order, which is not packet order. Decoded records are grouped by kind and
indexed within each kind, so `index` is a position within a kind, not a global receive order.
