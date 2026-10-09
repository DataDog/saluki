use std::io;
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use super::*;

fn metric() -> Metric {
    Metric {
        metric_type: 3,
        name: "é".into(),
        value: -0.0,
        timestamp: 0x0102_0304_0506_0708,
        tags: vec!["a".into(), String::new()],
        hostname: "h".into(),
        interval_secs: 9,
    }
}

fn log() -> Log {
    Log {
        message: "hi".into(),
        level: 40,
    }
}

fn service_check() -> ServiceCheck {
    ServiceCheck {
        status: 2,
        name: "a".into(),
        message: String::new(),
        tags: vec!["x".into()],
        hostname: "h".into(),
    }
}

fn event() -> Event {
    Event {
        title: "t".into(),
        text: "x".into(),
        priority: 2,
        hostname: String::new(),
        tags: vec!["q".into()],
        alert_type: 4,
        aggregation_key: "k".into(),
        source_type_name: "s".into(),
        timestamp: 5,
    }
}

#[test]
fn metric_bytes_preserve_field_order_utf8_and_float_bits() {
    let expected = [
        3, 0, 0, 0, // type
        2, 0, 0, 0, 0xc3, 0xa9, // name
        0, 0, 0, 0, 0, 0, 0, 0x80, // -0.0
        8, 7, 6, 5, 4, 3, 2, 1, // timestamp
        2, 0, 0, 0, // tag count
        1, 0, 0, 0, b'a', // first tag
        0, 0, 0, 0, // empty second tag
        1, 0, 0, 0, b'h', // hostname
        9, 0, 0, 0, 0, 0, 0, 0, // interval
    ];
    assert_eq!(metric().encode_payload().unwrap(), expected);
    let decoded = Metric::decode_payload(&expected).unwrap();
    assert_eq!(decoded.value.to_bits(), (-0.0f64).to_bits());
    assert_eq!(decoded, metric());
}

#[test]
fn other_record_types_have_stable_golden_bytes() {
    let log_bytes = [2, 0, 0, 0, b'h', b'i', 40, 0, 0, 0];
    assert_eq!(log().encode_payload().unwrap(), log_bytes);
    assert_eq!(Log::decode_payload(&log_bytes).unwrap(), log());

    let service_check_bytes = [
        2, 0, 0, 0, // status
        1, 0, 0, 0, b'a', // name
        0, 0, 0, 0, // message
        1, 0, 0, 0, // tag count
        1, 0, 0, 0, b'x', // tag
        1, 0, 0, 0, b'h', // hostname
    ];
    assert_eq!(service_check().encode_payload().unwrap(), service_check_bytes);
    assert_eq!(
        ServiceCheck::decode_payload(&service_check_bytes).unwrap(),
        service_check()
    );

    let event_bytes = [
        1, 0, 0, 0, b't', // title
        1, 0, 0, 0, b'x', // text
        2, 0, 0, 0, // priority
        0, 0, 0, 0, // hostname
        1, 0, 0, 0, // tag count
        1, 0, 0, 0, b'q', // tag
        4, 0, 0, 0, // alert type
        1, 0, 0, 0, b'k', // aggregation key
        1, 0, 0, 0, b's', // source type
        5, 0, 0, 0, 0, 0, 0, 0, // timestamp
    ];
    assert_eq!(event().encode_payload().unwrap(), event_bytes);
    assert_eq!(Event::decode_payload(&event_bytes).unwrap(), event());
}

#[test]
fn unknown_enum_numbers_round_trip_for_application_validation() {
    let mut value = metric();
    value.metric_type = 9876;
    assert_eq!(Metric::decode_payload(&value.encode_payload().unwrap()).unwrap(), value);
    let mut value = log();
    value.level = -7;
    assert_eq!(Log::decode_payload(&value.encode_payload().unwrap()).unwrap(), value);
}

#[test]
fn malformed_payloads_fail_before_unbounded_allocation() {
    let examples = [
        Message::Metric(metric()),
        Message::Log(log()),
        Message::ServiceCheck(service_check()),
        Message::Event(event()),
    ];
    for example in examples {
        let (kind, bytes) = example.encode_payload().unwrap();
        assert_eq!(example.encoded_len().unwrap(), bytes.len());
        for prefix in 0..bytes.len() {
            assert!(Message::decode_payload(kind, &bytes[..prefix]).is_err());
        }
        let mut with_trailing = bytes;
        with_trailing.push(1);
        assert!(Message::decode_payload(kind, &with_trailing).is_err());
    }
    assert!(Message::decode_payload(99, &[]).is_err());
    assert!(Log::decode_payload(&[1, 0, 0, 0, 0xff, 0, 0, 0, 0]).is_err());
    assert!(Log::decode_payload(&[u8::MAX; 4]).is_err());
    // A huge tag count is rejected before reserving memory for its elements.
    let mut bytes = metric().encode_payload().unwrap();
    let tags_offset = 4 + 4 + "é".len() + 8 + 8;
    bytes[tags_offset..tags_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Metric::decode_payload(&bytes).is_err());
    assert!(wire::add_size(wire::MAX_PAYLOAD, 1).is_err());
}

