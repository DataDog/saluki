---
slug: graceful-shutdown-within-30s
title: Graceful shutdown completes within the 30s grace window without forceful kill
family: Lifecycle Transitions & Configuration
type: Liveness (bounded-time) + Reachability
priority: High
status: assertion-missing
sut_commit: 042f41db3bd97118c38981765fd49696fce9d318
---

# graceful-shutdown-within-30s

## Origin

SUT analysis §5 Safety #6 ("Graceful shutdown completes within 30s without forceful kill (in-flight
data drained)"). This slug owns the **TIMING / clean-completion** angle. The data-loss agent owns
`shutdown-drains-no-loss` (WHAT data survives — e.g. open-window buckets dropped unless
`flush_open_windows`, retry-queue disk flush). Keep this property about *completing cleanly in
time*, not about which data is preserved.

## Files / functions / lines

- `bin/agent-data-plane/src/cli/run.rs:255-287` — the main `select!` that ends the run loop on one
  of three triggers:
  - internal supervisor finishes (run.rs:256-279),
  - `running_topology.wait_for_unexpected_finish()` (run.rs:280-283) → `topology_failed = true`,
  - `tokio::signal::ctrl_c()` (run.rs:284-286) → SIGINT, logs "Received SIGINT, shutting down…".
- `bin/agent-data-plane/src/cli/run.rs:289-290`:
  ```rust
  let topology_result = running_topology.shutdown_with_timeout(Duration::from_secs(30)).await;
  ```
  The **30s grace window** is hard-coded here.
- `bin/agent-data-plane/src/cli/run.rs:292-294`: after the topology shutdown completes (clean or
  forced), the internal supervisor is told to shut down (`internal_supervisor_shutdown_tx.send(())`)
  and awaited.
- `bin/agent-data-plane/src/cli/run.rs:300-315`: maps `topology_result` to the final process result.
  `Ok(())` → clean (or "clean despite errors" if `topology_failed`); `Err(e)` → propagated → exit 1.
- `lib/saluki-core/src/topology/running.rs:71-124` — `shutdown_with_timeout`:
  - `:72-78` sets `shutdown_deadline = now + timeout`, arms a `sleep(timeout)`, and a 5s
    `progress_interval` for "still waiting on component(s)" logs.
  - `:82` `self.shutdown_coordinator.shutdown()` triggers source shutdown, cascading downstream.
  - `:86-117` loop: as each task finishes, `handle_task_result` records clean/unclean; when
    `join_next_with_id()` returns `None` (all tasks done) → `info!("All components stopped.")` →
    break with `stopped_cleanly` reflecting whether every component returned Ok.
  - `:111-115` **forceful stop path**: if `shutdown_timeout` fires first →
    `warn!("Forcefully stopping topology after shutdown grace period.")`, `stopped_cleanly = false`,
    break (remaining component tasks are dropped/aborted by the `JoinSet` being dropped).
  - `:119-123` returns `Ok(())` iff `stopped_cleanly`, else
    `Err(generic_error!("Topology failed to shutdown cleanly."))`.
- `lib/saluki-core/src/topology/running.rs:130-162` — `handle_task_result`: a component returning
  `Ok(())` during shutdown is "stopped" (clean); `Err`/`JoinError` (panic/cancel) → unclean.

## Key observation / honest framing

- "Within 30s" is enforced by the `sleep(30s)` race in `shutdown_with_timeout`. The clean path
  (`stopped_cleanly == true`, run.rs topology_result `Ok`) means **all** component tasks finished
  and returned Ok before the deadline. The forceful path is reached only if at least one component
  fails to stop within 30s.
- This is a **bounded-time liveness** property. Under *bounded* in-flight load (the slug's
  condition), the expectation is that shutdown completes cleanly within 30s — the forceful-stop
  warning should be rare/never. Under *unbounded* or adversarial load (e.g. forwarder blocked on a
  dead intake with a huge retry queue), the forceful path is legitimately reachable, so do not
  assert it as Always-clean unconditionally; scope the clean-completion assertion to the
  bounded-load workload.
