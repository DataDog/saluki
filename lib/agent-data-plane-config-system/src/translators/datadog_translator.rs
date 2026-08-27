//! The Datadog witness translator: [`DatadogTranslator`].
//!
//! [`DatadogTranslator`] implements [`DatadogConfigWitness`], so the generated `drive` calls each
//! `consume_<key>` exactly once with the corresponding value from `DatadogConfiguration`. Each
//! method converts the Datadog value (`i64`, `String`, `Vec<serde_json::Value>`, ...) into the
//! refined model type (`u16`, `Duration`, `PathBuf`, an enum, ...) and assigns it
//! to its `SalukiConfiguration` destination.
//!
//! Most keys assign a single field directly. The endpoint keys (`api_key`, `dd_url`, `site`,
//! `additional_endpoints`) are copied into the model without selecting a primary endpoint here.
//!
//! A key whose meaning depends on whether its value was set explicitly or set by default is typed
//! as a [`ConfigValue`] which preserves that provenance.
//!
//! Conversions that can fail (enum parsing, byte-size parsing, JSON structure parsing) record a
//! [`TranslateError`] via `record_error` and should leave the resolved value unchanged (last known
//! good). `drive` returns all recorded errors as a [`TranslateErrors`]. The exception is a setting
//! whose recovery the Agent itself defines rather than leaving to a default: follow the Agent and
//! warn, so the strict startup gate does not reject a configuration the Agent runs with.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_data_plane_config::domains::dogstatsd::{
    FilterAction, MapperProfile, MetricMapping, MetricTagFilterEntry, OriginTagCardinality,
};
use agent_data_plane_config::domains::otlp::{
    CumulativeMonotonicMode, GrpcTransport, HistogramMode, InitialCumulativeMonotonicValue, SummaryMode,
    DEFAULT_GRPC_KEEPALIVE_TIME, DEFAULT_GRPC_KEEPALIVE_TIMEOUT, DEFAULT_GRPC_MAX_RECV_MSG_SIZE_MIB,
};
use agent_data_plane_config::shared::{ForwarderHttpProtocol, V3SeriesMode};
use agent_data_plane_config::{ConfigValue, SalukiConfiguration};
use bytesize::ByteSize;
use datadog_agent_config::{
    cast_to_string, drive, DatadogConfigWitness, DatadogConfiguration, TranslateError, TranslateErrors,
};
use tracing::warn;

use crate::source::SourceTree;

/// Translates a [`DatadogConfiguration`] into a [`SalukiConfiguration`].
///
/// Construct with [`DatadogTranslator::new`] and call [`DatadogTranslator::translate`]: it drives
/// the witness over every supported Datadog key and returns the populated configuration with any
/// translation errors.
#[derive(Debug)]
pub(crate) struct DatadogTranslator<'a> {
    datadog: &'a DatadogConfiguration,
    sources: &'a SourceTree,
    config: SalukiConfiguration,
    errors: Vec<TranslateError>,
}

type Result<T> = std::result::Result<T, TranslateError>;

impl<'a> DatadogTranslator<'a> {
    /// Creates a translator that will read from `datadog`, taking provenance from the merged
    /// `sources` layer `datadog` was deserialized from.
    pub(crate) fn new(datadog: &'a DatadogConfiguration, sources: &'a SourceTree) -> Self {
        Self {
            datadog,
            sources,
            config: SalukiConfiguration::default(),
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

    /// Narrows a raw `i64` listen port to a `u16`, recording an error for a value outside that range.
    ///
    /// A port is not a quantity that can be clamped: the low end (`0`) means "do not listen" for the
    /// DogStatsD listeners, and the high end would silently move a listener to a port nobody
    /// configured.
    fn parse_port(&mut self, key: &'static str, value: i64) -> Option<u16> {
        match u16::try_from(value) {
            Ok(port) => Some(port),
            Err(_) => {
                self.record_error(TranslateError::new_with_message(
                    key,
                    "port must be between 0 and 65535",
                ));
                None
            }
        }
    }
}

/// Resolves the keepalive ping interval, applying the default for unset values.
///
/// A zero duration means "unset" and is replaced with the default of 2 hours. A nonzero value
/// below 1 second is clamped up to 1 second, matching grpc-go's minimum.
fn resolve_keepalive_time(value: Duration) -> Duration {
    if value == Duration::ZERO {
        DEFAULT_GRPC_KEEPALIVE_TIME
    } else if value < Duration::from_secs(1) {
        Duration::from_secs(1)
    } else {
        value
    }
}

/// Resolves the keepalive ping timeout, applying the default for unset values.
///
/// A zero duration means "unset" and is replaced with the default of 20 seconds.
fn resolve_keepalive_timeout(value: Duration) -> Duration {
    if value == Duration::ZERO {
        DEFAULT_GRPC_KEEPALIVE_TIMEOUT
    } else {
        value
    }
}

/// Parses a V3 series mode, warning and disabling V3 for a value the Agent cannot interpret.
///
/// The Agent's evaluator defines this setting's recovery rather than leaving it to a default: it warns
/// and routes series to the older intake. Recovering the same way keeps a typo from failing the strict
/// startup gate on a configuration the Agent runs with.
fn parse_v3_series_mode(key: &'static str, value: &str) -> V3SeriesMode {
    value.parse().unwrap_or_else(|_| {
        warn!(
            config_key = key,
            value, "Invalid V3 series mode value. Expected true, false, or datadog_only; treating as false."
        );
        V3SeriesMode::Disabled
    })
}

/// Returns `None` for an empty `s`; otherwise returns `Some(s)`.
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Returns `None` for an `s` that is empty or entirely whitespace; otherwise returns `Some(s)` with
/// the surrounding whitespace removed.
///
/// Use this for a setting whose consumer treats "absent" and "blank" alike. A value padded in a
/// configuration file or an environment variable means the same thing as one that was never set, so
/// normalizing here keeps the padding from reaching a consumer that would read it as meaningful: a
/// credential that is really blank, or a site that composes into an intake URL nothing can parse.
fn non_empty_trimmed(s: String) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parses one `dogstatsd_mapper_profiles` object into a [`MapperProfile`].
///
/// The vendored Datadog schema declares `dogstatsd_mapper_profiles` (and `metric_tag_filterlist`)
/// as arrays of free-form objects, so the generated witness can only surface them as
/// `Vec<serde_json::Value>`. This parser imposes the typed model shape at the configuration
/// boundary via a local `#[derive(Deserialize)]` shim.
fn parse_mapper_profile(key: &str, raw: serde_json::Value) -> Result<MapperProfile> {
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
        #[serde(default)]
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

/// Parses one `metric_tag_filterlist` object into a [`MetricTagFilterEntry`].
///
/// Like `parse_mapper_profile`, this imposes the typed model shape on a free-form schema object via
/// a local `#[derive(Deserialize)]` shim.
fn parse_tag_filter_entry(key: &str, raw: serde_json::Value) -> Result<MetricTagFilterEntry> {
    #[derive(serde::Deserialize)]
    struct RawEntry {
        metric_name: String,
        #[serde(default)]
        action: Option<String>,
        // The Agent decodes an entry with `mapstructure`, so an absent `tags` yields an empty list
        // and keeps the entry. Requiring it here would reject a configuration the Agent runs with.
        #[serde(default)]
        tags: Vec<String>,
    }

    let parsed: RawEntry = serde_json::from_value(raw).map_err(|error| TranslateError::new(key, error))?;
    let action = match parsed.action.as_deref() {
        Some("include") => FilterAction::Include,
        None | Some("") | Some("exclude") => FilterAction::Exclude,
        Some(other) => {
            warn!(
                action = %other,
                "`metric_tag_filterlist.*.action` should be either `include` or `exclude`; defaulting to `exclude`."
            );
            FilterAction::Exclude
        }
    };

    Ok(MetricTagFilterEntry {
        metric_name: parsed.metric_name,
        action,
        tags: parsed.tags,
    })
}

impl DatadogConfigWitness for DatadogTranslator<'_> {
    fn consume_additional_endpoints(&mut self, value: HashMap<String, Vec<String>>) {
        self.config.shared.endpoints.additional_endpoints = value;
    }

    fn consume_agent_ipc_grpc_max_message_size(&mut self, value: i64) {
        match usize::try_from(value) {
            Ok(max_message_size) => self.config.control.ipc.grpc_max_message_size = max_message_size,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "agent_ipc.grpc_max_message_size",
                "maximum message size must be greater than or equal to 0",
            )),
        }
    }

