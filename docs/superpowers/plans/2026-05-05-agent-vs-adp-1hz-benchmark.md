# Agent-vs-ADP 1Hz dogstatsd benchmark — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an SMP regression benchmark comparing the Datadog Agent (baseline) against a converged Agent+ADP image (comparison) for raw dogstatsd ingest at a 1Hz aggregation bucket interval. Stacks on PR #1459.

**Architecture:** Reuse `test/smp/regression/adp/` in place — delete OTLP + quality_gates cases, rewrite the 15 `dsd_uds_*` cases to drive the Datadog Agent entrypoint with a `datadog.yaml` that pins `aggregator_bucket_size_seconds: 1`. Add two new GitLab jobs (`build-agent-adp-baseline-image`, `build-agent-adp-comparison-image`) and repoint `run-benchmarks-adp` to consume them. Extend `Dockerfile.datadog-agent` with `DD_DATA_PLANE_*` build args.

**Tech Stack:** SMP (single-machine-performance) regression harness, lading load generator, GitLab CI, Docker buildx, Datadog Agent + ADP.

**Spec:** [`docs/superpowers/specs/2026-05-05-agent-vs-adp-1hz-benchmark-design.md`](../specs/2026-05-05-agent-vs-adp-1hz-benchmark-design.md)

---

## Pre-flight

Source branch + base. PR #1459 (`luke/configurable-aggregation`, head `06f3965a2a00e68f03e1462655d6eb330437a3b7`) is the base. New branch is `jszwedko/agent-vs-adp-1hz-benchmark`.

This plan assumes the executing agent is already on `jszwedko/agent-vs-adp-1hz-benchmark`, branched from `luke/configurable-aggregation`. If not, run:

```bash
git fetch origin luke/configurable-aggregation
git switch -c jszwedko/agent-vs-adp-1hz-benchmark origin/luke/configurable-aggregation
```

The list of `dsd_uds_*` cases (used throughout the plan):

```
dsd_uds_512kb_3k_contexts_cpu        dsd_uds_1mb_3k_contexts_cpu          dsd_uds_10mb_3k_contexts_cpu         dsd_uds_100mb_3k_contexts_cpu        dsd_uds_500mb_3k_contexts_cpu
dsd_uds_512kb_3k_contexts_memory     dsd_uds_1mb_3k_contexts_memory       dsd_uds_10mb_3k_contexts_memory      dsd_uds_100mb_3k_contexts_memory     dsd_uds_500mb_3k_contexts_memory
dsd_uds_512kb_3k_contexts_throughput dsd_uds_1mb_3k_contexts_throughput   dsd_uds_10mb_3k_contexts_throughput  dsd_uds_100mb_3k_contexts_throughput dsd_uds_500mb_3k_contexts_throughput
```

The list of `bytes_per_second` values (per size prefix):

```
512kb  → 512 KiB
1mb    →   1 MiB
10mb   →  10 MiB
100mb  → 100 MiB
500mb  → 500 MiB
```

---

## Task 1: Delete OTLP and quality_gates cases

**Files:**
- Delete: `test/smp/regression/adp/cases/otlp_ingest_logs_5mb_*` (3 dirs)
- Delete: `test/smp/regression/adp/cases/otlp_ingest_metrics_5mb_*` (3 dirs)
- Delete: `test/smp/regression/adp/cases/otlp_ingest_traces_5mb_*` (3 dirs)
- Delete: `test/smp/regression/adp/cases/otlp_ingest_traces_ottl_filtering_5mb_*` (3 dirs)
- Delete: `test/smp/regression/adp/cases/otlp_ingest_traces_ottl_transform_5mb_*` (3 dirs)
- Delete: `test/smp/regression/adp/cases/quality_gates_rss_dsd_*` (4 dirs)
- Delete: `test/smp/regression/adp/cases/quality_gates_rss_idle` (1 dir)

20 directories total.

- [ ] **Step 1: Remove the directories**

```bash
git rm -r \
  test/smp/regression/adp/cases/otlp_ingest_logs_5mb_cpu \
  test/smp/regression/adp/cases/otlp_ingest_logs_5mb_memory \
  test/smp/regression/adp/cases/otlp_ingest_logs_5mb_throughput \
  test/smp/regression/adp/cases/otlp_ingest_metrics_5mb_cpu \
  test/smp/regression/adp/cases/otlp_ingest_metrics_5mb_memory \
  test/smp/regression/adp/cases/otlp_ingest_metrics_5mb_throughput \
  test/smp/regression/adp/cases/otlp_ingest_traces_5mb_cpu \
  test/smp/regression/adp/cases/otlp_ingest_traces_5mb_memory \
  test/smp/regression/adp/cases/otlp_ingest_traces_5mb_throughput \
  test/smp/regression/adp/cases/otlp_ingest_traces_ottl_filtering_5mb_cpu \
  test/smp/regression/adp/cases/otlp_ingest_traces_ottl_filtering_5mb_memory \
  test/smp/regression/adp/cases/otlp_ingest_traces_ottl_filtering_5mb_throughput \
  test/smp/regression/adp/cases/otlp_ingest_traces_ottl_transform_5mb_cpu \
  test/smp/regression/adp/cases/otlp_ingest_traces_ottl_transform_5mb_memory \
  test/smp/regression/adp/cases/otlp_ingest_traces_ottl_transform_5mb_throughput \
  test/smp/regression/adp/cases/quality_gates_rss_dsd_heavy \
  test/smp/regression/adp/cases/quality_gates_rss_dsd_low \
  test/smp/regression/adp/cases/quality_gates_rss_dsd_medium \
  test/smp/regression/adp/cases/quality_gates_rss_dsd_ultraheavy \
  test/smp/regression/adp/cases/quality_gates_rss_idle
```

