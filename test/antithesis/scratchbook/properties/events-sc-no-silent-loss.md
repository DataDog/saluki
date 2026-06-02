---
slug: events-sc-no-silent-loss
title: Events and service-checks are delivered without silent loss under backpressure/outage
type: Liveness (with a Safety no-silent-drop clause)
priority: High
status: net-new (no SDK assertion exists)
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
---

# events-sc-no-silent-loss

## Origin
Coverage gap: the catalog's data-loss family (`no-silent-interconnect-drop`,
`forwarder-eventual-delivery`, `shutdown-drains-no-loss`) is reasoned and instrumented entirely on
the **metrics** path (encoder = `dd_metrics_encode`, the aggregation pipeline). The events and
service-check sub-pipelines are always-on production paths with a **different shape** — no
aggregation buffer, a straight `dsd_in.{events,service_checks} → *_enrich → dd_{events,service_checks}_encode
→ dd_out` chain (`run.rs:681-684`) — and their own encoders with their own silent-drop branch. None
of the existing properties assert events/SC reach the forwarder; this fills that gap. (It EXTENDS
`no-silent-interconnect-drop` / `forwarder-eventual-delivery` rather than duplicating them: same
faults, different always-on edges and a different silent-loss site.)

## Code paths (file:line)
- Wiring: `bin/agent-data-plane/src/cli/run.rs:681-684` — events and service_checks edges; both
  terminate at `dd_out` (the shared Datadog forwarder).
- Source dispatch fan-out: `lib/saluki-components/src/sources/dogstatsd/mod.rs:1667-1716`
  (`dispatch_events`): `extract(is_eventd)` → `buffered_named("events").send_all(...)` then
  `extract(is_service_check)` → `buffered_named("service_checks").send_all(...)`. `send_all` awaits
  on a full bounded mpsc (backpressure), but a dispatch **error** is only `error!`-logged and
  swallowed (`mod.rs:1688`, `mod.rs:1702`) — no drop counter on this path.
- Events encoder silent-drop branch: `lib/saluki-components/src/encoders/datadog/events/mod.rs:179-194`
  — on a **recoverable** encode error the event is dropped and only `events_dropped_encoder()`
  incremented (`telemetry.rs:50,77`); TODO admits the dropped count is hardcoded `1`, not the real
  number (`events/mod.rs:186`). Flush build error is `error!`-logged and the request discarded
  (`events/mod.rs:208`).
- Service-checks encoder twin: `lib/saluki-components/src/encoders/datadog/service_checks/mod.rs:177-211`
  — identical recoverable-drop + flush-discard structure.
- Wrong-type silent swallow: encoder `process_event` calls `event.try_into_eventd()` /
  `try_into_service_check()` and returns `ProcessResult::Continue` (consuming + dropping) when the
  type does not match (`events/mod.rs:173-177`, `service_checks/mod.rs:171-175`;
  `data_model/event/mod.rs:167-182`). A mis-routed or mistyped event is lost here with NO counter —
  ties to `source-dispatch-no-misroute`.
- Zero-payload-size config trap: `events/mod.rs:64-67` documents that `serializer_max_payload_size: 0`
  makes **every** non-empty compressed payload exceed the limit and be dropped during flush (a silent
  total-loss config). Same clamp logic applies to service checks.
- Egress (shared with metrics): the `dd_out` forwarder retry/circuit-breaker/queue-drop behavior is
  already characterized in `forwarder-eventual-delivery` / `retry-queue-bounded-under-outage`.

## Failure scenario
Under a slow/throttled or transiently-down intake, the encoder→forwarder edge fills and backpressure
should propagate up the events/SC edges to the source read loop (queue-and-await, never drop). Two
silent-loss risks specific to these paths: (1) the encoder's recoverable-error branch drops
events/SC with an undercounted (`+1`) telemetry signal; (2) a wrong-type event reaching the encoder
is swallowed with no counter at all. After a transient intake outage clears, every accepted event/SC
that did not legitimately overflow the (shared) retry queue should still be delivered. A regression
that turns a backpressure-await into a drop, or mis-scopes the recoverable-error branch, silently
loses customer events/service-checks — the "won't lose customer data" half of the headline, on a
path no existing property watches.

