---
slug: topology-ready-before-intake
title: Topology becomes ready before data intake begins
family: Lifecycle Transitions & Configuration
type: Liveness + Safety (ordering)
priority: High
status: assertion-missing
sut_commit: 042f41db3bd97118c38981765fd49696fce9d318
---

# topology-ready-before-intake

## Origin

SUT analysis §2 ("Build → spawn lifecycle … The topology starts accepting data only
after `health_registry.all_ready()`") and §5 Liveness #3 ("Topology starts accepting
data only after all components report ready"). Owned by the Lifecycle agent.

## Files / functions / lines

- `bin/agent-data-plane/src/cli/run.rs:218-251` — startup ordering:
  - `run.rs:219-225`: internal supervisor is spawned (`run_with_shutdown`).
  - `run.rs:227-235`: `select!` waits on `health_registry.all_ready()` for the *internal
    supervisor* to become healthy before proceeding. If the supervisor task completes first
    (`early_result`), returns an error (`generic_error!("Internal supervisor completed
    unexpectedly…")`).
  - `run.rs:238`: `let built_topology = blueprint.build().await?;` — topology is only **built**
    after the internal supervisor is ready.
  - `run.rs:239`: `let mut running_topology = built_topology.spawn(&health_registry, memory_limiter).await?;`
    — components (incl. the `dsd_in` source that opens listeners) are spawned here.
  - `run.rs:242-251`: a *second* `all_ready()` wait, spawned as a detached task, logs
    `topology_ready_ms` and emits startup metrics once the full topology reports ready.
- `lib/saluki-core/src/health/mod.rs:354-375` — `HealthRegistry::all_ready()` / `check_all_ready()`:
  resolves only when **every** registered component's `is_ready()` is true
  (`shared.ready` AND `health != Dead`). Empty registry resolves immediately.
- `lib/saluki-core/src/health/mod.rs:49-66` — `Health::mark_ready()` flips the per-component
  `ready` atomic and `notify_waiters()` so `all_ready()` re-checks.
- `lib/saluki-core/src/topology/built.rs:158-410` — `BuiltTopology::spawn` wires interconnects
  and spawns each component into a `JoinSet` (`spawn_component`, ~666-687). A source only begins
  reading sockets after its own task runs and marks itself ready.

## Key observation / honest framing

The ordering guarantee is **subtle and only partial** as literally stated by the SUT analysis.
What the code actually does:

1. The DSD source's listeners are bound and accept loop starts when the **source component task**
   runs (after `spawn()` at run.rs:239), independent of whether *other* components (e.g.
   `dsd_agg`, `dd_out`) have marked ready.
2. The data path is gated not by an explicit "do not read until all_ready" check in the source,
   but by **backpressure**: bounded `mpsc` edges + `memory_limiter.wait_for_capacity().await`
   (SUT §4). A source that reads before downstream is ready will block on `Dispatcher::send`.
3. The *observable* "topology ready" signal (`topology_ready_ms` log, startup metrics at
   run.rs:244-251) fires only after `all_ready()`.

So the precise, defensible property is **ordering of readiness milestones**, not "zero bytes
read before all_ready". A truthful assertion:

- **Always / ordering:** the `topology_ready_ms` milestone (full `all_ready`) is reached, and the
  internal-supervisor `all_ready` (run.rs:229) always precedes `blueprint.build()` (run.rs:238).
  i.e. the topology is never **built/spawned** before the internal supervisor reports ready.
- **Sometimes(all_ready reached):** at least once the full topology reaches `all_ready` (the
  detached task at run.rs:242 logs `topology_ready_ms`) — proves the readiness path is live.

Overclaiming "no data processed pre-ready" would be **wrong**: the source can read and then block
on backpressure; data may sit in-flight in channels before downstream `all_ready`. Frame the
assertion around the readiness-milestone ordering + reaching ready, not byte-level intake gating.

## Failure scenario (Antithesis angle)

- Delay/stall a downstream component's readiness (e.g. forwarder `dd_out` blocked on a slow/dead
  mock intake during init, or aggregate slow to initialize). Verify the process either reaches
  `all_ready` eventually (liveness) or stays observably "Waiting for topology to become healthy"
  without crashing.
- Fault: internal supervisor child fails to initialize → run.rs:232 `early_result` branch returns
  error before topology is built. Assert the topology was **never spawned** in that case
  (Unreachable: "topology spawned after internal-supervisor init failure").
- Timing: with Antithesis scheduling, confirm `sup_ready_ms` (run.rs:230) is logged before any
  `topology_ready_ms` (run.rs:245).

## Config dependencies

- `data_plane.enabled` / `dp_config.enabled()` (run.rs:152) — if not enabled, ADP exits before
  building topology (no readiness milestones at all). Assertions must not fire in that case.
- Pipelines enabled (`dp_config.*_pipeline_required`, run.rs:414-457) determine which components
  register with the health registry, i.e. what `all_ready` is gating on.
- Memory mode (`MemoryMode::default()==Disabled`, SUT §7) affects `memory_limiter`; doesn't change
  ordering but affects backpressure behavior.

## Assertion (MISSING — net-new instrumentation)

No Antithesis SDK assertions exist (existing-assertions.md). Proposed SUT-side instrumentation in
`run.rs`:
- After run.rs:230 (internal supervisor healthy) and before run.rs:238: record a monotonic
  milestone flag `internal_supervisor_ready`.
- Inside the detached task at run.rs:243 after `all_ready()`: `assert_reachable!` /
  `Sometimes("topology reached all_ready")` and `assert_always!`/`Always("internal supervisor
  ready before topology build", internal_supervisor_ready == true)`.
- For the negative path: at run.rs:232 (`early_result`) before returning the error, the code never
  reaches `spawn()`; an `assert_unreachable!` placed *after* `spawn()` keyed on
  "internal_supervisor_failed_to_initialize" would be hard to express in-process — better tested
  via workload-side log assertion (`port_listening` should be false / no `topology_ready_ms` log).

## Open questions

- Does the `dsd_in` source mark itself ready *before* or *after* binding its listeners? If listeners
  bind during `initialize()` (before `mark_ready`), a client could connect pre-ready. Need to read
  `lib/saluki-components/src/sources/dogstatsd/mod.rs` listener-bind vs mark_ready ordering to know
  whether "accepting connections before ready" is observable. WHY IT MATTERS: determines if the
  truthful property is "ordering of milestones" only, or can be strengthened to "no socket bound
  before ready". WHAT CHANGES: the assertion strength and whether a workload `port_listening` probe
  pre-ready is a valid falsification.
- Is there any scenario where a component marks ready, processes data, then a *later*-registered
  component drops the aggregate `all_ready` back to false? `register_component` can happen while
  `all_ready` waits (mod.rs:347-353 docstring). WHY IT MATTERS: readiness is not latched; the
  milestone could flap. Confirm all data components register before the run.rs:243 wait.

## SUT-side instrumentation needs

- Antithesis SDK dependency must be added (none today).
- A monotonic `internal_supervisor_ready` flag readable at the topology-ready milestone, or rely on
  log-ordering assertions (`sup_ready_ms` before `topology_ready_ms`).
