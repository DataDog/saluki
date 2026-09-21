//! Network listeners.
#[cfg(unix)]
use std::path::PathBuf;
use std::{collections::VecDeque, future::pending, io, net::SocketAddr, num::NonZeroUsize, sync::Arc};
#[cfg(windows)]
use std::{ffi::c_void, mem, ptr};

use saluki_core::runtime::state::Subleases;
use snafu::{ResultExt as _, Snafu};
use socket2::SockRef;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::net::{TcpListener, UdpSocket as TokioUdpSocket};
#[cfg(unix)]
use tokio::net::{UnixDatagram, UnixListener};
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
use crate::net::util::retry::ExponentialBackoff;
use crate::net::{addr::BoundListenAddress, stream::SubleasedSocket};

mod recovery;
use self::recovery::AcceptRecovery;

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
    Tcp(TcpListener, SocketAddr),

    Udp {
        sockets: Vec<Arc<TokioUdpSocket>>,
        handed_out: usize,
        bound_addr: SocketAddr,
    },

    #[cfg(unix)]
    Unixgram {
        socket: Arc<UnixDatagram>,
        handed_out: bool,
        bound_path: PathBuf,
    },

    #[cfg(unix)]
    Unix(UnixListener, PathBuf),

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
/// # Connection-oriented vs connectionless listeners
///
/// For listeners on connection-oriented address families (for example, TCP, Unix domain sockets in stream mode), the listener
/// will listen for and accept new connections in the typical fashion. However, for connectionless address families
/// (for example, UDP, Unix domain sockets in datagram mode), there is no concept of a "connection" and so nothing to be
/// continually "accepted." Instead, `Listener` will emit a single `Stream` that can be used to send and receive data
/// from multiple remote peers.
///
/// # UDP autoscaling
///
/// On Linux, UDP listeners can be configured to bind multiple sockets to the same address using `SO_REUSEPORT`,
/// allowing the kernel to load-balance incoming datagrams across them. The configured number of sockets are yielded
/// one at a time from successive calls to [`Listener::accept`] before the listener returns pending forever. See
/// the `udp_streams` parameter of [`Listener::from_listen_address`].
pub struct Listener {
    listen_address: ListenAddress,
    inner: ListenerInner,
    socket_receive_buffer_size: Option<usize>,
    accept_recovery: AcceptRecovery,
    subleases: Option<Subleases>,
}

impl Listener {
    /// Creates a new `Listener` from the given listen address.
    ///
    /// # UDP streams
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
    /// # Errors
    ///
    /// If the listen address can't be bound, or if the listener can't be configured correctly, an error is returned.
    pub async fn from_listen_address(
        listen_address: ListenAddress, mut udp_streams: Option<NonZeroUsize>,
    ) -> Result<Self, ListenerError> {
        let inner = match &listen_address {
            ListenAddress::Tcp(addr) => TcpListener::bind(addr)
                .await
                .and_then(|listener| listener.local_addr().map(|addr| ListenerInner::Tcp(listener, addr)))
                .context(FailedToBind {
                    address: listen_address.clone(),
                })?,
            ListenAddress::Udp(addr) => {
                // See if we have platform support for SO_REUSEPORT, and if not, fall back to the default behavior.
                if !socket_reuseport_supported() {
                    udp_streams = None;
                    warn!("SO_REUSEPORT not supported on the current platform. Falling back to the default behavior.");
                }

                let (sockets, bound_addr) = bind_udp_sockets(*addr, udp_streams).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;
                ListenerInner::Udp {
                    sockets: sockets.into_iter().map(Arc::new).collect(),
                    handed_out: 0,
                    bound_addr,
                }
            }
            #[cfg(unix)]
            ListenAddress::Unixgram(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = UnixDatagram::bind(addr)
                    .map(|socket| ListenerInner::Unixgram {
                        socket: Arc::new(socket),
                        handed_out: false,
                        bound_path: addr.clone(),
                    })
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

                let listener = UnixListener::bind(addr)
                    .map(|listener| ListenerInner::Unix(listener, addr.clone()))
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
            accept_recovery: AcceptRecovery::default(),
            subleases: None,
        })
    }

    /// Sets the subleases issued for the connectionless sockets this listener lends out.
    ///
    /// Only meaningful for a listener owned by a
    /// [`ResourceRegistry`][saluki_core::runtime::state::ResourceRegistry]: it is what stops the registry handing the
    /// listener to another acquirer while a stream from the previous one is still reading the socket underneath it.
    pub fn with_subleases(mut self, subleases: Subleases) -> Self {
        self.subleases = Some(subleases);
        self
    }

