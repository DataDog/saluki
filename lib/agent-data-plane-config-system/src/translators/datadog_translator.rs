//! The Datadog witness translator: [`DatadogTranslator`].
//!
//! [`DatadogTranslator`] implements [`DatadogConfigWitness`], so the generated `drive` calls one
//! `consume_<key>` per supported Datadog key present in the source. Each method converts the raw
//! Datadog value (`i64`, `String`, `Vec<serde_json::Value>`, ...) into the refined model type
//! (`u16`, `Duration`, `PathBuf`, `ListenAddress`, an enum, ...) and assigns it into its
//! `SalukiConfiguration` destination.
//!
//! Most keys assign a single field directly. The endpoint keys (`api_key`, `dd_url`, `site`,
//! `additional_endpoints`) are carried through as raw values; the model does not resolve them into
//! a primary endpoint yet (see #1965).
//!
//! Conversions that can fail (enum parsing, byte-size parsing, JSON structure parsing) record a
//! [`TranslateError`] via `record_error` and default the field; `drive` returns them all at the
//! end as a [`TranslateErrors`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_data_plane_config::control::ListenAddress;
use agent_data_plane_config::domains::dogstatsd::{
    FilterAction, MapperProfile, MetricMapping, MetricTagFilterEntry, OriginTagCardinality,
};
use agent_data_plane_config::shared::{ForwarderHttpProtocol, ZSTD_DEFAULT_OVERRIDE};
use agent_data_plane_config::SalukiConfiguration;
use bytesize::ByteSize;
use datadog_agent_config::{drive, DatadogConfigWitness, DatadogConfiguration, TranslateError, TranslateErrors};

/// Translates a [`DatadogConfiguration`] into a [`SalukiConfiguration`].
///
/// Construct with [`DatadogTranslator::new`] and call [`DatadogTranslator::translate`]: it drives
/// the witness over every supported Datadog key, then assembles the multi-key endpoint field.
#[derive(Debug)]
pub(crate) struct DatadogTranslator<'a> {
    datadog: &'a DatadogConfiguration,
    config: SalukiConfiguration,
    errors: Vec<TranslateError>,
}

impl<'a> DatadogTranslator<'a> {
    /// Creates a translator that overlays `datadog` onto `base`.
    ///
    /// `base` is the lowest-precedence starting point (defaults plus any Saluki-only seed); the
    /// Datadog drive overlays its schema fields on top and is authoritative for every field it
    /// owns.
    pub(crate) fn new(datadog: &'a DatadogConfiguration, base: SalukiConfiguration) -> Self {
        Self {
            datadog,
            config: base,
            errors: Vec::new(),
        }
    }

    /// Drives the witness over every supported Datadog key. Returns the fully populated config
    /// (invalid values defaulted) plus every translation error recorded, if any.
    pub(crate) fn translate(mut self) -> (SalukiConfiguration, Option<TranslateErrors>) {
        let datadog = self.datadog;
        let errors = drive(datadog, &mut self).err();
        (self.config, errors)
    }

    /// Records a translation error encountered while consuming.
    fn record_error(&mut self, error: TranslateError) {
        self.errors.push(error);
    }

    fn checked_integer_conversion<T>(&mut self, key: &'static str, value: i64) -> Option<T>
    where
        T: TryFrom<i64>,
        T::Error: std::fmt::Display,
    {
        match T::try_from(value) {
            Ok(value) => Some(value),
            Err(error) => {
                self.record_error(TranslateError::new_with_message(
                    key,
                    format!("invalid value `{value}`: {error}"),
                ));
                None
            }
        }
    }
}

/// Returns `Some(s)` when `s` is non-empty, mapping the empty string to "unset".
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Trims surrounding whitespace and returns `Some` only when characters remain.
fn trimmed_non_empty(s: String) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Clamps a raw `i64` port into the `u16` range.
fn to_port(value: i64) -> u16 {
    value.clamp(0, u16::MAX as i64) as u16
}

/// Parses one `dogstatsd_mapper_profiles` object into a [`MapperProfile`].
///
/// The vendored Datadog schema types `dogstatsd_mapper_profiles` (and `metric_tag_filterlist`) as
/// arrays of free-form objects, so the generated witness can only surface them as
/// `Vec<serde_json::Value>`. This parser imposes the typed model shape via a local
/// `#[derive(Deserialize)]` shim, mirroring how the `saluki-components` `dogstatsd_mapper`
/// deserializes profiles.
fn parse_mapper_profile(key: &str, raw: serde_json::Value) -> Result<MapperProfile, TranslateError> {
    #[derive(serde::Deserialize)]
    struct RawMapping {
        #[serde(rename = "match")]
        metric_match: String,
        #[serde(default)]
        match_type: String,
        name: String,
        #[serde(default)]
        tags: HashMap<String, String>,
    }
    #[derive(serde::Deserialize)]
    struct RawProfile {
        name: String,
        prefix: String,
        mappings: Vec<RawMapping>,
    }

    let parsed: RawProfile = serde_json::from_value(raw).map_err(|error| TranslateError::new(key, error))?;
    Ok(MapperProfile {
        name: parsed.name,
        prefix: parsed.prefix,
        mappings: parsed
            .mappings
            .into_iter()
            .map(|m| MetricMapping {
                metric_match: m.metric_match,
                match_type: m.match_type,
                name: m.name,
                tags: m.tags,
            })
            .collect(),
    })
}

/// The result of parsing one `metric_tag_filterlist` object.
///
/// The two failure modes differ in whether the entry can still be used, and the caller relies on
/// that difference to honor the system's opposite stances on translation errors (strict at startup,
/// lenient at runtime). A [`Recovered`][Self::Recovered] entry is still usable, so a runtime update
/// keeps filtering with it while startup still rejects the config on the recorded error. A
/// [`Malformed`][Self::Malformed] object yields no value, so it is skipped.
enum TagFilterEntry {
    /// The object parsed and its `action` was recognized.
    Valid(MetricTagFilterEntry),
    /// The object parsed but its `action` was not `include`, `exclude`, or empty. The entry is kept
    /// with `action` defaulted to `exclude`, matching the component's own tolerance, and the error
    /// is recorded so a strict startup still rejects the config.
    Recovered(MetricTagFilterEntry, TranslateError),
    /// The object could not be deserialized into an entry; no value is recoverable.
    Malformed(TranslateError),
}

/// Parses one `metric_tag_filterlist` object into a [`TagFilterEntry`].
///
/// Like `parse_mapper_profile`, this imposes the typed model shape on a free-form schema object via
/// a local `#[derive(Deserialize)]` shim. Unlike it, an unrecognized `action` does not discard the
/// entry: the schema lets any string through, so the component tolerates typos by defaulting to
/// `exclude`, and this preserves that behavior while still surfacing the error.
fn parse_tag_filter_entry(key: &str, raw: serde_json::Value) -> TagFilterEntry {
    #[derive(serde::Deserialize)]
    struct RawEntry {
        metric_name: String,
        #[serde(default)]
        action: String,
        #[serde(default)]
        tags: Vec<String>,
    }

    let parsed: RawEntry = match serde_json::from_value(raw).map_err(|error| TranslateError::new(key, error)) {
        Ok(parsed) => parsed,
        Err(error) => return TagFilterEntry::Malformed(error),
    };

    let (action, action_error) = match parsed.action.as_str() {
        "include" => (FilterAction::Include, None),
        "" | "exclude" => (FilterAction::Exclude, None),
        other => (
            FilterAction::Exclude,
            Some(TranslateError::new_with_message(
                key,
                format!("unknown filter action `{other}`"),
            )),
        ),
    };

    let entry = MetricTagFilterEntry {
        metric_name: parsed.metric_name,
        action,
        tags: parsed.tags,
    };
    match action_error {
        Some(error) => TagFilterEntry::Recovered(entry, error),
        None => TagFilterEntry::Valid(entry),
    }
}

impl DatadogConfigWitness for DatadogTranslator<'_> {
    fn consume_additional_endpoints(&mut self, value: HashMap<String, Vec<String>>) {
        self.config.shared.endpoints.additional_endpoints = value;
    }

