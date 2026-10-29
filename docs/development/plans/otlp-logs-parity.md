# OTLP logs parity plan

This plan tracks the work to make ADP's OTLP logs intake behave the same as the core Datadog Agent's embedded OTLP
receiver (`otlp_config.receiver` + `otlp_config.logs.enabled`). It records what the Agent does, what ADP does today,
the gaps between them, and a sequence of small, independently testable steps to close them.

Agent references are relative to the root of the Datadog Agent repository (`DataDog/datadog-agent`). The OTLP logs translation is the in-repo copy at
`pkg/opentelemetry-mapping-go/otlp/logs` (`go.mod` replaces the upstream module with it).

## How the Agent handles OTLP logs

### Pipeline

```
OTLP receiver (gRPC/HTTP)
  -> infraattributes processor            (tagger enrichment, shared with metrics)
  -> logsagent exporter                   (OTLP -> HTTPLogItem translation, batching via exporterhelper)
  -> main logs agent pipeline channel     (same processor/sender as tailed logs)
  -> logs processor                       (global processing_rules, hostname fallback)
  -> JSON encoder + HTTP destination      (/api/v2/logs on the logs intake)
```

- Built in `comp/otelcol/otlp/map_provider_config_not_serverless.go` and
  `comp/otelcol/collector/impl-pipeline/pipeline.go`.
- The logs pipeline exists only when `otlp_config.logs.enabled` **and** `logs_enabled` are both true. If OTLP logs
  are enabled without the logs agent, creating the **whole** OTLP ingest pipeline fails with "OTLP logs is enabled but
  logs agent is not enabled" (`comp/otelcol/otlp/pipeline_validator.go`). The Agent logs the error and keeps running
  without any OTLP receiver, so OTLP metrics and traces stop too (`impl-pipeline/pipeline.go:107`). With the logs
  pipeline absent, the receiver doesn't serve the logs service at all.
- The exporter has an always-active `exporterhelper` batcher **before translation** (`min_size` 8192 items,
  `max_size` 0, `flush_timeout` 200 ms, 1000-request queue), configured by `otlp_config.logs.batch.*`. It is separate
  from the logs sender's `logs_config.batch_*`, and it sets exporter call boundaries, latency, queueing and
  backpressure, and the request and event telemetry.

### Translation (`transform.go`, `translator.go`)

Per resource:

- **Hostname**: `SourceFromAttrs` on resource attributes, used only for `HostnameKind`. If empty, it tries
  `hostFromAttributes` on the log record's attributes. The full precedence (`host`, `datadog.host.name`, Fargate,
  cloud providers, `k8s.node.name[-cluster]`, `host.id`, `host.name`, localhost filtering) lives in
  `attributes/source.go`.
- **Service**: resource `service.name` (`AsString`, any type), falling back to the log attribute `service.name`.
  - Quirk: host and service are per-resource variables, set outside the scope and record loops. A fallback value
    found on one record carries over to all following records of the same resource, including records in later
    `ScopeLogs` (`translator.go:248-269`).
- **`ddtags`**: one ordered, comma-separated string. Without a `ddtags` log attribute it's
  `join(TagsFromAttributes(resource), ",")` (core, Kubernetes, container, cloud, and process mappings). With one, the
  raw attribute value is appended to the resource tags. Then `,otel_source:datadog_agent` is appended, or the string is
  set to `otel_source:datadog_agent` alone if it's empty. Whitespace, duplicates, and empty elements are kept as they
  are.

Per record, in the order below. Collisions don't follow a simple "last write wins" rule; see the precedence notes.

1. **Log attributes**. Reserved keys are matched on the lowercased key, but ordinary attributes keep their original
   spelling:
   - `msg`, `message`, `log` set the message (`AsString`, any type).
   - `status`, `severity`, `level`, `syslog.severity` set the status as a **raw string**; they aren't stored as a
     property.
   - `traceid`/`trace_id`/`contextmap.traceid`/`oteltraceid` must be 32 hex chars. They set `dd.trace_id` (decimal of
     the low 8 bytes) and `otel.trace_id`. The span ID keys (16 hex chars) set `dd.span_id`/`otel.span_id`.
   - `otel.library.name`/`otel.library.version` map to `otel.scope.name`/`otel.scope.version`, unless the canonical key
     is also present in the same attribute collection.
   - `ddtags` is merged into the tags.
   - Everything else goes through `flattenAttribute`: maps are flattened to dotted keys, scalars keep
     their native JSON type, **arrays stay JSON arrays** (elements are recursed), and bytes are base64. Flattening
     stops only when `depth == 10`. Arrays increment depth too, so maps nested under arrays can pass depth 10 and
     keep flattening (`transform.go:229-267`). A non-finite double stays a float here, so the final JSON marshal
     fails and the exporter drops the log.
2. **Resource attributes**: `hostname`/`service` become `otel.hostname`/`otel.service`. Everything else is stored with
   `AsString` (not flattened; ints and maps become strings). Library aliases follow the same rule as for log
   attributes. Resource values overwrite record-derived values, including scope values from aliases.
3. **Scope attributes** (`AsString`) overwrite both. The instrumentation scope's name and version then fill
   `otel.scope.name`/`otel.scope.version` only if they're still missing (`transform.go:143-175`).
4. The record's `trace_id`/`span_id` fields overwrite the attribute-derived IDs.
5. **Severity**: `otel.severity_text` is stored raw. `otel.severity_number` is stored as a string. Status precedence is
   attribute, then severity text, then severity number, where the number maps as ≤4 trace, ≤8 debug, ≤12 info, ≤16 warn,
   ≤20 error, ≤24 fatal, otherwise error. `status` is **always** written into the properties, possibly as `""`.
6. **Timestamp**: `time_unix_nano`, falling back to `observed_time_unix_nano`. It sets `otel.timestamp` and
   `@timestamp` (`2006-01-02T15:04:05.000Z07:00`, UTC).
7. `event_name` is stored as `otel.event_name`.
8. If there's still no message, the body's `AsString` is used: maps and slices become compact JSON, bytes become base64,
   and numbers and bools become their string form.

### Exporter and logs agent

- The `ddtags` field is removed from the JSON and sent as message origin tags instead.
- The status is the `status` property if non-empty, otherwise `info`.
- The source is the `datadog.log.source` property if present, otherwise `otlp_log_ingestion`.
- The message *content* is the JSON serialization of the whole `HTTPLogItem`: message, hostname, service, and all
  properties, with properties overwriting same-named top-level fields.
- The logs processor applies global `logs_config.processing_rules` (`exclude_at_match`, `include_at_match`,
  `mask_sequences`) to the raw content bytes, in list order, using Go `regexp`. Masks change the bytes later rules see,
  every include rule must match, and the first failing rule drops the message. Only the content changes; envelope fields
  captured by the exporter stay as they were. The validator also accepts `multi_line`, `exclude_truncated` and
  `remap_source` (no-ops for OTLP messages). Invalid rules stop the logs agent from starting
  (`processing_rules.go:45-88`, `impl/agent.go:248-254`). If the log has no hostname, it uses the Agent's hostname. This
  fallback applies only to the envelope: the content JSON was marshaled earlier and has no `hostname` in that case.
- It adds no host tags, `tags`, or `expected_tags`. Global and entity tags come only from `infraattributes`.
- The HTTP payload is a JSON array of `{"message": "<content JSON string>", "status", "timestamp": <encode time, ms>,
  "hostname", "service", "ddsource", "ddtags"}`.
  - Headers: `DD-PROTOCOL: agent-json`, `DD-EVP-ORIGIN: agent`, `DD-EVP-ORIGIN-VERSION`, `dd-message-timestamp`,
    `dd-current-timestamp`, and `Content-Encoding`.
  - Defaults: zstd level 1, batches of up to 1000 logs or 5 MB of encoded content **before compression** (a single
    message over the content limit is dropped), and a periodic 5 s `batch_wait` ticker that flushes all batches
    (`sender/batch_strategy.go:101-130`).
- **Transport**: HTTP or TCP. HTTP is chosen when `force_use_http` is set, `logs_dd_url` has an `http(s)://` scheme, or
  the HTTP connectivity check passes and TCP isn't requested (`use_tcp`/`force_use_tcp`, SOCKS). Otherwise the Agent
  falls back to TCP (`comp/logs/agent/config/config.go:124-143`).
- Endpoints come from `logs_config.*` and belong only to the logs pipeline: `logs_dd_url`, `logs_no_ssl` (an explicit
  URL scheme overrides it), `additional_endpoints` (a list of objects with their own `host`, `api_key`, `use_ssl`,
  `path_prefix` and `is_reliable`), `api_key` (a non-empty value overrides the global one), `use_compression`,
  `compression_kind` (invalid values fall back to zstd), `zstd_compression_level`, `batch_*`, and `use_v2_api`
  (`false` sends to `/v1/input`).
