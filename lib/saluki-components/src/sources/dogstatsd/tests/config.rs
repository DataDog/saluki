use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

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
fn build_addresses_includes_named_pipe_when_configured() {
    let config = DogStatsDConfiguration {
        port: 0,
        pipe_name: Some("datadog-dogstatsd".to_string()),
        ..DogStatsDConfiguration::for_test()
    };

    let addresses = config.build_addresses(None);

    assert_eq!(addresses, vec![named_pipe_listen_address()]);
}

#[test]
fn build_addresses_uses_buffer_size_for_named_pipe_input_buffer() {
    let config = DogStatsDConfiguration {
        port: 0,
        pipe_name: Some("datadog-dogstatsd".to_string()),
        buffer_size: 16384,
        ..DogStatsDConfiguration::for_test()
    };

    let addresses = config.build_addresses(None);

    let [ListenAddress::NamedPipe { input_buffer_size, .. }] = addresses.as_slice() else {
        panic!("expected only a named pipe listen address, got {addresses:?}");
    };
    assert_eq!(*input_buffer_size, Some(16_384));
}

#[test]
fn eol_required_matches_named_pipe_listener_type() {
    let config = DogStatsDConfiguration {
        eol_required: vec!["named_pipe".to_string()],
        ..DogStatsDConfiguration::for_test()
    };
    let eol_required = config.eol_required();

    assert!(eol_required.for_listener(&named_pipe_listen_address()));
    assert!(!eol_required.for_listener(&udp_listen_address()));
    assert!(!eol_required.for_listener(&tcp_listen_address()));
}

#[test]
fn statsd_forward_no_host_disabled() {
    let config = DogStatsDConfiguration {
        statsd_forward_port: 9125,
        ..DogStatsDConfiguration::for_test()
    };
    assert!(config.statsd_forward_target().is_none());
}

#[test]
fn statsd_forward_zero_port_disabled() {
    let config = DogStatsDConfiguration {
        statsd_forward_host: Some(MetaString::from("127.0.0.1")),
        statsd_forward_port: 0,
        ..DogStatsDConfiguration::for_test()
    };
    assert!(config.statsd_forward_target().is_none());
}

#[test]
fn statsd_forward_host_and_port_enabled() {
    let config = DogStatsDConfiguration {
        statsd_forward_host: Some(MetaString::from("127.0.0.1")),
        statsd_forward_port: 9125,
        ..DogStatsDConfiguration::for_test()
    };
    let (host, port) = config.statsd_forward_target().expect("forwarding should be enabled");
    assert_eq!(host.as_ref(), "127.0.0.1");
    assert_eq!(port, 9125);
}

#[test]
fn statsd_forward_invalid_target_still_builds_forwarder_handle() {
    let config = DogStatsDConfiguration {
        statsd_forward_host: Some(MetaString::from("not a valid host")),
        statsd_forward_port: 9125,
        ..DogStatsDConfiguration::for_test()
    };
    assert!(config.packet_forwarder_target().is_some());
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
fn no_eol_required_listener_types_requires_no_newline() {
    let config = DogStatsDConfiguration {
        eol_required: Vec::new(),
        ..DogStatsDConfiguration::for_test()
    };
    let eol_required = config.eol_required();

    assert!(!eol_required.for_listener(&udp_listen_address()));
    assert!(!eol_required.for_listener(&tcp_listen_address()));
}

#[test]
fn eol_required_matches_configured_listener_types() {
    let config = DogStatsDConfiguration {
        eol_required: vec!["udp".to_string(), "uds".to_string()],
        ..DogStatsDConfiguration::for_test()
    };
    let eol_required = config.eol_required();

    assert!(eol_required.for_listener(&udp_listen_address()));
    assert!(!eol_required.for_listener(&tcp_listen_address()));

    #[cfg(unix)]
    {
        assert!(eol_required.for_listener(&ListenAddress::Unixgram("/tmp/dsd.sock".into())));
        assert!(eol_required.for_listener(&ListenAddress::Unix("/tmp/dsd-stream.sock".into())));
    }
}

#[test]
fn eol_required_ignores_unrecognized_listener_types() {
    let config = DogStatsDConfiguration {
        eol_required: vec!["udp".to_string(), "carrier_pigeon".to_string()],
        ..DogStatsDConfiguration::for_test()
    };
    let eol_required = config.eol_required();

    assert!(eol_required.for_listener(&udp_listen_address()));
    assert!(!eol_required.for_listener(&named_pipe_listen_address()));
}

#[test]
fn interner_size_from_entry_count() {
    // A Core Agent migration config with entry count 4096 should yield 2 MiB, not 4096 bytes.
    let config = DogStatsDConfiguration {
        context_string_interner_entry_count: 4096,
        ..DogStatsDConfiguration::for_test()
    };
    assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
}

#[test]
fn interner_size_from_explicit_bytes() {
    let config = DogStatsDConfiguration {
        context_string_interner_size_bytes: Some(ByteSize::b(4194304)),
        ..DogStatsDConfiguration::for_test()
    };
    assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(4194304));
}

