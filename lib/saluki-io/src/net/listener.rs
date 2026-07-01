//! Network listeners.
use std::{collections::VecDeque, future::pending, io, net::SocketAddr, num::NonZeroUsize};
#[cfg(windows)]
use std::{ffi::c_void, mem, ptr};

use snafu::{ResultExt as _, Snafu};
use socket2::SockRef;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::net::{TcpListener, UdpSocket as TokioUdpSocket};
use tracing::warn;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{LocalFree, FALSE, HLOCAL},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        SECURITY_ATTRIBUTES,
    },
};

#[cfg(target_os = "linux")]
use super::unix::socket_reuseport_supported;
#[cfg(unix)]
use super::unix::{enable_uds_socket_credentials, ensure_unix_socket_free, set_unix_socket_write_only};
use super::{
    addr::ListenAddress,
    stream::{Connection, Stream},
};

const SOCKET_RECV_BUFFER_SIZE_SETTING: &str = "SO_RCVBUF";

#[cfg(not(target_os = "linux"))]
const fn socket_reuseport_supported() -> bool {
    false
}

/// A listener error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ListenerError {
    /// An invalid configuration was given when creating the listener.
    #[snafu(display("invalid configuration: {}", reason))]
    InvalidConfiguration {
        /// Cause of the invalid configuration.
        reason: &'static str,
    },

    /// Failed to bind to the listen address.
    #[snafu(display("failed to bind to listen address {}: {}", address, source))]
    FailedToBind {
        /// Listen address.
        address: ListenAddress,

        /// Source of the error.
        source: io::Error,
    },

    /// Failed to configure a setting on the listening socket.
    #[snafu(display("failed to configure {} for listener on address {}: {}", setting, address, source))]
    FailedToConfigureListener {
        /// Listen address.
        address: ListenAddress,

        /// Name of the setting.
        setting: &'static str,

        /// Source of the error.
        source: io::Error,
    },

    /// Failed to configure a setting on an accepted stream.
    #[snafu(display("failed to configure {} for {} stream: {}", setting, stream_type, source))]
    FailedToConfigureStream {
        /// Name of the setting.
        setting: &'static str,

        /// Type of stream.
        stream_type: &'static str,

        /// Source of the error.
        source: io::Error,
    },

    /// Failed to accept a new stream from the listener.
    #[snafu(display("failed to accept new stream for listener on address {}: {}", address, source))]
    FailedToAccept {
        /// Listen address.
        address: ListenAddress,

        /// Source of the error.
        source: io::Error,
    },
}

enum ListenerInner {
    Tcp(TcpListener),
    Udp(VecDeque<TokioUdpSocket>),
    #[cfg(unix)]
    Unixgram(Option<tokio::net::UnixDatagram>),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    NamedPipe {
        server: NamedPipeServer,
        path: String,
        security_descriptor: String,
        input_buffer_size: Option<u32>,
    },
}

/// A network listener.
///
/// `Listener` is a abstract listener that works in conjunction with `Stream`, providing the ability to listen on
/// arbitrary addresses and accept new streams of that address family.
///
/// ## Connection-oriented vs connectionless listeners
///
/// For listeners on connection-oriented address families (for example, TCP, Unix domain sockets in stream mode), the listener
/// will listen for and accept new connections in the typical fashion. However, for connectionless address families
/// (for example, UDP, Unix domain sockets in datagram mode), there is no concept of a "connection" and so nothing to be
/// continually "accepted." Instead, `Listener` will emit a single `Stream` that can be used to send and receive data
/// from multiple remote peers.
///
/// ## UDP autoscaling
///
/// On Linux, UDP listeners can be configured to bind multiple sockets to the same address using `SO_REUSEPORT`,
/// allowing the kernel to load-balance incoming datagrams across them. The configured number of sockets are yielded
/// one at a time from successive calls to [`Listener::accept`] before the listener returns pending forever. See
/// the `udp_streams` parameter of [`Listener::from_listen_address`].
pub struct Listener {
    listen_address: ListenAddress,
    inner: ListenerInner,
    socket_receive_buffer_size: Option<usize>,
}