- Telemetry counters: `runtime.datadog_agent_otlp_logs_requests` (once per exporter call) and
  `runtime.datadog_agent_otlp_logs_events` (the raw input `LogRecordCount`), both incremented before translation and
  behind the exporterhelper batcher (`logs_exporter.go:105-112`). Status is the global Logs Agent status section
  (running state, endpoints, metrics, errors, warnings). The logs flare provider only adds file-permission data for
  file and journal tailers, so OTLP contributes nothing to the flare.

### Infra attributes enrichment

`infraattributesprocessor/logs.go`, `common.go`:

- Only when the `container.id` key is **absent** (an empty or non-string value suppresses this), resolves it through
  origin detection from `process.pid` (int), `k8s.pod.uid` + `k8s.container.name`, the cgroup inode (int), and
  `datadog.container.is_init`, then writes it as a resource attribute (`common.go:96-103`).
- Collects tagger tags for all entity IDs derived from the resource (container, image, ECS task, deployment,
  namespace, node, pod, process), plus `GlobalTags`. The first value for a duplicate key wins. Cardinality is always
  `low` for logs: the embedded pipeline doesn't map `otlp_config.metrics.tag_cardinality` into the processor.
- Writes them back as **resource attributes**. Unified service tags only fill `service.name`, `deployment.environment`,
  and `service.version` when they're missing (`env` counts as present if either `deployment.environment` or
  `deployment.environment.name` exists). Other tags overwrite existing resource attributes as `k=v`. The translator
  then turns them into JSON properties, and into ddtags for known keys.
- `otlp_config.logs.infra_attributes.tags_as_ddtags` appends custom keys to the record's `ddtags`.

## Send path: Agent logs sender vs. ADP forwarder

The Agent's logs pipeline doesn't use the core `defaultforwarder`; it has its own sender (`comp/logs-library/sender`,
`client/http`). ADP sends logs through the shared Datadog forwarder (`dd_out`) that also carries metrics, events,
service checks, and traces. The main differences:

| Aspect | Agent logs sender | ADP (shared `dd_out`) |
|---|---|---|
| Isolation | Dedicated to logs | Shared queues, concurrency permits, and one backoff state per endpoint across all payload types, so logs failures pause metrics and vice versa |
| Endpoints | `logs_config.logs_dd_url`, `logs_config.additional_endpoints`, MRF for logs | `dd_url`-derived logs host plus the global `additional_endpoints`; no MRF for logs |
| Reliable / unreliable | A payload blocks only until **at least one** reliable destination accepts it. Other reliable destinations get a non-blocking send, and their copy is dropped if their 10-slot buffer is full or they're retrying (`sender/worker.go:119-162`). Unreliable destinations get one non-blocking attempt, no retries | No such distinction; every endpoint retries |
| Failure behavior | A payload already admitted to a destination is retried forever while held (`client/http/destination.go:270-343`). When no reliable destination accepts, the pipeline blocks back to the OTLP exporter, whose queue eventually rejects requests. A down reliable additional endpoint loses payloads while the main one is healthy | Queues and retries; the retry queue (15 MiB default) evicts the oldest data, or spills to disk with `forwarder_storage_*`. No backpressure, so clients see success while data is lost |
| Retry order | Each in-flight request retries its own payload | Newest first (in memory before disk) |
| Backoff, timeout | `sender_backoff_*` (base 1 s, max 120 s), `http_timeout` 10 s | `forwarder_backoff_*` (base 2 s, max 64 s), `forwarder_timeout` 20 s |
| Concurrency | `pipelines × 10` (40) in-flight requests per destination, or `pipelines × batch_max_concurrent_send` | `forwarder_max_concurrent_requests × forwarder_num_workers`, shared with all payload types |
| Payload fan-out | One encoded buffer shared by all destinations | One transaction per endpoint from the same payload |
| Headers | `DD-PROTOCOL`, `DD-EVP-ORIGIN`(`-VERSION`), `dd-message-timestamp`, `dd-current-timestamp` refreshed per attempt, `User-Agent: datadog-agent/<ver>` | None of the `DD-*` headers; `User-Agent: agent-data-plane/<ver>`, `DD-Agent-Version` |
| Disk persistence | None for logs | Optional, via the shared `forwarder_storage_*` |
| Telemetry, status | `logs.sent`, `logs.dropped`, `logs.retry_count`, `logs.bytes_sent`, and per-endpoint status lines | Generic `network_http_*` tagged `endpoint=logs_v2`; queue and drop counters mixed across payload types; no status section |

Status-code classification is the same on both sides (400, 401, 413 dropped; 403 triggers a secret refresh when
secrets are used; everything else retried). Also note that setting `logs_config.additional_endpoints` makes the Agent
use **TCP** unless `force_use_http` is set (`config_keys.go:171-173`), so HTTP dual shipping only matches the Agent
for configurations with `force_use_http`.

Unverified: a possible Agent quirk where, with MRF enabled, a retrying main endpoint leads to drops instead of
backpressure (`sender/worker.go:123-127`), and the exporterhelper queue defaults (1000 requests, reject when full).

## How the logs intake handles Agent payloads

Based on a review of the logs intake's implementation. Logs are parsed after the intake stage, in the logs
processing pipeline, followed by its default attribute remapping. What matters for parity:

- **`DD-PROTOCOL` picks the parser.** With `agent-json`, only the seven envelope keys (`message`, `timestamp`,
  `hostname`, `status`, `service`, `ddsource`, `ddtags`) are read; **every other top-level key is dropped**. Without
  the header, the default JSON parser reads the whole object, merges an inner JSON `message` over it, and extracts
  `ddtags`/`ddsource` from the merged map. Any other value is rejected with 400. `DD-EVP-ORIGIN: agent` falls through
  to the plain agent-json config. `dd-message-timestamp`, `dd-current-timestamp`, and `DD-EVP-ORIGIN-VERSION` only
  feed latency metrics and logging.
- **The JSON `message` content becomes attributes.** If it starts with `{` and parses as an object, its inner `message`
  becomes the displayed message and everything else becomes attributes (invalid JSON leaves the string as the
  message). Inner `ddtags` and `ddsource` stay plain attributes.
- **Inner fields override the envelope.** The default remappers set the date from `@timestamp`/`timestamp`/`date`/...,
  the host from `host`/`hostname`, the service from `service`/`dd.service`, and the status from
  `status`/`severity`/`level`, first match wins, overwriting the envelope values.
- **Envelope `status` is stored raw** with no normalization (a missing status defaults to `info`). Remapped attribute
  statuses are normalized: numbers 0–8 map to syslog levels (other numbers to `info`),
  and strings mostly by first character, with extra rules (for example `e`/`err*` is error but `emerg*` is emergency,
  and `f*` is emergency). Any non-null value wins over the envelope, including an empty or unmappable one, which
  becomes `info`. The Agent always writes a `status` attribute
  into OTLP content, so **for OTLP logs the intake status always comes from the content**; the envelope status only
  matters for check logs.
- **`ddtags` is parsed into a set**: split on `,`, trimmed, empty elements dropped, duplicates removed. Tag order is
  never kept.
- **Key order doesn't matter**, except that dotted keys are expanded into nested maps, so a scalar and a map landing
  on the same key are resolved by order. Duplicate keys: the last one wins.
- **Timestamps**: the envelope `timestamp` is epoch ms. Logs older than 18 h or more than 12 h in the future are
  dropped.
- **Ingest size** is attributed per event from its share of the decompressed payload bytes, so serialization
  differences (whitespace, escaping, extra keys) change what customers are billed for.
- **Limits**: 5.1 MiB per payload (compressed and decompressed, 413 otherwise), at most 2048 keys per object, depth 20,
  and events over 1 MiB after processing are truncated. There's no per-payload log count limit, and invalid JSON
  still gets 202.
- Nothing special-cases `ddsource: otlp_log_ingestion` or `otel_source:*` for agent-json payloads.
- `/v1/input` is rewritten by the ingress before it reaches the logs parsing; that part wasn't traced.

Consequences: today ADP sends no `DD-PROTOCOL`, so its top-level attributes survive through the default parser. The
user-visible effect of `gap/content-in-message` is smaller than the wire diff suggests, but switching on `agent-json`
before the content moves into `message` would silently drop every attribute. The `ddtags` formatting differences
(`gap/ddtags-formatting`) aren't visible after intake parsing, but byte-level differences are, through ingest size.

## Configuration relevant to the OTLP logs path

### Agent keys

Of the ~130 logs-related keys in the vendored schema, about 50 affect the Agent's OTLP logs path. The rest only apply
to tailers, launchers, decoders, or the auditor, which OTLP logs never pass through.

- **Enablement**: `logs_enabled`/`log_enabled`, `otlp_config.logs.enabled`, and `otlp_config.logs.batch.*`,
  `otlp_config.logs.infra_attributes.tags_as_ddtags`.
