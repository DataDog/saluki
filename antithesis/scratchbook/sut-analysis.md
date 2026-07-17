---
sut_path: /home/ssm-user/src/saluki
commit: 93b9f37baf07004a9235f308305d78ef3d7fc226
updated: 2026-07-15
external_references:
  - path: https://datadoghq.atlassian.net/browse/SMPTNG-737
    why: Antithesis triage umbrella; authoritative ADP/Antithesis ticket set (738-767)
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/pages/6497671050
    why: "What Comes After DogStatsD?" states the load-independence liveness doctrine
  - path: https://datadoghq.atlassian.net/wiki/spaces/~602449d8f3d296006864db68/pages/6980436239
    why: "Agent Time Should be Monotonic, Is Not" — backward-clock aggregation write-up (SMPTNG-767)
  - path: git log (full history)
    why: mined fix commits for boot/stall/aggregation defect triggers
---

# SUT analysis — cheap-to-demonstrate liveness and aggregation

Scope of this pass: not the whole SUT. It targets two defect families the user named — aggregation bugs and "load comes in but no load goes out" flush-stall liveness — and asks which scenarios demonstrate them most cheaply. "Cheap to demonstrate" means few containers, a short window, and a deterministic trigger, not merely a small compute bill.

## The SUT

`agent-data-plane` (ADP) is the primary artifact. In the Antithesis harness it runs converged: the Datadog Agent image with the data plane ON (`DD_DATA_PLANE_ENABLED=true`), so the embedded ADP owns DogStatsD. Load enters over a Unix datagram/stream socket, is decoded, enriched, aggregated, serialized as v2 series and sketches, then POSTed to an HTTP intake.

## Data path: ingest → intake flush

Metrics lane wiring: `bin/agent-data-plane/src/cli/run.rs:763` —
`dsd_in.metrics → dsd_enrich → dsd_prefix_filter → dsd_tag_filterlist → dsd_agg → dsd_post_agg_filter → metrics_enrich → dd_metrics_encode → dd_out`

1. UDS bind — `sources/dogstatsd/mod.rs:895`, bind at `:956`
2. Accept/recv loop — `mod.rs:1298` accept, `:1402` `stream.receive`
3. Decode/frame into a `FixedSizeEventBuffer` (cap 1024, `saluki-core/src/topology/mod.rs:33`)
4. Source dispatch, **awaited inline** — `mod.rs:1489` (buffer full) and `:1555` (100 ms tick) `dispatch_events(...).await`
5. Enrich/filter transforms — `run.rs:763`
6. Aggregation — `transforms/aggregate/mod.rs:381` `state.insert`, per-type merge `saluki-core/.../metric/value/mod.rs:433`
7. Flush on window close, **awaited** — `aggregate/mod.rs:324` `state.flush(...).await`, 15 s interval / 10 s window
8. Post-agg filter → host enrich
9. v2 encode — `encoders/datadog/metrics/mod.rs:616`; builder over `mpsc::channel(8)`
10. Forwarder — `forwarders/datadog/mod.rs:165` `send_transaction`
11. Transaction fan-out — `common/datadog/io.rs:365`, per-endpoint `channel(8)`
12. HTTP send + retry circuit breaker — `io.rs:499`; open circuit re-enqueues to the retry queue `io.rs:638`
13. Wire — intake `POST /api/v2/series` at `test/antithesis/intake/src/http/datadog/metrics.rs:60`; contexts recorded `capture.rs:147`

Interconnects are bounded tokio mpsc, capacity 128 (`saluki-core/src/topology/mod.rs:34`).

## Pivotal counter-fact: the forwarder sheds, it does not wedge

The obvious "load in, no load out" scenario — point ADP at a dead or slow intake and watch it back up — **does not reproduce a stall.** The endpoint loop's retry-queue push DROPS when full (`io.rs:588`, `push_low_priority` `io.rs:640`) and its input-recv arm is always enabled, so it keeps draining even when intake is down or the concurrency limit is saturated. Series are shed to the retry queue, not blocked. A backpressure-only liveness scenario is a dud. Any scenario design that relies on slow-intake backpressure to force a wedge is wrong.

The stalls that ARE real, in order of how cheaply they can be demonstrated:

