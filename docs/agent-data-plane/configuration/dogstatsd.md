# Configuring DogStatsD on Agent Data Plane

<!-- Last updated: 2026-04-23 -->

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

| Config Key                                   | Description                       | Issue   |
|----------------------------------------------|-----------------------------------|---------|
| `allow_arbitrary_tags`                       | Allow arbitrary tag values        | [#1377] |
| `cri_connection_timeout`                     | CRI runtime connection timeout    | [#1348] |
| `cri_query_timeout`                          | CRI runtime query timeout         | [#1348] |
| `dogstatsd_capture_depth`                    | Traffic capture channel depth     | [#1381] |
| `dogstatsd_capture_path`                     | Traffic capture file location     | [#1381] |
| `dogstatsd_eol_required`                     | Require newline-terminated msgs   | [#1339] |
| `dogstatsd_log_file`                         | DSD dedicated log file path       | [#1356] |
| `dogstatsd_log_file_max_rolls`               | DSD log file max roll count       | [#1356] |
| `dogstatsd_log_file_max_size`                | DSD log file max size             | [#1356] |
| `dogstatsd_logging_enabled`                  | Enables DSD metric logging        | [#1356] |
| `dogstatsd_pipe_name`                        | Windows named pipe path           | [#1466] |
| `dogstatsd_so_rcvbuf`                        | Socket receive buffer size        | [#1341] |
| `dogstatsd_windows_pipe_security_descriptor` | Windows named pipe ACL descriptor | [#1466] |
| `dogstatsd_stream_log_too_big`               | Log oversized stream messages     | [#1342] |
| `extra_tags`                                 | Additional static tags            | [#1332] |
| `forwarder_http_protocol`                    | HTTP version (auto/http1)         | [#1361] |
| `forwarder_outdated_file_in_days`            | Retry file retention (days)       | [#1360] |
| `log_format_rfc3339`                         | Use RFC3339 timestamp format      | [#1373] |
| `log_to_syslog`                              | Log to syslog daemon              | [#1337] |
| `logging_frequency`                          | Transaction success log interval  | [#1380] |
| `min_tls_version`                            | Minimum TLS version for HTTPS     | [#1370] |
| `serializer_experimental_use_v3_api.*`       | V3 metrics API migration flags    | [#1468] |
| `sslkeylogfile`                              | TLS key log file path             | [#1372] |
| `tls_handshake_timeout`                      | HTTP TLS handshake timeout        | [#178]  |

<!-- section:unsupported-not-planned -->
### Not Planned

The following settings exist in the core agent but are not planned for ADP, typically because ADP's
architecture is fundamentally different or the feature is platform-specific.

| Config Key                                     | Description                    | Reason                                                       |
|------------------------------------------------|--------------------------------|--------------------------------------------------------------|
| `dogstatsd_mem_based_rate_limiter.enabled`     | Enable memory rate limiter     | Go GC specific; use `memory_limit`                           |
| `dogstatsd_no_aggregation_pipeline_batch_size` | No-agg pipeline batch size     | Fixed in ADP topology                                        |
| `dogstatsd_packet_buffer_flush_timeout`        | Packet buffer flush timeout    | ADP decodes inline                                           |
| `dogstatsd_packet_buffer_size`                 | Datagrams per packet buffer    | ADP decodes inline                                           |
| `dogstatsd_pipeline_autoadjust`                | Auto-adjust pipeline workers   | ADP uses async tasks                                         |
| `dogstatsd_pipeline_count`                     | Parallel processing pipelines  | ADP uses async tasks                                         |
| `dogstatsd_queue_size`                         | Packet channel buffer size     | ADP uses async tasks                                         |
| `dogstatsd_telemetry_enabled_listener_id`      | Per-listener telemetry tagging | Not feasible to thread through                               |
| `dogstatsd_workers_count`                      | Num DSD processing workers     | ADP uses async tasks                                         |
| `statsd_forward_host`                          | Host for raw packet forwarding | Not planned                                                  |
| `statsd_forward_port`                          | Port for raw packet forwarding | Not planned                                                  |
| `use_dogstatsd`                                | Master DogStatsD enable toggle | Core Agent evaluates and sets `data_plane.dogstatsd.enabled` |

## Behavioral Differences

<!-- section:behavioral-differences -->

The following settings are recognized by both ADP and the core agent, but with different behavior or
default values.

| Config Key                           | Description                      | Agent Behavior            | ADP Behavior                                    |
|--------------------------------------|----------------------------------|---------------------------|-------------------------------------------------|
| `dogstatsd_context_expiry_seconds`   | Context cache TTL (seconds)      | Default 20s, configurable | Hardcodes 30s ([#1340])                         |
| `dogstatsd_flush_incomplete_buckets` | Flush open buckets on shutdown   | Standard key              | Key is `aggregate_flush_open_windows` ([#1366]) |
| `dogstatsd_metrics_stats_enable`     | Enable per-metric debug stats    | Config toggle             | On-demand via API ([#1352])                     |
| `dogstatsd_stats_enable`             | Enable internal stats endpoint   | Config toggle             | On-demand via API ([#1352])                     |
| `dogstatsd_stats_buffer`             | Internal stats buffer size       | Configurable              | On-demand via API ([#1352])                     |
| `dogstatsd_stats_port`               | Internal stats endpoint port     | Configurable port         | On-demand via API ([#1352])                     |
| `enable_payloads.events`             | Allow sending event payloads     | Dot separator             | Underscore: `enable_payloads_events` ([#1366])  |
| `enable_payloads.series`             | Allow sending series payloads    | Dot separator             | Underscore: `enable_payloads_series` ([#1366])  |
| `enable_payloads.service_checks`     | Allow sending svc check payloads | Dot separator             | Underscore ([#1366])                            |
| `enable_payloads.sketches`           | Allow sending sketch payloads    | Dot separator             | Underscore ([#1366])                            |
| `serializer_zstd_compressor_level`   | Zstd compression level           | Default level 1           | Default level 3 (intentional)                   |
| `statsd_metric_namespace_blacklist`  | Prefixes exempt from namespace   | `_blacklist` key          | Use `_blocklist` key ([#1353])                  |
| `telemetry.enabled`                  | Global telemetry toggle          | Agent toggle              | Use `data_plane.telemetry_enabled` ([#1338])    |

### DogStatsD Statistics (`dogstatsd_stats_enable` / `dogstatsd_metrics_stats_enable`)

The core agent's DogStatsD stats feature collects per-metric statistics (name cardinality, tag
counts, etc.). You enable it via `dogstatsd_stats_enable`, wait a sampling period, then query the
`dogstatsd_stats_port` endpoint. `dogstatsd_metrics_stats_enable` is a related toggle for an older
stats path.

ADP exposes equivalent capability through its privileged API on demand — no config change is
required. You trigger collection directly via the ADP binary and retrieve results via the privileged
endpoint. The config keys `dogstatsd_stats_enable`, `dogstatsd_stats_buffer`,
`dogstatsd_stats_port`, and `dogstatsd_metrics_stats_enable` are not read by ADP. See [#1352].

### `dogstatsd_flush_incomplete_buckets`

ADP implements this behavior under the key `aggregate_flush_open_windows`. If you set
`dogstatsd_flush_incomplete_buckets`, ADP silently ignores it. Use `aggregate_flush_open_windows`
instead. Tracked in [#1366].

### `enable_payloads.*`

The core agent uses dot-separated keys (`enable_payloads.events`, `enable_payloads.series`, etc.)
while ADP currently reads underscore-separated variants (`enable_payloads_events`, etc.). The
underlying feature is implemented in ADP; only the config key naming differs. Setting the documented
core agent keys will have no effect on ADP until this is resolved. Tracked in [#1366].

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
|----------------------------------------------------|----------------------------------|---------|
| `dogstatsd_disable_verbose_logs`                   | Suppress noisy parse error logs  |         |
| `dogstatsd_origin_detection_client`                | Honor client origin proto fields | [#1426] |
| `forwarder_apikey_validation_interval`             | API key check interval (mins)    |         |
| `forwarder_flush_to_disk_mem_ratio`                | Mem-to-disk flush threshold      |         |
| `forwarder_high_prio_buffer_size`                  | High-priority request queue size |         |
| `forwarder_low_prio_buffer_size`                   | Low-priority request queue size  |         |
| `forwarder_max_concurrent_requests`                | Max concurrent HTTP requests     |         |
| `forwarder_retry_queue_capacity_time_interval_sec` | Retry queue time-based capacity  |         |
| `serializer_max_payload_size`                      | Max compressed payload size      |         |
| `serializer_max_series_payload_size`               | Max series compressed size       |         |
| `serializer_max_series_points_per_payload`         | Max series points per payload    |         |
| `serializer_max_series_uncompressed_payload_size`  | Max series uncompressed size     |         |
| `serializer_max_uncompressed_payload_size`         | Max uncompressed payload size    |         |
| `skip_ssl_validation`                              | Skip TLS cert validation         |         |
| `statsd_metric_blocklist`                          | Metric name blocklist            | [#1433] |
| `statsd_metric_blocklist_match_prefix`             | Blocklist matches by prefix      | [#1434] |
| `statsd_metric_namespace`                          | Prefix prepended to all metrics  |         |
| `tags`                                             | Global tags (DD_TAGS)            |         |

## ADP-Only Settings

<!-- section:adp-only -->

The following settings are specific to ADP and have no equivalent in the core agent.

| Config Key                                  | Description                      | Default |
|---------------------------------------------|----------------------------------|---------|
| `agent_ipc_endpoint`                        | Remote agent IPC URI             |         |
| `aggregate_flush_interval`                  | Aggregator flush period          |         |
| `aggregate_flush_open_windows`              | Flush open windows on stop       |         |
| `aggregate_passthrough_idle_flush_timeout`  | Passthrough buffer flush delay   |         |
| `aggregate_window_duration`                 | Aggregation window size          |         |
| `connect_retry_attempts`                    | IPC client connect retries       |         |
| `connect_retry_backoff`                     | IPC client retry delay           |         |
| `counter_expiry_seconds`                    | Idle counter keep-alive duration | 300s    |
| `data_plane.api_listen_address`             | ADP unprivileged API address     |         |
| `data_plane.remote_agent_enabled`           | Register as remote agent         |         |
| `data_plane.secure_api_listen_address`      | ADP privileged API address       |         |
| `data_plane.standalone_mode`                | ADP standalone mode toggle       |         |
| `data_plane.use_new_config_stream_endpoint` | Use new config stream endpoint   |         |
| `dogstatsd_allow_context_heap_allocs`       | Allow heap allocs for contexts   |         |
| `dogstatsd_buffer_count`                    | Number of receive buffers        |         |
| `dogstatsd_cached_contexts_limit`           | Max cached metric contexts       |         |
| `dogstatsd_cached_tagsets_limit`            | Max cached tagsets               |         |
| `dogstatsd_mapper_string_interner_size`     | Mapper string interner capacity  |         |
| `dogstatsd_minimum_sample_rate`             | Floor for metric sample rates    |         |
| `dogstatsd_permissive_decoding`             | Relaxes decoder strictness       | true    |
| `dogstatsd_tcp_port`                        | TCP listen port for DSD          |         |
| `enable_global_limiter`                     | Toggle global memory limiter     |         |
| `flush_timeout_secs`                        | Encoder flush timeout (secs)     |         |
| `memory_limit`                              | Process memory limit (bytes)     |         |
| `memory_slop_factor`                        | Memory headroom fraction         |         |
| `otlp_string_interner_size`                 | OTLP context interner capacity   |         |
| `remote_agent_string_interner_size_bytes`   | Tag string interner capacity     | 512 KB  |
| `serializer_max_metrics_per_payload`        | Max metrics per payload          |         |
| `statsd_metric_namespace_blocklist`         | Renamed alias for blacklist key  |         |

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

| Config Key                                | Description                      |
|-------------------------------------------|----------------------------------|
| `additional_endpoints`                    | Dual-ship to extra endpoints     |
| `aggregate_context_limit`                 | Max contexts per agg window      |
| `api_key`                                 | API key for endpoint auth        |
| `auth_token_file_path`                    | IPC auth token file path         |
| `bind_host`                               | Global listen host fallback      |
| `container_cgroup_root`                   | Cgroup filesystem root path      |
| `container_proc_root`                     | Procfs root path for containers  |
| `cri_socket_path`                         | CRI/containerd socket path       |
| `data_plane.dogstatsd.enabled`            | Enable DSD in data plane         |
| `data_plane.enabled`                      | Enable ADP globally              |
| `dd_url`                                  | Override intake endpoint URL     |
| `dogstatsd_buffer_size`                   | Receive buffer size (bytes)      |
| `dogstatsd_entity_id_precedence`          | Entity ID over auto-detection    |
| `dogstatsd_expiry_seconds`                | Counter zero-value TTL (secs)    |
| `dogstatsd_mapper_profiles`               | Metric mapping profile defs      |
| `dogstatsd_no_aggregation_pipeline`       | Enable no-agg timestamped path   |
| `dogstatsd_non_local_traffic`             | Accept non-localhost UDP/TCP     |
| `dogstatsd_origin_detection`              | Enable UDS origin detection      |
| `dogstatsd_origin_optout_enabled`         | Allow clients to opt out origin  |
| `dogstatsd_port`                          | UDP listen port                  |
| `dogstatsd_socket`                        | UDS datagram socket path         |
| `dogstatsd_stream_socket`                 | UDS stream socket path           |
| `dogstatsd_string_interner_size`          | String interner capacity         |
| `dogstatsd_tag_cardinality`               | Default tag cardinality level    |
| `dogstatsd_tags`                          | Extra tags added to all DSD data |
| `expected_tags_duration`                  | Host tag enrichment duration     |
| `forwarder_backoff_base`                  | Retry backoff base (secs)        |
| `forwarder_backoff_factor`                | Retry backoff jitter factor      |
| `forwarder_backoff_max`                   | Retry backoff ceiling (secs)     |
| `forwarder_connection_reset_interval`     | HTTP conn reset interval (secs)  |
| `forwarder_num_workers`                   | Concurrent forwarder workers     |
| `forwarder_recovery_interval`             | Backoff recovery decrease factor |
| `forwarder_recovery_reset`                | Reset errors on success          |
| `forwarder_retry_queue_max_size`          | Retry queue max size (depr.)     |
| `forwarder_retry_queue_payloads_max_size` | Retry queue max size (bytes)     |
| `forwarder_storage_max_disk_ratio`        | Max disk usage ratio for retry   |
| `forwarder_storage_max_size_in_bytes`     | Max on-disk retry storage size   |
| `forwarder_storage_path`                  | On-disk retry storage directory  |
| `forwarder_timeout`                       | Forwarder HTTP request timeout   |
| `histogram_aggregates`                    | Histogram aggregate statistics   |
| `histogram_copy_to_distribution`          | Copy histograms to distributions |
| `histogram_copy_to_distribution_prefix`   | Prefix for hist-to-dist copies   |
| `histogram_percentiles`                   | Histogram percentile quantiles   |
| `hostname`                                | Configured hostname override     |
| `ipc_cert_file_path`                      | IPC TLS certificate path         |
| `log_file`                                | Log output file path             |
| `log_file_max_rolls`                      | Max rotated log files kept       |
| `log_file_max_size`                       | Max log file size before rotate  |
| `log_format_json`                         | Use JSON log format              |
| `log_level`                               | Log verbosity level              |
| `log_to_console`                          | Log to stdout/stderr             |
| `metric_filterlist`                       | Metric name blocklist            |
| `metric_filterlist_match_prefix`          | Blocklist uses prefix matching   |
| `metric_tag_filterlist`                   | Per-metric tag include/exclude   |
| `no_proxy_nonexact_match`                 | Domain/CIDR no_proxy matching    |
| `origin_detection_unified`                | Unified origin detection mode    |
| `proxy`                                   | HTTP/HTTPS proxy configuration   |
| `run_path`                                | Runtime data directory path      |
| `secret_backend_command`                  | Secret resolver executable path  |
| `secret_backend_timeout`                  | Secret backend timeout (seconds) |
| `serializer_compressor_kind`              | Payload compression algorithm    |
| `site`                                    | Datadog site domain              |
| `use_proxy_for_cloud_metadata`            | Proxy cloud metadata endpoints   |

[#178]: https://github.com/DataDog/saluki/issues/178
[#1330]: https://github.com/DataDog/saluki/issues/1330
[#1331]: https://github.com/DataDog/saluki/issues/1331
[#1332]: https://github.com/DataDog/saluki/issues/1332
[#1333]: https://github.com/DataDog/saluki/issues/1333
[#1334]: https://github.com/DataDog/saluki/issues/1334
[#1337]: https://github.com/DataDog/saluki/issues/1337
[#1338]: https://github.com/DataDog/saluki/issues/1338
[#1339]: https://github.com/DataDog/saluki/issues/1339
[#1340]: https://github.com/DataDog/saluki/issues/1340
[#1341]: https://github.com/DataDog/saluki/issues/1341
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
[#1426]: https://github.com/DataDog/saluki/issues/1426
[#1380]: https://github.com/DataDog/saluki/issues/1380
[#1381]: https://github.com/DataDog/saluki/issues/1381
[#1382]: https://github.com/DataDog/saluki/issues/1382
[#1433]: https://github.com/DataDog/saluki/issues/1433
[#1434]: https://github.com/DataDog/saluki/issues/1434
[#1466]: https://github.com/DataDog/saluki/issues/1466
[#1468]: https://github.com/DataDog/saluki/issues/1468
