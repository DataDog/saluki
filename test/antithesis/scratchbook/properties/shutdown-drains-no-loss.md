---
slug: shutdown-drains-no-loss
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Liveness (with a safety boundary at the 30s timeout)
priority: High
assertion_status: MISSING (net-new instrumentation)
---

# Property: Events accepted before the shutdown signal are drained to the forwarder before clean shutdown

## Origin
SUT analysis §5 safety #6 ("Graceful shutdown completes within 30s without forceful kill"),
docs/reference/architecture/index.md "Shutdown" section, and the §3 note that open aggregate
windows are dropped on shutdown by default. No Antithesis assertion exists.

## What the code does

### Drain-by-channel-closure design
`docs/reference/architecture/index.md:196-215`: shutdown signals sources first; sources stop
intake and finish in-flight work, then signal done. Transforms/destinations are NOT signaled —
they drain because their input channels close once all upstream senders shut down, then they
"naturally complete." The doc claims: "we ensure that all remaining events are processed before
the topology is completely shutdown." This is the drain guarantee under test.

### The 30s grace window + forceful stop
`bin/agent-data-plane/src/cli/run.rs:289-290`: `running_topology.shutdown_with_timeout(Duration::from_secs(30))`.
`lib/saluki-core/src/topology/running.rs` `shutdown_with_timeout` (~71-124):
- `shutdown_coordinator.shutdown()` (~82) triggers source shutdown which cascades downstream.
- Loops on `component_tasks.join_next_with_id()` until all components stop → `stopped_cleanly`,
  logs "All components stopped." (~89-97).
- On `shutdown_timeout` elapse (~110-115): `warn!("Forcefully stopping topology after shutdown grace
  period.")`, sets `stopped_cleanly = false`, breaks the loop. Components still running are
  abandoned (the `JoinSet` is dropped) — **in-flight data in not-yet-drained components is lost**.
- Returns `Ok(())` only if `stopped_cleanly`, else `Err("Topology failed to shutdown cleanly.")` (~119-123).

### Aggregate open-window drop by default
`lib/saluki-components/src/transforms/aggregate/mod.rs:115-133`: `flush_open_windows`
(alias `dogstatsd_flush_incomplete_buckets`) **defaults to `false`**. On stop, the current open
bucket is NOT flushed by default (to avoid double counting across restart). So data in the *current
open aggregation window* at shutdown is intentionally dropped even on a clean, within-grace shutdown
— this is a designed loss boundary distinct from the timeout boundary.

## Failure scenario (Antithesis)
Drive sustained load, then issue the shutdown signal (SIGINT/SIGTERM). Expectation:
- Every event accepted *before* the signal that has already passed aggregation into a *closed*
  window/passthrough is forwarded to the mock intake before the process exits cleanly, provided the
  topology drains within 30s.
- If the grace window is exceeded (e.g. intake is slow/blocked so the forwarder can't drain), the
  forceful-stop path is taken and in-flight data is lost — this is the *acceptable* boundary, and the
  property should assert the *clean* case and merely characterize (not forbid) the timeout case.

## Key observations
- Two designed loss boundaries make the property conditional, not absolute:
  1. **Open aggregation window** at shutdown (dropped unless `flush_open_windows=true`).
  2. **30s timeout exceeded** → forceful stop drops in-flight data.
  The clean drain claim is: *for events that have exited aggregation into a flushed window (or are
  passthrough) and given drain completes within 30s, none are lost.*
- A blocked/slow forwarder (intake down) is exactly what pushes shutdown past 30s — so the
  forwarder-eventual-delivery and disk-persistence properties interact: with disk persistence on, the
  shutdown flush persists the retry backlog (no loss); without it, a forceful stop loses it.
- Backpressure during shutdown: the source stops reading, but already-accepted events in channels must
  flow through. If any downstream is wedged (e.g. the source-dispatch wedge from
  source-dispatch-no-misroute), drain stalls and the timeout fires.

## Config deps
- 30s timeout is hard-coded (`run.rs:290`), not configurable.
- `aggregate_flush_open_windows` (default false) — toggles whether open-window data is part of the
  drained set. The assertion's "accepted before signal" set must exclude open-window data unless this
  is true.
- Disk persistence (`forwarder_retry_queue_storage_max_size`) — determines whether a forceful-stop /
  blocked-forwarder shutdown loses the retry backlog or persists it.

## Suggested assertion (MISSING — net-new)
- **Sometimes(clean-drain-no-loss):** at least once, after a shutdown signal under load with a healthy
  intake, the topology stops cleanly within 30s AND every accepted-before-signal event that reached a
  flushed window is observed at the mock intake (reconcile input-before-signal vs received). This is
  the meaningful progress state proving drain works.
- **AlwaysOrUnreachable(timeout-implies-forceful):** whenever the 30s timeout fires, the run reports
  forceful stop (`stopped_cleanly=false` / "Forcefully stopping topology" / `shutdown` returns Err) —
  i.e. the timeout path is the only way in-flight loss occurs, and it is loudly signaled, never silent.
  Anchor at `running.rs:110-115`.
- Do NOT assert an absolute Always-no-loss: the open-window default-drop and the 30s forceful path are
  designed losses that would make a blanket Always false.

## SUT-side instrumentation needs
- SDK `assert_reachable` at the clean-stop branch (`running.rs:91` "All components stopped.") to
  confirm clean shutdown is exercised.
- SDK `assert_reachable` (characterization, not failure) at the forceful-stop branch
  (`running.rs:112`) so triage can see when the timeout boundary was hit.
- Primary check is workload-side reconciliation of the pre-signal input set against the mock intake,
  excluding open-window data unless `flush_open_windows=true`.

## Open questions
- **Does the source actually wait for already-read-but-not-yet-dispatched events on shutdown?** The
  doc says sources "wait for existing work to complete" — confirm the DSD `'read` loop finishes
  dispatching the current buffer before signaling done, else events accepted just before the signal
  but still in the source are lost even on a clean shutdown.
- **Final aggregate flush on stream close:** SUT §5 liveness #1 says aggregate does a final flush on
  stream close. Confirm that final flush emits *closed* windows (not open) and that those flushed
  metrics make it through the encoder→forwarder before the 30s deadline under load.
- **What is the realistic drain time under max load with a healthy intake?** If normal drain
  approaches 30s, the clean-case assertion is fragile and the timeout boundary becomes the common case
  — important for sizing the workload and for whether 30s is adequate (a potential finding).
- **PassthroughBatcher / passthrough_idle_flush_timeout (1s)**: pre-timestamped metrics buffered there
  at shutdown — are they flushed on stop or dropped? Affects which "accepted before signal" events are
  in the drained set.
