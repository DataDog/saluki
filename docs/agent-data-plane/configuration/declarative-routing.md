# Declarative telemetry routing

> [!WARNING]
> Settings under `experimental` are unstable and may change, move, or be removed. Do not rely on backward
> compatibility.

> [!NOTE]
> Only metrics are currently supported through declarative telemetry routing.

`experimental.pipeline_config` decides which metrics each Datadog intake endpoint receives. You describe the metrics
once, as **filter sets**, name the places they can go, as **destinations**, and connect the two with **routes**.

Without this configuration, every metric goes to the primary endpoint and to every entry in `additional_endpoints`.
With it, each endpoint receives exactly the metrics that the routes reaching it select, and nothing else.

Declaring `experimental.pipeline_config` replaces `experimental.metrics_endpoint_routing` entirely. If both are set,
ADP logs a warning at startup and ignores the endpoint allowlists.

## Example

The following configuration sends payment metrics, `bob.counter`, and every metric from Kafka workloads to a second
organization on US5 instead of the primary endpoint, unless they come from a non-production environment. Separately, a
third organization on US5 receives a copy of every Kafka metric.

```yaml
experimental:
  pipeline_config:
    filter_sets:
      kafka_workloads:
        - tags:
            service: [kafka]
      non_prod:
        - tags:
            env: [dev, sandbox]
      us5_metrics:
        - metric:
            - 'glob:payments.*'
            - bob.counter
        - $kafka_workloads
    destinations:
      us5_main:
        endpoints:
          - url: 'https://app.us5.datadoghq.com'
            api_key:
              main_org: <main-org-api-key>
      us5_kafka:
        endpoints:
          - url: 'https://app.us5.datadoghq.com'
            api_key:
              kafka_org: <kafka-org-api-key>
    routes:
      - name: us5-selected
        signals: [metrics]
        mode: exclusive
        filter:
          - us5_metrics
          - not: non_prod
        send_to: [us5_main]
        encoding:
          series: v3
      - name: us5-kafka-org
        signals: [metrics]
        filter: kafka_workloads
        send_to: [us5_kafka]

```

With this configuration:

| Metric | `primary` | `us5_main` | `us5_kafka` |
|---|---|---|---|
| `payments.checkout` with `service:payments, env:prod` | No | Yes | No |
| `bob.counter` with `service:kafka, env:prod` | No | Yes | Yes |
| `cpu.idle` with `service:kafka, env:dev` | Yes | No | Yes |
| `cpu.idle` with `service:web, env:prod` | Yes | No | No |

## Filter sets

A filter set names a set of metrics. It lists matchers, and it matches whatever any of its matchers matches. A filter
set carries no allow or block meaning of its own: the route that uses it decides that.

### Matchers

Each matcher sets one or more fields. A matcher matches when every field it sets matches, and a field matches when any
of its values does:

| Field | Matches when |
|---|---|
| `metric` | The metric name matches one of the patterns. |
| `tags` | Every listed tag key carries one of its listed values. |
| `has_tags` | One of the listed tag keys is present, with any value. |
| `missing_tags` | One of the listed tag keys is absent. |
| `bare_tags` | One of the listed bare tags, such as `canary`, is present. |

Because one value is enough, `missing_tags: [a, b]` matches when either key is absent. To require both keys to be
absent, put `has_tags: [a, b]` in a filter set and route on its negation with `not`.

A matcher must set at least one field, and a field must not be an empty list. Without these rules, an empty matcher
used under `not` or in an `exclusive` route would silently match every metric.

### Metric name patterns

ADP compares metric names in the normalized form the Datadog intake stores them in. For example, `cpu-idle` is stored,
and matched, as `cpu_idle`. Each pattern is one of:

| Pattern | Matches |
|---|---|
| `cpu.idle` | Exactly this name. The name must already be in normalized form. |
| `prefix:billing.` | Names starting with `billing.`. |
| `glob:payments.*` | Names matching the glob, where `*` matches any run of characters and `?` matches one. |
| `regex:cpu\.(idle\|user)` | Names matching the regular expression, which is fully anchored. |

ADP rejects an exact name that is not in normalized form, and suggests the normalized name instead, because such a
name could never match. Glob patterns may only contain characters that can appear in a normalized name, plus `*` and
`?`.

Setting `metric` limits a filter set, and every set that includes it, to metrics.

### Tags

Tags are matched exactly and case-sensitively, against every tag a metric carries, including origin tags. A key that
carries several values matches if any of them matches. An empty value matches a tag written with an empty value, such
as `team:`.

Routing happens after host enrichment, so host tags are visible to matchers. Tags that the encoder adds later, such as
`additional_tags` for a destination, are not. Tag keys cannot be `host`, because a metric's host is not one of its
tags.

Tag values in YAML must be strings. Quote values that YAML would otherwise read as numbers or booleans, such as
`version: ["2024"]`.

### Including other sets

A filter set can include another set by listing its name prefixed with `$`, such as `- $kafka_workloads`. The set then
also matches everything the included set matches. A reference to a set that does not exist, or includes that form a
cycle, stop ADP from starting.

## Destinations

A destination names one or more endpoints, each with one or more named API keys. Each API key is a separate delivery:
two keys under one URL deliver to two different organizations.

An endpoint's `api_key` maps a name to each key. An endpoint with a single key can write it as a string instead, which
names it `default`:

```yaml
destinations:
  us5_orgs:
    endpoints:
      - url: https://app.us5.datadoghq.com
        api_key:
          parent_org: <parent-org-api-key>
          child_org: <child-org-api-key>
  eu_org:
    endpoints:
      - url: https://app.datadoghq.eu
        api_key: <eu-org-api-key>
```

### API key names

A name identifies one organization's key at one endpoint URL, across every destination. ADP tells changes apart by the
name, not by the key:

- A key whose value changes under the same name is the same organization's key, rotated. Its endpoint keeps its retry
  queue, so transactions persisted to disk before a restart are retried with the new key.
- A new name is a new organization's key, with a retry queue of its own.

Don't reuse a name for a different organization's key: transactions persisted for the old organization would be retried
with the new organization's key. Give the new organization's key a new name instead.

Because a name means one key at one URL, ADP refuses to start when:

- one name has different keys for the same URL, in any destination. This catches a rotation applied to only some
  destinations. Two keys written as strings for the same URL are both named `default`, so they conflict unless they are
  the same key: name each organization's key instead.
- one key has different names for the same URL.

The same name and key declared in two destinations refer to one endpoint. Names may contain ASCII letters, digits, `_`,
`-`, and `.`.

### Implicit destinations

Two destinations are implicit, and you cannot declare them:

- `primary` is the endpoint configured with `dd_url` or `site`, and `api_key`.
- `additional` is every entry in `additional_endpoints`.

A declared destination endpoint that repeats one of these is handled as follows:

- The same URL and API key as the primary endpoint stop ADP from starting. Send to `primary` instead.
- The same URL and API key as an entry in `additional_endpoints` refer to that entry. It keeps receiving logs, events,
  service checks, and traces, it keeps following API key rotation, and it receives each metric once however many routes
  reach it.

Declared API keys are fixed when ADP starts: they do not follow configuration updates, `ENC[...]` secret references are
not supported, and ADP does not validate them. A declared endpoint that rejects its key does not retry.

Declared endpoints only receive metrics. Logs, events, service checks, and traces keep going to the primary endpoint and
the additional endpoints.

## Routes

A route sends the metrics its filter matches to a list of destinations:

| Field | Meaning |
|---|---|
| `name` | Required. Unique across routes. |
| `signals` | Required. Only `[metrics]` is supported. Listing signals means that a signal added later never changes what an existing route carries. |
| `filter` | Optional. Selects what the route carries. A route without a filter carries every metric. |
| `send_to` | Required. The destinations to send to. |
| `mode` | Optional. `copy`, the default, or `exclusive`. |
| `encoding` | Optional. Protocol versions for the payloads the route delivers. |

The order of routes never changes what they deliver.

### Filters

A filter refers to filter sets by name, with or without `$`, and combines them:

- A list of filters matches when every filter in it matches. `[us5_metrics, { not: non_prod }]` matches metrics in
  `us5_metrics` that are not in `non_prod`.
- `{ all_of: [...] }` matches when every filter matches, and `{ any_of: [...] }` when any filter matches.
- `{ not: ... }` matches when its filter does not.

`all_of` and `any_of` must not be empty.

### Copy and exclusive modes

A `copy` route adds deliveries and changes nothing else. An `exclusive` route also removes the metrics it matches from
the `default` route. It does not remove them from any other route.

An `exclusive` route without a filter removes every metric from the `default` route. ADP logs a warning at startup when
you configure one.

### The default route

The `default` route is implicit: it sends every metric to `primary` and `additional`, less whatever `exclusive` routes
take. You can redefine it by declaring a route named `default`, for example to filter it or to change its
destinations. A redefined `default` route must still list `signals`, and must use `copy` mode.

When the `default` route has a filter, metrics that match no route are dropped. ADP logs a warning at startup when
this is the case.

### Encoding

`encoding` pins the protocol version of the series or sketch payloads a route delivers:

```yaml
encoding: { series: v3, sketches: v2 }
```

A field you leave out keeps the version the endpoint would otherwise use, as selected by `use_v3_api` and
`serializer_experimental_use_v3_api`. Every route that reaches one endpoint must use the same `encoding`, including
the `default` route, which has none unless you redefine it.

ADP refuses to start when a pin contradicts other settings: a V3 pin with zlib compression, a series pin with
`use_v2_series_api: false`, or a series pin on `primary` while an Observability Pipelines Worker or Vector metrics intake
replaces it.

## How ADP delivers metrics

For each endpoint, ADP combines the filters of every route that reaches it. Endpoints that end up with the same
combined filter share one filtered stream and one encoder, so they cost no more than one endpoint. Each endpoint belongs
to exactly one stream, so it receives each metric at most once.

A configuration that routes exactly like the ordinary setup, such as one that only declares filter sets, builds the
ordinary topology with no filtering at all.

Multi-Region Failover and autoscaling failover forwarding are unaffected.

## Validation

ADP validates the configuration when it starts and refuses to start on any problem. It reports every problem at once.
Besides the rules above, it rejects:

- references to filter sets or destinations that do not exist, with the closest names suggested;
- duplicate route names;
- endpoint URLs, API keys, or API key names that are invalid;
- configurations that send metrics nowhere;
- configurations that need more than 64 differently filtered streams.

## Startup warnings

ADP logs a warning once at startup when:

- a filter set or destination is not used by any route;
- an endpoint in `additional_endpoints` receives no metrics;
- the `default` route has a filter, or an `exclusive` route has none;
- `primary` would not receive the `datadog.agent_data_plane.running` liveness metric.

It also logs one line per filtered stream, naming the endpoints it serves and the filter they share. Warnings and the
summary name API keys but never include them.

## Environment variables

Each key holds JSON:

- `DD_EXPERIMENTAL_PIPELINE_CONFIG_FILTER_SETS`
- `DD_EXPERIMENTAL_PIPELINE_CONFIG_DESTINATIONS`
- `DD_EXPERIMENTAL_PIPELINE_CONFIG_ROUTES`

Declaring any of the three, even empty, declares the configuration.

## Limitations

- Only metrics are routed.
- The configuration is read when ADP starts. Changing it requires a restart.
- Routes do not support sampling.
