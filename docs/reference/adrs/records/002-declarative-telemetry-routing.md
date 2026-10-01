---
date: 2026-10-01
title: ADR 002 - Declarative telemetry routing with `experimental.pipeline_config`
---

# ADR 002 - Declarative telemetry routing

## Problem statement

ADP sends every metric to the primary endpoint and to every entry in `additional_endpoints`. Operators increasingly need
to send different subsets of their metrics to different places: a second organization on another site, a separate
organization for one team's workloads, or an endpoint that only needs a narrow slice of the stream.

Between the various routing mechanisms (primary site/URL, additional endpoints, MRF, Cluster Agent Local Autoscaling)
and filtering mechanisms (DSD-specific blocklists, Agent-Side Tag Filtering, Metrics Endpoint Routing), we lack a
cohesive way to both define the complete set of target organizations/endpoints where telemetry data should be routed, as
well as the means by which deciding which subsets of the overall telemetry data flow should go to those targets.

## Context

### What exists today

For high-level routing, we have the following:

- **Dual shipping** through `additional_endpoints` is all-or-nothing: every additional endpoint receives every metric.
- **`experimental.metrics_endpoint_routing`** (#2618, #2690) adds per-endpoint allowlists of exact metric names and
  literal prefixes. An endpoint with an allowlist leaves the ordinary stream and receives only its allowlist, through its
  own filter and encoder.

`experimental.metrics_endpoint_routing` proved out the idea, but its shape limits how far it can go: allowlists can't be
shared between endpoints, it's keyed by URL and so per-API key routing is not possible, and it only operates on
endpoints that already exist in `dd_url`/`site` or `additional_endpoints`, and those endpoints then receive every other
signal too.

Beyond that, there's various mechanisms for filtering the actual telemetry data itself (`statsd_metrics_blocklist`,
ASTA) which apply to all endpoints, but exist as mechanisms adjacent to Metrics Endpoint Routing, and all of these
affect the pipeline at different points.

### Constraints

- **Correctness comes first.** An endpoint must receive each metric at most once, and a configuration mistake must not
  silently drop metrics or send them somewhere they weren't meant to go.
- **No cost when unused.** The metrics path is hot, and most deployments never need to configure advanced routing.
- **Encoding is expensive.** Encoding the same stream once per endpoint doesn't scale with the number of endpoints.
- **Delivery identity matters beyond routing.** Retry queues, including transactions persisted to disk, belong to an
  endpoint and must survive API key rotation.
- **Existing features keep working**: Multi-Region Failover, autoscaling failover forwarding, alternate metrics intakes,
  and V2/V3 protocol selection.

## Considered options

- **Extend `experimental.metrics_endpoint_routing`**: add tag allowlists and further per-endpoint settings to the
  existing keys.
- **General-purpose topology configuration**: let operators define arbitrary sources, transforms, and sinks, in the
  style of Vector.
- **Declarative routing compiled into a routing plan**: name sets of telemetry (**filter sets**), name places it can go
  (**destinations**), and connect them with **routes**; compile the result at startup into a plan that ADP builds its
  topology from.

## Decision outcome

Chosen option: **Declarative routing compiled into a routing plan**

Extending the endpoint allowlists keeps the endpoint-centric shape and the URL-keyed identity, which are the root of
their limits. A general-purpose topology configuration exposes far more surface than operators need, makes it easy to
build topologies that are wrong in ways we can't detect, and couples user configuration to ADP's internal component
graph.

Declarative routing separates *what* telemetry is (filter sets), *where* it can go (destinations), and *policy*
(routes). Because ADP compiles it into a plan rather than interpreting it, ADP can validate the whole configuration up
front, share work between endpoints that receive the same metrics, and fall back to the ordinary topology when the
configuration routes like the ordinary setup does.

Most importantly, however, is that this option can be implemented by the Core Agent to allow for controlling the routing
of all telemetry data types by using a unified configuration approach that all relevant processes interpret based on
their own needs. For example, the Core Agent could parse this routing configuration to support advanced routing of
_logs_ while ADP parses it to handle just _metrics_, both processes reading the same configuration but acting on it in
only the ways that are relevant to them.

The initial scope is deliberately narrow: metrics only, read at startup, under `experimental`. See the [user
documentation](../../../agent-data-plane/configuration/declarative-routing.md) for the full configuration surface.

### Consequences

- Good, because deployments that don't configure routing, or configure it trivially, run the same topology as before.
- Good, because encoding cost scales with distinct filters rather than with endpoints.
- Good, because delivery guarantees hold by construction: at most once per target, independent of route order, and
  validated before ADP starts.
- Good, because multiple organizations at one URL, and alternate intakes, are a first-class concept.
- Good, because the configuration and the matchers aren't specific to metrics, so other signals can adopt them without
  changing the configuration's shape.
- Good, because the compiler is a pure function from configuration to plan, which keeps it unit-testable; Panoramic
  correctness tests cover the end-to-end behavior.
- Bad, because changing the configuration requires a restart.
- Bad, because the router adds a hop and a per-context decision cache to the metrics path, and each additional branch a
  metric matches copies it.
- Bad, because each declared target has its own retry queue, which adds memory and disk use per declared API key.
- Bad, because a plan can't exceed 64 filtered branches.
- Bad, because two routing configurations coexist until `experimental.metrics_endpoint_routing` is removed.
- Bad, because the compiler and predicate canonicalization are a meaningful amount of code to maintain.

## Open questions

- How would we graduate this out of experimental status?
- Is there enough value in this to expose it as a first-class citizen on the Datadog UI?
- Would / could this cannibalize Observability Pipelines?
- Are there any gotchas around API key updates that we can't handle with our existing approach in the current
  encoder/forwarder code?
- Is the configuration shape as defined _actually_ sufficient enough to eventually support logs and traces (and other
  signals)?

## References

- [Declarative metrics routing](../../../agent-data-plane/configuration/declarative-routing.md) (user documentation)
