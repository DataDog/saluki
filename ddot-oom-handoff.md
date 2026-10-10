# DDOT OOM Investigation — Handoff Document

**Status:** Root cause NOT identified. Extensive elimination campaign complete; the failure is localized to the otel-agent binary (7.84.1) and survives every configuration change. This document separates verified facts from hypotheses. The next agent should treat the "Eliminated" section as settled and focus on the "Remaining suspects" and "Suggested next steps."

---

## 1. Problem statement

In the SMP (Single Machine Performance) benchmark, the DDOT legs — running the `otel-agent` binary from the converged Datadog Agent image 7.84.1 (embedded collector v0.159.0) — crash-loop under 5 MiB/s of OTLP metrics ingest. Each target life: ~13 seconds of quiet startup (~13–77 MB live heap), then ~2 GB/s of live-set growth from the moment traffic arrives, then death at ~16 seconds total. 188 target lives measured, all identical.

The Core Agent leg (~1.3 GB steady) and the ADP leg (~236 MB) pass the same load on the same infrastructure. Raymond's baseline case (`ddot_metrics` in the SMP agent suite) passes at ~556 MB with *heavier* load (6 MiB/s, 5 connections, richer contexts) — but on agent **7.71.0** (collector v0.133.0), an image 13 minor versions older than ours.

**The unexplained core fact:** the OTLP export path *succeeds* (200 OKs flowing to the blackhole), yet the process retains ~2 GB/s of live, GC-reachable memory. If the drain worked, the queue wouldn't fill; if the queue filled, it should reject (the Agent leg does exactly this — see §3.9); the DDOT leg neither drains boundedly nor rejects — it just retains until killed.

---

## 2. Verified facts (with evidence sources)

### 2.1 The balloon (measured, not inferred)

Instrument: `GODEBUG=gctrace=1` in the target env; one line per GC cycle in the target logs. Job `a918f884-fdc2-43aa-914e-f434ef7a63e3`, 188 complete target lives analyzed (5,000 gctrace lines; full extract at `~/Downloads/extract-2026-10-10T16_22_30.155Z.csv`).

- Quiet phase: ~13s (range 10.6–15.3), heap ~13–77 MB live
- Balloon: starts exactly when traffic arrives; live-set grows at **median 2,165 MB/s** (range 981–5,142)
- Death: median 7,479 MB live (max 10,790 MB), at ~16s total life
- GC healthy throughout: **4–11% CPU**, marks complete normally, no assist death-spiral. The live set (post-mark) grows monotonically — this is **retention of reachable memory, not garbage** and not GC misbehavior
- GC#-per-life ~24–31; heap goal tracks live × GOGC until GOMEMLIMIT (7 GiB) is overwhelmed

### 2.2 The death is a silent external kill

- Last log line of every dying target is a **mid-stream gctrace line** — no fatal, no panic, no shutdown, no exit message. Go programs exiting on their own print something; only SIGKILL ends one mid-line.
- No `fatal error: out of memory` anywhere (checked; Go never died in the allocator — it was killed).
- Kernel OOM-killer reports do **not** ship to Datadog logs (verified across indexes). The literal kernel line lives in `dmesg` on the RJO hosts — not yet pulled.
- lading reports: `target exited unexpectedly; exit code unavailable` / `Error: Target(TargetExited(None))`.
- **The kill threshold is ~10–11 GB, not the configured 8 GiB memory_allotment.** Evidence: gctrace live peaks up to 10.8 GB, and converged-era kernel OOM reports said ~11 GB. The enforced limit is pod/cgroup-level (shared with lading + capture), not target-only. A process with a hard 8 GiB limit could not reach a 10.6 GB live heap.

### 2.3 The export path works; the failures are on the internal path

Per-experiment forwarder transaction errors, job a918f884 (Datadog query: `index:single-machine-performance-target-logs job_id:a918f884... "Error while processing transaction"`, grouped by message):

