# Configuring DogStatsD on Agent Data Plane

<!-- Last updated: 2026-05-15 -->

The DogStatsD implementation on ADP has been redesigned in Rust for better resource guarantees and
efficiency. Because the architecture is different from the original implementation, certain
configuration values may behave differently, be planned but not yet implemented, or not apply at
all. This page documents those nuances.

ADP is designed to be transparent: customers configure DogStatsD the same way they always have. The
sections below call out the cases where that is either not yet true, or not quite possible.

If you find an error on this page, please [open an issue].

<!-- @formatter:off -->
[open an issue]: https://github.com/DataDog/saluki/issues
<!-- @formatter:on -->

## Unsupported Settings

<!-- section:unsupported-in-progress -->
### Being Worked On

The following settings are not yet supported in ADP but are planned with GitHub issue links for
tracking.

| Config Key                                       | Description                           | Issue   |
| ------------------------------------------------ | ------------------------------------- | ------- |
| `allow_arbitrary_tags`                           | Allow arbitrary tag values            | [#1377] |
| `cri_connection_timeout`                         | CRI runtime connection timeout        | [#1348] |
| `cri_query_timeout`                              | CRI runtime query timeout             | [#1348] |
| `dogstatsd_capture_depth`                        | Traffic capture channel depth         | [#1381] |
| `dogstatsd_capture_path`                         | Traffic capture file location         | [#1381] |
| `dogstatsd_pipe_name`                            | Windows named pipe path               | [#1466] |
| `dogstatsd_windows_pipe_security_descriptor`     | Windows named pipe ACL descriptor     | [#1466] |
| `forwarder_http_protocol`                        | HTTP version (auto/http1)             | [#1361] |
| `forwarder_outdated_file_in_days`                | Retry file retention (days)           | [#1360] |
| `log_format_rfc3339`                             | Use RFC3339 timestamp format          | [#1373] |
| `min_tls_version`                                | Minimum TLS version for HTTPS         | [#1370] |
| `serializer_experimental_use_v3_api.*`           | V3 metrics API migration flags        | [#1468] |
| `sslkeylogfile`                                  | TLS key log file path                 | [#1372] |
| `statsd_forward_host`                            | Host for packet forwarding            | [#1476] |
| `statsd_forward_port`                            | Port for packet forwarding            | [#1476] |
| `tls_handshake_timeout`                          | HTTP TLS handshake timeout            | [#178]  |

<!-- section:unsupported-not-planned -->
### Not Planned

The following settings exist in the core agent but are not planned for ADP, typically because ADP's
architecture is fundamentally different or the feature is platform-specific.

| Config Key                                     | Description                    | Reason                                                       |
| ---------------------------------------------- | ------------------------------ | ------------------------------------------------------------ |
| `dogstatsd_mem_based_rate_limiter.enabled`     | Enable memory rate limiter     | Go GC specific; use `memory_limit`                           |
| `dogstatsd_no_aggregation_pipeline_batch_size` | No-agg pipeline batch size     | Fixed in ADP topology                                        |
| `dogstatsd_packet_buffer_flush_timeout`        | Packet buffer flush timeout    | ADP decodes inline                                           |
| `dogstatsd_packet_buffer_size`                 | Datagrams per packet buffer    | ADP decodes inline                                           |
| `dogstatsd_pipeline_autoadjust`                | Auto-adjust pipeline workers   | ADP uses async tasks                                         |
| `dogstatsd_pipeline_count`                     | Parallel processing pipelines  | ADP uses async tasks                                         |
| `dogstatsd_queue_size`                         | Packet channel buffer size     | ADP uses async tasks                                         |
| `dogstatsd_telemetry_enabled_listener_id`      | Per-listener telemetry tagging | Not feasible to thread through                               |
| `dogstatsd_workers_count`                      | Num DSD processing workers     | ADP uses async tasks                                         |
| `use_dogstatsd`                                | Master DogStatsD enable toggle | Core Agent evaluates and sets `data_plane.dogstatsd.enabled` |

## Behavioral Differences

<!-- section:behavioral-differences -->

The following settings are recognized by both ADP and the core agent, but with different behavior or
default values.

| Config Key                          | Description                      | Agent Behavior                                 | ADP Behavior                                                   |
| ----------------------------------- | -------------------------------- | ---------------------------------------------- | -------------------------------------------------------------- |
| `dogstatsd_metrics_stats_enable`    | Enable per-metric debug stats    | Config toggle                                  | Gates debug log; stats API on-demand ([#1352], [#1356])        |
| `dogstatsd_stats_enable`            | Enable internal stats endpoint   | Config toggle                                  | On-demand via API ([#1352])                                    |
| `dogstatsd_stats_buffer`            | Internal stats buffer size       | Configurable                                   | On-demand via API ([#1352])                                    |
| `dogstatsd_stats_port`              | Internal stats endpoint port     | Configurable port                              | On-demand via API ([#1352])                                    |
| `log_level`                         | Log verbosity directives         | Controls Agent logs                            | Plain levels control ADP/Saluki-owned targets only             |
| `logging_frequency`                 | Transaction success log interval | Throttles success logs                         | Intentionally unused                                           |
| `serializer_zstd_compressor_level`  | Zstd compression level           | Default level 1                                | Default level 3 (intentional)                                  |
| `skip_ssl_validation`               | Skip TLS cert validation         | Disables validation for outbound HTTPS clients | Applies to the shared Datadog forwarder; rejected in FIPS mode |
| `telemetry.enabled`                 | Global telemetry toggle          | Agent toggle                                   | Use `data_plane.telemetry_enabled` ([#1338])                   |

### Datadog intake TLS validation (`skip_ssl_validation`)

ADP supports `skip_ssl_validation` for Datadog intake forwarding through the shared Datadog
forwarder. The default is `false`, which preserves normal server certificate validation. To accept
invalid server certificates for Datadog intake requests, set `skip_ssl_validation: true` or
`DD_SKIP_SSL_VALIDATION=true`.

When enabled, this setting affects the Datadog intake clients used by metrics, logs, traces, events,
and service checks that flow through the shared forwarder.

> [!WARNING]
> Setting `skip_ssl_validation: true` disables TLS server certificate validation for Datadog intake
> forwarding. Use it only when you understand and accept that risk.

This setting does not affect ADP IPC, local privileged APIs, ADP control-plane clients, OTLP
proxying to the core agent, or unrelated HTTP clients. In FIPS builds, ADP rejects
`skip_ssl_validation: true` because disabling TLS certificate validation is not FIPS-compliant.

### Logging verbosity (`log_level` / `logging_frequency`)

ADP accepts `log_level` as the startup logging control. A plain level applies to ADP-owned and
Saluki-owned targets only, including `agent_data_plane`, `saluki_*`, and runtime crates under
`lib/`.

```yaml
log_level: debug
```

This keeps third-party dependencies such as `hyper`, `tokio`, and `tonic` at their default filtering
unless you opt them in.

To control dependency logs or set a global fallback, use advanced `EnvFilter` directives in
`log_level`. ADP applies those directive strings as configured:

```yaml
log_level: warn,agent_data_plane=debug,hyper=warn
```

`logging_frequency` is intentionally unused by ADP. The core agent uses it to throttle repetitive
successful transaction logs. ADP logs successful forwarder operations below the default `info` level,
so there is no matching info-level success-log stream to throttle.

### DogStatsD statistics (`dogstatsd_stats_enable` / `dogstatsd_metrics_stats_enable`)

The core agent has two DogStatsD statistics mechanisms with different scopes.
`dogstatsd_stats_enable` enables packet-level throughput statistics from a ring buffer, exposed as
Go expvar data on `dogstatsd_stats_port` (default `5000`). Operators must configure an OpenMetrics
check to scrape that endpoint before the data is submitted. `dogstatsd_metrics_stats_enable`
enables runtime-toggleable metric-level debug statistics that track count and last-seen time per
unique metric and tag combination. That data powers the core agent's `dogstatsd-stats` CLI command
and HTTP endpoint.

ADP does not mirror the packet-level statistics config path. Instead, ADP provides an on-demand
metric-level view through a DogStatsD statistics destination that is always wired into the
topology, but only collects data during a time-bounded request. To collect statistics, run
`agent-data-plane dogstatsd stats --duration-secs N` or call the privileged
`/dogstatsd/stats?collection_duration_secs=N` API. The handler waits for the requested collection
window, then returns count and last-seen time per metric context inline as JSON. The CLI uses the
same API and renders the result as either summary or cardinality analysis.

ADP also exposes internal DogStatsD telemetry through its OpenMetrics endpoint when
`data_plane.telemetry_enabled` is enabled. Scrape `data_plane.telemetry_listen_addr` to collect
aggregate DogStatsD counters such as processed message counts, packet and byte counts, packet pool
usage, and channel latency. This telemetry endpoint is separate from `/dogstatsd/stats`: it does not
return the per-metric count and last-seen map, and it is not controlled by the core agent's
`dogstatsd_stats_*` keys.

ADP does not expose the core agent's packet-per-second expvar endpoint or a persistent per-metric
DogStatsD statistics endpoint to scrape. You do not need to set up scraper configuration for this
per-metric data. The config keys `dogstatsd_stats_enable`, `dogstatsd_stats_buffer`, and
`dogstatsd_stats_port` have no effect in ADP. See [#1352].

### DogStatsD metric debug log

ADP supports the core agent's DogStatsD metric debug log. To write this file, set
`dogstatsd_metrics_stats_enable: true`. `dogstatsd_logging_enabled` also must be `true`; it defaults
to `true`, so most configurations only need to enable `dogstatsd_metrics_stats_enable`.

When `dogstatsd_logging_enabled` is `true`, ADP connects an extra DogStatsD destination to the
decoded metric stream. The destination writes one line per metric sample with the metric name, tags,
count, and last-seen time while `dogstatsd_metrics_stats_enable` is `true`. When
`dogstatsd_metrics_stats_enable` is `false`, the destination drains decoded metrics and drops them.
This lets runtime configuration changes start and stop the debug log without rebuilding the
topology. This feature is for support and troubleshooting. It does not change normal metric
forwarding, and it does not replace the on-demand `/dogstatsd/stats` API.

Use these settings to control the file:

| Config Key                     | Behavior                                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------------------- |
| `dogstatsd_log_file`           | Output path. If empty, ADP uses the platform default DogStatsD stats log path.              |
| `dogstatsd_log_file_max_rolls` | Number of rotated files to keep. Defaults to `3`.                                           |
| `dogstatsd_log_file_max_size`  | Maximum active file size before rotation. Defaults to `10Mb`.                               |
| `dogstatsd_logging_enabled`    | Controls whether ADP wires the debug log destination into the topology. Defaults to `true`. |

The default `dogstatsd_log_file` path is
`/var/log/datadog/dogstatsd_info/dogstatsd-stats.log` on Linux and other Unix platforms,
`/opt/datadog-agent/logs/dogstatsd_info/dogstatsd-stats.log` on macOS, and
`%ProgramData%\datadog\logs\dogstatsd_info\dogstatsd-stats.log` on Windows.

This debug log differs from the `dogstatsd_capture_*` settings. The debug log records decoded metric
summaries after DogStatsD parsing. The capture settings record raw DogStatsD traffic for packet-level
investigation, and they remain tracked separately under [#1381].

### `telemetry.enabled`

The core agent enables internal Prometheus metrics via `telemetry.enabled` and ingests them with an
OpenMetrics check pointed at the agent's own telemetry endpoint. ADP has a separate telemetry
endpoint controlled by `data_plane.telemetry_enabled`. Customers enabling ADP telemetry must
configure a separate OpenMetrics check pointed at ADP's endpoint. See [#1338].

## Compatibility Unknown

<!-- section:compatibility-unknown -->

The following settings need further investigation. ADP behavior may differ from the core agent in
ways that are not yet fully characterized.

| Config Key                                         | Description                      | Issue   |
| -------------------------------------------------- | -------------------------------- | ------- |
| `aggregator_tag_filter_cache_capacity`             | Tag-filter dedup cache size      |         |
| `dogstatsd_disable_verbose_logs`                   | Suppress noisy parse error logs  | [#1350] |
| `forwarder_apikey_validation_interval`             | API key check interval (mins)    | [#1357] |
| `forwarder_flush_to_disk_mem_ratio`                | Mem-to-disk flush threshold      | [#1364] |
| `forwarder_high_prio_buffer_size`                  | High-priority request queue size | [#1362] |
| `forwarder_low_prio_buffer_size`                   | Low-priority request queue size  | [#1362] |
| `forwarder_max_concurrent_requests`                | Max concurrent HTTP requests     | [#1363] |
| `forwarder_retry_queue_capacity_time_interval_sec` | Retry queue time-based capacity  | [#1365] |
| `serializer_max_payload_size`                      | Max compressed payload size      | [#1354] |
| `serializer_max_series_payload_size`               | Max series compressed size       | [#1354] |
| `serializer_max_series_points_per_payload`         | Max series points per payload    | [#1354] |
| `serializer_max_series_uncompressed_payload_size`  | Max series uncompressed size     | [#1354] |
| `serializer_max_uncompressed_payload_size`         | Max uncompressed payload size    | [#1354] |

## ADP-Only Settings

<!-- section:adp-only -->

The following settings are specific to ADP and have no equivalent in the core agent.

| Config Key                                  | Description                                | Default |
| ------------------------------------------- | ------------------------------------------ | ------- |
| `agent_ipc_endpoint`                        | Remote agent IPC URI                       |         |
| `aggregate_flush_interval`                  | Aggregator flush period                    |         |
| `aggregate_flush_open_windows`              | Flush open windows on stop                 |         |
| `aggregate_passthrough_idle_flush_timeout`  | Passthrough buffer flush delay             |         |
| `aggregate_window_duration`                 | Aggregation window size                    |         |
| `connect_retry_attempts`                    | IPC client connect retries                 |         |
| `connect_retry_backoff`                     | IPC client retry delay                     |         |
| `counter_expiry_seconds`                    | Idle counter keep-alive duration           | 300s    |
| `data_plane.api_listen_address`             | ADP unprivileged API address               |         |
| `data_plane.remote_agent_enabled`           | Register as remote agent                   |         |
| `data_plane.secure_api_listen_address`      | ADP privileged API address                 |         |
| `data_plane.standalone_mode`                | ADP standalone mode toggle                 |         |
| `data_plane.use_new_config_stream_endpoint` | Use new config stream endpoint             |         |
| `dogstatsd_allow_context_heap_allocs`       | Allow heap allocs for contexts             |         |
| `dogstatsd_autoscale_udp_listeners`         | Bind multiple UDP sockets via SO_REUSEPORT |         |
| `dogstatsd_buffer_count`                    | Number of receive buffers                  |         |
| `dogstatsd_cached_contexts_limit`           | Max cached metric contexts                 |         |
| `dogstatsd_cached_tagsets_limit`            | Max cached tagsets                         |         |
| `dogstatsd_mapper_string_interner_size`     | Mapper string interner capacity            |         |
| `dogstatsd_minimum_sample_rate`             | Floor for metric sample rates              |         |
| `dogstatsd_permissive_decoding`             | Relaxes decoder strictness                 | true    |
| `dogstatsd_string_interner_size_bytes`      | Explicit byte budget for context interner  |         |
| `dogstatsd_tcp_port`                        | TCP listen port for DSD                    |         |
| `enable_global_limiter`                     | Toggle global memory limiter               |         |
| `flush_timeout_secs`                        | Encoder flush timeout (secs)               |         |
| `memory_limit`                              | Process memory limit (bytes)               |         |
| `memory_mode`                               | ADP global memory limiter mode             |         |
| `memory_slop_factor`                        | Memory headroom fraction                   |         |
| `metrics_level`                             | ADP internal metrics emission level        |         |
| `otlp_string_interner_size`                 | OTLP context interner capacity             |         |
| `remote_agent_string_interner_size_bytes`   | Tag string interner capacity               | 512 KB  |
| `serializer_max_metrics_per_payload`        | Max metrics per payload                    |         |
| `statsd_metric_namespace_blocklist`         | Renamed alias for blacklist key            |         |

### `memory_limit` / `memory_slop_factor`

ADP uses an explicit process memory limit (`memory_limit`) rather than relying on Go's garbage
collector. The `memory_slop_factor` reserves a fraction of the limit to account for allocations not
tracked by ADP's internal accounting. When memory usage approaches `memory_limit`, ADP's global
limiter begins exerting backpressure (see `enable_global_limiter`).

### `dogstatsd_minimum_sample_rate`

ADP enforces a minimum sample rate on incoming metrics to prevent memory exhaustion from extremely
low sample rates on histograms and sketches. Sending metrics with a very high inverse sample rate
(e.g. `@0.0000001`) can cause unbounded memory growth in a sketch; this setting prevents that. The
default is conservative enough that normal clients are unaffected.

### `dogstatsd_permissive_decoding`

By default, ADP parses DogStatsD packets with the same leniency as the core agent, accepting packets
that technically violate the spec. Setting this to `false` enables strict mode, which rejects
non-conformant packets. Strict mode is not available in the core agent.

### `data_plane.remote_agent_enabled` / `data_plane.use_new_config_stream_endpoint`

These two keys are transitional flags being phased out. Both will be implied by
`data_plane.standalone_mode=false` in a future release. Do not rely on them for new deployments.

## Transparent Settings

<!-- section:reference -->

The following settings work in ADP with the same behavior as the core agent.

To enable syslog logging, set `log_to_syslog: true`. Console logging remains controlled by
`log_to_console`; enabling syslog does not disable console or file logging. If `syslog_uri` is empty
while syslog logging is enabled, ADP uses the platform default local syslog socket:
`unixgram:///dev/log` on Linux and `unixgram:///var/run/syslog` on macOS. Set `syslog_rfc: true`
when the receiving syslog daemon expects the Agent's RFC-style header.

| Config Key                                       | Description                           |
| ------------------------------------------------ | ------------------------------------- |
| `additional_endpoints`                           | Dual-ship to extra endpoints          |
| `aggregate_context_limit`                        | Max contexts per agg window           |
| `api_key`                                        | API key for endpoint auth             |
| `auth_token_file_path`                           | IPC auth token file path              |
| `bind_host`                                      | Global listen host fallback           |
| `cmd_port`                                       | Agent IPC/CMD API port                |
| `container_cgroup_root`                          | Cgroup filesystem root path           |
| `container_proc_root`                            | Procfs root path for containers       |
| `cri_socket_path`                                | CRI/containerd socket path            |
| `data_plane.dogstatsd.enabled`                   | Enable DSD in data plane              |
| `data_plane.enabled`                             | Enable ADP globally                   |
| `dd_url`                                         | Override intake endpoint URL          |
| `dogstatsd_buffer_size`                          | Receive buffer size (bytes)           |
| `dogstatsd_context_expiry_seconds`               | Context cache TTL (seconds)           |
| `dogstatsd_entity_id_precedence`                 | Entity ID over auto-detection         |
| `dogstatsd_eol_required`                         | Require newline-terminated messages   |
| `dogstatsd_expiry_seconds`                       | Counter zero-value TTL (secs)         |
| `dogstatsd_flush_incomplete_buckets`             | Flush open buckets on shutdown        |
| `dogstatsd_log_file`                             | DSD metric debug log path             |
| `dogstatsd_log_file_max_rolls`                   | Max rotated DSD debug log files       |
| `dogstatsd_log_file_max_size`                    | Max DSD debug log file size           |
| `dogstatsd_logging_enabled`                      | Enable DSD metric debug logging       |
| `dogstatsd_mapper_profiles`                      | Metric mapping profile defs           |
| `dogstatsd_no_aggregation_pipeline`              | Enable no-agg timestamped path        |
| `dogstatsd_non_local_traffic`                    | Accept non-localhost UDP/TCP          |
| `dogstatsd_origin_detection`                     | Enable UDS origin detection           |
| `dogstatsd_origin_detection_client`              | Honor client origin proto fields      |
| `dogstatsd_origin_optout_enabled`                | Allow clients to opt out origin       |
| `dogstatsd_port`                                 | UDP listen port                       |
| `dogstatsd_so_rcvbuf`                            | Socket receive buffer size            |
| `dogstatsd_socket`                               | UDS datagram socket path              |
| `dogstatsd_stream_log_too_big`                   | Log oversized UDS stream frames       |
| `dogstatsd_stream_socket`                        | UDS stream socket path                |
| `dogstatsd_string_interner_size`                 | String interner capacity              |
| `dogstatsd_tag_cardinality`                      | Default tag cardinality level         |
| `dogstatsd_tags`                                 | Extra tags added to all DSD data      |
| `enable_payloads.events`                         | Allow sending event payloads          |
| `enable_payloads.series`                         | Allow sending series payloads         |
| `enable_payloads.service_checks`                 | Allow sending svc check payloads      |
| `enable_payloads.sketches`                       | Allow sending sketch payloads         |
| `expected_tags_duration`                         | Host tag enrichment duration          |
| `extra_tags`                                     | Additional static tags                |
| `forwarder_backoff_base`                         | Retry backoff base (secs)             |
| `forwarder_backoff_factor`                       | Retry backoff jitter factor           |
| `forwarder_backoff_max`                          | Retry backoff ceiling (secs)          |
| `forwarder_connection_reset_interval`            | HTTP conn reset interval (secs)       |
| `forwarder_num_workers`                          | Concurrent forwarder workers          |
| `forwarder_recovery_interval`                    | Backoff recovery decrease factor      |
| `forwarder_recovery_reset`                       | Reset errors on success               |
| `forwarder_retry_queue_max_size`                 | Retry queue max size (depr.)          |
| `forwarder_retry_queue_payloads_max_size`        | Retry queue max size (bytes)          |
| `forwarder_storage_max_disk_ratio`               | Max disk usage ratio for retry        |
| `forwarder_storage_max_size_in_bytes`            | Max on-disk retry storage size        |
| `forwarder_storage_path`                         | On-disk retry storage directory       |
| `forwarder_timeout`                              | Forwarder HTTP request timeout        |
| `histogram_aggregates`                           | Histogram aggregate statistics        |
| `histogram_copy_to_distribution`                 | Copy histograms to distributions      |
| `histogram_copy_to_distribution_prefix`          | Prefix for hist-to-dist copies        |
| `histogram_percentiles`                          | Histogram percentile quantiles        |
| `hostname`                                       | Configured hostname override          |
| `ipc_cert_file_path`                             | IPC TLS certificate path              |
| `log_file`                                       | Log output file path                  |
| `log_file_max_rolls`                             | Max rotated log files kept            |
| `log_file_max_size`                              | Max log file size before rotate       |
| `log_format_json`                                | Use JSON log format                   |
| `log_to_console`                                 | Log to stdout/stderr                  |
| `log_to_syslog`                                  | Log to syslog daemon                  |
| `metric_filterlist`                              | Metric name blocklist                 |
| `metric_filterlist_match_prefix`                 | Blocklist uses prefix matching        |
| `metric_tag_filterlist`                          | Per-metric tag include/exclude        |
| `no_proxy_nonexact_match`                        | Domain/CIDR no_proxy matching         |
| `observability_pipelines_worker.metrics.enabled` | Route metrics to OPW instance         |
| `observability_pipelines_worker.metrics.url`     | OPW metrics intake URL                |
| `origin_detection_unified`                       | Unified origin detection mode         |
| `provider_kind`                                  | Provider kind static tag              |
| `proxy`                                          | HTTP/HTTPS proxy configuration        |
| `run_path`                                       | Runtime data directory path           |
| `secret_backend_command`                         | Secret resolver executable path       |
| `secret_backend_timeout`                         | Secret backend timeout (seconds)      |
| `serializer_compressor_kind`                     | Payload compression algorithm         |
| `site`                                           | Datadog site domain                   |
| `statsd_metric_blocklist`                        | Metric name blocklist                 |
| `statsd_metric_blocklist_match_prefix`           | Blocklist uses prefix matching        |
| `statsd_metric_namespace`                        | Prefix prepended to all metrics       |
| `statsd_metric_namespace_blacklist`              | Namespace prefixes exempt (alias)     |
| `syslog_rfc`                                     | Use RFC-style syslog header           |
| `syslog_uri`                                     | Syslog destination URI                |
| `tags`                                           | Global tags (DD_TAGS)                 |
| `use_proxy_for_cloud_metadata`                   | Proxy cloud metadata endpoints        |
| `use_v2_api.series`                              | Send series via V2 protobuf endpoint  |
| `vector.metrics.enabled`                         | Route metrics to OPW (legacy alias)   |
| `vector.metrics.url`                             | OPW metrics intake URL (legacy alias) |

[#178]: https://github.com/DataDog/saluki/issues/178
[#1330]: https://github.com/DataDog/saluki/issues/1330
[#1331]: https://github.com/DataDog/saluki/issues/1331
[#1332]: https://github.com/DataDog/saluki/issues/1332
[#1333]: https://github.com/DataDog/saluki/issues/1333
[#1334]: https://github.com/DataDog/saluki/issues/1334
[#1338]: https://github.com/DataDog/saluki/issues/1338
[#1339]: https://github.com/DataDog/saluki/issues/1339
[#1342]: https://github.com/DataDog/saluki/issues/1342
[#1348]: https://github.com/DataDog/saluki/issues/1348
[#1350]: https://github.com/DataDog/saluki/issues/1350
[#1352]: https://github.com/DataDog/saluki/issues/1352
[#1353]: https://github.com/DataDog/saluki/issues/1353
[#1354]: https://github.com/DataDog/saluki/issues/1354
[#1356]: https://github.com/DataDog/saluki/issues/1356
[#1357]: https://github.com/DataDog/saluki/issues/1357
[#1360]: https://github.com/DataDog/saluki/issues/1360
[#1361]: https://github.com/DataDog/saluki/issues/1361
[#1362]: https://github.com/DataDog/saluki/issues/1362
[#1363]: https://github.com/DataDog/saluki/issues/1363
[#1364]: https://github.com/DataDog/saluki/issues/1364
[#1365]: https://github.com/DataDog/saluki/issues/1365
[#1366]: https://github.com/DataDog/saluki/issues/1366
[#1367]: https://github.com/DataDog/saluki/issues/1367
[#1368]: https://github.com/DataDog/saluki/issues/1368
[#1370]: https://github.com/DataDog/saluki/issues/1370
[#1371]: https://github.com/DataDog/saluki/issues/1371
[#1372]: https://github.com/DataDog/saluki/issues/1372
[#1373]: https://github.com/DataDog/saluki/issues/1373
[#1377]: https://github.com/DataDog/saluki/issues/1377
[#1380]: https://github.com/DataDog/saluki/issues/1380
[#1381]: https://github.com/DataDog/saluki/issues/1381
[#1382]: https://github.com/DataDog/saluki/issues/1382
[#1433]: https://github.com/DataDog/saluki/issues/1433
[#1434]: https://github.com/DataDog/saluki/issues/1434
[#1466]: https://github.com/DataDog/saluki/issues/1466
[#1468]: https://github.com/DataDog/saluki/issues/1468
[#1476]: https://github.com/DataDog/saluki/issues/1476
[#1640]: https://github.com/DataDog/saluki/issues/1640
[#1667]: https://github.com/DataDog/saluki/issues/1667
