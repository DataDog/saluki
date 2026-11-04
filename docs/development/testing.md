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
Cases that are kept but must not run live in `test/correctness/disabled/`. CI and the `test-correctness` targets select
only `test/correctness/cases/`.

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

**panoramic** is the entry-point for running a test. When given a correctness test config, it orchestrates the
containers using the `airlock` library (which talks to the Docker Engine API) and asserts the correctness of the output.
It:
- reads the test configuration files
- starts two sets of containers
  - `millstone` -> ADP -> `datadog-intake`
  - `millstone` -> Datadog Agent -> `datadog-intake`
- takes the output from `datadog-intake` and asserts that ADP and Datadog Agent behavior were equivalent

### Running

```bash
# Run all correctness tests:
make test-correctness

# Run a single test case:
make test-correctness-case CASE=dsd-plain
```

The `test-correctness` target handles building all of the necessary prerequisites -- `millstone`, `datadog-intake`, `panoramic`, the bundled Agent/ADP image, and so on -- before running the test cases.

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
`adp-host-binary` and `agent-host-install` to the paths the target passes as flags;
`make render-macos-test-artifacts` writes a matching
manifest to `MACOS_TEST_ARTIFACTS` (default: `target/panoramic-artifacts-macos.toml`). Use the same `BUILD_PROFILE` and
`MACOS_TEST_AGENT_INSTALL_DIR` values for rendering and running.
`test-integration-macos-ci` renders and forces its own manifest, even if you supply another path.

### Test case config

Test cases live in `test/integration/cases/` as `config.yaml` files. The runner is **panoramic**
(`bin/correctness/panoramic/`). A case file must be named `config.yaml`; discovery rejects `config.yml`.

