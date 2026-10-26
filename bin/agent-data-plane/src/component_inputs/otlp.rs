//! Inputs of the OTLP source, relay, decoder, and forwarder.

use agent_data_plane_config::domains::otlp;
use saluki_components::sources::{
    CorsConfiguration, CumulativeMonotonicMode, GrpcTransport, HistogramMode, InitialCumulativeMonotonicValue,
    OtlpContextSettings, OtlpGrpcReceiver, OtlpHttpReceiver, OtlpKeepalive, OtlpMetricsSettings, OtlpReceiver,
    OtlpReceiverTls, OtlpTracesSettings, SummaryMode,
};

use super::origin_tag_cardinality;

/// Builds the OTLP receiver settings.
pub(crate) fn otlp_receiver(receiver: &otlp::Receiver) -> OtlpReceiver {
    let grpc = &receiver.grpc;
    let http = &receiver.http;

    OtlpReceiver {
        logs_enabled: receiver.logs_enabled,
        metrics_enabled: receiver.metrics_enabled,
        grpc: OtlpGrpcReceiver {
            endpoint: grpc.endpoint.clone(),
            max_recv_msg_size_mib: grpc.max_recv_msg_size_mib,
            transport: match grpc.transport {
                otlp::GrpcTransport::Tcp => GrpcTransport::Tcp,
                otlp::GrpcTransport::Unix => GrpcTransport::Unix,
            },
            max_concurrent_streams: grpc.max_concurrent_streams,
            keepalive: OtlpKeepalive {
                max_connection_age: grpc.keepalive.max_connection_age,
                max_connection_age_grace: grpc.keepalive.max_connection_age_grace,
                time: grpc.keepalive.time,
                timeout: grpc.keepalive.timeout,
            },
            tls: receiver_tls(&grpc.tls),
        },
        http: OtlpHttpReceiver {
            endpoint: http.endpoint.clone(),
            transport: http.transport.clone(),
            cors: CorsConfiguration {
                allowed_origins: http.cors.allowed_origins.clone(),
                allowed_headers: http.cors.allowed_headers.clone(),
                exposed_headers: http.cors.exposed_headers.clone(),
                max_age: http.cors.max_age,
            },
            tls: receiver_tls(&http.tls),
            max_request_body_size: http.max_request_body_size,
        },
    }
}

/// Builds the OTLP trace translation settings.
pub(crate) fn otlp_traces_settings(traces: &otlp::Traces) -> OtlpTracesSettings {
    OtlpTracesSettings {
        string_interner_size: traces.string_interner_size,
        probabilistic_sampler_sampling_percentage: traces.probabilistic_sampler_sampling_percentage,
        enable_compute_top_level_by_span_kind: traces.enable_compute_top_level_by_span_kind,
        ignore_missing_datadog_fields: traces.ignore_missing_datadog_fields,
    }
}

/// Builds the OTLP metrics translation settings.
pub(crate) fn otlp_metrics_settings(metrics: &otlp::Metrics) -> OtlpMetricsSettings {
    OtlpMetricsSettings {
        tag_cardinality: origin_tag_cardinality(metrics.tag_cardinality),
        histogram_mode: match metrics.histogram_mode {
            otlp::HistogramMode::NoBuckets => HistogramMode::NoBuckets,
            otlp::HistogramMode::Counters => HistogramMode::Counters,
            otlp::HistogramMode::Distributions => HistogramMode::Distributions,
        },
        send_histogram_aggregations: metrics.send_histogram_aggregations,
        resource_attributes_as_tags: metrics.resource_attributes_as_tags,
        instrumentation_scope_metadata_as_tags: metrics.instrumentation_scope_metadata_as_tags,
        cumulative_monotonic_mode: match metrics.sums.cumulative_monotonic_mode {
            otlp::CumulativeMonotonicMode::ToDelta => CumulativeMonotonicMode::ToDelta,
            otlp::CumulativeMonotonicMode::RawValue => CumulativeMonotonicMode::RawValue,
        },
        initial_cumulative_monotonic_value: match metrics.sums.initial_cumulative_monotonic_value {
            otlp::InitialCumulativeMonotonicValue::Auto => InitialCumulativeMonotonicValue::Auto,
            otlp::InitialCumulativeMonotonicValue::Drop => InitialCumulativeMonotonicValue::Drop,
            otlp::InitialCumulativeMonotonicValue::Keep => InitialCumulativeMonotonicValue::Keep,
        },
        tags: metrics.tags.clone(),
        summary_mode: match metrics.summaries.mode {
            otlp::SummaryMode::Gauges => SummaryMode::Gauges,
            otlp::SummaryMode::NoQuantiles => SummaryMode::NoQuantiles,
        },
        delta_ttl: metrics.delta_ttl,
    }
}