#[test]
fn interner_size_explicit_bytes_takes_priority() {
    let config = DogStatsDConfiguration {
        context_string_interner_entry_count: 4096,
        context_string_interner_size_bytes: Some(ByteSize::b(8388608)),
        ..DogStatsDConfiguration::for_test()
    };
    // The explicit byte size (8 MiB) takes priority over the entry count.
    assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(8388608));
}

#[test]
fn interner_size_custom_entry_count() {
    let config = DogStatsDConfiguration {
        context_string_interner_entry_count: 8192,
        ..DogStatsDConfiguration::for_test()
    };
    // 8192 entries * 512 bytes = 4 MiB
    assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(4));
}

/// Asserts that two lists of ListenAddress are equivalent.
fn address_list_eq(expected: &mut [ListenAddress], actual: &mut [ListenAddress]) -> Result<(), String> {
    if expected.len() != actual.len() {
        return Err(format!(
            "length mismatch: expected {} addresses, got {}",
            expected.len(),
            actual.len()
        ));
    }

    expected.sort_by_key(|a| a.to_string());
    actual.sort_by_key(|a| a.to_string());

    for (e, a) in expected.iter().zip(actual.iter()) {
        let (es, as_) = (e.to_string(), a.to_string());
        if es != as_ {
            return Err(format!("address mismatch: expected {}, got {}", es, as_));
        }
    }

    Ok(())
}

/// This test verifies that we didn't accidentally break the `build_addresses_no_listeners` helper function which
/// would render all further tests useless.
#[test]
fn build_addresses_assertion_function_works() {
    let config = DogStatsDConfiguration {
        port: 0,
        tcp_port: 123,
        socket_path: None,
        socket_stream_path: None,
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
        // Close, but not quite! This is intentionally *not* 127.0.0.1 to test that the assertion will fail
        Ipv4Addr::new(127, 0, 0, 2),
        123,
    )))];
    let mut actual = config.build_addresses(None);
    assert!(address_list_eq(&mut expected, &mut actual).is_err())
}

/// With all four listener gates off, `build_addresses` returns an empty Vec.
#[test]
fn build_addresses_no_listeners() {
    let config = DogStatsDConfiguration {
        port: 0,
        tcp_port: 0,
        socket_path: None,
        socket_stream_path: None,
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// UDP port set, `non_local_traffic=false` -> UDP listener bound to `127.0.0.1`.
#[test]
fn build_addresses_udp_local_only() {
    let config = DogStatsDConfiguration {
        port: 8125,
        tcp_port: 0,
        socket_path: None,
        socket_stream_path: None,
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::udp_loopback(8125)];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// UDP port set, `non_local_traffic=true` -> UDP listener bound to `0.0.0.0`.
#[test]
fn build_addresses_udp_non_local_only() {
    let config = DogStatsDConfiguration {
        port: 8125,
        tcp_port: 0,
        socket_path: None,
        socket_stream_path: None,
        non_local_traffic: true,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::udp_any(8125)];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// TCP port set, `non_local_traffic=false` -> TCP listener bound to `127.0.0.1`.
#[test]
fn build_addresses_tcp_local_only() {
    let config = DogStatsDConfiguration {
        port: 0,
        tcp_port: 9000,
        socket_path: None,
        socket_stream_path: None,
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        9000,
    )))];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// TCP port set, `non_local_traffic=true` -> TCP listener bound to `0.0.0.0`.
#[test]
fn build_addresses_tcp_non_local_only() {
    let config = DogStatsDConfiguration {
        port: 0,
        tcp_port: 9000,
        socket_path: None,
        socket_stream_path: None,
        non_local_traffic: true,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(0, 0, 0, 0),
        9000,
    )))];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// `socket_path` set -> a `Unixgram` address is produced with that path.
