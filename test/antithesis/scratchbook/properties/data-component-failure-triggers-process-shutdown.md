---
slug: data-component-failure-triggers-process-shutdown
title: Any data component finishing triggers whole-process shutdown (fail-stop)
family: Lifecycle Transitions & Configuration
type: Safety (Always) + Reachability
priority: High
status: assertion-missing
sut_commit: 042f41db3bd97118c38981765fd49696fce9d318
---

# data-component-failure-triggers-process-shutdown

## Origin

SUT analysis §2 Supervision ("**the primary data topology is NOT** [supervised] — `RunningTopology`
spawns each data component into a `JoinSet` with **no restart**. Any data component finishing →
`wait_for_unexpected_finish` → **whole-process shutdown**") and §7 (fail-stop recovery model: s6
restarts ADP on exit). The invariant: ADP must never run a **silently half-running** pipeline.

## Files / functions / lines

- `lib/saluki-core/src/topology/built.rs:158-410` — `BuiltTopology::spawn`: each component
  (sources, transforms, encoders, forwarders, destinations, relays, decoders) is spawned via
  `spawn_component` (built.rs:666-687) into a single `JoinSet<Result<(), GenericError>>`. There is
  **no per-component restart wrapper** — unlike `runtime/supervisor.rs`, the topology does not
  re-spawn a finished component.
- `lib/saluki-core/src/topology/running.rs:40-51` — `wait_for_unexpected_finish`:
  ```rust
  let task_result = self.component_tasks.join_next_with_id().await
      .expect("no components to wait for");
  handle_task_result(&mut self.component_task_map, task_result, /*unexpected=*/true);
  ```
  Returns as soon as **any one** component task finishes (Ok, Err, or panic). It does NOT loop /
  restart — the single completion is surfaced to the caller.
- `bin/agent-data-plane/src/cli/run.rs:280-283` — in the main `select!`:
  ```rust
  _ = running_topology.wait_for_unexpected_finish() => {
      error!("Topology component unexpectedly finished. Shutting down...");
      topology_failed = true;
  },
  ```
  Any component finishing wins this select arm → falls through to
  `running_topology.shutdown_with_timeout(30s)` (run.rs:290) → shuts down the **whole** topology →
  process proceeds to exit. With `topology_failed = true`, the final result is `Ok` (clean shutdown)
  but logged as "Topology shutdown complete despite error(s)." (run.rs:302-303). Process then exits;
  the container's s6 supervisor restarts ADP (full-process restart = recovery model).
- `lib/saluki-core/src/topology/running.rs:130-162` — `handle_task_result` with `unexpected=true`:
  a clean `Ok(())` finish is logged as `warn!("Component unexpectedly finished.")` (running.rs:140);
  an `Err` as `error!("Component stopped with error.")`; a `JoinError` (panic/cancel) as
  `error!("Component task failed unexpectedly.")`.
- `lib/saluki-core/src/runtime/supervisor.rs` — the supervisor with `OneForOne`/`OneForAll` restart
  (supervisor.rs:477-481) applies to the **internal supervisor only** (control plane / internal
  telemetry / env), assembled at run.rs:185-202. It does **not** wrap data components. This is the
  "crucial split" from SUT §2.

## Key observation / honest framing

The fail-stop guarantee is real and structural: there is exactly one `JoinSet` for data components
and exactly one `wait_for_unexpected_finish` arm that converts any single completion into
whole-topology shutdown. The defensible invariant:

- **Always:** whenever a data component task terminates before an operator-initiated shutdown
  (SIGINT), the process initiates topology-wide shutdown — it never continues running the remaining
  components as a partial pipeline. Equivalently: there is no state where component count has
  decreased due to an unexpected finish *and* the topology keeps processing.
- **Reachable:** the `wait_for_unexpected_finish` → shutdown path is actually hit when a data
  component is induced to finish (proves fail-stop fires, not just that it exists).

