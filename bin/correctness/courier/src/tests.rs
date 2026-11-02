use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use panoramic_courier::Target;
use tokio::{
    io::{AsyncWriteExt, ReadBuf},
    net::{TcpListener, UdpSocket},
};

use super::*;

const LIMIT: Duration = Duration::from_secs(2);

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("socket operation must finish within five seconds")
}

fn invocation(command: Command) -> Invocation {
    Invocation {
        command,
        timeout: LIMIT,
    }
}

/// Windows retries a connection to a closed port for about two seconds before refusing it, so a
/// closed-port check needs more time than [`LIMIT`] to see the refusal rather than the deadline.
fn closed_port_invocation(command: Command) -> Invocation {
    Invocation {
        command,
        timeout: Duration::from_secs(10),
    }
}

async fn closed_tcp_target() -> Target {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap()
}

#[cfg(unix)]
fn unix_target(directory: &tempfile::TempDir, name: &str, datagram: bool) -> Target {
    let scheme = if datagram { "unixgram" } else { "unix" };
    Target::parse(&format!("{scheme}://{}", directory.path().join(name).display())).unwrap()
}

#[track_caller]
fn assert_unreachable(report: Report, to: &Target) {
    match report {
        Report::Unreachable { reason } => assert!(reason.contains(&to.to_string()), "{reason}"),
        report => panic!("expected unreachable for {to}, got {report:?}"),
    }
}

#[tokio::test]
async fn a_tcp_send_delivers_stdin_unchanged_on_its_own_connection() {
    within(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let mut received = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
                received.push((bytes, stream));
            }
            received
        });
        let payloads = [b"first\0\xff".as_slice(), b"second\nline".as_slice()];
        for payload in payloads {
            assert_eq!(
                execute(&invocation(Command::Send { to: to.clone() }), payload).await,
                Report::Sent {
                    bytes: payload.len() as u64
                }
            );
        }
        let received: Vec<_> = server
            .await
            .unwrap()
            .into_iter()
            .map(|(bytes, _stream)| bytes)
            .collect();
        assert_eq!(received, payloads);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_unix_stream_send_delivers_stdin_unchanged_and_closes_its_connection() {
    within(async {
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(directory.path().join("s")).unwrap();
        let to = unix_target(&directory, "s", false);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            (bytes, stream)
        });
        let payload = b"metric:1|c\nother:2|g\0\xff";
        assert_eq!(
            execute(&invocation(Command::Send { to }), payload.as_slice()).await,
            Report::Sent {
                bytes: payload.len() as u64
            }
        );
        let (received, _stream) = server.await.unwrap();
        assert_eq!(received, payload);
    })
    .await;
}