#[test]
fn build_addresses_unixgram_only() {
    let config = DogStatsDConfiguration {
        port: 0,
        tcp_port: 0,
        socket_path: Some("/tmp/dsd.sock".to_string()),
        socket_stream_path: None,
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::Unixgram("/tmp/dsd.sock".into())];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// `socket_stream_path` set -> a `Unix` (stream) address is produced with that path.
#[test]
fn build_addresses_unix_stream_only() {
    let config = DogStatsDConfiguration {
        port: 0,
        tcp_port: 0,
        socket_path: None,
        socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![ListenAddress::Unix("/tmp/dsd-stream.sock".into())];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// All four listener types enabled at once, with `non_local_traffic=true`.
#[test]
fn build_addresses_all_four_non_local() {
    let config = DogStatsDConfiguration {
        port: 8125,
        tcp_port: 9000,
        socket_path: Some("/tmp/dsd.sock".to_string()),
        socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
        non_local_traffic: true,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![
        ListenAddress::udp_any(8125),
        ListenAddress::tcp_any(9000),
        ListenAddress::Unixgram("/tmp/dsd.sock".into()),
        ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
    ];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// All four listener types enabled at once, with `non_local_traffic=false`.
#[test]
fn build_addresses_all_four_local() {
    let config = DogStatsDConfiguration {
        port: 8125,
        tcp_port: 9000,
        socket_path: Some("/tmp/dsd.sock".to_string()),
        socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let mut expected = vec![
        ListenAddress::udp_loopback(8125),
        ListenAddress::tcp_loopback(9000),
        ListenAddress::Unixgram("/tmp/dsd.sock".into()),
        ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
    ];
    let mut actual = config.build_addresses(None);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// Passing `Some(ip)` to `build_addresses` with `non_local_traffic=false` -> both UDP and TCP
/// bind to that IP. Includes a UDS datagram socket to confirm `bind_host` doesn't affect it.
#[test]
fn build_addresses_bind_host_applies_to_udp_and_tcp() {
    let config = DogStatsDConfiguration {
        port: 8125,
        tcp_port: 9000,
        socket_path: Some("/tmp/dsd.sock".to_string()),
        socket_stream_path: None,
        non_local_traffic: false,
        ..DogStatsDConfiguration::for_test()
    };
    let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
    let mut expected = vec![
        ListenAddress::Udp(([192, 168, 1, 50], 8125).into()),
        ListenAddress::Tcp(([192, 168, 1, 50], 9000).into()),
        ListenAddress::Unixgram("/tmp/dsd.sock".into()),
    ];
    let mut actual = config.build_addresses(bind_host);
    address_list_eq(&mut expected, &mut actual).unwrap();
}

/// Passing `Some(ip)` to `build_addresses` with `non_local_traffic=true` -> both UDP and TCP
/// bind to `0.0.0.0`; the `bind_host` parameter is ignored (precedence matches the Agent).
/// Includes a UDS stream socket to confirm `bind_host` doesn't affect it.
#[test]
fn build_addresses_non_local_clobbers_bind_host() {
    let config = DogStatsDConfiguration {
        port: 8125,
        tcp_port: 9000,
        socket_path: None,
        socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
        non_local_traffic: true,
        ..DogStatsDConfiguration::for_test()
    };
    let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
    let mut expected = vec![
        ListenAddress::udp_any(8125),
        ListenAddress::tcp_any(9000),
        ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
    ];
    let mut actual = config.build_addresses(bind_host);
    address_list_eq(&mut expected, &mut actual).unwrap();
}
