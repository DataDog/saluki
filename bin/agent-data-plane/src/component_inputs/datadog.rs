//! Inputs of the Datadog forwarders and metrics encoders.

use agent_data_plane_config::{
    domains::multi_region_failover,
    shared::{self, SharedConfiguration},
};
use saluki_components::{
    config::MrfConfiguration,
    encoders::{
        DatadogMetricsConfiguration, UseV3ApiConfig, UseV3ApiSeriesConfig, V3ApiConfig, V3ApiSettings, V3SeriesMode,
    },
    forwarders::{
        EndpointConfiguration, ForwarderConfiguration, ForwarderHttpProtocol, OpwMetricsConfiguration,
        OpwMetricsSettings, ProxyConfiguration, RetryConfiguration,
    },
};

/// Builds the forwarder settings shared by every Datadog intake forwarder.
pub(crate) fn forwarder_configuration(shared: &SharedConfiguration) -> ForwarderConfiguration {
    let endpoints = &shared.endpoints;
    let forwarder = &endpoints.forwarder;
    let proxy = &endpoints.proxy;
    let tls = &endpoints.tls;

    ForwarderConfiguration {
        endpoint_concurrency: forwarder.max_concurrent_requests,
        endpoint_concurrency_multiplier: forwarder.num_workers,
        request_timeout_secs: forwarder.timeout,
        endpoint_buffer_size: forwarder.high_prio_buffer_size,
        endpoint: EndpointConfiguration::new(
            endpoints.api_key.clone(),
            endpoints.primary_endpoint(),
            endpoints.additional_endpoints.clone(),
        ),
        retry: RetryConfiguration {
            backoff_factor: forwarder.backoff_factor,
            backoff_base: forwarder.backoff_base,
            backoff_max: forwarder.backoff_max,
            recovery_error_decrease_factor: forwarder.recovery_interval,
            recovery_reset: forwarder.recovery_reset,
            queue_max_size_bytes: forwarder.effective_retry_queue_max_size_bytes(),
            storage_max_size_bytes: forwarder.storage_max_size_in_bytes,
            flush_to_disk_mem_ratio: forwarder.flush_to_disk_mem_ratio,
            storage_path: forwarder.storage_path.clone(),
            run_path: shared.run_path.clone(),
            storage_max_disk_ratio: forwarder.storage_max_disk_ratio,
            outdated_file_in_days: forwarder.outdated_file_in_days,
            capacity_time_interval_secs: forwarder.retry_queue_capacity_time_interval_sec,
        },
        proxy: ProxyConfiguration {
            http: proxy.http.clone(),
            https: proxy.https.clone(),
            no_proxy: proxy.no_proxy.clone(),
            no_proxy_nonexact_match: proxy.no_proxy_nonexact_match,
            use_proxy_for_cloud_metadata: proxy.use_proxy_for_cloud_metadata,
        },
        opw_metrics: opw_metrics(endpoints),
        http_protocol: match forwarder.http_protocol {
            shared::ForwarderHttpProtocol::Auto => ForwarderHttpProtocol::Auto,
            shared::ForwarderHttpProtocol::Http1 => ForwarderHttpProtocol::Http1,
        },
        connection_reset_interval_secs: forwarder.connection_reset_interval,
        stop_wait_for_inflight: forwarder.stop_wait_for_inflight,
        v3_api: v3_api(&shared.metrics_encoding.v3_api),
        use_v3_api: use_v3_api(&shared.metrics_encoding),
        serializer_compressor_kind: endpoints.compression.compressor_kind.clone(),
        skip_ssl_validation: tls.skip_ssl_validation,
        sslkeylogfile: tls.sslkeylogfile.clone(),
        min_tls_version: tls.min_tls_version.clone(),
        tls_handshake_timeout: tls.handshake_timeout,
        allow_arbitrary_tags: endpoints.allow_arbitrary_tags,
        api_key_validation_interval_mins: forwarder.apikey_validation_interval,
        routing_targets: None,
    }
}

/// Builds the settings shared by every Datadog metrics encoder.
pub(crate) fn datadog_metrics_configuration(shared: &SharedConfiguration) -> DatadogMetricsConfiguration {
    let endpoints = &shared.endpoints;
    let metrics = &shared.metrics_encoding;

    DatadogMetricsConfiguration {
        max_metrics_per_payload: metrics.max_metrics_per_payload,
        max_payload_size: metrics.max_payload_size,
        max_uncompressed_payload_size: metrics.max_uncompressed_payload_size,
        max_series_payload_size: metrics.max_series_payload_size,
        max_series_uncompressed_payload_size: metrics.max_series_uncompressed_payload_size,
        max_series_points_per_payload: metrics.max_series_points_per_payload,
        flush_timeout: metrics.flush_timeout,
        compressor_kind: endpoints.compression.compressor_kind.clone(),
        zstd_compressor_level: endpoints.compression.effective_zstd_level(),
        use_v2_series_api: metrics.use_v2_series_api,
        log_payloads: metrics.log_payloads,
        additional_tags: None,
        v3_api: v3_api(&metrics.v3_api),
        use_v3_api: use_v3_api(metrics),
        opw_metrics: opw_metrics(endpoints),
        primary_endpoint: endpoints.primary_endpoint(),
        additional_endpoints: endpoints.additional_endpoints.clone(),
        routing: None,
    }
}

