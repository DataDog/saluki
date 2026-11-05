# Testing

> **Note for developing on macOS**: Docker typically runs inside a Linux VM and the socket may not be at the standard
> `/var/run/docker.sock` path. The test tooling automatically searches common non-standard socket locations when the
> standard path is absent. If auto-detection doesn't find your socket, set the `DOCKER_HOST` environment variable.

The Saluki testing strategy consists of four main pillars:

1. Unit Tests (Rust/cargo)
2. Correctness Tests (panoramic)
3. Integration Tests (panoramic)
4. Performance Tests (SMP)

Alongside these, we use fuzzing, Loom and Miri for concurrency and memory-model correctness, and Antithesis for
fault-injected equivalence testing.

## Unit tests

These are found throughout the Rust codebase as you would expect. You can run them with `cargo test --lib --bins` or
use `make test`, which runs them with `cargo nextest` for more parallelization. Platform-specific unit tests should be
skipped or compiled-out for platforms they're incompatible with. For conventions on test naming, table-driven tests,
shared fixtures, and assertion density, see [Testing patterns](./testing-patterns.md).

CI: `.gitlab/test.yml`—runs on both Linux (amd64/arm64) and macOS (amd64/arm64).

### Airlock daemon tests

Airlock's daemon tests exercise isolation groups, container creation, name resolution, port publication, structured
and streaming exec, placing, inspecting, and reading files, sharing the host PID namespace, container logs, exit waits, and
container and group removal against a real Docker daemon. The port tests publish on host ports in `30000`–`32699` of the
Docker host and need host networking.
Run these tests with `make test-airlock-daemon`, which runs `cargo nextest run -p airlock --test daemon`.
`AIRLOCK_TEST_IMAGE` defaults to `alpine:latest`; load it first, because these tests use only local images.
They fail if the daemon or image is unavailable. Plain `cargo test` or `cargo nextest run -p airlock` also runs them;
use `make test` or select `--lib` to run only unit tests.

CI: the `test-container-daemon` job in `.gitlab/e2e.yml` runs them on its Docker-in-Docker runner, with
`AIRLOCK_TEST_IMAGE` set to `registry.ddbuild.io/alpine:latest`, which the job pulls before testing.
The daemon suite runs separately from the Linux integration cases.

### Panoramic daemon tests

Panoramic's daemon tests exercise native case creation and the Linux container backend against a real Docker daemon:
scope preparation and removal, creating participants from recipes with files, the courier and host mounts, command
execution, file reads and status, courier probes and sends, and readiness gates before consumers start. They check
container exit and output observation, incremental stdout and stderr files, readiness waits ending when a server exits,
and process exit channels that preserve the first supervisor record through restarts. Process output tests check tagged
container lines, files created after following starts, and a final read that captures output from exited containers.
Output readiness tests check that consumers wait for a matching line from the declared process. They also check HTTP
responses through published TCP ports, courier probes of published ports with no listener, explicit loopback bindings,
held-port startup failures, and courier routes to exited participants. Container probes and raw sends run inside the
server because port proxies can accept connections even when the server has no listener.
Native procedure tests cover `passed`, `failed` and `errored` steps, fail-fast, endpoint routes, late-fact resolution,
missing process exit records, capture failures, cancellation and case deadlines. Reclaim tests check individual
participant entries, concurrent graceful stops, and interrupted creation that leaves both a participant and its scope
unconfirmed. Runner tests exercise creation and startup failures, removal during a procedure, output overflow,
interruption and case deadlines through artifact verification and the native runner. They check `result.json` and that
no container, volume or network with the case's scope label remains. Report tests also check native process reports
and snapshots after a case body exceeds its cancellation grace.
They are ignored by default. Run them with `make test-panoramic-daemon`, which builds the Linux courier first.
`PANORAMIC_TEST_IMAGE` defaults to `alpine:latest`; load it first, because these tests never pull images.
They fail if the daemon, image or courier is missing.
The target runs the ignored backend, lifecycle, runner failure and report tests in `panoramic-core` and `panoramic`.

CI: the `test-container-daemon` job in `.gitlab/e2e.yml` runs them after airlock's daemon tests, with
`PANORAMIC_TEST_IMAGE` set to the same local Alpine image.

#### Windows daemon tests

The `windows_container::daemon_tests` module in `panoramic-core` exercises the Windows container backend against a
Windows Docker daemon: participants reach each other by name on a `nat` network, the archive API reads and stats files
in running and stopped containers, exec delivers length-bounded stdin, and the courier reaches named pipes, TCP, UDP and
HTTP and follows files. They also check exit record and output observation, including a container that stops without a
record. Each test prints the daemon's isolation mode and OS version first.

The courier's `send` command requires `--stdin-len N` and reads exactly N bytes, including zero for an empty payload.
This lets sends finish over Windows engine named pipes, whose write-side shutdown only flushes. Short or timed-out
stdin is a harness error; network timeouts remain product failures. The stdin read and send share one time budget.

Set `PANORAMIC_TEST_WINDOWS_IMAGE` to a local Windows image and
`PANORAMIC_TEST_WINDOWS_COURIER` to the path of a natively built `panoramic-courier.exe`. The Windows integration job
sets both and runs them before the integration cases. `make test-panoramic-daemon` excludes them on Linux.

### Panoramic kind tests

`make test-panoramic-kind` runs the ignored `backend::pod::kind_tests` suite in a cluster it creates and deletes.
It covers Service routing, node-backed files, courier execution, TCP port-forwards, late files, exit and output
observation, and reclaim after successful, failed, timed-out and cancelled native scenarios.

Install `kind` and `docker`, set `DOCKER_HOST` to the daemon kind uses, and load a local `PANORAMIC_TEST_IMAGE`
(default `alpine:latest`). The target builds the static musl courier with `make build-courier-linux` before testing.
The suite runs locally only; it has no dedicated CI job.

### Panoramic host process tests

The `panoramic-core` unit tests run host process groups through the native lifecycle on Unix hosts, using `/bin/sh`.
They cover per-process exit and output observation, readiness, clean environments, file and command primitives,
and bounded process-group cleanup. Run them with `cargo nextest run -p panoramic-core`.
The macOS `agent-adp` participant cases use this native lifecycle.

Host participants run in a planned per-case state directory under `/tmp`, which must not already exist. Each process
inherits only `PATH`, `HOME` and `TMPDIR` from the runner when set, then receives the participant's environment and its
own overrides. Each leads its own process group. Its stdout and stderr reach separate files at
`<log_dir>/<participant>/<process>.stdout.log` and `<process>.stderr.log`, with process and stream tags on captured
lines. The participant ends after every process has an observed exit; a failed wait leaves the exit undetermined.
Host exec commands own their process groups: returning or cancelling an exec kills its remaining descendants,
including background processes started by a successful command.

### Panoramic product tests

Run `make test-panoramic-products` to exercise real product images through the native runner. These ignored tests
run locally, separately from the Alpine daemon suite.
The target builds the Linux courier and requires a Docker daemon with the images already loaded.
Image bindings come from:

| Variable | Artifact role | Local default |
| --- | --- | --- |
| `PANORAMIC_AGENT_ADP_LINUX_IMAGE` | `agent-adp-linux-image` | `saluki-images/datadog-agent:testing-devel` |
| `PANORAMIC_TOOLS_LINUX_IMAGE` | `tools-linux-image` | `saluki-images/correctness-tools:latest` |
| `PANORAMIC_TEST_IMAGE` | `fixup-image` | `alpine:latest` |

Each test verifies the roles its case needs. The intake test starts two intakes in one scope, requests dumps through
each intake's operation, sends a forwarded DogStatsD packet to one, and checks that only that intake retains it.
Agent tests deliver DogStatsD metrics to a connected intake, kill ADP and check its recorded signal death and
supervised restart, and compare the image's shipped service scripts and listener environment with the definition's
sources and defaults.
The target runs tests serially because each runner invocation allocates host ports from the same pool. Tests check
native participant reports and confirm that the scope's containers, volume and network are removed.

## Correctness tests (panoramic)

These tests serve to answer the question: *Does ADP produce the same output as the Datadog Agent for a given workload?*

To answer this question, a correctness test runs ADP and the Datadog Agent side-by-side in containers and compares their
output for a given input. The output comparison is semantic, not a simple byte-by-byte comparison, thus heuristics are
used to assert correctness.