/// Builds the OTLP context resolver settings.
pub(crate) fn otlp_context_settings(contexts: &otlp::Contexts) -> OtlpContextSettings {
    OtlpContextSettings {
        allow_context_heap_allocs: contexts.allow_context_heap_allocs,
        cached_contexts_limit: contexts.cached_contexts_limit,
        cached_tagsets_limit: contexts.cached_tagsets_limit,
        string_interner_size: contexts.string_interner_size,
    }
}

fn receiver_tls(tls: &otlp::Tls) -> OtlpReceiverTls {
    OtlpReceiverTls {
        cert_file: tls.cert_file.clone(),
        key_file: tls.key_file.clone(),
        ca_file: tls.ca_file.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, time::Duration};

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality as ModelOriginTagCardinality;
    use saluki_core::data_model::origin::OriginTagCardinality;

    use super::*;

    fn tls(prefix: &str) -> otlp::Tls {
        otlp::Tls {
            cert_file: format!("/{prefix}/cert.pem"),
            key_file: format!("/{prefix}/key.pem"),
            ca_file: format!("/{prefix}/ca.pem"),
        }
    }

    fn expected_tls(prefix: &str) -> OtlpReceiverTls {
        OtlpReceiverTls {
            cert_file: format!("/{prefix}/cert.pem"),
            key_file: format!("/{prefix}/key.pem"),
            ca_file: format!("/{prefix}/ca.pem"),
        }
    }

    #[test]
    fn otlp_receiver_comes_from_resolved_configuration() {
        let receiver = otlp::Receiver {
            logs_enabled: true,
            metrics_enabled: false,
            grpc: otlp::GrpcReceiver {
                endpoint: "0.0.0.0:4317".to_string(),
                max_recv_msg_size_mib: 11,
                transport: otlp::GrpcTransport::Unix,
                max_concurrent_streams: 12,
                keepalive: otlp::KeepaliveServerParameters {
                    max_connection_age: Duration::from_secs(13),
                    max_connection_age_grace: Duration::from_secs(14),
                    time: Duration::from_secs(15),
                    timeout: Duration::from_secs(16),
                },
                tls: tls("grpc"),
            },
            http: otlp::HttpReceiver {
                endpoint: "0.0.0.0:4318".to_string(),
                transport: "unix".to_string(),
                cors: otlp::Cors {
                    allowed_origins: vec!["https://origin.example.com".to_string()],
                    allowed_headers: vec!["x-allowed".to_string()],
                    exposed_headers: vec!["x-exposed".to_string()],
                    max_age: 17,
                },
                tls: tls("http"),
                max_request_body_size: 18,
            },
        };

        assert_eq!(
            OtlpReceiver {
                logs_enabled: true,
                metrics_enabled: false,
                grpc: OtlpGrpcReceiver {
                    endpoint: "0.0.0.0:4317".to_string(),
                    max_recv_msg_size_mib: 11,
                    transport: GrpcTransport::Unix,
                    max_concurrent_streams: 12,
                    keepalive: OtlpKeepalive {
                        max_connection_age: Duration::from_secs(13),
                        max_connection_age_grace: Duration::from_secs(14),
                        time: Duration::from_secs(15),
                        timeout: Duration::from_secs(16),
                    },
                    tls: expected_tls("grpc"),
                },
                http: OtlpHttpReceiver {
                    endpoint: "0.0.0.0:4318".to_string(),
                    transport: "unix".to_string(),
                    cors: CorsConfiguration {
                        allowed_origins: vec!["https://origin.example.com".to_string()],
                        allowed_headers: vec!["x-allowed".to_string()],
                        exposed_headers: vec!["x-exposed".to_string()],
                        max_age: 17,
                    },
                    tls: expected_tls("http"),
                    max_request_body_size: 18,
                },
            },
            otlp_receiver(&receiver)
        );
    }

    #[test]
    fn grpc_transport_maps_every_variant() {
        for (configured, expected) in [
            (otlp::GrpcTransport::Tcp, GrpcTransport::Tcp),
            (otlp::GrpcTransport::Unix, GrpcTransport::Unix),
        ] {
            let mut receiver = otlp::Receiver::default();
            receiver.grpc.transport = configured;

            assert_eq!(expected, otlp_receiver(&receiver).grpc.transport, "{configured:?}");
        }
    }

    #[test]
    fn otlp_traces_settings_come_from_resolved_configuration() {
        let traces = otlp::Traces {
            enabled: true,
            internal_port: 5003,
            probabilistic_sampler_sampling_percentage: 25.0,
            string_interner_size: NonZeroUsize::new(4096).unwrap(),
            enable_compute_top_level_by_span_kind: false,
            ignore_missing_datadog_fields: true,
        };

        assert_eq!(
            OtlpTracesSettings {
                string_interner_size: NonZeroUsize::new(4096).unwrap(),
                probabilistic_sampler_sampling_percentage: 25.0,
                enable_compute_top_level_by_span_kind: false,
                ignore_missing_datadog_fields: true,
            },
            otlp_traces_settings(&traces)
        );
    }

    #[test]
    fn otlp_metrics_settings_come_from_resolved_configuration() {
        let metrics = otlp::Metrics {
            tag_cardinality: ModelOriginTagCardinality::High,
            histogram_mode: otlp::HistogramMode::Counters,
            send_histogram_aggregations: true,
            resource_attributes_as_tags: true,
            instrumentation_scope_metadata_as_tags: false,
            sums: otlp::Sums {
                cumulative_monotonic_mode: otlp::CumulativeMonotonicMode::RawValue,
                initial_cumulative_monotonic_value: otlp::InitialCumulativeMonotonicValue::Keep,
            },
            tags: "env:prod".to_string(),
            summaries: otlp::Summaries {
                mode: otlp::SummaryMode::NoQuantiles,
            },
            delta_ttl: Duration::from_secs(21),
        };

        assert_eq!(
            OtlpMetricsSettings {
                tag_cardinality: OriginTagCardinality::High,
                histogram_mode: HistogramMode::Counters,
                send_histogram_aggregations: true,
                resource_attributes_as_tags: true,
                instrumentation_scope_metadata_as_tags: false,
                cumulative_monotonic_mode: CumulativeMonotonicMode::RawValue,
                initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue::Keep,
                tags: "env:prod".to_string(),
                summary_mode: SummaryMode::NoQuantiles,
                delta_ttl: Duration::from_secs(21),
            },
            otlp_metrics_settings(&metrics)
        );
    }

    #[test]
    fn otlp_metrics_enums_map_every_variant() {
        let settings = |metrics: otlp::Metrics| otlp_metrics_settings(&metrics);

        for (configured, expected) in [
            (otlp::HistogramMode::NoBuckets, HistogramMode::NoBuckets),
            (otlp::HistogramMode::Counters, HistogramMode::Counters),
            (otlp::HistogramMode::Distributions, HistogramMode::Distributions),
        ] {
            let metrics = otlp::Metrics {
                histogram_mode: configured,
                ..Default::default()
            };
            assert_eq!(expected, settings(metrics).histogram_mode, "{configured:?}");
        }

        for (configured, expected) in [
            (otlp::CumulativeMonotonicMode::ToDelta, CumulativeMonotonicMode::ToDelta),
            (
                otlp::CumulativeMonotonicMode::RawValue,
                CumulativeMonotonicMode::RawValue,
            ),
        ] {
            let mut metrics = otlp::Metrics::default();
            metrics.sums.cumulative_monotonic_mode = configured;
            assert_eq!(expected, settings(metrics).cumulative_monotonic_mode, "{configured:?}");
        }

        for (configured, expected) in [
            (
                otlp::InitialCumulativeMonotonicValue::Auto,
                InitialCumulativeMonotonicValue::Auto,
            ),
            (
                otlp::InitialCumulativeMonotonicValue::Drop,
                InitialCumulativeMonotonicValue::Drop,
            ),
            (
                otlp::InitialCumulativeMonotonicValue::Keep,
                InitialCumulativeMonotonicValue::Keep,
            ),
        ] {
            let mut metrics = otlp::Metrics::default();
            metrics.sums.initial_cumulative_monotonic_value = configured;
            assert_eq!(
                expected,
                settings(metrics).initial_cumulative_monotonic_value,
                "{configured:?}"
            );
        }

        for (configured, expected) in [
            (otlp::SummaryMode::Gauges, SummaryMode::Gauges),
            (otlp::SummaryMode::NoQuantiles, SummaryMode::NoQuantiles),
        ] {
            let mut metrics = otlp::Metrics::default();
            metrics.summaries.mode = configured;
            assert_eq!(expected, settings(metrics).summary_mode, "{configured:?}");
        }
    }

    #[test]
    fn otlp_context_settings_come_from_resolved_configuration() {
        let contexts = otlp::Contexts {
            allow_context_heap_allocs: false,
            cached_contexts_limit: 31,
            cached_tagsets_limit: 32,
            string_interner_size: 33,
        };

        assert_eq!(
            OtlpContextSettings {
                allow_context_heap_allocs: false,
                cached_contexts_limit: 31,
                cached_tagsets_limit: 32,
                string_interner_size: 33,
            },
            otlp_context_settings(&contexts)
        );
    }
}