/// Builds the Multi-Region Failover settings.
pub(crate) fn mrf_configuration(mrf: &multi_region_failover::Domain) -> MrfConfiguration {
    MrfConfiguration::new(mrf.enabled, mrf.api_key.clone(), mrf.metrics_endpoint_url())
}

fn opw_metrics(endpoints: &shared::Endpoints) -> OpwMetricsConfiguration {
    OpwMetricsConfiguration {
        observability_pipelines_worker: alternate_metrics_intake(&endpoints.opw_intake),
        vector: alternate_metrics_intake(&endpoints.vector_intake),
    }
}

fn alternate_metrics_intake(intake: &shared::AltMetricsIntake) -> OpwMetricsSettings {
    OpwMetricsSettings {
        enabled: intake.enabled,
        url: intake.url.clone(),
        use_v3_series: intake.use_v3_series,
    }
}

fn v3_api(v3_api: &shared::V3ApiEncoding) -> V3ApiConfig {
    V3ApiConfig {
        sketches: V3ApiSettings {
            endpoints: v3_api.sketches.endpoints.clone(),
        },
        compression_level: v3_api.compression_level,
    }
}

fn use_v3_api(metrics: &shared::MetricsEncoding) -> UseV3ApiConfig {
    UseV3ApiConfig {
        series: UseV3ApiSeriesConfig {
            enabled: v3_series_mode(metrics.v3_series_mode),
            endpoints: metrics
                .v3_series_endpoint_modes
                .iter()
                .map(|(endpoint, mode)| (endpoint.clone(), v3_series_mode(*mode)))
                .collect(),
        },
    }
}