- Note the **internal supervisor** shutdown (run.rs:294, `_ = internal_supervisor_handle.await`) has
  **no timeout** — it awaits indefinitely. The 30s bound applies only to the data topology. So
  "graceful shutdown within 30s" is precisely a *topology* property; the overall process exit could
  still hang on the internal supervisor (Open Question).

## Failure scenarios (Antithesis angle)

- **SIGINT under bounded load:** send SIGINT (run.rs:284) while a modest, finite amount of data is
  in flight. Expect: topology stops cleanly, `info!("All components stopped.")` logged, `Ok` result,
  exit 0; forceful-stop warning NOT emitted. Sometimes(clean shutdown completed within 30s).
- **SIGINT with a wedged downstream:** forwarder `dd_out` blocked on a dead/slow mock intake so a
  component cannot drain within 30s. Expect: forceful stop path (running.rs:111-115) →
  `Err("Topology failed to shutdown cleanly.")` → exit 1. Reachable("forced topology stop after
  grace period"). This proves the timeout actually bounds shutdown time (no indefinite hang of the
  topology).
- **Unexpected component finish** (run.rs:280) then shutdown — same 30s path.
- **Timing/interleaving:** Antithesis schedules so a component finishes right at the 30s boundary —
  exercises the race between `join_next` and `shutdown_timeout`.

## Config dependencies

- 30s is hard-coded (run.rs:290) — not configurable. The internal supervisor children use their own
  `ShutdownStrategy::Graceful(Duration::from_secs(5))` (supervisor.rs:125-126), distinct from the
  topology's 30s.
- `flush_open_windows` / aggregate flush behavior (SUT §3) affects how long the aggregate component
  takes to finish on shutdown — interacts with timing but is owned (for data preservation) by the
  data-loss agent.
- Forwarder retry-queue disk flush on shutdown (SUT §2) can extend shutdown time / block draining.
- Memory mode / backpressure (SUT §4) affects how much is in flight at shutdown.

## Assertion (MISSING — net-new instrumentation)

No Antithesis SDK assertions exist. Proposed SUT-side:
- In `shutdown_with_timeout`, on the clean break (running.rs:90-93, "All components stopped"):
  `assert_reachable!`/`Sometimes("topology shutdown completed cleanly")` and optionally record
  elapsed since shutdown start to assert `<= 30s` (it is structurally bounded, but the assertion
  documents intent).
- On the forceful-stop branch (running.rs:111-115): `assert_reachable!("topology forcefully stopped
  after grace period")` so the workload can confirm the timeout path is *reachable* under adversarial
  load, and — under the **bounded-load** workload only — a workload-side
  `AlwaysOrUnreachable`/Unreachable expectation that this branch is not taken.
- Workload-side: on SIGINT under bounded load, assert process exits 0 within ~35s (30s grace + slack)
  and that the "Forcefully stopping topology" warning is absent.

## Open questions

- **Does the overall process honor 30s, or can it still hang?** run.rs:294
  `internal_supervisor_handle.await` has no timeout. If an internal-supervisor child hangs on
  shutdown, the *process* exit can exceed 30s even though the *topology* shut down in time. WHY IT
  MATTERS: a workload asserting "process exits within ~35s" might fail for a reason outside this
  property's scope. WHAT CHANGES: either scope the assertion to topology-shutdown completion
  (log/assertion inside `shutdown_with_timeout`) rather than process exit, or file a separate
  property for internal-supervisor shutdown bounding.
- **What happens to tasks dropped on forceful stop?** On running.rs:115 break, the `JoinSet` is
  dropped, aborting remaining tasks. Confirm aborted component tasks cannot leave shared state
  (interner buffer, retry queue) corrupted. WHY IT MATTERS: clean-in-time vs. data-integrity overlap
  with the data-loss agent's property; keep the boundary clear.
- **Is `shutdown_coordinator.shutdown()` idempotent / does cascade reliably reach every component?**
  A source that never observes the shutdown signal would never finish and force the timeout. Needs a
  read of `ComponentShutdownCoordinator` to confirm all edges are signaled.

## SUT-side instrumentation needs

- Antithesis SDK dependency (none today).
- Reachable markers on both the clean-break and forceful-stop branches of `shutdown_with_timeout`
  (running.rs:90-93 and 111-115).
- Optional elapsed-time capture at shutdown start vs. completion to assert the 30s bound explicitly.
