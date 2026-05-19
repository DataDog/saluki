#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::{
    fmt, io,
    io::Write,
    net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    path::PathBuf,
};

use url::Url;

#[cfg(unix)]
const SUPPORTED_SCHEMES: &str = "udp, tcp, unixgram, unix";
#[cfg(not(unix))]
const SUPPORTED_SCHEMES: &str = "udp, tcp";

#[derive(Clone, Debug)]
enum SyslogDestination {
    Udp(String, u16),
    Tcp(String, u16),
    #[cfg(unix)]
    Unixgram(PathBuf),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl SyslogDestination {
    fn parse(uri: &str) -> Result<Self, SyslogError> {
        let parsed = Url::parse(uri).map_err(|source| SyslogError::InvalidUri {
            uri: uri.to_string(),
            reason: source.to_string(),
        })?;

        match parsed.scheme() {
            "udp" => parse_network_endpoint(&parsed, uri).map(|(host, port)| Self::Udp(host, port)),
            "tcp" => parse_network_endpoint(&parsed, uri).map(|(host, port)| Self::Tcp(host, port)),
            #[cfg(unix)]
            "unixgram" => parse_unix_path(&parsed, uri).map(Self::Unixgram),
            #[cfg(unix)]
            "unix" => parse_unix_path(&parsed, uri).map(Self::Unix),
            scheme => Err(SyslogError::UnsupportedScheme {
                scheme: scheme.to_string(),
            }),
        }
    }

    fn connect(&self) -> io::Result<SyslogConnection> {
        match self {
            Self::Udp(host, port) => connect_first((host.as_str(), *port), |addr| {
                let bind_addr = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
                let socket = UdpSocket::bind(bind_addr)?;
                socket.connect(addr)?;
                Ok(socket)
            })
            .map(SyslogConnection::Udp),
            Self::Tcp(host, port) => {
                connect_first((host.as_str(), *port), TcpStream::connect).map(SyslogConnection::Tcp)
            }
            #[cfg(unix)]
            Self::Unixgram(path) => connect_unixgram(path).map(SyslogConnection::Unixgram),
            #[cfg(unix)]
            Self::Unix(path) => UnixStream::connect(path).map(SyslogConnection::Unix),
        }
    }
}

fn parse_network_endpoint(parsed: &Url, uri: &str) -> Result<(String, u16), SyslogError> {
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| SyslogError::InvalidUri {
            uri: uri.to_string(),
            reason: "network syslog URI must include a host".to_string(),
        })?;
    let port = parsed.port().ok_or_else(|| SyslogError::InvalidUri {
        uri: uri.to_string(),
        reason: "network syslog URI must include a port".to_string(),
    })?;

    Ok((host.to_string(), port))
}

#[cfg(unix)]
fn parse_unix_path(parsed: &Url, uri: &str) -> Result<PathBuf, SyslogError> {
    let path = parsed.path();
    if path.is_empty() {
        return Err(SyslogError::InvalidUri {
            uri: uri.to_string(),
            reason: "Unix socket path cannot be empty".to_string(),
        });
    }

    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(SyslogError::InvalidUri {
            uri: uri.to_string(),
            reason: "Unix socket path must be absolute".to_string(),
        });
    }

    Ok(path)
}

fn connect_first<T>(addrs: impl ToSocketAddrs, mut connect: impl FnMut(SocketAddr) -> io::Result<T>) -> io::Result<T> {
    let mut last_error = None;
    for addr in addrs.to_socket_addrs()? {
        match connect(addr) {
            Ok(conn) => return Ok(conn),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "network syslog URI did not resolve to any socket addresses",
        )
    }))
}

#[cfg(unix)]
fn connect_unixgram(path: &PathBuf) -> io::Result<UnixDatagram> {
    let socket = UnixDatagram::unbound()?;
    socket.connect(path)?;
    Ok(socket)
}

fn write_all_as_len(writer: &mut impl Write, buf: &[u8]) -> io::Result<usize> {
    writer.write_all(buf)?;
    Ok(buf.len())
}

