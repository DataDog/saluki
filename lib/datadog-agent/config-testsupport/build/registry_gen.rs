//! Generates the config registry annotation files from [`SchemaOverlay`] and [`SALUKI_KEYS`].
//!
//! ## Output
//!
//! Each run produces:
//!
//! - **Per-subsystem files** (`dogstatsd.rs`, `forwarder.rs`, etc.)—one `declare_annotations!`
//!   block per file, covering supported keys grouped by `config_registry_filename`.
//! - **`unsupported.rs`**—all unsupported overlay keys plus any `investigate` entries that carry
//!   a severity, each mapped to `SupportLevel::Incompatible`.
//! - **`schema.rs`**—flat `SchemaEntry` constants for every key (delegated to [`schema_gen`]).
//! - **An aggregation layer**—lazy statics `SUPPORTED_ANNOTATIONS`, `UNSUPPORTED_ANNOTATIONS`,
//!   and `ALL_ANNOTATIONS` tying all subsystem slices together.
//!
//! ## Two entry points
//!
//! - [`generate`] writes everything into `OUT_DIR/config_registry/` for `include!()`. Production
//!   build path.
//! - [`generate_in_tree`] writes directly to the source tree so generated files appear in PR
//!   diffs. Each file gets a `use super::*` preamble so it compiles as a plain module.
//!   Produces `annotations_index.rs` instead of `mod.rs`; the hand-written `mod.rs`
//!   `include!()`'s it.
//!
//! ## Ordering
//!
//! `GOLDEN_ORDER` preserves the key sequence from the hand-written registry files on `main`,
//! keeping diffs readable. Keys absent from the list sort to the end alphabetically.

use std::fmt::Write as _;
use std::path::Path;

use datadog_agent_config_overlay_model::saluki_keys::{SalukiKey, SALUKI_KEYS};
use datadog_agent_config_overlay_model::schema_gen::{self, FieldInfo};
use datadog_agent_config_overlay_model::{
    Pipeline, PipelineAffinity, SchemaOverlay, Severity, SupportLevel, ValueType,
};
use indexmap::IndexMap;