    /// Sets the socket receive buffer size for this listener.
    ///
    /// The receive buffer size applies to accepted streams. `None` keeps the OS default.
    pub fn with_receive_buffer_size(mut self, socket_receive_buffer_size: Option<usize>) -> Self {
        self.socket_receive_buffer_size = socket_receive_buffer_size;
        self
    }

    /// Sets the backoff strategy used when [`accept`](Self::accept) encounters a recoverable, system-wide error.
    ///
    /// In some cases, accepting a connection can return a transient error that corresponds with an underlying issue
    /// with the system, such as too many file descriptors being open. While it is safe to retry accepting new
    /// connections after encountering such an error, an acceptor could potentially busy loop since calls will return
    /// immediately if not otherwise throttled in some way.
    ///
    /// Listeners will apply a backoff during subsequent `accept` calls to avoid excess resource consumption due to
    /// this type of busy looping.
    ///
    /// Defaults to 10 milliseconds, doubling per consecutive failure up to 1 second, jittered down by up to half.
    pub fn with_accept_backoff(mut self, accept_backoff: ExponentialBackoff) -> Self {
        self.accept_recovery = AcceptRecovery::from_backoff(accept_backoff);
        self
    }

    /// Gets a reference to the listen address.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

    /// Gets the bound listen address for this listener.
    pub fn bound_listen_address(&self) -> BoundListenAddress {
        match &self.inner {
            ListenerInner::Tcp(_, bound_addr) => BoundListenAddress::Tcp(*bound_addr),
            ListenerInner::Udp { bound_addr, .. } => BoundListenAddress::Udp(*bound_addr),
            #[cfg(unix)]
            ListenerInner::Unixgram { bound_path, .. } => BoundListenAddress::Unixgram(bound_path.clone()),
            #[cfg(unix)]
            ListenerInner::Unix(_, bound_addr) => BoundListenAddress::Unix(bound_addr.clone()),
            #[cfg(windows)]
            ListenerInner::NamedPipe { path, .. } => BoundListenAddress::NamedPipe(path.clone()),
        }
    }

    /// Minimum number of I/O buffers needed to service every stream this listener will yield.
    ///
    /// Connectionless listeners contribute one buffer per yielded stream. Connection-oriented listeners contribute one
    /// buffer per configured listener.
    pub fn min_buffer_reservation(&self) -> usize {
        match &self.inner {
            ListenerInner::Tcp(_, _) => 1,
            ListenerInner::Udp { sockets, .. } => sockets.len(),
            #[cfg(unix)]
            ListenerInner::Unixgram { .. } => 1,
            #[cfg(unix)]
            ListenerInner::Unix(_, _) => 1,
            #[cfg(windows)]
            ListenerInner::NamedPipe { .. } => 1,
        }
    }

    /// Readies the listener for its next holder.
    pub(crate) fn rearm(&mut self) {
        self.accept_recovery.reset();

        // Update our tracking of the connectionless streams we've handed out since we're being lent (overall) to a new
        // holder.
        match &mut self.inner {
            ListenerInner::Udp { handed_out, .. } => *handed_out = 0,
            #[cfg(unix)]
            ListenerInner::Unixgram { handed_out, .. } => *handed_out = false,
            ListenerInner::Tcp(_, _) => {}
            #[cfg(unix)]
            ListenerInner::Unix(_, _) => {}
            #[cfg(windows)]
            ListenerInner::NamedPipe { .. } => {}
        }
    }

    /// Accepts a new stream from the listener.
    ///
    /// For connection-oriented address families, this will accept a new connection and return a `Stream` that's bound
    /// to that remote peer. For connectionless address families, this will yield up to the configured number of
    /// pre-bound `Stream`s (one per call) before returning pending forever.
    ///
    /// # Recoverable failures
    ///
    /// Not every failed accept means anything is wrong with the listener. A connection can be reset between arriving
    /// and being accepted, and the process or kernel can run out of descriptors or buffers while a connection sits
    /// queued. In these cases, the listener is fine and the right response is to accept again -- immediately for the
    /// former, and after a wait for the latter, since nothing changes until the resource frees up. Both are handled
    /// here rather than being surfaced, so an error from this method always means the listener is done. See
    /// [`with_accept_backoff`](Self::with_accept_backoff) for the wait.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. No stream is ever lost by dropping the returned future: a successful accept returns
    /// straight away, so the only thing in flight at a cancellation point is a failure being recovered from.
    ///
    /// # Errors
    ///
    /// If the listener can no longer produce streams, or if an accepted stream can't be configured correctly, an error
    /// is returned.
    pub async fn accept(&mut self) -> Result<Stream, ListenerError> {
        loop {
            match self.accept_once().await {
                Ok(stream) => {
                    self.accept_recovery.accept_succeeded();
                    return Ok(stream);
                }
                Err(error) => self.accept_recovery.recover(&self.listen_address, error).await?,
            }
        }
    }

