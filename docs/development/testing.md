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

| Binary             | Purpose                                                                                  |
|--------------------|------------------------------------------------------------------------------------------|
| **panoramic**      | Unified test runner for both correctness and integration tests.                          |
| **millstone**      | Deterministic load generator.                                                            |
| **datadog-intake** | Mock Datadog API: receives test output                                                   |
| **airlock**        | Library for running containers in isolated groups.                                       |

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
(`bin/correctness/panoramic/`). A case file must be named `config.yaml`; discovery rejects `config.yml`.

A case file either sets `type: integration`, defining a container and a list of high-level assertions, or declares
participants; see [Participant cases](#participant-cases). Assertions run sequentially by default but can also be
configured to run in `parallel`.

CI: `.gitlab/e2e.yml`—same file as correctness, `e2e` stage, 10 min timeout, retry 2.

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

procedure:
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

These cases run on the integration runner. Panoramic translates each case into an integration case for every subject
OS and runs it: in a Linux or Windows container with default settings, or as host processes on macOS. The translation
supports this subset of the schema:

- One participant, of type `agent-adp` with `subject: true`, configured only through `config.env`, optionally with
  `config.os` overlays of `env` whose values replace the base values on that OS. Environment values must be YAML strings.
- The steps `log_contains` and `log_not_contains`, without `process`. They search the output of every process
  together, and what is captured differs by OS.
- The step `file_contains`. Its `path` may reference a participant fact, written as in the example above with the
  participant's name and the fact's name.
  `agent-adp` declares `default_log_file` for its Linux and Windows containers only, so a case that runs on macOS writes
  the path out.
- `parallel` blocks of these steps.

Durations in a participant case need a unit: write `30s`, not `30`.

Every step names the participant it acts on with `on`. Step fields differ from the integration schema: steps use
`step:` instead of `assertion:`, and `file_contains` requires `pattern`. The loader accepts other participant and
configuration fields, but the integration runner rejects a case that sets them, with an error naming the field.
The loader rejects keys in `realization.options` because no realization accepts options.
A `config.files` source must be a nonempty path relative to the case directory that exists when the case loads.
Parent-directory components must stay within the case directory; symlink targets may be outside it.

`panoramic list` shows each participant case's layout. A case whose layout is invalid on any OS it lists in
`subject_os` fails discovery for `list` and `run`, whatever runtime is selected.
A referenced fact missing for a participant's realization on any OS in `subject_os` also fails discovery.

Connections and endpoint overrides are checked the same way: on every OS in `subject_os`, each `connect` key
must be an endpoint the participant consumes, the target must serve the named endpoint with its configuration,
and the endpoint's kind must be one the consumer accepts. Every required consumed endpoint must be connected,
and each `config.endpoints` key must name an endpoint the participant serves. Both `connect` and
`config.endpoints` must be empty for `agent-adp`.

`--subject-os linux|macos|windows` selects which cases run. It defaults to the OS selected by `--runtime`, then the
host's OS. Conflicting values exit 2 before discovery. Correctness cases keep their configured runtime.

Before running, `run` checks each distinct realization needed by the selected cases once, without changing their
layouts:

| Realization | Host requirement |
| --- | --- |
| Linux container | A Docker daemon answers and reports Linux containers |
| Windows container | A Docker daemon answers and reports Windows containers |
| Host process group | The host runs the participant's OS |
| Kubernetes pod | The participant runs Linux, `kind` and `docker` are on `PATH`, and a Docker daemon answers and reports Linux containers |

A case whose realization the host can't provide is `errored` with error kind `setup`, appears in `run.json`, and
makes the run exit 2. `list` never checks host availability.

The source documents each part:

- `bin/correctness/panoramic/core/src/case/file.rs`: the case file's fields and their defaults.
- `bin/correctness/panoramic/core/src/layout.rs`: how participants are placed on an OS and realization.
- `bin/correctness/panoramic/participants/src/agent_adp.rs`: what `agent-adp` declares, including its facts.
- `bin/correctness/panoramic/steps/src/`: each step's fields.
- `bin/correctness/panoramic/src/config/integration_bridge.rs`: the subset the integration runner supports, and the output each OS captures.

Tests in the `panoramic` binary scan every file in each crate directory recursively, excluding `target/`, to check
that `panoramic-core` names no product and that `panoramic-steps` names no realization, host mechanism, or product path.
To exempt an unavoidable core line, add `// north-star: <reason>` on that line with a non-empty reason; markers on lines
without a match fail the check. Steps have no exemptions.
The crate dependency test keeps core and steps independent of `panoramic-participants`.

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
| 2 | Harness error, including a case that fails to load or a case name declared more than once (reported before any test runs, with no `run.json`), a case whose realization this host can't provide, a `timed_out` test, or an interrupted run |
| 3 | Empty selection or unmatched test name |

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

Follow report types in `bin/correctness/panoramic/core/src/machine_output.rs` for semantics. For precedence rules, see
`TestResult::from_steps` in `bin/correctness/panoramic/core/src/result.rs` and `exit_code` in
`bin/correctness/panoramic/src/collector.rs`. Check `schema_version` and `run.json`'s
`build_revision` against the working tree.

On Unix, send SIGINT or SIGTERM to stop a run with cleanup. A second signal may leave containers behind.

### Resource cleanup

After every test, the runner attempts cleanup and records each resource as `reclaimed`, `leaked`, or `unsettled` under
`reclaim` in `result.json`. Docker containers, their shared volume, and network share one isolation-group entry; host
processes and the state directory have separate entries. Correctness tests have one isolation-group entry per container
group, or one `namespace:` entry per kind namespace, and the runner stops their background tasks first. Failed or
skipped removal adds a `resource_leaked` issue; unconfirmed removal adds `resource_unsettled`. These issues leave the
test outcome unchanged.

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
