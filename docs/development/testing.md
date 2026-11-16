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

These are found throughout the Rust codebase as you would expect. You can run them with `cargo test` or you can use
`make test` which will run them with `cargo nextest` for more parallelization. Platform-specific unit tests should be
skipped or compiled-out for platforms they're incompatible with. For conventions on test naming, table-driven tests,
shared fixtures, and assertion density, see [Testing patterns](./testing-patterns.md).

CI: `.gitlab/test.yml`—runs on both Linux (amd64/arm64) and macOS (amd64/arm64).

### Tests that need a Docker daemon

Unit tests that create containers are ignored by default, so `make test` doesn't need Docker. Run them with
`make test-container-daemon`, which needs a reachable Docker daemon and builds the courier the tests copy into their
containers. Their image comes from the internal registry. CI doesn't run them, so run them by hand when you change the
container mechanisms, the container backend or the lifecycle.

### Architecture tests

The `architecture-tests` crate in `test/architecture/` protects architectural invariants as part of the unit-test
suite. Dependency guards check crate relationships; symbol guards restrict names used within selected source
directories. Add tests here for other architectural invariants, documenting each beside its test. The crate's API
documentation explains the guards and their limits.

```shell
cargo nextest run -p architecture-tests
```

Failures report a dependency route or source location. Inspect symbol matches before changing code: unrelated names
can produce false positives.

## Correctness tests (panoramic)

These tests serve to answer the question: *Does ADP produce the same output as the Datadog Agent for a given workload?*

To answer this question, a correctness test runs ADP and the Datadog Agent side-by-side in containers and compares their
output for a given input. The output comparison is semantic, not a simple byte-by-byte comparison, thus heuristics are
used to assert correctness.

