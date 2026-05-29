---
slug: source-dispatch-no-misroute
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: Medium
assertion_status: MISSING (net-new; likely needs SUT-side Unreachable instrumentation)
---

# Property: A mid-buffer dispatch failure never mis-routes remaining events across DSD outputs

## Origin
SUT analysis §7 finding #6 ("Source dispatch errors are logged and swallowed ... a mid-buffer
dispatch failure can mis-route remaining events (eventd/service-check events leaking into the
metrics path)"). The TODO at `sources/dogstatsd/mod.rs:1670-1676` explicitly flags this. No
Antithesis assertion exists.

## What the code does
`lib/saluki-components/src/sources/dogstatsd/mod.rs` `dispatch_events` (~1667-1716):
1. TODO (~1670-1676): "if we fail to dispatch the events, we may not have iterated over all of
   them, so there might still be eventd events when we get to the service checks point, and eventd
   events and/or service check events when we get to the metrics point."
2. Eventd path (~1679-1690): if `has_event_type(EventType::EventD)`, `extract(Event::is_eventd)`
   removes all eventd events from the buffer into an iterator, then `buffered_named("events")`
   `.expect("events output should always exist")` `.send_all(...)`. On error: logs, returns nothing
   (function returns `()`), **does not re-insert**.
3. Service-check path (~1692-1704): same shape with `extract(Event::is_service_check)` →
   `buffered_named("service_checks").expect(...)`.
4. Metrics path (~1706-1715): "if there are events left, they'll be metrics" → `dispatch_named("metrics",
   event_buffer)` with the *remaining* buffer.

### Why the actual misroute is subtler than the TODO implies
`lib/saluki-core/src/topology/interconnect/event_buffer.rs`:
- `extract` (~61-88) iterates ALL events, collects matching indices, removes them from the buffer,
  and **recomputes `seen_event_types` from what remains**. It removes matching events regardless of
  whether the later send succeeds. So after `extract(is_eventd)` returns, the buffer no longer
  contains eventd events even before `send_all` runs.
- `send_all` (`dispatcher.rs:197-206`) consumes the *already-extracted iterator*. If it errors
  mid-iteration, the events it failed to push are dropped (lost), but they were already removed from
  `event_buffer`, so they cannot leak into the service_checks or metrics path.
- Net: with the current `extract`-then-`send_all` ordering, a dispatch *send* failure causes **loss**
  of the events for that output, NOT misrouting into another output's path. The "leaking into metrics"
  hazard would require `extract` to leave matching events in the buffer on a send failure — which it
  does not, because extraction and sending are separate steps.

So the property splits into two distinct claims:
- **(A) No misroute (Safety):** events of type eventd/service-check never arrive at the `metrics`
  output, and vice versa. With current code this should hold structurally (extract is by predicate,
  recomputed types), but the TODO documents authorial uncertainty and `.expect()` on outputs is a
  crash if an output is ever missing.
- **(B) No silent loss on dispatch failure (related, overlaps no-silent-interconnect-drop):** a
  `send_all`/`dispatch_named` error here is logged and the extracted events are dropped, with the
  TODO noting the component will "continue to fail to dispatch ... until the process is restarted."

## Failure scenario (Antithesis)
Drive a mixed buffer (eventd + service_check + metric events) while forcing a downstream output to
error mid-dispatch (e.g. close/saturate the events or service_checks downstream so `send_all`
errors). Observe at the mock intake / per-output telemetry that:
- no eventd or service-check payload appears on the metrics encode/forward path (misroute = false);
- the events that failed to dispatch are accounted as failures, not silently mixed elsewhere.

## Key observations
- `.expect("events output should always exist")` (~1684) and `.expect("service checks output should
  always exist")` (~1698) are crash points if those named outputs are ever unwired — a separate
  liveness/crash hazard on this path.
- A send error in the eventd step returns control but the function continues? No — on error it only
  logs inside the `if let Err` and falls through to the next `if` block; it does not early-return.
  So after an eventd send failure it still attempts service_checks then metrics with the remaining
  (eventd-free) buffer. This is the partial-iteration concern, but because eventd events were already
  extracted out, the metrics dispatch gets only non-eventd, non-service-check events.

## Config deps
- DSD source must have all three named outputs (`metrics`, `events`, `service_checks`) wired — they
  are in the production blueprint (SUT §2). If a deployment omits one, the `.expect` panics.

## Suggested assertion (MISSING — net-new, SUT-side likely required)
- **Unreachable(misroute):** an eventd or service-check `Event` reaching the metrics output path, or a
  metric reaching events/service_checks. Best as an SDK `assert_unreachable` at the point where the
  metrics dispatch buffer is assembled (`mod.rs:1706-1715`) checking that no remaining event
  `is_eventd() || is_service_check()`. This directly encodes the misroute-must-never state and would
  fire if a future refactor breaks `extract`/type-recompute. SUT-side instrumentation needed because
  the routing decision is internal and not observable from telemetry alone.
- **AlwaysOrUnreachable(dispatch-failure-counted):** when `send_all`/`dispatch_named` returns Err on
  this path, a failure/discard counter increments (no silent swallow). Overlaps the
  no-silent-interconnect-drop property; here scoped to the source dispatch.

## SUT-side instrumentation needs
- An `assert_unreachable` checking `event_buffer` contents are metrics-only at the metrics dispatch
  step (`mod.rs:~1707`). The misroute path is not externally observable, so this must be in-process.
- Optional `assert_unreachable` guarding the two `.expect(...)` output lookups (~1684/1698) to convert
  the latent panic into a tracked property if an output is missing.

## Open questions
- **Can `extract` ever leave a matching event in the buffer?** Reading `event_buffer.rs:61-88`,
  removal is by collected indices via `swap_remove_back`, with a `pos < self.events.len()` guard
  (~79). `swap_remove_back` reorders, and indices were collected before removal then applied in
  reverse — confirm no index aliasing leaves a matching event behind (would be the actual misroute
  bug the TODO fears). This is the crux: if extraction is correct, misroute is structurally
  impossible; if not, the assertion catches it.
- **Is the dropped-on-send-failure data counted anywhere?** The `error!` log (~1688/1702/1713) is the
  only signal; there may be no counter for "events lost because the source could not dispatch them,"
  unlike the interconnect `events_discarded_total`. If uncounted, the loss is fully silent — worth a
  finding and a counter.
- **Does a persistent downstream failure here wedge the source** ("continue to fail ... until the
  process is restarted")? If so, this also feeds the fail-stop/shutdown story (slug
  shutdown-drains-no-loss / supervision §2).