static SALUKI_ENTRIES: &[SalukiKey] = SALUKI_KEYS;
// Golden ordering: yaml_paths in the order they appeared in the hand-written config_registry
// files on main. Used to sort generated output so the diff is readable.
// Keys not listed here (new keys) sort to the end alphabetically.
static GOLDEN_ORDER: &[(&str, &[&str])] = &[
    (
        "aggregate.rs",
        &[
            "aggregate_window_duration_seconds",
            "aggregate_flush_interval",
            "aggregate_context_limit",
            "dogstatsd_flush_incomplete_buckets",
            "counter_expiry_seconds",
            "dogstatsd_no_aggregation_pipeline",
            "aggregate_passthrough_idle_flush_timeout",
            "histogram_aggregates",
            "histogram_copy_to_distribution",
            "histogram_copy_to_distribution_prefix",
        ],
    ),
    ("containerd.rs", &["cri_connection_timeout", "cri_query_timeout"]),
    (
        "dogstatsd_mapper.rs",
        &[
            "dogstatsd_mapper_cache_size",
            "dogstatsd_mapper_profiles",
            "dogstatsd_mapper_string_interner_size",
        ],
    ),
    (
        "dogstatsd_prefix_filter.rs",
        &[
            "metric_filterlist",
            "metric_filterlist_match_prefix",
            "statsd_metric_blocklist",
            "statsd_metric_blocklist_match_prefix",
            "statsd_metric_namespace",
            "statsd_metric_namespace_blacklist",
        ],
    ),
    (
        "dogstatsd.rs",
        &[
            "bind_host",
            "dogstatsd_buffer_size",
            "dogstatsd_capture_depth",
            "dogstatsd_capture_path",
            "dogstatsd_context_expiry_seconds",
            "dogstatsd_entity_id_precedence",
            "dogstatsd_eol_required",
            "dogstatsd_non_local_traffic",
            "dogstatsd_origin_detection",
            "dogstatsd_origin_detection_client",
            "dogstatsd_origin_optout_enabled",
            "dogstatsd_port",
            "dogstatsd_socket",
            "dogstatsd_so_rcvbuf",
            "dogstatsd_stream_socket",
            "dogstatsd_stream_log_too_big",
            "statsd_forward_host",
            "statsd_forward_port",
            "dogstatsd_string_interner_size",
            "dogstatsd_tag_cardinality",
            "dogstatsd_tags",
            "provider_kind",
            "origin_detection_unified",
            "dogstatsd_log_file",
            "dogstatsd_log_file_max_rolls",
            "dogstatsd_log_file_max_size",
            "dogstatsd_logging_enabled",
            "dogstatsd_metrics_stats_enable",
            "dogstatsd_allow_context_heap_allocs",
            "dogstatsd_autoscale_udp_listeners",
            "dogstatsd_buffer_count",
            "dogstatsd_cached_contexts_limit",
            "dogstatsd_cached_tagsets_limit",
            "dogstatsd_minimum_sample_rate",
            "dogstatsd_permissive_decoding",
            "dogstatsd_string_interner_size_bytes",
            "dogstatsd_tcp_port",
            "enable_payloads.events",
            "enable_payloads.series",
            "enable_payloads.service_checks",
            "enable_payloads.sketches",
        ],
    ),
    (
        "encoders.rs",
        &[
            "serializer_compressor_kind",
            "serializer_zstd_compressor_level",
            "flush_timeout_secs",
            "serializer_max_metrics_per_payload",
            "log_payloads",
            "serializer_max_payload_size",
            "serializer_max_uncompressed_payload_size",
            "serializer_max_series_payload_size",
            "serializer_max_series_uncompressed_payload_size",
            "serializer_max_series_points_per_payload",
            "use_v2_api.series",
            "env",
        ],
    ),
    (
        "forwarder.rs",
        &[
            "api_key",
            "site",
            "dd_url",
            "observability_pipelines_worker.metrics.enabled",
            "observability_pipelines_worker.metrics.url",
            "vector.metrics.enabled",
            "vector.metrics.url",
            "additional_endpoints",
            "forwarder_num_workers",
            "forwarder_timeout",
            "forwarder_high_prio_buffer_size",
            "forwarder_connection_reset_interval",
            "forwarder_http_protocol",
            "skip_ssl_validation",
            "min_tls_version",
            "allow_arbitrary_tags",
            "forwarder_backoff_base",
            "forwarder_backoff_factor",
            "forwarder_backoff_max",
            "forwarder_recovery_interval",
            "forwarder_recovery_reset",
            "forwarder_retry_queue_max_size",
            "forwarder_retry_queue_payloads_max_size",
            "forwarder_storage_max_disk_ratio",
            "forwarder_storage_max_size_in_bytes",
            "forwarder_storage_path",
            "forwarder_outdated_file_in_days",
        ],
    ),
    (
        "get_typed.rs",
        &[
            "vsock_addr",
            "cmd_port",
            "log_format_rfc3339",
            "syslog_rfc",
            "syslog_uri",
        ],
    ),
    (
        "mrf.rs",
        &[
            "multi_region_failover.api_key",
            "multi_region_failover.dd_url",
            "multi_region_failover.enabled",
            "multi_region_failover.failover_metrics",
            "multi_region_failover.metric_allowlist",
            "multi_region_failover.site",
        ],
    ),
    (
        "otlp.rs",
        &[
            "otlp_config.receiver.protocols.grpc.endpoint",
            "otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib",
            "otlp_config.receiver.protocols.grpc.transport",
            "otlp_config.receiver.protocols.http.endpoint",
            "otlp_config.receiver.protocols.http.transport",
            "otlp_config.traces.enabled",
            "otlp_config.traces.ignore_missing_datadog_fields",
            "otlp_config.traces.enable_otlp_compute_top_level_by_span_kind",
            "otlp_config.traces.internal_port",
            "otlp_config.traces.probabilistic_sampler.sampling_percentage",
            "otlp_config.traces.string_interner_size",
            "otlp_config.logs.enabled",
            "otlp_config.metrics.enabled",
            "otlp_allow_context_heap_allocs",
            "otlp_cached_contexts_limit",
            "otlp_cached_tagsets_limit",
            "otlp_string_interner_size",
        ],
    ),
    (
        "proxy.rs",
        &[
            "proxy.http",
            "proxy.https",
            "proxy.no_proxy",
            "no_proxy_nonexact_match",
            "use_proxy_for_cloud_metadata",
        ],
    ),
    (
        "tag_filterlist.rs",
        &["data_plane.dogstatsd.aggregator_tag_filter_cache_capacity"],
    ),
    (
        "trace_obfuscation.rs",
        &[
            "apm_config.obfuscation.credit_cards.enabled",
            "apm_config.obfuscation.credit_cards.keep_values",
            "apm_config.obfuscation.credit_cards.luhn",
            "apm_config.obfuscation.elasticsearch.enabled",
            "apm_config.obfuscation.elasticsearch.keep_values",
            "apm_config.obfuscation.elasticsearch.obfuscate_sql_values",
            "apm_config.obfuscation.http.remove_paths_with_digits",
            "apm_config.obfuscation.http.remove_query_string",
            "apm_config.obfuscation.memcached.enabled",
            "apm_config.obfuscation.memcached.keep_command",
            "apm_config.obfuscation.mongodb.enabled",
            "apm_config.obfuscation.mongodb.keep_values",
            "apm_config.obfuscation.mongodb.obfuscate_sql_values",
            "apm_config.obfuscation.opensearch.enabled",
            "apm_config.obfuscation.opensearch.keep_values",
            "apm_config.obfuscation.opensearch.obfuscate_sql_values",
            "apm_config.obfuscation.redis.enabled",
            "apm_config.obfuscation.redis.remove_all_args",
            "apm_config.obfuscation.sql.dbms",
            "apm_config.obfuscation.sql.dollar_quoted_func",
            "apm_config.obfuscation.sql.keep_sql_alias",
            "apm_config.obfuscation.sql.replace_digits",
            "apm_config.obfuscation.sql.table_names",
            "apm_config.obfuscation.valkey.enabled",
            "apm_config.obfuscation.valkey.remove_all_args",
        ],
    ),
    (
        "unsupported.rs",
        &[
            "dogstatsd_disable_verbose_logs",
            "dogstatsd_pipe_name",
            "dogstatsd_stats_buffer",
            "dogstatsd_stats_enable",
            "dogstatsd_stats_port",
            "dogstatsd_telemetry_enabled_listener_id",
            "dogstatsd_windows_pipe_security_descriptor",
            "forwarder_apikey_validation_interval",
            "forwarder_flush_to_disk_mem_ratio",
            "forwarder_low_prio_buffer_size",
            "forwarder_max_concurrent_requests",
            "forwarder_retry_queue_capacity_time_interval_sec",
            "serializer_experimental_use_v3_api.compression_level",
            "serializer_experimental_use_v3_api.series.endpoints",
            "serializer_experimental_use_v3_api.series.validate",
            "serializer_experimental_use_v3_api.sketches.endpoints",
            "serializer_experimental_use_v3_api.sketches.validate",
            "sslkeylogfile",
            "tls_handshake_timeout",
            "aggregator_buffer_size",
            "aggregator_flush_metrics_and_serialize_in_parallel_buffer_size",
            "aggregator_flush_metrics_and_serialize_in_parallel_chan_size",
            "aggregator_stop_timeout",
            "aggregator_use_tags_store",
            "autoscaling.failover.enabled",
            "autoscaling.failover.metrics",
            "dogstatsd_experimental_http.enabled",
            "dogstatsd_experimental_http.listen_address",
            "forwarder_requeue_buffer_size",
            "forwarder_stop_timeout",
            "heroku_dyno",
            "telemetry.dogstatsd.aggregator_channel_latency_buckets",
            "telemetry.dogstatsd.listeners_channel_latency_buckets",
            "telemetry.dogstatsd.listeners_latency_buckets",
            "telemetry.dogstatsd_origin",
            "cluster_agent.enabled",
        ],
    ),
];