#[test]
fn notification_failure_preserves_the_published_count() {
    let outcome = BatchOutcome::from_core(SendResult {
        accepted: 2,
        rejection: None,
        notification_error: Some(io::Error::other("native wake failed")),
    });
    assert_eq!(outcome.accepted, 2);
    assert!(outcome.rejection.is_none());
    assert_eq!(outcome.notification_error.unwrap().kind(), io::ErrorKind::Other);
}

#[test]
fn batch_encoding_failure_retains_only_the_valid_prefix() {
    let prefix = encode_prefix(
        &[1, 2, 3],
        1024,
        |_| Ok(1),
        |value| {
            if *value == 2 {
                Err(io::Error::new(io::ErrorKind::InvalidInput, "bad second record"))
            } else {
                Ok((1, vec![*value]))
            }
        },
    )
    .unwrap();
    assert_eq!(prefix.records, vec![(1, vec![1])]);
    assert!(
        matches!(prefix.failure, Some(BatchRejection::Encoding(error)) if error.kind() == io::ErrorKind::InvalidInput)
    );

    let capacity_limited = encode_prefix(
        &[1, 2, 3],
        16,
        |_| Ok(1),
        |value| {
            if *value == 2 {
                Err(io::Error::new(io::ErrorKind::InvalidInput, "would fail encoding"))
            } else {
                Ok((1, vec![*value]))
            }
        },
    )
    .unwrap();
    assert_eq!(capacity_limited.records, vec![(1, vec![1])]);
    assert!(matches!(
        capacity_limited.failure,
        Some(BatchRejection::Queue(Rejection::Full))
    ));
}

fn check_block(source: &str, name: &str, expected: &[&str]) {
    let start = source.find(&format!("{name} {{")).unwrap();
    let body = source[start + name.len() + 2..].split_once('}').unwrap().0;
    let found: Vec<_> = body.lines().map(str::trim).filter(|line| line.ends_with(';')).collect();
    assert_eq!(found, expected, "Checks proto schema changed: {name}");
}

#[test]
fn proto_field_and_enum_contract_has_not_drifted() {
    let metric = include_str!("../../../protos/datadog/proto/checks/v1/metric.proto");
    let log = include_str!("../../../protos/datadog/proto/checks/v1/log.proto");
    let service_check = include_str!("../../../protos/datadog/proto/checks/v1/service_check.proto");
    let event = include_str!("../../../protos/datadog/proto/checks/v1/event.proto");
    let checks = include_str!("../../../protos/datadog/proto/checks/v1/checks.proto");
    check_block(
        metric,
        "message Metric",
        &[
            "MetricType type = 1;",
            "string name = 2;",
            "double value = 3;",
            "uint64 timestamp = 4;",
            "repeated string tags = 5;",
            "string hostname = 6;",
            "uint64 interval_secs = 7;",
        ],
    );
    check_block(log, "message Log", &["string message = 1;", "LogLevel level = 2;"]);
    check_block(
        service_check,
        "message ServiceCheck",
        &[
            "Status status = 1;",
            "string name = 2;",
            "string message = 3;",
            "repeated string tags = 4;",
            "string hostname = 5;",
        ],
    );
    check_block(
        event,
        "message Event",
        &[
            "string title = 1;",
            "string text = 2;",
            "Priority priority = 3;",
            "string hostname = 4;",
            "repeated string tags = 5;",
            "AlertType alert_type = 6;",
            "string aggregation_key = 7;",
            "string source_type_name = 8;",
            "uint64 timestamp = 9;",
        ],
    );
    check_block(
        checks,
        "oneof data",
        &[
            "datadog.checks.v1.metric.Metric metric = 1;",
            "datadog.checks.v1.log.Log log = 2;",
            "datadog.checks.v1.service_check.ServiceCheck service_check = 3;",
            "datadog.checks.v1.event.Event event = 4;",
        ],
    );
    check_block(
        metric,
        "enum MetricType",
        &[
            "METRIC_TYPE_UNSPECIFIED = 0;",
            "METRIC_TYPE_COUNTER = 1;",
            "METRIC_TYPE_RATE = 2;",
            "METRIC_TYPE_GAUGE = 3;",
            "METRIC_TYPE_HISTOGRAM = 4;",
        ],
    );
    check_block(
        log,
        "enum LogLevel",
        &[
            "LOG_LEVEL_UNSPECIFIED = 0;",
            "LOG_LEVEL_TRACE = 7;",
            "LOG_LEVEL_DEBUG = 10;",
            "LOG_LEVEL_INFO = 20;",
            "LOG_LEVEL_WARNING = 30;",
            "LOG_LEVEL_ERROR = 40;",
            "LOG_LEVEL_CRITICAL = 50;",
        ],
    );
    check_block(
        service_check,
        "enum Status",
        &[
            "STATUS_UNSPECIFIED = 0;",
            "STATUS_OK = 1;",
            "STATUS_WARNING = 2;",
            "STATUS_CRITICAL = 3;",
            "STATUS_UNKNOWN = 4;",
        ],
    );
    check_block(
        event,
        "enum Priority",
        &["PRIORITY_UNSPECIFIED = 0;", "PRIORITY_LOW = 1;", "PRIORITY_NORMAL = 2;"],
    );
    check_block(
        event,
        "enum AlertType",
        &[
            "ALERT_TYPE_UNSPECIFIED = 0;",
            "ALERT_TYPE_ERROR = 1;",
            "ALERT_TYPE_WARNING = 2;",
            "ALERT_TYPE_INFO = 3;",
            "ALERT_TYPE_SUCCESS = 4;",
        ],
    );
}

