//! Inputs of the OTLP source.

#[cfg(unix)]
use std::path::PathBuf;

use agent_data_plane_config::domains::{otlp, traces::ApmFeature};
use saluki_components::sources::{
    ContainerTagPromotion, CorsConfiguration, CumulativeMonotonicMode, GrpcTransport, HistogramMode,
    InitialCumulativeMonotonicValue, OtlpContextSettings, OtlpDebugVerbosity, OtlpGrpcReceiver, OtlpHttpReceiver,
    OtlpKeepalive, OtlpLogsSettings, OtlpMetricsSettings, OtlpProxyDestinations, OtlpReceiver, OtlpReceiverTls,
    OtlpTracesSettings, ProxyDestination, SummaryMode,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use super::origin_tag_cardinality;

/// Builds the OTLP receiver settings.
pub(crate) fn otlp_receiver(receiver: &otlp::Receiver) -> OtlpReceiver {
    let grpc = &receiver.grpc;
    let http = &receiver.http;

    OtlpReceiver {
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

/// Builds the OTLP proxy destinations, the Agent's relocated OTLP receivers.
///
/// Each destination follows the transport of its protocol's receiver: a `host:port` address, or an absolute Unix domain
/// socket path. A protocol whose receiver has no endpoint has no listener to proxy from, so its destination is left out
/// and isn't checked.
///
/// # Errors
///
/// If the destination of a protocol with a listener isn't valid for its transport, an error is returned.
pub(crate) fn otlp_proxy_destinations(otlp: &otlp::Domain) -> Result<OtlpProxyDestinations, GenericError> {
    let receiver = &otlp.receiver;
    Ok(OtlpProxyDestinations {
        grpc: (!receiver.grpc.endpoint.is_empty())
            .then(|| {
                proxy_destination(
                    &otlp.proxy.grpc_endpoint,
                    receiver.grpc.transport == otlp::GrpcTransport::Unix,
                    "data_plane.experimental.otlp.proxy.agent_receiver_grpc_endpoint",
                )
            })
            .transpose()?,
        http: (!receiver.http.endpoint.is_empty())
            .then(|| {
                proxy_destination(
                    &otlp.proxy.http_endpoint,
                    receiver.http.transport == "unix",
                    "data_plane.experimental.otlp.proxy.agent_receiver_http_endpoint",
                )
            })
            .transpose()?,
    })
}

fn proxy_destination(address: &str, unix: bool, key: &str) -> Result<ProxyDestination, GenericError> {
    if unix {
        // The default destination is a `host:port` address, which as a path is relative.
        #[cfg(unix)]
        {
            let path = PathBuf::from(address);
            if !path.is_absolute() {
                return Err(generic_error!(
                    "Invalid OTLP proxy destination `{key}` ({address}): the OTLP receiver transport is `unix`, so set \
                     it to the absolute path of the Agent's relocated OTLP receiver socket."
                ));
            }
            return Ok(ProxyDestination::Unix(path));
        }
        #[cfg(not(unix))]
        return Err(generic_error!(
            "Invalid OTLP proxy destination `{key}`: Unix domain sockets aren't supported on this platform."
        ));
    }

    ProxyDestination::from_tcp_address(address).with_error_context(|| format!("Invalid OTLP proxy destination `{key}`"))
}

/// Builds the OTLP logs translation settings.
pub(crate) fn otlp_logs_settings(logs: &otlp::Logs) -> OtlpLogsSettings {
    OtlpLogsSettings {
        tags_as_ddtags: logs.tags_as_ddtags,
    }
}

/// Builds the OTLP trace translation settings from the OTLP trace settings and the APM features.
pub(crate) fn otlp_traces_settings(traces: &otlp::Traces, features: &[ApmFeature]) -> OtlpTracesSettings {
    OtlpTracesSettings {
        string_interner_size: traces.string_interner_size,
        probabilistic_sampler_sampling_percentage: traces.probabilistic_sampler_sampling_percentage,
        enable_compute_top_level_by_span_kind: traces.enable_compute_top_level_by_span_kind,
        ignore_missing_datadog_fields: traces.ignore_missing_datadog_fields,
        infra_attributes_enabled: traces.infra_attributes_enabled,
        container_tag_promotion: match traces.container_tag_promotion {
            otlp::ContainerTagPromotion::Off => ContainerTagPromotion::Off,
            otlp::ContainerTagPromotion::Duplicate => ContainerTagPromotion::Duplicate,
            otlp::ContainerTagPromotion::Rename => ContainerTagPromotion::Rename,
        },
        span_name_as_resource_name: traces.span_name_as_resource_name,
        span_name_remappings: traces.span_name_remappings.clone(),
        disable_operation_and_resource_name_logic_v2: features
            .contains(&ApmFeature::DisableOperationAndResourceNameLogicV2),
        disable_receive_resource_spans_v2: features.contains(&ApmFeature::DisableReceiveResourceSpansV2),
        disable_otel_scope_convention: features.contains(&ApmFeature::DisableOtelScopeConvention),
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

/// Builds the verbosity of the OTLP debug logging.
pub(crate) fn otlp_debug_verbosity(debug: &otlp::DebugOutput) -> OtlpDebugVerbosity {
    match debug.verbosity {
        otlp::DebugVerbosity::None => OtlpDebugVerbosity::None,
        otlp::DebugVerbosity::Basic => OtlpDebugVerbosity::Basic,
        otlp::DebugVerbosity::Normal => OtlpDebugVerbosity::Normal,
        otlp::DebugVerbosity::Detailed => OtlpDebugVerbosity::Detailed,
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
    use std::collections::HashMap;
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
                tls_configured: false,
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
                tls_configured: false,
                max_request_body_size: 18,
            },
        };

        assert_eq!(
            OtlpReceiver {
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

    /// Returns an OTLP configuration whose receivers have an endpoint for each protocol.
    fn listening_domain() -> otlp::Domain {
        let mut otlp = otlp::Domain::default();
        otlp.receiver.grpc.endpoint = "0.0.0.0:4317".to_string();
        otlp.receiver.http.endpoint = "0.0.0.0:4318".to_string();
        otlp
    }

    #[test]
    fn otlp_proxy_destinations_follow_each_receiver_transport() {
        let tcp = |host: &str, port| {
            Some(ProxyDestination::Tcp {
                host: host.to_string(),
                port,
            })
        };
        let mut otlp = listening_domain();
        otlp.receiver.grpc.transport = otlp::GrpcTransport::Tcp;
        otlp.receiver.http.transport = "tcp".to_string();
        otlp.proxy.grpc_endpoint = "127.0.0.1:4319".to_string();
        otlp.proxy.http_endpoint = "localhost:4320".to_string();

        assert_eq!(
            otlp_proxy_destinations(&otlp).unwrap(),
            OtlpProxyDestinations {
                grpc: tcp("127.0.0.1", 4319),
                http: tcp("localhost", 4320),
            }
        );

        otlp.proxy.grpc_endpoint = "4319".to_string();
        let error = otlp_proxy_destinations(&otlp).unwrap_err();
        assert!(error.to_string().contains("agent_receiver_grpc_endpoint"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn otlp_proxy_destinations_are_sockets_for_unix_receivers() {
        let mut otlp = listening_domain();
        otlp.receiver.grpc.transport = otlp::GrpcTransport::Unix;
        otlp.receiver.http.transport = "unix".to_string();
        otlp.proxy.grpc_endpoint = "/run/agent/otlp-grpc.sock".to_string();
        otlp.proxy.http_endpoint = "/run/agent/otlp-http.sock".to_string();

        assert_eq!(
            otlp_proxy_destinations(&otlp).unwrap(),
            OtlpProxyDestinations {
                grpc: Some(ProxyDestination::Unix(PathBuf::from("/run/agent/otlp-grpc.sock"))),
                http: Some(ProxyDestination::Unix(PathBuf::from("/run/agent/otlp-http.sock"))),
            }
        );

        otlp.proxy.grpc_endpoint = "127.0.0.1:4319".to_string();
        let error = otlp_proxy_destinations(&otlp).unwrap_err();
        assert!(error.to_string().contains("agent_receiver_grpc_endpoint"), "{error}");
    }

    #[test]
    fn otlp_proxy_destinations_ignore_an_empty_destination_without_a_listener() {
        // gRPC-only ingest needs no HTTP destination.
        let mut otlp = listening_domain();
        otlp.receiver.http.endpoint = String::new();
        otlp.proxy.grpc_endpoint = "127.0.0.1:4319".to_string();
        otlp.proxy.http_endpoint = String::new();

        assert_eq!(
            otlp_proxy_destinations(&otlp).unwrap(),
            OtlpProxyDestinations {
                grpc: Some(ProxyDestination::Tcp {
                    host: "127.0.0.1".to_string(),
                    port: 4319,
                }),
                http: None,
            }
        );

        // With an HTTP listener, the empty destination isn't valid.
        otlp.receiver.http.endpoint = "0.0.0.0:4318".to_string();
        let error = otlp_proxy_destinations(&otlp).unwrap_err();
        assert!(error.to_string().contains("agent_receiver_http_endpoint"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn otlp_proxy_destinations_skip_a_protocol_without_a_listener() {
        // The default gRPC destination is a relative path for a Unix receiver, but nothing is proxied over gRPC.
        let mut otlp = listening_domain();
        otlp.receiver.grpc.transport = otlp::GrpcTransport::Unix;
        otlp.receiver.grpc.endpoint = String::new();
        otlp.proxy.grpc_endpoint = "127.0.0.1:4319".to_string();
        otlp.proxy.http_endpoint = "127.0.0.1:4320".to_string();

        assert_eq!(
            otlp_proxy_destinations(&otlp).unwrap(),
            OtlpProxyDestinations {
                grpc: None,
                http: Some(ProxyDestination::Tcp {
                    host: "127.0.0.1".to_string(),
                    port: 4320,
                }),
            }
        );

        // An invalid destination of the protocol that listens still fails.
        otlp.proxy.http_endpoint = "4320".to_string();
        let error = otlp_proxy_destinations(&otlp).unwrap_err();
        assert!(error.to_string().contains("agent_receiver_http_endpoint"), "{error}");

        // Without a listener for either protocol, no destination is needed.
        otlp.receiver.http.endpoint = String::new();
        assert_eq!(
            otlp_proxy_destinations(&otlp).unwrap(),
            OtlpProxyDestinations { grpc: None, http: None }
        );
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
            infra_attributes_enabled: false,
            container_tag_promotion: otlp::ContainerTagPromotion::Duplicate,
            span_name_as_resource_name: true,
            span_name_remappings: HashMap::from([("old.name".to_string(), "new.name".to_string())]),
        };

        assert_eq!(
            OtlpTracesSettings {
                string_interner_size: NonZeroUsize::new(4096).unwrap(),
                probabilistic_sampler_sampling_percentage: 25.0,
                enable_compute_top_level_by_span_kind: false,
                ignore_missing_datadog_fields: true,
                infra_attributes_enabled: false,
                container_tag_promotion: ContainerTagPromotion::Duplicate,
                span_name_as_resource_name: true,
                span_name_remappings: HashMap::from([("old.name".to_string(), "new.name".to_string())]),
                disable_operation_and_resource_name_logic_v2: false,
                disable_receive_resource_spans_v2: false,
                disable_otel_scope_convention: false,
            },
            otlp_traces_settings(&traces, &[])
        );
    }

    #[test]
    fn otlp_traces_settings_take_each_disable_feature_from_its_apm_feature() {
        let traces = otlp::Traces::default();
        let settings = |features: &[ApmFeature]| {
            let settings = otlp_traces_settings(&traces, features);
            (
                settings.disable_operation_and_resource_name_logic_v2,
                settings.disable_receive_resource_spans_v2,
                settings.disable_otel_scope_convention,
            )
        };

        assert_eq!(settings(&[ApmFeature::TableNames]), (false, false, false));
        assert_eq!(
            settings(&[ApmFeature::DisableOperationAndResourceNameLogicV2]),
            (true, false, false)
        );
        assert_eq!(
            settings(&[ApmFeature::DisableReceiveResourceSpansV2]),
            (false, true, false)
        );
        assert_eq!(
            settings(&[ApmFeature::DisableOtelScopeConvention]),
            (false, false, true)
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

    #[test]
    fn otlp_debug_verbosity_comes_from_resolved_configuration() {
        for (configured, expected) in [
            (otlp::DebugVerbosity::None, OtlpDebugVerbosity::None),
            (otlp::DebugVerbosity::Basic, OtlpDebugVerbosity::Basic),
            (otlp::DebugVerbosity::Normal, OtlpDebugVerbosity::Normal),
            (otlp::DebugVerbosity::Detailed, OtlpDebugVerbosity::Detailed),
        ] {
            let debug = otlp::DebugOutput { verbosity: configured };

            assert_eq!(otlp_debug_verbosity(&debug), expected);
        }
    }
}