- [ ] **Step 2: Verify only dsd_uds_* cases remain**

Run: `ls test/smp/regression/adp/cases/`

Expected output (15 lines, alphabetical):
```
dsd_uds_100mb_3k_contexts_cpu
dsd_uds_100mb_3k_contexts_memory
dsd_uds_100mb_3k_contexts_throughput
dsd_uds_10mb_3k_contexts_cpu
dsd_uds_10mb_3k_contexts_memory
dsd_uds_10mb_3k_contexts_throughput
dsd_uds_1mb_3k_contexts_cpu
dsd_uds_1mb_3k_contexts_memory
dsd_uds_1mb_3k_contexts_throughput
dsd_uds_500mb_3k_contexts_cpu
dsd_uds_500mb_3k_contexts_memory
dsd_uds_500mb_3k_contexts_throughput
dsd_uds_512kb_3k_contexts_cpu
dsd_uds_512kb_3k_contexts_memory
dsd_uds_512kb_3k_contexts_throughput
```

- [ ] **Step 3: Commit**

```bash
git commit -m "test(smp): remove OTLP and quality_gates cases from adp suite

The adp/ SMP target directory will be repurposed for an Agent-vs-Agent+ADP
1Hz dogstatsd comparison. Only the dsd_uds_* cases are kept; everything
else moves out of scope on this branch.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Delete the per-case `agent-data-plane/` subdirs

In the converged image, ADP boots via the Agent's s6 supervisor (not via `target.command`) and consumes `DD_DATA_PLANE_*` env vars + `datadog.yaml` over config-stream. The standalone-mode `empty.yaml` and `cert.pem` files are no longer referenced — delete them.

**Files:**
- Delete: `test/smp/regression/adp/cases/dsd_uds_*/agent-data-plane/empty.yaml` (15)
- Delete: `test/smp/regression/adp/cases/dsd_uds_*/agent-data-plane/cert.pem` (15)
- Delete: `test/smp/regression/adp/cases/dsd_uds_*/agent-data-plane/` (15 empty dirs)

- [ ] **Step 1: Remove the agent-data-plane/ subdirs**

```bash
git rm -r test/smp/regression/adp/cases/dsd_uds_*/agent-data-plane
```

- [ ] **Step 2: Verify each case now has only experiment.yaml + lading/**

Run: `ls test/smp/regression/adp/cases/dsd_uds_10mb_3k_contexts_throughput/`

Expected:
```
experiment.yaml
lading
```

- [ ] **Step 3: Commit**

```bash
git commit -m "test(smp): drop standalone-ADP files from dsd_uds cases

The converged Agent+ADP image boots ADP via the Agent's s6 supervisor
(not via target.command), so /etc/agent-data-plane/empty.yaml and the
IPC cert are no longer referenced. Remove them.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Add `datadog-agent/datadog.yaml` to each dsd_uds case

The same `datadog.yaml` content goes in all 15 cases. The 1Hz knob lives here so both Agent core and ADP (via config-stream) read it from a single source of truth.

**Files:**
- Create: `test/smp/regression/adp/cases/dsd_uds_*/datadog-agent/datadog.yaml` (15 files, identical content)

- [ ] **Step 1: Write the shared content to a temp file**

```bash
cat > /tmp/datadog-agent-1hz.yaml <<'EOF'
auth_token_file_path: /tmp/agent-auth-token
hostname: smp-regression

dd_url: http://127.0.0.1:9091
process_config.process_dd_url: http://localhost:9092

telemetry.enabled: true
telemetry.checks: '*'

# Disable cloud detection. This stops the Agent from poking around the
# execution environment & network. This is particularly important if the
# target has network access.
cloud_provider_metadata: []

dogstatsd_socket: '/tmp/dsd.socket'
dogstatsd_origin_detection: true

# 1Hz dogstatsd aggregation. Honored by Agent core directly and by ADP via
# the alias added in https://github.com/DataDog/saluki/pull/1459.
aggregator_bucket_size_seconds: 1
EOF
```

- [ ] **Step 2: Copy it into each case dir**

```bash
for d in test/smp/regression/adp/cases/dsd_uds_*; do
  mkdir -p "$d/datadog-agent"
  cp /tmp/datadog-agent-1hz.yaml "$d/datadog-agent/datadog.yaml"
done
```

- [ ] **Step 3: Verify all 15 files exist and are identical**

Run:
```bash
ls test/smp/regression/adp/cases/dsd_uds_*/datadog-agent/datadog.yaml | wc -l
md5sum test/smp/regression/adp/cases/dsd_uds_*/datadog-agent/datadog.yaml | awk '{print $1}' | sort -u | wc -l
```

Expected:
```
15
1
```
(15 files, all sharing one md5.)

- [ ] **Step 4: Commit**