impl Listener {
    /// Creates a new `Listener` from the given listen address.
    ///
    /// ## UDP streams
    ///
    /// For UDP listen addresses, `udp_streams` controls how many sockets are bound to the address and how many
    /// `Stream`s the listener will yield from [`accept`](Self::accept) before going pending forever. `None` behaves
    /// like `Some(1)`: a single socket is bound normally and one stream is yielded.
    ///
    /// When `Some(N)` with N > 1 is requested on Linux, the listener binds N sockets with `SO_REUSEPORT` set before
    /// `bind`, so the kernel will hash-load-balance incoming datagrams across them. On non-Linux platforms,
    /// `SO_REUSEPORT` doesn't provide load balancing, so the request is downgraded to a single socket.
    ///
    /// For non-UDP listen addresses, `udp_streams` is ignored.
    ///
    /// ## Errors
    ///
    /// If the listen address can't be bound, or if the listener can't be configured correctly, an error is returned.
    pub async fn from_listen_address(
        listen_address: ListenAddress, mut udp_streams: Option<NonZeroUsize>,
    ) -> Result<Self, ListenerError> {
        let inner = match &listen_address {
            ListenAddress::Tcp(addr) => {
                TcpListener::bind(addr)
                    .await
                    .map(ListenerInner::Tcp)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?
            }
            ListenAddress::Udp(addr) => {
                // See if we have platform support for SO_REUSEPORT, and if not, fall back to the default behavior.
                if !socket_reuseport_supported() {
                    udp_streams = None;
                    warn!("SO_REUSEPORT not supported on the current platform. Falling back to the default behavior.");
                }

                let sockets = bind_udp_sockets(*addr, udp_streams).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;
                ListenerInner::Udp(sockets)
            }
            #[cfg(unix)]
            ListenAddress::Unixgram(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = tokio::net::UnixDatagram::bind(addr)
                    .map(Some)
                    .map(ListenerInner::Unixgram)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?;

                set_unix_socket_write_only(addr)
                    .await
                    .context(FailedToConfigureListener {
                        address: listen_address.clone(),
                        setting: "read/write permissions",
                    })?;

                listener
            }
            #[cfg(unix)]
            ListenAddress::Unix(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = tokio::net::UnixListener::bind(addr)
                    .map(ListenerInner::Unix)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?;
                set_unix_socket_write_only(addr)
                    .await
                    .context(FailedToConfigureListener {
                        address: listen_address.clone(),
                        setting: "read/write permissions",
                    })?;

                listener
            }
            #[cfg(not(unix))]
            ListenAddress::Unixgram(_) | ListenAddress::Unix(_) => {
                return Err(ListenerError::InvalidConfiguration {
                    reason: "Unix listen addresses are not supported on this platform",
                });
            }
            #[cfg(windows)]
            ListenAddress::NamedPipe {
                name: _,
                security_descriptor,
                input_buffer_size,
            } => {
                let path = listen_address
                    .as_windows_named_pipe_path()
                    .expect("named pipe address should produce a named pipe path");
                create_named_pipe_server(&path, security_descriptor, *input_buffer_size, true)
                    .map(|server| ListenerInner::NamedPipe {
                        server,
                        path,
                        security_descriptor: security_descriptor.clone(),
                        input_buffer_size: *input_buffer_size,
                    })
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?
            }
            #[cfg(not(windows))]
            ListenAddress::NamedPipe { .. } => {
                return Err(ListenerError::InvalidConfiguration {
                    reason: "Named pipe listen addresses are not supported on this platform",
                });
            }
        };

        Ok(Self {
            listen_address,
            inner,
            socket_receive_buffer_size: None,
        })
    }

    /// Sets the socket receive buffer size for this listener.
    ///
    /// The receive buffer size applies to accepted streams. `None` keeps the OS default.
    pub fn with_receive_buffer_size(mut self, socket_receive_buffer_size: Option<usize>) -> Self {
        self.socket_receive_buffer_size = socket_receive_buffer_size;
        self
    }

    /// Gets a reference to the listen address.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