#[test]
fn mixed_batch_dispatches_each_type_after_tcp_setup() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let messages = vec![
        Message::Metric(metric()),
        Message::Log(log()),
        Message::ServiceCheck(service_check()),
        Message::Event(event()),
    ];
    let consumer = thread::spawn(move || {
        let mut config = ConsumerConfig::tcp(address);
        config.ring_capacity = 512;
        config.setup_timeout = Duration::from_secs(2);
        let mut consumer = Consumer::open(config).unwrap();
        (0..4).map(|_| consumer.receive().unwrap()).collect::<Vec<_>>()
    });
    let mut config = ProducerConfig::tcp(address);
    config.setup_timeout = Duration::from_secs(2);
    let mut producer = Producer::connect(config).unwrap();
    let outcome = producer.send_batch(&messages).unwrap();
    assert_eq!(outcome.accepted, messages.len());
    assert!(outcome.rejection.is_none());
    assert!(outcome.notification_error.is_none());
    assert_eq!(consumer.join().unwrap(), messages);
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err());
}

#[test]
fn mixed_records_preserve_order_across_many_ring_wraps() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let messages: Vec<_> = (0..1_000)
        .map(|sequence| match sequence % 4 {
            0 => {
                let mut value = metric();
                value.name = format!("metric.{sequence}");
                Message::Metric(value)
            }
            1 => Message::Log(Log {
                message: format!("log.{sequence}"),
                level: 20,
            }),
            2 => {
                let mut value = service_check();
                value.name = format!("check.{sequence}");
                Message::ServiceCheck(value)
            }
            _ => {
                let mut value = event();
                value.title = format!("event.{sequence}");
                Message::Event(value)
            }
        })
        .collect();
    let consumer = thread::spawn(move || {
        let mut config = ConsumerConfig::tcp(address);
        config.ring_capacity = 256;
        config.setup_timeout = Duration::from_secs(2);
        let mut consumer = Consumer::open(config).unwrap();
        (0..1_000).map(|_| consumer.receive().unwrap()).collect::<Vec<_>>()
    });
    let mut config = ProducerConfig::tcp(address);
    config.setup_timeout = Duration::from_secs(2);
    let mut producer = Producer::connect(config).unwrap();
    for message in &messages {
        loop {
            let outcome = producer.send_batch(std::slice::from_ref(message)).unwrap();
            assert!(outcome.notification_error.is_none());
            if outcome.accepted == 1 {
                break;
            }
            assert!(matches!(
                outcome.rejection,
                Some(BatchRejection::Queue(Rejection::Full))
            ));
            thread::yield_now();
        }
    }
    assert_eq!(consumer.join().unwrap(), messages);
}

#[test]
fn full_ring_reports_the_published_prefix_of_an_explicit_batch() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let consumer = thread::spawn(move || {
        let mut config = ConsumerConfig::tcp(address);
        config.ring_capacity = 128;
        config.setup_timeout = Duration::from_secs(2);
        let mut consumer = Consumer::open(config).unwrap();
        release_rx.recv().unwrap();
        vec![consumer.receive().unwrap(), consumer.receive().unwrap()]
    });
    let mut config = ProducerConfig::tcp(address);
    config.setup_timeout = Duration::from_secs(2);
    let mut producer = Producer::connect(config).unwrap();
    let oversized = Log {
        message: "x".repeat(200),
        level: 20,
    };
    let too_large = producer.send(&oversized).unwrap();
    assert_eq!(too_large.accepted, 0);
    assert!(matches!(
        too_large.rejection,
        Some(BatchRejection::Queue(Rejection::Oversized))
    ));
    let messages: Vec<_> = (0..3)
        .map(|n| {
            Message::Log(Log {
                message: format!("{n}{:0<31}", ""),
                level: 20,
            })
        })
        .collect();
    let outcome = producer.send_batch(&messages).unwrap();
    assert_eq!(outcome.accepted, 2);
    assert!(matches!(
        outcome.rejection,
        Some(BatchRejection::Queue(Rejection::Full))
    ));
    assert!(outcome.notification_error.is_none());
    release_tx.send(()).unwrap();
    assert_eq!(consumer.join().unwrap(), messages[..2]);
}