**Terminology Note:** Correctness tests ***are integration tests*** in the sense that they run the entire system.
However, in our repository, *integration tests* refer to a specific set of smoke tests [below](#integration-tests-panoramic).

Correctness test cases are specified by YAML configuration files found in `test/correctness/cases`.

### Binaries and program flow

All binaries live under `bin/correctness/`:

| Binary             | Purpose                                                                                  |
|--------------------|------------------------------------------------------------------------------------------|
| **panoramic**      | Unified test runner for both correctness and integration tests.                          |
| **millstone**      | Deterministic load generator.                                                            |
| **`otlp-fixtures`** | Builds the static OTLP payloads of some correctness cases.                              |
| **datadog-intake** | Mock Datadog API: receives test output                                                   |
| **airlock**        | Library for running containers in isolated groups.                                       |
| **courier**        | Static probe, send, HTTP and file-follow tool that panoramic runs inside containers.     |

Test case configs live in `test/correctness/cases/` (for example, `test/correctness/cases/dsd-plain/config.yaml`).

### Program flow

**panoramic** is the entry-point for running a test. When given a correctness test config, it orchestrates the
containers using the `airlock` library (which talks to containerd via gRPC) and asserts the correctness of the output. It:
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

### Static fixtures

Some cases send a fixed request through a `static_base64` payload in their `millstone.yaml`.
`bin/correctness/otlp-fixtures` builds that request, with a module for each OTLP signal that has static payloads: edit
the fixture there, then run `make generate-correctness-fixtures` to rewrite the payloads. Don't edit the payloads by
hand. The `check-correctness-fixtures` CI job runs the same target and fails if it changes any committed payload.

## Integration tests (panoramic)

Integration tests run a containerized ADP instance and assert high-level invariants: process stability, expected log
output, port availability, exit behavior. They catch regressions from enabling new features or settings that cause
crashes or early exits. They don't test output correctness. This type of test is often known as a "smoke test." For
integration tests that check system output, see [correctness tests](#correctness-tests-panoramic) above.

### Running

```bash
make test-integration        # build images + run all
make test-integration-quick  # skip image builds
make list-integration-tests  # list available tests
```

### Test case config

Test cases live in `test/integration/cases/` as `config.yaml` files. The runner is **panoramic**
(`bin/correctness/panoramic/`).

Each test defines a container and a list of high-level assertions. Assertions run sequentially by default but can also
be configured to run in `parallel`.

Panoramic moves every port the Core Agent and ADP bind off its default value, so tests don't collide with an Agent
already running on the host. The defaults live in `bin/correctness/panoramic/src/test_env.rs`. A port that a test binds
**MUST** be below 49152. On macOS, tests run as host processes, and ports from 49152 up are handed out as source ports
for outbound connections. macOS refuses to bind a port that another user's process holds, so a listener in that range
fails intermittently.

CI: `.gitlab/e2e.yml`—same file as correctness, `e2e` stage, 10 min timeout, retry 2.

### Step deadlines

Each step uses its `timeout`, `duration`, or `during` as its budget. The `dogstatsd_replay` action uses a whole-action
budget of `4 × timeout + capture_duration + 50ms`. The driver allows 10 seconds beyond that budget for the step to
report its result, then stops it as `errored`. This makes the test a harness error (exit code 2), unless a parallel
member failed. Case timeout or run cancellation interrupts unfinished steps and preserves completed results.

Each polling attempt (an HTTP request, Docker exec, or intake poll) is limited to the shorter of 10 seconds or the
step's remaining time. Timed-out probes are retried while the step has time left. A step is `errored` if harness failures
(such as Docker API or intake sidecar errors) prevent evaluation, or its configuration is invalid. An unmet expectation
remains `failed` after any product observation, even if later probes encounter harness errors.

## Panoramic CLI and environment

Both suites run through the same `panoramic` binary.

```bash
cargo run --release --bin panoramic -- --help
```

`panoramic plan <case directory> --subject-os <os>` reads a case written in the participant case schema, the one
without a top-level `type:` key, and prints what it does on that OS as JSON, without starting anything: each
participant's realization, image, command, environment and files with their contents, each endpoint and connection
with its address, and the order participants are created and started in, with what each one waits for. `--text` prints
the same plan in a format for people. It plans the case for every OS in its `subject_os`, so a field that can't be
honored on any of them is an error on any host. The rejected case files under
`bin/correctness/panoramic/testdata/bad/` show a few of these errors.

`panoramic list` and `panoramic run` read participant cases beside legacy ones, telling them apart by the top-level
`type:` key. A run selects one subject OS with `--subject-os`, or `--runtime`, which names the same OS (`mac` is
`macos`); both default to the host's, and a `--runtime` that names no OS is an error. A participant case that doesn't declare that OS is reported as `skipped`, with
its reason. `--set <name>=<image>`, or `PANORAMIC_SET` as a comma-separated list, runs `<image>` wherever a case
names `<name>`; `plan` takes it too and shows both. Before the first case, the run checks once that the selected
participant cases can run: a Docker daemon for their Linux containers, every image they run, which it pulls if the
daemon lacks it, and the courier, which `make build-courier` builds. A participant case that doesn't pass writes its
plan as `plan.json` in its log directory.

The `panoramic` skill walks LLM agents through choosing and running a suite non-interactively.

### Reading panoramic results

The Make targets write artifacts beneath `target/test-output/panoramic/` by default. Set `SALUKI_TEST_OUTPUT_DIR` to
relocate the shared test-output root, or `PANORAMIC_LOG_DIR` to override the location for panoramic only. Each run
creates a timestamped `panoramic-*` directory beneath the base, and stdout and `run.json` identify the directory for a
run. `make clean-test-logs` deletes the panoramic directory, and `make clean` includes that target.

Exit codes distinguish the run-level result:

| Code | Meaning |
|------|---------|
| 0 | Every test passed; run completed |
| 1 | Assertion failure, unless code 2 applies |
| 2 | Harness error, `timed_out` test, or interrupted run |
| 3 | Empty selection, unmatched test name, or every test skipped |

An `errored` step couldn't be evaluated. Inspect test `error`, `timeout`, `cause`, and step `message` fields.

Check `run.json` first: it's updated during execution, so passing entries don't prove the run finished. Inspect
completeness before individual results, and treat unknown states, outcomes, or statuses as non-passing:

```bash
jq '{state, interrupted_reason, running, not_run}' run.json
jq -r '.tests[] | select(.outcome != "passed" or (.issues | length) > 0) | "\(.name) \(.outcome) \(.log_dir)"' run.json
```

Follow the selected tests' `log_dir` to `result.json`. Use `outcome` for the test result and `assertions[].status` for
each step. Check `issues` even on passes; they don't change `outcome`. Open reported artifact paths and
`diagnostic_lines` sources for context and uncapped details.

Follow report types in `bin/correctness/panoramic/src/machine_output.rs` for semantics. For precedence rules, see
`TestResult::from_steps` in `reporter.rs` and `exit_code` in `collector.rs`. Check `schema_version` and `run.json`'s
`build_revision` against the working tree.

On Unix, send SIGINT or SIGTERM to stop a run with cleanup. A second signal may leave containers behind.

### Resource cleanup

After every test, the runner attempts cleanup and records each resource as `reclaimed`, `leaked`, or `unsettled` under
`reclaim` in `result.json`. Docker containers, their shared volume, and network share one isolation-group entry; host
processes and the state directory have separate entries. Correctness tests have one isolation-group entry per container
group, or one `namespace:` entry per kind namespace, and the runner stops their background tasks first. Failed or
skipped removal adds a `resource_leaked` issue; unconfirmed removal adds `resource_unsettled`. These issues leave the
test outcome unchanged.

The runner saves host-process `stdout.log` and `stderr.log` after reclaim drains the log pumps, including shutdown
output. Setup failures and cancelled cases also retain these logs.

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

## Reviewing test changes

Reviewers check test changes against two checklists: one for the test tools, and one for the test cases.

### Correctness tooling review checklist

Reviewers check changes to the tools under `bin/correctness/` against these rules:

<!-- review:correctness-tooling -->
Apply these rules to files under `bin/correctness/`: `panoramic`, `millstone`, `airlock`, `stele`, `datadog-intake`,
and `otlp-fixtures`. Look first for bugs that hang a run, crash it, leave processes or containers behind, or let a
case pass when ADP is wrong.

- Each wait, probe, and exec has a deadline, and each read has a deadline or an idle timeout. A target that stops
  responding fails the case, and doesn't hang the job. (A step that never returned from one await used the whole case
  budget: #2894.) Don't ask for a size limit on an intake dump. The dump is the data under test, so `IntakeClient`
  reads all of it.
- The harness kills the process group of a host process when its handle drops, also after a timeout, a cancellation,
  or a panic. Flag a host process that has no such handle, or whose cleanup runs only on the normal path. (Host
  processes outlived a timed-out case and could keep its ports: #2885.)
- Dropping a handle doesn't remove a container, a network, a volume, or a Kubernetes namespace. The code registers
  the isolation group or the namespace with the case's `ResourceLedger` before the creation request, so that reclaim
  removes it. Flag a resource that the code creates before it registers it, or never registers.
- A target can report healthy before its receivers listen. A client such as `millstone` retries the first connection
  and transient errors, and fails on other errors. It uses bounded backoff, as `retry_backoff` in
  `bin/correctness/millstone/src/target.rs` does. (`millstone` exited after one failed connection to the Agent's gRPC
  receiver: #2175.)
- A fixed port that a target binds is below 49152 for a case that runs on macOS or Windows. In a Linux container that
  `airlock` starts, the port is below 60000, because `airlock` moves the ephemeral range to `60000 65535`. (ADP
  couldn't bind its API port in a Linux container and started 23 times: #3149.)
- A harness feature that resolves values for a target, such as dynamic variables, writes them for ADP and for the
  Core Agent. If it doesn't, the feature works only in standalone mode. (Panoramic wrote resolved dynamic variables
  only for ADP, so they worked only in standalone cases: #1854.)
- The correctness tools build with the `release` profile, which keeps overflow checks, so an overflow panics. When
  the load generator can make values near the limit of their type, such as span durations, a sum of them matches the
  upstream. For example, the trace-agent adds APM stats as `uint64` values that wrap, so `stele` uses
  `wrapping_add`. (Summed span durations overflowed and made `stele` panic: #3062.)
- An analysis compares only output that the case controls. It removes the targets' own telemetry, as
  `is_internal_telemetry` does in the metrics analysis, because that telemetry depends on timing and on the container.
  (The DogStatsD client telemetry of system-probe carried each container's ID and failed `dsd-forwarding`: #2851.)
- Don't ask for edge cases that no current case can produce. An analysis that is too strict fails a case, so it
  doesn't hide a regression. Flag analysis logic that can let a case pass when ADP is wrong, such as a filter or a
  tolerance that also matches real output.
<!-- /review -->

### Test case review checklist

Reviewers check changes to the test cases under `test/` against these rules:

<!-- review:test-cases -->
Apply these rules to files under `test/`: correctness cases, integration cases, and SMP experiments.

- A correctness case must fail when ADP is wrong. Flag a new `additional_span_ignore_fields` entry that has no comment
  to say why the field differs. Valid reasons include an ADP gap with its issue, a DDOT baseline, or a field that the
  two targets store in different forms. An ignored field can hide the bug that the case exists to find. (An ignored
  `meta.error.msg` field hid a wrong HTTP status text: #1317.)
- Flag a new `logs.known_differences` entry that covers more than its gap. For example, it has a `**` path, or it has
  no `kinds` or `when` but the gap affects only some kinds or fixtures. A broad entry can also hide a new bug.
  `panoramic` already requires `gap` and `reason`, and fails a stale entry.
- Check each `DD_` variable against its configuration key. The name is `DD_` and the key in uppercase, with `_` for
  each `.`, unless the schema entry of the key under `lib/datadog-agent/config/schema/core/` lists `env_vars`. The
  Agent doesn't read the `__` form. A wrong name leaves the setting at its default in ADP. (A `__` typo left DogStatsD
  off in ADP, and the DogStatsD cases still passed: #1072.)
- To check a Saluki-only key, look in `SalukiOnly` in `lib/agent-data-plane-config-system/src/saluki_only.rs`, not in
  the schema. An `ADP_DD_` variable goes to ADP only, so check the part after `ADP_`.
- The Core Agent and ADP can both run some pipelines, such as DogStatsD and OTLP. If ADP doesn't receive the traffic,
  the Core Agent handles it, and the case compares the Agent with itself. When a case turns on such a pipeline in ADP,
  ask what shows that ADP received the traffic. Don't ask when the case or the description already shows it.
- With `DD_DATA_PLANE_STANDALONE_MODE` set to `"true"`, ADP uses only its local configuration and doesn't register
  with the Core Agent. The value is `"false"` or unset in connected mode, which is the production mode. Ask for a
  connected-mode case when a change affects how settings reach ADP. Examples are changes to the configuration stream,
  registration, and environment translation.
- The `baseline` and `comparison` targets differ only in the setting under test and in the settings that turn on ADP.
  Settings that only ADP reads can also differ, such as `DD_AGGREGATE_CONTEXT_LIMIT` and `ADP_DD_` variables. Flag a
  setting that the Datadog Agent also reads and that only one side sets, when no comment explains it.
- A correctness case tests only the settings that it sets. Panoramic collects output before it stops the targets, and
  the metrics analysis reduces each series to one value. Ask for a unit test for shutdown behavior or single points.
  If a case only shows that a value parses, ask for a unit test instead. (A shutdown flush bug and a bug at a
  non-default window both passed the suite: #2820.)
- Flag a setting in a case or an SMP experiment that ADP no longer reads, such as a removed or renamed key. ADP
  ignores it without an error, so the case no longer tests it. For SMP, comment on
  `test/smp/regression/adp/experiments.yaml`, not on the generated `experiment.yaml` files.
  `make generate-smp-experiments` writes them from that file. (SMP experiments kept a removed V3 setting, so they sent
  V2 series: #2632.)
- A fixed port in an integration case is below 49152. On macOS, cases run as host processes, and macOS gives ports
  49152 and higher to outbound connections. (Integration ports from 55001 to 58125 failed intermittently on macOS:
  #3050.)
- A fake service doesn't pin one API version. It matches routes with any version prefix, and it advertises its
  version where the client negotiates one, such as the `API-Version` header on `/_ping`. The fake Docker daemon in
  `test/integration/cases/liveness-container-tags/run_liveness_test.py` does both. The nightly job runs the latest
  Agent. (A fake Docker daemon served only `/v1.54`, and the nightly Agent moved to 1.55: #2258.)
<!-- /review -->

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
├── otlp-fixtures/         : static OTLP payload builder
├── datadog-intake/        : mock Datadog API
├── airlock/               : container isolation lib
├── courier/               : in-container probe and send tool
└── stele/                 : telemetry data types

# actual test cases
test/
├── correctness/           : correctness test configs
├── antithesis/            : antithesis harness and scenarios
├── integration/           : integration test configs
└── smp/                   : smp experiments
```