    /// Minimum number of I/O buffers needed to safely service every stream this listener will yield.
    ///
    /// Connectionless streams permanently retain their buffer for the lifetime of the stream, so the reservation
    /// matches the total number of yielded streams. Connection-oriented streams return their buffer to the pool
    /// between connections, so the reservation is a lower bound of `1` per listener.
    pub fn min_buffer_reservation(&self) -> usize {
        match &self.inner {
            ListenerInner::Tcp(_) => 1,
            ListenerInner::Udp(sockets) => sockets.len(),
            #[cfg(unix)]
            ListenerInner::Unixgram(_) => 1,
            #[cfg(unix)]
            ListenerInner::Unix(_) => 1,
            #[cfg(windows)]
            ListenerInner::NamedPipe { .. } => 1,
        }
    }

    /// Accepts a new stream from the listener.
    ///
    /// For connection-oriented address families, this will accept a new connection and return a `Stream` that's bound
    /// to that remote peer. For connectionless address families, this will yield up to the configured number of
    /// pre-bound `Stream`s—one per call—before returning pending forever.
    ///
    /// ## Errors
    ///
    /// If the listener fails to accept a new stream, or if the accepted stream can't be configured correctly, an error
    /// is returned.
    pub async fn accept(&mut self) -> Result<Stream, ListenerError> {
        let stream_type = self.listen_address.listener_type();
        match &mut self.inner {
            ListenerInner::Tcp(tcp) => {
                let (socket, addr) = tcp.accept().await.context(FailedToAccept {
                    address: self.listen_address.clone(),
                })?;
                configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                Ok((socket, addr).into())
            }
            ListenerInner::Udp(udp) => {
                if let Some(socket) = udp.pop_front() {
                    configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                    Ok(socket.into())
                } else {
                    pending().await
                }
            }
            #[cfg(unix)]
            ListenerInner::Unixgram(unix) => {
                if let Some(socket) = unix.take() {
                    configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type,
                    })?;
                    Ok(socket.into())
                } else {
                    pending().await
                }
            }
            #[cfg(unix)]
            ListenerInner::Unix(unix) => unix
                .accept()
                .await
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                })
                .and_then(|(socket, _)| {
                    configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type,
                    })?;
                    Ok(socket.into())
                }),
            #[cfg(windows)]
            ListenerInner::NamedPipe {
                server,
                path,
                security_descriptor,
                input_buffer_size,
            } => {
                server.connect().await.context(FailedToAccept {
                    address: self.listen_address.clone(),
                })?;
                let connected = mem::replace(
                    server,
                    create_named_pipe_server(path, security_descriptor, *input_buffer_size, false).context(
                        FailedToBind {
                            address: self.listen_address.clone(),
                        },
                    )?,
                );
                Ok(connected.into())
            }
        }
    }
}

#[cfg(windows)]
fn create_named_pipe_server(
    path: &str, security_descriptor: &str, input_buffer_size: Option<u32>, first_instance: bool,
) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first_instance).out_buffer_size(0);
    if let Some(input_buffer_size) = input_buffer_size {
        options.in_buffer_size(input_buffer_size);
    }

    let mut security_attributes = NamedPipeSecurityAttributes::from_sddl(security_descriptor)?;
    unsafe { options.create_with_security_attributes_raw(path, security_attributes.as_mut_ptr()) }
}

#[cfg(windows)]
struct NamedPipeSecurityAttributes {
    descriptor: *mut c_void,
    attributes: SECURITY_ATTRIBUTES,
}

#[cfg(windows)]
impl NamedPipeSecurityAttributes {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let mut descriptor = ptr::null_mut();
        let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SDDL_REVISION_1 as u32,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: FALSE,
            },
        })
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }
}

#[cfg(windows)]
impl Drop for NamedPipeSecurityAttributes {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            unsafe {
                let _ = LocalFree(self.descriptor as HLOCAL);
            }
        }
    }
}

fn configure_stream_socket_receive_buffer_size<'sock, S>(
    socket: &'sock S, recv_buffer_size: Option<usize>, stream_type: &'static str,
) -> Result<(), ListenerError>
where
    SockRef<'sock>: From<&'sock S>,
{
    if let Some(size) = recv_buffer_size {
        SockRef::from(socket)
            .set_recv_buffer_size(size)
            .context(FailedToConfigureStream {
                setting: SOCKET_RECV_BUFFER_SIZE_SETTING,
                stream_type,
            })?;
    }

    Ok(())
}