fn v3_series_mode(mode: shared::V3SeriesMode) -> V3SeriesMode {
    match mode {
        shared::V3SeriesMode::Enabled => V3SeriesMode::Enabled,
        shared::V3SeriesMode::Disabled => V3SeriesMode::Disabled,
        shared::V3SeriesMode::DatadogOnly => V3SeriesMode::DatadogOnly,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, time::Duration};

    use agent_data_plane_config::{
        shared::{
            AltMetricsIntake, Compression, Endpoints, Forwarder, MetricsEncoding, Proxy, Tls, V3ApiEncoding,
            V3ApiSettings as ModelV3ApiSettings,
        },
        ConfigValue,
    };

    use super::*;

    /// Returns shared configuration in which every value the adapters copy is distinct from its default and from the
    /// other values of its type.
    fn distinct_shared_configuration() -> SharedConfiguration {
        SharedConfiguration {
            endpoints: Endpoints {
                api_key: "primary-key".to_string(),
                site: ConfigValue::explicit("datadoghq.eu".to_string()),
                dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
                additional_endpoints: HashMap::from([(
                    "https://additional.example.com".to_string(),
                    vec!["additional-key".to_string()],
                )]),
                allow_arbitrary_tags: true,
                proxy: Proxy {
                    http: "http://http-proxy.example.com".to_string(),
                    https: "http://https-proxy.example.com".to_string(),
                    no_proxy: vec!["no-proxy.example.com".to_string()],
                    no_proxy_nonexact_match: true,
                    use_proxy_for_cloud_metadata: false,
                },
                tls: Tls {
                    skip_ssl_validation: true,
                    min_tls_version: "tlsv1.3".to_string(),
                    sslkeylogfile: "/keys.log".to_string(),
                    handshake_timeout: Duration::from_secs(21),
                },
                compression: Compression {
                    compressor_kind: "gzip".to_string(),
                    adp_zstd_level: ConfigValue::defaulted(3),
                    agent_zstd_level: ConfigValue::explicit(9),
                },
                forwarder: Forwarder {
                    apikey_validation_interval: 22,
                    backoff_base: 1.5,
                    backoff_factor: 2.5,
                    backoff_max: 3.5,
                    connection_reset_interval: 23,
                    flush_to_disk_mem_ratio: 0.25,
                    high_prio_buffer_size: 24,
                    http_protocol: shared::ForwarderHttpProtocol::Http1,
                    max_concurrent_requests: 25,
                    num_workers: 26,
                    outdated_file_in_days: 27,
                    recovery_interval: 28,
                    recovery_reset: true,
                    retry_queue_capacity_time_interval_sec: 29,
                    retry_queue_max_size: ConfigValue::explicit(30),
                    retry_queue_payloads_max_size: ConfigValue::defaulted(31),
                    stop_timeout: Duration::from_secs(32),
                    stop_wait_for_inflight: true,
                    storage_max_disk_ratio: 0.75,
                    storage_max_size_in_bytes: 33,
                    storage_path: PathBuf::from("/storage"),
                    timeout: 34,
                },
                opw_intake: AltMetricsIntake {
                    enabled: true,
                    url: "https://opw.example.com".to_string(),
                    use_v3_series: true,
                },
                vector_intake: AltMetricsIntake {
                    enabled: false,
                    url: "https://vector.example.com".to_string(),
                    use_v3_series: false,
                },
            },
            metrics_encoding: MetricsEncoding {
                flush_timeout: Duration::from_millis(41),
                max_metrics_per_payload: 42,
                max_payload_size: 43,
                max_series_payload_size: 44,
                max_series_points_per_payload: 45,
                max_series_uncompressed_payload_size: 46,
                max_uncompressed_payload_size: 47,
                use_v2_series_api: true,
                log_payloads: true,
                v3_api: V3ApiEncoding {
                    sketches: ModelV3ApiSettings {
                        endpoints: vec!["https://sketches.example.com".to_string()],
                    },
                    compression_level: 48,
                },
                v3_series_mode: shared::V3SeriesMode::Enabled,
                v3_series_endpoint_modes: HashMap::from([
                    ("https://v2.example.com".to_string(), shared::V3SeriesMode::Disabled),
                    (
                        "https://datadog.example.com".to_string(),
                        shared::V3SeriesMode::DatadogOnly,
                    ),
                ]),
                ..Default::default()
            },
            run_path: Some(PathBuf::from("/run")),
            ..Default::default()
        }
    }

    fn expected_opw_metrics() -> OpwMetricsConfiguration {
        OpwMetricsConfiguration {
            observability_pipelines_worker: OpwMetricsSettings {
                enabled: true,
                url: "https://opw.example.com".to_string(),
                use_v3_series: true,
            },
            vector: OpwMetricsSettings {
                enabled: false,
                url: "https://vector.example.com".to_string(),
                use_v3_series: false,
            },
        }
    }

    fn expected_v3_api() -> V3ApiConfig {
        V3ApiConfig {
            sketches: V3ApiSettings {
                endpoints: vec!["https://sketches.example.com".to_string()],
            },
            compression_level: 48,
        }
    }

    fn expected_use_v3_api() -> UseV3ApiConfig {
        UseV3ApiConfig {
            series: UseV3ApiSeriesConfig {
                enabled: V3SeriesMode::Enabled,
                endpoints: HashMap::from([
                    ("https://v2.example.com".to_string(), V3SeriesMode::Disabled),
                    ("https://datadog.example.com".to_string(), V3SeriesMode::DatadogOnly),
                ]),
            },
        }
    }

    #[test]
    fn forwarder_configuration_comes_from_resolved_configuration() {
        let config = forwarder_configuration(&distinct_shared_configuration());

        assert_eq!(25, config.endpoint_concurrency);
        assert_eq!(26, config.endpoint_concurrency_multiplier);
        assert_eq!(34, config.request_timeout_secs);
        assert_eq!(24, config.endpoint_buffer_size);
        // The primary endpoint comes from the explicit site, because `dd_url` is only at its default.
        assert_eq!(
            EndpointConfiguration::new(
                "primary-key".to_string(),
                "https://app.datadoghq.eu".to_string(),
                HashMap::from([(
                    "https://additional.example.com".to_string(),
                    vec!["additional-key".to_string()],
                )]),
            ),
            config.endpoint
        );
        assert_eq!(expected_opw_metrics(), config.opw_metrics);
        assert_eq!(ForwarderHttpProtocol::Http1, config.http_protocol);
        assert_eq!(23, config.connection_reset_interval_secs);
        assert!(config.stop_wait_for_inflight);
        assert_eq!(expected_v3_api(), config.v3_api);
        assert_eq!(expected_use_v3_api(), config.use_v3_api);
        assert_eq!("gzip", config.serializer_compressor_kind);
        assert!(config.skip_ssl_validation);
        assert_eq!("/keys.log", config.sslkeylogfile);
        assert_eq!("tlsv1.3", config.min_tls_version);
        assert_eq!(Duration::from_secs(21), config.tls_handshake_timeout);
        assert!(config.allow_arbitrary_tags);
        assert_eq!(22, config.api_key_validation_interval_mins);
        assert!(config.routing_targets.is_none());
    }

    #[test]
    fn retry_configuration_comes_from_resolved_configuration() {
        let retry = forwarder_configuration(&distinct_shared_configuration()).retry;

        assert_eq!(2.5, retry.backoff_factor);
        assert_eq!(1.5, retry.backoff_base);
        assert_eq!(3.5, retry.backoff_max);
        assert_eq!(28, retry.recovery_error_decrease_factor);
        assert!(retry.recovery_reset);
        // Only the deprecated size is explicit, so it wins, scaled from payloads to bytes.
        assert_eq!(30 * shared::MAX_PAYLOAD_SIZE_BYTES, retry.queue_max_size_bytes);
        assert_eq!(33, retry.storage_max_size_bytes);
        assert_eq!(0.25, retry.flush_to_disk_mem_ratio);
        assert_eq!(PathBuf::from("/storage"), retry.storage_path);
        assert_eq!(Some(PathBuf::from("/run")), retry.run_path);
        assert_eq!(0.75, retry.storage_max_disk_ratio);
        assert_eq!(27, retry.outdated_file_in_days);
        assert_eq!(29, retry.capacity_time_interval_secs);
    }

    #[test]
    fn proxy_configuration_comes_from_resolved_configuration() {
        let proxy = forwarder_configuration(&distinct_shared_configuration()).proxy;

        assert_eq!("http://http-proxy.example.com", proxy.http);
        assert_eq!("http://https-proxy.example.com", proxy.https);
        assert_eq!(vec!["no-proxy.example.com".to_string()], proxy.no_proxy);
        assert!(proxy.no_proxy_nonexact_match);
        assert!(!proxy.use_proxy_for_cloud_metadata);
    }

    #[test]
    fn forwarder_http_protocol_maps_every_variant() {
        for (configured, expected) in [
            (shared::ForwarderHttpProtocol::Auto, ForwarderHttpProtocol::Auto),
            (shared::ForwarderHttpProtocol::Http1, ForwarderHttpProtocol::Http1),
        ] {
            let mut shared = distinct_shared_configuration();
            shared.endpoints.forwarder.http_protocol = configured;

            assert_eq!(
                expected,
                forwarder_configuration(&shared).http_protocol,
                "{configured:?}"
            );
        }
    }

    #[test]
    fn v3_series_mode_maps_every_variant() {
        for (configured, expected) in [
            (shared::V3SeriesMode::Enabled, V3SeriesMode::Enabled),
            (shared::V3SeriesMode::Disabled, V3SeriesMode::Disabled),
            (shared::V3SeriesMode::DatadogOnly, V3SeriesMode::DatadogOnly),
        ] {
            assert_eq!(expected, v3_series_mode(configured), "{configured:?}");
        }
    }

    #[test]
    fn datadog_metrics_configuration_comes_from_resolved_configuration() {
        let config = datadog_metrics_configuration(&distinct_shared_configuration());

        assert_eq!(42, config.max_metrics_per_payload);
        assert_eq!(43, config.max_payload_size);
        assert_eq!(47, config.max_uncompressed_payload_size);
        assert_eq!(44, config.max_series_payload_size);
        assert_eq!(46, config.max_series_uncompressed_payload_size);
        assert_eq!(45, config.max_series_points_per_payload);
        assert_eq!(Duration::from_millis(41), config.flush_timeout);
        assert_eq!("gzip", config.compressor_kind);
        // The Agent's level is explicit and ADP's is not, so the Agent's level applies.
        assert_eq!(9, config.zstd_compressor_level);
        assert!(config.use_v2_series_api);
        assert!(config.log_payloads);
        assert!(config.additional_tags.is_none());
        assert_eq!(expected_v3_api(), config.v3_api);
        assert_eq!(expected_use_v3_api(), config.use_v3_api);
        assert_eq!(expected_opw_metrics(), config.opw_metrics);
        assert_eq!("https://app.datadoghq.eu", config.primary_endpoint);
        assert_eq!(
            HashMap::from([(
                "https://additional.example.com".to_string(),
                vec!["additional-key".to_string()],
            )]),
            config.additional_endpoints
        );
        assert!(config.routing.is_none());
    }

    #[test]
    fn mrf_configuration_comes_from_resolved_configuration() {
        let mrf = multi_region_failover::Domain {
            enabled: true,
            api_key: Some("mrf-api-key".to_string()),
            site: Some("datadoghq.eu".to_string()),
            ..Default::default()
        };
        let config = mrf_configuration(&mrf);

        assert!(config.is_enabled());
        assert_eq!(Some("mrf-api-key"), config.api_key());
        assert_eq!(Some("https://app.mrf.datadoghq.eu"), config.metrics_endpoint_url());
    }
}