- **Endpoints, transport, auth**: `logs_config.api_key`, `additional_endpoints`, `dd_url`, `logs_dd_url`,
  `logs_no_ssl`, `dev_mode_no_ssl`, `use_http`/`force_use_http`, `use_tcp`/`force_use_tcp`, `socks5_proxy_address`,
  `use_v2_api`, `use_compression`, `compression_kind`, `compression_level`, `zstd_compression_level`,
  `connection_reset_interval`, `http_protocol`, `http_timeout`, `sender_backoff_*`, `sender_recovery_*`,
  `http_connectivity_retry_interval_max`, and `delegated_auth.org_uuid`/`refresh_interval_mins`. TCP only:
  `dd_port`, `dd_url_443`, `use_port_443`, `dev_mode_use_proto`.
- **Batching and sender**: `batch_wait`, `batch_max_size`, `batch_max_content_size`, `batch_max_concurrent_send`,
  `disable_distributed_senders`.
- **Pipeline topology and shutdown**: `pipelines`, `pipeline_failover.*`, `message_channel_size`,
  `payload_channel_size`, `stop_grace_period`.
- **Processing**: `processing_rules`. An invalid `processing_rules` or `fingerprint_config.*` also stops the logs
  agent from starting, which in turn fails the whole OTLP pipeline.