async fn bind_udp_socket_standard(addr: SocketAddr) -> io::Result<VecDeque<TokioUdpSocket>> {
    let socket = TokioUdpSocket::bind(addr).await?;
    let mut sockets = VecDeque::with_capacity(1);
    sockets.push_back(socket);
    Ok(sockets)
}

#[cfg(target_os = "linux")]
async fn bind_udp_sockets(
    addr: SocketAddr, maybe_socket_count: Option<NonZeroUsize>,
) -> io::Result<VecDeque<TokioUdpSocket>> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};

    fn bind_one(addr: SocketAddr) -> io::Result<TokioUdpSocket> {
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&SockAddr::from(addr))?;
        let std_socket: std::net::UdpSocket = socket.into();
        TokioUdpSocket::from_std(std_socket)
    }

    let socket_count = maybe_socket_count.map(NonZeroUsize::get).unwrap_or(1);
    if socket_count == 1 {
        return bind_udp_socket_standard(addr).await;
    }

    let mut sockets = VecDeque::with_capacity(socket_count);

    // Bind the first socket to learn the effective address. When the caller passed port `0`, the OS assigns an
    // ephemeral port; every remaining socket must bind to that same port for SO_REUSEPORT load balancing to work
    // (otherwise each subsequent socket would receive its own distinct ephemeral port).
    let first = bind_one(addr)?;
    let effective_addr = first.local_addr()?;
    sockets.push_back(first);

    for _ in 1..socket_count {
        sockets.push_back(bind_one(effective_addr)?);
    }

    Ok(sockets)
}

#[cfg(not(target_os = "linux"))]
async fn bind_udp_sockets(addr: SocketAddr, _: Option<NonZeroUsize>) -> io::Result<VecDeque<TokioUdpSocket>> {
    bind_udp_socket_standard(addr).await
}

enum ConnectionOrientedListenerInner {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

/// A connection-oriented network listener.
///
/// `ConnectionOrientedListener` is conceptually the same as `Listener`, but specifically works with connection-oriented
/// protocols. This variant is provided to facilitate usages where only a connection-oriented stream makes sense, such
/// as an HTTP server.
pub struct ConnectionOrientedListener {
    listen_address: ListenAddress,
    inner: ConnectionOrientedListenerInner,
}

impl ConnectionOrientedListener {
    /// Creates a new `ConnectionOrientedListener` from the given listen address.
    ///
    /// ## Errors
    ///
    /// If the listen address isn't a connection-oriented address family, or if the listen address can't be bound, or
    /// if the listener can't be configured correctly, an error is returned.
    pub async fn from_listen_address(listen_address: ListenAddress) -> Result<Self, ListenerError> {
        let inner = match &listen_address {
            ListenAddress::Tcp(addr) => TcpListener::bind(addr)
                .await
                .map(ConnectionOrientedListenerInner::Tcp)
                .context(FailedToBind {
                    address: listen_address.clone(),
                })?,
            #[cfg(unix)]
            ListenAddress::Unix(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = tokio::net::UnixListener::bind(addr)
                    .map(ConnectionOrientedListenerInner::Unix)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?;
                set_unix_socket_write_only(addr)
                    .await
                    .context(FailedToConfigureListener {
                        address: listen_address.clone(),
                        setting: "read/write permissions",
                    })?;

                listener
            }
            _ => {
                return Err(ListenerError::InvalidConfiguration {
                    #[cfg(unix)]
                    reason: "only TCP and Unix listen addresses are supported",
                    #[cfg(not(unix))]
                    reason: "only TCP listen addresses are supported on this platform",
                })
            }
        };

        Ok(Self { listen_address, inner })
    }

    /// Gets a reference to the listen address.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

    /// Returns the actual local socket address this listener is bound to.
    ///
    /// This is useful when binding to port `0` to discover the ephemeral port assigned by the OS.
    pub fn local_addr(&self) -> Result<SocketAddr, ListenerError> {
        match &self.inner {
            ConnectionOrientedListenerInner::Tcp(tcp) => tcp.local_addr().context(FailedToConfigureListener {
                address: self.listen_address.clone(),
                setting: "local_addr",
            }),
            #[cfg(unix)]
            ConnectionOrientedListenerInner::Unix(_) => Err(ListenerError::InvalidConfiguration {
                reason: "local_addr is not supported for Unix listeners",
            }),
        }
    }