    fn consume_agent_ipc_grpc_max_message_size(&mut self, value: i64) {
        self.config.control.ipc.grpc_max_message_size = value;
    }

    fn consume_aggregator_stop_timeout(&mut self, value: i64) {
        self.config.control.aggregator_stop_timeout = value.max(0) as u64;
    }

    fn consume_allow_arbitrary_tags(&mut self, value: bool) {
        self.config.shared.endpoints.allow_arbitrary_tags = value;
    }

    fn consume_api_key(&mut self, value: String) {
        self.config.shared.endpoints.api_key = value;
    }

    fn consume_apm_config_compute_stats_by_span_kind(&mut self, value: bool) {
        self.config.domains.traces.compute_stats_by_span_kind = value;
    }

    fn consume_apm_config_enable_rare_sampler(&mut self, value: bool) {
        self.config.domains.traces.enable_rare_sampler = value;
    }

    fn consume_apm_config_error_tracking_standalone_enabled(&mut self, value: bool) {
        self.config.domains.traces.error_tracking_standalone_enabled = value;
    }

    fn consume_apm_config_errors_per_second(&mut self, value: f64) {
        self.config.domains.traces.errors_per_second = value;
    }

    fn consume_apm_config_obfuscation_credit_cards_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.credit_cards.enabled = value;
    }

    fn consume_apm_config_obfuscation_credit_cards_keep_values(&mut self, value: Vec<String>) {
        self.config.domains.traces.obfuscation.credit_cards.keep_values = value;
    }

    fn consume_apm_config_obfuscation_credit_cards_luhn(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.credit_cards.luhn = value;
    }

    fn consume_apm_config_obfuscation_elasticsearch_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.elasticsearch.enabled = value;
    }

    fn consume_apm_config_obfuscation_elasticsearch_keep_values(&mut self, value: Vec<String>) {
        self.config.domains.traces.obfuscation.elasticsearch.keep_values = value;
    }

    fn consume_apm_config_obfuscation_elasticsearch_obfuscate_sql_values(&mut self, value: Vec<String>) {
        self.config
            .domains
            .traces
            .obfuscation
            .elasticsearch
            .obfuscate_sql_values = value;
    }

    fn consume_apm_config_obfuscation_http_remove_paths_with_digits(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.http.remove_paths_with_digits = value;
    }

    fn consume_apm_config_obfuscation_http_remove_query_string(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.http.remove_query_string = value;
    }

    fn consume_apm_config_obfuscation_memcached_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.memcached.enabled = value;
    }

    fn consume_apm_config_obfuscation_memcached_keep_command(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.memcached.keep_command = value;
    }

    fn consume_apm_config_obfuscation_mongodb_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.mongodb.enabled = value;
    }

    fn consume_apm_config_obfuscation_mongodb_keep_values(&mut self, value: Vec<String>) {
        self.config.domains.traces.obfuscation.mongodb.keep_values = value;
    }

    fn consume_apm_config_obfuscation_mongodb_obfuscate_sql_values(&mut self, value: Vec<String>) {
        self.config.domains.traces.obfuscation.mongodb.obfuscate_sql_values = value;
    }

    fn consume_apm_config_obfuscation_opensearch_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.opensearch.enabled = value;
    }

    fn consume_apm_config_obfuscation_opensearch_keep_values(&mut self, value: Vec<String>) {
        self.config.domains.traces.obfuscation.opensearch.keep_values = value;
    }

    fn consume_apm_config_obfuscation_opensearch_obfuscate_sql_values(&mut self, value: Vec<String>) {
        self.config.domains.traces.obfuscation.opensearch.obfuscate_sql_values = value;
    }

    fn consume_apm_config_obfuscation_redis_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.redis.enabled = value;
    }

    fn consume_apm_config_obfuscation_redis_remove_all_args(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.redis.remove_all_args = value;
    }

    fn consume_apm_config_obfuscation_valkey_enabled(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.valkey.enabled = value;
    }

    fn consume_apm_config_obfuscation_valkey_remove_all_args(&mut self, value: bool) {
        self.config.domains.traces.obfuscation.valkey.remove_all_args = value;
    }

    fn consume_apm_config_peer_tags(&mut self, value: Vec<String>) {
        self.config.domains.traces.peer_tags = value;
    }

    fn consume_apm_config_peer_tags_aggregation(&mut self, value: bool) {
        self.config.domains.traces.peer_tags_aggregation = value;
    }

    fn consume_apm_config_probabilistic_sampler_enabled(&mut self, value: bool) {
        self.config.domains.traces.probabilistic_sampler.enabled = value;
    }

    fn consume_apm_config_probabilistic_sampler_sampling_percentage(&mut self, value: f64) {
        self.config.domains.traces.probabilistic_sampler.sampling_percentage = value;
    }

    fn consume_apm_config_target_traces_per_second(&mut self, value: f64) {
        self.config.domains.traces.target_traces_per_second = value;
    }

    fn consume_autoscaling_failover_enabled(&mut self, value: bool) {
        self.config.shared.autoscaling_failover.enabled = value;
    }

    fn consume_autoscaling_failover_metrics(&mut self, value: Vec<String>) {
        self.config.shared.autoscaling_failover.metrics = value;
    }

    fn consume_bind_host(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.bind_host = non_empty(value);
    }

    fn consume_cluster_agent_auth_token(&mut self, value: String) {
        self.config.shared.cluster_agent.auth_token = trimmed_non_empty(value);
    }

    fn consume_cluster_agent_enabled(&mut self, value: bool) {
        self.config.shared.cluster_agent.enabled = value;
    }

    fn consume_cluster_agent_kubernetes_service_name(&mut self, value: String) {
        self.config.shared.cluster_agent.kubernetes_service_name = Some(value.trim().to_string());
    }

    fn consume_cluster_agent_url(&mut self, value: String) {
        self.config.shared.cluster_agent.url = trimmed_non_empty(value);
    }

    fn consume_cmd_port(&mut self, value: i64) {
        self.config.control.ipc.cmd_port = to_port(value);
    }

    fn consume_cri_connection_timeout(&mut self, value: i64) {
        self.config.control.ipc.cri_connection_timeout = value;
    }

    fn consume_cri_query_timeout(&mut self, value: i64) {
        self.config.control.ipc.cri_query_timeout = value;
    }

    fn consume_data_plane_api_listen_address(&mut self, value: String) {
        self.config.control.api_listen_address = ListenAddress(value);
    }

    fn consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(&mut self, value: i64) {
        self.config
            .domains
            .dogstatsd
            .aggregation
            .aggregator_tag_filter_cache_capacity = value.max(0) as usize;
    }

    fn consume_data_plane_dogstatsd_enabled(&mut self, value: bool) {
        self.config.control.dogstatsd = value;
    }

    fn consume_data_plane_enabled(&mut self, value: bool) {
        self.config.control.enabled = value;
    }

    fn consume_data_plane_log_file(&mut self, value: Option<String>) {
        // Flagged `saluki_overrides_default`, so the source is absence-aware: `None` (unset) stays
        // `None` on the model, which `LoggingConfigurationTranslator` resolves to the platform
        // default log path. The Agent schema's Linux literal is never adopted as the default here.
        self.config.control.logging.file = value;
    }

    fn consume_data_plane_otlp_enabled(&mut self, value: bool) {
        self.config.control.otlp = value;
    }

    fn consume_data_plane_otlp_proxy_enabled(&mut self, value: bool) {
        self.config.domains.otlp.proxy.enabled = value;
    }

    fn consume_data_plane_otlp_proxy_logs_enabled(&mut self, value: bool) {
        self.config.domains.otlp.proxy.logs_enabled = value;
    }

    fn consume_data_plane_otlp_proxy_metrics_enabled(&mut self, value: bool) {
        self.config.domains.otlp.proxy.metrics_enabled = value;
    }

    fn consume_data_plane_otlp_proxy_receiver_protocols_grpc_endpoint(&mut self, value: String) {
        self.config.domains.otlp.proxy.grpc_endpoint = value;
    }

    fn consume_data_plane_otlp_proxy_traces_enabled(&mut self, value: bool) {
        self.config.domains.otlp.proxy.traces_enabled = value;
    }

    fn consume_data_plane_remote_agent_enabled(&mut self, value: bool) {
        self.config.control.remote_agent_enabled = value;
    }

    fn consume_data_plane_secure_api_listen_address(&mut self, value: String) {
        self.config.control.secure_api_listen_address = ListenAddress(value);
    }

    fn consume_data_plane_use_new_config_stream_endpoint(&mut self, value: bool) {
        self.config.control.use_new_config_stream_endpoint = value;
    }

    fn consume_dd_url(&mut self, value: String) {
        self.config.shared.endpoints.dd_url = non_empty(value);
    }

    fn consume_disable_file_logging(&mut self, value: bool) {
        self.config.control.logging.disable_file_logging = value;
    }

    fn consume_dogstatsd_buffer_size(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_buffer_size", value) {
            self.config.domains.dogstatsd.listeners.buffer_size = value;
        }
    }

    fn consume_dogstatsd_capture_depth(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_capture_depth", value) {
            self.config.domains.dogstatsd.listeners.capture_depth = value;
        }
    }

    fn consume_dogstatsd_capture_path(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.capture_path = PathBuf::from(value);
    }

    fn consume_dogstatsd_context_expiry_seconds(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_context_expiry_seconds", value) {
            self.config.domains.dogstatsd.aggregation.context_expiry_seconds = value;
        }
    }

    fn consume_dogstatsd_disable_verbose_logs(&mut self, value: bool) {
        self.config.domains.dogstatsd.debug_log.disable_verbose_logs = value;
    }

    fn consume_dogstatsd_entity_id_precedence(&mut self, value: bool) {
        self.config.domains.dogstatsd.origin.entity_id_precedence = value;
    }

    fn consume_dogstatsd_eol_required(&mut self, value: Vec<String>) {
        self.config.domains.dogstatsd.listeners.eol_required = value;
    }

    fn consume_dogstatsd_expiry_seconds(&mut self, value: i64) {
        self.config.domains.dogstatsd.aggregation.counter_expiry_seconds = Some(value.max(0) as u64);
    }

    fn consume_dogstatsd_flush_incomplete_buckets(&mut self, value: bool) {
        self.config.domains.dogstatsd.aggregation.flush_open_windows = value;
    }

    fn consume_dogstatsd_log_file(&mut self, value: String) {
        self.config.domains.dogstatsd.debug_log.log_file = PathBuf::from(value);
    }

    fn consume_dogstatsd_log_file_max_rolls(&mut self, value: i64) {
        self.config.domains.dogstatsd.debug_log.log_file_max_rolls = value.max(0) as usize;
    }

    fn consume_dogstatsd_log_file_max_size(&mut self, value: String) {
        match value.parse::<ByteSize>() {
            Ok(size) => self.config.domains.dogstatsd.debug_log.log_file_max_size = size.as_u64(),
            Err(reason) => self.record_error(TranslateError::new_with_message("dogstatsd_log_file_max_size", reason)),
        }
    }

    fn consume_dogstatsd_logging_enabled(&mut self, value: bool) {
        self.config.domains.dogstatsd.debug_log.logging_enabled = value;
    }

    fn consume_dogstatsd_mapper_cache_size(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_mapper_cache_size", value) {
            self.config.domains.dogstatsd.mapper.cache_size = value;
        }
    }

    fn consume_dogstatsd_mapper_profiles(&mut self, value: Vec<serde_json::Value>) {
        let mut profiles = Vec::with_capacity(value.len());
        for raw in value {
            match parse_mapper_profile("dogstatsd_mapper_profiles", raw) {
                Ok(profile) => profiles.push(profile),
                Err(error) => {
                    self.record_error(error);
                    return;
                }
            }
        }
        self.config.domains.dogstatsd.mapper.profiles = profiles;
    }

    fn consume_dogstatsd_metrics_stats_enable(&mut self, value: bool) {
        self.config.domains.dogstatsd.debug_log.metrics_stats_enable = value;
    }

    fn consume_dogstatsd_no_aggregation_pipeline(&mut self, value: bool) {
        self.config.domains.dogstatsd.aggregation.no_aggregation_pipeline = value;
    }

    fn consume_dogstatsd_non_local_traffic(&mut self, value: bool) {
        self.config.domains.dogstatsd.listeners.non_local_traffic = value;
    }

    fn consume_dogstatsd_origin_detection(&mut self, value: bool) {
        self.config.domains.dogstatsd.origin.detection = value;
    }

    fn consume_dogstatsd_origin_detection_client(&mut self, value: bool) {
        self.config.domains.dogstatsd.origin.detection_client = value;
    }

    fn consume_dogstatsd_origin_optout_enabled(&mut self, value: bool) {
        self.config.domains.dogstatsd.origin.optout_enabled = value;
    }

    fn consume_dogstatsd_pipe_name(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.pipe_name = non_empty(value);
    }

    fn consume_dogstatsd_port(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_port", value) {
            self.config.domains.dogstatsd.listeners.port = value;
        }
    }

    fn consume_dogstatsd_so_rcvbuf(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_so_rcvbuf", value) {
            self.config.domains.dogstatsd.listeners.so_rcvbuf = value;
        }
    }

    fn consume_dogstatsd_socket(&mut self, value: Option<String>) {
        self.config.domains.dogstatsd.listeners.socket = value.and_then(non_empty);
    }

    fn consume_dogstatsd_stream_log_too_big(&mut self, value: bool) {
        self.config.domains.dogstatsd.listeners.stream_log_too_big = value;
    }

    fn consume_dogstatsd_stream_socket(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.stream_socket = non_empty(value);
    }

    fn consume_dogstatsd_string_interner_size(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("dogstatsd_string_interner_size", value) {
            self.config.domains.dogstatsd.contexts.string_interner_size = value;
        }
    }

    fn consume_dogstatsd_tag_cardinality(&mut self, value: String) {
        // TODO: consider moving the enum to agent-data-plane-config
        let cardinality = match value.to_ascii_lowercase().as_str() {
            "low" => OriginTagCardinality::Low,
            "orchestrator" => OriginTagCardinality::Orchestrator,
            "high" => OriginTagCardinality::High,
            "none" => OriginTagCardinality::None,
            other => {
                self.record_error(TranslateError::new_with_message(
                    "dogstatsd_tag_cardinality",
                    format!("unknown tag cardinality `{other}`"),
                ));
                return;
            }
        };
        self.config.domains.dogstatsd.origin.tag_cardinality = cardinality;
    }

    fn consume_dogstatsd_tags(&mut self, value: Vec<String>) {
        self.config.domains.dogstatsd.tags = value;
    }

    fn consume_dogstatsd_windows_pipe_security_descriptor(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.windows_pipe_security_descriptor = value;
    }

    fn consume_enable_payloads_events(&mut self, value: bool) {
        self.config.domains.dogstatsd.enable_payloads.events = value;
    }

    fn consume_enable_payloads_series(&mut self, value: bool) {
        self.config.domains.dogstatsd.enable_payloads.series = value;
    }

    fn consume_enable_payloads_service_checks(&mut self, value: bool) {
        self.config.domains.dogstatsd.enable_payloads.service_checks = value;
    }

    fn consume_enable_payloads_sketches(&mut self, value: bool) {
        self.config.domains.dogstatsd.enable_payloads.sketches = value;
    }

    fn consume_env(&mut self, value: String) {
        self.config.domains.traces.env = value;
    }

    fn consume_expected_tags_duration(&mut self, value: Duration) {
        self.config.shared.tags.expected_tags_duration = value;
    }

    fn consume_forwarder_apikey_validation_interval(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.apikey_validation_interval = value;
    }

    fn consume_forwarder_backoff_base(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.backoff_base = value as f64;
    }

    fn consume_forwarder_backoff_factor(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.backoff_factor = value as f64;
    }

    fn consume_forwarder_backoff_max(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.backoff_max = value as f64;
    }

    fn consume_forwarder_connection_reset_interval(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.connection_reset_interval = value.max(0) as u64;
    }

    fn consume_forwarder_flush_to_disk_mem_ratio(&mut self, value: f64) {
        self.config.shared.endpoints.forwarder.flush_to_disk_mem_ratio = value;
    }

    fn consume_forwarder_high_prio_buffer_size(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.high_prio_buffer_size = value.max(0) as usize;
    }

    fn consume_forwarder_http_protocol(&mut self, value: String) {
        self.config.shared.endpoints.forwarder.http_protocol = match value.as_str() {
            "http1" => ForwarderHttpProtocol::Http1,
            _ => ForwarderHttpProtocol::Auto,
        };
    }

    fn consume_forwarder_max_concurrent_requests(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.max_concurrent_requests = value.max(0) as usize;
    }

    fn consume_forwarder_num_workers(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.num_workers = value.max(0) as usize;
    }

    fn consume_forwarder_outdated_file_in_days(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.outdated_file_in_days = value.max(0) as u32;
    }

    fn consume_forwarder_recovery_interval(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.recovery_interval = value.max(0) as u32;
    }

    fn consume_forwarder_recovery_reset(&mut self, value: bool) {
        self.config.shared.endpoints.forwarder.recovery_reset = value;
    }

    fn consume_forwarder_retry_queue_capacity_time_interval_sec(&mut self, value: i64) {
        self.config
            .shared
            .endpoints
            .forwarder
            .retry_queue_capacity_time_interval_sec = value.max(0) as u64;
    }

    fn consume_forwarder_retry_queue_max_size(&mut self, value: Option<i64>) {
        self.config.shared.endpoints.forwarder.retry_queue_max_size = value.map(|v| v.max(0) as u64);
    }

    fn consume_forwarder_retry_queue_payloads_max_size(&mut self, value: Option<i64>) {
        self.config.shared.endpoints.forwarder.retry_queue_payloads_max_size = value.map(|v| v.max(0) as u64);
    }

    fn consume_forwarder_stop_timeout(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.stop_timeout = value.max(0) as u64;
    }

    fn consume_forwarder_storage_max_disk_ratio(&mut self, value: f64) {
        self.config.shared.endpoints.forwarder.storage_max_disk_ratio = value;
    }

    fn consume_forwarder_storage_max_size_in_bytes(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.storage_max_size_in_bytes = value.max(0) as u64;
    }

    fn consume_forwarder_storage_path(&mut self, value: String) {
        self.config.shared.endpoints.forwarder.storage_path = PathBuf::from(value);
    }

    fn consume_forwarder_timeout(&mut self, value: i64) {
        self.config.shared.endpoints.forwarder.timeout = value.max(0) as u64;
    }

    fn consume_histogram_aggregates(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.histogram.aggregates = value;
    }

    fn consume_histogram_copy_to_distribution(&mut self, value: bool) {
        self.config.shared.metrics_encoding.histogram.copy_to_distribution = value;
    }

    fn consume_histogram_copy_to_distribution_prefix(&mut self, value: String) {
        self.config
            .shared
            .metrics_encoding
            .histogram
            .copy_to_distribution_prefix = value;
    }

    fn consume_histogram_percentiles(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.histogram.percentiles = value;
    }

    fn consume_log_file_max_rolls(&mut self, value: i64) {
        self.config.control.logging.file_max_rolls = value.max(0) as usize;
    }

    fn consume_log_file_max_size(&mut self, value: String) {
        match value.parse::<ByteSize>() {
            Ok(size) => self.config.control.logging.file_max_size = size.as_u64(),
            Err(reason) => self.record_error(TranslateError::new_with_message("log_file_max_size", reason)),
        }
    }

    fn consume_log_format_json(&mut self, value: bool) {
        self.config.control.logging.format_json = value;
    }

    fn consume_log_format_rfc3339(&mut self, value: bool) {
        self.config.control.logging.format_rfc3339 = value;
    }

    fn consume_log_level(&mut self, value: String) {
        self.config.control.logging.level = value;
    }

    fn consume_log_payloads(&mut self, value: bool) {
        self.config.shared.metrics_encoding.log_payloads = value;
    }

    fn consume_log_to_console(&mut self, value: bool) {
        self.config.control.logging.to_console = value;
    }

    fn consume_log_to_syslog(&mut self, value: bool) {
        self.config.control.logging.to_syslog = value;
    }

    fn consume_metric_filterlist(&mut self, value: Vec<String>) {
        self.config.domains.dogstatsd.prefix_filter.metric_filterlist = value;
    }

    fn consume_metric_filterlist_match_prefix(&mut self, value: bool) {
        self.config
            .domains
            .dogstatsd
            .prefix_filter
            .metric_filterlist_match_prefix = value;
    }

    fn consume_metric_tag_filterlist(&mut self, value: Vec<serde_json::Value>) {
        // A single bad entry must not empty the whole list: startup rejects the config on any
        // recorded error, but a runtime update stores what translated, so dropping the list here
        // would silently disable all tag filtering. Keep every entry we can build and record the
        // errors alongside them.
        let mut entries = Vec::with_capacity(value.len());
        for raw in value {
            match parse_tag_filter_entry("metric_tag_filterlist", raw) {
                TagFilterEntry::Valid(entry) => entries.push(entry),
                TagFilterEntry::Recovered(entry, error) => {
                    self.record_error(error);
                    entries.push(entry);
                }
                TagFilterEntry::Malformed(error) => self.record_error(error),
            }
        }
        self.config.domains.dogstatsd.tag_filterlist = entries;
    }

    fn consume_min_tls_version(&mut self, value: String) {
        self.config.shared.endpoints.tls.min_tls_version = value;
    }

    fn consume_multi_region_failover_api_key(&mut self, value: String) {
        self.config.domains.multi_region_failover.api_key = non_empty(value.trim().to_string());
    }

    fn consume_multi_region_failover_dd_url(&mut self, value: String) {
        self.config.domains.multi_region_failover.dd_url = non_empty(value.trim().to_string());
    }

    fn consume_multi_region_failover_enabled(&mut self, value: bool) {
        self.config.domains.multi_region_failover.enabled = value;
    }

    fn consume_multi_region_failover_failover_metrics(&mut self, value: bool) {
        self.config.domains.multi_region_failover.failover_metrics = value;
    }

    fn consume_multi_region_failover_metric_allowlist(&mut self, value: Vec<String>) {
        self.config.domains.multi_region_failover.metric_allowlist = value;
    }

    fn consume_multi_region_failover_site(&mut self, value: String) {
        self.config.domains.multi_region_failover.site = non_empty(value.trim().to_string());
    }

    fn consume_no_proxy_nonexact_match(&mut self, value: bool) {
        self.config.shared.endpoints.proxy.no_proxy_nonexact_match = value;
    }

    fn consume_observability_pipelines_worker_metrics_enabled(&mut self, value: bool) {
        self.config.shared.endpoints.opw_intake.enabled = value;
    }

    fn consume_observability_pipelines_worker_metrics_url(&mut self, value: String) {
        self.config.shared.endpoints.opw_intake.url = value;
    }

    fn consume_observability_pipelines_worker_metrics_use_v3_api_series(&mut self, value: bool) {
        self.config.shared.endpoints.opw_intake.use_v3_series = value;
    }

    fn consume_origin_detection_unified(&mut self, value: bool) {
        self.config.domains.dogstatsd.origin.unified = value;
    }

    fn consume_otlp_config_logs_enabled(&mut self, value: Option<bool>) {
        // Flagged `saluki_overrides_default`: absent (`None`) keeps the model default (ADP enables
        // OTLP logs); an explicit value wins, so `Some(false)` still disables logs.
        if let Some(value) = value {
            self.config.domains.otlp.receiver.logs_enabled = value;
        }
    }

    fn consume_otlp_config_metrics_enabled(&mut self, value: bool) {
        self.config.domains.otlp.receiver.metrics_enabled = value;
    }

    fn consume_otlp_config_receiver_protocols_grpc_endpoint(&mut self, value: Option<String>) {
        // Flagged `saluki_overrides_default`: absent (`None`) keeps the model default (ADP binds all
        // interfaces); an explicit endpoint wins.
        if let Some(value) = value {
            self.config.domains.otlp.receiver.grpc.endpoint = value;
        }
    }

    fn consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(&mut self, value: i64) {
        self.config.domains.otlp.receiver.grpc.max_recv_msg_size_mib = value.max(0) as u64;
    }

    fn consume_otlp_config_receiver_protocols_grpc_transport(&mut self, value: String) {
        self.config.domains.otlp.receiver.grpc.transport = value;
    }

    fn consume_otlp_config_receiver_protocols_http_endpoint(&mut self, value: Option<String>) {
        // Flagged `saluki_overrides_default`: absent (`None`) keeps the model default (ADP binds all
        // interfaces); an explicit endpoint wins.
        if let Some(value) = value {
            self.config.domains.otlp.receiver.http.endpoint = value;
        }
    }

    fn consume_otlp_config_traces_enabled(&mut self, value: bool) {
        self.config.domains.traces.otlp.enabled = value;
    }

    fn consume_otlp_config_traces_internal_port(&mut self, value: i64) {
        self.config.domains.traces.otlp.internal_port = to_port(value);
    }

    fn consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(&mut self, value: f64) {
        self.config
            .domains
            .traces
            .otlp
            .probabilistic_sampler_sampling_percentage = value;
    }

    fn consume_provider_kind(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.provider_kind = value;
    }

    fn consume_proxy_http(&mut self, value: String) {
        self.config.shared.endpoints.proxy.http = value;
    }

    fn consume_proxy_https(&mut self, value: String) {
        self.config.shared.endpoints.proxy.https = value;
    }

    fn consume_proxy_no_proxy(&mut self, value: Vec<String>) {
        self.config.shared.endpoints.proxy.no_proxy = value;
    }

    fn consume_run_path(&mut self, value: String) {
        // The vendored schema default is the literal template `${run_path}`, which Saluki does not
        // expand. Treat that placeholder (as well as empty or whitespace-only values) as unset,
        // leaving `run_path` as the empty default so the forwarder does not derive a bogus
        // `${run_path}/transactions_to_retry` storage path.
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed == "${run_path}" {
            return;
        }
        self.config.shared.run_path = PathBuf::from(value);
    }

    fn consume_secret_backend_command(&mut self, value: String) {
        self.config.shared.secrets.backend_command = value;
    }

    fn consume_secret_refresh_on_api_key_failure_interval(&mut self, value: i64) {
        self.config.shared.secrets.refresh_on_api_key_failure_interval = value.max(0) as u64;
    }

    fn consume_serializer_compressor_kind(&mut self, value: String) {
        self.config.shared.endpoints.compression.compressor_kind = value;
    }

    fn consume_serializer_experimental_use_v3_api_compression_level(&mut self, value: i64) {
        self.config.shared.metrics_encoding.v3_api.compression_level = value as i32;
    }

    fn consume_serializer_experimental_use_v3_api_series_beta_route(&mut self, value: String) {
        self.config.shared.metrics_encoding.v3_api.series.beta_route = value;
    }

    fn consume_serializer_experimental_use_v3_api_series_endpoints(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.v3_api.series.endpoints = value;
    }

    fn consume_serializer_experimental_use_v3_api_series_shadow_sample_rate(&mut self, value: f64) {
        self.config.shared.metrics_encoding.v3_api.series.shadow_sample_rate = value;
    }

    fn consume_serializer_experimental_use_v3_api_series_shadow_sites(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.v3_api.series.shadow_sites = value;
    }

    fn consume_serializer_experimental_use_v3_api_series_use_beta(&mut self, value: bool) {
        self.config.shared.metrics_encoding.v3_api.series.use_beta = value;
    }

    fn consume_serializer_experimental_use_v3_api_series_validate(&mut self, value: bool) {
        self.config.shared.metrics_encoding.v3_api.series.validate = value;
    }

    fn consume_serializer_experimental_use_v3_api_sketches_endpoints(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.v3_api.sketches.endpoints = value;
    }

    fn consume_serializer_experimental_use_v3_api_sketches_validate(&mut self, value: bool) {
        self.config.shared.metrics_encoding.v3_api.sketches.validate = value;
    }

    fn consume_serializer_max_payload_size(&mut self, value: i64) {
        self.config.shared.metrics_encoding.max_payload_size = value.max(0) as usize;
    }

    fn consume_serializer_max_series_payload_size(&mut self, value: i64) {
        self.config.shared.metrics_encoding.max_series_payload_size = value.max(0) as usize;
    }

    fn consume_serializer_max_series_points_per_payload(&mut self, value: i64) {
        self.config.shared.metrics_encoding.max_series_points_per_payload = value.max(0) as usize;
    }

    fn consume_serializer_max_series_uncompressed_payload_size(&mut self, value: i64) {
        self.config.shared.metrics_encoding.max_series_uncompressed_payload_size = value.max(0) as usize;
    }

    fn consume_serializer_max_uncompressed_payload_size(&mut self, value: i64) {
        self.config.shared.metrics_encoding.max_uncompressed_payload_size = value.max(0) as usize;
    }

    fn consume_serializer_zstd_compressor_level(&mut self, value: i64) {
        // TODO: The core Agent streams a fully resolved config, so its schema default for
        // `serializer_zstd_compressor_level` arrives here as a concrete value rather than being
        // absent. When the incoming level matches that Agent default we swap in ADP's intended
        // level; without this the Agent default would silently override it. We compare against the
        // schema-generated default, so an operator who deliberately sets exactly the Agent default is
        // indistinguishable from the default itself and also gets overridden. Removing that ambiguity
        // needs per-value source tracking (user-set vs Agent default), which is follow-up work.
        let agent_default = DatadogConfiguration::default().serializer_zstd_compressor_level;
        self.config.shared.endpoints.compression.zstd_compressor_level = if value == agent_default {
            ZSTD_DEFAULT_OVERRIDE
        } else {
            value as i32
        };
    }

    fn consume_site(&mut self, value: String) {
        self.config.shared.endpoints.site = non_empty(value);
    }

    fn consume_skip_ssl_validation(&mut self, value: bool) {
        self.config.shared.endpoints.tls.skip_ssl_validation = value;
    }

    fn consume_sslkeylogfile(&mut self, value: String) {
        self.config.shared.endpoints.tls.sslkeylogfile = value;
    }

    fn consume_statsd_forward_host(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.forward_host = non_empty(value);
    }

    fn consume_statsd_forward_port(&mut self, value: i64) {
        if let Some(value) = self.checked_integer_conversion("statsd_forward_port", value) {
            self.config.domains.dogstatsd.listeners.forward_port = value;
        }
    }

    fn consume_statsd_metric_blocklist(&mut self, value: Vec<String>) {
        self.config.domains.dogstatsd.prefix_filter.metric_blocklist = value;
    }

    fn consume_statsd_metric_blocklist_match_prefix(&mut self, value: bool) {
        self.config
            .domains
            .dogstatsd
            .prefix_filter
            .metric_blocklist_match_prefix = value;
    }

    fn consume_statsd_metric_namespace(&mut self, value: String) {
        self.config.domains.dogstatsd.prefix_filter.metric_namespace = value;
    }

    fn consume_statsd_metric_namespace_blacklist(&mut self, value: Vec<String>) {
        self.config.domains.dogstatsd.prefix_filter.metric_namespace_blocklist = value;
    }

    fn consume_syslog_rfc(&mut self, value: bool) {
        self.config.control.logging.syslog_rfc = value;
    }

    fn consume_syslog_uri(&mut self, value: String) {
        self.config.control.logging.syslog_uri = value;
    }

    fn consume_telemetry_dogstatsd_origin(&mut self, value: bool) {
        self.config.domains.dogstatsd.telemetry.origin_breakdown = value;
    }

    fn consume_use_proxy_for_cloud_metadata(&mut self, value: bool) {
        self.config.shared.endpoints.proxy.use_proxy_for_cloud_metadata = value;
    }

    fn consume_use_v2_api_series(&mut self, value: bool) {
        self.config.shared.metrics_encoding.use_v2_series_api = value;
    }

    fn consume_use_v3_api_series_enabled(&mut self, value: String) {
        // TODO: consider modeling as an enum.
        self.config.shared.metrics_encoding.v3_series_mode.mode = value;
    }

    fn consume_use_v3_api_series_endpoints(&mut self, value: ::serde_json::Map<String, ::serde_json::Value>) {
        // The old `V3SeriesModeValue` deserializer accepted only strings and booleans as endpoint
        // mode values; every other JSON kind failed deserialization and rejected the config. This
        // path receives untyped values, so reproduce that domain here: keep strings as-is, normalize
        // booleans to `"true"`/`"false"`, and reject anything else. A bad entry must not empty the
        // whole map (startup rejects the config on the recorded error, but a runtime update stores
        // what translated), so keep every valid entry and record the errors alongside them.
        let mut endpoint_modes = HashMap::with_capacity(value.len());
        for (endpoint, mode) in value {
            match mode {
                ::serde_json::Value::String(mode) => {
                    endpoint_modes.insert(endpoint, mode);
                }
                ::serde_json::Value::Bool(mode) => {
                    endpoint_modes.insert(endpoint, mode.to_string());
                }
                other => self.record_error(TranslateError::new_with_message(
                    "use_v3_api.series.endpoints",
                    format!("endpoint `{endpoint}` mode must be a string or boolean, got {other}"),
                )),
            }
        }
        self.config.shared.metrics_encoding.v3_series_mode.endpoint_modes = endpoint_modes;
    }

    fn consume_vector_metrics_enabled(&mut self, value: bool) {
        self.config.shared.endpoints.vector_intake.enabled = value;
    }

    fn consume_vector_metrics_url(&mut self, value: String) {
        self.config.shared.endpoints.vector_intake.url = value;
    }

    fn consume_vector_metrics_use_v3_api_series(&mut self, value: bool) {
        self.config.shared.endpoints.vector_intake.use_v3_series = value;
    }

    fn consume_vsock_addr(&mut self, value: String) {
        self.config.control.ipc.vsock_addr = value;
    }

    fn translate_errors(&mut self) -> Vec<TranslateError> {
        std::mem::take(&mut self.errors)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use agent_data_plane_config::SalukiConfiguration;
    use datadog_agent_config::DatadogConfiguration;
    use serde_json::json;

    use super::DatadogTranslator;
    use crate::saluki_only::SalukiOnly;

    #[test]
    fn translate_small_map_through_witness_and_seed() {
        // A small raw Datadog source map exercising a scalar conversion, an enum parse, a
        // duration parse, and the raw endpoint inputs.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "api_key": "abc",
            "dd_url": "https://custom.example.com",
            "dogstatsd_port": 9125,
            "dogstatsd_tag_cardinality": "high",
            "expected_tags_duration": "15s",
            "multi_region_failover": {
                "api_key": " mrf-key ",
                "dd_url": " https://mrf.example.com ",
                "site": " datadoghq.eu "
            },
            "telemetry": { "dogstatsd_origin": true },
        }))
        .expect("datadog source deserializes");

        // A small Saluki-only source setting one seeded field.
        let saluki_only: SalukiOnly = serde_json::from_value(json!({
            "dogstatsd_tcp_port": 8126,
        }))
        .expect("saluki-only source deserializes");

        // Seed builds the base; the Datadog drive overlays and is authoritative.
        let mut base = SalukiConfiguration::default();
        saluki_only.seed(&mut base);
        let (config, errors) = DatadogTranslator::new(&datadog, base).translate();
        assert!(errors.is_none());

        // Driven scalar conversion: i64 -> u16.
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
        // Driven enum parse.
        assert_eq!(
            config.domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
        // Driven `format: duration` parse: a Go duration string becomes a `Duration`.
        assert_eq!(config.shared.tags.expected_tags_duration, Duration::from_secs(15));
        // Driven bool in a nested Datadog section.
        assert!(config.domains.dogstatsd.telemetry.origin_breakdown);
        // Raw endpoint inputs: carried through without resolution (see #1965).
        assert_eq!(config.shared.endpoints.api_key, "abc");
        assert_eq!(
            config.shared.endpoints.dd_url.as_deref(),
            Some("https://custom.example.com")
        );
        assert_eq!(config.domains.multi_region_failover.api_key.as_deref(), Some("mrf-key"));
        assert_eq!(
            config.domains.multi_region_failover.dd_url.as_deref(),
            Some("https://mrf.example.com")
        );
        assert_eq!(
            config.domains.multi_region_failover.site.as_deref(),
            Some("datadoghq.eu")
        );
        // Seeded Saluki-only field.
        assert_eq!(config.domains.dogstatsd.listeners.tcp_port, 8126);
    }

    #[test]
    fn bad_tag_filter_action_keeps_the_whole_list() {
        use agent_data_plane_config::domains::dogstatsd::FilterAction;

        // One entry has a typo'd action between two valid entries. The bad action must not discard
        // the list: the entry is kept with `action` defaulted to `exclude`, an error is recorded
        // (so a strict startup rejects the config), and the surrounding valid entries survive (so a
        // lenient runtime update keeps filtering).
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "metric_tag_filterlist": [
                { "metric_name": "a", "action": "include", "tags": ["x"] },
                { "metric_name": "b", "action": "exlude", "tags": ["y"] },
                { "metric_name": "c", "action": "exclude", "tags": ["z"] },
            ],
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();

        let entries = &config.domains.dogstatsd.tag_filterlist;
        assert_eq!(entries.len(), 3, "a bad action must not drop the other entries");
        assert_eq!(entries[0].action, FilterAction::Include);
        assert_eq!(
            entries[1].action,
            FilterAction::Exclude,
            "unknown action defaults to exclude"
        );
        assert_eq!(entries[1].metric_name, "b");
        assert_eq!(entries[2].action, FilterAction::Exclude);

        // The error is still surfaced, so startup's strict gate rejects the config.
        assert!(errors.is_some(), "an unknown action must record a translation error");
    }

    #[test]
    fn v3_series_endpoint_modes_preserve_strings_and_normalize_bools() {
        // Strings pass through unchanged and booleans normalize to `"true"`/`"false"`, matching the
        // old `V3SeriesModeValue` deserializer's accepted domain.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "use_v3_api": {
                "series": {
                    "endpoints": {
                        "https://app.datadoghq.com": "datadog_only",
                        "https://true.example.com": true,
                        "https://false.example.com": false,
                    }
                }
            },
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none(), "valid endpoint modes must not record errors");

        let modes = &config.shared.metrics_encoding.v3_series_mode.endpoint_modes;
        assert_eq!(
            modes.get("https://app.datadoghq.com").map(String::as_str),
            Some("datadog_only")
        );
        assert_eq!(modes.get("https://true.example.com").map(String::as_str), Some("true"));
        assert_eq!(
            modes.get("https://false.example.com").map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn v3_series_endpoint_mode_number_is_rejected() {
        // A numeric mode used to fail deserialization; it must now record an error rather than being
        // stringified into a valid-looking mode.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "use_v3_api": { "series": { "endpoints": { "https://app.datadoghq.com": 1 } } },
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();

        assert!(
            errors.is_some(),
            "a numeric endpoint mode must record a translation error"
        );
        assert!(
            config.shared.metrics_encoding.v3_series_mode.endpoint_modes.is_empty(),
            "an invalid mode must not be stringified into the map"
        );
    }

    #[test]
    fn v3_series_endpoint_mode_null_and_object_are_rejected() {
        // Cover other invalid JSON kinds beyond numbers: null and object values are rejected too.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "use_v3_api": {
                "series": {
                    "endpoints": {
                        "https://null.example.com": null,
                        "https://object.example.com": { "nested": true },
                    }
                }
            },
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();

        assert!(
            errors.is_some(),
            "null and object endpoint modes must record translation errors"
        );
        assert!(
            config.shared.metrics_encoding.v3_series_mode.endpoint_modes.is_empty(),
            "invalid modes must not be stringified into the map"
        );
    }

    #[test]
    fn v3_series_endpoint_modes_keep_valid_entries_when_one_is_invalid() {
        // A single bad entry must not empty the whole map: startup rejects the config on the recorded
        // error, but a runtime update keeps the valid entries while omitting the malformed one.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "use_v3_api": {
                "series": {
                    "endpoints": {
                        "https://good.example.com": "datadog_only",
                        "https://bad.example.com": 1,
                    }
                }
            },
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();

        assert!(errors.is_some(), "the invalid entry must record a translation error");

        let modes = &config.shared.metrics_encoding.v3_series_mode.endpoint_modes;
        assert_eq!(
            modes.get("https://good.example.com").map(String::as_str),
            Some("datadog_only"),
            "the valid entry must survive alongside the invalid one"
        );
        assert!(
            !modes.contains_key("https://bad.example.com"),
            "the invalid entry must be omitted from the map"
        );
    }

    #[test]
    fn run_path_schema_default_placeholder_leaves_run_path_unset() {
        // The vendored schema default for `run_path` is the literal template `${run_path}`. Saluki
        // does not expand templates, so this must not propagate; `run_path` stays empty (unset).
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "run_path": "${run_path}",
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none());
        assert_eq!(config.shared.run_path, std::path::PathBuf::new());
    }

    #[test]
    fn run_path_explicit_value_is_preserved() {
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "run_path": "/opt/datadog-agent/run",
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none());
        assert_eq!(
            config.shared.run_path,
            std::path::PathBuf::from("/opt/datadog-agent/run")
        );
    }

    #[test]
    fn run_path_whitespace_only_value_leaves_run_path_unset() {
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "run_path": "   ",
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none());
        assert_eq!(config.shared.run_path, std::path::PathBuf::new());
    }

    #[test]
    fn retry_queue_sizes_absent_when_unset() {
        // An empty source must leave both retry-queue size keys unset. The schema default for
        // `forwarder_retry_queue_payloads_max_size` is 15 MiB, but ADP owns that effective default
        // downstream in `RetryConfiguration::queue_max_size_bytes()`; if the schema default leaked
        // into the model as `Some(..)`, the deprecated-key fallback below could never fire.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({})).expect("datadog source deserializes");

        let (config, _errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        let forwarder = &config.shared.endpoints.forwarder;
        assert_eq!(forwarder.retry_queue_payloads_max_size, None);
        assert_eq!(forwarder.retry_queue_max_size, None);
    }

    #[test]
    fn retry_queue_deprecated_key_only_is_preserved() {
        // Setting only the deprecated `forwarder_retry_queue_max_size` must reach the model as
        // `Some(..)` so the getter's precedence honors it; the payloads key must stay `None`.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "forwarder_retry_queue_max_size": 5000,
        }))
        .expect("datadog source deserializes");

        let (config, _errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        let forwarder = &config.shared.endpoints.forwarder;
        assert_eq!(forwarder.retry_queue_payloads_max_size, None);
        assert_eq!(forwarder.retry_queue_max_size, Some(5000));
    }

    #[test]
    fn retry_queue_payloads_key_is_preserved() {
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "forwarder_retry_queue_payloads_max_size": 2048,
        }))
        .expect("datadog source deserializes");

        let (config, _errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert_eq!(
            config.shared.endpoints.forwarder.retry_queue_payloads_max_size,
            Some(2048)
        );
    }

    #[test]
    fn logging_defaults_come_from_the_schema() {
        // An empty source resolves the logging model to the vendored schema defaults via `drive`.
        let datadog = DatadogConfiguration::default();

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none());

        let logging = &config.control.logging;
        assert_eq!(logging.level, "info");
        assert!(!logging.format_json);
        assert!(!logging.format_rfc3339);
        assert!(logging.to_console);
        assert!(!logging.to_syslog);
        assert!(!logging.syslog_rfc);
        assert_eq!(logging.syslog_uri, "");
        // `data_plane.log_file` is flagged `saluki_overrides_default`, so an unset key stays `None`
        // rather than adopting the Agent schema's Linux literal. `LoggingConfigurationTranslator`
        // resolves `None` to the platform default path.
        assert_eq!(logging.file, None);
        assert!(!logging.disable_file_logging);
        assert_eq!(logging.file_max_rolls, 1);
        // `10Mb` parses as 10 decimal megabytes.
        assert_eq!(logging.file_max_size, 10_000_000);
    }

    #[test]
    fn data_plane_log_file_explicit_path_is_preserved() {
        // An explicitly configured `data_plane.log_file` must reach the model unchanged, even when
        // it equals the Agent's Linux default: the absence-aware source distinguishes a deliberate
        // choice (`Some`) from an unset key (`None`).
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "data_plane": { "log_file": "/var/log/datadog/agent-data-plane.log" },
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none());
        assert_eq!(
            config.control.logging.file.as_deref(),
            Some("/var/log/datadog/agent-data-plane.log")
        );
    }

    #[test]
    fn logging_values_are_driven_from_the_source() {
        // The Datadog log-file byte-size string parses into a byte count on the model; the remaining
        // logging keys copy through. `data_plane.log_file` is the per-subagent destination.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "log_level": "debug",
            "log_format_json": true,
            "log_to_console": false,
            "log_to_syslog": true,
            "syslog_rfc": true,
            "syslog_uri": "udp://127.0.0.1:1514",
            "log_file_max_size": "64kB",
            "log_file_max_rolls": 5,
            "disable_file_logging": true,
            "data_plane": { "log_file": "/tmp/adp.log" },
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(errors.is_none());

        let logging = &config.control.logging;
        assert_eq!(logging.level, "debug");
        assert!(logging.format_json);
        assert!(!logging.to_console);
        assert!(logging.to_syslog);
        assert!(logging.syslog_rfc);
        assert_eq!(logging.syslog_uri, "udp://127.0.0.1:1514");
        assert_eq!(logging.file.as_deref(), Some("/tmp/adp.log"));
        assert!(logging.disable_file_logging);
        assert_eq!(logging.file_max_rolls, 5);
        assert_eq!(logging.file_max_size, 64_000);
    }

    #[test]
    fn log_file_max_size_invalid_records_error_and_keeps_default() {
        // An unparseable byte-size string records a translation error (so a strict startup rejects
        // it) while the field keeps its seeded default (so a lenient runtime update keeps running).
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "log_file_max_size": "not-a-size",
        }))
        .expect("datadog source deserializes");

        let (config, errors) = DatadogTranslator::new(&datadog, SalukiConfiguration::default()).translate();
        assert!(
            errors.is_some(),
            "an unparseable byte size must record a translation error"
        );
        assert_eq!(config.control.logging.file_max_size, 0);
    }
}