fn golden_sort_key(filename: &str, yaml_path: &str) -> (usize, String) {
    for &(file, paths) in GOLDEN_ORDER {
        if file == filename {
            if let Some(pos) = paths.iter().position(|&p| p == yaml_path) {
                return (pos, String::new());
            }
            break;
        }
    }
    (usize::MAX, yaml_path.to_string())
}

#[allow(dead_code)]
pub fn generate(overlay: &SchemaOverlay, schema_map: &IndexMap<String, FieldInfo>, out_dir: &Path) {
    let registry_dir = out_dir.join("config_registry");
    std::fs::create_dir_all(&registry_dir).unwrap();

    validate_saluki_entries(overlay);
    schema_gen::generate_schema_rs(schema_map, &registry_dir);
    generate_subsystem_files(overlay, schema_map, &registry_dir, "");
    generate_unsupported_rs(overlay, &registry_dir, "");
    generate_mod_rs(overlay, &registry_dir);
}

/// Write generated registry files directly into the source tree for PR diff visibility.
///
/// Each subsystem file gets `use super::*;` and `use super::schema;` prepended so
/// it compiles as a file-based Rust module. The `annotations_index.rs` is designed to be
/// `include!()`'d from the hand-written `mod.rs` which already has types in scope.
pub fn generate_in_tree(overlay: &SchemaOverlay, schema_map: &IndexMap<String, FieldInfo>, src_dir: &Path) {
    std::fs::create_dir_all(src_dir).unwrap();

    validate_saluki_entries(overlay);

    let preamble = "#[allow(unused_imports)]\nuse super::schema;\n#[allow(unused_imports)]\nuse super::*;\n\n";
    generate_subsystem_files(overlay, schema_map, src_dir, preamble);
    generate_unsupported_rs(overlay, src_dir, preamble);
    generate_annotations_index(overlay, src_dir);
}