enum SyslogConnection {
    Udp(UdpSocket),
    Tcp(TcpStream),
    #[cfg(unix)]
    Unixgram(UnixDatagram),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Write for SyslogConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Udp(socket) => socket.send(buf),
            Self::Tcp(stream) => write_all_as_len(stream, buf),
            #[cfg(unix)]
            Self::Unixgram(socket) => socket.send(buf),
            #[cfg(unix)]
            Self::Unix(stream) => write_all_as_len(stream, buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Udp(_) => Ok(()),
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unixgram(_) => Ok(()),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

impl Drop for SyslogConnection {
    fn drop(&mut self) {
        match self {
            Self::Tcp(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            #[cfg(unix)]
            Self::Unix(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Self::Udp(_) => {}
            #[cfg(unix)]
            Self::Unixgram(_) => {}
        }
    }
}

pub(super) struct SyslogWriter {
    destination: SyslogDestination,
    connection: Option<SyslogConnection>,
}

impl SyslogWriter {
    pub(super) fn from_uri(uri: &str) -> Result<Self, SyslogError> {
        let destination = SyslogDestination::parse(uri)?;
        let connection = destination.connect().ok();

        Ok(Self {
            destination,
            connection,
        })
    }
}

impl Write for SyslogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.connection.is_none() {
            self.connection = Some(self.destination.connect()?);
        }

        let result = self.connection.as_mut().expect("connection should exist").write(buf);
        if result.is_err() {
            self.connection = None;
        }

        result
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.connection.as_mut() {
            Some(connection) => connection.flush(),
            None => Ok(()),
        }
    }
}

#[derive(Debug)]
pub(super) enum SyslogError {
    InvalidUri { uri: String, reason: String },
    UnsupportedScheme { scheme: String },
}

impl fmt::Display for SyslogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUri { uri, reason } => write!(f, "Invalid syslog URI '{}': {}", uri, reason),
            Self::UnsupportedScheme { scheme } => write!(
                f,
                "Unsupported syslog URI scheme '{}'. Supported schemes: {}.",
                scheme, SUPPORTED_SCHEMES
            ),
        }
    }
}

impl std::error::Error for SyslogError {}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::{TcpListener, UdpSocket},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn invalid_uri_returns_clear_error() {
        let error = SyslogWriter::from_uri("://invalid-uri")
            .err()
            .expect("invalid URI should fail");

