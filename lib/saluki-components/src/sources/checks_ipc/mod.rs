use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::checks::{
    check_data::Data,
    checks_server::{Checks, ChecksServer},
    event::{AlertType as ProtoAlertType, Event as ProtoEvent, Priority as ProtoPriority},
    log::{Log as ProtoLog, LogLevel},
    metric::{Metric as ProtoMetric, MetricType},
    service_check::{ServiceCheck as ProtoServiceCheck, Status as ServiceCheckStatus},
    SendCheckPayloadRequest, SendCheckPayloadResponse,
};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::HandleExt as _;
use saluki_context::tags::{Tag, TagSet};
use saluki_context::Context;
use saluki_core::data_model::event::eventd::{AlertType, EventD, Priority};
use saluki_core::data_model::event::log::Log;
use saluki_core::data_model::event::metric::Metric;
use saluki_core::data_model::event::service_check::{CheckStatus, ServiceCheck};
use saluki_core::data_model::event::{Event, EventType};
use saluki_core::topology::OutputDefinition;
use saluki_core::{
    components::{sources::*, ComponentContext},
    data_model::event::log::LogStatus,
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::sync::mpsc;
use tokio::{pin, select};
use tonic::transport::Server;
use tonic::{Response, Status};
use tracing::{debug, trace, warn};

const fn default_grpc_endpoint() -> ListenAddress {
    ListenAddress::any_tcp(5105)
}

/// Checks IPC source.
#[derive(Debug, Deserialize)]
pub struct ChecksIPCConfiguration {
    #[serde(rename = "checks_ipc_endpoint", default = "default_grpc_endpoint")]
    grpc_endpoint: ListenAddress,
}

impl ChecksIPCConfiguration {
    /// Creates a new `ChecksIPCConfiguration` from a native endpoint.
    pub fn from_native(grpc_endpoint: ListenAddress) -> Self {
        Self { grpc_endpoint }
    }
}

#[async_trait]
impl SourceBuilder for ChecksIPCConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("logs", EventType::Log),
                OutputDefinition::named_output("events", EventType::EventD),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
            ]
        });

        &OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ChecksIPC {
            grpc_endpoint: self.grpc_endpoint.clone(),
        }))
    }
}

impl MemoryBounds for ChecksIPCConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<ChecksIPC>("checks_ipc");
    }
}

struct ChecksIPC {
    grpc_endpoint: ListenAddress,
}

#[async_trait]
impl Source for ChecksIPC {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();

        let (events_tx, mut events_rx) = mpsc::channel(16);

        let grpc_server = Server::builder().add_service(ChecksServer::new(ChecksService { events_tx }));

        let grpc_socket_addr = match self.grpc_endpoint {
            ListenAddress::Tcp(addr) => addr,
            _ => return Err(generic_error!("OTLP gRPC endpoint must be a TCP address.")),
        };
        context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("checks-ipc-grpc-server", grpc_server.serve(grpc_socket_addr));

        health.mark_ready();
        debug!("Checks IPC source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                Some(event) = events_rx.recv() => {
                    let output_name = match &event {
                        Event::Metric(_) => "metrics",
                        Event::Log(_) => "logs",
                        Event::EventD(_) => "events",
                        Event::ServiceCheck(_) => "service_checks",
                        _ => continue,
                    };

                    if let Err(e) = context.dispatcher().dispatch_one_named(output_name, event).await {
                        warn!("Failed to dispatch {output_name} event: {:?}", e);
                    }
                },
            }
        }

        debug!("Checks IPC source stopped.");
        Ok(())
    }
}

struct ChecksService {
    events_tx: mpsc::Sender<Event>,
}

#[async_trait]
impl Checks for ChecksService {
    async fn send_check_payload(
        &self, request: tonic::Request<SendCheckPayloadRequest>,
    ) -> Result<Response<SendCheckPayloadResponse>, Status> {
        trace!("Received check payload.");

        let payload = request.into_inner();
        for check_data in payload.data.into_iter().filter_map(|data| data.data) {
            let Some(event) = check_data_to_event(check_data) else {
                continue;
            };

            if let Err(e) = self.events_tx.send(event).await {
                warn!("Failed to send check event: {:?}", e);
            }
        }

        Ok(Response::new(SendCheckPayloadResponse {}))
    }
}