Caveat to state honestly: between the moment a component finishes and the moment shutdown completes,
the pipeline is transiently "half-running" (other components still alive, draining). That window is
*bounded by the 30s shutdown* (see `graceful-shutdown-within-30s`) and is by design. The invariant
is about **not silently staying** half-running, not about instantaneous teardown.

## Failure scenarios (Antithesis angle)

- **Induce a component panic** (e.g. trigger one of the hot-path `.expect`/`unreachable!` sites in
  SUT §7 #14; note the sub-second `aggregate_window_duration` panic of §7 #8 is now closed upstream)
  → component task ends with
  `JoinError` → `wait_for_unexpected_finish` fires → process shuts down. Assert the shutdown path is
  reached and the process exits (s6 then restarts). Falsify if the pipeline keeps running with a
  dead component.
- **Component returns Ok unexpectedly** (clean finish mid-run, e.g. a source whose loop exits on a
  closed socket) → same fail-stop path (running.rs:140 warn). Confirms even a "successful" early
  finish triggers shutdown.
- **Forwarder task exits on permanent error** → fail-stop.
- Distinguish from SIGINT: under SIGINT the `ctrl_c` arm (run.rs:284) wins, `topology_failed` stays
  false. The property is specifically about the **non-SIGINT** completion arm.

## Config dependencies

- Number/identity of data components depends on enabled pipelines (run.rs:414-457). The invariant
  holds regardless, but the workload should know which component it is killing.
- Internal-supervisor components are explicitly **excluded** — a control-plane component failing at
  runtime is handled by the supervisor (run.rs:263-271 logs and continues), NOT by this fail-stop.
  Do not assert fail-stop for internal-supervisor component failures.

## Assertion (MISSING — net-new instrumentation)

No Antithesis SDK assertions exist. Proposed SUT-side:
- In the `wait_for_unexpected_finish` select arm (run.rs:280-283), before/at the `error!` log:
  `assert_reachable!("data component unexpectedly finished → process shutting down")`.
- To express the Always invariant in-process is awkward (it is enforced by control flow). Best
  approach: a workload-side temporal assertion — *whenever* the
  `"Topology component unexpectedly finished. Shutting down..."` log appears (or any component-death
  telemetry), the process must subsequently exit (and not continue serving). Antithesis
  query-logs/temporal checks (event A always precedes process-exit B) fit this well.
- Optionally instrument `handle_task_result` (running.rs) to emit a distinct telemetry/log on
  unexpected component finish so the workload can detect the half-running window and assert it is
  always followed by shutdown.

## Open questions

- **Is `wait_for_unexpected_finish` always being polled?** It is one arm of the run.rs:255 `select!`.
  Once any other arm completes (SIGINT, internal supervisor finish) the `select!` exits and
  `wait_for_unexpected_finish` is no longer polled — but at that point shutdown is already happening,
  so the invariant still holds. Confirm there is no window after topology spawn (run.rs:239) but
  before the `select!` (run.rs:255) where a component could finish unobserved. The intervening code
  (run.rs:241-253) spawns a detached readiness task and sets two bools — quick and non-awaiting on
  the topology — so the gap is negligible, but worth noting. WHY IT MATTERS: a component dying in
  that gap would still be caught by `join_next_with_id` once the select polls (JoinSet buffers
  completions), so likely safe; confirm.
- **`expect("no components to wait for")` (running.rs:45):** if the topology has zero components,
  `join_next_with_id()` returns `None` and this panics. Could an empty topology be built? run.rs:401
  errors out if `!data_pipelines_enabled()`, and `create_topology` adds at least a forwarder +
  components for any enabled pipeline, so a spawned topology should be non-empty. WHY IT MATTERS: a
  panic here would itself trigger process exit (still fail-stop-ish) but via an ugly path. WHAT
  CHANGES: low priority; document as a defensive-panic site.

## SUT-side instrumentation needs

- Antithesis SDK dependency (none today).
- Reachable marker on the run.rs:280 unexpected-finish arm.
- A distinct log/telemetry event on unexpected component finish to anchor a workload-side temporal
  "death-implies-exit" assertion.