fn validate_saluki_entries(overlay: &SchemaOverlay) {
    for entry in SALUKI_ENTRIES {
        if overlay.supported.contains_key(entry.yaml_path)
            || overlay.unsupported.contains_key(entry.yaml_path)
            || overlay.ignored.contains_key(entry.yaml_path)
        {
            panic!(
                "Saluki entry '{}' collides with a vendored schema key in the overlay — \
                 it should use the schema entry instead of a hard-coded SchemaEntry",
                entry.yaml_path
            );
        }
    }
}

fn generate_subsystem_files(
    overlay: &SchemaOverlay, schema_map: &IndexMap<String, FieldInfo>, dir: &Path, preamble: &str,
) {
    let mut datadog_by_file: IndexMap<String, Vec<(&str, &datadog_agent_config_overlay_model::Supported)>> =
        IndexMap::new();
    for (yaml_path, entry) in &overlay.supported {
        let filename = entry
            .additional_attributes
            .get("config_registry_filename")
            .unwrap_or_else(|| panic!("supported key '{}' missing config_registry_filename", yaml_path));
        datadog_by_file
            .entry(filename.clone())
            .or_default()
            .push((yaml_path.as_str(), entry));
    }

    let mut saluki_by_file: IndexMap<&str, Vec<&SalukiKey>> = IndexMap::new();
    for entry in SALUKI_ENTRIES {
        saluki_by_file.entry(entry.filename).or_default().push(entry);
    }

    let mut all_files: Vec<String> = datadog_by_file.keys().cloned().collect();
    for &f in saluki_by_file.keys() {
        if !all_files.iter().any(|x| x == f) {
            all_files.push(f.to_string());
        }
    }
    all_files.sort_unstable();

    for filename in &all_files {
        let datadog_entries = datadog_by_file
            .get(filename.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let saluki_entries = saluki_by_file
            .get(filename.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        generate_one_file(filename, datadog_entries, saluki_entries, schema_map, dir, preamble);
    }
}

enum AnnotationEntry<'a> {
    Datadog(&'a str, &'a datadog_agent_config_overlay_model::Supported),
    Saluki(&'a SalukiKey),
}

impl<'a> AnnotationEntry<'a> {
    fn yaml_path(&self) -> &str {
        match self {
            Self::Datadog(p, _) => p,
            Self::Saluki(se) => se.yaml_path,
        }
    }
}

fn generate_one_file(
    filename: &str, datadog_entries: &[(&str, &datadog_agent_config_overlay_model::Supported)],
    saluki_entries: &[&SalukiKey], schema_map: &IndexMap<String, FieldInfo>, dir: &Path, preamble: &str,
) {
    let mut entries: Vec<AnnotationEntry> = Vec::new();
    for &(yaml_path, entry) in datadog_entries {
        entries.push(AnnotationEntry::Datadog(yaml_path, entry));
    }
    for &se in saluki_entries {
        entries.push(AnnotationEntry::Saluki(se));
    }
    entries.sort_by_key(|a| golden_sort_key(filename, a.yaml_path()));

    let mut out = String::new();
    writeln!(out, "// @generated by build.rs from schema_overlay.yaml — DO NOT EDIT").unwrap();
    out.push_str(preamble);

    for entry in &entries {
        if let AnnotationEntry::Saluki(se) = entry {
            let const_name = format!("{}_SCHEMA", schema_gen::yaml_path_to_const(se.yaml_path));
            let default_lit = match se.schema_default {
                Some(d) => format!("Some(\"{}\")", schema_gen::escape_str(d)),
                None => "None".to_string(),
            };
            let env_vars_lit = if se.env_vars.is_empty() {
                "&[]".to_string()
            } else {
                let items: Vec<String> = se.env_vars.iter().map(|e| format!("\"{}\"", e)).collect();
                format!("&[{}]", items.join(", "))
            };
            writeln!(out, "static {}: SchemaEntry = SchemaEntry {{", const_name).unwrap();
            writeln!(out, "    schema: Schema::Saluki,").unwrap();
            writeln!(out, "    yaml_path: \"{}\",", se.yaml_path).unwrap();
            writeln!(out, "    env_vars: {},", env_vars_lit).unwrap();
            writeln!(out, "    value_type: {},", se.value_type).unwrap();
            writeln!(out, "    default: {},", default_lit).unwrap();
            writeln!(out, "}};").unwrap();
            writeln!(out).unwrap();
        }
    }

    writeln!(out, "crate::declare_annotations! {{").unwrap();

    for entry in &entries {
        match entry {
            AnnotationEntry::Datadog(yaml_path, e) => {
                emit_datadog_annotation(&mut out, yaml_path, e, schema_map);
            }
            AnnotationEntry::Saluki(se) => {
                emit_saluki_annotation(&mut out, se);
            }
        }
    }

    writeln!(out, "}}").unwrap();

    let path = dir.join(filename);
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
}

fn emit_datadog_annotation(
    out: &mut String, yaml_path: &str, entry: &datadog_agent_config_overlay_model::Supported,
    _schema_map: &IndexMap<String, FieldInfo>,
) {
    let const_name = schema_gen::yaml_path_to_const(yaml_path);
    let support_level = overlay_support_level(&entry.support_level);
    let pipeline_affinity = overlay_pipeline_affinity_expr(&entry.pipelines);

    let alias_paths = &entry.additional_yaml_paths;
    let alias_lit = if alias_paths.is_empty() {
        "&[]".to_string()
    } else {
        let items: Vec<String> = alias_paths.iter().map(|p| format!("\"{}\"", p)).collect();
        format!("&[{}]", items.join(", "))
    };

    let env_override = match &entry.env_var_override {
        None => "None".to_string(),
        Some(vars) => {
            let items: Vec<String> = vars
                .iter()
                .map(|v| format!("\"{}\"", schema_gen::escape_str(v)))
                .collect();
            format!("Some(&[{}])", items.join(", "))
        }
    };

    let used_by_lit = {
        let items: Vec<String> = entry
            .used_by
            .iter()
            .map(|u| format!("structs::{}", u.as_smoke_test_const()))
            .collect();
        format!("&[{}]", items.join(", "))
    };

    let vt_override = match &entry.value_type_override {
        None => "None".to_string(),
        Some(vt) => format!("Some({})", overlay_value_type(vt)),
    };

    let test_json_lit = match &entry.test_json {
        None => "None".to_string(),
        Some(s) => format_test_json(s),
    };

    let description = &entry.description;

    writeln!(out, "    /// `{}`-{}", yaml_path, description).unwrap();
    writeln!(out, "    {} = SalukiAnnotation {{", const_name).unwrap();
    writeln!(out, "        schema: &schema::{},", const_name).unwrap();
    writeln!(out, "        support_level: {},", support_level).unwrap();
    writeln!(out, "        additional_yaml_paths: {},", alias_lit).unwrap();
    writeln!(out, "        env_var_override: {},", env_override).unwrap();
    writeln!(out, "        used_by: {},", used_by_lit).unwrap();
    writeln!(out, "        value_type_override: {},", vt_override).unwrap();
    writeln!(out, "        test_json: {},", test_json_lit).unwrap();
    writeln!(out, "        pipeline_affinity: {},", pipeline_affinity).unwrap();
    writeln!(out, "    }};").unwrap();
}

fn emit_saluki_annotation(out: &mut String, se: &SalukiKey) {
    let const_name = schema_gen::yaml_path_to_const(se.yaml_path);
    let schema_const = format!("{}_SCHEMA", const_name);

    let alias_lit = if se.additional_yaml_paths.is_empty() {
        "&[]".to_string()
    } else {
        let items: Vec<String> = se.additional_yaml_paths.iter().map(|p| format!("\"{}\"", p)).collect();
        format!("&[{}]", items.join(", "))
    };

    let env_override = match se.env_var_override {
        None => "None".to_string(),
        Some(vars) => {
            let items: Vec<String> = vars.iter().map(|v| format!("\"{}\"", v)).collect();
            format!("Some(&[{}])", items.join(", "))
        }
    };

    let used_by_lit = {
        let items: Vec<String> = se.used_by.iter().map(|u| format!("structs::{}", u)).collect();
        format!("&[{}]", items.join(", "))
    };

    let test_json_lit = match se.test_json {
        None => "None".to_string(),
        Some(s) => format_test_json(s),
    };

    writeln!(out, "    /// `{}`", se.yaml_path).unwrap();
    writeln!(out, "    {} = SalukiAnnotation {{", const_name).unwrap();
    writeln!(out, "        schema: &{},", schema_const).unwrap();
    writeln!(out, "        support_level: SupportLevel::Full,").unwrap();
    writeln!(out, "        additional_yaml_paths: {},", alias_lit).unwrap();
    writeln!(out, "        env_var_override: {},", env_override).unwrap();
    writeln!(out, "        used_by: {},", used_by_lit).unwrap();
    writeln!(out, "        value_type_override: None,").unwrap();
    writeln!(out, "        test_json: {},", test_json_lit).unwrap();
    writeln!(out, "        pipeline_affinity: {},", se.pipeline_affinity).unwrap();
    writeln!(out, "    }};").unwrap();
}

fn generate_unsupported_rs(overlay: &SchemaOverlay, dir: &Path, preamble: &str) {
    enum UnsupportedEntry<'a> {
        Unsupported(&'a str, &'a datadog_agent_config_overlay_model::Unsupported),
        Investigate(&'a str, &'a datadog_agent_config_overlay_model::Investigate),
    }
    impl<'a> UnsupportedEntry<'a> {
        fn yaml_path(&self) -> &str {
            match self {
                Self::Unsupported(p, _) => p,
                Self::Investigate(p, _) => p,
            }
        }
    }

    let mut entries: Vec<UnsupportedEntry> = Vec::new();
    for (yaml_path, entry) in &overlay.unsupported {
        entries.push(UnsupportedEntry::Unsupported(yaml_path, entry));
    }
    for (yaml_path, entry) in &overlay.investigate {
        if entry.severity.is_some() {
            entries.push(UnsupportedEntry::Investigate(yaml_path, entry));
        }
    }
    entries.sort_by(|a, b| {
        golden_sort_key("unsupported.rs", a.yaml_path()).cmp(&golden_sort_key("unsupported.rs", b.yaml_path()))
    });

    let mut out = String::new();
    writeln!(out, "// @generated by build.rs from schema_overlay.yaml — DO NOT EDIT").unwrap();
    out.push_str(preamble);
    writeln!(out, "crate::declare_annotations! {{").unwrap();

    for entry in &entries {
        match entry {
            UnsupportedEntry::Unsupported(yaml_path, e) => {
                let const_name = schema_gen::yaml_path_to_const(yaml_path);
                let severity = match e.severity {
                    Severity::Low => "Severity::Low",
                    Severity::Medium => "Severity::Medium",
                    Severity::High => "Severity::High",
                };
                let pipeline_affinity = overlay_pipeline_affinity_expr(&e.pipelines);

                writeln!(out, "    /// `{}`-{}", yaml_path, e.description).unwrap();
                writeln!(out, "    {} = SalukiAnnotation {{", const_name).unwrap();
                writeln!(out, "        schema: &schema::{},", const_name).unwrap();
                writeln!(out, "        support_level: SupportLevel::Incompatible({}),", severity).unwrap();
                writeln!(out, "        additional_yaml_paths: &[],").unwrap();
                writeln!(out, "        env_var_override: None,").unwrap();
                writeln!(out, "        used_by: &[],").unwrap();
                writeln!(out, "        value_type_override: None,").unwrap();
                writeln!(out, "        test_json: None,").unwrap();
                writeln!(out, "        pipeline_affinity: {},", pipeline_affinity).unwrap();
                writeln!(out, "    }};").unwrap();
            }
            UnsupportedEntry::Investigate(yaml_path, e) => {
                let severity = match e.severity {
                    Some(Severity::Low) => "Severity::Low",
                    Some(Severity::Medium) => "Severity::Medium",
                    Some(Severity::High) => "Severity::High",
                    None => unreachable!(),
                };
                let const_name = schema_gen::yaml_path_to_const(yaml_path);

                writeln!(out, "    /// `{}`-{}", yaml_path, e.description).unwrap();
                writeln!(out, "    {} = SalukiAnnotation {{", const_name).unwrap();
                writeln!(out, "        schema: &schema::{},", const_name).unwrap();
                writeln!(out, "        support_level: SupportLevel::Incompatible({}),", severity).unwrap();
                writeln!(out, "        additional_yaml_paths: &[],").unwrap();
                writeln!(out, "        env_var_override: None,").unwrap();
                writeln!(out, "        used_by: &[],").unwrap();
                writeln!(out, "        value_type_override: None,").unwrap();
                writeln!(out, "        test_json: None,").unwrap();
                writeln!(out, "        pipeline_affinity: PipelineAffinity::CrossCutting,").unwrap();
                writeln!(out, "    }};").unwrap();
            }
        }
    }

    writeln!(out, "}}").unwrap();

    let path = dir.join("unsupported.rs");
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
}

#[allow(dead_code)]
fn generate_mod_rs(overlay: &SchemaOverlay, dir: &Path) {
    let supported_files: Vec<String> = {
        let mut files: std::collections::HashSet<String> = overlay
            .supported
            .values()
            .filter_map(|e| e.additional_attributes.get("config_registry_filename").cloned())
            .collect();
        for se in SALUKI_ENTRIES {
            files.insert(se.filename.to_string());
        }
        let mut files: Vec<String> = files.into_iter().collect();
        files.sort_unstable();
        files
    };

    let mut out = String::new();
    writeln!(out, "// @generated by build.rs from schema_overlay.yaml — DO NOT EDIT").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "use crate::config_registry::{{Pipeline, PipelineAffinity, SalukiAnnotation, Schema, SchemaEntry, Severity, SupportLevel, ValueType, structs}};"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "#[allow(dead_code)]").unwrap();
    writeln!(out, "mod schema {{").unwrap();
    writeln!(out, "    #[allow(unused_imports)]").unwrap();
    writeln!(out, "    use super::*;").unwrap();
    writeln!(
        out,
        "    include!(concat!(env!(\"OUT_DIR\"), \"/config_registry/schema.rs\"));"
    )
    .unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for filename in &supported_files {
        let stem = filename.trim_end_matches(".rs");
        writeln!(out, "mod {} {{", stem).unwrap();
        writeln!(out, "    #[allow(unused_imports)]").unwrap();
        writeln!(out, "    use super::*;").unwrap();
        writeln!(
            out,
            "    include!(concat!(env!(\"OUT_DIR\"), \"/config_registry/{}\"));",
            filename
        )
        .unwrap();
        writeln!(out, "}}").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(out, "pub(super) mod unsupported {{").unwrap();
    writeln!(out, "    #[allow(unused_imports)]").unwrap();
    writeln!(out, "    use super::*;").unwrap();
    writeln!(
        out,
        "    include!(concat!(env!(\"OUT_DIR\"), \"/config_registry/unsupported.rs\"));"
    )
    .unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "use std::sync::LazyLock;").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "pub static SUPPORTED_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {{"
    )
    .unwrap();
    writeln!(out, "    let mut v: Vec<&'static SalukiAnnotation> = Vec::new();").unwrap();
    for filename in &supported_files {
        let stem = filename.trim_end_matches(".rs");
        writeln!(out, "    v.extend_from_slice({}::ALL);", stem).unwrap();
    }
    writeln!(out, "    v").unwrap();
    writeln!(out, "}});").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "pub static UNSUPPORTED_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {{"
    )
    .unwrap();
    writeln!(out, "    let mut v: Vec<&'static SalukiAnnotation> = Vec::new();").unwrap();
    writeln!(out, "    v.extend_from_slice(unsupported::ALL);").unwrap();
    writeln!(out, "    v").unwrap();
    writeln!(out, "}});").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "pub static ALL_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {{"
    )
    .unwrap();
    writeln!(out, "    let mut v = SUPPORTED_ANNOTATIONS.clone();").unwrap();
    writeln!(out, "    v.extend_from_slice(&UNSUPPORTED_ANNOTATIONS);").unwrap();
    writeln!(out, "    v").unwrap();
    writeln!(out, "}});").unwrap();

    let path = dir.join("mod.rs");
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
}

