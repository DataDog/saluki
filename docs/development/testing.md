# Testing

The Saluki testing strategy consists of four main pillars:

1. Unit Tests (Rust/cargo)
2. Correctness Tests (ground-truth)
3. Integration Tests (panoramic)
4. Performance Tests (SMP)

## Unit Tests

These are found throughout the Rust codebase as you would expect. You can run them with `cargo test` or you can use
`make test` which will run them with `cargo nextest` for more parallelization. Platform-specific unit tests should be
skipped or compiled-out for platforms they are incompatible with.

CI: `.gitlab/test.yml` — runs on both Linux (amd64/arm64) and macOS (amd64/arm64).

## Correctness Tests (ground-truth)

These tests serve to answer the question: *Does ADP produce the same output as the Datadog Agent for a given workload?*

To answer this question, a correctness test runs ADP and the Datadog Agent side-by-side in containers and compares their
output for a given input. The output comparison is semantic, not a simple byte-by-byte comparison, thus heuristics are
used to assert correctness.

**Terminology Note:** Correctness tests ***are integration tests*** in the sense that they run the entire system.
However, in our repo, *integration tests* refer to a specific set of smoke tests [below](#integration-tests-panoramic).

Correctness test cases are specified by YAML configuration files found in `test/correctness`.

### Binaries and Program Flow

All binaries live under `bin/correctness/`:

| Binary             | Purpose                                                                                  |
|--------------------|------------------------------------------------------------------------------------------|
| **ground-truth**   | Test runner and analyzer. Orchestrates containers, collects outputs and asserts outputs. |
| **millstone**      | Deterministic load generator.                                                            |
| **datadog-intake** | Mock Datadog API: receives test output                                                   |
| **airlock**        | Library for running containers in isolated groups.                                       |

Test case configs live in `test/correctness/` (e.g. `test/correctness/dsd-plain/config.yaml`).

### Program Flow

**ground-truth** is the entry-point for running a test. It orchestrates the containers using the airlock library (which
talks to containerd via gRPC) and asserts the correctness of the output. It:
- reads the test configuration files
- starts two sets of containers
  - `millstone` -> ADP -> `datadog-intake`
  - `millstone` -> Datadog Agent -> `datadog-intake`
- takes the output from `datadog-intake` and asserts that ADP and Datadog Agent behavior were equivalent

### Running

Build the required container images, then run:

```bash
# build images (only needed once, or after changes)
make build-datadog-intake-image build-millstone-image build-datadog-agent-image

# run all correctness tests
make test-correctness

# run a single test case
make test-correctness-dsd-plain
```

CI: `.gitlab/e2e.yml` — `e2e` stage, 10 min timeout, retry 2.

## Integration Tests (panoramic)

Integration tests run a containerized ADP instance and assert high-level invariants: process stability, expected log
output, port availability, exit behavior. They catch regressions from enabling new features or settings that cause
crashes or early exits. They do not test output correctness. This type of test is often known as a "smoke test." For
integration tests that check system output, see [correctness tests](#correctness-tests-ground-truth) above.

### Running

```bash
make test-integration        # build images + run all
make test-integration-quick  # skip image builds
make list-integration-tests  # list available tests
```

### Test Case Config

Test cases live in `test/integration/cases/` as `config.yaml` files. The runner is **panoramic**
(`bin/correctness/panoramic/`).

Each test defines a container and a list of high-level assertions. Assertions run sequentially by default but can also
be configured to run in `parallel`.

CI: `.gitlab/e2e.yml` — same file as correctness, `e2e` stage, 10 min timeout, retry 2.

## Benchmark Tests: Single Machine Performance (SMP)

SMP is a system that runs on internal, dedicated infrastructure to check the Agent for performance regressions. It runs
experiments across multiple replicates with statistical analysis and posts reports to PRs. Maps to the `benchmark` stage
in GitLab CI (`.gitlab/benchmark.yml`).

Each experiment pairs a **target** (ADP container) with **Lading** (deterministic load generator). Lading sends payloads
(DogStatsD, OTLP, logs, etc.) at configured rates using seeded generators for reproducibility. SMP measures CPU, memory,
throughput — not output correctness.

### Experiments

Defined in `test/smp/regression/adp/experiments.yaml`. Run `make generate-smp-experiments` to generate per-case configs
in `test/smp/regression/adp/cases/`. Each case gets an `experiment.yaml` (target config) and `lading/lading.yaml` (load
config).

CI compares current branch against merge-base of main — purely "has your change regressed or improved?"

You can run experiments locally with `smp local-run` to debug experiment configs without waiting for CI (single
replicate, no statistical analysis). This is mainly useful when iterating on a new or broken experiment — for normal
development, lean on CI.

## Fuzzing

A fifth type of testing is fuzzing. We aren't doing a lot with fuzzing right now, but what we have uses `cargo-fuzz` and
operates at the function-level. More fuzzing coverage will likely come in the future.

## Directory Index

```
.gitlab/
├── test.yml               unit tests
├── e2e.yml                correctness + integration
├── benchmark.yml          SMP benchmarks
└── fuzz.yml               fuzz testing

# custom test tooling
bin/correctness/
├── ground-truth/          : test orchestrator
├── millstone/             : load generator
├── datadog-intake/        : mock Datadog API
├── panoramic/             : integration test runner
├── airlock/               : container isolation lib
└── stele/                 : telemetry data types

# actual test cases
test/
├── correctness/           : correctness test configs
├── integration/           : integration test configs
└── smp/                   : smp experiments
```