A case file either sets `type: integration`, defining a container and a list of high-level assertions, or declares
participants; see [Participant cases](#participant-cases). Assertions run sequentially by default but can also be
configured to run in `parallel`.

CI: `test-integration` in `.gitlab/e2e.yml` runs in the `e2e` stage with a 15 min timeout and retry 2. It uses the
panoramic and courier binaries from `build-panoramic-binary`.

### Participant cases

A case file without a top-level `type` declares **participants**: named instances of a participant type. The case
states what runs, how it's configured and what the procedure checks. The participant type knows how its product is
installed and where it writes. The `adp-logging-*` cases in `test/integration/cases/` use this schema;
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

Panoramic routes a participant case to the integration runner when every participant's type has an integration bridge
route on the selected subject OS. `agent-adp` uses the bridge on macOS and Windows, running as host processes on macOS
or in a Windows container with default settings. Its Linux participant cases use native execution, as do other
participant types without a bridge route. Panoramic prepares each case's scope, creates participants in create order
from verified image IDs, and starts each level once the readiness evidence of the servers it connects to is observed. A
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
After the start levels, native Linux container cases run their procedure in the `steps` phase, using the same step
budgets, case deadline, parallel blocks and fail-fast behavior as the integration runner. Steps receive the started
participants through borrowed handles. File paths resolve early and late facts when the step runs.
A `parallel` block accepts only assertions. Actions, such as `send`, must be sequential steps; discovery rejects an
action in a parallel block before building its fields.

The native log and file steps accept `on`, `pattern` and an optional `regex` flag (default false):

- `log_contains` searches output from the participant's start, optionally narrowed by `process`, until `timeout`.
  A match passes; completed output without a match or a timeout fails.
- `log_not_contains` searches the same output for `during`, optionally narrowed by `process`. A match fails and quotes
  the matching line. Completed output keeps the wall-clock window; an exit alone does not fail this assertion.
- `file_contains` resolves `path` once, then reads until the contents match or `timeout` expires. Invalid UTF-8 bytes
  become replacement characters. Missing files and contents without a match are retried. Timeout reports the last
  observed file state.

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
  make it error. The last diagnostic gives the status and says when the body did not match. It supports plain HTTP;
  TLS fields are rejected.
- `send` is an action requiring `endpoint`, `payload` and `timeout`. It sends the string's UTF-8 bytes verbatim,
  including any framing, through the endpoint's planned route. YAML escapes above `\x7f` encode as UTF-8, so
  frame-length bytes must be below 0x80. It retries failed sends until one is accepted or the timeout expires.
  A retry after a harness error may deliver the payload twice. A timeout fails after any product refusal; it errors
  if only harness failures occurred. UDP cannot confirm a listener, so gate UDP sends on the participant's listener
  output.

An undetermined process exit errors stability and exit assertions. A process's first recorded exit remains final
across supervised restarts. The integration bridge translates only `log_contains`, `log_not_contains` and
`file_contains`; log steps on the bridge must omit `process`.

Participant types offer named **operations** per realization. An operation takes JSON arguments and returns JSON,
keeping product-specific commands inside the participant definition. Steps declare the operations they call;
plan compilation rejects an operation the participant's type does not offer for its realization, so `panoramic list`
reports the error before anything starts. Operations run commands through the participant's exec primitive or request
their own declared HTTP endpoints from the runner host. Planning validates those endpoints and allocates request
routes and any published ports. An operation can request only the HTTP endpoints it declares.
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

- `metric_type`: `count`, `rate`, `gauge`, or `sketch`.
- `value`: compared with the normalized value, using the same approximate float equality as correctness analysis. A
  rate is compared at its own interval. `value` can't be combined with `metric_type: sketch`; the case fails to
  load.
- `tags`: compared as `tags_match` directs. Both sides are sorted and deduplicated first.
- `tags_match`: `subset` (the default) requires every listed tag and allows extras, such as the environment-derived
  ones the Agent adds. `exact` requires the context's tags to equal the list. The intake reports a metric's host as
  a `host:<value>` tag, so under `exact`, `tags: []` means no tags and no host.

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

The `observe` phase gives exited participants up to five seconds to finish
capturing output; it also runs after a setup failure if any participant started. Running participants are skipped, and
case cancellation ends the wait. The runner snapshots observations, stops observation tasks, then reclaims resources.
Each case keeps its chosen route.

The `datadog-intake` participant runs the mock intake in a Linux container from `tools-linux-image`, with one process
named `intake`. Its exit and output come from the container. It serves two endpoints:

| Endpoint | Kind | Listen variable | Default |
| --- | --- | --- | --- |
| `api` | HTTP | `INTAKE_HTTP_LISTEN_ADDR` | `0.0.0.0:2049` |
| `dogstatsd_forward` | UDP | `INTAKE_DOGSTATSD_LISTEN_ADDR` | `0.0.0.0:9125` |

Set these variables in `config.env` to change the listener ports. Each must be a socket address with a nonzero port
and host `0.0.0.0` or `[::]`, so participants and published ports can reach it. Both endpoints declare readiness as
HTTP 200 from `api` at `/ready`: the intake binds UDP before starting HTTP. Connections wait for that evidence before
starting their consumer. Each intake stores its own telemetry. When the listen variables are unset, the intake binary
uses the defaults listed above.

The intake's `dump` operation accepts exactly `{"kind": "metrics"}` or `{"kind": "dogstatsd_forwarded"}`.
`metrics` returns the JSON array of stele metrics unchanged. `dogstatsd_forwarded` returns a JSON array of packets,
each a JSON array of byte values; the operation decodes the intake's base64 wire format. The operation declares only
`api` and requests `/metrics/dump` or `/dogstatsd-forwarding/dump`, respectively. Unknown kinds and extra arguments
are harness errors before any request. Non-success HTTP status, invalid JSON and invalid packet encoding are product
errors; messages omit response bodies. The operation is offered independently of realization. HTTP requests through
a courier route are harness errors.

The `agent-adp` Linux container definition uses `agent-adp-linux-image`. Its environment defaults match the legacy
runner's port isolation: command API 55001, expvar 55000, APM 58126, process command API 56062, Agent IPC 55004,
DogStatsD UDP 58125, ADP APIs 55100/55101 and Agent OTLP 54317/54318. The GUI is disabled. Case environment values
replace those defaults. The legacy translation receives only the case's environment after its OS overlay, so the
legacy runner applies its own defaults.

The definition derives these endpoints from the effective environment:

| Endpoint | Kind | Environment key | Conditions and readiness |
| --- | --- | --- | --- |
| `dogstatsd_udp` | UDP | `DD_DOGSTATSD_PORT` | Requires nonzero port, enabled DogStatsD and a non-loopback bind; ADP's listener output supplies readiness |
| `dogstatsd_tcp` | TCP | `DD_DOGSTATSD_TCP_PORT` | Requires ADP-owned DogStatsD, a nonzero port and a non-loopback bind; defaults to 0 (disabled); waits for TCP acceptance |
| `dogstatsd_uds` | Unix datagram | `DD_DOGSTATSD_SOCKET` | Defaults to `/var/run/datadog/dsd.socket` on Linux; empty disables it; waits for the socket |
| `dogstatsd_uds_stream` | Unix stream | `DD_DOGSTATSD_STREAM_SOCKET` | Nonempty enables it; waits for the socket |
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

ADP's OTLP endpoints require `DD_DATA_PLANE_ENABLED=true` and an enabled OTLP pipeline. The OTLP switch defaults
to false; `ADP_DD_DATA_PLANE_OTLP_ENABLED` takes precedence over `DD_DATA_PLANE_OTLP_ENABLED`. The receiver
address keys use the same `ADP_DD_` over `DD_` precedence. Proxy mode does not change endpoint availability.
Addresses use `host:port` without a scheme. A loopback host omits the endpoint, including the defaults
`localhost:6317` and `localhost:6318`; set a non-loopback address to reach the receiver. Port 0 also omits the endpoint.
Malformed addresses fail configuration with an error naming the key. The HTTP endpoint uses HTTP even when TLS is
enabled.

An optional `connect: {intake: <participant>.api}` connection accepts HTTP and delivers its URL as both `DD_DD_URL`
and `DD_APM_DD_URL`. It gates Agent startup on the intake's ready route.
An optional `connect: {dogstatsd_forward: <participant>.dogstatsd_forward}` connection accepts UDP and delivers
`DD_STATSD_FORWARD_HOST` and `DD_STATSD_FORWARD_PORT` from the target's host and port. It gates startup on the target's
readiness too. The native recipe uses the image's command and overlays the `data-plane` and `agent` s6 services. Each
finish script records the exit before running the shipped body, preserving ADP's two-second restart after failure and
disablement after a clean exit. Each run script merges stderr into stdout, and its logger appends to `adp.log` or
`agent.log` while retaining the lines on container stdout.
These merged process logs include the run and finish scripts' own messages. Container stdout and stderr are also
captured separately. The default ten-second stop grace covers the measured roughly 2.6-second supervisor shutdown.

Only the `agent-adp` Linux container realization offers the `config_dump` operation. Its arguments are exactly
`{"view": "source"}` or `{"view": "runtime"}`. It runs
`/opt/datadog-agent/embedded/bin/agent-data-plane config --json`, adding `--runtime` for the runtime view.
Unknown views or extra arguments are harness errors. Exit codes 126 and 127 mean the CLI could not run and are
harness errors; other nonzero exits and non-JSON stdout are product errors. Error messages omit stdout and stderr
because configuration can contain secrets.

The `PANORAMIC_` environment-variable prefix is reserved. Plan compilation rejects keys with this prefix in the
case's effective environment after OS overlays, in type defaults, and in connection deliveries. Errors identify
whether the case, type default or connection set the key.

Every native Linux container receives three reserved variables: `PANORAMIC_PARTICIPANT` names the participant,
`PANORAMIC_EXIT_DIR` names its exit-record directory, and `PANORAMIC_LOG_DIR` names its process-log directory.
Recipes that set any of these variables fail creation. The backend prepares the directories before start. The
`agent-adp` recipe uses the default host PID namespace and the container's own cgroup view.
macOS and Windows use the integration bridge.

The integration runner's translation supports this subset of the schema:

- One participant, of type `agent-adp` with `subject: true`, configured only through `config.env`, optionally with
  `config.os` overlays of `env` whose values replace the base values on that OS. Environment values must be YAML strings;
  the integration runner rejects references in them.
- The steps `log_contains` and `log_not_contains`, without `process`. They search the output of every process
  together, and what is captured differs by OS.
- The step `file_contains`. Its `path` may reference a participant fact, written as in the example above with the
  participant's name and the fact's name.
  `agent-adp` declares `default_log_file` for its Linux and Windows containers only, so a case that runs on macOS writes
  the path out.
- `parallel` blocks of these steps.

Step fields can also reference a participant's late facts as `<participant>.late.<fact>`: `ip` on every realization,
`container_id` on containers and pods, and `pod_uid` on pods only. They exist once the participant is created or running,
so the integration runner rejects them. Step fields accept only facts and late facts of explicitly named participants.

Environment variable values and string parameters, at any depth, can also contain references, including to the
participant itself as `self`. Variable names and parameter keys can't. On every OS in `subject_os`, a fact must exist
for the referenced participant's realization, and panoramic substitutes it before the participant type reads the
configuration. A host process group's `ip` is `127.0.0.1` for itself and for other host process groups on the same OS,
and is substituted the same way. Another participant's `container_id` is allowed when that participant is a Linux or
Windows container. A participant's own late facts are allowed only in environment variables, and only when its type
substitutes them after start; `agent-adp` substitutes none. Other late facts are rejected. An endpoint reference needs
an endpoint the participant serves and a supported route from the referencing participant. A connection reference
needs a matching `connect` key. Planning stores each connection's and endpoint reference's address as seen by its
holder; the configuration keeps the reference text until recipe rendering. `host` and `port` apply to network
endpoints, `path` to sockets and pipes, and `url` to every kind. The integration runner rejects any reference in an
environment variable, and `agent-adp` accepts no parameters.

A `config.files` source can be a map with `from` and `template: true`. Panoramic reads that file when the case
loads: it must be a regular UTF-8 file, and every double opening brace in it must start a reference.
Other sources aren't read. On every OS in `subject_os`, references in the files a participant uses after its
overlay follow the rules for environment variables, except for late facts. A Kubernetes pod's template files
may reference any late fact the referenced participant has, including the pod's own, except a host process group's
`ip`, which only participants on that host process group's realization can use. On other realizations, a template
file may not reference the participant's own late facts, except for a host process group's fixed `ip`.
The integration runner rejects `config.files`.

Panoramic orders participants before running a case. A participant is created after every other Linux or Windows container
whose `container_id` its configuration references. It starts after every participant it connects to. A Kubernetes pod
also starts after every other container or pod whose `ip` its template files reference, and after every other pod whose
`container_id` or `pod_uid` they reference. Own late facts are supplied by init hooks or late files; a host process
group's fixed `ip`, facts and endpoint addresses are available before creation, and steps run after all participants
start. Use `connect` when a participant needs another running first. A cycle in either order on any OS in `subject_os`
fails the case. `panoramic list` prints both orders, grouping independent participants.

Durations in a participant case need a unit: write `30s`, not `30`.

Every step names the participant it acts on with `on`. Step fields differ from the integration schema: steps use
`step:` instead of `assertion:`, and `file_contains` requires `pattern`. The loader accepts other participant and
configuration fields, but the integration runner rejects a case that sets them, with an error naming the field.
The loader rejects keys in `realization.options` because no realization accepts options.
A `config.files` source must be a nonempty path relative to the case directory that exists when the case loads.
Parent-directory components must stay within the case directory; symlink targets may be outside it.

`panoramic list` shows each participant case's layout. A case whose layout is invalid on any OS it lists in
`subject_os` fails discovery for `list` and `run`, whatever runtime is selected.
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
these bindings. With `run --artifacts <file>`, typed and bridge runners also verify and use them; without the flag,
those runners use case files, harness defaults and the image and binary flags or their environment variables.

Typed integration and correctness cases map their containers and host processes to these roles:

| Case and subject OS | Container or process | Artifact roles |
| --- | --- | --- |
| Integration, Linux | Target container | `agent-adp-linux-image` |
| Integration, Linux, intake enabled | Intake sidecar | `tools-linux-image` |
| Integration, Windows | Target container | `agent-adp-windows-image` |
| Integration, macOS | ADP and Core Agent host processes | `adp-host-binary`, `agent-host-install` |
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

Integration cases need the intake role only on Linux, where the sidecar runs. Correctness cases run only for subject OS
`linux`; their roles appear in text listings, `artifacts verify`/`pull`, and `run --artifacts` verification only when you
select Linux. Only runnable cases contribute artifact roles.

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
| `--image-override container=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `agent-adp-linux-image` on Linux, `agent-adp-windows-image` on Windows; no role on macOS |
| `--image-override intake=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `tools-linux-image` |
| `--image-override datadog-intake=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `tools-linux-image` |
| `--image-override millstone=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `tools-linux-image` |
| `--image-override baseline=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `agent-adp-linux-image` |
| `--image-override comparison=IMAGE` | `PANORAMIC_IMAGE_OVERRIDES` | `agent-adp-linux-image` |
| `--adp-binary-path` | `PANORAMIC_ADP_BINARY_PATH` | `adp-host-binary` |
| `--core-agent-binary-path` | `PANORAMIC_CORE_AGENT_BINARY_PATH` | `agent-host-install`, joined with `bin/agent/agent` |
| `--alpine-image` | `PANORAMIC_ALPINE_IMAGE` | `fixup-image` |

Only values explicitly supplied by a flag or environment variable are compared; defaults don't count as inputs.
Images are compared as strings and paths as paths, without canonicalization (trailing and repeated separators and
non-leading `.` components are ignored): `alpine` differs from `alpine:latest`, and `./x` differs from `x`.
The comparison uses every planned case in scope, before `-t` selection, and ignores inputs for roles no case needs.
Image override names must be unique and match a container in scope; for example, `container` is unmatched on macOS.
Several names feeding one role must all agree with its binding.

Panoramic reports every disagreement, including the role, bound reference and whether it came from the manifest or
a local default. Any disagreement exits 2 before verification, log directory creation or `run.json`. Agreeing
inputs leave the binding as the only effective input. CI can keep its legacy variables alongside a manifest that
binds the same references, including `fixup-image` for `PANORAMIC_ALPINE_IMAGE`.

The image roles feed the containers listed in the case table above. `adp-host-binary` supplies the host ADP executable;
`agent-host-install` supplies the Core Agent executable at `bin/agent/agent` beneath that directory. `fixup-image`
supplies the shared-volume permission fix-up container. `courier-linux-binary` supplies the static courier executable
for Linux containers. Kind loads the runners' bound images into its cluster.
`--mounts-dir`, kind options, `-l` and TUI options remain available. In `list --json`, `images` and `runtime` report
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
`make test-integration-macos-ci` binds the ADP binary and Agent install it builds and provisions;
`ci/tooling/windows-integration-tests.ps1` binds the image it builds. Image and binary overrides must agree with
the manifest. Every selected correctness case must use a `saluki-images/` baseline, because both sides take
`agent-adp-linux-image`.

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
artifacts verify, including when none are needed. Participant, integration and correctness cases contribute
artifact roles.

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
The integration runner rejects the `shared_dir` fact.

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
Step routes depend on the operation. Probes and raw sends to containers and pods use the courier inside the
server participant: a port proxy can accept a connection even when the server has no listener. HTTP and gRPC
requests to Linux containers and pods use published or forwarded ports and must observe a server response.
Host process group endpoints are reached directly. Request/response step needs and operations' declared HTTP
endpoints allocate runner-host ports. Connections and configuration references allocate ports as described above.

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
the participant serves. The integration bridge requires `connect` to be empty. The Linux `agent-adp` definition derives
its listener ports from the environment; configure those ports through the listener's environment key.

`--subject-os linux|macos|windows` selects which cases run. It defaults to the OS selected by `--runtime`, then the
host's OS. Conflicting values exit 2 before discovery. Correctness cases run only for subject OS `linux` and are
reported as `skipped` for other subject OSes. Pass `--subject-os linux` to run them from another host; the
correctness Make targets do this.

A case whose `subject_os` omits the selected OS appears in `run.json` with outcome `skipped` and `skip_reason`
`subject_os excludes <os>`. It counts in `totals.skipped`. Only executed cases receive a log directory and
`result.json`. The exit code follows the runnable cases; an all-skipped selection writes `run.json` and exits 3.
Text listings show every enabled case's status: `runnable` or `skipped (subject_os)`.

`list --json` includes every enabled case, keyed by name. Every row has `type`, `timeout` and `status`, which is
`runnable` or `skipped`. Runnable rows that use a typed runner or the integration bridge also have `runtime` and
`images`. Skipped rows have `skip_reason`. Each row also has:

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
- `bin/correctness/panoramic/src/config/integration_bridge.rs`: the subset the integration runner supports, and the output each OS captures.

Tests in the `panoramic` binary scan every file in each crate directory recursively, excluding `target/`, to check
that `panoramic-core` names no product and that `panoramic-steps` names no realization, host mechanism, or product path.
To exempt an unavoidable core line, add `// north-star: <reason>` on that line with a non-empty reason; markers on lines
without a match fail the check. Steps have no exemptions.
The crate dependency test keeps core and steps independent of `panoramic-participants`. Core's only panoramic
dependency is `panoramic-courier`, for shared endpoint execution and its calling contract.

### Step deadlines

Each step uses its `timeout`, `duration`, or `during` as its budget. The `dogstatsd_replay` action uses a whole-action
budget of `4 × timeout + capture_duration + 50ms`. The driver allows 10 seconds beyond that budget for the step to
report its result, then stops it as `errored`. This makes the test a harness error (exit code 2), unless a parallel
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
`exit` with `state: running | exited | undetermined` and `source: container | record`. Exit codes, signals and failure
reasons appear only when observed. A container runtime error accompanies its exit as `reason`. Process `output` lists
host paths under `stdout`, `stderr` or `merged`. The runner snapshots observations before cancelling an interrupted case,
or after the case body returns normally, then stops background tasks. The snapshot survives a dropped case body.
Legacy cases report an empty participant list.

For native cases, `diagnostic_lines` reads declared process outputs and container output that no process owns.
Non-blank stderr lines qualify; stdout and merged output require severity markers. Legacy cases use
filename-based stream classification. `artifacts` lists files under the test's log directory.

Follow report types in `bin/correctness/panoramic/core/src/machine_output.rs` for semantics. For precedence rules, see
`TestResult::from_steps` in `bin/correctness/panoramic/core/src/result.rs` and `exit_code` in
`bin/correctness/panoramic/src/collector.rs`. The current `schema_version` is 3. Check it and `run.json`'s
`build_revision` against the working tree.

On Unix, send SIGINT or SIGTERM to stop a run with cleanup. A second signal may leave containers behind.

### Resource cleanup

After every test, the runner attempts cleanup and records each resource as `reclaimed`, `leaked`, or `unsettled` under
`reclaim` in `result.json`. Native participants have separate `participant:<name>` entries with a `participant` field.
Each stops with its type's grace, ten seconds by default, before removal. Consecutive participant entries stop
concurrently; the scope's shared volume and network follow through the run's Docker connection. An interrupted creation
marks both its participant and scope `unsettled`, because creation might finish after removal. The `details` and
`traffic` artifact directories are reserved participant names.

Legacy Docker containers, their shared volume, and network share one isolation-group entry; host processes and the
state directory have separate entries. Correctness tests have one isolation-group entry per container
group, or one `namespace:` entry per kind namespace, and the runner stops their background tasks first. Failed or
skipped removal adds a `resource_leaked` issue; unconfirmed removal adds `resource_unsettled`. These issues leave the
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
For intake sidecar captures, see "Intake artifacts" in `test/integration/README.md`.

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