        assert!(error.to_string().contains("Invalid syslog URI"));
    }

    #[test]
    fn unsupported_scheme_returns_clear_error() {
        let error = SyslogWriter::from_uri("http://127.0.0.1:514")
            .err()
            .expect("unsupported scheme should fail");

        assert!(error.to_string().contains("Unsupported syslog URI scheme 'http'"));
    }

    #[test]
    fn network_destination_parsing_does_not_require_dns_resolution() {
        let udp_destination = SyslogDestination::parse("udp://saluki-syslog.invalid:1514")
            .expect("UDP hostname should parse without DNS resolution");
        let tcp_destination = SyslogDestination::parse("tcp://saluki-syslog.invalid:1514")
            .expect("TCP hostname should parse without DNS resolution");

        assert!(matches!(udp_destination, SyslogDestination::Udp(..)));
        assert!(matches!(tcp_destination, SyslogDestination::Tcp(..)));
    }

    #[test]
    fn write_all_as_len_retries_after_partial_writes() {
        let mut writer = PartialWriter {
            max_chunk: 2,
            bytes: Vec::new(),
            writes: 0,
        };

        let len = write_all_as_len(&mut writer, b"partial-write").expect("partial writes should complete");

        assert_eq!(len, b"partial-write".len());
        assert_eq!(writer.bytes, b"partial-write");
        assert!(writer.writes > 1);
    }

    #[test]
    fn udp_writer_delivers_to_local_listener() {
        let listener = UdpSocket::bind("127.0.0.1:0").expect("bind UDP listener");
        listener.set_read_timeout(Some(TEST_TIMEOUT)).expect("set read timeout");
        let listener_addr = listener.local_addr().expect("read listener address");

        let mut writer = SyslogWriter::from_uri(&format!("udp://{}", listener_addr)).expect("create UDP syslog writer");
        writer.write_all(b"udp-message").expect("write UDP syslog message");

        let mut buffer = [0; 64];
        let (bytes_read, _) = listener.recv_from(&mut buffer).expect("receive UDP syslog message");
        assert_eq!(&buffer[..bytes_read], b"udp-message");
    }

    #[test]
    fn tcp_writer_delivers_to_local_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind TCP listener");
        let listener_addr = listener.local_addr().expect("read listener address");

        let receiver = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept TCP syslog connection");
            stream.set_read_timeout(Some(TEST_TIMEOUT)).expect("set read timeout");

            let mut buffer = [0; 11];
            stream.read_exact(&mut buffer).expect("read TCP syslog message");
            buffer
        });

        let mut writer = SyslogWriter::from_uri(&format!("tcp://{}", listener_addr)).expect("create TCP syslog writer");
        writer.write_all(b"tcp-message").expect("write TCP syslog message");

        assert_eq!(&receiver.join().expect("receiver should not panic"), b"tcp-message");
    }

    #[cfg(unix)]
    #[test]
    fn unixgram_writer_delivers_to_local_listener() {
        let socket_path = unique_socket_path("unixgram-delivery");
        let listener = UnixDatagram::bind(&socket_path).expect("bind Unix datagram listener");
        listener.set_read_timeout(Some(TEST_TIMEOUT)).expect("set read timeout");

        let mut writer = SyslogWriter::from_uri(&format!("unixgram://{}", socket_path.display()))
            .expect("create Unix datagram syslog writer");
        writer
            .write_all(b"unixgram-message")
            .expect("write Unix datagram syslog message");

        let mut buffer = [0; 64];
        let bytes_read = listener
            .recv(&mut buffer)
            .expect("receive Unix datagram syslog message");
        assert_eq!(&buffer[..bytes_read], b"unixgram-message");

        remove_socket(&socket_path);
    }

    #[cfg(unix)]
    #[test]
    fn unix_stream_writer_delivers_to_local_listener() {
        let socket_path = unique_socket_path("unix-stream-delivery");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind Unix stream listener");
        let receiver = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Unix stream syslog connection");
            stream.set_read_timeout(Some(TEST_TIMEOUT)).expect("set read timeout");

            let mut buffer = [0; 19];
            stream.read_exact(&mut buffer).expect("read Unix stream syslog message");
            buffer
        });

        let mut writer =
            SyslogWriter::from_uri(&format!("unix://{}", socket_path.display())).expect("create Unix syslog writer");
        writer
            .write_all(b"unix-stream-message")
            .expect("write Unix stream syslog message");

        assert_eq!(
            &receiver.join().expect("receiver should not panic"),
            b"unix-stream-message"
        );

        remove_socket(&socket_path);
    }

    #[cfg(unix)]
    #[test]
    fn initial_failure_is_nonfatal_and_write_retries_after_listener_appears() {
        let socket_path = unique_socket_path("unixgram-retry");
        let mut writer = SyslogWriter::from_uri(&format!("unixgram://{}", socket_path.display()))
            .expect("valid URI should not fail setup when listener is missing");
        assert!(writer.connection.is_none());

        let listener = UnixDatagram::bind(&socket_path).expect("bind Unix datagram listener after setup");
        listener.set_read_timeout(Some(TEST_TIMEOUT)).expect("set read timeout");

        writer
            .write_all(b"retry-message")
            .expect("write should reconnect after listener appears");

        let mut buffer = [0; 64];
        let bytes_read = listener.recv(&mut buffer).expect("receive retried syslog message");
        assert_eq!(&buffer[..bytes_read], b"retry-message");

        remove_socket(&socket_path);
    }

    #[cfg(unix)]
    #[test]
    fn connectionless_write_returns_error_when_endpoint_remains_unavailable() {
        let socket_path = unique_socket_path("unixgram-missing");
        let mut writer = SyslogWriter::from_uri(&format!("unixgram://{}", socket_path.display()))
            .expect("valid URI should not fail setup when listener is missing");

        let error = writer
            .write_all(b"missing-listener")
            .expect_err("write should fail while Unix datagram endpoint is unavailable");

        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::AddrNotAvailable
        ));
    }

    #[cfg(unix)]
    fn unique_socket_path(name: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();

        PathBuf::from(format!(
            "/tmp/saluki-syslog-{}-{}-{}.sock",
            std::process::id(),
            timestamp,
            name
        ))
    }

    #[cfg(unix)]
    fn remove_socket(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    struct PartialWriter {
        max_chunk: usize,
        bytes: Vec<u8>,
        writes: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = self.max_chunk.min(buf.len());
            self.bytes.extend_from_slice(&buf[..len]);
            self.writes += 1;
            Ok(len)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