/// Generate `annotations_index.rs`, designed to be `include!()`'d from the hand-written `mod.rs`.
///
/// Contains: `mod schema` (include!'ing OUT_DIR/schema.rs), plain `mod` declarations for each
/// subsystem file, and the LazyLock aggregation statics.
fn generate_annotations_index(overlay: &SchemaOverlay, dir: &Path) {
    let supported_files: Vec<String> = {
        let mut files: std::collections::HashSet<String> = overlay
            .supported
            .values()
            .filter_map(|e| e.additional_attributes.get("config_registry_filename").cloned())
            .collect();
        for se in SALUKI_ENTRIES {
            files.insert(se.filename.to_string());
        }
        let mut files: Vec<String> = files.into_iter().collect();
        files.sort_unstable();
        files
    };

    let mut out = String::new();
    writeln!(out, "// @generated by build.rs from schema_overlay.yaml — DO NOT EDIT").unwrap();
    writeln!(
        out,
        "// This file is include!()'d from mod.rs — types are already in scope."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "#[allow(dead_code)]").unwrap();
    writeln!(out, "mod schema {{").unwrap();
    writeln!(out, "    #[allow(unused_imports)]").unwrap();
    writeln!(out, "    use super::*;").unwrap();
    writeln!(
        out,
        "    include!(concat!(env!(\"OUT_DIR\"), \"/config_registry/schema.rs\"));"
    )
    .unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for filename in &supported_files {
        let stem = filename.trim_end_matches(".rs");
        writeln!(out, "mod {};", stem).unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "pub(super) mod unsupported;").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "use std::sync::LazyLock;").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "pub static SUPPORTED_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {{"
    )
    .unwrap();
    writeln!(out, "    let mut v: Vec<&'static SalukiAnnotation> = Vec::new();").unwrap();
    for filename in &supported_files {
        let stem = filename.trim_end_matches(".rs");
        writeln!(out, "    v.extend_from_slice({}::ALL);", stem).unwrap();
    }
    writeln!(out, "    v").unwrap();
    writeln!(out, "}});").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "pub static UNSUPPORTED_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {{"
    )
    .unwrap();
    writeln!(out, "    let mut v: Vec<&'static SalukiAnnotation> = Vec::new();").unwrap();
    writeln!(out, "    v.extend_from_slice(unsupported::ALL);").unwrap();
    writeln!(out, "    v").unwrap();
    writeln!(out, "}});").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "pub static ALL_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {{"
    )
    .unwrap();
    writeln!(out, "    let mut v = SUPPORTED_ANNOTATIONS.clone();").unwrap();
    writeln!(out, "    v.extend_from_slice(&UNSUPPORTED_ANNOTATIONS);").unwrap();
    writeln!(out, "    v").unwrap();
    writeln!(out, "}});").unwrap();

    let path = dir.join("annotations_index.rs");
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
}