    fn consume_aggregator_stop_timeout(&mut self, value: i64) {
        // The schema explicitly says this value is denominated in seconds. We disambiguate here at
        // the earliest possible opportunity.
        match parse_seconds("aggregator_stop_timeout", value) {
            Ok(duration) => self.config.control.aggregator_stop_timeout = duration,
            Err(e) => self.record_error(e),
        }
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

    fn consume_auth_token_file_path(&mut self, value: String) {
        self.config.control.ipc.auth_token_file_path = PathBuf::from(value);
    }

    fn consume_autoscaling_failover_enabled(&mut self, value: bool) {
        self.config.shared.autoscaling_failover.enabled = value;
    }

    fn consume_autoscaling_failover_metrics(&mut self, value: Vec<String>) {
        self.config.shared.autoscaling_failover.metrics = value;
    }

    fn consume_basic_telemetry_add_container_tags(&mut self, value: bool) {
        self.config.shared.basic_telemetry.add_container_tags = value;
    }

    fn consume_bind_host(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.bind_host = non_empty(value);
    }

    fn consume_cluster_agent_auth_token(&mut self, value: String) {
        self.config.shared.cluster_agent.auth_token = non_empty_trimmed(value);
    }

    fn consume_cluster_agent_enabled(&mut self, value: bool) {
        self.config.shared.cluster_agent.enabled = value;
    }

    fn consume_cluster_agent_kubernetes_service_name(&mut self, value: String) {
        // An empty name is meaningful here: it turns Kubernetes service discovery off, so the value is trimmed but
        // kept rather than collapsed into the schema default.
        self.config.shared.cluster_agent.kubernetes_service_name = value.trim().to_string();
    }

    fn consume_cluster_agent_url(&mut self, value: String) {
        self.config.shared.cluster_agent.url = non_empty_trimmed(value);
    }

    fn consume_cluster_name(&mut self, value: String) {
        self.config.shared.static_tags.cluster_name = value;
    }

    fn consume_cmd_port(&mut self, value: i64) {
        if let Some(port) = self.parse_port("cmd_port", value) {
            self.config.control.ipc.cmd_port = port;
        }
    }

    fn consume_cri_connection_timeout(&mut self, value: i64) {
        self.config.control.ipc.cri_connection_timeout = value;
    }

    fn consume_cri_query_timeout(&mut self, value: i64) {
        self.config.control.ipc.cri_query_timeout = value;
    }

    fn consume_data_plane_api_listen_address(&mut self, value: String) {
        self.config.control.api_listen_address = value;
    }

    fn consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(&mut self, value: i64) {
        match usize::try_from(value) {
            Ok(capacity) => {
                self.config
                    .domains
                    .dogstatsd
                    .aggregation
                    .aggregator_tag_filter_cache_capacity = capacity;
            }
            Err(_) => self.record_error(TranslateError::new_with_message(
                "data_plane.dogstatsd.aggregator_tag_filter_cache_capacity",
                format!("tag filter cache capacity must be greater than or equal to 0, got {value}"),
            )),
        }
    }

    fn consume_data_plane_dogstatsd_enabled(&mut self, value: bool) {
        self.config.control.dogstatsd = value;
    }

    fn consume_data_plane_enabled(&mut self, value: bool) {
        self.config.control.enabled = value;
    }

    fn consume_data_plane_log_file(&mut self, value: String) {
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
        self.config.control.secure_api_listen_address = value;
    }

    fn consume_data_plane_use_new_config_stream_endpoint(&mut self, value: bool) {
        self.config.control.use_new_config_stream_endpoint = value;
    }

    fn consume_dd_url(&mut self, value: String) {
        // The Agent may send its default URL even when the operator configured only `site`; retain
        // the value and use provenance to decide whether it overrides `site`. Programmatic
        // overrides via `EndpointConfiguration::set_dd_url` bypass this translator.
        let provenance = self.sources.provenance("dd_url");
        self.config.shared.endpoints.dd_url = ConfigValue::new(value, provenance);
    }

    fn consume_disable_file_logging(&mut self, value: bool) {
        self.config.control.logging.disable_file_logging = value;
    }

    fn consume_dogstatsd_buffer_size(&mut self, value: i64) {
        // A negative buffer size is invalid and must be rejected.
        match usize::try_from(value) {
            Ok(buffer_size) => self.config.domains.dogstatsd.listeners.buffer_size = buffer_size,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "dogstatsd_buffer_size",
                "buffer size must be greater than or equal to 0",
            )),
        }
    }

    fn consume_dogstatsd_capture_depth(&mut self, value: i64) {
        self.config.domains.dogstatsd.listeners.capture_depth = value.max(0) as usize;
    }

    fn consume_dogstatsd_capture_path(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.capture_path = PathBuf::from(value);
    }

    fn consume_dogstatsd_context_expiry_seconds(&mut self, value: i64) {
        // A negative expiry is invalid and must be rejected.
        match u64::try_from(value) {
            Ok(expiry_seconds) => self.config.domains.dogstatsd.aggregation.context_expiry_seconds = expiry_seconds,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "dogstatsd_context_expiry_seconds",
                "context expiry seconds must be greater than or equal to 0",
            )),
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
        if !value.is_empty() {
            self.config.domains.dogstatsd.debug_log.log_file = Some(PathBuf::from(value));
        }
    }

    fn consume_dogstatsd_log_file_max_rolls(&mut self, value: i64) {
        match usize::try_from(value) {
            Ok(max_rolls) => self.config.domains.dogstatsd.debug_log.log_file_max_rolls = max_rolls,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "dogstatsd_log_file_max_rolls",
                "log file max rolls must be greater than or equal to 0",
            )),
        }
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
        match usize::try_from(value) {
            Ok(cache_size) => self.config.domains.dogstatsd.mapper.cache_size = cache_size,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "dogstatsd_mapper_cache_size",
                "mapper cache size must be greater than or equal to 0",
            )),
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
        if let Some(port) = self.parse_port("dogstatsd_port", value) {
            self.config.domains.dogstatsd.listeners.port = port;
        }
    }

    fn consume_dogstatsd_so_rcvbuf(&mut self, value: i64) {
        // A negative receive buffer size is invalid and must be rejected.
        match usize::try_from(value) {
            Ok(so_rcvbuf) => self.config.domains.dogstatsd.listeners.so_rcvbuf = so_rcvbuf,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "dogstatsd_so_rcvbuf",
                "socket receive buffer size must be greater than or equal to 0",
            )),
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
        self.config.domains.dogstatsd.contexts.string_interner_size = value.max(0) as u64;
    }

    fn consume_dogstatsd_tag_cardinality(&mut self, value: String) {
        match value.parse::<OriginTagCardinality>() {
            Ok(cardinality) => self.config.domains.dogstatsd.origin.tag_cardinality = cardinality,
            Err(error) => self.record_error(TranslateError::new("dogstatsd_tag_cardinality", error)),
        }
    }

    fn consume_dogstatsd_tags(&mut self, value: Vec<String>) {
        self.config.domains.dogstatsd.tags = value;
    }

    fn consume_dogstatsd_windows_pipe_security_descriptor(&mut self, value: String) {
        self.config.domains.dogstatsd.listeners.windows_pipe_security_descriptor = value;
    }

    fn consume_dogstatsd_workers_count(&mut self, value: i64) {
        match usize::try_from(value) {
            Ok(worker_count) => self.config.domains.dogstatsd.listeners.workers_count = worker_count,
            Err(_) => self.record_error(TranslateError::new_with_message(
                "dogstatsd_workers_count",
                "worker count must be greater than or equal to 0",
            )),
        }
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

    fn consume_eks_fargate(&mut self, value: bool) {
        self.config.shared.static_tags.eks_fargate = value;
    }

    fn consume_extra_tags(&mut self, value: Vec<String>) {
        self.config.shared.tags.extra_tags = value;
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

    fn consume_forwarder_retry_queue_max_size(&mut self, value: i64) {
        // This deprecated key's schema default is `0`, which is also a value an operator can set, so
        // only provenance distinguishes the two.
        let provenance = self.sources.provenance("forwarder_retry_queue_max_size");
        self.config.shared.endpoints.forwarder.retry_queue_max_size = ConfigValue::new(value.max(0) as u64, provenance);
    }

    fn consume_forwarder_retry_queue_payloads_max_size(&mut self, value: i64) {
        let provenance = self.sources.provenance("forwarder_retry_queue_payloads_max_size");
        self.config.shared.endpoints.forwarder.retry_queue_payloads_max_size =
            ConfigValue::new(value.max(0) as u64, provenance);
    }

    fn consume_forwarder_stop_timeout(&mut self, value: i64) {
        // The schema explicitly says this value is denominated in seconds. We disambiguate here at
        // the earliest possible opportunity.
        match parse_seconds("forwarder_stop_timeout", value) {
            Ok(duration) => self.config.shared.endpoints.forwarder.stop_timeout = duration,
            Err(e) => self.record_error(e),
        }
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

    fn consume_ipc_cert_file_path(&mut self, value: String) {
        self.config.control.ipc.ipc_cert_file_path = PathBuf::from(value);
    }

    fn consume_kubernetes_kubelet_nodename(&mut self, value: String) {
        self.config.shared.static_tags.kubernetes_kubelet_nodename = value;
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
        // A non-empty current filterlist takes precedence over the legacy blocklist.
        if !self.datadog.metric_filterlist.is_empty() {
            self.config.domains.dogstatsd.metric_filter.values = value;
        }
    }

    fn consume_metric_filterlist_match_prefix(&mut self, value: bool) {
        if !self.datadog.metric_filterlist.is_empty() {
            self.config.domains.dogstatsd.metric_filter.match_prefix = value;
        }
    }

    fn consume_metric_tag_filterlist(&mut self, value: Vec<serde_json::Value>) {
        let mut entries = Vec::with_capacity(value.len());
        for raw in value {
            match parse_tag_filter_entry("metric_tag_filterlist", raw) {
                Ok(entry) => entries.push(entry),
                Err(error) => self.record_error(error),
            }
        }
        self.config.domains.dogstatsd.tag_filterlist = entries;
    }

    fn consume_min_tls_version(&mut self, value: String) {
        self.config.shared.endpoints.tls.min_tls_version = value;
    }

    fn consume_tls_handshake_timeout(&mut self, value: Duration) {
        self.config.shared.endpoints.tls.handshake_timeout = value;
    }

    fn consume_multi_region_failover_api_key(&mut self, value: String) {
        self.config.domains.multi_region_failover.api_key = non_empty_trimmed(value);
    }

    fn consume_multi_region_failover_dd_url(&mut self, value: String) {
        self.config.domains.multi_region_failover.dd_url = non_empty_trimmed(value);
    }

    fn consume_multi_region_failover_enabled(&mut self, value: bool) {
        self.config.domains.multi_region_failover.enabled = value;
    }

    fn consume_multi_region_failover_failover_metrics(&mut self, value: bool) {
        self.config.domains.multi_region_failover.metric_mirroring.enabled = value;
    }

    fn consume_multi_region_failover_metric_allowlist(&mut self, value: Vec<String>) {
        self.config.domains.multi_region_failover.metric_mirroring.allowlist = value;
    }

    fn consume_multi_region_failover_site(&mut self, value: String) {
        self.config.domains.multi_region_failover.site = non_empty_trimmed(value);
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

    fn consume_otlp_config_logs_enabled(&mut self, value: bool) {
        self.config.domains.otlp.receiver.logs_enabled = value;
    }

    fn consume_otlp_config_metrics_delta_ttl(&mut self, value: i64) {
        if value <= 0 {
            self.record_error(TranslateError::new_with_message(
                "otlp_config.metrics.delta_ttl",
                format!("time to live must be positive: {value}"),
            ));
            return;
        }
        match parse_seconds("otlp_config.metrics.delta_ttl", value) {
            Ok(ttl) => self.config.domains.otlp.metrics.delta_ttl = ttl,
            Err(error) => self.record_error(error),
        }
    }

    fn consume_otlp_config_metrics_enabled(&mut self, value: bool) {
        self.config.domains.otlp.receiver.metrics_enabled = value;
    }

    fn consume_otlp_config_metrics_histograms_mode(&mut self, value: String) {
        match value.parse::<HistogramMode>() {
            Ok(mode) => self.config.domains.otlp.metrics.histogram_mode = mode,
            Err(error) => self.record_error(TranslateError::new("otlp_config.metrics.histograms.mode", error)),
        }
    }

    fn consume_otlp_config_metrics_histograms_send_aggregation_metrics(&mut self, value: bool) {
        self.config.domains.otlp.metrics.send_histogram_aggregations = value;
    }

    fn consume_otlp_config_metrics_resource_attributes_as_tags(&mut self, value: bool) {
        self.config.domains.otlp.metrics.resource_attributes_as_tags = value;
    }

    fn consume_otlp_config_metrics_instrumentation_scope_metadata_as_tags(&mut self, value: bool) {
        self.config.domains.otlp.metrics.instrumentation_scope_metadata_as_tags = value;
    }

    fn consume_otlp_config_metrics_sums_cumulative_monotonic_mode(&mut self, value: String) {
        match value.parse::<CumulativeMonotonicMode>() {
            Ok(mode) => self.config.domains.otlp.metrics.sums.cumulative_monotonic_mode = mode,
            Err(error) => self.record_error(TranslateError::new(
                "otlp_config.metrics.sums.cumulative_monotonic_mode",
                error,
            )),
        }
    }

    fn consume_otlp_config_metrics_sums_initial_cumulative_monotonic_value(&mut self, value: String) {
        match value.parse::<InitialCumulativeMonotonicValue>() {
            Ok(mode) => self.config.domains.otlp.metrics.sums.initial_cumulative_monotonic_value = mode,
            Err(error) => self.record_error(TranslateError::new(
                "otlp_config.metrics.sums.initial_cumulative_monotonic_value",
                error,
            )),
        }
    }

    fn consume_otlp_config_metrics_summaries_mode(&mut self, value: String) {
        match value.parse::<SummaryMode>() {
            Ok(mode) => self.config.domains.otlp.metrics.summaries.mode = mode,
            Err(error) => self.record_error(TranslateError::new("otlp_config.metrics.summaries.mode", error)),
        }
    }

    fn consume_otlp_config_metrics_tag_cardinality(&mut self, value: String) {
        match value.parse::<OriginTagCardinality>() {
            Ok(cardinality) => self.config.domains.otlp.metrics.tag_cardinality = cardinality,
            Err(error) => self.record_error(TranslateError::new("otlp_config.metrics.tag_cardinality", error)),
        }
    }

    fn consume_otlp_config_metrics_tags(&mut self, value: String) {
        self.config.domains.otlp.metrics.tags = value;
    }

    fn consume_otlp_config_receiver_protocols_grpc_endpoint(&mut self, value: String) {
        self.config.domains.otlp.receiver.grpc.endpoint = value;
    }

    fn consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(&mut self, value: i64) {
        // A configured `0` selects grpc-go's built-in limit; carry the effective value in the model.
        let mib = value.max(0) as u64;
        self.config.domains.otlp.receiver.grpc.max_recv_msg_size_mib = if mib == 0 {
            DEFAULT_GRPC_MAX_RECV_MSG_SIZE_MIB
        } else {
            mib
        };
    }

    fn consume_otlp_config_receiver_protocols_grpc_transport(&mut self, value: String) {
        match value.parse::<GrpcTransport>() {
            Ok(transport) => self.config.domains.otlp.receiver.grpc.transport = transport,
            Err(error) => self.record_error(TranslateError::new(
                "otlp_config.receiver.protocols.grpc.transport",
                error,
            )),
        }
    }

    fn consume_otlp_config_receiver_protocols_grpc_keepalive_server_parameters_max_connection_age(
        &mut self, value: std::time::Duration,
    ) {
        self.config.domains.otlp.receiver.grpc.keepalive.max_connection_age = value;
    }

    fn consume_otlp_config_receiver_protocols_grpc_keepalive_server_parameters_max_connection_age_grace(
        &mut self, value: std::time::Duration,
    ) {
        self.config
            .domains
            .otlp
            .receiver
            .grpc
            .keepalive
            .max_connection_age_grace = value;
    }

    fn consume_otlp_config_receiver_protocols_grpc_keepalive_server_parameters_time(
        &mut self, value: std::time::Duration,
    ) {
        self.config.domains.otlp.receiver.grpc.keepalive.time = resolve_keepalive_time(value);
    }

    fn consume_otlp_config_receiver_protocols_grpc_keepalive_server_parameters_timeout(
        &mut self, value: std::time::Duration,
    ) {
        self.config.domains.otlp.receiver.grpc.keepalive.timeout = resolve_keepalive_timeout(value);
    }

    fn consume_otlp_config_receiver_protocols_grpc_tls_ca_file(&mut self, value: Option<String>) {
        self.config.domains.otlp.receiver.grpc.tls.ca_file = value.unwrap_or_default();
    }

    fn consume_otlp_config_receiver_protocols_grpc_tls_cert_file(&mut self, value: Option<String>) {
        self.config.domains.otlp.receiver.grpc.tls.cert_file = value.unwrap_or_default();
    }

    fn consume_otlp_config_receiver_protocols_grpc_tls_key_file(&mut self, value: Option<String>) {
        self.config.domains.otlp.receiver.grpc.tls.key_file = value.unwrap_or_default();
    }

    fn consume_otlp_config_receiver_protocols_http_endpoint(&mut self, value: String) {
        self.config.domains.otlp.receiver.http.endpoint = value;
    }

    fn consume_otlp_config_receiver_protocols_http_cors_allowed_headers(&mut self, value: Vec<String>) {
        self.config.domains.otlp.receiver.http.cors.allowed_headers = value;
    }

    fn consume_otlp_config_receiver_protocols_http_cors_allowed_origins(&mut self, value: Vec<String>) {
        self.config.domains.otlp.receiver.http.cors.allowed_origins = value;
    }

    fn consume_otlp_config_receiver_protocols_http_cors_exposed_headers(&mut self, value: Vec<String>) {
        self.config.domains.otlp.receiver.http.cors.exposed_headers = value;
    }

    fn consume_otlp_config_receiver_protocols_http_cors_max_age(&mut self, value: Option<i64>) {
        if let Some(v) = value {
            match u64::try_from(v) {
                Ok(max_age) => self.config.domains.otlp.receiver.http.cors.max_age = max_age,
                Err(error) => self.record_error(TranslateError::new(
                    "otlp_config.receiver.protocols.http.cors.max_age",
                    error,
                )),
            }
        }
    }

    fn consume_otlp_config_receiver_protocols_http_tls_ca_file(&mut self, value: Option<String>) {
        self.config.domains.otlp.receiver.http.tls.ca_file = value.unwrap_or_default();
    }

    fn consume_otlp_config_receiver_protocols_http_tls_cert_file(&mut self, value: Option<String>) {
        self.config.domains.otlp.receiver.http.tls.cert_file = value.unwrap_or_default();
    }

    fn consume_otlp_config_receiver_protocols_http_tls_key_file(&mut self, value: Option<String>) {
        self.config.domains.otlp.receiver.http.tls.key_file = value.unwrap_or_default();
    }

    fn consume_otlp_config_traces_enabled(&mut self, value: bool) {
        self.config.domains.otlp.traces.enabled = value;
    }

    fn consume_otlp_config_traces_internal_port(&mut self, value: i64) {
        match u16::try_from(value) {
            Ok(port) => self.config.domains.otlp.traces.internal_port = port,
            Err(error) => self.record_error(TranslateError::new("otlp_config.traces.internal_port", error)),
        }
    }

    fn consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(&mut self, value: f64) {
        self.config
            .domains
            .otlp
            .traces
            .probabilistic_sampler_sampling_percentage = value;
    }

    fn consume_provider_kind(&mut self, value: String) {
        self.config.shared.static_tags.provider_kind = value;
    }

    fn consume_tags(&mut self, value: Vec<String>) {
        self.config.shared.tags.tags = value;
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
        // In the vendored schema, run_path is defaulted to the placeholder ${run_path}. Though it
        // seems highly unlikely this could slip through to ADP, we check for it, warn and treat
        // the value as unset.
        //
        // Note that for this config, we do not care whether provenance is explicit or default. We
        // just want to know whether we have a value or not.
        if value == "${run_path}" {
            warn!("`run_path` contains the unresolved schema placeholder '${{run_path}}'. Treating it as unset.");
            self.config.shared.run_path = None;
        } else if value.is_empty() {
            self.config.shared.run_path = None;
        } else {
            self.config.shared.run_path = Some(PathBuf::from(value))
        }
    }

    fn consume_serializer_compressor_kind(&mut self, value: String) {
        self.config.shared.endpoints.compression.compressor_kind = value;
    }

    fn consume_serializer_experimental_use_v3_api_compression_level(&mut self, value: i64) {
        self.config.shared.metrics_encoding.v3_api.compression_level = value as i32;
    }

    fn consume_serializer_experimental_use_v3_api_series_endpoints(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.v3_api.series.endpoints = value;
    }

    fn consume_serializer_experimental_use_v3_api_sketches_endpoints(&mut self, value: Vec<String>) {
        self.config.shared.metrics_encoding.v3_api.sketches.endpoints = value;
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
        // ADP keeps its own default of 3 unless an operator asked for the Agent's level, and only
        // provenance separates a configured 1 from the schema default of 1, because drive delivers the
        // key either way. `Compression::zstd_compressor_level` applies the precedence.
        let provenance = self.sources.provenance("serializer_zstd_compressor_level");
        match i32::try_from(value) {
            Ok(value) => {
                self.config.shared.endpoints.compression.agent_zstd_level = ConfigValue::new(value, provenance);
            }
            Err(error) => self.record_error(TranslateError::new("serializer_zstd_compressor_level", error)),
        }
    }

    fn consume_site(&mut self, value: String) {
        let provenance = self.sources.provenance("site");
        self.config.shared.endpoints.site = ConfigValue::new(value, provenance);
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
        if let Some(port) = self.parse_port("statsd_forward_port", value) {
            self.config.domains.dogstatsd.listeners.forward_port = port;
        }
    }

    fn consume_statsd_metric_blocklist(&mut self, value: Vec<String>) {
        // The legacy blocklist is effective only when the current filterlist is empty.
        if self.datadog.metric_filterlist.is_empty() {
            self.config.domains.dogstatsd.metric_filter.values = value;
        }
    }

    fn consume_statsd_metric_blocklist_match_prefix(&mut self, value: bool) {
        if self.datadog.metric_filterlist.is_empty() {
            self.config.domains.dogstatsd.metric_filter.match_prefix = value;
        }
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
        self.config.shared.metrics_encoding.v3_series_mode = parse_v3_series_mode("use_v3_api.series.enabled", &value);
    }

    fn consume_use_v3_api_series_endpoints(&mut self, value: ::serde_json::Map<String, ::serde_json::Value>) {
        // This key arrives as raw JSON, so each mode is rendered the way the Agent's own string cast
        // renders it before it is parsed: a boolean, an integer, `1.0`, and a null all reach the
        // parser as the Agent reads them.
        let mut modes: HashMap<String, V3SeriesMode> = HashMap::with_capacity(value.len());
        for (endpoint, mode) in value {
            match cast_to_string(&mode) {
                Ok(rendered) => {
                    modes.insert(endpoint, parse_v3_series_mode("use_v3_api.series.endpoints", &rendered));
                }
                Err(reason) => {
                    self.record_error(TranslateError::new_with_message("use_v3_api.series.endpoints", reason))
                }
            }
        }

        self.config.shared.metrics_encoding.v3_series_endpoint_modes = modes;
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

/// A helper to parse values in the schema that are denominated in seconds (per documentation) but
/// represented as i64 values.
fn parse_seconds(key: &str, value: i64) -> Result<Duration> {
    let seconds =
        u64::try_from(value).map_err(|e| TranslateError::new_with_context(key, "invalid duration seconds value", e))?;
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::path::PathBuf;
    use std::time::Duration;

    use agent_data_plane_config::defaults::DEFAULT_ZSTD_COMPRESSOR_LEVEL;
    use agent_data_plane_config::domains::{
        dogstatsd::{MetricFilter, OriginTagCardinality},
        otlp::{
            CumulativeMonotonicMode, InitialCumulativeMonotonicValue, SummaryMode, DEFAULT_DELTA_TTL,
            DEFAULT_GRPC_MAX_RECV_MSG_SIZE_MIB,
        },
    };
    use agent_data_plane_config::shared::V3SeriesMode;
    use agent_data_plane_config::{ConfigValue, SalukiConfiguration};
    use datadog_agent_config::{DatadogConfiguration, TranslateErrors};
    use saluki_config::dynamic::{ConfigSetting, Provenance as StreamProvenance};
    use serde_json::{json, Value};

    use super::DatadogTranslator;
    use crate::saluki_only::SalukiOnly;
    use crate::source::SourceTree;

    /// Translates `sources`, treating every value it supplies as one an input set explicitly.
    ///
    /// This is the local file and environment: a value is present only because someone set it.
    fn translate_explicit(sources: Value) -> (SalukiConfiguration, Option<TranslateErrors>) {
        translate_sources(&SourceTree::all_explicit(sources))
    }

    /// Translates a configuration producer's `(key, value, provenance)` settings.
    ///
    /// Use this when a test turns on whether the producer was given a value or supplied its own
    /// default, which is what the Datadog Agent's configuration stream distinguishes.
    fn translate_stream(
        settings: &[(&str, Value, StreamProvenance)],
    ) -> (SalukiConfiguration, Option<TranslateErrors>) {
        let settings: Vec<_> = settings
            .iter()
            .map(|(key, value, provenance)| ConfigSetting::new(*key, value.clone(), *provenance))
            .collect();

        translate_sources(&SourceTree::from_settings(&settings))
    }

    /// Asserts that `actual` was set explicitly and holds `expected`.
    #[track_caller]
    fn assert_explicit<T, U>(actual: &ConfigValue<T>, expected: U)
    where
        T: fmt::Debug + PartialEq<U>,
        U: fmt::Debug,
    {
        assert!(actual.is_explicit(), "{actual:?} should be explicit");
        assert_eq!(actual.value, expected);
    }

    /// Asserts that nothing set `actual` and that it holds `expected` as its default.
    #[track_caller]
    fn assert_defaulted<T, U>(actual: &ConfigValue<T>, expected: U)
    where
        T: fmt::Debug + PartialEq<U>,
        U: fmt::Debug,
    {
        assert!(!actual.is_explicit(), "{actual:?} should be a default");
        assert_eq!(actual.value, expected);
    }

    /// Deserializes the Datadog source model from `sources` and drives the witness over it.
    fn translate_sources(sources: &SourceTree) -> (SalukiConfiguration, Option<TranslateErrors>) {
        let datadog: DatadogConfiguration =
            serde_json::from_value(sources.to_value()).expect("datadog source deserializes");

        DatadogTranslator::new(&datadog, sources).translate()
    }

    #[test]
    fn translate_small_map_through_witness_and_seed() {
        // A small raw Datadog source map exercising a scalar conversion, an enum parse, a
        // duration parse, and the raw endpoint inputs.
        let (mut config, errors) = translate_explicit(json!({
            "api_key": "abc",
            "dd_url": "https://custom.example.com",
            "dogstatsd_port": 9125,
            "dogstatsd_workers_count": 3,
            "dogstatsd_tag_cardinality": "high",
            "otlp_config": { "metrics": { "tag_cardinality": "orchestrator" } },
            "expected_tags_duration": "15s",
            "telemetry": { "dogstatsd_origin": true },
        }));

        // A small Saluki-only source setting one seeded field.
        let saluki_only: SalukiOnly = serde_json::from_value(json!({
            "dogstatsd_tcp_port": 8126,
        }))
        .expect("saluki-only source deserializes");
        saluki_only.seed(&mut config);
        assert!(errors.is_none());

        // Driven scalar conversion: i64 -> u16.
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
        assert_eq!(config.domains.dogstatsd.listeners.workers_count, 3);
        // Driven enum parse.
        assert_eq!(
            config.domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
        assert_eq!(
            config.domains.otlp.metrics.tag_cardinality,
            OriginTagCardinality::Orchestrator
        );
        // Driven `format: duration` parse: a Go duration string becomes a `Duration`.
        assert_eq!(config.shared.tags.expected_tags_duration, Duration::from_secs(15));
        // Driven bool in a nested Datadog section.
        assert!(config.domains.dogstatsd.telemetry.origin_breakdown);
        // Raw endpoint inputs: carried through without selecting a primary endpoint here.
        assert_eq!(config.shared.endpoints.api_key, "abc");
        assert_explicit(&config.shared.endpoints.dd_url, "https://custom.example.com");
        // Seeded Saluki-only field.
        assert_eq!(config.domains.dogstatsd.listeners.tcp_port, 8126);
    }

    #[test]
    fn basic_telemetry_container_tags_default_and_translation() {
        let defaulted: DatadogConfiguration = serde_json::from_value(json!({})).expect("datadog source deserializes");
        assert_eq!(
            serde_json::to_value(&defaulted).expect("source serializes")["basic_telemetry_add_container_tags"],
            false
        );

        let (config, errors) = translate_explicit(json!({}));
        assert!(errors.is_none());
        assert_eq!(
            serde_json::to_value(&config).expect("typed configuration serializes")["shared"]["basic_telemetry"]
                ["add_container_tags"],
            false
        );

        let (config, errors) = translate_explicit(json!({
            "basic_telemetry_add_container_tags": true,
        }));
        assert!(errors.is_none());
        assert_eq!(
            serde_json::to_value(&config).expect("typed configuration serializes")["shared"]["basic_telemetry"]
                ["add_container_tags"],
            true
        );
    }

    #[test]
    fn negative_dogstatsd_workers_count_records_translation_error() {
        let (config, errors) = translate_explicit(json!({
            "dogstatsd_workers_count": -1,
        }));

        assert_eq!(config.domains.dogstatsd.listeners.workers_count, 0);
        let errors = errors.expect("negative worker count should record a translation error");
        assert!(errors.to_string().contains("dogstatsd_workers_count"));
        assert!(errors.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn negative_dogstatsd_buffer_size_records_translation_error() {
        let (config, errors) = translate_explicit(json!({
            "dogstatsd_buffer_size": -1,
        }));

        // The invalid size is not applied, and the recorded error fails the strict startup gate rather
        // than leaving the source with a buffer that truncates every payload.
        assert_eq!(config.domains.dogstatsd.listeners.buffer_size, 0);
        let errors = errors.expect("negative buffer size should record a translation error");
        assert!(errors.to_string().contains("dogstatsd_buffer_size"));
        assert!(errors.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn negative_dogstatsd_context_expiry_seconds_records_translation_error() {
        let (_, errors) = translate_explicit(json!({
            "dogstatsd_context_expiry_seconds": -1,
        }));

        let errors = errors.expect("a negative context expiry should record a translation error");
        assert!(errors.to_string().contains("dogstatsd_context_expiry_seconds"));
        assert!(errors.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn negative_dogstatsd_so_rcvbuf_records_translation_error() {
        let (config, errors) = translate_explicit(json!({
            "dogstatsd_so_rcvbuf": -1,
        }));

        // Zero is a meaningful value here, so the invalid size must not silently select the OS default.
        assert_eq!(config.domains.dogstatsd.listeners.so_rcvbuf, 0);
        let errors = errors.expect("a negative socket receive buffer size should record a translation error");
        assert!(errors.to_string().contains("dogstatsd_so_rcvbuf"));
        assert!(errors.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn out_of_range_dogstatsd_ports_record_translation_errors() {
        for (key, value) in [
            ("dogstatsd_port", -1),
            ("dogstatsd_port", 70_000),
            ("statsd_forward_port", -1),
            ("statsd_forward_port", 70_000),
        ] {
            let (_, errors) = translate_explicit(json!({ key: value }));

            let errors = errors.unwrap_or_else(|| panic!("{key} = {value} should record a translation error"));
            assert!(errors.to_string().contains(key));
            assert!(errors.to_string().contains("between 0 and 65535"));
        }
    }

    #[test]
    fn dogstatsd_tag_cardinality_accepts_the_agent_spellings() {
        for (value, expected) in [
            ("low", OriginTagCardinality::Low),
            ("orch", OriginTagCardinality::Orchestrator),
            ("ORCHESTRATOR", OriginTagCardinality::Orchestrator),
            ("High", OriginTagCardinality::High),
            ("none", OriginTagCardinality::None),
        ] {
            let (config, errors) = translate_explicit(json!({ "dogstatsd_tag_cardinality": value }));

            assert!(errors.is_none(), "`{value}` should translate without error");
            assert_eq!(config.domains.dogstatsd.origin.tag_cardinality, expected);
        }

        let (_, errors) = translate_explicit(json!({ "dogstatsd_tag_cardinality": "orchestral" }));
        let errors = errors.expect("an unknown cardinality should record a translation error");
        assert!(errors.to_string().contains("dogstatsd_tag_cardinality"));
    }

    #[test]
    fn empty_dogstatsd_listener_paths_translate_to_unset() {
        // The source treats a blank path or host the same as an absent one, so translation must not hand
        // it an empty string to bind or forward to.
        let (config, errors) = translate_explicit(json!({
            "bind_host": "",
            "dogstatsd_pipe_name": "",
            "dogstatsd_socket": "",
            "dogstatsd_stream_socket": "",
            "statsd_forward_host": "",
        }));

        assert!(errors.is_none());
        let listeners = &config.domains.dogstatsd.listeners;
        assert_eq!(listeners.bind_host, None);
        assert_eq!(listeners.pipe_name, None);
        assert_eq!(listeners.socket, None);
        assert_eq!(listeners.stream_socket, None);
        assert_eq!(listeners.forward_host, None);
    }

    #[test]
    fn dogstatsd_debug_log_configuration_translates() {
        let (config, errors) = translate_explicit(json!({}));
        assert!(errors.is_none());
        let debug_log = &config.domains.dogstatsd.debug_log;
        assert!(debug_log.logging_enabled);
        assert!(debug_log.log_file.is_none());
        assert_eq!(debug_log.log_file_max_rolls, 3);
        assert_eq!(debug_log.log_file_max_size, 10_000_000);
        assert!(!debug_log.metrics_stats_enable);

        let (config, errors) = translate_explicit(json!({
            "dogstatsd_log_file": "/tmp/dsd-debug.log",
            "dogstatsd_log_file_max_rolls": 0,
            "dogstatsd_log_file_max_size": "42MB",
            "dogstatsd_logging_enabled": false,
            "dogstatsd_metrics_stats_enable": true,
        }));
        assert!(errors.is_none());
        let debug_log = &config.domains.dogstatsd.debug_log;
        assert_eq!(debug_log.log_file, Some(std::path::PathBuf::from("/tmp/dsd-debug.log")));
        assert_eq!(debug_log.log_file_max_rolls, 0);
        assert_eq!(debug_log.log_file_max_size, 42_000_000);
        assert!(!debug_log.logging_enabled);
        assert!(debug_log.metrics_stats_enable);
    }

    #[test]
    fn negative_dogstatsd_log_file_max_rolls_records_translation_error() {
        let (config, errors) = translate_explicit(json!({ "dogstatsd_log_file_max_rolls": -1 }));

        assert_eq!(config.domains.dogstatsd.debug_log.log_file_max_rolls, 0);
        let error = errors.expect("negative log file max rolls should record an error");
        assert!(error.to_string().contains("dogstatsd_log_file_max_rolls"));
        assert!(error.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn negative_dogstatsd_mapper_cache_size_records_translation_error() {
        let (config, errors) = translate_explicit(json!({ "dogstatsd_mapper_cache_size": -1 }));

        assert_eq!(config.domains.dogstatsd.mapper.cache_size, 0);
        let error = errors.expect("a negative mapper cache size should record an error");
        assert!(error.to_string().contains("dogstatsd_mapper_cache_size"));
        assert!(error.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn dogstatsd_mapper_profiles_translate_from_a_sequence_or_json_string() {
        let profiles = json!([{
            "name": "workers",
            "prefix": "worker.",
            "mappings": [{
                "match": "worker.*",
                "match_type": "wildcard",
                "name": "worker",
                "tags": { "worker_name": "$1" }
            }]
        }]);

        for source in [profiles.clone(), Value::String(profiles.to_string())] {
            let (config, errors) = translate_explicit(json!({ "dogstatsd_mapper_profiles": source }));

            assert!(errors.is_none());
            let profile = &config.domains.dogstatsd.mapper.profiles[0];
            assert_eq!(profile.name, "workers");
            assert_eq!(profile.prefix, "worker.");
            let mapping = &profile.mappings[0];
            assert_eq!(mapping.metric_match, "worker.*");
            assert_eq!(mapping.match_type, "wildcard");
            assert_eq!(mapping.name, "worker");
            assert_eq!(mapping.tags["worker_name"], "$1");
        }
    }

    #[test]
    fn malformed_dogstatsd_mapper_profiles_record_translation_errors() {
        for profile in [
            json!({ "prefix": "worker.", "mappings": [] }),
            json!({ "name": "workers", "mappings": [] }),
            json!({
                "name": "workers",
                "prefix": "worker.",
                "mappings": [{ "match": "worker.*" }]
            }),
        ] {
            let (config, errors) = translate_explicit(json!({ "dogstatsd_mapper_profiles": [profile] }));

            assert!(config.domains.dogstatsd.mapper.profiles.is_empty());
            let error = errors.expect("a malformed mapper profile should record an error");
            assert!(error.to_string().contains("dogstatsd_mapper_profiles"));
        }
    }

    #[test]
    fn mapper_profile_without_mappings_is_accepted() {
        let (config, errors) = translate_explicit(json!({
            "dogstatsd_mapper_profiles": [{ "name": "workers", "prefix": "worker." }]
        }));

        assert!(errors.is_none());
        assert_eq!(config.domains.dogstatsd.mapper.profiles.len(), 1);
        assert!(config.domains.dogstatsd.mapper.profiles[0].mappings.is_empty());
    }

    #[test]
    fn v3_series_modes_translate_from_every_form_the_agent_accepts() {
        let (config, errors) = translate_explicit(json!({
            "use_v3_api": {
                "series": {
                    "enabled": true,
                    "endpoints": {
                        "https://app.datadoghq.com": "datadog_only",
                        "https://opw.example.com": false,
                        "https://shadow.example.com": 1.0,
                        "https://null.example.com": null,
                    },
                }
            }
        }));

        assert!(errors.is_none());
        assert_eq!(config.shared.metrics_encoding.v3_series_mode, V3SeriesMode::Enabled);
        assert_eq!(
            config
                .shared
                .metrics_encoding
                .v3_series_endpoint_modes
                .get("https://app.datadoghq.com"),
            Some(&V3SeriesMode::DatadogOnly)
        );
        assert_eq!(
            config
                .shared
                .metrics_encoding
                .v3_series_endpoint_modes
                .get("https://opw.example.com"),
            Some(&V3SeriesMode::Disabled)
        );
        // The Agent's string cast renders `1.0` as `1`, which it reads as true, and a null as the
        // empty string, which it reads as false.
        assert_eq!(
            config
                .shared
                .metrics_encoding
                .v3_series_endpoint_modes
                .get("https://shadow.example.com"),
            Some(&V3SeriesMode::Enabled)
        );
        assert_eq!(
            config
                .shared
                .metrics_encoding
                .v3_series_endpoint_modes
                .get("https://null.example.com"),
            Some(&V3SeriesMode::Disabled)
        );
    }

    #[test]
    fn an_uninterpretable_v3_series_mode_disables_v3_without_failing_translation() {
        // The Agent warns and routes to the older intake for a mode it cannot interpret, so the strict
        // startup gate must not reject a configuration the Agent runs with. The recovered value is
        // disabled rather than the `datadog_only` field default.
        let (config, errors) = translate_explicit(json!({
            "use_v3_api": { "series": { "enabled": "sometimes", "endpoints": { "https://app.datadoghq.com": "often" } } }
        }));

        assert!(errors.is_none());
        assert_eq!(config.shared.metrics_encoding.v3_series_mode, V3SeriesMode::Disabled);
        assert_eq!(
            config
                .shared
                .metrics_encoding
                .v3_series_endpoint_modes
                .get("https://app.datadoghq.com"),
            Some(&V3SeriesMode::Disabled)
        );
    }

    #[test]
    fn a_compound_v3_series_endpoint_mode_records_a_translation_error() {
        // A mode written as a list or map is a structural error, not a mode the Agent interprets.
        let (config, errors) = translate_explicit(json!({
            "use_v3_api": { "series": { "endpoints": { "https://app.datadoghq.com": ["true"] } } }
        }));

        assert!(config.shared.metrics_encoding.v3_series_endpoint_modes.is_empty());
        let errors = errors.expect("a compound mode should record a translation error");
        assert!(errors.to_string().contains("use_v3_api.series.endpoints"));
    }

    // Issue #1965: the Core Agent streams `dd_url` at its schema default even when the operator
    // configured only `site`. The translator used to compare the URL against that default and treat a
    // match as unset, which also discarded an operator's deliberate choice of the default intake.
    // Provenance separates the two.
    #[test]
    fn a_defaulted_dd_url_is_not_an_override_so_site_wins() {
        let (config, errors) = translate_stream(&[
            ("site", json!("datadoghq.eu"), StreamProvenance::Explicit),
            ("dd_url", json!("https://app.datadoghq.com"), StreamProvenance::Default),
        ]);

        assert!(errors.is_none());
        // The effective value survives, so a consumer that wants the URL need not restate it.
        assert_defaulted(&config.shared.endpoints.dd_url, "https://app.datadoghq.com");
        assert_explicit(&config.shared.endpoints.site, "datadoghq.eu");
    }

    #[test]
    fn an_explicit_dd_url_is_an_override_even_at_the_default_intake() {
        // Comparing values cannot see this: the operator chose the default intake deliberately, and
        // that choice must still override `site`.
        let (config, errors) = translate_explicit(json!({
            "site": "datadoghq.eu",
            "dd_url": "https://app.datadoghq.com",
        }));

        assert!(errors.is_none());
        assert_explicit(&config.shared.endpoints.dd_url, "https://app.datadoghq.com");
    }

    #[test]
    fn a_dd_url_override_is_carried_through() {
        let (config, errors) = translate_explicit(json!({
            "site": "datadoghq.eu",
            "dd_url": "https://proxy.internal.example.com:3128",
        }));

        assert!(errors.is_none());
        assert_explicit(
            &config.shared.endpoints.dd_url,
            "https://proxy.internal.example.com:3128",
        );
    }

    #[test]
    fn an_empty_endpoint_value_keeps_its_provenance() {
        // Empty endpoint strings retain the provenance of the source that supplied them.
        let (config, errors) = translate_explicit(json!({ "site": "", "dd_url": "" }));

        assert!(errors.is_none());
        assert_explicit(&config.shared.endpoints.site, "");
        assert_explicit(&config.shared.endpoints.dd_url, "");

        let (config, errors) = translate_stream(&[
            ("site", json!(""), StreamProvenance::Default),
            ("dd_url", json!(""), StreamProvenance::Default),
        ]);

        assert!(errors.is_none());
        assert_defaulted(&config.shared.endpoints.site, "");
        assert_defaulted(&config.shared.endpoints.dd_url, "");
    }

    // The Agent streams `serializer_zstd_compressor_level` at its schema default of 1 even when the
    // operator configured nothing. ADP wants its own default of 3, so it applies the Agent's level only
    // when an input set one. Comparing the value against 1 also discarded an operator who deliberately
    // asked for 1; provenance separates the two.
    #[test]
    fn the_agent_zstd_level_is_honored_only_when_explicit() {
        let (config, errors) =
            translate_stream(&[("serializer_zstd_compressor_level", json!(1), StreamProvenance::Default)]);

        assert!(errors.is_none());
        assert_eq!(
            DEFAULT_ZSTD_COMPRESSOR_LEVEL,
            config.shared.endpoints.compression.effective_zstd_level()
        );

        let (config, errors) = translate_explicit(json!({ "serializer_zstd_compressor_level": 1 }));

        assert!(errors.is_none());
        assert_eq!(1, config.shared.endpoints.compression.effective_zstd_level());
    }

    #[test]
    fn the_adp_zstd_level_wins_over_an_explicit_agent_level() {
        let (mut config, errors) = translate_explicit(json!({ "serializer_zstd_compressor_level": 5 }));
        assert!(errors.is_none());

        let saluki_only: SalukiOnly =
            serde_json::from_value(json!({ "data_plane": { "serializer_zstd_compressor_level": 4 } }))
                .expect("saluki-only source deserializes");
        saluki_only.seed(&mut config);

        assert_eq!(4, config.shared.endpoints.compression.effective_zstd_level());
    }

    #[test]
    fn an_out_of_range_agent_zstd_level_records_a_translation_error() {
        let (config, errors) = translate_explicit(json!({
            "serializer_zstd_compressor_level": i64::from(i32::MAX) + 1,
        }));

        let errors = errors.expect("out-of-range zstd level should record a translation error");
        assert!(errors.to_string().contains("serializer_zstd_compressor_level"));
        assert_eq!(
            DEFAULT_ZSTD_COMPRESSOR_LEVEL,
            config.shared.endpoints.compression.effective_zstd_level()
        );
    }

    #[test]
    fn retry_queue_sizes_are_honored_only_when_explicit() {
        // The Core Agent streams both keys even when the operator configured neither. Treating those
        // defaults as explicit settings would hide a value supplied through the deprecated key.
        let (config, errors) = translate_stream(&[
            ("forwarder_retry_queue_max_size", json!(0), StreamProvenance::Default),
            (
                "forwarder_retry_queue_payloads_max_size",
                json!(15 * 1024 * 1024),
                StreamProvenance::Default,
            ),
        ]);
        assert!(errors.is_none());
        // The schema defaults are still the effective values.
        let forwarder = &config.shared.endpoints.forwarder;
        assert_defaulted(&forwarder.retry_queue_max_size, 0);
        assert_defaulted(&forwarder.retry_queue_payloads_max_size, 15 * 1024 * 1024);

        // Only the deprecated key is explicit, so the retry configuration must use it.
        let (config, errors) = translate_explicit(json!({ "forwarder_retry_queue_max_size": 42 }));
        assert!(errors.is_none());
        let forwarder = &config.shared.endpoints.forwarder;
        assert_explicit(&forwarder.retry_queue_max_size, 42);
        assert_defaulted(&forwarder.retry_queue_payloads_max_size, 15 * 1024 * 1024);

        // Only the new key is explicit.
        let (config, errors) = translate_explicit(json!({ "forwarder_retry_queue_payloads_max_size": 1024 }));
        assert!(errors.is_none());
        let forwarder = &config.shared.endpoints.forwarder;
        assert_explicit(&forwarder.retry_queue_payloads_max_size, 1024);
        assert_defaulted(&forwarder.retry_queue_max_size, 0);
    }

    #[test]
    fn an_explicit_retry_queue_size_of_zero_is_not_a_default() {
        // `0` is this deprecated key's schema default and also a value an operator can set. Comparing
        // values conflated the two, silently discarding the operator's setting.
        let (config, errors) = translate_explicit(json!({ "forwarder_retry_queue_max_size": 0 }));

        assert!(errors.is_none());
        assert_explicit(&config.shared.endpoints.forwarder.retry_queue_max_size, 0);
    }

    #[test]
    fn padded_failover_endpoint_settings_are_trimmed() {
        // A padded value must resolve to the same endpoint as a bare one. Carrying the whitespace
        // through would compose an intake URL the forwarder cannot parse.
        let (config, errors) = translate_explicit(json!({
            "multi_region_failover": { "api_key": " mrf-key ", "site": " datadoghq.eu " }
        }));

        assert!(errors.is_none());
        let mrf = &config.domains.multi_region_failover;
        assert_eq!(Some("mrf-key"), mrf.api_key.as_deref());
        assert_eq!(Some("datadoghq.eu"), mrf.site.as_deref());
        assert_eq!(
            Some("https://app.mrf.datadoghq.eu".to_string()),
            mrf.metrics_endpoint_url()
        );

        let (config, errors) = translate_explicit(json!({
            "multi_region_failover": { "dd_url": "  https://custom-mrf.example.com  " }
        }));

        assert!(errors.is_none());
        assert_eq!(
            Some("https://custom-mrf.example.com"),
            config.domains.multi_region_failover.dd_url.as_deref()
        );
    }

    #[test]
    fn a_blank_failover_endpoint_setting_is_unset() {
        // A whitespace-only value says no more than an absent one. Retaining it would report the
        // failover region as credentialed when its key is blank.
        let (config, errors) = translate_explicit(json!({
            "multi_region_failover": { "enabled": true, "api_key": "   ", "site": "\t", "dd_url": " " }
        }));

        assert!(errors.is_none());
        let mrf = &config.domains.multi_region_failover;
        assert_eq!(None, mrf.api_key);
        assert_eq!(None, mrf.site);
        assert_eq!(None, mrf.dd_url);
        assert_eq!(None, mrf.metrics_endpoint_url());
    }

    #[test]
    fn cluster_agent_settings_default_to_schema_values_when_unset() {
        let (config, errors) = translate_explicit(json!({}));

        assert!(errors.is_none());
        let cluster_agent = &config.shared.cluster_agent;
        assert!(!cluster_agent.enabled);
        assert_eq!(None, cluster_agent.url);
        assert_eq!(None, cluster_agent.auth_token);
        assert_eq!("datadog-cluster-agent", cluster_agent.kubernetes_service_name);
    }

    #[test]
    fn padded_cluster_agent_settings_are_trimmed() {
        // Whitespace around the endpoint would travel into the request URL, and whitespace around the
        // token would be sent as part of the credential.
        let (config, errors) = translate_explicit(json!({
            "cluster_agent": {
                "enabled": true,
                "url": " https://cluster-agent.example.com ",
                "auth_token": " cluster-agent-token ",
                "kubernetes_service_name": " custom-cluster-agent "
            }
        }));

        assert!(errors.is_none());
        let cluster_agent = &config.shared.cluster_agent;
        assert_eq!(Some("https://cluster-agent.example.com"), cluster_agent.url.as_deref());
        assert_eq!(Some("cluster-agent-token"), cluster_agent.auth_token.as_deref());
        assert_eq!("custom-cluster-agent", cluster_agent.kubernetes_service_name);
    }

    #[test]
    fn a_blank_cluster_agent_setting_is_unset() {
        // A blank URL or token says no more than an absent one. A blank service name is different: it
        // is the way to ask that the injected Kubernetes environment variables be ignored, so it stays
        // empty instead of falling back to the schema default.
        let (config, errors) = translate_explicit(json!({
            "cluster_agent": {
                "enabled": true,
                "url": "  ",
                "auth_token": "\t",
                "kubernetes_service_name": "   "
            }
        }));

        assert!(errors.is_none());
        let cluster_agent = &config.shared.cluster_agent;
        assert_eq!(None, cluster_agent.url);
        assert_eq!(None, cluster_agent.auth_token);
        assert_eq!("", cluster_agent.kubernetes_service_name);
    }

    #[test]
    fn metric_filter_resolves_current_and_legacy_precedence() {
        let cases = [
            (
                json!({
                    "metric_filterlist": ["current"],
                    "metric_filterlist_match_prefix": true,
                    "statsd_metric_blocklist": ["legacy"],
                    "statsd_metric_blocklist_match_prefix": false,
                }),
                MetricFilter {
                    values: vec!["current".to_string()],
                    match_prefix: true,
                },
            ),
            (
                json!({
                    "metric_filterlist": [],
                    "metric_filterlist_match_prefix": true,
                    "statsd_metric_blocklist": ["legacy"],
                    "statsd_metric_blocklist_match_prefix": false,
                }),
                MetricFilter {
                    values: vec!["legacy".to_string()],
                    match_prefix: false,
                },
            ),
            (json!({}), MetricFilter::default()),
        ];

        for (source, expected) in cases {
            let (config, errors) = translate_explicit(source);
            assert!(errors.is_none());
            assert_eq!(config.domains.dogstatsd.metric_filter, expected);
        }
    }

    #[test]
    fn metric_namespace_translates_schema_default_and_blocklist_alias() {
        let (default_config, errors) = translate_explicit(json!({}));
        assert!(errors.is_none());
        assert_eq!(
            default_config
                .domains
                .dogstatsd
                .prefix_filter
                .metric_namespace_blocklist,
            DatadogConfiguration::default().statsd_metric_namespace_blacklist
        );

        let (config, errors) = translate_explicit(json!({
            "statsd_metric_namespace": "tenant",
            "statsd_metric_namespace_blocklist": ["custom"],
        }));
        assert!(errors.is_none());
        assert_eq!(config.domains.dogstatsd.prefix_filter.metric_namespace, "tenant");
        assert_eq!(
            config.domains.dogstatsd.prefix_filter.metric_namespace_blocklist,
            vec!["custom".to_string()]
        );
    }

    #[test]
    fn tag_filter_actions_preserve_tolerant_parsing() {
        use agent_data_plane_config::domains::dogstatsd::FilterAction;

        let (config, errors) = translate_explicit(json!({
            "metric_tag_filterlist": [
                { "metric_name": "missing", "tags": ["a"] },
                { "metric_name": "null", "action": null, "tags": ["b"] },
                { "metric_name": "empty", "action": "", "tags": ["c"] },
                { "metric_name": "include", "action": "include", "tags": ["d"] },
                { "metric_name": "exclude", "action": "exclude", "tags": ["e"] },
                { "metric_name": "unknown", "action": "exlude", "tags": ["f"] },
            ],
        }));

        assert!(errors.is_none());
        let entries = &config.domains.dogstatsd.tag_filterlist;
        let actions: Vec<_> = entries.iter().map(|entry| entry.action).collect();
        assert_eq!(
            actions,
            vec![
                FilterAction::Exclude,
                FilterAction::Exclude,
                FilterAction::Exclude,
                FilterAction::Include,
                FilterAction::Exclude,
                FilterAction::Exclude,
            ]
        );
        assert_eq!(entries[0].metric_name, "missing");
        assert_eq!(entries[5].tags, ["f"]);
    }

    #[test]
    fn tag_filter_entry_without_tags_is_kept_with_no_tags() {
        let (config, errors) = translate_explicit(json!({
            "metric_tag_filterlist": [{ "metric_name": "svc.latency" }],
        }));

        assert!(errors.is_none(), "the Agent accepts an entry without tags");
        let entries = &config.domains.dogstatsd.tag_filterlist;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metric_name, "svc.latency");
        assert!(entries[0].tags.is_empty());
    }

    #[test]
    fn malformed_tag_filter_entry_does_not_drop_valid_neighbors() {
        let (config, errors) = translate_explicit(json!({
            "metric_tag_filterlist": [
                { "metric_name": "before", "tags": ["a"] },
                { "tags": ["orphan"] },
                { "metric_name": "after", "tags": ["b"] },
            ],
        }));

        let names: Vec<_> = config
            .domains
            .dogstatsd
            .tag_filterlist
            .iter()
            .map(|entry| entry.metric_name.as_str())
            .collect();
        assert_eq!(names, ["before", "after"]);
        assert!(
            errors.is_some(),
            "an entry without a metric name must record a translation error"
        );
    }

    #[test]
    fn ipc_settings_default_to_schema_values_when_unset() {
        // The typed model derives `Default`, so a translation that silently skipped these keys would
        // leave zeroes behind and still look healthy. Pin the schema defaults instead.
        let (config, errors) = translate_explicit(json!({}));

        assert!(errors.is_none());
        assert_eq!(config.control.ipc.cmd_port, 5001);
        assert_eq!(config.control.ipc.grpc_max_message_size, 128 * 1024 * 1024);
        assert_eq!(config.control.ipc.vsock_addr, "");
    }

    #[test]
    fn ipc_auth_paths_preserve_empty_defaults_and_explicit_values() {
        let (config, errors) = translate_explicit(json!({}));
        assert!(errors.is_none());
        assert!(config.control.ipc.auth_token_file_path.as_os_str().is_empty());
        assert!(config.control.ipc.ipc_cert_file_path.as_os_str().is_empty());

        let (config, errors) = translate_explicit(json!({
            "auth_token_file_path": "/secret/auth_token",
            "ipc_cert_file_path": "/secret/ipc_cert.pem",
        }));
        assert!(errors.is_none());
        assert_eq!(
            config.control.ipc.auth_token_file_path,
            PathBuf::from("/secret/auth_token")
        );
        assert_eq!(
            config.control.ipc.ipc_cert_file_path,
            PathBuf::from("/secret/ipc_cert.pem")
        );
    }

    #[test]
    fn cmd_port_preserves_u16_validation() {
        for value in [0, 5001, u16::MAX as i64] {
            let (config, errors) = translate_explicit(json!({ "cmd_port": value }));

            assert!(errors.is_none());
            assert_eq!(config.control.ipc.cmd_port, value as u16);
        }

        for value in [-1, u16::MAX as i64 + 1] {
            let (config, errors) = translate_explicit(json!({ "cmd_port": value }));

            assert_eq!(config.control.ipc.cmd_port, 0);
            let errors = errors.expect("an out-of-range port should record a translation error");
            assert!(errors.to_string().contains("cmd_port"));
        }
    }

    #[test]
    fn negative_ipc_grpc_max_message_size_is_rejected() {
        let (config, errors) = translate_explicit(json!({
            "agent_ipc": { "grpc_max_message_size": -1 },
        }));

        assert_eq!(config.control.ipc.grpc_max_message_size, 0);
        let errors = errors.expect("a negative maximum message size must record a translation error");
        assert!(errors.to_string().contains("agent_ipc.grpc_max_message_size"));
    }

    #[test]
    fn negative_tag_filter_cache_capacity_is_rejected() {
        let (config, errors) = translate_explicit(json!({
            "data_plane": { "dogstatsd": { "aggregator_tag_filter_cache_capacity": -1 } },
        }));

        assert_eq!(
            config
                .domains
                .dogstatsd
                .aggregation
                .aggregator_tag_filter_cache_capacity,
            0
        );
        let errors = errors.expect("a negative cache capacity must record a translation error");
        assert!(errors
            .to_string()
            .contains("data_plane.dogstatsd.aggregator_tag_filter_cache_capacity"));
    }

    #[test]
    fn otlp_trace_internal_port_preserves_u16_validation() {
        // The typed forwarder receives this value after translation. Rejecting conversion here
        // preserves the u16 validation formerly provided by GenericConfiguration instead of
        // clamping invalid ports.
        for value in [0, 5003, u16::MAX as i64] {
            let (config, errors) = translate_explicit(json!({
                "otlp_config": { "traces": { "internal_port": value } }
            }));

            assert!(errors.is_none());
            assert_eq!(config.domains.otlp.traces.internal_port, value as u16);
        }

        for value in [-1, u16::MAX as i64 + 1] {
            let (config, errors) = translate_explicit(json!({
                "otlp_config": { "traces": { "internal_port": value } }
            }));

            assert_eq!(config.domains.otlp.traces.internal_port, 0);
            let errors = errors.expect("an out-of-range port should record a translation error");
            assert!(errors.to_string().contains("otlp_config.traces.internal_port"));
        }
    }

    #[test]
    fn summary_mode_translates_known_values() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": {
                "metrics": {
                    "summaries": {
                        "mode": "noquantiles"
                    }
                }
            }
        }));

        assert!(errors.is_none());
        assert_eq!(config.domains.otlp.metrics.summaries.mode, SummaryMode::NoQuantiles);
    }

    #[test]
    fn invalid_summary_mode_records_error_and_keeps_default() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": {
                "metrics": {
                    "summaries": {
                        "mode": "unsupported"
                    }
                }
            }
        }));

        assert_eq!(config.domains.otlp.metrics.summaries.mode, SummaryMode::Gauges);
        let errors = errors.expect("invalid mode should record a translation error");
        assert!(errors.to_string().contains("otlp_config.metrics.summaries.mode"));
        assert!(errors.to_string().contains("unknown summary mode `unsupported`"));
    }

    #[test]
    fn cumulative_monotonic_sum_mode_translates_known_values() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": {
                "metrics": {
                    "sums": {
                        "cumulative_monotonic_mode": "raw_value"
                    }
                }
            }
        }));

        assert!(errors.is_none());
        assert_eq!(
            config.domains.otlp.metrics.sums.cumulative_monotonic_mode,
            CumulativeMonotonicMode::RawValue
        );
    }

    #[test]
    fn invalid_cumulative_monotonic_sum_mode_records_error_and_keeps_default() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": {
                "metrics": {
                    "sums": {
                        "cumulative_monotonic_mode": "unsupported"
                    }
                }
            }
        }));

        assert_eq!(
            config.domains.otlp.metrics.sums.cumulative_monotonic_mode,
            CumulativeMonotonicMode::ToDelta
        );
        let errors = errors.expect("invalid mode should record a translation error");
        assert!(errors
            .to_string()
            .contains("otlp_config.metrics.sums.cumulative_monotonic_mode"));
        assert!(errors
            .to_string()
            .contains("unknown cumulative monotonic sum mode `unsupported`"));
    }

    #[test]
    fn initial_cumulative_monotonic_value_translates_known_values() {
        for (value, expected) in [
            ("auto", InitialCumulativeMonotonicValue::Auto),
            ("drop", InitialCumulativeMonotonicValue::Drop),
            ("keep", InitialCumulativeMonotonicValue::Keep),
        ] {
            let (config, errors) = translate_explicit(json!({
                "otlp_config": {
                    "metrics": {
                        "sums": {
                            "initial_cumulative_monotonic_value": value
                        }
                    }
                }
            }));

            assert!(errors.is_none());
            assert_eq!(
                config.domains.otlp.metrics.sums.initial_cumulative_monotonic_value,
                expected
            );
        }
    }

    #[test]
    fn invalid_initial_cumulative_monotonic_value_records_error_and_keeps_default() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": {
                "metrics": {
                    "sums": {
                        "initial_cumulative_monotonic_value": "unsupported"
                    }
                }
            }
        }));

        assert_eq!(
            config.domains.otlp.metrics.sums.initial_cumulative_monotonic_value,
            InitialCumulativeMonotonicValue::Auto
        );
        let errors = errors.expect("invalid value should record a translation error");
        assert!(errors
            .to_string()
            .contains("otlp_config.metrics.sums.initial_cumulative_monotonic_value"));
        assert!(errors
            .to_string()
            .contains("unknown initial cumulative monotonic value `unsupported`"));
    }

    #[test]
    fn grpc_max_recv_msg_size_zero_translates_to_grpc_go_default() {
        // The schema default of `0` selects grpc-go's built-in 4 MiB limit, so translation must
        // substitute the default; any positive value is carried through unchanged.
        for (configured, expected) in [
            (json!({}), DEFAULT_GRPC_MAX_RECV_MSG_SIZE_MIB),
            (
                json!({ "max_recv_msg_size_mib": 0 }),
                DEFAULT_GRPC_MAX_RECV_MSG_SIZE_MIB,
            ),
            (json!({ "max_recv_msg_size_mib": 8 }), 8),
        ] {
            let (config, errors) = translate_explicit(json!({
                "otlp_config": { "receiver": { "protocols": { "grpc": configured } } }
            }));

            assert!(errors.is_none());
            assert_eq!(config.domains.otlp.receiver.grpc.max_recv_msg_size_mib, expected);
        }
    }

    #[test]
    fn otlp_metrics_delta_ttl_translates_explicit_value() {
        // An explicit integer (seconds) is carried through the witness as a `Duration`.
        let (config, errors) = translate_explicit(json!({
            "otlp_config": { "metrics": { "delta_ttl": 7200 } }
        }));

        assert!(errors.is_none());
        assert_eq!(config.domains.otlp.metrics.delta_ttl, Duration::from_secs(7200));
    }

    #[test]
    fn otlp_metrics_delta_ttl_defaults_to_3600s_when_unset() {
        let (config, errors) = translate_explicit(json!({}));

        assert!(errors.is_none());
        assert_eq!(config.domains.otlp.metrics.delta_ttl, DEFAULT_DELTA_TTL);
    }

    #[test]
    fn otlp_metrics_delta_ttl_negative_records_error_and_keeps_default() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": { "metrics": { "delta_ttl": -1 } }
        }));

        assert_eq!(config.domains.otlp.metrics.delta_ttl, DEFAULT_DELTA_TTL);
        let errors = errors.expect("negative delta_ttl should record a translation error");
        assert!(errors.to_string().contains("otlp_config.metrics.delta_ttl"));
        assert!(errors.to_string().contains("time to live must be positive"));
    }

    #[test]
    fn otlp_metrics_delta_ttl_zero_records_error_and_keeps_default() {
        let (config, errors) = translate_explicit(json!({
            "otlp_config": { "metrics": { "delta_ttl": 0 } }
        }));

        assert_eq!(config.domains.otlp.metrics.delta_ttl, DEFAULT_DELTA_TTL);
        let errors = errors.expect("zero delta_ttl should record a translation error");
        assert!(errors.to_string().contains("otlp_config.metrics.delta_ttl"));
        assert!(errors.to_string().contains("time to live must be positive"));
    }
}