#[tokio::test]
async fn a_udp_send_delivers_stdin_as_one_datagram() {
    within(async {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let to = Target::parse(&format!("udp://{}", socket.local_addr().unwrap())).unwrap();
        let payload = b"first\nsecond\nthird\0\xff";
        assert_eq!(
            execute(&invocation(Command::Send { to }), payload.as_slice()).await,
            Report::Sent {
                bytes: payload.len() as u64
            }
        );
        let mut buffer = [0; 128];
        let read = socket.recv(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..read], payload);
        assert_eq!(
            socket.try_recv(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_unix_datagram_send_delivers_stdin_as_one_datagram() {
    within(async {
        let directory = tempfile::tempdir().unwrap();
        let socket = tokio::net::UnixDatagram::bind(directory.path().join("s")).unwrap();
        let to = unix_target(&directory, "s", true);
        let payload = b"first\nsecond\nthird\0\xff";
        assert_eq!(
            execute(&invocation(Command::Send { to }), payload.as_slice()).await,
            Report::Sent {
                bytes: payload.len() as u64
            }
        );
        let mut buffer = [0; 128];
        let read = socket.recv(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..read], payload);
        assert_eq!(
            socket.try_recv(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    })
    .await;
}

#[tokio::test]
async fn an_empty_stdin_sends_an_empty_datagram() {
    within(async {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let to = Target::parse(&format!("udp://{}", socket.local_addr().unwrap())).unwrap();
        assert_eq!(
            execute(&invocation(Command::Send { to }), tokio::io::empty()).await,
            Report::Sent { bytes: 0 }
        );
        assert_eq!(socket.recv(&mut [0; 1]).await.unwrap(), 0);
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().unwrap();
            let socket = tokio::net::UnixDatagram::bind(directory.path().join("s")).unwrap();
            let to = unix_target(&directory, "s", true);
            assert_eq!(
                execute(&invocation(Command::Send { to }), tokio::io::empty()).await,
                Report::Sent { bytes: 0 }
            );
            assert_eq!(socket.recv(&mut [0; 1]).await.unwrap(), 0);
        }
    })
    .await;
}

#[tokio::test]
async fn a_send_to_nothing_listening_is_unreachable() {
    let to = closed_tcp_target().await;
    assert_unreachable(
        execute(
            &closed_port_invocation(Command::Send { to: to.clone() }),
            &b"payload"[..],
        )
        .await,
        &to,
    );
    #[cfg(unix)]
    {
        let directory = tempfile::tempdir().unwrap();
        drop(tokio::net::UnixListener::bind(directory.path().join("stale")).unwrap());
        drop(tokio::net::UnixDatagram::bind(directory.path().join("gram")).unwrap());
        for to in [
            unix_target(&directory, "missing", false),
            unix_target(&directory, "stale", false),
            unix_target(&directory, "missing", true),
            unix_target(&directory, "gram", true),
        ] {
            assert_unreachable(
                execute(&invocation(Command::Send { to: to.clone() }), &b"payload"[..]).await,
                &to,
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_send_the_server_never_reads_times_out_within_its_limit() {
    within(async {
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(directory.path().join("s")).unwrap();
        let to = unix_target(&directory, "s", false);
        let invocation = Invocation {
            command: Command::Send { to },
            timeout: Duration::from_millis(300),
        };
        let payload = vec![b'x'; 16 * 1024 * 1024];
        let started = Instant::now();
        let (report, connection) = tokio::join!(execute(&invocation, payload.as_slice()), listener.accept());
        let _connection = connection.unwrap();
        assert_eq!(report, Report::TimedOut { limit_ms: 300 });
        assert!(started.elapsed() < LIMIT);
    })
    .await;
}

#[tokio::test]
async fn stdin_left_open_past_the_limit_is_a_local_failure() {
    // An exec transport leaving stdin open must fail locally, rather than blame the endpoint for timing out.
    let (reader, _writer) = tokio::io::duplex(64);
    let to = closed_tcp_target().await;
    let invocation = Invocation {
        command: Command::Send { to: to.clone() },
        timeout: Duration::from_millis(100),
    };
    let started = Instant::now();
    let report = within(execute(&invocation, reader)).await;
    assert_eq!(
        report,
        Report::Failed {
            reason: format!("{to}: stdin was still open after the 100 ms limit")
        }
    );
    assert_eq!(report.exit_code(), 13);
    assert!(started.elapsed() < LIMIT);
}

// The send must still be blocked when the limit expires. Windows loopback accepts the whole payload
// without the peer reading it, so the send finishes first there.
#[cfg(not(windows))]
#[tokio::test]
async fn stdin_and_the_send_share_one_time_limit() {
    within(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
        let invocation = Invocation {
            command: Command::Send { to },
            timeout: Duration::from_millis(200),
        };
        let payload = vec![b'x'; 16 * 1024 * 1024];
        let (reader, mut writer) = tokio::io::duplex(payload.len());
        writer.write_all(&payload).await.unwrap();
        let started = Instant::now();
        let (report, connection, ()) = tokio::join!(execute(&invocation, reader), listener.accept(), async {
            tokio::time::sleep(Duration::from_millis(120)).await;
            drop(writer);
        });
        let _connection = connection.unwrap();
        assert_eq!(report, Report::TimedOut { limit_ms: 200 });
        let elapsed = started.elapsed();
        assert!(elapsed < LIMIT);
        assert!(elapsed < Duration::from_millis(300), "elapsed: {elapsed:?}");
    })
    .await;
}

#[tokio::test]
async fn a_probe_of_a_listening_server_reports_listening() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let to = Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
    assert_eq!(
        execute(&invocation(Command::Probe { to }), tokio::io::empty()).await,
        Report::Listening
    );
    #[cfg(unix)]
    {
        let directory = tempfile::tempdir().unwrap();
        let _stream = tokio::net::UnixListener::bind(directory.path().join("s")).unwrap();
        let _datagram = tokio::net::UnixDatagram::bind(directory.path().join("d")).unwrap();
        for to in [unix_target(&directory, "s", false), unix_target(&directory, "d", true)] {
            assert_eq!(
                execute(&invocation(Command::Probe { to }), tokio::io::empty()).await,
                Report::Listening
            );
        }
    }
}

#[tokio::test]
async fn a_tcp_probe_connects_without_sending_data() {
    within(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
        let (stdin, _writer) = tokio::io::duplex(64);
        assert_eq!(
            execute(&invocation(Command::Probe { to }), stdin).await,
            Report::Listening
        );
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        assert_eq!(stream.read_to_end(&mut received).await.unwrap(), 0);
        assert!(received.is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_probe_of_nothing_listening_is_unreachable() {
    let to = closed_tcp_target().await;
    assert_unreachable(
        execute(
            &closed_port_invocation(Command::Probe { to: to.clone() }),
            tokio::io::empty(),
        )
        .await,
        &to,
    );
    #[cfg(unix)]
    {
        let directory = tempfile::tempdir().unwrap();
        drop(tokio::net::UnixListener::bind(directory.path().join("stale")).unwrap());
        drop(tokio::net::UnixDatagram::bind(directory.path().join("gram")).unwrap());
        for to in [
            unix_target(&directory, "missing", false),
            unix_target(&directory, "stale", false),
            unix_target(&directory, "missing", true),
            unix_target(&directory, "gram", true),
        ] {
            assert_unreachable(
                execute(&invocation(Command::Probe { to: to.clone() }), tokio::io::empty()).await,
                &to,
            );
        }
    }
}

#[tokio::test]
async fn a_udp_probe_reports_addressable_whether_or_not_anything_listens() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = Target::parse(&format!("udp://{}", socket.local_addr().unwrap())).unwrap();
    let invocation = invocation(Command::Probe { to });
    assert_eq!(execute(&invocation, tokio::io::empty()).await, Report::Addressable);
    assert_eq!(
        socket.try_recv(&mut [0; 1]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(socket);
    assert_eq!(execute(&invocation, tokio::io::empty()).await, Report::Addressable);
}

#[tokio::test]
async fn a_full_run_writes_one_report_line_and_exits_with_its_code() {
    within(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
        let mut stderr = Vec::new();
        let args = invocation(Command::Send { to })
            .args()
            .into_iter()
            .map(OsString::from)
            .collect();
        let code = run(args, &b"payload"[..], &mut stderr).await;
        assert_eq!(code, 0);
        assert_eq!(stderr.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(stderr.last(), Some(&b'\n'));
        assert_eq!(
            Report::from_exec(i64::from(code), &stderr).unwrap(),
            Report::Sent { bytes: 7 }
        );
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"payload");
    })
    .await;
}

#[tokio::test]
async fn a_run_with_bad_args_reports_usage_and_exits_2() {
    let mut stderr = Vec::new();
    let (reader, _writer) = tokio::io::duplex(64);
    let code = within(run(vec!["invalid".into()], reader, &mut stderr)).await;
    assert_eq!(code, 2);
    assert_eq!(stderr.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert_eq!(
        Report::from_exec(i64::from(code), &stderr).unwrap(),
        Report::Usage {
            reason: "unknown command \"invalid\"; use send or probe".into()
        }
    );
}

#[tokio::test]
async fn an_oversized_datagram_is_a_send_failure_without_payload_bytes() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = Target::parse(&format!("udp://{}", socket.local_addr().unwrap())).unwrap();
    let payload = b"secret payload".repeat(1024 * 1024);
    let targets = vec![to];
    #[cfg(unix)]
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let _socket = tokio::net::UnixDatagram::bind(directory.path().join("s")).unwrap();
    #[cfg(unix)]
    let targets = {
        let mut targets = targets;
        targets.push(unix_target(&directory, "s", true));
        targets
    };
    for to in targets {
        let report = execute(&invocation(Command::Send { to: to.clone() }), payload.as_slice()).await;
        match report {
            Report::SendFailed { reason } => {
                assert!(reason.contains(&to.to_string()));
                assert!(!reason.contains("secret payload"));
            }
            report => panic!("expected send failure for {to}, got {report:?}"),
        }
    }
}

struct BrokenStdin;

impl AsyncRead for BrokenStdin {
    fn poll_read(self: Pin<&mut Self>, _cx: &mut Context<'_>, _buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("stdin unavailable")))
    }
}

#[tokio::test]
async fn a_stdin_failure_is_local_and_names_the_target() {
    let to = closed_tcp_target().await;
    assert_eq!(
        execute(&invocation(Command::Send { to: to.clone() }), BrokenStdin).await,
        Report::Failed {
            reason: format!("{to}: reading stdin: stdin unavailable")
        }
    );
}

#[cfg(not(unix))]
#[tokio::test]
async fn unix_targets_report_an_unsupported_platform() {
    for to in [
        Target::parse("unix:///run/s").unwrap(),
        Target::parse("unixgram:///run/s").unwrap(),
    ] {
        for command in [Command::Send { to: to.clone() }, Command::Probe { to }] {
            assert_eq!(
                execute(&invocation(command), tokio::io::empty()).await,
                Report::Failed {
                    reason: "Unix sockets are not supported on this platform".into()
                }
            );
        }
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn non_utf8_arguments_report_usage_and_exit_2() {
    // Decode OS arguments explicitly so malformed text produces a usage report rather than an args() panic.
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    };
    let (reader, _writer) = tokio::io::duplex(64);
    let mut stderr = Vec::new();
    let code = within(run(vec![invalid], reader, &mut stderr)).await;
    assert_eq!(code, 2);
    assert_eq!(stderr.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert_eq!(
        Report::from_exec(i64::from(code), &stderr).unwrap(),
        Report::Usage {
            reason: "arguments must be UTF-8".into()
        }
    );
}
