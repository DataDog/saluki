---
name: panoramic-tests
description: >
  How to run Saluki's panoramic correctness and integration tests non-interactively and read their
  machine-readable output. Read this before running `panoramic`, `make test-integration`, or
  `make test-correctness`, or when diagnosing a failing case from its artifacts.
disable-model-invocation: false
---
# /panoramic-tests

`panoramic` runs both test suites: correctness cases in `test/correctness/cases` (ADP output compared
against the Datadog Agent) and integration cases in `test/integration/cases` (smoke tests with
assertions on logs, ports, HTTP endpoints, and config). Run `panoramic --help` for the flag and
environment-variable reference; this skill covers the workflow and the output contract.

## Choose the suite and runtime

Build the runner first (`make build-panoramic`), or use the Make targets, which also rebuild the
container images the tests depend on:

```bash
make test-correctness                      # whole correctness suite
make test-correctness-case CASE=dsd-plain  # one correctness case
make test-integration                      # whole integration suite
```

`make test-integration-quick` skips the image rebuild. Only use it when the images already match the
working tree; a stale image produces failures that look like code failures.

`--runtime` scopes integration discovery. `linux` is the container runtime: on Linux and on macOS,
where it runs through the local Docker setup (Docker Desktop, Lima, Colima), it is the runtime to
assume unless you are asked for the native macOS suite by name. On macOS the default is `mac`, so
pass `--runtime linux` explicitly there. The `mac` runtime runs ADP as a host process and needs `make
provision-macos-test-env` first. Windows cases run under the `windows` runtime on a Windows host.
Correctness cases select `docker` or `kubernetes_in_docker` from each case's own `runtime` setting.

## Run it the way a program should

```bash
target/release/panoramic run -d "$(pwd)/test/integration/cases" --runtime linux \
  -t basic-startup,telemetry-endpoint --no-tui -o json -l /tmp/panoramic-logs
```

- `--no-tui -o json`: stdout is exactly one JSON document, so `... -o json | jq` works. All tracing
  goes to stderr in both output formats.
- `-t` takes a comma-separated list of test names. A name that matches nothing exits `3` and names
  the misses.
- `-p N` sets parallelism; `-p 1` when you want attributable output for one case.
- `-l DIR` sets the log base directory. Each run gets its own timestamped subdirectory underneath;
  `run.json` and the human summary both name the directory the run actually wrote to.

## Exit codes

| Code | Meaning | What to do |
|------|---------|------------|
| 0 | Every test passed | Nothing |
| 1 | At least one assertion failed | Read the failing assertion and its details |
| 2 | The harness could not reach a verdict on at least one test | Fix the environment: images, provisioning, Docker |
| 3 | The selection named nothing to run | Fix the `-t` names or drop `-t` |

Code 2 wins over code 1: fix setup before reading a diff.

## Read the artifacts

Every run writes these regardless of `-o` or the TUI, so a killed run still leaves evidence:

```
<log-dir>/run.json                                 whole run: totals, runtime, per-test summaries
<log-dir>/<suite>/<case>/result.json               one test: outcome, error, assertions, artifacts
<log-dir>/<suite>/<case>/result.log                the same verdict, for humans
<log-dir>/<suite>/<case>/details/<n>-<kind>.txt    uncapped assertion detail (correctness diffs)
<log-dir>/<suite>/<case>/stdout.log, stderr.log    captured container/process output
```

Read `run.json`, then the `result.json` of the tests that did not pass, then `details/` when an
assertion carries more mismatch lines than the 100 `result.json` keeps inline.

`outcome` carries the verdict: `passed`, `failed` (an assertion decided against the code under
test), `errored` (the harness never got to a verdict), or `timed_out`. `error.kind` is `setup`, `timeout`, or `internal`. `run.json` also carries
`build_revision`, the commit the runner was built from, `-dirty` suffixed for a modified checkout and
`unknown` when the build could not read a repository.

A `timed_out` test carries `timeout`, which answers two separate questions. `deadline` says which
configured deadline fired: `test_deadline` (the case's own `timeout` setting) or `cleanup_grace`
(teardown overran after that deadline fired). `active_phase` says what the test was doing at that
moment — the same phase names as `phase_timings`, for example `container_start` or `assertions`, and
`unknown` when the test had not entered a phase. `configured_ms` is the duration of the deadline that
fired and `test_deadline_ms` is the case's own deadline. An assertion, action, or setup deadline
expiring shows up as a failed assertion with its own message, and a cancelled run reports `errored`
with `error.kind` of `internal`.

A test that did not pass also carries `diagnostic_lines`: captured output lines to read first. A
non-empty line in `stderr.log` qualifies on its own, with `reason` of `captured_stderr`; a
`stdout.log` line qualifies when it carries a severity token (`ERROR`, `FATAL`, `CRITICAL`, `PANIC`,
`panicked at`, or a structured `level=error`), with `reason` of `severity_marker`. Each line names
its `source` artifact and `line_number`: treat it as a lead and read it in context there before
drawing a conclusion. The set is capped, and `diagnostic_lines_truncated` says when the cap cut it
short.

```bash
jq -r '.tests[] | select(.outcome != "passed") | "\(.name) \(.outcome) \(.log_dir)"' run.json
jq -r '.tests[] | select(.outcome == "errored") | .error.message' run.json
jq -r '.tests[] | select(.outcome == "timed_out") | "\(.name) \(.timeout.deadline) \(.timeout.active_phase) \(.timeout.configured_ms)ms"' run.json
jq -r '.diagnostic_lines[] | "\(.reason) \(.source):\(.line_number) \(.text)"' \
  <log-dir>/integration/<case>/result.json
jq -r '.assertions[] | select(.passed == false) | .kind + ": " + .message' \
  <log-dir>/integration/<case>/result.json
```

## Caveats when reading logs

- `stdout.log` for a container case is the whole converged Agent's console: ADP is a small fraction
  of it. Filter on `DATAPLANE` to see the component under test.
- An empty `stderr.log` is normal; container output is merged into `stdout.log`.
- Sandbox noise is benign: `Invalid API key` 403s, timeouts to `*.datadoghq.com`, `connection
  refused` before a listener binds, and `Unknown environment variable: DD_DATA_PLANE_*` warnings from
  Go components.
- A green `process_stable_for` says the container's process tree survived, not that ADP was running.
  A green `port_listening` on UDP says Docker published the port, not that anything is bound behind
  it. Check the SUT's own log lines before concluding the subject was alive.
- Correctness cases have a fixed collection window of roughly a minute, so a short run is not a hang.

## Cleanup

`make clean-airlock` removes leftover `airlock-*` Docker volumes and networks; `make clean-kind`
removes kind namespaces. Runs can leave volumes behind, so reap them periodically.
