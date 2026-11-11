use std::net::{IpAddr, Ipv4Addr};

use bytesize::ByteSize;

use super::*;
use crate::sources::dogstatsd::default_decoder_worker_count;
#[cfg(not(target_os = "linux"))]
use crate::sources::dogstatsd::OriginEnrichmentConfiguration;

fn udp_listen_address() -> ListenAddress {
    ListenAddress::udp_loopback(8125)
}

fn tcp_listen_address() -> ListenAddress {
    ListenAddress::tcp_loopback(8125)
}

#[test]
fn additional_tags_are_sorted_and_deduplicated() {
    let config = DogStatsDConfiguration {
        additional_tags: vec![
            "dogstatsd:configured".to_string(),
            "env:prod".to_string(),
            "provider_kind:autopilot".to_string(),
            "kube_distribution:eks".to_string(),
            "env:prod".to_string(),
        ],
        ..DogStatsDConfiguration::for_test()
    };

    assert_eq!(
        config.additional_tags(),
        [
            "dogstatsd:configured",
            "env:prod",
            "kube_distribution:eks",
            "provider_kind:autopilot",
        ]
    );
}

#[test]
fn statsd_forward_target_requires_host_and_port() {
    let cases = [
        ("no host", None, 9125, None),
        ("zero port", Some("127.0.0.1"), 0, None),
        ("host and port", Some("127.0.0.1"), 9125, Some(("127.0.0.1", 9125))),
    ];

    for (name, host, port, expected) in cases {
        let config = DogStatsDConfiguration {
            statsd_forward_host: host.map(MetaString::from),
            statsd_forward_port: port,
            ..DogStatsDConfiguration::for_test()
        };
        let target = config.statsd_forward_target().map(|(host, port)| (&**host, port));
        assert_eq!(target, expected, "{name}");
    }
}

#[test]
fn statsd_forward_invalid_target_still_builds_forwarder_handle() {
    let config = DogStatsDConfiguration {
        statsd_forward_host: Some(MetaString::from("not a valid host")),
        statsd_forward_port: 9125,
        ..DogStatsDConfiguration::for_test()
    };
    assert!(config.statsd_forward_target().is_some());
}

#[test]
fn autoscaling_disabled_yields_a_single_udp_stream() {
    let config = DogStatsDConfiguration {
        autoscale_udp_listeners: false,
        ..DogStatsDConfiguration::for_test()
    };
    assert!(config.udp_streams_to_yield().is_none());
}

#[test]
fn effective_max_buffer_count_never_below_baseline() {
    fn buffer_counts(buffer_count: usize, buffer_count_max: usize) -> DogStatsDConfiguration {
        DogStatsDConfiguration {
            buffer_count,
            buffer_count_max,
            ..DogStatsDConfiguration::for_test()
        }
    }

    // A config that only raised the baseline keeps its full capacity rather than being capped to the maximum.
    assert_eq!(buffer_counts(65536, 32_768).effective_max_buffer_count(), 65536);

    // An explicit maximum above the baseline is honored as-is.
    assert_eq!(buffer_counts(128, 512).effective_max_buffer_count(), 512);

    // A maximum below the baseline is treated as equal to the baseline.
    assert_eq!(buffer_counts(200, 64).effective_max_buffer_count(), 200);
}

#[test]
fn decoder_worker_count_matches_core_agent_defaults() {
    assert_eq!(default_decoder_worker_count(1), 2);
    assert_eq!(default_decoder_worker_count(4), 2);
    assert_eq!(default_decoder_worker_count(8), 6);
}

#[test]
fn decoder_worker_count_honors_explicit_override() {
    let config = DogStatsDConfiguration {
        workers_count: 1,
        ..DogStatsDConfiguration::for_test()
    };

    assert_eq!(config.decoder_worker_count().get(), 1);
}