fn overlay_support_level(sl: &SupportLevel) -> &'static str {
    match sl {
        SupportLevel::Full => "SupportLevel::Full",
        SupportLevel::Partial => "SupportLevel::Partial",
    }
}

fn overlay_pipeline_affinity_expr(pa: &PipelineAffinity) -> String {
    match pa {
        PipelineAffinity::CrossCutting => "PipelineAffinity::CrossCutting".to_string(),
        PipelineAffinity::Pipelines(ps) => {
            let parts: Vec<&str> = ps
                .iter()
                .map(|p| match p {
                    Pipeline::DogStatsD => "Pipeline::DogStatsD",
                    Pipeline::Checks => "Pipeline::Checks",
                    Pipeline::Otlp => "Pipeline::Otlp",
                    Pipeline::Traces => "Pipeline::Traces",
                })
                .collect();
            format!("PipelineAffinity::Pipelines(&[{}])", parts.join(", "))
        }
    }
}

fn overlay_value_type(vt: &ValueType) -> &'static str {
    match vt {
        ValueType::Boolean => "ValueType::Bool",
        ValueType::Integer => "ValueType::Integer",
        ValueType::Float => "ValueType::Float",
        ValueType::String => "ValueType::String",
        ValueType::StringList => "ValueType::StringList",
    }
}

/// Format a `test_json` value using raw string literals when the value contains quotes,
/// matching the style used in the hand-written `config_registry` files on main.
fn format_test_json(s: &str) -> String {
    if s.contains('"') {
        format!("Some(r#\"{}\"#)", s)
    } else {
        format!("Some(\"{}\")", s)
    }
}
