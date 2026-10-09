use std::sync::LazyLock;
use std::time::Duration;
use std::{io, thread};

use async_trait::async_trait;
use datadog_checks_protocol::{Consumer as ChecksConsumer, Message};
use datadog_protos::checks::{
    event::{AlertType as ProtoAlertType, Priority as ProtoPriority},
    log::LogLevel,
    metric::MetricType,
    service_check::Status as ServiceCheckStatus,
};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{sources::*, BuildContext},
    data_model::{
        event::{
            eventd::{AlertType, EventD, Priority},
            log::{Log, LogStatus},
            metric::{context::Context, Metric},
            service_check::{CheckStatus, ServiceCheck},
            Event, EventType,
        },
        tags::{Tag, TagSet},
    },
    topology::OutputDefinition,
};
use saluki_error::{generic_error, GenericError};
use saluki_fit::{CancellationToken, ConsumerConfig};
use stringtheory::MetaString;
use tokio::sync::{mpsc, oneshot};
use tokio::{pin, select};
use tracing::{debug, warn};

/// Checks IPC source.
#[derive(Debug)]
pub struct ChecksIPCConfiguration {
    default_hostname: MetaString,
    consumer_config: ConsumerConfig,
}

impl ChecksIPCConfiguration {
    /// Creates a Checks FIT source from a validated consumer configuration and default hostname.
    pub fn new(consumer_config: ConsumerConfig, default_hostname: impl Into<MetaString>) -> Self {
        Self {
            default_hostname: default_hostname.into(),
            consumer_config,
        }
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

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ChecksIPC {
            consumer_config: self.consumer_config.clone(),
            default_hostname: self.default_hostname.clone(),
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
    consumer_config: ConsumerConfig,
    default_hostname: MetaString,
}

#[async_trait]
impl Source for ChecksIPC {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let ChecksIPC {
            consumer_config,
            default_hostname,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();

        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        // FIT's mapping is thread-local; create and use the consumer on one dedicated thread.
        // The bounded channel applies backpressure to this worker when downstream dispatch lags.
        let worker = thread::Builder::new()
            .name("checks-fit-receiver".to_string())
            .spawn(move || {
                receive_checks(
                    consumer_config,
                    default_hostname,
                    worker_cancellation,
                    ready_tx,
                    events_tx,
                )
            })
            .map_err(|error| generic_error!("Failed to start Checks FIT receiver: {error}"))?;

        let mut ready = false;
        let mut shutting_down = false;
        let mut result = Ok(());

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    shutting_down = true;
                    break;
                },
                _ = health.live() => continue,
                setup = &mut ready_rx, if !ready => {
                    match setup {
                        Ok(Ok(())) => {
                            ready = true;
                            health.mark_ready();
                            debug!("Checks FIT session established.");
                        }
                        Ok(Err(error)) => {
                            result = Err(generic_error!("Checks FIT setup failed: {error}"));
                            break;
                        }
                        Err(_) => {
                            result = Err(generic_error!("Checks FIT receiver stopped before setup completed."));
                            break;
                        }
                    }
                },
                event = events_rx.recv(), if ready => {
                    let Some(event) = event else {
                        // Once the worker closes the channel, report its transport failure below.
                        break;
                    };
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

        // Closing the receiver releases a worker blocked by channel backpressure. FIT cancellation
        // interrupts a worker blocked in setup or on the shared-memory notification word.
        drop(events_rx);
        let cancellation_result = cancellation.cancel();
        let worker_result = tokio::task::spawn_blocking(move || worker.join())
            .await
            .map_err(|error| generic_error!("Failed to join Checks FIT receiver: {error}"))?;
        match worker_result {
            Ok(Err(error)) if result.is_ok() && !(shutting_down && error.kind() == io::ErrorKind::Interrupted) => {
                result = Err(generic_error!("Checks FIT receiver failed: {error}"));
            }
            Err(_) if result.is_ok() => result = Err(generic_error!("Checks FIT receiver panicked.")),
            _ => {}
        }
        if let Err(error) = cancellation_result {
            if result.is_ok() {
                result = Err(generic_error!(
                    "Failed to wake Checks FIT receiver during shutdown: {error}"
                ));
            }
        }
        debug!("Checks IPC source stopped.");
        result
    }
}

fn receive_checks(
    config: ConsumerConfig, default_hostname: MetaString, cancellation: CancellationToken,
    ready: oneshot::Sender<io::Result<()>>, events: mpsc::Sender<Event>,
) -> io::Result<()> {
    let mut consumer = match ChecksConsumer::open_with_cancel(config, &cancellation) {
        Ok(consumer) => consumer,
        Err(error) => {
            let _ = ready.send(Err(io::Error::new(error.kind(), error.to_string())));
            return Err(error);
        }
    };
    let _ = ready.send(Ok(()));
    loop {
        let Some(message) = consumer.receive_with_cancel(&cancellation)? else {
            return Ok(());
        };
        if let Some(event) = check_data_to_event(message, &default_hostname) {
            if events.blocking_send(event).is_err() {
                return Ok(());
            }
        }
    }
}

fn check_data_to_event(check_data: Message, default_hostname: &MetaString) -> Option<Event> {
    // Each arm exhaustively destructures its FIT model (no `..`) so adding a new field
    // upstream becomes a compile error here until it's mapped or explicitly ignored.
    match check_data {
        Message::Metric(metric) => {
            let datadog_checks_protocol::Metric {
                metric_type,
                name,
                value,
                timestamp,
                tags,
                hostname,
                interval_secs,
            } = metric;

            let metric_type = MetricType::try_from(metric_type).ok()?;

            let tags = tags.into_iter().map(Tag::from).collect::<TagSet>();
            let mut context = Context::from_parts(name, tags.into_shared());
            let hostname = if hostname.is_empty() {
                default_hostname.clone()
            } else {
                MetaString::from(hostname)
            };
            context = context.with_host(Some(hostname));
            let metric = match metric_type {
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
            Some(Event::Metric(metric))
        }
        Message::Log(log) => {
            let datadog_checks_protocol::Log { message, level } = log;

            let level = LogLevel::try_from(level).ok()?;
            let status = log_level_to_log_status(level);

            Some(Event::Log(Log::new(message).with_status(status)))
        }
        Message::Event(event) => {
            let datadog_checks_protocol::Event {
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
        Message::ServiceCheck(sc) => {
            let datadog_checks_protocol::ServiceCheck {
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
    use datadog_checks_protocol::{
        Event as ProtoEvent, Log as ProtoLog, Metric as ProtoMetric, ServiceCheck as ProtoServiceCheck,
    };
    use datadog_protos::checks::{
        metric::MetricType as ProtoMetricType, service_check::Status as ProtoServiceCheckStatus,
    };
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;
    use datadog_checks_protocol::Producer as ChecksProducer;
    use saluki_fit::{ProducerConfig, SetupEndpoint};

    fn metric_data(
        r#type: i32, name: &str, value: f64, timestamp: u64, interval_secs: u64, tags: &[&str], hostname: &str,
    ) -> Message {
        Message::Metric(ProtoMetric {
            metric_type: r#type,
            name: name.to_string(),
            value,
            timestamp,
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            hostname: hostname.to_string(),
            interval_secs,
        })
    }

    fn log_data(level: i32, message: &str) -> Message {
        Message::Log(ProtoLog {
            message: message.to_string(),
            level,
        })
    }

    fn event_data(title: &str, text: &str, timestamp: u64, tags: &[&str], hostname: &str) -> Message {
        Message::Event(ProtoEvent {
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

    fn service_check_data(status: i32, name: &str, message: &str, tags: &[&str], hostname: &str) -> Message {
        Message::ServiceCheck(ProtoServiceCheck {
            status,
            name: name.to_string(),
            message: message.to_string(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            hostname: hostname.to_string(),
        })
    }

    fn default_event() -> ProtoEvent {
        ProtoEvent {
            title: String::new(),
            text: String::new(),
            priority: 0,
            hostname: String::new(),
            tags: Vec::new(),
            alert_type: 0,
            aggregation_key: String::new(),
            source_type_name: String::new(),
            timestamp: 0,
        }
    }

    fn check_data_to_event_for_tests(check_data: Message) -> Option<Event> {
        check_data_to_event(check_data, &MetaString::from_static("default-host"))
    }

    #[test]
    fn metric_counter_conversion() {
        let event = check_data_to_event_for_tests(metric_data(
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
        let event = check_data_to_event_for_tests(metric_data(
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
        let event = check_data_to_event_for_tests(metric_data(
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
        let event = check_data_to_event_for_tests(metric_data(
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
        let event = check_data_to_event_for_tests(metric_data(
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
        let event = check_data_to_event_for_tests(metric_data(
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
        let event = check_data_to_event_for_tests(metric_data(99, "x", 1.0, 1234, 0, &[], ""));
        assert!(event.is_none(), "unknown metric type must be skipped");
    }

    #[test]
    fn log_unknown_level_is_skipped() {
        // 99 is not part of the LogLevel proto enum, so try_from returns Err.
        let event = check_data_to_event_for_tests(log_data(99, "hello"));
        assert!(event.is_none(), "unknown log level must be skipped");
    }

    #[test]
    fn event_conversion_preserves_fields() {
        let event = check_data_to_event_for_tests(event_data("title", "body", 1234, &["env:prod", "team:foo"], ""))
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
            let event = check_data_to_event_for_tests(service_check_data(proto_status as i32, "n", "m", &[], ""))
                .unwrap_or_else(|| panic!("status {proto_status:?} should convert"));
            let Event::ServiceCheck(sc) = event else {
                panic!("expected ServiceCheck event for {proto_status:?}");
            };
            assert_eq!(sc.status(), expected, "status {proto_status:?}");
        }
    }

    #[test]
    fn service_check_unspecified_status_is_skipped() {
        let event = check_data_to_event_for_tests(service_check_data(
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
        let event = check_data_to_event_for_tests(service_check_data(99, "n", "m", &[], ""));
        assert!(
            event.is_none(),
            "service check with out-of-range status must be skipped"
        );
    }

    #[test]
    fn service_check_preserves_name_message_and_tags() {
        let event = check_data_to_event_for_tests(service_check_data(
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
        let event = check_data_to_event_for_tests(metric_data(
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
        assert_eq!(m.context().host(), Some("host-a"));
    }

    #[test]
    fn metric_empty_hostname_uses_default_host() {
        let event =
            check_data_to_event_for_tests(metric_data(ProtoMetricType::Counter as i32, "n", 1.0, 0, 0, &[], ""))
                .expect("metric should convert");
        let Event::Metric(m) = event else {
            panic!("expected Metric event");
        };
        assert_eq!(m.context().host(), Some("default-host"));
    }

    #[test]
    fn eventd_hostname_propagates() {
        let event =
            check_data_to_event_for_tests(event_data("title", "body", 0, &[], "host-b")).expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.hostname(), Some("host-b"));
    }

    #[test]
    fn eventd_empty_hostname_stays_unset() {
        let event =
            check_data_to_event_for_tests(event_data("title", "body", 0, &[], "")).expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.hostname(), None);
    }

    #[test]
    fn service_check_hostname_propagates() {
        let event = check_data_to_event_for_tests(service_check_data(
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
        let event = check_data_to_event_for_tests(service_check_data(
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
        let event = check_data_to_event_for_tests(Message::Event(ProtoEvent {
            priority: ProtoPriority::Low as i32,
            ..default_event()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.priority(), Some(Priority::Low));
    }

    #[test]
    fn eventd_alert_type_propagates() {
        let event = check_data_to_event_for_tests(Message::Event(ProtoEvent {
            alert_type: ProtoAlertType::Warning as i32,
            ..default_event()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.alert_type(), Some(AlertType::Warning));
    }

    #[test]
    fn eventd_aggregation_key_propagates() {
        let event = check_data_to_event_for_tests(Message::Event(ProtoEvent {
            aggregation_key: "agg-key-1".to_string(),
            ..default_event()
        }))
        .expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.aggregation_key(), Some("agg-key-1"));
    }

    #[test]
    fn eventd_source_type_name_propagates() {
        let event = check_data_to_event_for_tests(Message::Event(ProtoEvent {
            source_type_name: "my-source".to_string(),
            ..default_event()
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
        let event = check_data_to_event_for_tests(Message::Event(default_event())).expect("event should convert");
        let Event::EventD(ev) = event else {
            panic!("expected EventD event");
        };
        assert_eq!(ev.priority(), Some(Priority::Normal));
        assert_eq!(ev.alert_type(), Some(AlertType::Info));
        assert_eq!(ev.aggregation_key(), None);
        assert_eq!(ev.source_type_name(), None);
        assert_eq!(ev.hostname(), None);
    }

    #[tokio::test]
    async fn fit_receiver_establishes_session_and_delivers_event() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (ready_tx, ready_rx) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let worker_token = cancellation.clone();
        let consumer_config = ConsumerConfig::tcp(address);
        let worker = thread::spawn(move || {
            receive_checks(
                consumer_config,
                MetaString::from_static("default-host"),
                worker_token,
                ready_tx,
                events_tx,
            )
        });
        let producer = thread::spawn(move || {
            let mut producer = ChecksProducer::connect(ProducerConfig::for_endpoint(SetupEndpoint::Tcp(address)))
                .expect("producer connects");
            producer
                .send(&ProtoLog {
                    message: "hello".into(),
                    level: LogLevel::Info as i32,
                })
                .expect("log published");
        });

        ready_rx.await.expect("worker reports setup").expect("setup succeeds");
        let received = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
            .await
            .expect("event delivered")
            .expect("worker channel open");
        assert!(matches!(received, Event::Log(_)));
        producer.join().unwrap();
        cancellation.cancel().unwrap();
        drop(events_rx);
        assert!(worker.join().unwrap().is_ok());
    }

    #[tokio::test]
    async fn fit_receiver_reports_setup_timeout_before_readiness() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let (events_tx, _events_rx) = mpsc::channel(16);
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut consumer_config = ConsumerConfig::tcp(address);
        consumer_config.setup_timeout = Duration::from_millis(100);
        let worker = thread::spawn(move || {
            receive_checks(
                consumer_config,
                MetaString::from_static("default-host"),
                CancellationToken::new(),
                ready_tx,
                events_tx,
            )
        });

        assert_eq!(ready_rx.await.unwrap().unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