#[test]
#[cfg(target_os = "linux")]
fn autoscale_udp_listeners_yields_multiple_streams_on_linux() {
    let config = DogStatsDConfiguration {
        autoscale_udp_listeners: true,
        ..DogStatsDConfiguration::for_test()
    };

    let streams = config
        .udp_streams_to_yield()
        .expect("autoscale yields at least 1 stream");
    let n = streams.get();
    assert!(
        (1..=4).contains(&n),
        "expected 1..=4 streams from vCPU formula, got {n}"
    );
}

#[test]
#[cfg(not(target_os = "linux"))]
fn warns_for_uds_origin_detection_on_non_linux() {
    let config = DogStatsDConfiguration {
        port: 0,
        socket_path: Some("/tmp/dsd.sock".to_string()),
        origin_enrichment: OriginEnrichmentConfiguration {
            enabled: true,
            ..OriginEnrichmentConfiguration::for_test()
        },
        ..DogStatsDConfiguration::for_test()
    };
    let addresses = config.build_addresses(None);

    assert!(config.uds_origin_detection_unsupported_on_platform(&addresses));
}

#[test]
#[cfg(not(target_os = "linux"))]
fn does_not_warn_for_udp_origin_detection_on_non_linux() {
    let config = DogStatsDConfiguration {
        origin_enrichment: OriginEnrichmentConfiguration {
            enabled: true,
            ..OriginEnrichmentConfiguration::for_test()
        },
        ..DogStatsDConfiguration::for_test()
    };
    let addresses = config.build_addresses(None);

    assert!(!config.uds_origin_detection_unsupported_on_platform(&addresses));
}

#[test]
#[cfg(not(target_os = "linux"))]
fn autoscale_udp_listeners_yields_a_single_stream_on_non_linux() {
    let config = DogStatsDConfiguration {
        autoscale_udp_listeners: true,
        ..DogStatsDConfiguration::for_test()
    };

    assert_eq!(None, config.udp_streams_to_yield());
}

#[test]
fn eol_required_matches_configured_listener_types() {
    let cases: &[(&[&str], ListenAddress, bool)] = &[
        (&["named_pipe"], named_pipe_listen_address(), true),
        (&["named_pipe"], udp_listen_address(), false),
        (&["named_pipe"], tcp_listen_address(), false),
        (&[], udp_listen_address(), false),
        (&[], tcp_listen_address(), false),
        (&["udp", "uds"], udp_listen_address(), true),
        (&["udp", "uds"], tcp_listen_address(), false),
        #[cfg(unix)]
        (&["udp", "uds"], ListenAddress::Unixgram("/tmp/dsd.sock".into()), true),
        #[cfg(unix)]
        (
            &["udp", "uds"],
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
            true,
        ),
        // Unrecognized listener types are ignored.
        (&["udp", "carrier_pigeon"], udp_listen_address(), true),
        (&["udp", "carrier_pigeon"], named_pipe_listen_address(), false),
    ];

    for (eol_required, listen_addr, expected) in cases {
        let config = DogStatsDConfiguration {
            eol_required: eol_required.iter().map(|value| value.to_string()).collect(),
            ..DogStatsDConfiguration::for_test()
        };
        assert_eq!(
            config.eol_required().for_listener(listen_addr),
            *expected,
            "eol_required {eol_required:?} for {listen_addr}"
        );
    }
}

#[test]
fn interner_size_prefers_explicit_bytes_over_entry_count() {
    // Each entry counts for 512 bytes.
    let cases = [
        // A Core Agent migration config with entry count 4096 should yield 2 MiB, not 4096 bytes.
        (4096, None, ByteSize::mib(2)),
        (8192, None, ByteSize::mib(4)),
        (4096, Some(ByteSize::b(4194304)), ByteSize::b(4194304)),
        (4096, Some(ByteSize::b(8388608)), ByteSize::b(8388608)),
    ];

    for (entry_count, size_bytes, expected) in cases {
        let config = DogStatsDConfiguration {
            context_string_interner_entry_count: entry_count,
            context_string_interner_size_bytes: size_bytes,
            ..DogStatsDConfiguration::for_test()
        };
        assert_eq!(
            config.effective_context_string_interner_bytes(),
            expected,
            "entry count {entry_count}, explicit bytes {size_bytes:?}"
        );
    }
}