- **Outside `logs_config`**: `site` and the global `api_key` (fallbacks; the top-level `dd_url` isn't used for logs),
  `multi_region_failover.*` including `failover_logs` and `logs_service_allowlist`,
  `observability_pipelines_worker.logs.*` and `vector.logs.*` (OPW replaces or dual-ships to the main endpoint and
  forces HTTP), the HTTP transport keys (`proxy.*`, `no_proxy_nonexact_match`, `skip_ssl_validation`,
  `min_tls_version`, `sslkeylogfile`, `tls_handshake_timeout`, `http_dial_fallback_delay`), secret refresh keys,
  global `delegated_auth.*`, hostname resolution keys, `otlp_config.receiver.*`, `otlp_config.debug.verbosity`,
  `data_plane.otlp.proxy.*`, and `provider_kind` (through the tagger's global tags).

Notable findings:

- **Nothing on the OTLP path truncates content.** `max_message_size_bytes` and `tag_truncated_logs` are decoder
  settings. The only size limit is `batch_max_content_size`, and a larger message is dropped.
- **Adaptive sampling and noisy-log detection run in the decoder** (`pkg/logs/internal/decoder/decoder.go`), so
  `experimental_adaptive_sampling.*` and `experimental_noisy_log_detection` don't apply to OTLP logs.
- **`tags`/`extra_tags` aren't added to OTLP logs.** The OTLP log source has an empty `LogsConfig`
  (`logsagentexporter/factory.go:105`), and the processor adds no host tags. Only the tagger's global tags arrive,
  through infraattributes.
- **Logs `additional_endpoints` force TCP and gzip** unless HTTP is forced (`force_use_http`, an `http(s)://`
  `logs_dd_url`, or OPW) and `compression_kind` is set explicitly.
- **Likely Agent bug, not reproduced**: the OTLP exporter takes one pipeline channel at startup. A later TCP-to-HTTP
  upgrade stops that pipeline provider and closes the channel (`comp/logs/agent/impl/agent_restart.go`), so the
  exporter would send to a closed channel and panic. ADP shouldn't replicate this.

### ADP's configuration system

- `schema_overlay.yaml` classifies every schema key as `full`, `partial`, `none` (with a severity and a `planned`
  flag), or `unknown` in its `inventory` section, or silently ignores it in its `excluded` section. Docs and a runtime
  classifier are generated from it (see the configuration system guide).
- **The startup compatibility gate** (`agent-data-plane-config-system/src/compatibility.rs`) refuses to start ADP
  when an input explicitly sets a `none`/`high` key that affects an active pipeline; `medium` and `partial` keys only
  log warnings. Excluded keys are never checked.
- **Today all 123 `logs_config.*` keys, `logs_enabled`, and `log_enabled` are in `excluded`** ("ignored in initial
  audit"), so a configuration ADP would silently mishandle, such as `processing_rules` or a custom `logs_dd_url`,
  never blocks startup or even warns. `otlp_config.logs.batch.*` and `tags_as_ddtags` are excluded too.
- **`otlp_config.logs.enabled` is classified `full`**, although `gap/signal-gating` shows it isn't honored per signal.
- **The classifier has no logs pipeline.** Pipelines are `dogstatsd`, `checks`, `otlp`, and `traces` (`traces` is
  active only when OTLP traces are handled natively). Classifying logs keys as `otlp` would also block ADP in proxy
  mode, where the core Agent handles logs and the keys don't matter to ADP.
- The Agent's data plane **preflight mode** (`comp/dataplane/preflightmode`) runs ADP briefly in isolation with OTLP
  explicitly disabled, so it doesn't exercise any OTLP configuration.

## What ADP has today

- **Source**: `lib/saluki-components/src/sources/otlp/logs/{translator,transform}.rs` ports a large part of the
  translator (message and status keys, trace and span IDs from fields and attributes, nested-map flattening,
  `otel.severity_*`, `otel.timestamp`/`@timestamp`, the `otel.hostname`/`otel.service` renames, `ddtags`, and
  `otel_source:datadog_agent`). It resolves origin tags through `OtlpOriginTagResolver` and adds them directly as tags.
- **Topology**: `otlp_in.logs -> dd_logs_encode -> dd_out` (`bin/agent-data-plane/src/cli/run.rs`). There's no
  processing stage in between.
- **Encoder**: `lib/saluki-components/src/encoders/datadog/logs/mod.rs`. It is also used by the checks logs path.
- **Config**: only `otlp_config.logs.enabled` (`domains/otlp.rs` `Receiver::logs_enabled`). All 123 `logs_config.*` keys
  are ignored in `schema_overlay.yaml`.
- **Proxy mode** (`data_plane.otlp.proxy.logs`) already relays logs to the core Agent unchanged.

## Gaps

### Wire format (encoder)

| ID | Gap |
|---|---|
| `gap/content-in-message` | ADP puts additional properties at the **top level** of each log object and `message` contains only `{"message","service"}`. The Agent puts message, hostname, service, status and **all properties** inside the `message` content string. |
| `gap/status-strings` | Status strings are capitalized (`"Error"`, `"Warning"`) instead of the Agent's raw or lowercase values (`"error"`, `"warn"`, and raw attribute or severity-text values). The intake stores the envelope status raw, but for OTLP logs the content `status` attribute always overrides it, so after `otlp-render-step` this is wire-only for OTLP. It stays user-visible for check logs, and for OTLP logs until `otlp-render-step` lands. |
| `gap/envelope-timestamp` | ADP has no outer `timestamp` (ms) field and injects `@timestamp: now` when the record has none. The Agent leaves `@timestamp` out of the content and sets the outer `timestamp`. |
| `gap/missing-headers` | The `DD-PROTOCOL`, `DD-EVP-ORIGIN`, `DD-EVP-ORIGIN-VERSION`, `dd-message-timestamp` and `dd-current-timestamp` headers are missing. |
| `gap/logs-batching-limits` | Compression and batching follow the shared metrics settings (zstd level 3, 3,200,000-byte compressed limit, ~60 MiB uncompressed limit, one-shot flush timer after the first event) instead of `logs_config` (zstd level 1, 5 MB pre-compression content limit with oversized messages dropped, 1000 logs, periodic 5 s ticker). |

### Translation (source)

| ID | Gap |
|---|---|
| `gap/zero-severity-status` | `severity_number == 0` with no text gives `Trace`. The Agent gives no status, which the exporter turns into `info`. |
| `gap/raw-status-strings` | Unrecognized status strings from attributes or severity text are dropped and fall through to the number mapping. The Agent passes the raw string through. A `status` property is always present in the Agent's output. |
| `gap/as-string-coercion` | Resource and scope attributes are emitted as native JSON types. The Agent uses `AsString`, so ints, bools and maps become strings. The same applies to `service.name` (resource and record fallback), and to empty `AnyValue`s, which the Agent turns into `""` where ADP drops them. |
| `gap/array-attributes` | Arrays in log attributes are stringified. The Agent keeps them as JSON arrays, flattens maps inside them, and has the `depth == 10` flattening quirk. |
| `gap/bytes-encoding` | Bytes values become UTF-8 text or `"<n bytes>"`. The Agent uses base64, in both attributes and the body. |
| `gap/non-string-message` | Message attributes (`msg`/`message`/`log`) are honored only for string values. The Agent uses `AsString` for any type. |
| `gap/observed-timestamp` | `observed_time_unix_nano` isn't used as a timestamp fallback. |
| `gap/event-name` | `otel.event_name` is missing. |
| `gap/scope-metadata` | Scope name and version (`otel.scope.name`/`otel.scope.version`) and the `otel.library.*` aliases are missing. |
| `gap/log-source-attribute` | `datadog.log.source` doesn't set `ddsource`. The Agent reads the final property after all collisions, uses it only if it's a non-empty string, and keeps it in the content. |
| `gap/host-resolution` | Hostname resolution is incomplete at both levels. The logs path uses `resource_to_source`, which only knows ECS Fargate and `host.name`. ADP already has a near-complete port in `resource_to_metric_source`/`hostname_from_attributes` (`common/otlp/util.rs`), but logs don't use it. Record fallback checks only `host.name`. The per-resource host **and** service carry-over isn't replicated (it must be, see Decisions). |
| `gap/default-hostname` | The Agent's hostname isn't used when the record has none. ADP omits `hostname`. `default_hostname` is plumbed into the source but unused for logs. |
| `gap/value-formatting` | Number and JSON formatting of non-string bodies and attributes (Go `strconv` versus `serde_json`, map key order) isn't verified. |
| `gap/ddtags-formatting` | ADP builds `ddtags` as a deduplicated tag set from trimmed, non-empty entries. The Agent builds the ordered raw string described above. An empty `ddtags` attribute gives an empty element only when resource tags exist; otherwise the result is `otel_source` alone. **Intentional difference**, see Decisions: only the formatting differs, and the intake reduces both to the same set. |
| `gap/attribute-key-case` | Every record attribute key is lowercased in place, so an ordinary attribute `RequestID` becomes `requestid`. The Agent lowercases only for reserved-key matching (`transform.rs` attribute loop vs. `transform.go:92-139`). |

### Enrichment

| ID | Gap |
|---|---|
| `gap/infra-attributes` | ADP logs use single-origin enrichment (`resolve_origin_tags`): tags from only the first non-empty local data, pod, or PID entity, added directly as tags, with no global tags. Metrics already use the fuller `resolve_resource_tags`. In the Agent, tags for all entities plus global tags become resource attributes, so they appear as JSON properties, become ddtags only for known keys, and fill missing unified service tags, which changes `service`. |
| `gap/tags-as-ddtags` | `otlp_config.logs.infra_attributes.tags_as_ddtags` isn't supported. |
| `gap/origin-detection` | Origin-detection edge semantics differ: ADP treats an empty `container.id` as absent, accepts only strings for entity IDs that the Agent reads with `AsString`, and ignores `datadog.container.is_init`. ADP's resolver also can't return the detected container ID for injection as an attribute. |

### Configuration and gating

| ID | Gap |
|---|---|
| `gap/logs-enabled` | `logs_enabled` is unsupported (ignored in the overlay) and doesn't gate OTLP logs. Setting `otlp_config.logs.enabled` without it should disable the whole OTLP receiver with a logged error, as in the Agent. |
| `gap/signal-gating` | `receiver.logs_enabled` isn't enforced per signal: logs are accepted and forwarded whenever any OTLP signal is enabled. The Agent doesn't serve the logs service. |
| `gap/logs-endpoints` | `logs_config` endpoint keys are ignored: `logs_dd_url`, `logs_no_ssl`, `additional_endpoints`, `api_key`, `use_compression`, `compression_kind`, `zstd_compression_level`, `batch_*`, `use_v2_api`. Logs always go to the `dd_url`-derived logs authority, through the shared `dd_out` forwarder, so they also go to the global `additional_endpoints`, which the Agent never does for logs. |
| `gap/processing-rules` | `logs_config.processing_rules` isn't supported: it's marked unsupported in the overlay and nothing parses, validates, or applies it. |
| `gap/exporter-batching` | The OTLP exporter's pre-translation batcher (`otlp_config.logs.batch.*`) doesn't exist. It affects call boundaries, latency, backpressure, and telemetry, though not log content. |
| `gap/shared-forwarder` | Logs share the metrics forwarder: one backoff state, concurrency limit, and queue set per endpoint across all payload types, with `forwarder_*` settings instead of the logs sender's (`sender_backoff_*`, `http_timeout`, `batch_max_concurrent_send`). See the send path comparison. |
| `gap/tcp-transport` | ADP sends logs only over HTTP. The Agent picks HTTP or TCP from configuration and an HTTP connectivity check, and falls back to TCP. Whether and how to address this is decided in the TCP transport phase. |
| `gap/config-inventory` | The overlay doesn't reflect OTLP logs support: the ~50 relevant Agent keys are silently excluded, `otlp_config.logs.enabled` is marked `full`, and there's no logs pipeline in the classifier, so unsupported configurations neither block startup nor warn. See "Configuration relevant to the OTLP logs path". |
| `gap/logs-mrf-opw` | Multi-region failover for logs (`multi_region_failover.failover_logs`, `logs_service_allowlist`, and the MRF endpoint) and OPW/Vector for logs (`observability_pipelines_worker.logs.*`, `vector.logs.*`) aren't supported. |
| `gap/proxy-signal-flags` | In proxy mode, ADP ignores `data_plane.otlp.proxy.logs.enabled` and `proxy.metrics.enabled` and always relays both. The relay replies success to the client once the payload is queued, so logs the core Agent rejects are dropped silently, and the forwarder logs the failure as a metrics failure. |

### Operational

| ID | Gap |
|---|---|
| `gap/telemetry-status` | Telemetry: the `runtime.datadog_agent_otlp_logs_requests` and `runtime.datadog_agent_otlp_logs_events` counters are missing. ADP's `logs_received` counts translated output records instead, and there are no remapping rules for OTLP logs. ADP registers no logs status section (only DogStatsD). |
| `gap/backpressure` | Backpressure and retry semantics differ. The Agent blocks the exporter on the pipeline channel, and the sender holds a retryable payload and retries it with backoff until success or shutdown. ADP acknowledges OTLP requests once they're queued in a 1024-item channel, and its forwarder uses bounded high- and low-priority queues that favor fresh traffic and can drop older transactions. |

## Testing infrastructure

| Layer | OTLP logs today | Gap |
|---|---|---|
| Unit | 16 translator tests. `transform.rs` has no direct tests. 4 `build_agent_json_*` encoder tests. | No golden vectors from the Agent. No driver-based encoder test (payload shape, headers, compression, batching). |
| Component | One source test that only counts events. | No source -> encoder round trip. |
| Integration | Config plumbing only (`adp-config-otlp`). | No action sends OTLP. No intake assertion for logs. |
| Correctness | Nothing. | Millstone has no logs payload. `datadog-intake` sends `/api/v2/logs` to a fallback that returns 200 and **discards the body**. `stele` has no log type. Panoramic has no log collection or analyzer. |
| SMP | `otlp_ingest_logs_5mb` (throughput, `erratic`, no `checks:` bounds, so not a quality gate). | It only sets `DD_OTLP_CONFIG: "{}"`. After the gating phase it needs both `logs_enabled` and `otlp_config.logs.enabled`, native mode, and proof that logs were accepted. |
| Antithesis | No OTLP coverage. The harness, simulated intake (metrics and events only), and scenarios are DogStatsD-only. | Whether and how to extend it to OTLP logs is decided in the backpressure and fault tolerance phase (`antithesis-otlp-investigation`). |

Useful assets:

- **Millstone generator**: lading (pinned rev) already ships `opentelemetry::log`, and millstone's gRPC target is
  generic, so `static_base64` corpora can send logs before the variant exists.
- **Agent golden vectors**:
  - `pkg/opentelemetry-mapping-go/otlp/logs/transform_test.go`: about 29 table cases with expected `HTTPLogItem`
    JSON, plus `TestDeriveStatus`.
  - `logsagentexporter/logs_exporter_test.go`: content JSON, source, and tags.
  - `infraattributesprocessor/logs_ddtags_test.go`: 8 `tags_as_ddtags` cases.
- **Correctness baseline config**: the Agent needs `logs_enabled: true`, `otlp_config.logs.enabled: true`,
  `logs_config.logs_dd_url: datadog-intake:2049`, `logs_config.logs_no_ssl: true` and `logs_config.force_use_http:
  true`. In the comparison container, make sure the core Agent's own logs agent collects nothing else.

## Plan

Each step is meant to be one PR, or a small stack of PRs, and includes its own automated tests. The correctness
framework comes first so that every later step can show its effect against the real Agent.

**Ratchet**: known differences are tracked explicitly, per gap ID, at two levels:

- **Correctness cases** carry a list of known-different fields, each tagged with its gap ID. Entries are scoped to the
  fields and fixtures they affect, never global.
- **Unit golden vectors** keep the Agent's target output and ADP's current output side by side, with a
  machine-checked gap manifest naming which vectors are expected to differ and why. Tests assert the current output
  exactly, so unexpected changes (regressions or accidental fixes) fail CI.

Each later step removes its gap's entries from both and updates the current fixtures to the target in the same change.
A step isn't done until its entries are gone and the tests pass without them.

### Test foundations

- [x] `intake-logs-capture` **Fake intake decodes logs.**
  - Add `POST /api/v2/logs` and `POST /v1/input` (used when `use_v2_api` is false) to `datadog-intake`, plus
    `GET /logs/dump`.
  - Capture per request: `{path, headers, logs}`, so later steps can assert headers (`intake-headers`) and batching
    (`logs-sender-batching`) against the request that carried them.
  - Add `stele::Log` that keeps the **location** of every field: the envelope fields (message, status, hostname,
    service, ddsource, timestamp, and ddtags as a set: split, trimmed, empty elements and duplicates dropped, as the
    intake does), any other `envelope_properties`, and `content_properties` parsed from the `message` string when it's
    JSON. Also provide a canonical semantic view for matching. Keeping the locations separate means
    `gap/content-in-message` shows up as a diff instead of being normalized away.
  - Tests: intake unit tests with captured Agent and ADP payloads (gzip and zstd), and malformed compression and JSON.
- [x] `logs-analyzer` **Panoramic logs plumbing and analyzer.**
  - Add `IntakeClient::logs()`, the request-level captures in `CollectedData`, and logs in the traffic artifacts.
  - Add `AnalysisMode::Logs`. Matching uses a deliberately stable identity key (message plus input timestamp and
    trace identifiers, after configured normalization), not all properties, so a single property regression still
    pairs the right logs. Within each bucket, pair logs by minimum difference, deterministically. Report count
    mismatches and per-field diffs, including field-location diffs.
  - Classify each diff as **intake-visible** or **wire-only**, using a small model of the intake's parsing
    (agent-json envelope keys, inner `message` merge, remapper precedence and status mapping, `ddtags` as a set).
    Wire-only diffs still count against the ratchet, but the classification shows which gaps users would notice.
    Tests for the model cover numeric statuses, `error` vs. `emergency`, and empty and unmappable status strings.
  - Require non-empty output from both sides, with an optional expected minimum count, as the events and service
    check analyzers already do.
  - Add the gap-tagged `ignore_fields` list (the ratchet) and a comparator for encode-time timestamps. This landed as
    `logs.known_differences`: each entry names a gap, location-qualified path globs (`*` within a segment, `**` for
    any number of segments), and optionally the diff kinds (`value`, `missing`, `extra`, `location`). An entry path
    that matches nothing fails the case as stale. A known difference may not name an identity field, since logs are
    paired before known differences apply; drop the field from `logs.identity_fields` instead.
  - Tests: analyzer unit tests for identical, reordered, field diff, field-location diff, missing, extra, both
    sides empty, one side empty, duplicate keys, competing near-matches, ignored nested fields, timestamp tolerance
    boundaries, and a large input.
- [ ] `millstone-otlp-logs` **Millstone `opentelemetry_logs` payload** wrapping lading's logs generator.
  - Tests: the existing `every_test_case_config_parses` test, plus a determinism test for the same seed.
  - The lading generator only produces string bodies, empty severity text, no observed timestamp, and no event name,
    so it gives volume, not field coverage. Field coverage comes from the static corpora in `first-correctness-cases`.
- [ ] `first-correctness-cases` **First correctness cases.**
  - Set every enablement key explicitly on both sides (`logs_enabled`, `otlp_config.logs.enabled`, and native ADP
    handling with proxying disabled for logs), rather than relying on defaults.
  - `otlp-logs`: lading generator, for volume and batching.
  - `otlp-logs-attributes` and further `static_base64` corpora, with a **gap-to-fixture coverage matrix** in this plan
    that maps every wire format and translation gap to the fixture that exercises it. This covers service,
    host, host carry-over, ddtags (including an empty `ddtags` attribute, compared as a set), trace context,
    severity variants, timestamp fallback, event name, bytes, non-string message attributes, structured and array
    bodies, and resource and scope attributes. `gap/ddtags-formatting` is listed as an intentional difference.
  - Enrichment, configuration, and operational gaps are marked "not yet exercised" in the matrix, and each gets its
    own case in the phase that fixes it.
  - Both cases start with ratchet entries for every diff they show, which confirms the gap list against real output.
- [ ] `golden-vectors` **Golden vectors and source-to-encoder round trip.**
  - Port the Agent's `transform_test.go` and `logs_exporter_test.go` cases into a table-driven Rust test with the
    target and current fixtures and the gap manifest described under Ratchet.
  - The test drives encoded OTLP requests through the source, translator, and logs encoder, then decodes the HTTP
    request with the same parser as the fake intake (shared through `stele`). That covers the production handoff
    `otlp_in.logs -> dd_logs_encode`, where field-location and collision bugs occur.
- [ ] `encoder-characterization` **Logs encoder characterization test** with the component test driver.
  - This test records **current** behavior, with named target assertions for later steps: exact headers belong to
    `intake-headers`, and Agent-compatible sizes and batching belong to `logs-sender-batching`.
  - It covers the URI, compression, the 1000-logs cap (with 1001 logs), size splitting with deterministic
    incompressible input, a single oversized log, and lossless reconstruction across splits.
- [ ] `integration-otlp-send` **Integration harness can send OTLP and inspect logs.** Add an `otlp_send` action to
  panoramic (gRPC and HTTP, from checked-in protobuf fixtures, recording the response status) and an `intake_has_log`
  assertion backed by the fake intake from `intake-logs-capture`.
  - Tests: panoramic unit tests for the action and assertion, and an `otlp-logs-enabled` integration case that sends a
    log and finds it in the intake.

### Configuration parity

These tasks make the configuration inventory honest early, so unsupported configurations are visible, and refuse to
start when ADP would silently diverge, before native OTLP logs are used widely. Every later task that implements a key
updates its overlay classification and generated docs in the same change.

- [ ] `logs-enabled-config` **`logs_enabled` configuration (`gap/logs-enabled`).** Move `logs_enabled` (and the
  deprecated `log_enabled`) from `excluded` into the inventory, add a typed global field, and translate it.
  - Tests: config translator tests, including the deprecated alias.
- [ ] `native-logs-pipeline` **Classifier pipeline for native OTLP logs (`gap/config-inventory`).** Depends on
  `logs-enabled-config`. Add a pipeline that is active only when ADP handles OTLP logs natively, like `traces` for OTLP
  traces. Logs-only keys then affect startup only when ADP actually processes logs. Derive it from the same function
  that decides the topology's logs wiring, so the classifier and the wiring can't disagree. Today that's "OTLP enabled,
  logs enabled, and proxy mode off"; once `proxy-signal-flags` lands it becomes "OTLP enabled, logs enabled, and
  either proxy mode off or `proxy.logs.enabled` false".
  - Tests: `active_pipelines` tests for native, proxy, and per-signal proxy configurations, and compatibility gate tests
    showing a logs key is checked in native mode and skipped in proxy mode.
- [ ] `logs-config-inventory` **Classify the OTLP logs keys (`gap/config-inventory`).** Audit all the Agent keys that
  affect the OTLP logs path (see "Configuration relevant to the OTLP logs path"), and check in a key-by-key matrix of
  their classification. Move the excluded ones into `inventory` with accurate support levels, the new pipeline,
  documentation of differences, and `planned` flags pointing at the tasks in this plan. Some are already inventoried
  (for example `api_key`, `additional_endpoints`, `site`, `min_tls_version`, `proxy.http`, `provider_kind`, and the
  `data_plane.otlp.proxy.*` flags); review those for logs affinity and refine them deliberately rather than
  replacing them. Reclassify `otlp_config.logs.enabled` as `partial` until `otlp-signal-gating` lands. Give the
  keys that don't affect OTLP logs dated exclusion reasons that name the feature, instead of "ignored in initial
  audit". Check that the keys outside `logs_config` (MRF, OPW, HTTP transport, hostname) are classified with the logs
  pipeline in mind.
  - Tests: overlay validation and code generation (`make build-schema-overlay`), classifier tests for representative
    keys, and a review of the generated compatibility docs.
- [ ] `logs-config-startup-gate` **Decide which unsupported keys block startup (`gap/config-inventory`).** Depends on
  `logs-config-inventory`. `high` stops ADP from starting, `medium` warns, and `low` only logs at debug level, so
  severities need care. Extend the key matrix with each unsupported key's user-visible consequence, pipeline affinity,
  native vs. proxy applicability, and behavior in connected and standalone mode, then derive the severities from it.
  As part of this, determine what happens to OTLP traffic when ADP refuses to start, in connected and standalone
  mode, since preflight mode doesn't cover OTLP.
  - Output: the severities recorded in the overlay, with the matrix as their rationale.
  - Tests: derived from the matrix: integration cases where a `high` key set explicitly stops ADP with the error, a
    `medium` key starts with a warning, and the same keys don't block ADP in proxy mode.

### Gating and proxy mode

- [ ] `otlp-signal-gating` **Per-signal gating and Agent validator semantics (`gap/logs-enabled`,
  `gap/signal-gating`).**
  - When OTLP logs are disabled, don't serve the logs service (gRPC `Unimplemented`, HTTP 404) and don't create the
    logs output path. Make `logs_pipeline_required` and the `otlp_in.logs` wiring conditional, while keeping the
    checks logs path.
  - When `otlp_config.logs.enabled` is set without `logs_enabled`, log the Agent's error and don't start any OTLP
    receiver, while ADP keeps running.
  - Apply the same gating in proxy mode, in ADP itself (see the proxy-mode gating decision): the relay doesn't serve
    disabled signals, and doesn't listen at all when the validator fails. Native and proxy mode share one gating
    implementation.
  - Tests: source and relay component tests, and integration cases over a matrix of native, proxy, and
    checks-enabled configurations. Assert gRPC `Unimplemented` and HTTP 404 for logs, metrics still accepted, zero
    logs in the intake, process stability, the error message, and no OTLP ports when the validator fails. In proxy
    mode, compare the responses a client sees from the Agent alone with those it sees through ADP.
- [ ] `proxy-signal-flags` **Per-signal proxy flags and upstream rejection visibility (`gap/proxy-signal-flags`).**
  - Precursor: proxy mode emits raw gRPC payloads for logs and metrics and wires them straight to the core Agent
    forwarder; only traces have a standalone decoder (`run.rs` proxy wiring). Native logs decoding and translation
    are coupled to the OTLP source and its converter (`sources/otlp/mod.rs`). Extract a reusable logs decoder, as
    exists for traces, so relayed logs can take the native path. This may be its own PR.
  - Honor `data_plane.otlp.proxy.logs.enabled` and `data_plane.otlp.proxy.metrics.enabled`: a signal whose flag is
    false is handled natively instead of being relayed, as traces already are. This is how native logs can be turned
    on selectively while proxy mode stays the default.
  - Update the shared logs-wiring function used by `native-logs-pipeline`, so the classifier follows the new flag.
  - Count and log rejections from the core Agent per signal, with the correct signal name (the forwarder currently
    reports logs failures as "Failed to export metrics"). This catches drift between ADP's gating and the Agent's.
  - Tests: topology tests for each flag combination, relay and forwarder tests for the rejection counter, and an
    integration case with proxy mode on and `proxy.logs.enabled: false` that finds the log in the intake via the
    native path.

### Wire format

- [ ] `rendered-content-model` **Log data model for rendered content (`gap/zero-severity-status`,
  `gap/raw-status-strings`).**
  - Mark a `Log`'s message as rendered content.
  - Represent status as the Agent does: the raw content status string (from attributes, severity text, or severity
    number, possibly empty) separately from the envelope status, which defaults to `info` when the raw status is
    empty. Severity number 0 gives an empty status.
  - Tests: translator unit tests, and golden-vector manifest entries for `gap/zero-severity-status` and
    `gap/raw-status-strings` removed.
- [ ] `otlp-render-step` **OTLP render step (`gap/content-in-message`).** Serialize a translated log into the Agent's
  `HTTPLogItem` content JSON. Follow the Agent's collision rules: properties overwrite top-level fields, only `ddtags`
  is removed from content, and `status` and `datadog.log.source` are copied to the envelope and kept in the content.
  - Serialize byte for byte like the Agent, which uses Go's standard `encoding/json` (the `goccy_gojson` build tag
    isn't set): alphabetically sorted keys, HTML escaping of `<`, `>` and `&` as `\u003c`/`\u003e`/`\u0026`, invalid
    UTF-8 replaced with U+FFFD, `\u2028`/`\u2029` escaped, and Go float formatting. The intake bills ingest size by
    serialized bytes, so these differences are customer-visible.
  - Tests: golden tests from the Agent's `logs_exporter_test.go`, and full-wire collision cases for `message`,
    `hostname`, `service`, `status`, `datadog.log.source`, and `ddtags`. Go-generated byte-exact vectors for key
    order, HTML-sensitive characters, invalid UTF-8, line separators, and floats.
- [ ] `checks-logs-characterization` **Checks logs characterization test.** Add a checks source-to-wire test that
  records the current check log payload before the encoder changes, so `envelope-encoder`'s change to check logs is
  deliberate and visible.
- [ ] `envelope-encoder` **Generic envelope encoder (`gap/content-in-message`, `gap/status-strings`,
  `gap/envelope-timestamp`, `gap/default-hostname`).**
  - The encoder writes the Agent envelope with the content copied verbatim into `message`, an encode-time `timestamp`
    in ms, no injected `@timestamp`, and the envelope status.
  - Apply the Agent hostname fallback to the envelope only, after rendering.
  - Wire the render step into the topology between `otlp_in.logs` and `dd_logs_encode`.
  - This changes check logs: the `{"message": ...}` wrapper is replaced by the raw message. Update
    `checks-logs-characterization`'s expectation and tell the Agent Check Runner owners before landing.
  - Tests: encoder envelope tests. Add a hostname test: a missing host gives the default outer `hostname` and no
    content `hostname`, and an explicit OTLP host appears in both. Remove the envelope entries from the ratchet.
- [ ] `intake-headers` **Headers (`gap/missing-headers`).** Must land together with or after `envelope-encoder`. With
  `DD-PROTOCOL: agent-json` the intake drops every top-level key except the seven envelope keys, so sending it while
  attributes are still at the top level would silently lose them.
  - Static headers: `DD-PROTOCOL: agent-json`, `DD-EVP-ORIGIN: agent`, and `DD-EVP-ORIGIN-VERSION`.
  - `dd-message-timestamp` is the ingestion time (ms) of the last message in the payload. This needs a per-log
    ingestion timestamp set when the log is created and carried into payload metadata.
  - `dd-current-timestamp` is set at each send attempt, so it belongs in the send path (refreshed on retry), not in
    the encoder's static headers. Implement it in the current forwarder; `logs-send-architecture` may move it.
  - Tests: numeric values, selection of the last message across a batch, the timestamp refreshed on retry, and header
    assertions in the intake for the correctness cases.

### Translation fidelity

Each item is a small PR that updates golden vectors and removes ratchet entries. Collision and precedence behavior is
tested with one table-driven case per precedence edge, not only with presence checks.

- [ ] `attribute-key-spelling` **Attribute key spelling (`gap/attribute-key-case`).** Match reserved keys
  case-insensitively and keep the original spelling for everything else.
  - Tests: mixed-case ordinary, nested, and reserved keys.
- [ ] `pdata-as-string` **pdata `AsString` helper and Go oracle vectors (`gap/value-formatting`).** Add a shared
  `as_string(AnyValue)` that matches pdata exactly: float formatting and exponent thresholds, `-0`, `NaN`/`±Infinity`,
  base64 bytes, sorted map keys, and no HTML escaping.
  - Tests: version-pinned vectors generated with Go and checked in, compared byte for byte. Cover boundary
    magnitudes, `-0`, non-finite values, and nested maps containing `<>&`.
- [ ] `value-coercion` **Value coercion (`gap/as-string-coercion`, `gap/array-attributes`, `gap/bytes-encoding`,
  `gap/non-string-message`).** Use `AsString` everywhere the Agent does: resource and scope attributes, `service.name`
  (resource and record fallback), message attributes, the body, and empty values (`""`). Keep native types for record
  attributes, keep arrays as arrays with maps inside flattened, and replicate the `depth == 10` quirk. Drop the log when
  a non-finite double in a record attribute would make the Agent's JSON marshal fail.
  - Tests: empty-value vectors for ordinary, message, resource, scope, and tag attributes. Maps at depths 9, 10, and
    11 reached through arrays. Non-finite doubles in every location.
- [ ] `timestamp-event-name` **Timestamps and event name**: the observed-timestamp fallback (`gap/observed-timestamp`)
  and `otel.event_name` (`gap/event-name`).
- [ ] `scope-alias-precedence` **Scope metadata and alias precedence (`gap/scope-metadata`).** Implement the full
  collision matrix: aliases on record and resource attributes defer to canonical keys in the same collection, resource
  values overwrite record-derived ones, scope attributes overwrite both, and the instrumentation scope's name and
  version fill only missing keys.
  - Tests: one case per precedence edge.
- [ ] `host-service-resolution` **Hostname and service resolution (`gap/host-resolution`).** Replace
  `resource_to_source` in the logs path with the Agent's full `SourceFromAttrs` algorithm. Start from
  `resource_to_metric_source`/`hostname_from_attributes`, and check its serverless handling (Azure, GCP, ECS Fargate)
  against `attributes/source.go`. Use the full `hostFromAttributes` for the record fallback, and replicate the host and
  service carry-over across records and `ScopeLogs`.
  - Tests: a table test for every precedence branch, non-host serverless sources, invalid hostnames, resource vs.
    record precedence, first-record-only host, first-record-only service, reversed record order, and carry-over
    across a scope boundary.
- [ ] `log-source-attribute` **`datadog.log.source` (`gap/log-source-attribute`).** Depends on `value-coercion`. Read
  the final property after all collisions, use it as `ddsource` only when it's a non-empty string, and keep it in the
  content.
  - Tests: full-wire cases for record, resource, and scope collisions, non-string values from each location, empty
    strings, and the property still present in the content.
### Enrichment parity

- [ ] `container-id-detection` **Container ID detection and injection (`gap/origin-detection`).** Extend the workload
  provider and resolver API to return the canonical container ID from origin detection, equivalent to the Agent's
  `GenerateContainerIDFromOriginInfo`, and inject it as `container.id` when the key is absent, even when the container
  has no tags.
  - Tests: a compatibility matrix with missing, empty, and wrong-typed `container.id`; signed, overflowing, and zero
    PID and inode values; `is_init`; incomplete pod and container pairs; and precedence when several detection inputs
    are present. Port the detection cases from `infraattributesprocessor/logs_test.go`.
- [ ] `infra-attributes` **Infra attributes as resource attributes (`gap/infra-attributes`).** Depends on
  `container-id-detection`. Switch logs to the full ordered entity list plus global tags, and apply them the way
  `infraattributes` does: fill unified service tags when missing, overwrite other resource attributes, and let the
  translator turn known keys into ddtags.
  - Always request `low` cardinality for logs, independently of `otlp_config.metrics.tag_cardinality`.
  - Tests: table-driven tests with a mock workload provider for all three unified service tags, both environment
    spellings, entity vs. global duplicate keys, existing resource attribute collisions, invalid tags, and every
    supported entity ID. A regression test with metrics cardinality set to `none` and to `high`. Source-to-wire
    assertions showing the `service` change and the placement in properties and `ddtags`. A `kubernetes_in_docker`
    correctness case (`otlp-logs-k8s-origin`).
- [ ] `tags-as-ddtags` **`tags_as_ddtags` (`gap/tags-as-ddtags`).** Config key, overlay update, and translator support.
  Only custom tagger-derived keys are diverted; known convention, Kubernetes, and unified service tag keys stay resource
  attributes either way.
  - Tests: YAML and environment variable translation tests, a source-builder test, and the 8 `logs_ddtags_test.go`
    cases. A correctness fixture that produces a custom key (a pod label in the Kubernetes harness, or a configured
    custom global tag), run with the option enabled and disabled, asserting the normalized `ddtags` set, the presence
    or absence of the content property, and that known keys are preserved.

### Logs endpoint configuration and send path

- [ ] `logs-send-architecture` **Investigate and decide the logs send architecture (`gap/shared-forwarder`,
  `gap/backpressure`).** Starting from the send path comparison, choose how ADP sends logs. Candidates to evaluate
  include: adding logs-specific policy to the shared forwarder; a second forwarder instance dedicated to logs and
  configured from `logs_config.*`; a dedicated logs sender that mirrors the Agent's design (periodic batching, one
  encoded buffer shared by all destinations, reliable and unreliable destinations, retry forever with backpressure); or
  a staged combination. Evaluate each against parity (especially backpressure and the "at least one reliable
  destination" rule), risk to the metrics path, code reuse (TLS, proxy, API key rotation, HTTP client, classifier,
  backoff), and testability.
  - First deliverable, before the decision: a reproducible scripted-intake characterization of both sides' failure
    behavior, with the results recorded in this plan. Cover response-code classification, attempt sequence and
    backoff, queue exhaustion and loss, recovery, shutdown, multiple reliable destinations with one retrying, and
    unreliable destinations. Assert client-visible OTLP latency and status, and exact delivered counts.
  - Output: a recorded decision in this plan, with the architecture-dependent steps updated to match: `intake-headers`
    (where the per-attempt `dd-current-timestamp` header is set), `logs-routing-isolation`, `logs-endpoint-config`
    (endpoint construction), `logs-additional-endpoints`, `logs-sender-batching`, `logs-status-section` (status and
    telemetry ownership), and `backpressure-retries`.
- [ ] `logs-routing-isolation` **Logs routing isolation (`gap/logs-endpoints`).** Depends on `logs-send-architecture`.
  Give logs their own routing, as decided in `logs-send-architecture`. It must recognize both `/api/v2/logs` and
  `/v1/input`. Logs must stop going to the global `additional_endpoints`, and no other payload type may reach logs
  endpoints.
  - Tests: forwarder unit tests showing that logs endpoints receive only logs, and that the shared metric endpoints
    keep their existing traffic. Include conflicting `dd_url` and logs destinations.
- [ ] `logs-endpoint-config` **Typed `logs_config` endpoint domain (`gap/logs-endpoints`).** `logs_dd_url` (with scheme
  handling), `logs_no_ssl`, `logs_config.api_key` precedence, `use_v2_api`, and
  `use_compression`/`compression_kind`/`zstd_compression_level`, with the Agent's fallbacks for invalid values. Update
  the overlay and the configuration docs.
  - Tests: config translator tests. Request-level integration tests with deliberately conflicting global and logs
    endpoint and API-key values. Wire matrices for no compression, gzip, zstd, V1 and V2, explicit HTTP and HTTPS
    URLs, and invalid-value fallbacks. Assert destination, path, `DD-API-KEY`, `Content-Encoding`, and lossless
    decoded content. Switch the correctness cases to `logs_dd_url` on both sides.
- [ ] `logs-additional-endpoints` **`logs_config.additional_endpoints` (`gap/logs-endpoints`).** Depends on
  `logs-send-architecture`. Only configurations that force HTTP (`force_use_http`, an `http(s)://` `logs_dd_url`, or
  OPW) are covered here, since otherwise the Agent switches to TCP (see the TCP transport phase). The full typed entry:
  `host`, `api_key`, `use_ssl`, `path_prefix`, and `is_reliable`, with HTTP entries inheriting the primary's compression
  and API version.
  - Tests: table tests for per-endpoint API keys, SSL override, path prefix, V1/V2 inheritance, duplicate and invalid
    entries, and reliable vs. best-effort classification, plus an integration case with two intakes. Retry behavior
    is left to `gap/backpressure`.
- [ ] `logs-mrf-opw` **Multi-region failover and OPW for logs (`gap/logs-mrf-opw`).** Depends on
  `logs-send-architecture`. MRF: the MRF endpoint, `failover_logs` and `logs_service_allowlist` tagging, and routing,
  including their runtime updates. OPW/Vector: replacing the main endpoint, or dual shipping to an unreliable (or
  `dual_ship_reliable`) endpoint, and forcing HTTP.
  - Tests: endpoint construction tables, and integration cases with a second intake standing in for the MRF and OPW
    endpoints.
- [ ] `otlp-exporter-batching` **OTLP exporter batching (`gap/exporter-batching`).** Implement the pre-translation
  batcher in the OTLP logs path with all three `otlp_config.logs.batch.*` keys, separate from the encoder batching in
  `logs-sender-batching`.
  - Tests: fake-clock unit tests for `min_size`, `max_size`, and `flush_timeout`, and config translator tests.
- [ ] `logs-sender-batching` **Logs sender batching (`gap/logs-batching-limits`).** `batch_max_size`, and
  `batch_max_content_size` counted on encoded content before compression. Remove the shared compressed and uncompressed
  limits for logs, drop a single message over the content limit, and flush on a periodic `batch_wait` ticker rather than
  a one-shot timer.
  - Tests: deterministic driver tests for exact count and content boundaries, incompressible data, and the oversized
    drop. Fake-clock tests for an event just before and just after a tick. Correctness cases asserting request counts
    and logs per request for the count and content boundaries (timing isn't compared in correctness runs, since wall
    clock comparisons would be flaky).
### Backpressure and fault tolerance

- [ ] `backpressure-retries` **Backpressure and retries: implementation (`gap/backpressure`).** Based on the
  `logs-send-architecture` characterization and decision, implement the Agent's semantics for logs (blocking until at
  least one reliable destination accepts a payload, retrying admitted payloads until success or shutdown, dropping
  copies for full or retrying destinations, and backpressure reaching the OTLP client), or record each remaining
  difference as an explicit, documented decision.
  - Tests: the `logs-send-architecture` characterization scenarios turned into regression tests against the chosen
    behavior.
- [ ] `antithesis-otlp-investigation` **Decide whether and how to use Antithesis for OTLP logs.** Antithesis
  currently covers only DogStatsD. Determine what it would add beyond the scripted-intake tests (for example network
  faults, intake outages, and restarts during backpressure), what's needed to extend the harness, simulated intake, and
  scenarios to OTLP logs, and whether that's worth doing. Details are deliberately left to this investigation.
  - Output: a recorded decision in this plan and, if it's worth doing, follow-up tasks in this phase.

### Processing rules

- [ ] `processing-rules-config` **Processing rules configuration (`gap/processing-rules`).** Move
  `logs_config.processing_rules` from `excluded` to the appropriate inventory classification (following the overlay
  convention of the configuration parity phase). Parse both structured YAML and the JSON-string form (environment
  variables), and validate like the Agent: non-empty names, types and patterns, compilable regexes, and `remap_source`
  matching entries. Accept all runtime-valid types, including `multi_line`, `exclude_truncated` and `remap_source`.
  - Invalid rules stop the Agent's logs agent from starting. With OTLP logs enabled, that likely also triggers the
    `otlp-signal-gating` validator failure (no OTLP receiver at all); verify this, then replicate it.
  - Tests: config translator tests for both forms, empty and default handling, every validation error, and an
    integration case for the invalid-rules startup behavior.
- [ ] `go-regex-compat` **Go-compatible regex engine (`gap/processing-rules`).** The Agent uses Go `regexp` (`Compile`,
  `ReplaceAll`). Rust's `regex` crate differs: for example, `\w`, `\d` and `\s` are Unicode-aware there but ASCII-only
  in Go. Pick an engine configuration that matches, or translate patterns.
  - Tests: Go-generated compatibility vectors for Unicode vs. ASCII classes and word boundaries, POSIX classes,
    rejected constructs such as look-around, anchors and newlines, capture substitutions (`${1}`, `$$`), empty
    replacements, and multiple matches.
- [ ] `processing-rules-engine` **Rule engine (`gap/processing-rules`).** Apply rules in order to the rendered content:
  masks mutate the bytes later rules see, every include must match, and the first failing rule drops the message.
  `exclude_truncated`, `remap_source` and `multi_line` behave as the Agent does for these messages (no-ops). Only the
  content changes; the envelope fields stay as they were. This needs a safe content-mutation API on `Log`.
  - Tests: a fixture for each rule type and for ordered combinations (mask then include, include then mask, multiple
    includes, exclude short-circuiting). Cover escaped strings, patterns spanning rendered properties, capture
    replacements, and a mask that makes the content invalid JSON. Masks that change embedded `status`, `service`,
    `hostname` or `datadog.log.source` must leave the envelope fields unchanged. Assert output counts and exact raw
    envelope `message` bytes.
- [ ] `processing-rules-topology` **Topology integration (`gap/processing-rules`).** Add a shared logs processing
  transform in front of `dd_logs_encode`, after the OTLP render step, and rewire both the OTLP and checks log paths
  through it.
  - Tests: separate OTLP and checks source-to-wire tests showing pass, mask, and drop through the shared transform,
    and an `otlp-logs-processing-rules` correctness case.

### TCP transport

- [ ] `tcp-transport-investigation` **Decide whether and how to support TCP transport (`gap/tcp-transport`).**
  Investigate the Agent's HTTP/TCP selection and TCP sending path, and determine whether ADP should implement it, how,
  and which parts. Details are deliberately left to this investigation.
  - Output: a recorded decision in this plan and, if TCP is to be supported, follow-up tasks in this phase.

### Operational parity and hardening

- [ ] `otlp-logs-telemetry` **Telemetry counters (`gap/telemetry-status`).** Depends on `otlp-exporter-batching`. Add
  `runtime.datadog_agent_otlp_logs_requests` and `runtime.datadog_agent_otlp_logs_events`, counted after the
  pre-translation batcher on raw input records (before translation and before any drop), and add the remapping rules so
  the Agent reports them.
  - Tests: exact deltas for multiple OTLP requests, batch merging and splitting, empty requests, and records dropped
    later. An integration assertion on the remapped metrics.
- [ ] `logs-status-section` **Logs status section (`gap/telemetry-status`).** Define a field-level status contract
  matching the Agent's Logs Agent section (running state, endpoints, the relevant metrics, errors and warnings), and
  register it with the remote agent runtime. OTLP contributes nothing to the Agent flare, so ADP adds no flare artifact.
  - Tests: unit tests on the status contents, and integration assertions on the Agent's status output.
- [ ] `smp-otlp-logs` **Performance.** Update `otlp_ingest_logs_5mb`: native mode with proxying disabled, both
  `logs_enabled` and `otlp_config.logs.enabled`, and a check that the logs counter is nonzero, so rejected traffic can't
  look like throughput. Add a gRPC variant. Stabilize both, then promote them to the quality gates by adding CPU,
  memory, and ingress bounds.
- [ ] `integration-smoke` **Integration smoke coverage.**
  - Extend the existing `otlp-tls` case to send a log with `otlp_send` (both enablement keys set) and find it in the
    intake. Use verified server certificates and the configured CA and client-certificate behavior.
  - Add gRPC-over-Unix-socket coverage, with `otlp_send` running inside the container. HTTP over Unix sockets isn't
    possible today: the source ignores `http.transport` and always listens on TCP. That's an OTLP receiver gap
    outside this plan.
- [ ] `parity-docs` **Docs**: document OTLP logs support and its remaining differences in `docs/agent-data-plane/`. Move
  lasting testing guidance into `docs/development/testing.md`, then delete this plan.

## Decisions

- **Bug-for-bug compatibility**: ADP replicates Agent quirks exactly, including the per-resource host and service
  carry-over (`gap/host-resolution`). Any intentional difference needs an explicit decision and documentation.
- **Proxy mode stays the default**: `data_plane.otlp.proxy.logs` keeps its current default until the correctness
  suite gives enough confidence to switch to native handling.
- **Proxy-mode gating happens in ADP**: in proxy mode, ADP applies the same OTLP gating as in native mode instead of
  relying on the core Agent's response. The relay replies before forwarding, so relying on the Agent would mean silent
  drops, or a synchronous relay that changes latency and backpressure for every signal. Upstream rejections are
  counted and logged per signal to catch drift from the Agent's rules.
- **`ddtags` formatting is an intentional difference (`gap/ddtags-formatting`)**: ADP doesn't replicate the Agent's raw
  `ddtags` string: its tag order, whitespace, duplicates, and empty elements. The intake parses `ddtags` into a set
  (split on `,`, trimmed, empty elements and duplicates dropped, order discarded), so the difference
  is semantically invisible, and replicating it would need a raw tag string next to the `TagSet`. It isn't byte-for-byte
  invisible: whitespace, duplicates, and empty elements change the serialized size, and so the attributed ingest size.
  Tag order doesn't change the size, and the other cases only arise from unusual input (for example a `ddtags` attribute
  with spaces or repeated tags). This billing effect is accepted. The Agent's order isn't reproducible anyway:
  `TagsFromAttributes` (`attributes/attributes.go:218-287`) emits the container, cloud, ECS, `datadog.container.tag.*`,
  and GCP serverless tags in randomized Go map order. The **normalized tag set** must still match exactly, and the
  analyzer and golden vectors compare `ddtags` as that set.