fn anomaly_event() -> AnomalyEvent {
    AnomalyEvent {
        title: "AAD anomaly: system.load.1".into(),
        description: "severity=medium score=0.87 host=my-host".into(),
        timestamp: 0x0102_0304_0506_0708,
    }
}

/// These bytes are the cross-language pin: the Go copy of this protocol in the Agent
/// repository asserts the same literal, so any divergence in field order, prefixes, or
/// endianness fails on one side or the other.
#[test]
fn anomaly_event_bytes_are_the_documented_golden_vector() {
    let expected = [
        26, 0, 0, 0, // title length
        b'A', b'A', b'D', b' ', b'a', b'n', b'o', b'm', b'a', b'l', b'y', b':', b' ', b's', b'y', b's', b't', b'e',
        b'm', b'.', b'l', b'o', b'a', b'd', b'.', b'1', // "AAD anomaly: system.load.1"
        39, 0, 0, 0, // description length
        b's', b'e', b'v', b'e', b'r', b'i', b't', b'y', b'=', b'm', b'e', b'd', b'i', b'u', b'm', b' ', b's', b'c',
        b'o', b'r', b'e', b'=', b'0', b'.', b'8', b'7', b' ', b'h', b'o', b's', b't', b'=', b'm', b'y', b'-', b'h',
        b'o', b's', b't', // "severity=medium score=0.87 host=my-host"
        8, 7, 6, 5, 4, 3, 2, 1, // timestamp
    ];
    assert_eq!(anomaly_event().encode_payload().unwrap(), expected);
    assert_eq!(AnomalyEvent::decode_payload(&expected).unwrap(), anomaly_event());
    assert_eq!(anomaly_event().encoded_len().unwrap(), expected.len());
}

#[test]
fn anomaly_event_descriptor_names_the_event_protocol() {
    assert_eq!(&ANOMALY_EVENTS_DESCRIPTOR.id, b"AAD-EVNT");
    assert_eq!(ANOMALY_EVENTS_DESCRIPTOR.version, 1);
    assert_eq!(ANOMALY_EVENTS_DESCRIPTOR.message_types, &[ANOMALY_EVENT_TYPE_ID]);
    assert_eq!(ANOMALY_EVENT_TYPE_ID, 1);
}

#[test]
fn anomaly_events_round_trip_unicode_and_an_empty_description() {
    let event = AnomalyEvent {
        title: "AAD debug trigger: debug.trigger-anomaly.café".into(),
        description: String::new(),
        timestamp: 1_791_536_046,
    };
    let bytes = event.encode_payload().unwrap();
    assert_eq!(bytes.len(), event.encoded_len().unwrap());
    assert_eq!(AnomalyEvent::decode_payload(&bytes).unwrap(), event);
}

#[test]
fn oversized_anomaly_events_are_rejected_before_encoding() {
    let event = AnomalyEvent {
        title: "t".into(),
        description: "x".repeat(MAX_EVENT_PAYLOAD),
        timestamp: 1,
    };
    let error = event.encode_payload().expect_err("payload cap applies");
    assert!(error.to_string().contains("64 KiB"), "{error}");
    assert!(event.encoded_len().is_err());

    let oversized = vec![0u8; MAX_EVENT_PAYLOAD + 1];
    assert!(AnomalyEvent::decode_payload(&oversized).is_err());
}

#[test]
fn malformed_anomaly_event_payloads_are_rejected() {
    let golden = anomaly_event().encode_payload().unwrap();

    let truncated = &golden[..golden.len() - 1];
    let error = AnomalyEvent::decode_payload(truncated).expect_err("truncated payload");
    assert!(error.to_string().contains("truncated"), "{error}");

    let mut trailing = golden.clone();
    trailing.push(0);
    let error = AnomalyEvent::decode_payload(&trailing).expect_err("trailing bytes");
    assert!(error.to_string().contains("trailing"), "{error}");

    // A title length prefix that overruns the payload.
    let error = AnomalyEvent::decode_payload(&[255, 255, 255, 255, b'a']).expect_err("bad length");
    assert!(error.to_string().contains("truncated"), "{error}");
}