/// Configures the four socket listeners. A port of 0 or a path of `None` turns that listener off.
fn listeners(
    port: u16, tcp_port: u16, socket_path: Option<&str>, socket_stream_path: Option<&str>, non_local_traffic: bool,
) -> DogStatsDConfiguration {
    DogStatsDConfiguration {
        port,
        tcp_port,
        socket_path: socket_path.map(String::from),
        socket_stream_path: socket_stream_path.map(String::from),
        non_local_traffic,
        ..DogStatsDConfiguration::for_test()
    }
}

#[test]
fn build_addresses_follows_the_configured_listeners() {
    let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
    let unixgram = || ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let unix = || ListenAddress::Unix("/tmp/dsd-stream.sock".into());
    let named_pipe = |buffer_size| DogStatsDConfiguration {
        pipe_name: Some("datadog-dogstatsd".to_string()),
        buffer_size,
        ..listeners(0, 0, None, None, false)
    };

    let cases = [
        ("no listeners", listeners(0, 0, None, None, false), None, vec![]),
        (
            "udp local",
            listeners(8125, 0, None, None, false),
            None,
            vec![ListenAddress::udp_loopback(8125)],
        ),
        (
            "udp non-local",
            listeners(8125, 0, None, None, true),
            None,
            vec![ListenAddress::udp_any(8125)],
        ),
        (
            "tcp local",
            listeners(0, 9000, None, None, false),
            None,
            vec![ListenAddress::tcp_loopback(9000)],
        ),
        (
            "tcp non-local",
            listeners(0, 9000, None, None, true),
            None,
            vec![ListenAddress::tcp_any(9000)],
        ),
        (
            "unixgram",
            listeners(0, 0, Some("/tmp/dsd.sock"), None, false),
            None,
            vec![unixgram()],
        ),
        (
            "unix stream",
            listeners(0, 0, None, Some("/tmp/dsd-stream.sock"), false),
            None,
            vec![unix()],
        ),
        (
            "all four non-local",
            listeners(8125, 9000, Some("/tmp/dsd.sock"), Some("/tmp/dsd-stream.sock"), true),
            None,
            vec![
                ListenAddress::udp_any(8125),
                ListenAddress::tcp_any(9000),
                unixgram(),
                unix(),
            ],
        ),
        (
            "all four local",
            listeners(8125, 9000, Some("/tmp/dsd.sock"), Some("/tmp/dsd-stream.sock"), false),
            None,
            vec![
                ListenAddress::udp_loopback(8125),
                ListenAddress::tcp_loopback(9000),
                unixgram(),
                unix(),
            ],
        ),
        // `bind_host` applies to UDP and TCP only.
        (
            "bind host",
            listeners(8125, 9000, Some("/tmp/dsd.sock"), None, false),
            bind_host,
            vec![
                ListenAddress::Udp(([192, 168, 1, 50], 8125).into()),
                ListenAddress::Tcp(([192, 168, 1, 50], 9000).into()),
                unixgram(),
            ],
        ),
        // `non_local_traffic` takes precedence over `bind_host`, as in the Agent.
        (
            "non-local clobbers bind host",
            listeners(8125, 9000, None, Some("/tmp/dsd-stream.sock"), true),
            bind_host,
            vec![ListenAddress::udp_any(8125), ListenAddress::tcp_any(9000), unix()],
        ),
        (
            "named pipe",
            named_pipe(TEST_BUFFER_SIZE),
            None,
            vec![named_pipe_listen_address()],
        ),
        // The input buffer of the named pipe follows `buffer_size`.
        (
            "named pipe buffer size",
            named_pipe(16384),
            None,
            vec![ListenAddress::named_pipe_with_input_buffer_size(
                "datadog-dogstatsd",
                TEST_WINDOWS_PIPE_SECURITY_DESCRIPTOR,
                16_384,
            )],
        ),
    ];

    for (name, config, bind_host, expected) in cases {
        assert_eq!(config.build_addresses(bind_host), expected, "{name}");
    }
}