1. **Boot gate — Core-Agent gRPC partition.** Steps 1-13 never start until boot completes. `get_hostname` retries only a bounded ~10 attempts (`lib/datadog-agent/commons/src/ipc/client/mod.rs`); registration and the post-connect health check were each historically added outside the retry scheme and made fatal. A partition outlasting the budget → ADP `exit(1)`, no load ever goes out. This is the freshest, most recurring family (current branch `blt/fix_hostname_fetch_retry`, SMPTNG-766). Deterministic trigger, boot-length window.
2. **First-connection TLS/entropy.** The first `dd_out` POST is ADP's first outbound TLS handshake. AWS-LC CPU-jitter entropy once SIGABRTed under Antithesis determinism (SMPTNG-762, platform-patched). Regression guard now.
3. **Killed, non-restarted component.** Root supervisor is `OneForOne` with **0 restarts** (`run.rs:212`). If aggregate/encode/forward dies, its bounded input channel fills and the awaited upstream send (steps 4, 7) blocks forever — ingest wedges while the UDS stays bound. Real wedge, but needs a fault that kills exactly one component task.
4. **Sustained backpressure.** Least reliable — see the counter-fact. Do not rely on it.

## Aggregation failure-prone areas

Single deterministic packets or one config value provoke these — no fault injection needed:

- Counter NaN poisoning — `merge_scalar_sum` (`value/mod.rs:493`) has no NaN guard; one `|g NaN` sample poisons the window sum. Surfaces intake-side as Pyld20.
- `NaN`/`±Inf` forwarded as JSON `null` — `e955c5a44e`, deser accepts `"NaN".parse::<f64>()`.
- Sub-second aggregate window → divide-by-zero panic — `738b6874a9`.
- Sketch bin explosion at minimum sample rate → downstream builder panic — `bc3cc747e5`; the `sketchburst` driver already sweeps past `bin_limit`.
- `host:` tag treated as a normal tag → wrong context identity — `380ebef07d`.
- Timestamped `|T` count reinflated by `@rate` → count divergence — `f0a3687dcc`.
- v2 encoder ignored `max_series_points_per_payload` — `0025407262` / SMPTNG-765, still open.
- Non-monotonic flush wall-clock under clock jitter → out-of-order/duplicate timestamps — SMPTNG-767, in progress. clock_jitter is a global, near-free fault that triggers this.
- Type/rate change mid-window silently discarded — `value/mod.rs:436`, `:450`.

## Concurrency model

Actors over locks: components are tasks joined by bounded mpsc channels. Source dispatch and aggregate flush are `.await`ed inline in single-task `select!` loops, so downstream backpressure parks the upstream loop — the mechanism behind the killed-component wedge (stall #3). The intake capture state is an `Arc<Mutex<Lanes>>` dedup map.

## Cost model for Antithesis runs

Per `test/antithesis/bin/launch.sh`, cost per run scales with: container count (state space per timeline), fault surface (node termination/hang/throttle on `FAULT_NODES`, plus global cpu_mod + clock_jitter, plus network faults), and duration (default 30 m). The two existing scenarios:

- `general` — 3 containers, node faults on `agent` + both globals. The fault-heavy one.
- `differential` — 4 containers, no node faults. The expensive one by container count.

Cheapest demonstrative profile: 3 containers, **no node faults**, keep only cpu_mod + clock_jitter (near-free globals, and clock_jitter is a real aggregation trigger), short duration. Drops the entire node-fault branching factor.

## Assumptions

- ADP dials the Core Agent at a hardcoded `https://127.0.0.1:{cmd_port}` (`lib/datadog-agent/commons/src/ipc/config.rs:209`), vsock the only alternative. There is no way to point ADP at a Core Agent in another container over TCP, so the ADP↔Core-Agent boot gRPC link cannot be network-partitioned by splitting containers. Boot-gate stall #1 is reachable only via timing faults on the converged container, already present in `general`.
- `standalone` ADP mode is vestigial (AGENTS.md) and skips the Core-Agent boot gRPC, so it cannot exercise stall #1.
- The one cross-container link a network fault can reach is ADP → intake. Partitioning it exercises recovery liveness (ADP survives, load resumes on heal), not a wedge.

## Open questions

- Can stall #3 (killed component) be triggered deterministically enough to be a cheap scenario, or only under node-hang faults?
- Does an intake-side per-run marker liveness probe belong in a no-fault scenario as a green-by-default regression anchor, given no current-code bug reproduces via it?