fn check_data_to_event(check_data: Data) -> Option<Event> {
    // Each arm exhaustively destructures its proto message (no `..`) so adding a new field
    // upstream becomes a compile error here until it's mapped or explicitly ignored.
    match check_data {
        Data::Metric(metric) => {
            let ProtoMetric {
                r#type,
                name,
                value,
                timestamp,
                tags,
                hostname,
                interval_secs,
            } = metric;

            let metric_type = MetricType::try_from(r#type).ok()?;

            let tags = tags.into_iter().map(Tag::from).collect::<TagSet>();
            let context = Context::from_parts(name, tags.into_shared());
            let mut metric = match metric_type {
                MetricType::Counter => Metric::counter(context, (timestamp, value)),
                MetricType::Gauge => Metric::gauge(context, (timestamp, value)),
                MetricType::Rate => {
                    if interval_secs == 0 {
                        warn!("Received rate metric from check with interval of zero. Skipping.");
                        return None;
                    }
                    Metric::rate(context, (timestamp, value), Duration::from_secs(interval_secs))
                }
                MetricType::Histogram => Metric::histogram(context, (timestamp, value)),
                MetricType::Unspecified => {
                    warn!("Received metric with unspecified type. Skipping.");
                    return None;
                }
            };
            if !hostname.is_empty() {
                metric.metadata_mut().set_hostname(Arc::from(hostname));
            }
            Some(Event::Metric(metric))
        }
        Data::Log(log) => {
            let ProtoLog { message, level } = log;

            let level = LogLevel::try_from(level).ok()?;
            let status = log_level_to_log_status(level);

            Some(Event::Log(Log::new(message).with_status(status)))
        }
        Data::Event(event) => {
            let ProtoEvent {
                title,
                text,
                priority,
                hostname,
                tags,
                alert_type,
                aggregation_key,
                source_type_name,
                timestamp,
            } = event;

            let tags = tags.into_iter().map(Tag::from).collect::<TagSet>();
            let mut eventd = EventD::new(title, text)
                .with_timestamp(timestamp)
                .with_tags(tags.into_shared());

            if !hostname.is_empty() {
                eventd.set_hostname(MetaString::from(hostname));
            }
            if !aggregation_key.is_empty() {
                eventd.set_aggregation_key(MetaString::from(aggregation_key));
            }
            if !source_type_name.is_empty() {
                eventd.set_source_type_name(MetaString::from(source_type_name));
            }
            if let Some(p) = ProtoPriority::try_from(priority)
                .ok()
                .and_then(proto_priority_to_priority)
            {
                eventd.set_priority(p);
            }
            if let Some(a) = ProtoAlertType::try_from(alert_type)
                .ok()
                .and_then(proto_alert_type_to_alert_type)
            {
                eventd.set_alert_type(a);
            }
            Some(Event::EventD(eventd))
        }
        Data::ServiceCheck(sc) => {
            let ProtoServiceCheck {
                status,
                name,
                message,
                tags,
                hostname,
            } = sc;

            let Some(status) = ServiceCheckStatus::try_from(status)
                .ok()
                .and_then(service_check_status_to_check_status)
            else {
                warn!(
                    "Received service check with unspecified or invalid status: {}. Skipping.",
                    status
                );
                return None;
            };
            let tags = tags.into_iter().map(Tag::from).collect::<TagSet>();
            let mut service_check = ServiceCheck::new(name, status)
                .with_message(MetaString::from(message))
                .with_tags(tags.into_shared());
            if !hostname.is_empty() {
                service_check.set_hostname(MetaString::from(hostname));
            }
            Some(Event::ServiceCheck(service_check))
        }
    }
}

fn log_level_to_log_status(log_level: LogLevel) -> LogStatus {
    match log_level {
        LogLevel::Trace => LogStatus::Trace,
        LogLevel::Debug => LogStatus::Debug,
        LogLevel::Info => LogStatus::Info,
        LogLevel::Warning => LogStatus::Warning,
        LogLevel::Error => LogStatus::Error,
        LogLevel::Critical => LogStatus::Emergency,
        _ => LogStatus::Info,
    }
}

fn service_check_status_to_check_status(status: ServiceCheckStatus) -> Option<CheckStatus> {
    match status {
        ServiceCheckStatus::Ok => Some(CheckStatus::Ok),
        ServiceCheckStatus::Warning => Some(CheckStatus::Warning),
        ServiceCheckStatus::Critical => Some(CheckStatus::Critical),
        ServiceCheckStatus::Unknown => Some(CheckStatus::Unknown),
        ServiceCheckStatus::Unspecified => None,
    }
}

fn proto_priority_to_priority(priority: ProtoPriority) -> Option<Priority> {
    match priority {
        ProtoPriority::Normal => Some(Priority::Normal),
        ProtoPriority::Low => Some(Priority::Low),
        ProtoPriority::Unspecified => None,
    }
}

fn proto_alert_type_to_alert_type(alert_type: ProtoAlertType) -> Option<AlertType> {
    match alert_type {
        ProtoAlertType::Info => Some(AlertType::Info),
        ProtoAlertType::Error => Some(AlertType::Error),
        ProtoAlertType::Warning => Some(AlertType::Warning),
        ProtoAlertType::Success => Some(AlertType::Success),
        ProtoAlertType::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use datadog_protos::checks::{
        check_data::Data,
        event::Event as ProtoEvent,
        log::Log as ProtoLog,
        metric::{Metric as ProtoMetric, MetricType as ProtoMetricType},
        service_check::{ServiceCheck as ProtoServiceCheck, Status as ProtoServiceCheckStatus},
    };
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;

    fn metric_data(
        r#type: i32, name: &str, value: f64, timestamp: u64, interval_secs: u64, tags: &[&str], hostname: &str,
    ) -> Data {
        Data::Metric(ProtoMetric {
            r#type,
            name: name.to_string(),
            value,
            timestamp,
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            hostname: hostname.to_string(),
            interval_secs,
        })
    }

    fn log_data(level: i32, message: &str) -> Data {
        Data::Log(ProtoLog {
            message: message.to_string(),
            level,
        })
    }

    fn event_data(title: &str, text: &str, timestamp: u64, tags: &[&str], hostname: &str) -> Data {
        Data::Event(ProtoEvent {
            title: title.to_string(),
            text: text.to_string(),
            priority: 0,
            hostname: hostname.to_string(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            alert_type: 0,
            aggregation_key: String::new(),
            source_type_name: String::new(),
            timestamp,
        })
    }

    fn service_check_data(status: i32, name: &str, message: &str, tags: &[&str], hostname: &str) -> Data {
        Data::ServiceCheck(ProtoServiceCheck {
            status,
            name: name.to_string(),
            message: message.to_string(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            hostname: hostname.to_string(),
        })
    }

    #[test]
    fn metric_counter_conversion() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Counter as i32,
            "my_counter",
            1.0,
            1234,
            0,
            &["tag1:value1", "tag2:value2"],
            "",
        ))
        .expect("counter should convert");

        let Event::Metric(metric) = event else {
            panic!("expected Metric event");
        };
        assert_eq!(metric.context().name().as_ref(), "my_counter");
        assert!(metric.context().tags().has_tag("tag1:value1"));
        assert!(metric.context().tags().has_tag("tag2:value2"));
        assert!(matches!(metric.values(), MetricValues::Counter(_)));
    }

    #[test]
    fn metric_gauge_conversion() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Gauge as i32,
            "my_gauge",
            42.0,
            1234,
            0,
            &[],
            "",
        ))
        .expect("gauge should convert");
        let Event::Metric(metric) = event else {
            panic!("expected Metric event");
        };
        assert!(matches!(metric.values(), MetricValues::Gauge(_)));
    }

    #[test]
    fn metric_histogram_conversion() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Histogram as i32,
            "my_hist",
            1.0,
            1234,
            0,
            &[],
            "",
        ))
        .expect("histogram should convert");
        let Event::Metric(metric) = event else {
            panic!("expected Metric event");
        };
        assert!(matches!(metric.values(), MetricValues::Histogram(_)));
    }

    #[test]
    fn metric_rate_conversion_uses_interval() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Rate as i32,
            "my_rate",
            10.0,
            1234,
            60,
            &[],
            "",
        ))
        .expect("rate should convert");
        let Event::Metric(metric) = event else {
            panic!("expected Metric event");
        };
        match metric.values() {
            MetricValues::Rate(_, interval) => assert_eq!(*interval, Duration::from_secs(60)),
            other => panic!("expected Rate values, got {other:?}"),
        }
    }

    #[test]
    fn metric_rate_with_zero_interval_is_skipped() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Rate as i32,
            "my_rate",
            10.0,
            1234,
            0,
            &[],
            "",
        ));
        assert!(event.is_none(), "rate with zero interval must be skipped");
    }

    #[test]
    fn metric_unspecified_type_is_skipped() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Unspecified as i32,
            "x",
            1.0,
            1234,
            0,
            &[],
            "",
        ));
        assert!(event.is_none(), "unspecified metric type must be skipped");
    }

    #[test]
    fn metric_unknown_type_is_skipped() {
        // Any i32 outside the proto enum range fails MetricType::try_from.
        let event = check_data_to_event(metric_data(99, "x", 1.0, 1234, 0, &[], ""));
        assert!(event.is_none(), "unknown metric type must be skipped");
    }

    #[test]
    fn log_unknown_level_is_skipped() {
        // 99 is not part of the LogLevel proto enum, so try_from returns Err.
        let event = check_data_to_event(log_data(99, "hello"));
        assert!(event.is_none(), "unknown log level must be skipped");
    }

    #[test]
    fn event_conversion_preserves_fields() {
        let event = check_data_to_event(event_data("title", "body", 1234, &["env:prod", "team:foo"], ""))
            .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.title(), "title");
        assert_eq!(ev.text(), "body");
        assert_eq!(ev.timestamp(), Some(1234));
        assert!(ev.tags().has_tag("env:prod"));
        assert!(ev.tags().has_tag("team:foo"));
    }

    #[test]
    fn service_check_status_mapping() {
        let cases = [
            (ProtoServiceCheckStatus::Ok, CheckStatus::Ok),
            (ProtoServiceCheckStatus::Warning, CheckStatus::Warning),
            (ProtoServiceCheckStatus::Critical, CheckStatus::Critical),
            (ProtoServiceCheckStatus::Unknown, CheckStatus::Unknown),
        ];

        for (proto_status, expected) in cases {
            let event = check_data_to_event(service_check_data(proto_status as i32, "n", "m", &[], ""))
                .unwrap_or_else(|| panic!("status {proto_status:?} should convert"));
            let Event::ServiceCheck(sc) = event else {
                panic!("expected ServiceCheck event for {proto_status:?}");
            };
            assert_eq!(sc.status(), expected, "status {proto_status:?}");
        }
    }

    #[test]
    fn service_check_unspecified_status_is_skipped() {
        let event = check_data_to_event(service_check_data(
            ProtoServiceCheckStatus::Unspecified as i32,
            "n",
            "m",
            &[],
            "",
        ));
        assert!(event.is_none(), "service check with unspecified status must be skipped");
    }

    #[test]
    fn service_check_unknown_status_value_is_skipped() {
        // 99 is outside the proto Status enum, so try_from returns Err.
        let event = check_data_to_event(service_check_data(99, "n", "m", &[], ""));
        assert!(
            event.is_none(),
            "service check with out-of-range status must be skipped"
        );
    }

    #[test]
    fn service_check_preserves_name_message_and_tags() {
        let event = check_data_to_event(service_check_data(
            ProtoServiceCheckStatus::Ok as i32,
            "my.check",
            "all good",
            &["env:prod"],
            "",
        ))
        .expect("service check should convert");
        let Event::ServiceCheck(sc) = event else {
            panic!("expected ServiceCheck event");
        };
        assert_eq!(sc.name(), "my.check");
        assert_eq!(sc.status(), CheckStatus::Ok);
        assert_eq!(sc.message(), Some("all good"));
        assert!(sc.tags().has_tag("env:prod"));
    }

    #[test]
    fn metric_hostname_propagates() {
        let event = check_data_to_event(metric_data(
            ProtoMetricType::Counter as i32,
            "n",
            1.0,
            0,
            0,
            &[],
            "host-a",
        ))
        .expect("metric should convert");
        let Event::Metric(m) = event else {
            panic!("expected Metric event");
        };
        assert_eq!(m.metadata().hostname(), Some("host-a"));
    }

    #[test]
    fn metric_empty_hostname_stays_unset() {
        let event = check_data_to_event(metric_data(ProtoMetricType::Counter as i32, "n", 1.0, 0, 0, &[], ""))
            .expect("metric should convert");
        let Event::Metric(m) = event else {
            panic!("expected Metric event");
        };
        assert_eq!(m.metadata().hostname(), None);
    }

    #[test]
    fn eventd_hostname_propagates() {
        let event = check_data_to_event(event_data("title", "body", 0, &[], "host-b")).expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.hostname(), Some("host-b"));
    }

    #[test]
    fn eventd_empty_hostname_stays_unset() {
        let event = check_data_to_event(event_data("title", "body", 0, &[], "")).expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.hostname(), None);
    }

    #[test]
    fn service_check_hostname_propagates() {
        let event = check_data_to_event(service_check_data(
            ProtoServiceCheckStatus::Ok as i32,
            "n",
            "m",
            &[],
            "host-c",
        ))
        .expect("service check should convert");
        let Event::ServiceCheck(sc) = event else {
            panic!("expected ServiceCheck event");
        };
        assert_eq!(sc.hostname(), Some("host-c"));
    }

    #[test]
    fn service_check_empty_hostname_stays_unset() {
        let event = check_data_to_event(service_check_data(
            ProtoServiceCheckStatus::Ok as i32,
            "n",
            "m",
            &[],
            "",
        ))
        .expect("service check should convert");
        let Event::ServiceCheck(sc) = event else {
            panic!("expected ServiceCheck event");
        };
        assert_eq!(sc.hostname(), None);
    }

    #[test]
    fn eventd_priority_propagates() {
        let event = check_data_to_event(Data::Event(ProtoEvent {
            priority: ProtoPriority::Low as i32,
            ..Default::default()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.priority(), Some(Priority::Low));
    }

    #[test]
    fn eventd_alert_type_propagates() {
        let event = check_data_to_event(Data::Event(ProtoEvent {
            alert_type: ProtoAlertType::Warning as i32,
            ..Default::default()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.alert_type(), Some(AlertType::Warning));
    }

    #[test]
    fn eventd_aggregation_key_propagates() {
        let event = check_data_to_event(Data::Event(ProtoEvent {
            aggregation_key: "agg-key-1".to_string(),
            ..Default::default()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.aggregation_key(), Some("agg-key-1"));
    }

    #[test]
    fn eventd_source_type_name_propagates() {
        let event = check_data_to_event(Data::Event(ProtoEvent {
            source_type_name: "my-source".to_string(),
            ..Default::default()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.source_type_name(), Some("my-source"));
    }

    #[test]
    fn eventd_unspecified_proto_keeps_saluki_defaults() {
        // A default-initialized ProtoEvent has priority=0 (Unspecified), alert_type=0 (Unspecified),
        // and all strings empty. Our mapping treats Unspecified as "source did not set it", so
        // `EventD::new`'s defaults (priority=Normal, alert_type=Info) survive, while the empty
        // string fields stay unset.
        let event = check_data_to_event(Data::Event(ProtoEvent::default())).expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.priority(), Some(Priority::Normal));
        assert_eq!(ev.alert_type(), Some(AlertType::Info));
        assert_eq!(ev.aggregation_key(), None);
        assert_eq!(ev.source_type_name(), None);
        assert_eq!(ev.hostname(), None);
    }
}