    /// Accepts a new connection from the listener.
    ///
    /// ## Errors
    ///
    /// If the listener fails to accept a new connection, or if the accepted connection can't be configured correctly,
    /// an error is returned.
    pub async fn accept(&mut self) -> Result<Connection, ListenerError> {
        match &mut self.inner {
            ConnectionOrientedListenerInner::Tcp(tcp) => tcp
                .accept()
                .await
                .map(|(stream, addr)| Connection::Tcp(stream, addr))
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                }),
            #[cfg(unix)]
            ConnectionOrientedListenerInner::Unix(unix) => unix
                .accept()
                .await
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                })
                .and_then(|(socket, _)| {
                    let stream_type = self.listen_address.listener_type();
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type,
                    })?;
                    Ok(Connection::Unix(socket))
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::BytesMut;
    use tokio::{net::TcpStream, time::timeout};

    use super::*;

    const REQUESTED_RECV_BUFFER_SIZE: usize = 131_072;
    const TEST_PACKET: &[u8] = b"hello";

    #[cfg(not(windows))]
    #[tokio::test]
    async fn named_pipe_listener_is_unsupported_on_non_windows() {
        let address = ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)");

        let err = match Listener::from_listen_address(address, None).await {
            Ok(_) => panic!("named pipes should be unsupported on non-Windows"),
            Err(err) => err,
        };

        assert!(err
            .to_string()
            .contains("Named pipe listen addresses are not supported"));
    }

    #[tokio::test]
    async fn zero_receive_buffer_size_preserves_udp_default() {
        let default_address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut default_listener = Listener::from_listen_address(default_address, None)
            .await
            .expect("default listener should bind");
        let default_stream = default_listener
            .accept()
            .await
            .expect("default stream should be accepted");
        let default_recv_buffer_size = default_stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");

        let zero_address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut zero_listener = Listener::from_listen_address(zero_address, None)
            .await
            .expect("zero-sized listener should bind")
            .with_receive_buffer_size(None);
        let zero_stream = zero_listener
            .accept()
            .await
            .expect("zero-sized stream should be accepted");
        let zero_recv_buffer_size = zero_stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");

        assert_eq!(zero_recv_buffer_size, default_recv_buffer_size);
    }

    #[tokio::test]
    async fn udp_listener_sets_receive_buffer_size() {
        let address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address, None)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));
        let local_addr = udp_local_addr(&listener).expect("local addr should be available");

        let sender = TokioUdpSocket::bind("127.0.0.1:0").await.expect("sender should bind");
        sender
            .send_to(TEST_PACKET, local_addr)
            .await
            .expect("packet should send");

        let mut stream = listener.accept().await.expect("listener should accept UDP stream");
        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );

        let mut buffer = BytesMut::with_capacity(TEST_PACKET.len());
        let (bytes_read, _) = timeout(Duration::from_secs(1), stream.receive(&mut buffer))
            .await
            .expect("receive should not time out")
            .expect("packet should receive");

        assert_eq!(bytes_read, TEST_PACKET.len());
        assert_eq!(&buffer[..], TEST_PACKET);
    }

    #[tokio::test]
    async fn tcp_listener_sets_receive_buffer_size() {
        let address = ListenAddress::Tcp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address, None)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));
        let local_addr = tcp_local_addr(&listener).expect("local addr should be available");

        let client = tokio::spawn(async move { TcpStream::connect(local_addr).await });
        let stream = timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("accept should not time out")
            .expect("listener should accept TCP stream");
        client
            .await
            .expect("client task should complete")
            .expect("client should connect");

        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );
        assert!(!stream.is_connectionless());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unixgram_listener_sets_receive_buffer_size() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = temp_dir.path().join("dogstatsd.sock");
        let address = ListenAddress::Unixgram(socket_path.clone());
        let mut listener = Listener::from_listen_address(address, None)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));

        let sender = tokio::net::UnixDatagram::unbound().expect("sender should be created");
        sender
            .send_to(TEST_PACKET, &socket_path)
            .await
            .expect("packet should send");

        let mut stream = listener
            .accept()
            .await
            .expect("listener should accept UDS datagram stream");
        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );

        let mut buffer = BytesMut::with_capacity(TEST_PACKET.len());
        let (bytes_read, _) = timeout(Duration::from_secs(1), stream.receive(&mut buffer))
            .await
            .expect("receive should not time out")
            .expect("packet should receive");

        assert_eq!(bytes_read, TEST_PACKET.len());
        assert_eq!(&buffer[..], TEST_PACKET);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_stream_listener_accepts_with_receive_buffer_size() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = temp_dir.path().join("dogstatsd-stream.sock");
        let address = ListenAddress::Unix(socket_path.clone());
        let mut listener = Listener::from_listen_address(address, None)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));

        let client = tokio::spawn(async move { tokio::net::UnixStream::connect(socket_path).await });
        let stream = timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("accept should not time out")
            .expect("listener should accept UDS stream");
        client
            .await
            .expect("client task should complete")
            .expect("client should connect");

        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );
        assert!(!stream.is_connectionless());
    }

    #[tokio::test]
    async fn udp_listener_default_yields_single_stream_then_pends() {
        let address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address, None)
            .await
            .expect("listener should bind");
        assert_eq!(listener.min_buffer_reservation(), 1);

        let _stream = listener.accept().await.expect("first accept should yield a stream");

        let pending_result = timeout(Duration::from_millis(50), listener.accept()).await;
        assert!(pending_result.is_err(), "second accept should be pending forever");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn udp_listener_with_streams_yields_n_streams_then_pends() {
        let count = NonZeroUsize::new(3).unwrap();
        let address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address, Some(count))
            .await
            .expect("listener should bind with multiple sockets");
        assert_eq!(listener.min_buffer_reservation(), 3);

        let ports = udp_socket_ports(&listener);
        assert_eq!(ports.len(), 3);
        assert!(ports.iter().all(|p| *p == ports[0]), "all sockets should share a port");

        for _ in 0..3 {
            listener.accept().await.expect("accept should yield a stream");
        }

        let pending_result = timeout(Duration::from_millis(50), listener.accept()).await;
        assert!(pending_result.is_err(), "fourth accept should be pending forever");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn udp_listener_port_zero_shares_port_across_sockets() {
        let count = NonZeroUsize::new(2).unwrap();
        let address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let listener = Listener::from_listen_address(address, Some(count))
            .await
            .expect("listener should bind two sockets to the same ephemeral port");

        let ports = udp_socket_ports(&listener);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0], ports[1]);
        assert_ne!(ports[0], 0, "OS should have assigned an ephemeral port");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn udp_listener_with_streams_applies_receive_buffer_size_to_all_sockets() {
        let count = NonZeroUsize::new(3).unwrap();
        let address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address, Some(count))
            .await
            .expect("listener should bind multiple sockets")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));

        for _ in 0..3 {
            let stream = listener.accept().await.expect("stream should be accepted");
            let buf_size = stream
                .recv_buffer_size()
                .expect("receive buffer size should be available");
            assert!(
                buf_size >= REQUESTED_RECV_BUFFER_SIZE,
                "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {buf_size}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn udp_socket_ports(listener: &Listener) -> Vec<u16> {
        match &listener.inner {
            ListenerInner::Udp(sockets) => sockets
                .iter()
                .map(|s| s.local_addr().expect("socket should have local addr").port())
                .collect(),
            _ => panic!("expected UDP listener"),
        }
    }

    fn tcp_local_addr(listener: &Listener) -> io::Result<SocketAddr> {
        match &listener.inner {
            ListenerInner::Tcp(tcp) => tcp.local_addr(),
            _ => panic!("expected TCP listener"),
        }
    }

    fn udp_local_addr(listener: &Listener) -> io::Result<SocketAddr> {
        match &listener.inner {
            ListenerInner::Udp(sockets) => sockets.front().expect("UDP listener has no sockets").local_addr(),
            _ => panic!("expected UDP listener"),
        }
    }
}
