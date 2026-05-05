# Agent-vs-ADP 1Hz dogstatsd benchmark — design

## Context

We want a side-by-side SMP regression benchmark comparing the Datadog Agent (baseline) against the converged Agent+Agent-Data-Plane (ADP) image (comparison) for raw dogstatsd ingest at a 1-second aggregation bucket interval. The benchmark stacks on PR [#1459](https://github.com/DataDog/saluki/pull/1459) (`luke/configurable-aggregation`), which adds `aggregator_bucket_size_seconds` as a serde alias for ADP's `aggregate_window_duration`. With that knob in place, a single `aggregator_bucket_size_seconds: 1` setting in `datadog.yaml` drives both the Agent core aggregator and ADP's aggregate transform to a 1Hz bucket width.

Modeled after PR [#1327](https://github.com/DataDog/saluki/pull/1327), but slimmer:

- No tag filtering; only raw dogstatsd processing.
- Reuses existing `test/smp/regression/adp/cases/dsd_uds_*` cases by rewriting them in place rather than creating a parallel `agent-adp-*` directory.
- Drops unrelated cases (OTLP, quality_gates) from the SMP target dir on this branch.
- Skips correctness + integration smoke tests and unrelated saluki source tweaks (gRPC max msg size, info-log addition).

PR shipped as a draft.

## Goals

- Produce a single SMP submission that runs all 15 `dsd_uds_*` cases (5 sizes × 3 optimization goals) with the Datadog Agent dev image as baseline and a converged Agent+ADP image as comparison.
- Both sides honor `aggregator_bucket_size_seconds: 1` from `datadog.yaml`.
- PR diff stays focused on benchmark plumbing — no unit tests, no source changes outside CI/Docker/case files.

## Non-goals

- Standalone-ADP regression coverage on this branch (replaced).
- Tag filtering (covered by reference PR).
- Correctness / integration smoke tests (out of scope for a benchmark-only PR).
- Tuning ADP-internal knobs (`ADP_DD_AGGREGATE_CONTEXT_LIMIT`, `ADP_DD_DOGSTATSD_STRING_INTERNER_SIZE`, gRPC max message size) — those were tag-filterlist-driven in the reference PR.

## Branch + PR layout

- Base branch: `luke/configurable-aggregation` (PR #1459, head `06f3965a`).
- This branch: `jszwedko/agent-vs-adp-1hz-benchmark`.
- PR opened as draft, targeted at `luke/configurable-aggregation`. GitHub auto-retargets to `main` when #1459 merges.

## File-by-file changes

### `test/smp/regression/adp/cases/` — deletions

Remove these case directories (not part of this benchmark):

- `otlp_ingest_logs_5mb_{cpu,memory,throughput}` (3)
- `otlp_ingest_metrics_5mb_{cpu,memory,throughput}` (3)
- `otlp_ingest_traces_5mb_{cpu,memory,throughput}` (3)
- `otlp_ingest_traces_ottl_filtering_5mb_{cpu,memory,throughput}` (3)
- `otlp_ingest_traces_ottl_transform_5mb_{cpu,memory,throughput}` (3)
- `quality_gates_rss_dsd_{heavy,low,medium,ultraheavy}` (4)
- `quality_gates_rss_idle` (1)

Total: 20 case dirs deleted.

### `test/smp/regression/adp/cases/dsd_uds_*` — in-place rewrite (15 cases)

Each case currently has:

```
agent-data-plane/cert.pem
agent-data-plane/empty.yaml
experiment.yaml
lading/lading.yaml
```

After the rewrite each case has:

```
datadog-agent/datadog.yaml
experiment.yaml
lading/lading.yaml
```

#### `agent-data-plane/` subdir — deleted

`empty.yaml` was used as ADP's `--config` arg in standalone mode. `cert.pem` was the IPC cert for ADP-as-client. In the converged image, ADP boots via the Agent's s6 supervisor (not via `target.command`) and consumes `DD_DATA_PLANE_*` / `ADP_DD_*` env vars baked into the Dockerfile plus `datadog.yaml` over the config-stream gRPC. Neither file is referenced. Delete both.

#### `datadog-agent/datadog.yaml` — new (per case)

```yaml
auth_token_file_path: /tmp/agent-auth-token
hostname: smp-regression

dd_url: http://127.0.0.1:9091
process_config.process_dd_url: http://localhost:9092

telemetry.enabled: true
telemetry.checks: '*'

# Disable cloud detection so the Agent doesn't probe the network.
cloud_provider_metadata: []

dogstatsd_socket: '/tmp/dsd.socket'
dogstatsd_origin_detection: true

# 1Hz bucketing; ADP picks this up via the alias added in PR #1459.
aggregator_bucket_size_seconds: 1
```

Identical content across all 15 cases. (Reference PR varied this file with `metric_tag_filterlist` per case; we don't.)

#### `experiment.yaml` — rewritten (per case)

Switch target from ADP-standalone to Datadog Agent entrypoint. Preserve each case's existing `optimization_goal` (cpu / memory / throughput).

```yaml
# Example: dsd_uds_10mb_3k_contexts_throughput
optimization_goal: ingress_throughput
erratic: false

target:
  name: datadog-agent
  command: /bin/entrypoint.sh
  cpu_allotment: 4
  memory_allotment: 3200MiB
  environment:
    DD_API_KEY: a0000001
    DD_HOSTNAME: smp-regression

  profiling_environment:
    DD_INTERNAL_PROFILING_BLOCK_PROFILE_RATE: 10000
    DD_INTERNAL_PROFILING_CPU_DURATION: 1m
    DD_INTERNAL_PROFILING_DELTA_PROFILES: true
    DD_INTERNAL_PROFILING_ENABLED: true
    DD_INTERNAL_PROFILING_ENABLE_GOROUTINE_STACKTRACES: true
    DD_INTERNAL_PROFILING_MUTEX_PROFILE_FRACTION: 10
    DD_INTERNAL_PROFILING_PERIOD: 1m
    DD_INTERNAL_PROFILING_UNIX_SOCKET: /smp-host/apm.socket
    DD_PROFILING_EXECUTION_TRACE_ENABLED: true
    DD_PROFILING_EXECUTION_TRACE_PERIOD: 1m
    DD_PROFILING_WAIT_PROFILE: true
    DD_APM_INTERNAL_PROFILING_ENABLED: true
    DD_INTERNAL_PROFILING_EXTRA_TAGS: experiment:dsd_uds_10mb_3k_contexts_throughput

report_links:
  - text: "bounds checks dashboard"
    link: "https://app.datadoghq.com/dashboard/vz3-jd5-bdi?fromUser=true&refresh_mode=paused&tpl_var_experiment%5B0%5D={{ experiment }}&tpl_var_job_id%5B0%5D={{ job_id }}&tpl_var_run-id%5B0%5D={{ job_id }}&view=spans&from_ts={{ start_time_ms }}&to_ts={{ end_time_ms }}&live=false"
```

Notes:

- `DD_AGGREGATOR_BUCKET_SIZE_SECONDS` is **not** set on `target.environment`. The single source of truth is `datadog.yaml`.
- `cpu_allotment: 4` carries over unchanged (uniform across all 15 cases).
- `memory_allotment` bumped from `2GiB` → `3200MiB` for all cases (matches reference PR; converged image runs Agent + ADP + JVM in one container so 2GiB is tight).
- `optimization_goal` carries over unchanged (`ingress_throughput`, `cpu`, `memory`).
- Quality gates / `checks:` blocks are **not** carried over — none of the existing `dsd_uds_*` cases had them.

#### `lading/lading.yaml` — rewritten (per case)

Two changes vs. existing file:

1. Add second http blackhole on `127.0.0.1:9092` for the Agent's `process_dd_url`.
2. Add second prometheus target at `http://127.0.0.1:5000/telemetry` for Agent core telemetry, alongside the existing ADP target at `127.0.0.1:5102/scrape` (renamed to `prometheus` with `tags: { sub_agent: adp }`).
3. UDS path stays `/tmp/dsd.socket` (matches `datadog.yaml`'s `dogstatsd_socket`).

Generator block (dogstatsd shape, contexts, byte rate, etc.) stays per-case unchanged.

```yaml
# Example: dsd_uds_10mb_3k_contexts_throughput
blackhole:
  - http:
      binding_addr: "127.0.0.1:9091"
  - http:
      binding_addr: "127.0.0.1:9092"
generator:
  - unix_datagram:
      seed: [...]   # unchanged from existing file
      path: "/tmp/dsd.socket"
      block_cache_method: Fixed
      variant:
        dogstatsd:
          # ... existing per-case shape carries over verbatim ...
      maximum_prebuild_cache_size_bytes: 500 Mb
      bytes_per_second: 10 MiB

target_metrics:
  - prometheus:
      uri: "http://127.0.0.1:5102/scrape"
      tags:
        sub_agent: "adp"
  - prometheus:
      uri: "http://127.0.0.1:5000/telemetry"
      tags:
        sub_agent: "core"
```

### `.gitlab/benchmark.yml`

Three classes of change:

1. **Add `AGENT_IMG` env in `.setup-smp-env`-using jobs** (referenced by both build-agent jobs):

   ```yaml
   # https://github.com/DataDog/datadog-agent/pull/49676/changes/15f1e04a4d7b07854ae372d50350af0889c25582
   - export AGENT_IMG=docker.io/datadog/agent-dev:luke-configurable-aggregation-15f1e04a-full
   ```

   Defined inline in each job that needs it (matches reference PR's `STEPHEN_AGENT_IMG` pattern) — not added globally to `.setup-smp-env` since not all jobs need it.

2. **Add two new build jobs**:

   - `build-agent-adp-baseline-image`: `docker pull ${AGENT_IMG}`, retag as `${SMP_ECR_HOST}/${SMP_TEAM_ID}-saluki:agent-adp-${CI_PIPELINE_ID}-baseline-15f1e04a`, push, write `BASELINE_AGENT_SHA=15f1e04a` + `BASELINE_AGENT_IMG=...` to dotenv.
   - `build-agent-adp-comparison-image`: `docker buildx build` `Dockerfile.datadog-agent` with `--build-arg DD_AGENT_IMAGE=${AGENT_IMG}` + `--build-arg ADP_IMAGE=${COMPARISON_ADP_IMG}` + the `DD_DATA_PLANE_*` / `ADP_DD_DATA_PLANE_TELEMETRY_*` build args. Depends on `build-adp-comparison-image`. Tags as `agent-adp-${CI_PIPELINE_ID}-comparison-${CI_COMMIT_SHA}`.

   Both jobs use the same `rules:` `changes:` matcher pattern as reference PR but adapted: `test/smp/regression/adp/**/*` (not `agent-adp-tagfilter`).

3. **Replace `run-benchmarks-adp`** to consume the new agent images:

   - `--baseline-image ${BASELINE_AGENT_IMG}` (was `${BASELINE_ADP_IMG}`)
   - `--comparison-image ${COMPARISON_AGENT_IMG}` (was `${COMPARISON_ADP_IMG}`)
   - `--baseline-sha ${BASELINE_AGENT_SHA}`, `--comparison-sha ${COMPARISON_AGENT_SHA}`
   - `--target-config-dir ./test/smp/regression/adp/` (unchanged)
   - `needs:` switches to `build-agent-adp-baseline-image` + `build-agent-adp-comparison-image`.
   - PR comment header: `"Regression Detector (Agent vs Agent+ADP, 1Hz dogstatsd)"`.

`build-adp-baseline-image` stays — `binary-size-analysis` still depends on it for the bloaty diff. `build-adp-comparison-image` also stays (feeds the converged image build).

### `docker/Dockerfile.datadog-agent`

Add ARGs (top of file, before first `FROM`) and re-declare in the final stage:

```
ARG DD_DATA_PLANE_ENABLED=
ARG DD_DATA_PLANE_STANDALONE_MODE=
ARG DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=
ARG DD_DATA_PLANE_REMOTE_AGENT_ENABLED=
ARG DD_DATA_PLANE_DOGSTATSD_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=
```

And `ENV` lines bridging each ARG to runtime env. Drop `DD_CLUSTER_AGENT_CLUSTER_TAGGER_GRPC_MAX_MESSAGE_SIZE`, `ADP_DD_AGGREGATE_CONTEXT_LIMIT`, `ADP_DD_DOGSTATSD_STRING_INTERNER_SIZE` — tag-filterlist-specific tuning, not relevant here.

Comparison image build invokes:

```
--build-arg DD_DATA_PLANE_ENABLED=true
--build-arg DD_DATA_PLANE_STANDALONE_MODE=false
--build-arg DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=true
--build-arg DD_DATA_PLANE_REMOTE_AGENT_ENABLED=true
--build-arg DD_DATA_PLANE_DOGSTATSD_ENABLED=true
--build-arg ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=true
--build-arg ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=tcp://127.0.0.1:5102
```

## Data flow

```
lading (UDS dgram → /tmp/dsd.socket) ──→ Agent dogstatsd (port 0, UDS only)
                                              │
                                              │  (comparison only) DD_DATA_PLANE_DOGSTATSD_ENABLED=true
                                              ▼
                                            ADP (config-stream gRPC → datadog.yaml from Agent)
                                              │
                                              ▼  http://127.0.0.1:9091 (lading blackhole)
                                            metrics intake

Agent core telemetry          → http://127.0.0.1:5000/telemetry (lading prometheus scrape)
ADP telemetry (comparison)    → http://127.0.0.1:5102/scrape    (lading prometheus scrape)
process-agent dd_url          → http://127.0.0.1:9092           (lading blackhole, drops)
```

Aggregation: with `aggregator_bucket_size_seconds: 1`, both Agent core and ADP bucket dogstatsd into 1-second windows. Pass-through (pre-aggregated, client-timestamped) metrics stay pinned to the default 10s rate interval per PR #1459's invariant.

## Risks / open questions

1. **ADP env-var alias support**. PR #1459 adds `aggregator_bucket_size_seconds` as a serde alias on the `aggregate_window_duration` field. Saluki's config-stream path replicates `datadog.yaml` keys to ADP, so setting it there is reliable. Setting it as a `DD_AGGREGATOR_BUCKET_SIZE_SECONDS` env var may or may not work depending on saluki's env-var handler — we sidestep by using `datadog.yaml` only.
2. **First-run cost**. 15 SMP cases is the largest dsd_uds suite Saluki has run. The 1h timeout on `run-benchmarks-adp` should still be enough (existing OTLP+quality-gates run in similar time), but worth watching the first run.
3. **Image churn**. PR #1459 is open — base branch may shift. Stacking minimizes the rebase surface to the saluki source diff (which we don't touch).

## Test plan

- Open PR as draft.
- CI runs: `build-adp-baseline-image`, `build-adp-comparison-image`, `build-agent-adp-baseline-image`, `build-agent-adp-comparison-image`, `run-benchmarks-adp`, `binary-size-analysis`.
- SMP report posted as PR comment under header `"Regression Detector (Agent vs Agent+ADP, 1Hz dogstatsd)"`.
- Manual review of report: confirm comparison image actually ran ADP (look for ADP-side metrics in the report), confirm 1Hz bucketing visible (e.g., flush rate metrics, output volume).

## Out of scope (defer)

- Re-adding standalone-ADP regression suite (separate dir or follow-up PR).
- Correctness + integration smoke tests for converged image at 1Hz.
- Tuning ADP-internal knobs to match Agent behavior at 1Hz.
- Multi-rate sweep (0.5Hz, 2Hz, etc.) — could be a follow-up if 1Hz signal is interesting.