**Terminology Note:** Correctness tests ***are integration tests*** in the sense that they run the entire system.
However, in our repository, *integration tests* refer to a specific set of smoke tests [below](#integration-tests-panoramic).

Correctness test cases are specified by YAML configuration files found in `test/correctness/cases`.
Files under `test/correctness/disabled/` are ordinary cases that CI does not select. CI and the `test-correctness`
targets select only `test/correctness/cases/`. `panoramic list -d test/correctness/disabled --subject-os linux`
compiles the disabled cases without running them.

### Binaries and program flow

All binaries live under `bin/correctness/`:

| Binary                | Purpose                                                                                                                                    |
|-----------------------|--------------------------------------------------------------------------------------------------------------------------------------------|
| **panoramic**         | Unified test runner for both correctness and integration tests.                                                                            |
| **millstone**         | Deterministic load generator.                                                                                                              |
| **datadog-intake**    | Mock Datadog API: receives test output                                                                                                     |
| **airlock**           | Library for running containers in isolated groups.                                                                                         |
| **panoramic-courier** | Sends to and probes TCP, UDP and Unix socket endpoints. Panoramic calls its library for direct access from the runner host, or its binary inside a participant's container. |

Test case configs live in `test/correctness/cases/` (for example, `test/correctness/cases/dsd-plain/config.yaml`).

### Program flow

Panoramic loads each case's participants, connections and procedure, then runs the participants as Linux containers
or Kubernetes pods. One load participant sends the same workload to the Agent and ADP, each connected to its own
intake. The `compare` step compares the subject and baseline intakes using the case's telemetry kind and analysis
options.

### Running

```bash
# Run all correctness tests:
make test-correctness

# Run a single test case:
make test-correctness-case CASE=dsd-plain
```

Both targets build the prerequisites and render `target/panoramic-artifacts-correctness.toml`, binding
`agent-adp-linux-image` to `saluki-images/datadog-agent:testing-release`. Set `CORRECTNESS_TEST_ARTIFACTS` to change
the rendered manifest's path. Pass `PANORAMIC_ARTIFACTS=<file>` on the make command line to run with your own
manifest instead. Integration keeps the `testing-devel` default.

The seven `dsd-origin-detection-matrix-<variant>` cases run as Kubernetes pods in kind. Each has its own directory
under `test/correctness/cases/`, with symlinks to the configuration and workload files in
`test/correctness/shared/dsd-origin-detection-matrix/`. To run one with your artifact manifest:

```bash
target/release/panoramic run -d "$(pwd)/test/correctness/cases" \
  -t dsd-origin-detection-matrix-legacy --subject-os linux --artifacts artifacts.toml --no-tui
```

Correctness tests are run in the `e2e` stage in CI (`.gitlab/e2e.yml`).

## Integration tests (panoramic)

Integration tests run a containerized ADP instance and assert high-level invariants: process stability, expected log
output, port availability, exit behavior. They catch regressions from enabling new features or settings that cause
crashes or early exits. They don't test output correctness. This type of test is often known as a "smoke test." For
integration tests that check system output, see [correctness tests](#correctness-tests-panoramic) above.

### Running

```bash
make test-integration        # build images + run all
make test-integration-quick  # skip image builds
make run-integration-quick   # skip binary and image builds
make list-integration-tests  # list available tests
```

`test-integration-quick` builds panoramic and the Linux courier, then calls `run-integration-quick`.
CI calls `run-integration-quick` with binaries from `build-panoramic-binary`.

These targets select subject OS `linux`, including on macOS, where tests run in Linux containers through the local
Docker setup. Use the macOS Make targets to run tests as host processes.

Pass `PANORAMIC_ARTIFACTS=<file>` on the make command line to run with an artifact manifest:

```bash
make test-integration-quick PANORAMIC_ARTIFACTS=artifacts.toml
```

`test-integration`, `test-integration-quick`, `run-integration-quick`, `test-integration-macos-run`,
`test-correctness` and `test-correctness-case` accept this variable only on the command line; they reject values
from the environment, including empty values. For `test-integration-macos-run`, the manifest must bind
`adp-host-binary` to the ADP build, `agent-host-install` to the Agent install, and `intake-host-binary` to the
host intake built by `make build-intake-host`. `make render-macos-test-artifacts` writes a matching manifest to
`MACOS_TEST_ARTIFACTS` (default: `target/panoramic-artifacts-macos.toml`). Use the same `BUILD_PROFILE` and
`MACOS_TEST_AGENT_INSTALL_DIR` values for rendering and running.
`test-integration-macos-ci` renders and forces its own manifest, even if you supply another path.

### Test case config

Test cases live in `test/integration/cases/` as `config.yaml` files. The runner is **panoramic**
(`bin/correctness/panoramic/`). A case file must be named `config.yaml`; discovery rejects `config.yml`.

Integration cases declare participants; see [Participant cases](#participant-cases). Assertions run sequentially
by default but can also run in `parallel`.

CI: `test-integration` in `.gitlab/e2e.yml` runs in the `e2e` stage with a 15 min timeout and retry 2. It uses the
panoramic and courier binaries from `build-panoramic-binary`.

### Participant cases

A case file without a top-level `type` declares **participants**: named instances of a participant type. The case
states what runs, how it's configured and what the procedure checks. The participant type knows how its product is
installed and where it writes. Every case in `test/integration/cases/` uses this schema;
`adp-logging-default-path` looks like this:

```yaml
name: "adp-logging-default-path"     # Optional; must match the directory name.
description: "Verifies ADP writes to its default log file"
timeout: 120s                       # Required.
subject_os: [linux]                 # Required; any of linux, macos and windows.

participants:
  adp:                             # The participant's name, used by `on` and in references.
    type: agent-adp                # The Core Agent and ADP running together.
    subject: true
    config:
      env:
        DD_API_KEY: "00000000000000000000000000000000"
        DD_DATA_PLANE_ENABLED: "true"

procedure:   # Required; at least one step.
  - step: log_contains
    on: adp
    pattern: "Initial configuration received."
    timeout: 30s
  - step: file_contains
    on: adp
    path: "{{ adp.facts.default_log_file }}"
    pattern: "DATAPLANE"
    timeout: 30s
```

A participant case's reporting suite is the directory above its discovery root: both `test/correctness/cases/`
and `test/correctness/disabled/` report `correctness`; `test/integration/cases/` reports `integration`.

Participant cases use native execution on every subject OS, including `agent-adp` in Windows containers.
Panoramic prepares each case's scope, creates participants in create order
from verified artifacts, and starts each level once the readiness evidence of the servers it connects to is observed. A
readiness wait fails as soon as its server's container exit is observed. Each started container's exit and output are
watched in case-owned tasks. Output reaches `<log_dir>/<participant>/container.stdout.log` and `container.stderr.log` as
it arrives, keeping the streams separate. Each process declares either container output or a file in its participant's
log directory. File output reaches `<process>.stdout.log`, `<process>.stderr.log` or `<process>.log` for merged streams;
a final read after exit appends bytes the follower missed. Lines carry their participant, process, stream and receipt
time. Output readiness waits for a regex match in the named process's lines and fails on that process's capture error.
Readiness and step readers share a 64 MiB line-text bound per participant across all output sources. After overflow,
the buffer drops new lines, preserving existing cursors. Readers can consume the retained prefix, then get a capture
error naming the bound and the output files. Empty lines count as one byte; unterminated lines are also bounded.
Capture still writes every byte to disk, so the files remain the full output source.
After the start levels, native cases run their procedure in the `steps` phase, with step
budgets, case deadline, parallel blocks and fail-fast behavior. Steps receive the started
participants through borrowed handles. File paths and literal log patterns resolve early and late facts when the step runs.
A `parallel` block accepts only assertions. Actions, such as `exec` and `send`, must be sequential steps; discovery rejects an
action in a parallel block before building its fields.

The `wait` action pauses for its required, positive `duration`, such as `30s`. It names no participant and takes no
`on` field. Its step budget is that duration, and case cancellation interrupts the pause. As an action, it cannot
appear inside `parallel`.

The native log and file steps accept `on`, `pattern` and an optional `regex` flag (default false):

- `log_contains` searches output from the participant's start, optionally narrowed by `process`, until `timeout`.
  Without `process`, it searches every process's output together. On a Windows `agent-adp` container that includes the
  Agent's output as well as the container console.
  A match passes; completed output without a match or a timeout fails.
- `log_not_contains` searches the same output for `during`, optionally narrowed by `process`. A match fails and quotes
  the matching line. Completed output keeps the wall-clock window; an exit alone does not fail this assertion.
- `file_contains` resolves `path` once, then reads until the contents match or `timeout` expires. Invalid UTF-8 bytes
  become replacement characters. Missing files and contents without a match are retried. Timeout reports the last
  observed file state.

The log steps' `pattern` may reference named participants' facts and late facts when `regex` is false. Patterns
with references and `regex: true` are rejected at load. Each step resolves its pattern once before searching;
an unavailable reference errors the step. Diagnostics quote the resolved pattern.

Capture failures and failed read mechanisms are errors, not assertion failures. Output steps scan available lines
before reporting a capture failure. With `process`, only that process's capture failure errors the step; a process
that uses container output also observes failures of that capture. Without `process`, a failure in any capture of
the participant errors the step. The shared line-buffer bound applies to every cursor, even when narrowed to one
process.

Native process and endpoint steps also require `on`:

- `process_stable_for` requires `duration` and optionally accepts `process`. It checks the named process, or every
  declared process when `process` is omitted. The participant must declare at least one process. Every checked
  process must be running at the start, throughout the window and at the end. An exit before or during the window
  fails, even if a supervisor restarts the process.
  The step concurrently re-reads exit records at the end, allowing up to five seconds for all reads.
- `exits_with` requires `process`, `code` and `timeout`. It waits for the process's first exit and compares the code
  exactly: code 10 does not satisfy code 1. A different code, a signal death or a process still running at timeout
  fails. It re-reads the exit record before reporting a timeout.
- `port_listening` requires `endpoint` and `timeout`. It probes the named endpoint through its planned route until
  it listens or the timeout expires. An addressable UDP endpoint also passes, because UDP cannot confirm a listener.
  A timeout fails after any product answer; it errors if only harness failures prevented observation. A participant
  exit does not end the retry window early.
- `http_check` requires `endpoint`, `path`, `status` and `timeout`. The path must start with `/`; status is either
  `{equal: N}` or `{not_equal: N}`. An optional `pattern` matches the response body, with `regex: true` for a regular
  expression; `regex: true` requires `pattern`. Body bytes use lossy UTF-8 decoding. The step retries until both
  status and body match. At timeout, any HTTP response or product error makes the step fail; only harness errors
  make it error. The last diagnostic gives the status and says when the body did not match. For an HTTPS endpoint,
  set `insecure_skip_verify: true` to skip certificate and handshake-signature verification. It defaults to false;
  HTTPS certificate verification is unsupported and produces a harness error. Skipping verification on a plain HTTP
  endpoint also produces a harness error.
- `exec` is an action requiring a nonempty `argv` list and a positive `timeout`. Each argument may reference
  participant facts or late facts, resolved before execution. It runs once: exit 0 passes, a nonzero exit or an
  abandoned command at timeout fails, and a failed execution mechanism errors. Diagnostics include the exit code
  and trimmed stdout and stderr, capped at 4096 bytes.
- `send` is an action requiring `endpoint`, `payload` and `timeout`. It sends the string's UTF-8 bytes verbatim,
  including any framing, through the endpoint's planned route. Optional `repeat` is a positive integer (default 1):
  it repeats the payload in one message, and diagnostics report the total bytes. Zero is rejected at load.
  YAML escapes above `\x7f` encode as UTF-8, so frame-length bytes must be below 0x80.
  It retries failed sends until one is accepted or the timeout expires.
  A retry after a harness error may deliver the payload twice. A timeout fails after any product refusal; it errors
  if only harness failures occurred. UDP cannot confirm a listener, so gate UDP sends on the participant's listener
  output. UDP selects the first IPv4 address from hostname resolution, or the first address if none is IPv4.
  The send report includes every resolved address and the selected destination; an IPv6 literal selects IPv6.

Three native actions use participant operations:

- `cli` requires `on`, `process`, `args` (a list of strings, which may be empty) and a positive `timeout`.
  It calls `cli_<process>` once. Planning rejects a process whose CLI operation is unavailable.
  Optional `output_contains` requires a literal substring in the complete output. Success passes; a missing
  substring or product error fails. A harness error, including an abandoned command at timeout, errors the step.
  This differs from the integration runner's CLI timeout, which fails. Successful calls and substring mismatches
  quote trimmed output; messages are capped at 4096 bytes without splitting UTF-8 characters.
- `config_set` requires `on`, a nonempty `key`, `value` (any JSON value) and a positive `timeout`.
  It sends strings unchanged and other values as JSON text to `config_set`. It retries every 500 ms until accepted,
  giving each call the remaining budget. At timeout, any product error makes the step fail; only harness errors
  make it error. The diagnostic includes the last attempt.
- `dogstatsd_replay` requires `on`, `endpoint`, a nonempty `payloads` list, `capture_duration`, `stats_duration`,
  a nonempty `expected_metrics` list and a positive `timeout`. Each payload has string `data` and an optional
  positive `after` duration, waited before that payload. Statistics duration must be whole seconds, at least `1s`.
  The action starts capture, sends each payload once through the endpoint, waits the capture duration, then runs
  statistics and replay concurrently. It repeats that pair every 50 ms until the statistics contain every expected
  metric name exactly. Each command gets `timeout`; sends use the endpoint's attempt limits and are not retried.
  A capture or send error ends the action: product errors fail and harness errors error. A statistics error after
  a successful replay ends the action; an earlier completed replay/statistics attempt or replay product error
  keeps the result failed. Otherwise the statistics error's classification decides the result. If replay itself
  fails, the action retries even if statistics failed too. At the replay deadline, a completed replay/statistics
  attempt or replay product error makes the step fail; only replay harness errors make it error. Diagnostics
  retain the latest replay error or missing metrics. Dropping the action cancels every wait.

The replay step budget is `timeout × (3 + number of payloads) + capture_duration + sum(after) + 50 ms`.
It covers capture, each send, and a final statistics/replay pair that starts at the replay deadline.

An undetermined process exit errors stability and exit assertions. A process's first recorded exit remains final
across supervised restarts.

Participant types offer named **operations** per realization. An operation takes JSON arguments and returns JSON,
keeping product-specific commands inside the participant definition. Steps declare the operations they call;
plan compilation rejects an operation the participant's type does not offer for its realization, so `panoramic list`
reports the error before anything starts. Operations run commands through the participant's exec primitive or request
their own declared HTTP endpoints through planned routes. On Windows, HTTP requests run through the courier inside
the server participant. Host operations can use `exec_process` to run a declared process's program with replacement
arguments, its own environment and the participant directory. Other realizations reject this primitive.
Planning validates those endpoints and allocates request routes and any published ports.
An operation can request only the HTTP endpoints it declares.
Argument and mechanism problems are harness errors; a product answer that does not satisfy the operation is a
product error.

The native `config_key_equals` step requires `on`, `key` (a dotted path), `value` (any JSON value), `view` (`source`
or `runtime`) and `timeout`. It calls the participant's `config_dump` operation with the requested view until the key's
JSON value equals `value`. Each call receives the whole remaining step budget. Mismatches, missing keys and product
or harness errors are retried; at timeout, any product observation makes the step fail, while only harness errors
make it error. A participant exit does not end the retry window early. The timeout message includes the last
observed value, or `<missing>` for an absent key.

The native `intake_has_metric` step polls the intake participant named by `on` until it holds a metric matching
every criterion given, or until `timeout` elapses. Polling, rather than a single check, absorbs the target's
aggregation and flush window without a fixed sleep.

Each poll normalizes the intake's metrics per context (name and tags) and type, the same way correctness analysis
does: counts and rates are summed, a gauge takes its latest value, and sketches are merged. A counter flushed as
one `0.6` rate point or as two `0.3` points over equal intervals therefore matches `value: 0.6`. Contexts are never
combined: the assertion passes when one context meets every criterion by itself. Expect totals, since a partial sum
can match an early poll before the rest arrives.

`on`, `name` and `timeout` are required; `name` must match exactly. The other criteria are:

| Field | Matching rule |
| --- | --- |
| `metric_type` | `count`, `rate`, `gauge`, or `sketch`; omitted accepts every type. |
| `value` | Compares the normalized value using approximate float equality, as correctness analysis does. A rate uses its own interval. Combining `value` with `metric_type: sketch` fails loading. |
| `tags` | Compares sorted, deduplicated tags as `tags_match` directs; defaults to empty. |
| `tags_match` | `subset` (default) requires every listed tag and allows extras. `exact` requires the context's tags to equal the list. |
| `tag_patterns` | A list of regular expressions, each requiring a match in at least one tag of the same context, including its host tag. Defaults to empty; invalid patterns fail loading. |
| `max_age` | Optional positive duration. The context's latest point must have a positive timestamp within this duration of check time, including the boundary. Excessive future skew also fails. Omitted accepts any timestamp. |

The intake reports a metric's host as a `host:<value>` tag, so under `exact`, `tags: []` means no tags and no host.
For gauges, the value and freshness criteria apply to the same latest point. Other metric types retain their
normalized totals; `max_age` checks the latest contributing point's timestamp.

`exact` constrains the one matching context. It doesn't prove that no other contexts arrived.

```yaml
- step: intake_has_metric
  on: intake
  name: "example.counter"
  metric_type: rate
  value: 0.6
  tags: ["source:integration-test"]
  timeout: 120s

- step: intake_has_metric
  on: intake
  name: "hosttag.metric"
  metric_type: gauge
  value: 4
  tags: ["host:custom"]
  tags_match: exact
  timeout: 120s
```

On failure, the message lists the contexts that share the expected name, with each one's type, normalized value,
point count, and the first criterion it failed. When no context has that name, it lists other observed names
instead.

A timeout fails if any poll returned a valid metrics snapshot; otherwise it errors with the last poll error.
The step keeps polling until its timeout even if the subject exits.

The native `intake_has_forwarded_packets` assertion requires `on`, a nonempty `contains` list of strings, and
`timeout`. It polls `dump` with `{"kind": "dogstatsd_forwarded"}` until every string's UTF-8 bytes equal a whole
forwarded packet. Partial packet contents do not match. One packet can satisfy repeated identical entries; this
assertion does not count packets. If any dump returned a valid snapshot, a timeout fails and lists the missing
entries and up to ten observed packets as escaped, lossy UTF-8. Otherwise it errors with the last dump error.
Diagnostics are capped at 4096 bytes, as for `intake_has_metric`.

The native `intake_has_service_check` assertion requires `on`, an exact `name`, and a positive `timeout`.
Optional `status` is an integer from 0 through 255, `hostname` matches exactly, and `tags` is a subset list that
allows extra tags. Omitted criteria accept any value; `tags` defaults to empty. It polls `dump` with
`{"kind": "service_checks"}` until one service check satisfies every criterion, without combining checks.
At timeout, any decoded snapshot makes the assertion fail; otherwise it errors with the last dump error. Failure
messages list up to ten checks with the expected name and each one's first failed criterion, or up to ten other
observed names. Messages are capped at 4096 bytes. A subject exit does not end polling early.

The `observe` phase gives exited participants up to five seconds to finish
capturing output; it also runs after a setup failure if any participant started. Running participants are skipped, and
case cancellation ends the wait. The runner snapshots observations, stops observation tasks, then reclaims resources.

The `datadog-intake` participant runs one process named `intake`:

| Subject OS | Realization | Artifact role | Local default |
| --- | --- | --- | --- |
| Linux | Linux container | `tools-linux-image` | `saluki-images/correctness-tools:latest` |
| macOS | Host process group | `intake-host-binary` | `target/release/datadog-intake` |
| Windows | Windows container | `tools-windows-image` | `saluki-images/correctness-tools:testing-windows` |

In containers, its exit and output come from the container. On macOS, they come from the waited process and its
stdout and stderr pipes. The Windows tools image holds `C:\tools\datadog-intake.exe` on the Agent LTSC base already
used by the Windows ADP image. The intake serves two endpoints:

| Endpoint | Kind | Listen variable | Container default |
| --- | --- | --- | --- |
| `api` | HTTP | `INTAKE_HTTP_LISTEN_ADDR` | `0.0.0.0:2049` |
| `dogstatsd_forward` | UDP | `INTAKE_DOGSTATSD_LISTEN_ADDR` | `0.0.0.0:9125` |

Set these variables in `config.env` to pin listener addresses. Each must be a socket address with a nonzero port.
In containers, the host must be unspecified (`0.0.0.0` or `[::]`), so other participants can reach it. On macOS,
unset listeners get assigned loopback ports; a set listener may bind loopback or an unspecified host. Pinned host
ports must be outside the assigned pool, `20000–29999`.
Both endpoints declare readiness as HTTP 200 from `api` at `/ready`: the intake binds UDP before starting HTTP.
Connections wait for that evidence before starting their consumer. Each intake stores its own telemetry.

The intake's `dump` operation accepts one `kind`: `metrics`, `events`, `service_checks`, `spans`, `trace_stats`,
or `dogstatsd_forwarded`. The first five return JSON unchanged from `/metrics/dump`, `/events/dump`,
`/service_checks/dump`, `/traces/dump_spans`, and `/traces/dump_stats`, respectively.
`dogstatsd_forwarded` requests `/dogstatsd-forwarding/dump` and returns
an array of packets, each an array of byte values decoded from the intake's base64 wire format.
The operation declares only `api`. Unknown kinds and extra arguments
are harness errors before any request. Non-success HTTP status, invalid JSON and invalid packet encoding are product
errors; messages omit response bodies. The operation is offered on every realization. Through a courier route,
the HTTP request runs inside the server participant, including in Windows containers.

The `fixture` participant runs a case-supplied command in a Linux container from `tools-linux-image`, which includes
Python 3. Its one process, `fixture`, uses the container's exit and output. It accepts the `files/` directory name;
regular files land under `/var/lib/panoramic/files` with mode 0644, available through its `files_dir` fact.
`config.params.command` is a required nonempty list of strings used as the entrypoint. Facts and other participants'
container IDs are resolved before creation. Optional `config.params.endpoints` maps names to exactly
`{unix: /absolute/path}`. Each declares a Unix-stream endpoint with socket-existence readiness. Consumed sockets
must lie beneath the serving participant's `shared_dir`. Other parameters and endpoint fields are rejected.
The `liveness-container-tags` case shows a fixture created after its subject but started before it.

The `agent-adp` Linux container definition uses `agent-adp-linux-image`. Its environment defaults match the legacy
runner's port isolation: command API 55001, expvar 55000, APM 58126, process command API 56062, Agent IPC 55004,
DogStatsD UDP 58125, ADP APIs 55100/55101 and Agent OTLP 54317/54318. The GUI is disabled. Case environment values
replace those defaults. A supplied `datadog.yaml` also suppresses defaults for keys it sets: explicit environment
values win over the file, and file values win over type defaults. A file's zero DogStatsD port disables UDP rather
than being replaced by a default.

Linux `agent-adp` and `agent` accept `config.files.datadog.yaml`, placed at `/etc/datadog-agent/datadog.yaml`.
Use `template: true` for early facts and connection references. Definitions read the file's listener keys while
planning; endpoint, connection, or late references in a key they read are errors naming the file and key.
Other late references remain valid.

The Linux-only `agent` type uses `agent-adp-linux-image` with ADP disabled and observes only the Agent process.
It retains the image's defaults and shares file handling and connections with `agent-adp`. Its operations are
`cli_agent` and `config_dump`. The `runtime` dump view returns `agent config`, decoded from YAML to JSON.
The Agent rejects the `source` view before execution; it offers only runtime configuration.

On Linux, `agent` exposes `agent_otlp_grpc` and `agent_otlp_http` from the corresponding
`DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT` and `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT` variables,
or `otlp_config.receiver.protocols.grpc.endpoint` and `otlp_config.receiver.protocols.http.endpoint` in
`datadog.yaml`. Environment values win over the file. These listeners have no default; empty, loopback and port-zero
values disable the endpoint. Connections keep the configured ports and wait for TCP acceptance. Linux `agent-adp`
doesn't expose the Agent's OTLP receivers; its `otlp_grpc` and `otlp_http` belong to ADP.

The `agent` type also accepts `otel-config.yaml`, placed at `/etc/datadog-agent/otel-config.yaml`.
Enabling the embedded collector through `DD_OTELCOLLECTOR_ENABLED` or `otelcollector.enabled` in `datadog.yaml`
requires that file. Its `receivers.otlp.protocols.grpc.endpoint` declares `otel_otlp_grpc`: a non-loopback
`host:port` with a nonzero port, gated on TCP acceptance. It cannot share a port with `agent_otlp_grpc`.
Only the receiver address must be available while planning; exporter URLs may use connection references.

Both types accept `config.params.host_view: true` on Linux. In containers this mounts `/proc`, `/sys/fs/cgroup`,
and the Docker socket read-only and sets the corresponding Agent host paths. Rootless Docker can reject file placement
with a `remount-ro /host/proc: operation not permitted` error. Removing host view can diagnose other behavior, but cannot
prove container-origin enrichment.

The Linux-only `millstone` type runs `baseline` and `subject` senders in one container from `tools-linux-image`.
Both use the seed in the required `millstone.yaml` template. Its target is a reference to the `target` connection's
URL, optionally followed by a literal path starting with `/`, such as an OTLP service path. The baseline uses the
`baseline_target` URL with the same suffix. Both connections are required; suffixes cannot contain references.
In Linux containers, senders stay alive after `Millstone finished sending` so their container identity remains
available through flushing. Each has its own exit record and log. The baseline capture is collected into `traffic/`;
the subject capture manifest is kept under `load/collected/` when the participant is named `load`, allowing
payload-digest comparison without retaining a second payload stream. `dsd-plain` and `dsd-origin-detection`
demonstrate this composition.

In Linux containers, `agent-adp` accepts the directory configuration names `files/` and `cont-init.d/`.
Files under `files/` land beneath `/var/lib/panoramic/files`, available through the `files_dir` fact, with mode 0644.
Files under `cont-init.d/` become executable hooks at `/etc/cont-init.d/50-<name>` with mode 0755, before the image's
`99-agent-data-plane.sh` hook. Nested hook paths are rejected.
Windows containers accept `files/`, placing regular files under `C:\panoramic\files` with mode 0644 and backslash
paths. The Windows `files_dir` fact names that directory. `cont-init.d/` is Linux-only.
macOS host process groups reject case files, including files in OS overlays.

The definition derives these endpoints from the effective environment and, on Linux, the supplied configuration file:

| Endpoint | Kind | Environment key | Conditions and readiness |
| --- | --- | --- | --- |
| `dogstatsd_udp` | UDP | `DD_DOGSTATSD_PORT` | Requires nonzero port, enabled DogStatsD and a non-loopback bind; ADP's listener output supplies readiness |
| `dogstatsd_tcp` | TCP | `DD_DOGSTATSD_TCP_PORT` | Requires ADP-owned DogStatsD, a nonzero port and a non-loopback bind; defaults to 0 (disabled); waits for TCP acceptance |
| `dogstatsd_uds` | Unix datagram | `DD_DOGSTATSD_SOCKET` | Defaults to `/var/run/datadog/dsd.socket` on Linux; empty disables it; waits for the socket |
| `dogstatsd_uds_stream` | Unix stream | `DD_DOGSTATSD_STREAM_SOCKET` | Nonempty enables it; waits for the socket |
| `dogstatsd_pipe` | Named pipe | `DD_DOGSTATSD_PIPE_NAME` | Windows only; nonempty enables it when DogStatsD is enabled; reached through the courier at `\\.\pipe\<name>`; gate sends on ADP's `listen_addr:"npipe://<name>"` log line |
| `apm_tcp` | HTTP | `DD_APM_RECEIVER_PORT` | Linux only; requires APM explicitly enabled and a nonzero port; waits for TCP acceptance |
| `apm_uds` | Unix stream | `DD_APM_RECEIVER_SOCKET` | Linux only; requires APM explicitly enabled; defaults to `/var/run/datadog/apm.socket`; empty disables it; waits for the socket |
| `api` | HTTP | `DD_DATA_PLANE_API_LISTEN_ADDRESS` | Requires ADP and a non-loopback host; waits for TCP acceptance |
| `secure_api` | TCP | `DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS` | Requires ADP and a non-loopback host; waits for TCP acceptance; the listener uses TLS |
| `otlp_grpc` | gRPC | `DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY` | Requires ADP and its OTLP pipeline, a nonzero port and a non-loopback host; waits for TCP acceptance |
| `otlp_http` | HTTP | `DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY` | Requires ADP and its OTLP pipeline, a nonzero port and a non-loopback host; waits for TCP acceptance |
| `agent_cmd` | TCP | `DD_CMD_PORT` | Nonzero enables it; waits for TCP acceptance |
| `agent_ipc` | TCP | `DD_AGENT_IPC_PORT` | Positive enables it; waits for TCP acceptance |

ADP owns DogStatsD when `DD_DATA_PLANE_ENABLED` and `DD_DATA_PLANE_DOGSTATSD_ENABLED` are true; otherwise the
Agent owns it. The master ADP switch defaults to false, its DogStatsD switch to true, and `DD_USE_DOGSTATSD` to true.
Disabling DogStatsD removes its socket endpoints too. ADP-owned listener keys prefer the `ADP_DD_` spelling over
`DD_`; Agent-owned listeners use only `DD_`. Agent-owned UDP readiness follows container startup.
The UDP and TCP endpoints require `DD_DOGSTATSD_NON_LOCAL_TRAFFIC=true` or a non-loopback `DD_BIND_HOST`. These keys
follow the same owner precedence. With both unset, DogStatsD binds loopback and both endpoints are omitted.
ADP API addresses use `tcp://host:port`; a loopback host is a configuration error, and port 0 disables the endpoint.
Invalid listener values name the configuration key in their errors.

On Linux, both `agent` and `agent-adp` declare APM endpoints only when `DD_APM_ENABLED` or `apm_config.enabled`
explicitly enables APM. ADP owns these receivers when `DD_DATA_PLANE_ENABLED` and
`DD_DATA_PLANE_EXPERIMENTAL_APM_ENABLED` are true; otherwise the trace-agent owns them. ADP-owned values prefer
`ADP_DD_` over `DD_`, then the file. Setting `ADP_DD_APM_RECEIVER_PORT` also suppresses the injected Core Agent
APM port default, whose config-stream value would otherwise mask the ADP-only override.
The TCP endpoint is omitted when `bind_host` is loopback unless
`apm_config.apm_non_local_traffic` enables non-local traffic. With neither key set, APM binds every interface.

ADP's OTLP endpoints require `DD_DATA_PLANE_ENABLED=true` and an enabled OTLP pipeline. The OTLP switch defaults
to false; `ADP_DD_DATA_PLANE_OTLP_ENABLED` takes precedence over `DD_DATA_PLANE_OTLP_ENABLED`. The receiver
address keys use the same `ADP_DD_` over `DD_` precedence. Proxy mode does not change endpoint availability.
Addresses use `host:port` without a scheme. A loopback host omits the endpoint, including the defaults
`localhost:6317` and `localhost:6318`; set a non-loopback address to reach the receiver. Port 0 also omits the endpoint.
Malformed addresses fail configuration with an error naming the key. The HTTP endpoint uses HTTPS when both
`DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_CERT_FILE` and `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TLS_KEY_FILE`
are nonempty, with the same `ADP_DD_` precedence. Setting only one is a configuration error. HTTP-status readiness
on a TLS endpoint is rejected; TCP-acceptance readiness remains supported.

On every realization, `agent-adp` accepts an optional `connect: {intake: <participant>.api}` connection. It accepts
HTTP and delivers its URL as both `DD_DD_URL` and `DD_APM_DD_URL`, gating startup on the intake's ready route.
On macOS, both the Agent and ADP receive these keys through their shared environment.
Only Linux containers accept the optional `dogstatsd_forward` and `docker_api` connections:
`connect: {dogstatsd_forward: <participant>.dogstatsd_forward}` accepts UDP and delivers `DD_STATSD_FORWARD_HOST`
and `DD_STATSD_FORWARD_PORT` from the target's host and port. `connect: {docker_api: <participant>.<endpoint>}`
accepts a Unix stream and sets `DOCKER_HOST` to its Unix URL. Both gate startup on the server's readiness.
The Linux recipe uses the image's command and overlays the `data-plane` and `agent` s6 services. Each
finish script records the exit before running the shipped body, preserving ADP's two-second restart after failure and
disablement after a clean exit. Each run script merges stderr into stdout, and its logger appends to `adp.log` or
`agent.log` while retaining the lines on container stdout.
These merged process logs include the run and finish scripts' own messages. Container stdout and stderr are also
captured separately. The default ten-second stop grace covers the measured roughly 2.6-second supervisor shutdown.

The `agent-adp` definition offers the same seven operations on all three realizations:

| Realization | ADP program | Agent program |
| --- | --- | --- |
| Linux container | `/opt/datadog-agent/embedded/bin/agent-data-plane` | `/opt/datadog-agent/bin/agent/agent` |
| Windows container | `C:\adp\agent-data-plane.exe` | `C:\Program Files\Datadog\Datadog Agent\bin\agent.exe` |
| macOS host process group | The participant's own `adp` program, with `-c datadog.yaml` | The participant's own `agent` program, with `-c .` |

On macOS, configuration arguments are relative to the participant directory. Each command receives the declared
process's environment over the group environment, including the Agent's run path and force-enable setting.

| Operation | Arguments | Command and result |
| --- | --- | --- |
| `cli_adp` | `args`: list of strings | ADP with those arguments; returns `output`, stdout followed by stderr using lossy UTF-8. |
| `cli_agent` | `args`: list of strings | Agent with those arguments; returns `output` the same way. |
| `config_dump` | `view`: `source` or `runtime` | ADP `config --json`, adding `--runtime` for runtime view; returns parsed JSON stdout. |
| `config_set` | `key`, `value`: strings | Agent `config set <key> <value>`; returns an empty object. |
| `dogstatsd_capture` | `duration_ms`: unsigned integer | ADP `dogstatsd capture --duration <ms>ms --compressed false`; returns the reported capture `path`. |
| `dogstatsd_replay` | `path`: string | ADP `dogstatsd replay --file <path>`; returns an empty object. |
| `dogstatsd_stats` | `seconds`: unsigned integer | ADP `dogstatsd stats --duration-secs <n>`; returns `metrics`, the names in the table's first column after its header. |

Arguments are exact: missing, unknown or wrongly typed keys are harness errors. Exit codes 126 and 127 mean the
command could not run and are harness errors; other nonzero exits are product errors. An abandoned exec at its
limit is a harness error. Invalid JSON from `config_dump`, a missing or incomplete capture path, and a malformed
statistics table are product errors. CLI failure messages omit output, which may contain secrets. In particular,
ADP CLI output is never quoted on a nonzero exit. `config_set` includes trimmed stderr capped at 512 bytes on
product failure; it uses the Agent's own client for its runtime configuration API.

On Windows, the `agent-adp` definition runs the Agent and ADP in one container through its own entrypoint, using
`agent-adp-windows-image`. It declares the Linux container listener table and fixed defaults above, except APM and Unix
sockets: their paths default to empty, and nonempty paths are rejected. Windows rejects `ADP_DD_` variables;
listeners use the `DD_` keys. The named-pipe name must contain no slash, backslash or NUL.
Loopback filtering and the DogStatsD non-local-traffic/bind-host rule match Linux.

The `PANORAMIC_` environment-variable prefix is reserved. Plan compilation rejects keys with this prefix in the
case's effective environment after OS overlays, in type defaults, and in connection deliveries. Errors identify
whether the case, type default or connection set the key.

Every native Linux container receives three reserved variables: `PANORAMIC_PARTICIPANT` names the participant,
`PANORAMIC_EXIT_DIR` names its exit-record directory, and `PANORAMIC_LOG_DIR` names its process-log directory.
Recipes that set any of these variables fail creation. The backend prepares the directories before start. The
Linux backend always shares the host PID namespace. Its cgroup namespace is private unless
`realization.options.host_cgroup_namespace` is true.

Every native Windows container receives the same three reserved variables. The backend creates each participant's
`C:\airlock\<participant>\{exit,log,shared}` directories in its container's own filesystem before start; `shared_dir`
resolves to that container-local `shared` directory. Recipes that set reserved variables, declare host mounts or
publish runner-host ports are rejected. The courier occupies the reserved path `C:\panoramic\courier.exe`.
Steps reach Windows endpoints through the courier inside the server participant.

`agent-adp`, `agent`, `datadog-intake` and `millstone` support Linux Kubernetes pods, with the same listener and
process declarations as their Linux containers. Each participant runs in its own pod and has a same-named Service.
Only the intake exposes a pod operation, `dump`. Agent pods accept `datadog.yaml`, but reject `extra_hosts`, `files/`
and `cont-init.d/`. Set `config.params.host_view: true` to mount the node's `/proc`, cgroup filesystem and containerd
socket read-only. Host view defaults to false. Pod host view requires `realization.options.host_pid: true`, so peer
credential PIDs refer to the mounted node process tree. Planning rejects missing or false `host_pid` and names the
participant. `host_pid` and `cri_socket` remain realization options; they do not add mounts.

Both millstone senders share one container named `millstone`, so they have the same container identity. Their
configuration files are written after start, once the pod's identities exist. Unlike Linux-container senders, pod
senders exit after sending; use `exits_with` followed by `wait` for the targets' flush window. Pods collect no input
capture.

Pod commands wait for their late files before running. The backend writes those files once all referenced
identities exist. Shared files and exit records remain readable through the kind node after a pod exits. Container
output is merged into `<log_dir>/<participant>/containers/<container>.log`; named process files keep their usual
paths.

For any selected pod case, native or legacy, `panoramic run` creates or reuses the cluster named by
`--kind-cluster-name` (default `saluki-correctness`) and loads its image references. Native pods use the `kubeconfig`
context `kind-<cluster-name>`, regardless of the current context. Cluster setup failure errors pod cases before
participant creation. Use `--no-delete-kind-cluster` to retain the cluster after the run.

The `agent-adp` definition declares `default_log_file` for its Linux and Windows containers only, so a case that
runs on macOS writes the path out or uses its core-provided `shared_dir` fact.

Step fields can also reference a participant's late facts as `<participant>.late.<fact>`: `ip` on every realization,
`container_id` on containers and pods, and `pod_uid` on pods only. They exist once the participant is created or running.
Step fields accept only facts and late facts of explicitly named participants.

Environment values must be YAML strings. Optional `config.os` overlays replace base values on the selected OS.
Environment variable values and string parameters, at any depth, can also contain references, including to the
participant itself as `self`. Variable names and parameter keys can't. On every OS in `subject_os`, a fact must exist
for the referenced participant's realization, and panoramic substitutes it before the participant type reads the
configuration. A host process group's `ip` is `127.0.0.1` for itself and for other host process groups on the same OS,
and is substituted the same way. Another participant's `container_id` is allowed when that participant is a Linux or
Windows container. A participant's own late facts are allowed only in environment variables, and only when its type
substitutes them after start. Core gives the type one canonical placeholder spelling, whether the case used `self`
or the participant's own name and regardless of spacing. Other late facts are rejected. An endpoint reference needs
an endpoint the participant serves and a supported route from the referencing participant. A connection reference
needs a matching `connect` key. Planning stores each connection's and endpoint reference's address as seen by its
holder; the configuration keeps the reference text until recipe rendering. `host` and `port` apply to network
endpoints, `path` to sockets and pipes, and `url` to every kind.

In Linux containers, `agent-adp` substitutes its own `ip` through
`/etc/cont-init.d/10-panoramic-late-substitution.sh`. The init hook runs before the products and case hooks,
writing resolved values into both services' environment directories. An empty address fails the hook and stops
the container. Values in `ADP_DD_*` variables cannot use this placeholder: the image copies those overrides
unresolved later; use the `DD_` spelling instead. Processes started through operations (Docker exec) inherit the
container's original environment, so they still see the placeholder.

Windows containers substitute the same own-`ip` placeholder through `C:\adp\participant-late.ps1`.
The entrypoint dot-sources the script before starting either product, so resolved process environment variables
reach both. It finds the container's primary IPv4 address using hostname resolution; failure to find an address
stops the entrypoint before any exit record is written. Process observation reports that exit as undetermined.

`agent-adp` accepts `config.params.extra_hosts` in Linux and Windows containers: a map from host names to the string
`self`. The late-substitution hook maps each name to the participant's own address in `/etc/hosts` on Linux or
`C:\Windows\System32\drivers\etc\hosts` on Windows. Names must be nonempty,
contain only ASCII letters, digits, dots and hyphens, and not be `localhost` (case-insensitive, including a trailing
dot). Other targets and value shapes are rejected. macOS host process groups reject nonempty mappings.

```yaml
participants:
  adp:
    type: agent-adp
    subject: true
    config:
      env:
        DD_API_KEY: "00000000000000000000000000000000"
        DD_DATA_PLANE_ENABLED: "true"
        DD_BIND_HOST: foo.local
      params:
        extra_hosts:
          foo.local: self
```

The hook is installed only when an environment value references the participant's own `ip` or `extra_hosts` is
nonempty.

A declared configuration name ending in `/` requires a directory source; other names require regular files.
Directories expand recursively into regular files in sorted path order. Empty directories contribute nothing.
Symlinks and other file kinds inside a directory are rejected. A directory cannot use `template: true`.
The participant type chooses destination paths and file modes.

A `config.files` source can be a map with `from` and `template: true`. Panoramic reads that file when the case
loads: it must be a regular UTF-8 file, and every double opening brace in it must start a reference.
Other sources are read verbatim during planning; creation uses those saved bytes even if a source changes.
On every OS in `subject_os`, references in the files a participant uses after its overlay follow the rules for
environment variables, except for late facts. A Kubernetes pod's template files
may reference any late fact the referenced participant has, including the pod's own, except a host process group's
`ip`, which only participants on that host process group's realization can use. On other realizations, a template
file may not reference the participant's own late facts, except for a host process group's fixed `ip`.

Panoramic orders participants before running a case. A participant is created after every other Linux or Windows container
whose `container_id` its configuration references. It starts after every participant it connects to. A Kubernetes pod
also starts after every other container or pod whose `ip` its template files reference, and after every other pod whose
`container_id` or `pod_uid` they reference. Own late facts are supplied by init hooks or late files; a host process
group's fixed `ip`, facts and endpoint addresses are available before creation, and steps run after all participants
start. Use `connect` when a participant needs another running first. A cycle in either order on any OS in `subject_os`
fails the case. `panoramic list` prints both orders, grouping independent participants.

Durations in a participant case need a unit: write `30s`, not `30`.

Steps name the participant they act on with `on`; `wait` names none. Step fields differ from the integration schema:
steps use `step:` instead of `assertion:`, and `file_contains` requires `pattern`.
Each backend declares its accepted `realization.options`. The loader rejects unknown options and non-boolean
values, naming the participant, realization kind, and option. Linux containers accept `host_cgroup_namespace`
(default false); true exposes the host's cgroup paths to the participant. Pods accept `host_pid` and `cri_socket`,
both defaulting to false. `host_pid` shares the node's PID namespace. `cri_socket` requires the node's containerd
socket to be mounted by the recipe; it adds no mount. Before creating a pod, the backend checks every recipe host-mount
source on the node and rejects missing sources. HostPath volumes pin the observed file, directory or socket type;
`cri_socket` requires a socket. Participant recipes own host mounts and their destination paths. Host process groups
and Windows containers accept no options.
A `config.files` source must be a nonempty path relative to the case directory that exists when the case loads.
Parent-directory components must stay within the case directory; symlink targets may be outside it.

`panoramic list` shows each participant case's layout. A case whose layout is invalid on any OS it lists in
`subject_os` fails discovery for `list` and `run`, whatever subject OS is selected.
Plan compilation checks fact availability for both steps and configuration on every OS in `subject_os`.
A referenced fact missing for a participant's realization fails planning and therefore discovery.
It also checks every declared process name: it must be a plain file name, not empty, `.` or `..`, and contain no
`/` or `\`. When a type declares process specifications for a realization, planning validates its exit and output
channels too: specifications must name declared processes, every process needs exactly one specification, record
and output file names must be plain and unique, and only one process may own each container channel. Output files
must not collide with host capture paths.
A realization with no specifications can still plan, but native preparation requires them.

`panoramic list` also prints each artifact role a case needs on the selected subject OS, with the
reference it resolves to and whether it comes from a manifest or the role's local default. Roles are stable names
for the images, binaries and installs its runner uses. Use `--artifacts <file>` to
read a TOML manifest:

```toml
schema = 1

[roles]
agent-adp-linux-image = "registry.example/agent-adp:dev"
fixup-image = "registry.example/alpine:3"
```

Omitted roles use their local defaults: for example, `agent-adp-linux-image` defaults to
`saluki-images/datadog-agent:testing-devel`. A role no participant type, backend or typed schema declares fails the listing;
a declared role the selected cases don't need is ignored. Relative binary and install paths resolve against the
working directory, whether supplied by a manifest or a local default. Native execution always verifies and uses
these bindings. With `run --artifacts <file>`, typed runners also verify and use them; without the flag,
those runners use case files, harness defaults and image flags or their environment variables.

Typed correctness cases map their containers to these roles:

| Case and subject OS | Container or process | Artifact roles |
| --- | --- | --- |
| Correctness, Linux, Docker or Kubernetes-in-Docker | Baseline and comparison | `agent-adp-linux-image` |
| Correctness, Linux, Docker or Kubernetes-in-Docker | Datadog intake and millstone | `tools-linux-image` |

Each case also needs the roles its backend declares: Linux containers need `fixup-image` and `courier-linux-binary`,
the static courier. On aarch64 and x86_64, its local default is `target/<arch>-unknown-linux-musl/release/panoramic-courier`,
where `<arch>` is the architecture panoramic was built for. Verification checks that the build matches the Docker
daemon's architecture. Other runner architectures require a manifest binding.
`make build-courier-linux` builds it, installing the Rust target with `rustup` on first use.
The `test-integration`, `test-integration-quick`, `test-correctness` and `test-correctness-case` targets build it first.
For a Docker daemon of another architecture, build with `COURIER_LINUX_TARGET=<arch>-unknown-linux-musl` and bind
`courier-linux-binary` to that build in a manifest.

Correctness cases run only for subject OS `linux`; their roles appear in text listings, `artifacts verify`/`pull`,
and `run --artifacts` verification only when you select Linux. Only runnable cases contribute artifact roles.

Both correctness targets bind to `agent-adp-linux-image`, as CI supplies the same bundled image to both sides.
Locally, correctness files declare `saluki-images/datadog-agent:testing-release`, while that role defaults to the
integration image, `saluki-images/datadog-agent:testing-devel`. These are release and debug builds, respectively.
With `run --artifacts`, both targets use the role binding: locally, `:testing-devel` unless the manifest binds it.
Without the flag, runners keep the images the files declare, subject to image overrides.

Use `panoramic run --artifacts <file> -d <cases>` to verify all selected artifact roles before any case starts.
Native cases' roles are verified on every run, with or without `--artifacts`. Verification follows
`-t` selection and the check that the host can provide the cases. Unavailable cases stay `errored`/`setup` rows and
contribute no artifacts to verification. Any verification failure exits 2 before creating the log directory or
`run.json`, starting kind setup or running a case. The report goes to stderr, including in TUI mode, leaving stdout
for `-o json`.

When verification passes, `run.json`'s `artifacts` lists each verified binding as `{role, reference, identity, kind}`
in role-name order. `identity` is the image ID for images, the SHA-256 of the file for binaries, and the SHA-256 of the
main binary for installs. `reference` is preserved as written. Roles needed only by skipped cases or cases the host
can't provide aren't verified and don't appear. With `--artifacts`, the list is empty (`[]`) when no bindings need
verification. Without the flag, `artifacts` lists verified native bindings, or is `null` when nothing was verified.

The flag has no environment fallback. Legacy flags and their environment variables can accompany `--artifacts`,
but each explicit value must name the same reference as the binding of the role it feeds:

| Flag | Environment variable | Artifact role |
| --- | --- | --- |
| `--image-override datadog-intake=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `tools-linux-image` |
| `--image-override millstone=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `tools-linux-image` |
| `--image-override baseline=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `agent-adp-linux-image` |
| `--image-override comparison=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `agent-adp-linux-image` |
| `--alpine-image` | `PANORAMIC_ALPINE_IMAGE` | `fixup-image` |

Only values explicitly supplied by a flag or environment variable are compared; defaults don't count as inputs.
Images are compared as strings: `alpine` differs from `alpine:latest`.
The comparison uses every planned case in scope, before `-t` selection, and ignores inputs for roles no case needs.
Image override names must be unique and match a correctness container in scope.
Several names feeding one role must all agree with its binding.

Panoramic reports every disagreement, including the role, bound reference and whether it came from the manifest or
a local default. Any disagreement exits 2 before verification, log directory creation or `run.json`. Agreeing
inputs leave the binding as the only effective input. CI can keep its legacy variables alongside a manifest that
binds the same references, including `fixup-image` for `PANORAMIC_ALPINE_IMAGE`.

The image roles feed the containers listed in the case table above. `adp-host-binary` supplies the host ADP executable;
`agent-host-install` supplies the Core Agent executable at `bin/agent/agent` beneath that directory. `fixup-image`
supplies the shared-volume permission fix-up container. `courier-linux-binary` supplies the static courier executable
for Linux containers. Kind loads the runners' bound images into its cluster.
Kind options, `-l` and TUI options remain available. In `list --json`, `images` and `runtime` report
the case files' images and runtime regardless of `--artifacts`, while `roles` shows the bound references and their sources.

Use `panoramic artifacts render --set <role>=<reference> … -o <file>` to write a manifest, replacing an existing
file. Repeat `--set` to bind more roles; omitting it writes a manifest binding nothing. Rendering checks bindings
against every declared role, whatever cases a run selects. A duplicate role, empty reference or unknown role writes
nothing and exits 2, leaving an existing output file untouched. References may contain `=`: only the first `=`
separates the role from its reference.

**Artifact manifests in CI.** Every panoramic CI job renders a manifest and runs with `--subject-os` and
`--artifacts`, so its `run.json` records what it tested in `artifacts`. Linux integration and correctness jobs bind
`agent-adp-linux-image` and `tools-linux-image` to the pipeline's images and `fixup-image` to
`registry.ddbuild.io/alpine:latest`, then run `panoramic artifacts pull` before testing.
Linux integration and correctness jobs bind `courier-linux-binary` to the courier from `build-panoramic-binary`.
`make test-integration-macos-ci` binds the ADP and intake binaries and Agent install it builds and provisions;
`ci/tooling/windows-integration-tests.ps1` binds the ADP and tools images and courier binary it builds.
Both Windows images use `WINDOWS_ADP_BASE_IMAGE`, the Agent LTSC image already pulled by the job; local invocations
without that variable fall back to `mcr.microsoft.com/windows/servercore:ltsc2022`.
Image overrides must agree with the manifest. Every selected correctness case must use a `saluki-images/`
baseline, because both sides take `agent-adp-linux-image`.

Use `panoramic artifacts verify -d <cases> [--subject-os linux|macos|windows] [--artifacts <file>]` to check every
artifact the selected cases need. The subject OS defaults to the host's OS. Verification prints each
role once, sorted by name, with its reference, source and identity or failure:

- An image must exist on the Docker daemon, and the daemon must run containers of the image's OS (Linux images on
  a Linux daemon, Windows images on a Windows daemon). Its identity is the image ID.
- A binary must be a file, executable on Unix. Its identity is the SHA-256 of its contents.
- A container binary must be a file, executable on Unix, and a static 64-bit little-endian ELF executable for the
  Docker daemon's architecture, x86_64 or aarch64, on a Linux daemon. Its identity is the SHA-256 of its contents.
- An install must be a directory containing the role's main binary, a file executable on Unix. Its identity is the
  SHA-256 of that binary. For `agent-host-install`, the main binary is `bin/agent/agent` inside the install.

File checks follow symlinks. Verification never pulls images or creates containers, and contacts the Docker daemon
only when an image or a container binary is needed. It reports every verification failure and exits 2 if any fails,
or if no test directory is given, the manifest is invalid or cases fail discovery or planning. It exits 0 when all
artifacts verify, including when none are needed. Participant and correctness cases contribute artifact roles.

Use `panoramic artifacts pull -d <cases> [--subject-os linux|macos|windows] [--artifacts <file>]` to fetch only the
images those cases need, in role-name order. Check binary and install roles with `verify`;
`pull` reports only images. Images are always pulled again, even if present, so moving tags are refreshed. A failed
pull fails even if a local copy exists: use `verify` for locally built images. An untagged reference pulls `latest`.
An image for an OS the daemon doesn't run fails without pulling. An image role with no reference fails without
contacting the daemon.
Each pull has a five-minute limit; the inspection afterwards has a 30-second limit. Either timeout fails that image
and all remaining images without contacting the daemon again; other failures affect only that image. Pulling logs
each reference and role before starting, and prints each role's reference, source and image ID or failure under
`Images needed on <os>:`. When no images are needed, it prints `No images needed on <os>.` and exits 0.
Successful pulls exit 0; any failed pull or inspection, invalid selection or manifest, or discovery or planning
error exits 2. Registry access is unauthenticated, and platform selection follows the daemon rather than
`--subject-os`. Test runs also pull images during container creation.

Connections and endpoint overrides are checked the same way: on every OS in `subject_os`, each `connect` key
must be an endpoint the participant consumes, the target must serve the named endpoint with its configuration,
and the endpoint's kind must be one the consumer accepts. A connection also fails when its participants'
realizations have no route for the endpoint's kind. A participant can connect to or reference its own endpoints,
including named pipes and, outside Windows containers, Unix sockets. Startup dependencies link distinct
participants. Between different participants, network endpoints connect the same realization, or a host process
group to a Linux container. Unix sockets need the same realization and can't use Windows containers.
Named pipes are reachable only within their own participant.

Every participant has a `shared_dir` fact: its directory for sockets it serves. It is
`/airlock/<participant>/shared` in Linux containers and pods, `C:\airlock\<participant>\shared` in Windows
containers, and `<state dir>/<participant>/shared` for host process groups. Planning chooses a short per-case
state directory under `/tmp`. A Unix socket another participant connects to or references must lie under the
server's `shared_dir` and fit the OS limit: 103 bytes on macOS, 107 on Linux. Other sockets may lie anywhere.

On every OS in `subject_os`, panoramic resolves each connection to an address as the consumer sees it and sets
its type's delivered environment variables, replacing the type's defaults. Containers of one kind reach each
other, and pods reach each other, by participant name and the server's port. Host process groups use
`127.0.0.1` and the server's port. A host process group reaches a Linux container on `127.0.0.1` and a runner-host
port, allocated while planning from one pool per invocation below the OS's default ephemeral port range.
An endpoint reference in a host process group's configuration to a Linux container's network endpoint reserves a
runner-host port the same way. Every connection and reference that reaches one endpoint shares its port.
A host process group's type may also have panoramic assign a listener port from this pool, delivered through
its environment defaults. A case pins the port by setting the type's variable in `env`.
No two network listeners of a case's host process groups may share a port on one transport (TCP or UDP).
A fixed or pinned host port must be outside the pool, `20000–29999`; avoiding collisions across concurrent
cases is the case author's responsibility.
Step routes depend on the operation. Probes to containers and pods, and raw sends to containers, use the courier
inside the server participant: a port proxy can accept a connection even when the server has no listener. Raw sends
from the runner host to pods fail planning; use a sending participant. HTTP and gRPC
requests to Linux containers and pods use published or forwarded ports and must observe a server response.
HTTP requests to Windows containers run through the courier inside the server participant, without published ports.
Host process group endpoints are reached directly. Request/response step needs and operations' declared HTTP
endpoints allocate runner-host ports when their route needs them. Connections and configuration references allocate
ports as described above.

Run one panoramic invocation per runner host and Docker daemon at a time: each invocation allocates
deterministically from 20000. Linux container participants publish exactly their planned ports on `127.0.0.1`.
If a port is held on the daemon, startup fails with a message naming the participant's ports.
On a VM-backed daemon, a port held on the runner host isn't detected; the runner host may reach the process
holding it. Each CI job has its own Docker-in-Docker daemon. Drivers built from an isolation group require
explicit host ports; drivers built with `Driver::from_config` also support ephemeral publication.

A Unix socket is reached at the path its server listens on. Every served socket path must be absolute for its
participant's OS, even when nobody consumes it. It may use fact references, but not references resolved later,
such as late facts. Every listener port must be from 1 to 65535. `config.endpoints` gives the port of a listener
whose port the type can't derive from configuration. Overriding any other endpoint fails, as does leaving
such a port out.

A case must not set, in `config.env` or the overlay for that OS, an environment variable that one of its
connections delivers. Two connections of one participant must not deliver the same variable, and a connection
must not deliver a variable carrying an assigned listener port.
Every required consumed endpoint must be connected, and each `config.endpoints` key must name an endpoint
the participant serves. The `agent-adp` definition derives listener ports from its environment and Linux configuration file; configure those
ports through the listener's environment key.

On macOS, the `agent-adp` definition declares every listener and lets core assign its port unless the case sets
its environment key. Agent-internal listeners have macOS-only endpoint declarations so they also receive assigned
ports. When ADP OTLP proxying is enabled, the Agent's proxy gRPC endpoint replaces its normal OTLP gRPC endpoint.
The recipe keeps the gRPC protocol configured using ADP's gRPC address unless the case supplies that setting.
The definition rejects `ADP_DD_` variables, which require Linux service scripts, and the definition-owned
keys `DD_AUTH_TOKEN_FILE_PATH`, `DD_RUN_PATH` and `DD_DATA_PLANE_FORCE_ENABLE`. It rejects case configuration files.
The Agent and ADP start together in the participant directory, which holds an empty `datadog.yaml` and their shared
authentication token. ADP retries the token read while the Agent starts. Only the Agent receives the run path and,
when ADP is enabled, the macOS force-enable switch. Each process's output goes to `<process>.stdout.log` and
`<process>.stderr.log` under the participant's log directory.

`--subject-os linux|macos|windows` selects which cases run. It defaults to the host's OS. Correctness cases run only
for subject OS `linux` and are reported as `skipped` for other subject OSes. Pass `--subject-os linux` to run them
from another host; the correctness Make targets do this.

A case whose `subject_os` omits the selected OS appears in `run.json` with outcome `skipped` and `skip_reason`
`subject_os excludes <os>`. It counts in `totals.skipped`. Only executed cases receive a log directory and
`result.json`. The exit code follows the runnable cases; an all-skipped selection writes `run.json` and exits 3.
Text listings show every enabled case's status: `runnable` or `skipped (subject_os)`.

`list --json` includes every enabled case, keyed by name. Every row has `type`, `timeout` and `status`, which is
`runnable` or `skipped`. Runnable rows that use a typed runner also have `runtime` and `images`.
Skipped rows have `skip_reason`. Each row also has:

- `layout`: a list of `{participant, type, os, realization}` in participant-name order. `type` names the participant
  type, `os` is `linux`, `macos` or `windows`, and `realization` is `linux-container`, `windows-container`,
  `host-process-group` or `kubernetes-pod`. Cases with a top-level `type` have one entry with `participant` and
  `type` set to `null`, giving the realization their runner uses. Only skipped cases have `layout: null`.
- `roles`: a list of `{role, kind, reference, source}` in role-name order. `kind` is `image`, `binary` or `install`;
  `source` is `manifest` or `local_default`. A runnable case needing no roles has an empty list;
  a skipped case has `roles: null` because it wasn't planned for this OS.

The correctness CI generator reads `status`, `layout[].realization` and `roles` to select jobs and their templates.

Before running, `run` checks each distinct realization needed by the selected cases once, without changing their
layouts:

| Realization | Host requirement |
| --- | --- |
| Linux container | A Docker daemon answers and reports Linux containers |
| Windows container | A Docker daemon answers and reports Windows containers |
| Host process group | The host runs the participant's OS |
| Kubernetes pod | The participant runs Linux, `kind` and `docker` are on `PATH`, and a Docker daemon answers and reports Linux containers |

`artifacts verify`, `artifacts pull`, and the host and artifact checks `run` makes before starting cases share one
Docker daemon connection and ask its OS type and architecture once. A connection failure fails every such check
that needs the daemon.

A case whose realization the host can't provide is `errored` with error kind `setup`, appears in `run.json`, and
makes the run exit 2. `list` never checks host availability.

The source documents each part:

- `bin/correctness/panoramic/core/src/case/file.rs`: the case file's fields and their defaults.
- `bin/correctness/panoramic/core/src/artifact.rs`: artifact roles and the manifest.
- `bin/correctness/panoramic/core/src/artifact/verify.rs`: artifact verification and identities.
- `bin/correctness/panoramic/core/src/artifact/pull.rs`: image pulling and identities.
- `bin/correctness/panoramic/core/src/backend/run.rs`: host availability checks, artifact verification and image pulls over one Docker connection.
- `bin/correctness/panoramic/core/src/layout.rs`: how participants are placed on an OS and realization.
- `bin/correctness/panoramic/participants/src/agent_adp/mod.rs`: what `agent-adp` declares, including its facts and artifact roles.
- `bin/correctness/panoramic/steps/src/`: each step's fields.

Tests in the `panoramic` binary scan every file in each crate directory recursively, excluding `target/`, to check
that `panoramic-core` names no product and that `panoramic-steps` names no realization, host mechanism, or product path.
The crate dependency test keeps core and steps independent of `panoramic-participants`. Core's only panoramic
dependency is `panoramic-courier`, for shared endpoint execution and its calling contract.

### Step deadlines

Each step uses its `timeout`, `duration`, or `during` as its budget. The driver allows 10 seconds beyond that budget
for the step to report its result, then stops it as `errored`. This makes the test a harness error (exit code 2), unless a parallel
member failed. Case timeout or run cancellation interrupts unfinished steps and preserves completed results.

Polling HTTP requests, endpoint probes and intake polls are limited to the shorter of 10 seconds or the step's
remaining time. The native `config_key_equals` step gives each `config_dump` call the whole remaining budget for
its exec. Timed-out probes are retried while the step has time left. A step is `errored` if harness failures
(such as Docker API or intake sidecar errors) prevent evaluation, or its configuration is invalid. An unmet expectation
remains `failed` after any product observation, even if later probes encounter harness errors.

## Panoramic CLI and environment

Both suites run through the same `panoramic` binary.

```bash
cargo run --release --bin panoramic -- --help
```

The `panoramic` skill walks LLM agents through choosing and running a suite non-interactively.

### Reading panoramic results

The Make targets write artifacts beneath `target/test-output/panoramic/` by default. Set `SALUKI_TEST_OUTPUT_DIR` to
relocate the shared test-output root, or `PANORAMIC_LOG_DIR` to override the location for panoramic only. Each run
creates a timestamped `panoramic-*` directory beneath the base, and stdout and `run.json` identify the directory for a
run. `make clean-test-logs` deletes the panoramic directory, and `make clean` includes that target.

Exit codes distinguish the run-level result:

| Code | Meaning |
|------|---------|
| 0 | At least one test ran and every test that ran passed; run completed |
| 1 | Assertion failure, unless code 2 applies |
| 2 | Harness error, including a case that fails to load or a case name declared more than once (reported before any test runs, with no `run.json`), a case whose realization this host can't provide, a `timed_out` test, or an interrupted run |
| 3 | Empty selection, unmatched test name, or every selected test skipped (`run.json` is written) |

An `errored` step couldn't be evaluated. Inspect test `error`, `timeout`, `cause`, and step `message` fields.

Check `run.json` first: it's updated during execution, so passing entries don't prove the run finished. Inspect
completeness before individual results, and treat unknown states, outcomes, or statuses as non-passing:

```bash
jq '{state, interrupted_reason, running, not_run}' run.json
jq -r '.tests[] | select((.outcome != "passed" and .outcome != "skipped") or (.issues | length) > 0) | "\(.name) \(.outcome) \(.log_dir)"' run.json
```

Follow the selected tests' `log_dir` to `result.json`. Use `outcome` for the test result and `assertions[].status` for
each step. Check `issues` even on passes; they don't change `outcome`. Open reported artifact paths and
`diagnostic_lines` sources for context and uncapped details.

Each `run.json` entry in `tests[]` and each `result.json` contains `layout`, using the same
`{participant, type, os, realization}` entries as `list --json`. It records the planned layout even when setup
fails, including when the host can't provide a realization. Cases with a top-level `type` have one entry with
`participant` and `type` set to `null`, giving the realization their runner uses. Only skipped cases have
`layout: null`. Run-level `artifacts` in `run.json` records every binding and identity verified before cases
started, including native cases' roles on runs without `--artifacts`, or `null` when the run didn't verify artifacts.

Native cases also report `participants[]`, with each participant's `name`, `type`, `os`, `realization`, and `processes`.
A participant appears once its creation is attempted; its process list stays empty until startup. Each process has an
`exit` with `state: running | exited | undetermined` and `source: container | record | process`. Exit codes, signals and
failure reasons appear only when observed. A container runtime error accompanies its exit as `reason`. Process `output`
lists host paths under `stdout`, `stderr` or `merged`. The runner snapshots observations before cancelling an interrupted
case, or after the case body returns normally, then stops background tasks. The snapshot survives a dropped case body.
Legacy cases report an empty participant list.

For native cases, `diagnostic_lines` reads declared process outputs and container output that no process owns.
Non-blank stderr lines qualify; stdout and merged output require severity markers. Legacy cases use
filename-based stream classification. `artifacts` lists files under the test's log directory.

After the procedure, participant types can collect declared files from running or exited participants.
Files go under `<participant>/collected/`, or `traffic/` for declared traffic captures. Copies still present after
traffic retention appear as `{participant, kind, path}` in `collected` in `result.json`; merged sidecars and deleted
captures are described by `traffic/manifest.json`. Collection continues after a case deadline but stops on run
interruption. A missing file, failed read or write, or a collection exceeding five seconds adds a `collection_failed`
issue without changing the procedure's verdict.

Follow report types in `bin/correctness/panoramic/core/src/machine_output.rs` for semantics. For precedence rules, see
`TestResult::from_steps` in `bin/correctness/panoramic/core/src/result.rs` and `exit_code` in
`bin/correctness/panoramic/src/collector.rs`. The current `schema_version` is 3. Check it and `run.json`'s
`build_revision` against the working tree.

On Unix, send SIGINT or SIGTERM to stop a run with cleanup. A second signal may leave containers behind.

### Resource cleanup

After every test, the runner attempts cleanup and records each resource as `reclaimed`, `leaked`, or `unsettled` under
`reclaim` in `result.json`. Native participants have separate `participant:<name>` entries with a `participant` field.
Each stops with its type's grace, ten seconds by default, before removal. Host participants receive SIGTERM in each
process group, then SIGKILL after the grace for groups still running, including descendants of an exited leader.
Child reapers remain active after case tasks stop. The host scope has a `directory:<path>` entry, removed after
participants; it also removes an empty parent run directory. A failed participant removal keeps the state directory
for diagnosis. Consecutive participant entries stop concurrently; a container scope's shared volume and network follow
through the run's Docker connection. An interrupted creation marks both its participant and scope `unsettled`, because
creation might finish after removal. The `details` and `traffic` artifact directories are reserved participant names.

Correctness tests have one isolation-group entry per container group, or one `namespace:` entry per kind namespace.
The runner stops their background tasks first. Failed or skipped removal adds a `resource_leaked` issue; unconfirmed removal adds `resource_unsettled`. These issues leave the
test outcome unchanged. A background task still tracked after the five-second stop budget adds a
`tasks_still_running` issue without changing the outcome; resource cleanup still runs.

### Reading traffic artifacts

Traffic captures live under `<log_dir>/traffic/`. Start with `traffic/manifest.json`: it lists each capture, whether its
file is still on disk, and why any capture is missing. Take file names from the manifest rather than guessing them.
Captures are zstd-compressed JSON Lines:

```bash
jq -r '(.input // empty), .decoded[] | select(.file.present) | .file.name' traffic/manifest.json
zstd -dc traffic/<file-name> | head -5
```

The module docs in `bin/correctness/panoramic/src/traffic.rs` explain each artifact, its retention, and record ordering.
Native intake assertions read the intake through its participant operations. The `compare` step reads each dump
once from two distinct participants offering `dump`. Both snapshots are fetched concurrently; decoded captures
identify the participants and the dump operation, without assuming which products they run:

```yaml
- step: compare
  kind: metrics
  subject: intake-adp
  baseline: intake-agent
  require_forwarded_packets: false
```

`kind` is `metrics`, `events`, `service_checks`, or `traces`. Trace comparisons also accept `otlp_direct` (default
false) and `ignore_fields` (default empty). Forwarded packets are compared for every kind; `require_forwarded_packets`
(default false) additionally requires nonempty captures. Mismatches and invalid product responses fail the step;
transport and other harness failures error it. Capture-write failures are recorded in the traffic manifest without
changing the comparison verdict.

For metrics, `required_metric_tag_keys` (default empty) requires at least one workload context on each side with
nonempty values for every listed tag key. Internal telemetry doesn't satisfy this check, nor do tags split across
contexts. The origin-detection cases require `image_name`, a low-cardinality tag from container discovery that their
workloads don't send. Equal but unenriched outputs therefore fail.

## Benchmark tests: Single Machine Performance (SMP)

SMP is a system that runs on internal, dedicated infrastructure to check the Agent for performance regressions. It runs
experiments across multiple replicates with statistical analysis and posts reports to PRs. Maps to the `benchmark` stage
in GitLab CI (`.gitlab/benchmark.yml`).

Each experiment pairs a **target** (ADP container) with **Lading** (deterministic load generator). Lading sends payloads
(DogStatsD, OTLP, logs, etc.) at configured rates using seeded generators for reproducibility. SMP measures CPU, memory,
throughput—not output correctness.

### Experiments

Defined in `test/smp/regression/adp/experiments.yaml`. Run `make generate-smp-experiments` to generate per-case configs
into two suites: `test/smp/regression/adp/quality-gates/` (experiments with `checks:` bounds) and
`test/smp/regression/adp/full/` (all experiments). Each case gets an `experiment.yaml` (target config) and
`lading/lading.yaml` (load config). See `test/smp/README.md` for the suite model.

PRs gate only on the quality-gates suite, comparing the branch against the merge-base of main—"has your change regressed
or improved against the bounds?" The full suite runs nightly on `main` (date-anchored against the previous nightly,
reported to Slack) and on-demand as a manual job on a PR; it is for long-term trend analysis and does not gate.

You can run experiments locally with `smp local-run` to debug experiment configs without waiting for CI (single
replicate, no statistical analysis). This is mainly useful when iterating on a new or broken experiment—for normal
development, lean on CI.

## Fuzzing

We aren't doing a lot with fuzzing right now, but what we've uses `cargo-fuzz` and
operates at the function-level. More fuzzing coverage will likely come in the future.

## Loom and Miri testing

Loom and Miri target concurrency and memory-model correctness rather than business logic. **Loom**
exhaustively explores how concurrent operations can interleave, to catch ordering bugs that are hard to hit through
normal test runs. Run it with `make test-loom`. **Miri** runs tests under an interpreter that catches undefined
behavior—data races, use-after-free, invalid memory access—that a normal test run wouldn't detect. Run it with `make
test-miri`.

## Antithesis

[Antithesis](https://antithesis.com/) runs ADP under deterministic simulation with injected faults. Our focus is on
establishing that ADP and the Datadog Agent's DogStatsD behave equivalently: if ADP replaces the Agent's DogStatsD
server, users shouldn't notice shifts in their telemetry. Better operational behavior is an acceptable deviation.

This is separate from the panoramic correctness tests, which compare outputs for fixed workloads without fault
injection. The harness and scenarios live under `test/antithesis/`, and `test/antithesis/AGENTS.md` describes how
agents drive runs there.

## Directory index

```
.gitlab/
├── test.yml               unit tests
├── e2e.yml                correctness + integration
├── benchmark.yml          SMP benchmarks
└── fuzz.yml               fuzz testing

# custom test tooling
bin/correctness/
├── panoramic/             : unified test runner (correctness + integration)
├── millstone/             : load generator
├── datadog-intake/        : mock Datadog API
├── airlock/               : container isolation lib
└── stele/                 : telemetry data types

# actual test cases
test/
├── correctness/           : correctness test configs
├── antithesis/            : antithesis harness and scenarios
├── integration/           : integration test configs
└── smp/                   : smp experiments
```