- **The fake intake's fallback stays lenient**: unhandled paths still get 200. Failing loudly on known Datadog intake
  paths isn't related to logs, and the Agent calls some of those endpoints at startup.
- **HTTP first, TCP later**: everything before the TCP transport phase covers only the HTTP logs transport.
  Correctness cases force HTTP on the Agent side (`logs_config.force_use_http: true`) until that phase decides
  otherwise.
- **Content rendering**: the logs encoder becomes generic, like the Agent's JSON encoder. It writes the
  envelope (`message`, `status`, `timestamp`, `hostname`, `service`, `ddsource`, `ddtags`) and copies the log's
  already-rendered content verbatim into `message`. Producers own the content:
  - OTLP logs stay structured through translation and enrichment. A shared render step at the end of the OTLP logs
    path serializes them into the Agent's `HTTPLogItem` content JSON, applying the Agent's collision rules (properties
    overwrite top-level fields, only `ddtags` removed from the content and moved to the envelope). `status` and
    `datadog.log.source` are copied to the envelope status and source but stay in the content.
  - Processing rules (the processing rules phase) run after rendering, on the content bytes, as in the Agent.
  - Check logs use their message as content, with no checks-specific logic in the encoder.
- **Check logs are out of scope**: they have their own parity gap, which starts in the Agent Check Runner. This plan
  changes only their envelope (the `{"message": ...}` wrapper is replaced by the raw message), which moves toward
  Agent behavior. Findings for that separate effort:
  - **Agent**: `AgentCheck.send_log` (`DataDog/integrations-core`, `datadog_checks_base/datadog_checks/base/checks/base.py`) sends
    `json.encode(attributes)` with `ddtags` defaulted from the instance tags and `timestamp` converted to ms. The Go
    integrations launcher (`pkg/logs/launchers/integration/launcher.go`) writes each line to a file and tails it, but
    only when the check config has a `logs:` entry of type `integration`; otherwise the log is dropped. Source,
    service, and tags come from that entry, and the generic encoder writes the line verbatim as `message`.
  - **Agent Check Runner**: its `datadog_agent.send_log(check_id, level, message)` binding
    doesn't match the `(data, check_id)` call made by `datadog_checks_base`, so real checks calling `send_log` should
    raise `TypeError` (this conclusion comes from reading the code; it hasn't been run). The runner also drops source,
    service, and hostname before IPC, and ADP's checks source rebuilds only the
    message and status.

## Open questions

- None at the moment.