```bash
git add test/smp/regression/adp/cases/dsd_uds_*/datadog-agent/datadog.yaml
git commit -m "test(smp): add datadog.yaml with 1Hz bucket setting to dsd_uds cases

Single source of truth for aggregator_bucket_size_seconds. Agent core
reads the file directly; ADP reads it via config-stream gRPC when
DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=true.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Rewrite `experiment.yaml` for each dsd_uds case

The new `experiment.yaml` switches the target from ADP-standalone to the Datadog Agent entrypoint. Only `optimization_goal` and the per-case `experiment` profiling tag vary. `cpu_allotment` stays at `4`; `memory_allotment` bumps from `2GiB` to `3200MiB` (converged image holds Agent + ADP + JVM).

**Files:**
- Modify: `test/smp/regression/adp/cases/dsd_uds_*/experiment.yaml` (15 files)

- [ ] **Step 1: Define a Python helper to render each experiment.yaml**

```bash
cat > /tmp/render_experiment.py <<'PYEOF'
import sys
from pathlib import Path

CASES = [
    "dsd_uds_512kb_3k_contexts_cpu",
    "dsd_uds_512kb_3k_contexts_memory",
    "dsd_uds_512kb_3k_contexts_throughput",
    "dsd_uds_1mb_3k_contexts_cpu",
    "dsd_uds_1mb_3k_contexts_memory",
    "dsd_uds_1mb_3k_contexts_throughput",
    "dsd_uds_10mb_3k_contexts_cpu",
    "dsd_uds_10mb_3k_contexts_memory",
    "dsd_uds_10mb_3k_contexts_throughput",
    "dsd_uds_100mb_3k_contexts_cpu",
    "dsd_uds_100mb_3k_contexts_memory",
    "dsd_uds_100mb_3k_contexts_throughput",
    "dsd_uds_500mb_3k_contexts_cpu",
    "dsd_uds_500mb_3k_contexts_memory",
    "dsd_uds_500mb_3k_contexts_throughput",
]

GOAL_BY_SUFFIX = {
    "cpu": "cpu",
    "memory": "memory",
    "throughput": "ingress_throughput",
}

TEMPLATE = """optimization_goal: {goal}
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
    DD_INTERNAL_PROFILING_EXTRA_TAGS: experiment:{case}

report_links:
  - text: "bounds checks dashboard"
    link: "https://app.datadoghq.com/dashboard/vz3-jd5-bdi?fromUser=true&refresh_mode=paused&tpl_var_experiment%5B0%5D={{{{ experiment }}}}&tpl_var_job_id%5B0%5D={{{{ job_id }}}}&tpl_var_run-id%5B0%5D={{{{ job_id }}}}&view=spans&from_ts={{{{ start_time_ms }}}}&to_ts={{{{ end_time_ms }}}}&live=false"
"""

base = Path("test/smp/regression/adp/cases")
for case in CASES:
    suffix = case.rsplit("_", 1)[1]
    goal = GOAL_BY_SUFFIX[suffix]
    out = base / case / "experiment.yaml"
    out.write_text(TEMPLATE.format(goal=goal, case=case))
    print(f"wrote {out}")
PYEOF
```

- [ ] **Step 2: Run the renderer**

```bash
python3 /tmp/render_experiment.py
```

Expected: 15 lines of `wrote test/smp/regression/adp/cases/dsd_uds_*/experiment.yaml`.

- [ ] **Step 3: Spot-check three cases (different sizes / goals)**

Run:
```bash
grep -E "optimization_goal|memory_allotment|EXTRA_TAGS|target:|name: datadog-agent" \
  test/smp/regression/adp/cases/dsd_uds_512kb_3k_contexts_cpu/experiment.yaml \
  test/smp/regression/adp/cases/dsd_uds_10mb_3k_contexts_throughput/experiment.yaml \
  test/smp/regression/adp/cases/dsd_uds_500mb_3k_contexts_memory/experiment.yaml
```

Expected (per file): `optimization_goal:` matches the case suffix, `memory_allotment: 3200MiB`, `name: datadog-agent`, EXTRA_TAGS matches the case name.

- [ ] **Step 4: Verify count and uniformity**

Run:
```bash
grep -c "name: datadog-agent" test/smp/regression/adp/cases/dsd_uds_*/experiment.yaml | awk -F: '{s+=$2} END {print s}'
grep -c "memory_allotment: 3200MiB" test/smp/regression/adp/cases/dsd_uds_*/experiment.yaml | awk -F: '{s+=$2} END {print s}'
```

Expected: `15` then `15`.

- [ ] **Step 5: Commit**

```bash
git add test/smp/regression/adp/cases/dsd_uds_*/experiment.yaml
git commit -m "test(smp): switch dsd_uds experiments to Datadog Agent entrypoint

Targets the Agent's /bin/entrypoint.sh instead of the ADP binary directly,
so the same case files drive both the Datadog Agent baseline image and the
converged Agent+ADP comparison image. Memory allotment bumped from 2GiB to
3200MiB to fit Agent + ADP + JVM in one container.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Rewrite `lading.yaml` for each dsd_uds case

Two structural changes vs. existing files:

