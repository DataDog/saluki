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

### FIPS unit tests

`make test-fips` runs the unit tests of the crates with FIPS-specific behavior in a FIPS build, which links the AWS-LC
FIPS module. Those crates are the ones that have FIPS-only code, or that build TLS configurations and check them for
FIPS compliance. Building the FIPS module needs Go and CMake, so `make test-all` doesn't include this target. CI runs it
on Linux (amd64). Run it when you change TLS configuration or FIPS-specific code.

### Tests that need a Docker daemon

Unit tests that create containers are ignored by default, so `make test` doesn't need Docker. Run them with
`make test-container-daemon`, which needs a reachable Docker daemon and builds the courier the tests copy into their
containers. Their image comes from the internal registry. CI doesn't run them, so run them by hand when you change the
container mechanisms, the container backend or the lifecycle.

### Tests that need a kind cluster

Unit tests that create pods are ignored too. Run them with `make test-kind`, which needs `kind` and `kubectl` on your
`PATH` and a single-node kind cluster named `saluki-correctness`, or the one `KIND_CLUSTER_NAME` names, and builds the
courier the tests place on its node. CI doesn't run them, so run them by hand when you change the pod backend.

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

Correctness test cases are specified by YAML configuration files found in `test/correctness/cases`. For the case
format, the steps, and how to read results, see [Panoramic cases](./panoramic.md).

### Binaries and program flow

All binaries live under `bin/correctness/`:

| Binary             | Purpose                                                                                  |
|--------------------|------------------------------------------------------------------------------------------|
| **panoramic**      | Test runner for the correctness and integration tests.                                   |
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

### OTLP receiver endpoints

A correctness case that sends OTLP to ADP and to the Agent leaves `otlp_config.receiver.protocols.*.endpoint` unset in
`datadog.yaml`. The `adp` participant's environment sets `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_{GRPC,HTTP}_ENDPOINT`, and
ADP and the Agent in its container read the same value. The `agent` participant takes the harness default, which
setting the key in `datadog.yaml` would withdraw.

### Static fixtures

Some cases send a fixed request through a `static_base64` payload in a sender config, such as a correctness case's
`millstone.yaml`. `bin/correctness/otlp-fixtures` builds that request, with a module for each OTLP signal that has
static payloads: edit the fixture there, then run `make generate-correctness-fixtures` to rewrite the payloads. Don't
edit the payloads by hand. To add a fixture, create the sender config with a `static_base64: ""` placeholder, then add
a table entry in `main.rs` with the config's path from the repository root. The `check-correctness-fixtures` CI job
runs the same target and fails if it changes any committed payload.

Name every sender config that `otlp-fixtures` writes `millstone*.yaml`, such as `millstone.yaml` or
`millstone-traces-grpc.yaml`. Millstone's `every_test_case_config_parses` test only checks files with that name, so a
differently named config skips the parse and validation check. `otlp-fixtures` refuses a target with another name.
The trace builder in `traces.rs` takes a seed for the trace and span IDs: give each fixture that can reach the same
destination its own seed, such as one per protocol.

## Integration tests (panoramic)

Integration tests run ADP, with the Datadog Agent where a case needs it, and assert high-level invariants: process
stability, expected log output, port availability, exit behavior, and the metrics that reach a mock intake. Each case
runs on the OSes it declares: in Linux and Windows containers, and as host processes on macOS. They catch regressions
from enabling new features or settings that cause crashes or early exits. They don't compare ADP's output with the
Datadog Agent's. This type of test is often known as a "smoke test." For tests that compare the two, see
[correctness tests](#correctness-tests-panoramic) above.

Test cases live in `test/integration/cases/`, one directory for each case, and the runner is **panoramic**
(`bin/correctness/panoramic/`). Panoramic runs integration and correctness cases the same way: the directory says
what a case is for. See [The two suites](./panoramic.md#the-two-suites). [Panoramic cases](./panoramic.md) explains how to write a case, run one, and read its
results.

### Running

```bash
make test-integration        # build images + run all
make test-integration-quick  # skip image builds
make list-integration-tests  # list available tests
```

Panoramic moves every port the Datadog Agent and ADP bind off its default value, so tests don't collide with an Agent
already running on the host. The defaults live in `DEFAULT_ENV` in
`bin/correctness/panoramic/src/participants/datadog_agent.rs`. A port that a test binds **MUST** be below 49152. On
macOS, tests run as host processes, and ports from 49152 up are handed out as source ports for outbound connections.
macOS refuses to bind a port that another user's process holds, so a listener in that range fails intermittently.

CI: `.gitlab/e2e.yml`—same file as correctness, `e2e` stage. The Windows job is in `.gitlab/windows.yml`.

The `panoramic` skill walks LLM agents through choosing and running a suite non-interactively.

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
  responding fails the case, and doesn't hang the job.
- An analysis compares only output that the case controls. It removes the targets' own telemetry, as
  `is_internal_telemetry` does in the metrics analysis, because that telemetry depends on timing and on the container.
- Don't ask for edge cases that no current case can produce. An analysis that is too strict fails a case, so it
  doesn't hide a regression. Flag analysis logic that can let a case pass when ADP is wrong, such as a filter or a
  tolerance that also matches real output.
- Make sure that a regression in the testing harness and libraries doesn't leave a formerly asserted invariant
  unchecked.
<!-- /review -->

### Test case review checklist

Reviewers check changes to the test cases under `test/` against these rules:

<!-- review:test-cases -->
Apply these rules to files under `test/`: correctness cases, integration cases, and SMP experiments.

- A correctness case must fail when ADP is wrong.
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
- Flag a setting in a case or an SMP experiment that ADP no longer reads, such as a removed or renamed key. ADP
  ignores it without an error, so the case no longer tests it. For SMP, comment on
  `test/smp/regression/adp/experiments.yaml`, not on the generated `experiment.yaml` files.
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
├── panoramic/             : test runner (correctness + integration)
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