    /// Makes a single attempt to accept a new stream, without recovering from a failure.
    async fn accept_once(&mut self) -> Result<Stream, ListenerError> {
        let stream_type = self.listen_address.listener_type();
        match &mut self.inner {
            ListenerInner::Tcp(tcp, _) => {
                let (socket, addr) = tcp.accept().await.context(FailedToAccept {
                    address: self.listen_address.clone(),
                })?;
                configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                Ok((socket, addr).into())
            }
            ListenerInner::Udp {
                sockets, handed_out, ..
            } => {
                match sockets.get(*handed_out) {
                    Some(socket) => {
                        *handed_out += 1;

                        configure_stream_socket_receive_buffer_size(
                            &**socket,
                            self.socket_receive_buffer_size,
                            stream_type,
                        )?;

                        let sublease = self.subleases.as_ref().and_then(Subleases::issue);
                        Ok(SubleasedSocket::new(Arc::clone(socket), sublease).into())
                    }
                    // Every socket is already in use. There is nothing further to yield, but the caller is typically an
                    // accept loop, so go quiet rather than returning an error.
                    None => pending().await,
                }
            }
            #[cfg(unix)]
            ListenerInner::Unixgram { socket, handed_out, .. } => {
                if *handed_out {
                    return pending().await;
                }
                *handed_out = true;

                configure_stream_socket_receive_buffer_size(&**socket, self.socket_receive_buffer_size, stream_type)?;
                enable_uds_socket_credentials(&**socket).context(FailedToConfigureStream {
                    setting: "SO_PASSCRED",
                    stream_type,
                })?;

                let sublease = self.subleases.as_ref().and_then(Subleases::issue);
                Ok(SubleasedSocket::new(Arc::clone(socket), sublease).into())
            }
            #[cfg(unix)]
            ListenerInner::Unix(unix, _) => unix
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

fn bind_udp_socket(addr: SocketAddr, _allow_multi_bind: bool) -> io::Result<TokioUdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};

    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;

    #[cfg(target_os = "linux")]
    if _allow_multi_bind {
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
    }

    socket.set_nonblocking(true)?;
    socket.bind(&SockAddr::from(addr))?;
    let std_socket: std::net::UdpSocket = socket.into();
    TokioUdpSocket::from_std(std_socket)
}

#[cfg(target_os = "linux")]
async fn bind_udp_sockets(
    addr: SocketAddr, maybe_socket_count: Option<NonZeroUsize>,
) -> io::Result<(VecDeque<TokioUdpSocket>, SocketAddr)> {
    let socket_count = maybe_socket_count.map(NonZeroUsize::get).unwrap_or(1);
    let mut sockets = VecDeque::with_capacity(socket_count);

    // Bind the first socket to learn the effective address. When the caller passed port `0`, the OS assigns an
    // ephemeral port; every remaining socket must bind to that same port for SO_REUSEPORT load balancing to work
    // (otherwise each subsequent socket would receive its own distinct ephemeral port).
    let first = bind_udp_socket(addr, true)?;
    let effective_addr = first.local_addr()?;
    sockets.push_back(first);

    for _ in 1..socket_count {
        sockets.push_back(bind_udp_socket(effective_addr, true)?);
    }

    Ok((sockets, effective_addr))
}

#[cfg(not(target_os = "linux"))]
async fn bind_udp_sockets(
    addr: SocketAddr, _: Option<NonZeroUsize>,
) -> io::Result<(VecDeque<TokioUdpSocket>, SocketAddr)> {
    let socket = bind_udp_socket(addr, false)?;
    let local_addr = socket.local_addr()?;

    let mut sockets = VecDeque::new();
    sockets.push_back(socket);

    Ok((sockets, local_addr))
}

enum ConnectionOrientedListenerInner {
    Tcp(TcpListener, SocketAddr),
    #[cfg(unix)]
    Unix(UnixListener, PathBuf),
}

/// A connection-oriented network listener.
///
/// `ConnectionOrientedListener` is conceptually the same as `Listener`, but specifically works with connection-oriented
/// protocols. This variant is provided to facilitate usages where only a connection-oriented stream makes sense, such
/// as an HTTP server.
pub struct ConnectionOrientedListener {
    listen_address: ListenAddress,
    inner: ConnectionOrientedListenerInner,
    accept_recovery: AcceptRecovery,
}

impl ConnectionOrientedListener {
    /// Creates a new `ConnectionOrientedListener` from the given listen address.
    ///
    /// # Errors
    ///
    /// If the listen address isn't a connection-oriented address family, or if the listen address can't be bound, or
    /// if the listener can't be configured correctly, an error is returned.
    pub async fn from_listen_address(listen_address: ListenAddress) -> Result<Self, ListenerError> {
        let inner = match &listen_address {
            ListenAddress::Tcp(addr) => TcpListener::bind(addr)
                .await
                .and_then(|listener| {
                    listener
                        .local_addr()
                        .map(|addr| ConnectionOrientedListenerInner::Tcp(listener, addr))
                })
                .context(FailedToBind {
                    address: listen_address.clone(),
                })?,
            #[cfg(unix)]
            ListenAddress::Unix(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = UnixListener::bind(addr)
                    .map(|listener| ConnectionOrientedListenerInner::Unix(listener, addr.clone()))
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

        Ok(Self {
            listen_address,
            inner,
            accept_recovery: AcceptRecovery::default(),
        })
    }

    /// Sets the backoff strategy used when [`accept`](Self::accept) encounters a recoverable, system-wide error.
    ///
    /// See [`Listener::with_accept_backoff`], which this mirrors.
    pub fn with_accept_backoff(mut self, accept_backoff: ExponentialBackoff) -> Self {
        self.accept_recovery = AcceptRecovery::from_backoff(accept_backoff);
        self
    }

    /// Gets a reference to the listen address.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

    /// Gets the bound listen address for this listener.
    pub fn bound_listen_address(&self) -> BoundListenAddress {
        match &self.inner {
            ConnectionOrientedListenerInner::Tcp(_, bound_addr) => BoundListenAddress::Tcp(*bound_addr),
            #[cfg(unix)]
            ConnectionOrientedListenerInner::Unix(_, bound_addr) => BoundListenAddress::Unix(bound_addr.clone()),
        }
    }

    /// Readies the listener for its next holder.
    pub(crate) fn rearm(&mut self) {
        self.accept_recovery.reset();
    }

    /// Accepts a new connection from the listener.
    ///
    /// # Recoverable failures
    ///
    /// Failures that say nothing about the listener -- a connection reset before it could be accepted, a momentary
    /// shortage of descriptors -- are recovered from here rather than surfaced, so an error from this method means the
    /// listener is done. See [`with_accept_backoff`](Self::with_accept_backoff) for the wait applied to a shortage.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. No connection is ever lost by dropping the returned future: a successful accept
    /// returns straight away, so the only thing in flight at a cancellation point is a failure being recovered from.
    ///
    /// # Errors
    ///
    /// If the listener can no longer produce connections, or if an accepted connection can't be configured correctly,
    /// an error is returned.
    pub async fn accept(&mut self) -> Result<Connection, ListenerError> {
        loop {
            match self.accept_once().await {
                Ok(connection) => {
                    self.accept_recovery.accept_succeeded();
                    return Ok(connection);
                }
                Err(error) => self.accept_recovery.recover(&self.listen_address, error).await?,
            }
        }
    }

    /// Makes a single attempt to accept a new connection, without recovering from a failure.
    async fn accept_once(&mut self) -> Result<Connection, ListenerError> {
        match &mut self.inner {
            ConnectionOrientedListenerInner::Tcp(tcp, _) => tcp
                .accept()
                .await
                .map(|(stream, addr)| Connection::Tcp(stream, addr))
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                }),
            #[cfg(unix)]
            ConnectionOrientedListenerInner::Unix(unix, _) => unix
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
        let default_address = ListenAddress::udp_loopback(0);
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

        let zero_address = ListenAddress::udp_loopback(0);
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
        let address = ListenAddress::udp_loopback(0);
        let mut listener = Listener::from_listen_address(address, None)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));
        let local_addr = udp_local_addr(&listener);

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
        let local_addr = tcp_local_addr(&listener);

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
        let address = ListenAddress::udp_loopback(0);
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
        let address = ListenAddress::udp_loopback(0);
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
        let address = ListenAddress::udp_loopback(0);
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
        let address = ListenAddress::udp_loopback(0);
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
            ListenerInner::Udp { sockets, .. } => sockets
                .iter()
                .map(|socket| socket.local_addr().expect("socket should have local addr").port())
                .collect(),
            _ => panic!("expected UDP listener"),
        }
    }

    fn tcp_local_addr(listener: &Listener) -> SocketAddr {
        match &listener.inner {
            ListenerInner::Tcp(_, local_addr) => *local_addr,
            _ => panic!("expected TCP listener"),
        }
    }

    fn udp_local_addr(listener: &Listener) -> SocketAddr {
        match &listener.inner {
            ListenerInner::Udp { bound_addr, .. } => *bound_addr,
            _ => panic!("expected UDP listener"),
        }
    }
}