1. Add second http blackhole on `127.0.0.1:9092` (process-agent's `process_dd_url`).
2. Add second prometheus target at `http://127.0.0.1:5000/telemetry` (Agent core telemetry), tag the existing `5102/scrape` target as `sub_agent: adp`.
3. UDS path switches from `/tmp/adp-dogstatsd-dgram.sock` → `/tmp/dsd.socket` to match the new `datadog.yaml`.

The dogstatsd `variant:` block (contexts, name_length, tag_length, tags_per_msg, multivalue, kind/metric weights) is identical across all 15 cases. Only `bytes_per_second` varies.

**Files:**
- Modify: `test/smp/regression/adp/cases/dsd_uds_*/lading/lading.yaml` (15 files)

- [ ] **Step 1: Define a Python helper to render each lading.yaml**

```bash
cat > /tmp/render_lading.py <<'PYEOF'
import re
from pathlib import Path

BPS_BY_PREFIX = {
    "512kb": "512 KiB",
    "1mb":   "1 MiB",
    "10mb":  "10 MiB",
    "100mb": "100 MiB",
    "500mb": "500 MiB",
}

TEMPLATE = """blackhole:
  - http:
      binding_addr: "127.0.0.1:9091"
  - http:
      binding_addr: "127.0.0.1:9092"
target_metrics:
  - prometheus:
      uri: "http://127.0.0.1:5102/scrape"
      tags:
        sub_agent: "adp"
  - prometheus:
      uri: "http://127.0.0.1:5000/telemetry"
      tags:
        sub_agent: "core"
generator:
  - unix_datagram:
      seed: [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103, 107, 109, 113, 127, 131]
      path: "/tmp/dsd.socket"
      block_cache_method: Fixed
      variant:
        dogstatsd:
          contexts:
            inclusive:
              min: 3000
              max: 3001
          name_length:
            inclusive:
              min: 1
              max: 200
          tag_length:
            inclusive:
              min: 3
              max: 150
          tags_per_msg:
            inclusive:
              min: 2
              max: 50
          multivalue_count:
            inclusive:
              min: 2
              max: 32
          multivalue_pack_probability: 0.08
          kind_weights:
            metric: 100
            event: 0
            service_check: 0
          metric_weights:
            count: 208
            gauge: 66
            timer: 0
            distribution: 72
            set: 9
            histogram: 1
      maximum_prebuild_cache_size_bytes: "500 Mb"
      bytes_per_second: "{bps}"
"""

base = Path("test/smp/regression/adp/cases")
for case_dir in sorted(base.glob("dsd_uds_*")):
    m = re.match(r"dsd_uds_([0-9a-z]+)_3k_contexts_", case_dir.name)
    if not m:
        raise SystemExit(f"unexpected case dir name: {case_dir.name}")
    bps = BPS_BY_PREFIX[m.group(1)]
    out = case_dir / "lading" / "lading.yaml"
    out.write_text(TEMPLATE.format(bps=bps))
    print(f"wrote {out}  ({bps})")
PYEOF
```

- [ ] **Step 2: Run the renderer**

```bash
python3 /tmp/render_lading.py
```

Expected: 15 lines, each printing the case's `bytes_per_second`.

- [ ] **Step 3: Spot-check `bytes_per_second` per size prefix**

Run:
```bash
for sz in 512kb 1mb 10mb 100mb 500mb; do
  grep "bytes_per_second" "test/smp/regression/adp/cases/dsd_uds_${sz}_3k_contexts_throughput/lading/lading.yaml"
done
```

Expected output:
```
      bytes_per_second: "512 KiB"
      bytes_per_second: "1 MiB"
      bytes_per_second: "10 MiB"
      bytes_per_second: "100 MiB"
      bytes_per_second: "500 MiB"
```

- [ ] **Step 4: Verify all 15 files have both blackholes and both prometheus targets**

Run:
```bash
grep -c "127.0.0.1:9092" test/smp/regression/adp/cases/dsd_uds_*/lading/lading.yaml | awk -F: '{s+=$2} END {print s}'
grep -c "127.0.0.1:5000/telemetry" test/smp/regression/adp/cases/dsd_uds_*/lading/lading.yaml | awk -F: '{s+=$2} END {print s}'
grep -c '/tmp/dsd.socket' test/smp/regression/adp/cases/dsd_uds_*/lading/lading.yaml | awk -F: '{s+=$2} END {print s}'
```

Expected: `15` then `15` then `15`.

- [ ] **Step 5: Commit**

```bash
git add test/smp/regression/adp/cases/dsd_uds_*/lading/lading.yaml
git commit -m "test(smp): adapt dsd_uds lading configs for converged Agent+ADP target

- Add 127.0.0.1:9092 blackhole for the process-agent's process_dd_url.
- Scrape Agent core telemetry on 127.0.0.1:5000 alongside ADP on :5102,
  tagging both with sub_agent so the SMP report can attribute metrics.
- Switch dogstatsd UDS path to /tmp/dsd.socket to match datadog.yaml.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Extend `docker/Dockerfile.datadog-agent` with `DD_DATA_PLANE_*` build args

The converged Agent+ADP image needs the runtime ENV vars baked in so ADP boots in non-standalone, config-stream-driven mode the moment the container starts. Build args provide the mechanism; the comparison build job in Task 8 supplies the values.

**Files:**
- Modify: `docker/Dockerfile.datadog-agent`

- [ ] **Step 1: Read the existing Dockerfile**

Run: `cat docker/Dockerfile.datadog-agent`

Expected current top:
```
ARG DD_AGENT_VERSION=7.78.1-jmx
ARG DD_AGENT_IMAGE=registry.datadoghq.com/agent:${DD_AGENT_VERSION}
ARG ADP_IMAGE=saluki-images/agent-data-plane:testing-devel

# Reference the ADP image so we can copy the relevant bits out of it.
FROM ${ADP_IMAGE} AS adp
...
```

- [ ] **Step 2: Apply the edit**

Replace the top of the file (above the `# Reference the ADP image...` comment) and add a new ARG/ENV block in the final stage. Resulting file should look like:

```dockerfile
ARG DD_AGENT_VERSION=7.78.1-jmx
ARG DD_AGENT_IMAGE=registry.datadoghq.com/agent:${DD_AGENT_VERSION}
ARG ADP_IMAGE=saluki-images/agent-data-plane:testing-devel
ARG DD_DATA_PLANE_ENABLED=
ARG DD_DATA_PLANE_STANDALONE_MODE=
ARG DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=
ARG DD_DATA_PLANE_REMOTE_AGENT_ENABLED=
ARG DD_DATA_PLANE_DOGSTATSD_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=

# Reference the ADP image so we can copy the relevant bits out of it.
FROM ${ADP_IMAGE} AS adp

# Build off of the official Datadog Agent image.
FROM ${DD_AGENT_IMAGE}

ARG DD_DATA_PLANE_ENABLED=
ARG DD_DATA_PLANE_STANDALONE_MODE=
ARG DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=
ARG DD_DATA_PLANE_REMOTE_AGENT_ENABLED=
ARG DD_DATA_PLANE_DOGSTATSD_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=

ENV DD_DATA_PLANE_ENABLED=${DD_DATA_PLANE_ENABLED} \
    DD_DATA_PLANE_STANDALONE_MODE=${DD_DATA_PLANE_STANDALONE_MODE} \
    DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=${DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT} \
    DD_DATA_PLANE_REMOTE_AGENT_ENABLED=${DD_DATA_PLANE_REMOTE_AGENT_ENABLED} \
    DD_DATA_PLANE_DOGSTATSD_ENABLED=${DD_DATA_PLANE_DOGSTATSD_ENABLED} \
    ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=${ADP_DD_DATA_PLANE_TELEMETRY_ENABLED} \
    ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=${ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR}

# Copy the ADP binary and all of the required licensing bits.
COPY --from=adp /usr/local/bin/agent-data-plane /opt/datadog-agent/embedded/bin/agent-data-plane
COPY --from=adp /opt/datadog/agent-data-plane /opt/datadog/agent-data-plane

# Add the s6 service files for Agent Data Plane.
#
# ADP will only run when the `DD_DATA_PLANE_ENABLED` environment variable is set to `true`.
COPY docker/cont-init.d /etc/cont-init.d/
COPY docker/s6-services /etc/services.d/

COPY --chmod=755 docker/entrypoint.sh /entrypoint.sh

# Remove default check configuration to ensure no checks end up running by default.
RUN rm -rf /etc/datadog-agent/conf.d/*

# Use an updated healthcheck since the default healthcheck from `datadog/agent` is wildly overconservative.
HEALTHCHECK --interval=60s --timeout=5s --retries=2 --start-period=30s --start-interval=1s \
    CMD ["/probe.sh"]

# Everything else we'll leave at the defaults of the Datadog Agent image.
```

Use the Edit tool with these two old/new pairs (or rewrite via Write — file is small):

Edit 1 — old:
```
ARG ADP_IMAGE=saluki-images/agent-data-plane:testing-devel

# Reference the ADP image so we can copy the relevant bits out of it.
```

Edit 1 — new:
```
ARG ADP_IMAGE=saluki-images/agent-data-plane:testing-devel
ARG DD_DATA_PLANE_ENABLED=
ARG DD_DATA_PLANE_STANDALONE_MODE=
ARG DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=
ARG DD_DATA_PLANE_REMOTE_AGENT_ENABLED=
ARG DD_DATA_PLANE_DOGSTATSD_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=

# Reference the ADP image so we can copy the relevant bits out of it.
```

Edit 2 — old:
```
# Build off of the official Datadog Agent image.
FROM ${DD_AGENT_IMAGE}

# Copy the ADP binary and all of the required licensing bits.
```

Edit 2 — new:
```
# Build off of the official Datadog Agent image.
FROM ${DD_AGENT_IMAGE}

ARG DD_DATA_PLANE_ENABLED=
ARG DD_DATA_PLANE_STANDALONE_MODE=
ARG DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=
ARG DD_DATA_PLANE_REMOTE_AGENT_ENABLED=
ARG DD_DATA_PLANE_DOGSTATSD_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=
ARG ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=

ENV DD_DATA_PLANE_ENABLED=${DD_DATA_PLANE_ENABLED} \
    DD_DATA_PLANE_STANDALONE_MODE=${DD_DATA_PLANE_STANDALONE_MODE} \
    DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=${DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT} \
    DD_DATA_PLANE_REMOTE_AGENT_ENABLED=${DD_DATA_PLANE_REMOTE_AGENT_ENABLED} \
    DD_DATA_PLANE_DOGSTATSD_ENABLED=${DD_DATA_PLANE_DOGSTATSD_ENABLED} \
    ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=${ADP_DD_DATA_PLANE_TELEMETRY_ENABLED} \
    ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=${ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR}

# Copy the ADP binary and all of the required licensing bits.
```

- [ ] **Step 3: Verify the file parses (basic Dockerfile lint)**

Run: `docker buildx build --check -f docker/Dockerfile.datadog-agent .`

(If `docker buildx` is not available, skip — CI will catch parse errors.)

Expected: no parse errors. Warnings about empty default ARG values are fine.

- [ ] **Step 4: Commit**

```bash
git add docker/Dockerfile.datadog-agent
git commit -m "docker(datadog-agent): plumb DD_DATA_PLANE_* build args

Lets the converged Agent+ADP image bake in the env vars ADP needs to boot
in non-standalone, config-stream-driven mode (REMOTE_AGENT_ENABLED=true,
USE_NEW_CONFIG_STREAM_ENDPOINT=true, DOGSTATSD_ENABLED=true). The build
job in .gitlab/benchmark.yml supplies the values for the SMP comparison
image.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Add the two new build jobs to `.gitlab/benchmark.yml`

`build-agent-adp-baseline-image` retags the upstream Datadog Agent dev image. `build-agent-adp-comparison-image` builds the converged Dockerfile from the comparison ADP image.

**Files:**
- Modify: `.gitlab/benchmark.yml`

- [ ] **Step 1: Locate the insertion point**

Run: `grep -n "^run-benchmarks-adp:" .gitlab/benchmark.yml`

Expected: a single line like `116:run-benchmarks-adp:` (line number may differ — what matters is that this is the existing job, and the two new build jobs go ABOVE it).

- [ ] **Step 2: Insert the two new jobs**

Use the Edit tool with the following old/new pair. The `old_string` matches the start of `run-benchmarks-adp:` (the job whose run script we'll repoint in Task 8). The `new_string` prepends the two new build jobs.

Edit — old:
```
run-benchmarks-adp:
  stage: benchmark
```

Edit — new:
```
build-agent-adp-baseline-image:
  extends: .build-common-variables
  stage: benchmark
  tags: ["docker-in-docker:amd64"]
  rules:
    - if: !reference [.on_mq_branch, rules, if]
      when: never
    - if: !reference [.on_development_branch, rules, if]
  image: "${SALUKI_SMP_CI_IMAGE}"
  needs: []
  retry: 2
  timeout: 15m
  before_script:
    - *setup-smp-env
  script:
    - ./test/smp/configure-smp-aws-credentials.sh
    # See https://github.com/DataDog/datadog-agent/pull/49676/changes/15f1e04a4d7b07854ae372d50350af0889c25582
    - export AGENT_SHA=15f1e04a
    - export AGENT_IMG=docker.io/datadog/agent-dev:15f1e04a-py3-jmx
    - export AGENT_ADP_IMG_BASE="${SMP_ECR_HOST}/${SMP_TEAM_ID}-saluki:agent-adp-${CI_PIPELINE_ID}"
    - export BASELINE_AGENT_IMG="${AGENT_ADP_IMG_BASE}-baseline-${AGENT_SHA}"
    - aws ecr get-login-password --region us-west-2 --profile ${AWS_NAMED_PROFILE} | docker login --username AWS --password-stdin ${SMP_ECR_HOST}
    - docker pull ${AGENT_IMG}
    - docker tag ${AGENT_IMG} ${BASELINE_AGENT_IMG}
    - docker push ${BASELINE_AGENT_IMG}
    - echo "BASELINE_AGENT_SHA=${AGENT_SHA}" >> smp-vars.env
    - echo "BASELINE_AGENT_IMG=${BASELINE_AGENT_IMG}" >> smp-vars.env
  artifacts:
    reports:
      dotenv: smp-vars.env

build-agent-adp-comparison-image:
  extends: .build-common-variables
  stage: benchmark
  tags: ["docker-in-docker:amd64"]
  rules:
    - if: !reference [.on_mq_branch, rules, if]
      when: never
    - if: !reference [.on_development_branch, rules, if]
  image: "${SALUKI_SMP_CI_IMAGE}"
  needs:
    - build-adp-comparison-image
  retry: 2
  timeout: 20m
  before_script:
    - *setup-smp-env
  script:
    - ./test/smp/configure-smp-aws-credentials.sh
    # See https://github.com/DataDog/datadog-agent/pull/49676/changes/15f1e04a4d7b07854ae372d50350af0889c25582
    - export AGENT_IMG=docker.io/datadog/agent-dev:15f1e04a-py3-jmx
    - export AGENT_ADP_IMG_BASE="${SMP_ECR_HOST}/${SMP_TEAM_ID}-saluki:agent-adp-${CI_PIPELINE_ID}"
    - export COMPARISON_AGENT_SHA=${CI_COMMIT_SHA}
    - export COMPARISON_AGENT_IMG="${AGENT_ADP_IMG_BASE}-comparison-${COMPARISON_AGENT_SHA}"
    - git switch --detach ${COMPARISON_AGENT_SHA}
    - aws ecr get-login-password --region us-west-2 --profile ${AWS_NAMED_PROFILE} | docker login --username AWS --password-stdin ${SMP_ECR_HOST}
    - docker buildx create --name agent-adp-builder --driver docker-container --use
    - docker buildx build
      --file ./docker/Dockerfile.datadog-agent
      --tag ${COMPARISON_AGENT_IMG}
      --build-arg DD_AGENT_IMAGE=${AGENT_IMG}
      --build-arg ADP_IMAGE=${COMPARISON_ADP_IMG}
      --build-arg DD_DATA_PLANE_ENABLED=true
      --build-arg DD_DATA_PLANE_STANDALONE_MODE=false
      --build-arg DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT=true
      --build-arg DD_DATA_PLANE_REMOTE_AGENT_ENABLED=true
      --build-arg DD_DATA_PLANE_DOGSTATSD_ENABLED=true
      --build-arg ADP_DD_DATA_PLANE_TELEMETRY_ENABLED=true
      --build-arg ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR=tcp://127.0.0.1:5102
      --label git.repository=${CI_PROJECT_NAME}
      --label git.branch=${CI_COMMIT_REF_NAME}
      --label git.commit=${CI_COMMIT_SHA}
      --label ci.pipeline_id=${CI_PIPELINE_ID}
      --label ci.job_id=${CI_JOB_ID}
      --push
      .
    - echo "COMPARISON_AGENT_SHA=${COMPARISON_AGENT_SHA}" >> smp-vars.env
    - echo "COMPARISON_AGENT_IMG=${COMPARISON_AGENT_IMG}" >> smp-vars.env
  artifacts:
    reports:
      dotenv: smp-vars.env

run-benchmarks-adp:
  stage: benchmark
```

- [ ] **Step 3: Sanity-check the YAML parses**

Run: `python3 -c "import yaml; yaml.safe_load(open('.gitlab/benchmark.yml'))"`

Expected: no output (valid YAML).

- [ ] **Step 4: Verify the new jobs exist**

Run: `grep -E "^(build-agent-adp-baseline-image|build-agent-adp-comparison-image|run-benchmarks-adp):" .gitlab/benchmark.yml`

Expected:
```
build-agent-adp-baseline-image:
build-agent-adp-comparison-image:
run-benchmarks-adp:
```

- [ ] **Step 5: Commit**

```bash
git add .gitlab/benchmark.yml
git commit -m "ci(benchmark): add Agent+ADP image build jobs for SMP comparison

build-agent-adp-baseline-image retags the upstream Datadog Agent dev
image (15f1e04a-py3-jmx, from datadog-agent#49676) as the SMP baseline.
build-agent-adp-comparison-image builds Dockerfile.datadog-agent on top
of the comparison ADP image with all DD_DATA_PLANE_* knobs set to drive
non-standalone, config-stream-driven mode.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Repoint `run-benchmarks-adp` to consume the agent images

Switch `--baseline-image` / `--comparison-image` (and the corresponding SHAs and `needs:`) from the ADP-only images to the agent images built in Task 7. Update the PR-comment header so the report's purpose is obvious.

**Files:**
- Modify: `.gitlab/benchmark.yml` (the existing `run-benchmarks-adp` job)

- [ ] **Step 1: Locate and inspect the existing job**

Run: `awk '/^run-benchmarks-adp:/,/^[a-z]/' .gitlab/benchmark.yml | head -60`

(The `awk` prints from the job header to the next top-level YAML key.)

Expected: a `script:` block with `--baseline-image ${BASELINE_ADP_IMG}` and `--comparison-image ${COMPARISON_ADP_IMG}`, `needs: build-adp-baseline-image` + `build-adp-comparison-image`, and a pr-commenter call with header `"Regression Detector (Agent Data Plane)"`.

- [ ] **Step 2: Apply the edits**

Three Edit calls.

Edit 1 — repoint `needs:`. Old:
```
run-benchmarks-adp:
  stage: benchmark
  # Don't run benchmarks unless it's a PR, basically.
  rules:
    - if: !reference [.on_mq_branch, rules, if]
      when: never
    - if: !reference [.on_development_branch, rules, if]
  timeout: 1h
  needs:
    - build-adp-baseline-image
    - build-adp-comparison-image
```

Edit 1 — new:
```
run-benchmarks-adp:
  stage: benchmark
  # Don't run benchmarks unless it's a PR, basically.
  rules:
    - if: !reference [.on_mq_branch, rules, if]
      when: never
    - if: !reference [.on_development_branch, rules, if]
  timeout: 1h
  needs:
    - build-agent-adp-baseline-image
    - build-agent-adp-comparison-image
```

Edit 2 — repoint `--baseline-image` / `--comparison-image` / SHAs. Old:
```
    - ./smp --team-id ${SMP_TEAM_ID} --aws-named-profile ${AWS_NAMED_PROFILE}
      job submit
      --warmup-seconds 0
      --baseline-image ${BASELINE_ADP_IMG}
      --comparison-image ${COMPARISON_ADP_IMG}
      --baseline-sha ${BASELINE_ADP_SHA}
      --comparison-sha ${COMPARISON_ADP_SHA}
      --target-config-dir ./test/smp/regression/adp/
      --submission-metadata submission_metadata
```

Edit 2 — new:
```
    - ./smp --team-id ${SMP_TEAM_ID} --aws-named-profile ${AWS_NAMED_PROFILE}
      job submit
      --warmup-seconds 0
      --baseline-image ${BASELINE_AGENT_IMG}
      --comparison-image ${COMPARISON_AGENT_IMG}
      --baseline-sha ${BASELINE_AGENT_SHA}
      --comparison-sha ${COMPARISON_AGENT_SHA}
      --target-config-dir ./test/smp/regression/adp/
      --submission-metadata submission_metadata
```

Edit 3 — update report header. Old:
```
    - cat outputs/report.md | /usr/local/bin/pr-commenter --for-pr="$CI_COMMIT_REF_NAME" --header="Regression Detector (Agent Data Plane)"
```

Edit 3 — new:
```
    - cat outputs/report.md | /usr/local/bin/pr-commenter --for-pr="$CI_COMMIT_REF_NAME" --header="Regression Detector (Agent vs Agent+ADP, 1Hz dogstatsd)"
```

- [ ] **Step 3: Sanity-check the YAML parses**

Run: `python3 -c "import yaml; yaml.safe_load(open('.gitlab/benchmark.yml'))"`

Expected: no output.

- [ ] **Step 4: Verify the wiring**

Run:
```bash
awk '/^run-benchmarks-adp:/,/^[a-z]/' .gitlab/benchmark.yml | grep -E "needs:|--baseline-image|--comparison-image|--baseline-sha|--comparison-sha|--header="
```

Expected (in this order):
```
  needs:
      --baseline-image ${BASELINE_AGENT_IMG}
      --comparison-image ${COMPARISON_AGENT_IMG}
      --baseline-sha ${BASELINE_AGENT_SHA}
      --comparison-sha ${COMPARISON_AGENT_SHA}
    - cat outputs/report.md | /usr/local/bin/pr-commenter --for-pr="$CI_COMMIT_REF_NAME" --header="Regression Detector (Agent vs Agent+ADP, 1Hz dogstatsd)"
```

(The `needs:` itself has no inline value; verify the next two list items match by also running:)

```bash
awk '/^run-benchmarks-adp:/,/^[a-z]/' .gitlab/benchmark.yml | grep -A 2 "^  needs:"
```

Expected:
```
  needs:
    - build-agent-adp-baseline-image
    - build-agent-adp-comparison-image
```

- [ ] **Step 5: Commit**

```bash
git add .gitlab/benchmark.yml
git commit -m "ci(benchmark): repoint run-benchmarks-adp at converged agent images

Now consumes BASELINE_AGENT_IMG (vanilla Datadog Agent dev image) and
COMPARISON_AGENT_IMG (converged Agent+ADP image) from the new build
jobs. Same SMP target dir; the dsd_uds cases are now driven through the
Agent entrypoint with aggregator_bucket_size_seconds: 1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Push branch and open draft PR targeting `luke/configurable-aggregation`

The PR is intentionally a draft — review hinges on the SMP report posted by `run-benchmarks-adp`, which doesn't exist until CI runs.

**Files:**
- None (Git + GitHub only)

- [ ] **Step 1: Confirm branch state**

Run:
```bash
git status
git log --oneline origin/luke/configurable-aggregation..HEAD
```

Expected: clean working tree; 8 commits ahead of `origin/luke/configurable-aggregation` (one per Task 1–8).

- [ ] **Step 2: Push the branch**

```bash
git push -u origin jszwedko/agent-vs-adp-1hz-benchmark
```

- [ ] **Step 3: Open the draft PR**

```bash
gh pr create \
  --draft \
  --base luke/configurable-aggregation \
  --title "test(smp): Agent vs Agent+ADP 1Hz dogstatsd benchmark" \
  --body "$(cat <<'EOF'
## Summary

Adds an SMP regression benchmark comparing the Datadog Agent (baseline) against a converged Agent+Agent-Data-Plane image (comparison) for raw dogstatsd ingest at a 1-second aggregation bucket interval. Stacked on #1459 so the single `aggregator_bucket_size_seconds: 1` knob in `datadog.yaml` reaches both Agent core and ADP. Reuses `test/smp/regression/adp/` in place — OTLP and quality_gates cases removed; the 15 `dsd_uds_*` cases rewritten to drive the Agent entrypoint and a converged Dockerfile.

Draft because the meaningful review signal is the SMP report comment, which CI posts after the benchmark runs.

## Test plan

- [ ] CI green: `build-adp-baseline-image`, `build-adp-comparison-image`, `build-agent-adp-baseline-image`, `build-agent-adp-comparison-image`, `run-benchmarks-adp`, `binary-size-analysis`.
- [ ] SMP report posted as a PR comment under header "Regression Detector (Agent vs Agent+ADP, 1Hz dogstatsd)".
- [ ] Spot-check the report: comparison side shows ADP-side metrics (`sub_agent: adp` series), 1Hz bucketing visible (flush rate / output volume).

## References

Stacked on #1459 (`luke/configurable-aggregation`). Modeled after #1327 but slimmer — no tag filtering, reuses existing dsd_uds cases.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Capture and report the PR URL**

The `gh pr create` output ends with the URL — surface it in the final report so the user can find the PR.

---

## Self-review

Skimmed the spec section by section against the plan:

- Goals (1Hz both sides; single SMP submission; draft PR; no source/test changes) → Tasks 3–9. ✓
- Deletions (OTLP, quality_gates) → Task 1. ✓
- Per-case rewrites (drop `agent-data-plane/`, add `datadog-agent/datadog.yaml`, rewrite `experiment.yaml` + `lading.yaml`) → Tasks 2–5. ✓
- `Dockerfile.datadog-agent` ARG/ENV plumbing → Task 6. ✓
- Two new build jobs + repointed `run-benchmarks-adp` → Tasks 7–8. ✓
- Branch base `luke/configurable-aggregation`, draft PR → Pre-flight + Task 9. ✓
- Out-of-scope: standalone-ADP benchmark, correctness/integration smoke tests, gRPC max msg size, run.rs log, ADP-internal knob tuning — none of these appear as tasks. ✓

Placeholder scan: each step shows the actual command, file content, or expected output. No "TBD" / "similar to Task N" / "add appropriate error handling".

Type/name consistency:
- `BASELINE_AGENT_IMG` / `COMPARISON_AGENT_IMG` / `BASELINE_AGENT_SHA` / `COMPARISON_AGENT_SHA` used identically in Tasks 7 and 8. ✓
- `AGENT_IMG=docker.io/datadog/agent-dev:15f1e04a-py3-jmx` matches in both build-agent-adp-* jobs and the spec. ✓
- `dogstatsd_socket: '/tmp/dsd.socket'` (Task 3) matches `path: "/tmp/dsd.socket"` (Task 5). ✓
- ADP env-var set: `DD_DATA_PLANE_ENABLED`, `DD_DATA_PLANE_STANDALONE_MODE`, `DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT`, `DD_DATA_PLANE_REMOTE_AGENT_ENABLED`, `DD_DATA_PLANE_DOGSTATSD_ENABLED`, `ADP_DD_DATA_PLANE_TELEMETRY_ENABLED`, `ADP_DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR` — same seven names in Task 6 (Dockerfile ARG/ENV) and Task 7 (build args). ✓