## Observations
- Events/SC have **no aggregation stage**, so unlike metrics there is no flush-window semantics —
  every accepted event/SC should map ~1:1 to a delivered intake item (modulo batching of up to
  `MAX_EVENTS_PER_PAYLOAD = 100`, `events/mod.rs:35`). This makes a count-in == count-out reconcile
  cleaner than for metrics.
- The `events_received` / `service_checks_received` source counters share the metric name
  `component_events_received_total` distinguished only by a `message_type` tag
  (`sources/dogstatsd/metrics.rs:111-119`) — the workload checker must filter by tag, not name.
- `events_sent` (`telemetry.rs:41,83`) on the encoders is the delivery-side anchor.

## Suggested assertions (MISSING / net-new)
- Safety: `Always(no silent drop on a wired events/SC edge under load)` — modeled like
  `no-silent-interconnect-drop` but asserted on the events + service_checks edges; backpressure
  (await), never discard, on a connected output.
- Liveness: `Sometimes(all-accepted-events-delivered-after-recovery)` and
  `Sometimes(all-accepted-service-checks-delivered-after-recovery)` — post-recovery delivered count
  (`events_sent`, filtered) ≥ accepted count (`events_received` by `message_type`), minus legitimate
  retry-queue overflow. Liveness ⇒ progress, not an instantaneous invariant.
- Reachability anchors (REQUIRED to prevent vacuity, esp. for a metrics-heavy workload):
  `Sometimes(events_received{message_type=events} > 0)` and
  `Sometimes(service_checks_received > 0)`.
- Optional Safety guard: `Always(events_dropped_encoder delta == 0)` while intake is healthy and
  config is non-pathological — catches the recoverable-error drop firing when it shouldn't.

## Config dependencies
- DSD enabled; events/service_checks on by default (`mod.rs:205-221`).
- Keep `serializer_max_payload_size` / `serializer_max_uncompressed_payload_size` at non-pathological
  values for the "no-loss" branch; a separate negative case can set `serializer_max_payload_size: 0`
  to confirm the documented total-drop trap (`events/mod.rs:64-67`).
- Shared forwarder/retry-queue config (disk persistence, queue byte caps) governs the eventual-
  delivery branch exactly as for `forwarder-eventual-delivery`.

## SUT-side instrumentation needs
- Workload-side: drive an event/SC stream, throttle/down the mock intake, then restore; reconcile
  accepted (`component_events_received_total{message_type in (events, service_checks)}`) vs delivered
  (`component_events_sent_total` on the events/SC encoders) at the mock intake, allowing for retry-
  queue overflow and ~`MAX_*_PER_PAYLOAD` batching slack.
- The dispatch-error path (`mod.rs:1688,1702`) and the wrong-type swallow have NO counter — a strict
  no-silent-loss assertion needs net-new SUT-side instrumentation (a drop counter or an
  `assert_unreachable`) there, else loss on those branches is invisible to a workload checker.

## Open Questions
- Is the encoder "recoverable error" branch (`events/mod.rs:183`) ever hit on healthy intake with
  well-formed events, or only on genuinely oversized single events? Determines whether the optional
  `Always(events_dropped_encoder == 0)` guard is sound or flaky.
- Does the events/SC retry traffic share `dd_out`'s per-endpoint queue with metrics, so a metric
  flood can evict queued events (cross-stream eviction)? Affects how the overflow allowance is scoped.
- Are events/SC requests `Clone` (so retryable failures take the `Error::Open` re-enqueue path), as
  was confirmed for the metrics forwarder requests in `forwarder-eventual-delivery`? Needs checking
  for the `/api/v1/events_batch` and service-check request builders.
- Does `dispatch_events` count anything when `send_all` errors, or is dispatch-time loss fully silent
  (a finding, shared with `source-dispatch-no-misroute`)?