- **DDOT leg: 453 errors, ALL to `https://api.datadoghq.com/api/v2/series` and `/api/v1/metadata`** — the real Datadog intake, DNS-dead in the sandbox ("network is unreachable"), each retrying on a 15-minute budget. **Zero errors to `127.0.0.1:9091`.**
- The DDOT leg's OTLP exports to the blackhole succeed: 200 OKs to `/api/beta/sketches` (~788K transactions) and `/api/intake/metrics/v3/series` (~100K).
- Agent leg: 236 errors to `127.0.0.1:9091` (connection refused — the blackhole's ~4-second refusal window at measurement start) + the known process.datadoghq.com noise. ADP leg: 76 to 9091.
- **No evidence connects the api.datadoghq.com retry loop to the 2 GB/s retention.** The retry queue is bounded (~15 MiB serialized payloads); DNS failures are instant, not blocking. This is noise from running in a DNS-dead sandbox, not (demonstrably) the balloon's cause.

### 2.4 Two silent env gates (both found the hard way, both now fixed)

1. **`DD_OTELCOLLECTOR_ENABLED=true` is required even when the binary is invoked directly.** Without it the process prints "OpenTelemetry Collector is not enabled, exiting application" and exits before ingesting anything. **Every "standalone" run before job a918f884 never ran** (696 instances of the exit message in job `70617ae2` alone). This includes a GOMEMLIMIT test that was believed to have "failed" — it never started; GOMEMLIMIT is therefore **untested**, not disproven.
2. **`DD_OTEL_STANDALONE=true` selects the binary's true-standalone mode.** Without it, `otel-agent run` runs in **connected mode** — expecting a core agent at `:5001` — producing the `:5001` stream failures (116×), "Could not resolve hostname from core-agent: retries exhausted", and internal submissions posting directly to api.datadoghq.com. Both modes were running in our tests; the config was never actually what we believed it was.
   - Source: `cmd/otel-agent/subcommands/run/command.go` — `standaloneAgentFxOptions` vs `connectedAgentFxOptions`, gated on `otel_standalone` (env `DD_OTEL_STANDALONE`).

### 2.5 The sibling legs jam too — but survive by rejecting

Job a918f884, Agent leg (`otlp_ingest_metrics_agent_5mb`):

- `"sending queue is full"` (gRPC error returned to the generator): **1,602×**
- `"Exporting failed. Rejecting data."`: **798×**
- `"Too many errors for endpoint 'http://127.0.0.1:9091/...'"` (forwarder endpoint circuit-breaker on the blackhole): 160×

So the Agent leg's export queue ALSO fills — it **rejects data** at queue-full, stays memory-bounded, the breaker recovers, the replicate completes, and it **passes** (with data loss). The DDOT leg never logs the rejection messages — it grows to the kill boundary instead. Whether the DDOT's queue never fills (something else grows), or fills at a memory level that crosses the cgroup before rejection engages, is **unresolved** (see §4.2).

Caveat for the benchmark: the Agent leg's "pass" is partly load-shedding — its measured numbers were recorded while rejecting generator traffic.

### 2.6 The converged shape fails too

The original OOM reports (pre-standalone-switch, converged runs with a real core agent at `:5001`, working internal paths, internal telemetry flowing to the blackhole) died at ~11 GB. So the balloon is **shape-independent** (converged and standalone-command both fail) and **mode-independent** (connected-with-core-agent also failed). The mode confusion in §2.4 explains noise, not the balloon.

### 2.7 The version gap

- Raymond's baseline: image tag `agent:4973-7.71.0-full`, collector **v0.133.0** (from log tags on his passing runs, e.g. job `6609f46f`). SMP pins his suite to this image — **his case has never been run on the current agent.**
- Ours: 7.84.1 base (saluki-built `agent-adp-` image layered on `registry.datadoghq.com/agent:7.84.1-full`), collector **v0.159.0**.
- The gap contains, among 26 collector versions: **#51333** (`b0d9b5a1945`, 2026-07-08, OTAGENT-1024) — wrapped the serializer exporter in OTel exporterhelper, overriding OTel defaults (10 consumers / 1000 slots) with legacy-forwarder parity (1 consumer / 300 slots); the legacy worker threaded serialized bytes while the new consumer threads translation of decoded pdata — right numbers, wrong unit. Also **#56878** (`529961229eb`, 2026-09-25, OTAGENT-1362): standalone-mode billing-metric emission (gated on `DD_OTEL_STANDALONE=true`; not the operative emitter in our false-mode runs).
- The datadog-agent go.mod pins mapping-go at a **July 14 devel commit** (`fee4bbf7ff73`) — translator changes also land in the gap.

### 2.8 SMP is exonerated

Sibling symmetry: Agent and ADP pass on identical SMP machinery (harness, lading, blackhole, fleet). The memory usage is measured inside the process (gctrace is the Go runtime's own numbers). The one SMP artifact — the ~4-second blackhole refusal window at measurement start — hits all legs equally (siblings show refused transactions from it) and they recover. SMP's kill at the pod ceiling is enforcement working as designed on a process that genuinely consumed the memory.

---

## 3. Eliminated suspects (do not re-walk these)

| Suspect | How eliminated | Evidence |
|---|---|---|
| **Payload types** (histograms/summaries, lading #1908 defaults) | data | 95%-gauge/sum control leg balloons identically to the full mix (job a918f884) |
| **Consumer count** (#51333's 10→1) | data | `sending_queue: num_consumers: 10` balloons identically to the default 1 |
| **GC misbehavior / overshoot** | data | gctrace: 4–11% CPU, clean marks, monotonic live growth |
| **Blackhole / intake** | data + infra | 200 OKs flowing; zero 9091 errors on the DDOT leg; siblings pass |
| **URL mangling** | data | fixed long ago; exports succeed; replication leg re-proved the bug on 7.84.1 exactly as predicted (the `datadog/dd-autoconfigured` extension built `https://api.127.0.0.1:9091` from `api.site`) |
| **Transport** | data | gRPC and HTTP both failed (historical runs) |
| **Agent shape** (converged vs standalone command) | data | both fail; converged died at ~11 GB |
| **Mode confusion** (connected vs true-standalone) | data | converged runs with a real core agent also OOMed |
| **Blackhole netns placement (PR-pool fault)** | infra | non-split layout shares the netns; user verified; payloads received |
| **Throughput mismatch / queue fills at ingest rate** | timing math | death in ~16s at 5 MiB/s requires ~100–400x amplification — no legitimate mechanism; also balloon is payload-independent, which a translation-cost theory requires to be payload-dependent |
| **Instant allocation from u32::MAX bucket counts** | lading source | generated histogram bucket counts are 1–10 (delta) / slow-growing (cumulative); u32::MAX is only a validation bound |
| **Summary count (≤1M) as a loop bound** | translator source | `MapSummaryMetrics` treats count as a value; no count-sized loops anywhere in the mapper (read end-to-end: summaries, explicit hists, exp hists — all per-point, bounded, errors dropped at debug level) |
| **"Standalone passes locally"** | log forensics | the local repro never had load applied (2 total exports, 441 MB peak) — it proved boot, not the pipeline |
| **The GOMEMLIMIT test "failing"** | log forensics | that run never started (env gate #1); GOMEMLIMIT remains untested |

Also eliminated-by-symmetry (shared by passing sibling legs): env vars, profiling environment, load shape, connections, rate, contexts, harness.

---

## 4. Remaining suspects (ranked, honest)

### 4.1 Something in the 7.71.0 → 7.84.1 version gap — the widest-open suspect

Everything local to our configuration is eliminated. What remains lives in the binary between those versions: the #51333 exporterhelper wrapping, 26 collector versions of receiver/pdata/batch changes, the July-14 mapping-go pin, the OTAGENT-1362 standalone-emission family. The one run never done: **the same load on 7.71.0** (see §6.1).

### 4.2 The exporterhelper sending_queue holding decoded batches — leading STRUCTURE hypothesis, weakened but alive

- Supporting: the death zone (median 7.5 GB, max ~10.5) matches the queue's capacity arithmetic (300 slots × ~25–35 MB decoded batches); the Agent leg proves this queue exists and fills in practice.
- Against: the DDOT legs never show the queue-full rejection messages ("Exporting failed. Rejecting data." / "sending queue is full") that the Agent leg shows when ITS queue fills. If the DDOT's queue filled, it should reject the same way.
- Unresolvable from logs/gctrace — needs direct queue-depth measurement (§6.2) or heap composition (§6.3).

### 4.3 What holds the memory — unknown

No instrument has captured the heap composition. The native profiler never ships from the dying targets (they die every ~16s, faster than the upload cycle — verified: DDOT hosts shipped zero `service:agent-data-plane` profiles while surviving targets ship ~40 per 8h). Candidates: exporter queue, receiver-side buffering, batch-processor accumulation on failed flush, forwarder transaction channels, per-series state.

---

## 4b. Test coverage matrix — what has data and what is EMPTY

| Cell | 7.71.0 | 7.84.1 |
|---|---|---|
| full mix, converged, default config | — | **OOM** (historical, real data) |
| full mix, standalone, 10 consumers | **EMPTY** | **ballooned** (a918f884 leg A, measured; note: ran in connected mode — pre-gate-fix) |
| ~95% gauge/sum, standalone, 1 consumer | **EMPTY** | **ballooned identically** (a918f884 leg B, measured; also connected mode) |
| true-standalone mode (DD_OTEL_STANDALONE=true) | **EMPTY** | **EMPTY** — staged, never run |
| slowburn (32 KiB/s) profile capture | **EMPTY** | **EMPTY** — staged, never run |
| Raymond's exact case (converged, his config) | **EMPTY** — his own suite passes it; never run through OURS | **OOM via URL mangling** (replication leg — as predicted, not a pipeline signal) |
| GOMEMLIMIT actually running | **EMPTY** | **EMPTY** — the original test was vacuous (env gate); now staged in every leg |

**The single biggest empty cell: the 7.71.0 + our-load job.** It requires a separate CI job (Run pipeline with `PUBLIC_DD_AGENT_VERSION=7.71.0-full`) because SMP fixes one image per job. Reading: old+full passes + old+Raymond passes → the version gap carries the failure; old+full fails → the payload kills even the old agent; old+Raymond fails → our harness is confounded. Legs A/B on the 7.71 job would also have been vacuous before the env-gate fixes, but the replication leg (which carries its own env from Raymond's config) would have run from day one — that job should have been triggered in parallel from the first push.

---

## 5. The staged experiment suite (uncommitted, 75 configs verified)

All in `test/smp/regression/adp/experiments.yaml` (branch `lt/otlp-benchmark`, saluki/benchmarking repo). All DDOT legs carry: `DD_OTELCOLLECTOR_ENABLED=true`, `DD_OTEL_STANDALONE=true`, `GOMEMLIMIT=7GiB`, `GODEBUG=gctrace=1`.

| Experiment (`experiment:` tag) | What it is |
|---|---|
| `otlp_ingest_metrics_ddot_5mb` | Full payload mix + `sending_queue: num_consumers: 10` |
| `otlp_ingest_metrics_ddot_5mb_gaugesums_control` | ~95% gauges/sums (trace weights on new types, ~4.8%), single-variable vs the failures (contexts 100) |
| `otlp_ingest_metrics_ddot_slowburn` | Same as full-mix leg but 32 KiB/s — designed so the target outlives the profiler's upload cycle, so the live-heap profile of the ballooning process ships to Datadog (Profiling explorer: `service:agent-data-plane`, `env:single-machine-performance`) |
| `otlp_ingest_metrics_ddot_raymond_replication` | Raymond's exact case (converged, his otel-config/datadog.yaml/env, HTTP/4318 load at 6 MiB/s / 5 conns, his context nesting, 2 GiB, 3 blackholes). On 7.84.1 it failed exactly as predicted (URL mangling); on a 7.71.0 job it is the harness anchor |
| `otlp_ingest_metrics_agent_5mb` / `otlp_ingest_metrics_adp_5mb` | Sibling controls (~1.3 GB / ~236 MB expected) |

Note: exact gauge/sum-only payloads are **inexpressible** in current lading — #1908's validation rejects zero metric weights ("Metric weights cannot be 0"). Trace weights are the closest legal form.

---

## 6. Suggested next steps (ranked by decisiveness per unit of effort)

### 6.1 Run the suite on agent 7.71.0 — the version-axis test

GitLab → Run pipeline → add variable `PUBLIC_DD_AGENT_VERSION=7.71.0-full` → trigger the benchmark job. SMP fixes one image per job, so this is a separate job. If 7.71.0 drains the same load without ballooning, the regression is in the version gap; if it balloons too, the failure predates Raymond's baseline era and the environment interaction is deeper than version. The replication leg on this job doubles as the harness anchor (his exact case, his agent version).

### 6.2 Enable the collector's internal telemetry scrape — measure the queue directly

Add `service.telemetry.metrics` to the DDOT otel-config (expose a prometheus endpoint, e.g. :8888) and point the leg's lading `target_metrics` scrape at it (currently scrapes a dead 5100). SMP then records `otelcol_exporter_send_queue_size` and accepted-vs-sent counters over time — directly showing whether the queue climbs to 300 while the heap balloons (naming the structure) or stays low while something else grows (killing the queue hypothesis). Verify the exact config-key shape against collector v0.159 before staging.

### 6.3 Get a heap profile

Either the slowburn leg (staged — survives long enough to be profiled) or locally: run the standalone binary in docker with any OTLP load ≥5 MiB/s and grab a pprof heap dump at ~10s (before the ~16s death). The binary + load + memory limit reproduces the crash anywhere in ~16 seconds — no SMP needed.

### 6.4 Host kernel logs

`dmesg | grep -i "killed process"` on any RJO host that ran the failing legs — the literal OOM-killer line (expected: kill at ~10–11 GB, matching gctrace peaks). Confirms the killer and the threshold; does not identify the structure.

### 6.5 Source-level

The serializerexporter + exporterhelper queue path in 7.84.1 vs 7.71: what holds ~25–35 MB decoded batches, and what changed. Key files: `comp/otelcol/otlp/components/exporter/serializerexporter/{exporter,consumer,config}.go`, `pkg/opentelemetry-mapping-go/otlp/metrics/default_mapper.go` (read end-to-end this session — all bounded), `cmd/otel-agent/subcommands/run/command.go` (the two modes).

---

## 7. Key artifacts

- **Jobs (Datadog, `index:single-machine-performance-target-logs`):**
  - `a918f884` — the measured run: gctrace balloon data, error breakdown, sibling jam signatures
  - `98b11e7c` — env-gate #1 discovery (instant exits)
  - `70617ae2`, `0cba3c15`, `474e9c33`, `e36673e4`, `d940d31b`, `0ea111e6`, `6e5854bb` — historical (various configs; all DDOT variants failed; 6e5854bb is the original mangling repro)
- **CSV extract:** `~/Downloads/extract-2026-10-10T16_22_30.155Z.csv` — 5,000 gctrace lines, 188 lives, per-life trajectories (quiet phase, balloon rate, peak live, goal)
- **Checkouts:** `~/datadog-agent` (agent source), `~/lading` (generator source), `~/smp-runner` (Raymond's cases: `experiments/regression/agent/cases/ddot_metrics/`)
- **Commits:** `b0d9b5a1945` (#51333, consumer/queue parity), `529961229eb` (#56878, standalone billing emission), `8999bd79` (lading #1908, new payload defaults + zero-weight validation)
- **Metric:** `single_machine_performance.regression_detector.capture.total_pss_bytes` (SMP capture; note: job a918f884's capture data had not shipped to Datadog at time of writing)
- **Profiling:** profiles only ship from surviving targets; Profiling explorer query: `service:agent-data-plane env:single-machine-performance`

---

## 8. Gotchas (learned the hard way — check these before trusting any result)

1. **`global:` section in experiments.yaml injects ADP-suite env vars into every leg** (`DD_DATA_PLANE_STANDALONE_MODE`, `DD_USE_V3_API_SERIES_ENDPOINTS`, `DD_METRICS_LEVEL=trace`, ...) — strip with `KEY: null` in the leg's environment.
2. **Generator config merges BY TYPE** — an `http` generator APPENDS to the base's `grpc` generator (double load). Replication-style legs must extend `otlp_base` (no generator) and define their own.
3. **`cumulativetodelta`** (no underscores) is the only processor name valid on BOTH v0.133 and v0.159.
4. **SMP fixes one target image per job** — version comparisons need the CI-variable job (`PUBLIC_DD_AGENT_VERSION`).
5. **Setting `DD_SITE` mangles URLs** (`https://api.<site>`) — the original bug; it is not a fix for internal endpoints.
6. **The standalone binary's env gates are silent** — `DD_OTELCOLLECTOR_ENABLED` and `DD_OTEL_STANDALONE` both default to off/dead paths; verify boot before interpreting any result (the config-comment claims in the repo about these were wrong and cost two fleet runs).
7. **Profiles only ship from surviving targets** — die-too-fast targets never upload.
8. **"Exit code unavailable" from lading is the silent-kill signature** — no s6 in standalone to log signal 9; kernel reports don't ship to Datadog.
9. The kill threshold is the **~10–11 GB pod-level ceiling**, not the 8 GiB memory_allotment.
10. **No profiling API exists in the Datadog SDK** — profiles are viewable in the UI only.
