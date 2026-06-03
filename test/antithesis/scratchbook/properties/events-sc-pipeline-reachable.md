---
slug: events-sc-pipeline-reachable
title: Events and service-check sub-pipelines are actually exercised (anti-vacuity anchor)
type: Reachability
priority: Medium
status: net-new (no SDK assertion exists)
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
---

# events-sc-pipeline-reachable

## Origin
Coverage gap + anti-vacuity guard. The entire existing 27-property catalog is metrics-only. A
realistic ADP workload is dominated by metric samples; events and service-checks are comparatively
rare. Without an explicit reachability anchor, the two new event/SC safety/liveness properties
(`malformed-event-sc-no-crash`, `events-sc-no-silent-loss`) can pass **vacuously** — the assertions
never fail simply because no event ever traversed the parse → enrich → encode → deliver chain. This
property exists to make the event/SC paths' execution a first-class, observable test obligation, per
the catalog-wide note that `Sometimes(...)` anchors are mandatory to prove a path is reached
(`property-catalog.md` "Catalog-wide notes").

## Code paths (file:line)
- Parse: `lib/saluki-io/src/deser/codec/dogstatsd/event.rs:31` /
  `.../service_check.rs:28` (codecs decode a frame into `ParsedPacket::Event` / `::ServiceCheck`).
- Source accept + counter: `lib/saluki-components/src/sources/dogstatsd/mod.rs:1502-1517` increments
  `events_received()` on a successfully handled event; `mod.rs:1519-1537` increments
  `service_checks_received()`. Counters: `sources/dogstatsd/metrics.rs:34-39,114-119` (both emit
  `component_events_received_total` with a distinguishing `message_type` tag).
- Dispatch onto the named outputs: `mod.rs:1679-1704` (`buffered_named("events")` /
  `buffered_named("service_checks")`).
- Delivery: events encoder `encoders/datadog/events/mod.rs:197-213` and service-checks encoder
  `encoders/datadog/service_checks/mod.rs:195-211` dispatch an `HttpPayload`; success increments
  `events_sent` (`common/datadog/telemetry.rs:83`) → reaches `dd_out` → mock intake
  `/api/v1/events_batch` (events) and the service-checks intake endpoint.

## Failure scenario
Not a SUT bug per se — a **test-quality** failure: if this anchor never fires, the event/SC
properties provide no real assurance. It also surfaces a genuine SUT regression class: a wiring or
filter change (e.g. `EnablePayloadsConfiguration` defaulting events/SC off, a future filter dropping
all events, a broken named output) that silently removes the event/SC path entirely would make this
`Sometimes` go unsatisfied — a real, observable defect on an "always-on production path."

## Observations
- Defaults make the path live: `EnablePayloadsConfiguration { events: true, service_checks: true }`
  (`sources/dogstatsd/mod.rs:205-221`); the edges are unconditionally wired in `run.rs:681-684`
  (not behind a feature flag like `dsd_debug_log_out`).
- Two milestones are worth separate anchors so a parse-but-don't-deliver regression is visible:
  (a) **parsed/accepted** at the source, (b) **delivered** at the encoder/intake.

## Suggested assertions (MISSING / net-new)
- `Sometimes(event_parsed_and_accepted)` — at least once `events_received`
  (`component_events_received_total{message_type=events}`) advances.
- `Sometimes(service_check_parsed_and_accepted)` — `service_checks_received` advances.
- `Sometimes(event_delivered)` / `Sometimes(service_check_delivered)` — the events/SC encoder's
  `events_sent` advances and a payload reaches the mock intake's events / service-check endpoint.
- (Strengthen to `Reachable` if the workload guarantees ≥1 well-formed event + SC per run.)

## Config dependencies
- DSD enabled; events/service_checks left at their `true` defaults (`mod.rs:205-221`).
- Workload MUST emit at least one well-formed event (`_e{...}`) and one well-formed service check
  (`_sc|...`) so the anchors can fire — this is a workload-construction requirement, not a SUT config.

## SUT-side instrumentation needs
- Source-side anchors read the existing `component_events_received_total` counter (filter by
  `message_type` tag) — no new instrumentation strictly required for the "parsed/accepted" milestone.
- Delivery-side anchors read `component_events_sent_total` on the events/SC encoders and/or observe
  the mock intake receiving an events/service-check payload — the cleanest signal is a mock-intake
  observation, which the deployment topology's controllable mock intake already supports.

## Open Questions
- Should the delivery anchor key on the encoder `events_sent` counter or on the mock intake actually
  receiving the `/api/v1/events_batch` (and service-check) POST? Intake observation is stronger
  (proves end-to-end) but depends on the mock intake distinguishing those endpoints.
- Is one anchor per stream sufficient, or do we want per-(event vs service-check) AND
  per-(parsed vs delivered) granularity (4 anchors) to localize a parse-but-not-deliver regression?
  Leaning toward 4 for diagnostic value at negligible cost.
