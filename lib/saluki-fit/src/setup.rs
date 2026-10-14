use crate::cancellation::CancellationToken;
use crate::config::{validate_capacity, ConsumerConfig, ProducerConfig, SetupEndpoint};
use crate::contract::{check_contract, contract, ProtocolDescriptor, RECORD_HEADER_SIZE};
use crate::mapping::Shared;
use crate::ring::RING_OFFSET;
use crate::{invalid, Consumer, Producer};
use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
const MAX_FRAME: usize = 256;
const CANCEL_CHECK_INTERVAL: Duration = Duration::from_millis(50);
static LAST_SESSION_ID: AtomicU64 = AtomicU64::new(0);

fn next_session_id() -> io::Result<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?;
    let nanos = u64::try_from(now.as_nanos()).map_err(|_| invalid("session clock exceeds u64"))?;
    let seed = nanos ^ (std::process::id() as u64).rotate_left(32);
    loop {
        let previous = LAST_SESSION_ID.load(Ordering::Relaxed);
        let next = previous
            .max(seed)
            .checked_add(1)
            .ok_or_else(|| invalid("session identifiers exhausted"))?;
        if LAST_SESSION_ID
            .compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(next);
        }
    }
}

fn phase(name: &'static str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{name}: {error}"))
}

fn deadline_remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "setup deadline expired"))
}

fn check_cancel(cancellation: Option<&CancellationToken>) -> io::Result<()> {
    if let Some(token) = cancellation {
        token.check()?;
    }
    Ok(())
}

fn sleep_until(duration: Duration, cancellation: Option<&CancellationToken>) -> io::Result<()> {
    if let Some(token) = cancellation {
        token.wait_for(duration)
    } else {
        thread::sleep(duration);
        Ok(())
    }
}

fn wait_for(
    fd: RawFd, events: libc::c_short, deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<()> {
    loop {
        check_cancel(cancellation)?;
        let remaining = deadline_remaining(deadline)?;
        let remaining = if cancellation.is_some() {
            remaining.min(CANCEL_CHECK_INTERVAL)
        } else {
            remaining
        };
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd { fd, events, revents: 0 };
        let count = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if count > 0 {
            return Ok(());
        }
        if count == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

enum SetupStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Read for SetupStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for SetupStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

impl AsRawFd for SetupStream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Unix(stream) => stream.as_raw_fd(),
            Self::Tcp(stream) => stream.as_raw_fd(),
        }
    }
}

fn read_exact_until(
    stream: &mut SetupStream, mut buf: &mut [u8], deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<()> {
    while !buf.is_empty() {
        check_cancel(cancellation)?;
        deadline_remaining(deadline)?;
        match stream.read(buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "setup connection closed")),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_for(stream.as_raw_fd(), libc::POLLIN, deadline, cancellation)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all_until(
    stream: &mut SetupStream, mut buf: &[u8], deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<()> {
    while !buf.is_empty() {
        check_cancel(cancellation)?;
        deadline_remaining(deadline)?;
        match stream.write(buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "setup write returned zero")),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_for(stream.as_raw_fd(), libc::POLLOUT, deadline, cancellation)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn send(
    stream: &mut SetupStream, body: &[u8], deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<()> {
    check_cancel(cancellation)?;
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(invalid("invalid outgoing setup frame size"));
    }
    write_all_until(stream, &(body.len() as u32).to_be_bytes(), deadline, cancellation)?;
    write_all_until(stream, body, deadline, cancellation)
}

fn receive(
    stream: &mut SetupStream, deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<Vec<u8>> {
    let mut prefix = [0; 4];
    read_exact_until(stream, &mut prefix, deadline, cancellation)?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(invalid(format!("setup frame length {len} exceeds 1..={MAX_FRAME}")));
    }
    let mut body = vec![0; len];
    read_exact_until(stream, &mut body, deadline, cancellation)?;
    Ok(body)
}

fn reject(stream: &mut SetupStream, error: &io::Error, deadline: Instant) {
    let detail = error.to_string();
    let bytes = detail.as_bytes();
    let mut frame = vec![2];
    frame.extend_from_slice(&bytes[..bytes.len().min(MAX_FRAME - 1)]);
    let _ = send(stream, &frame, deadline, None);
}

fn expected_message(frame: &[u8], tag: u8) -> io::Result<&[u8]> {
    if frame.first() == Some(&2) {
        return Err(invalid(format!(
            "peer rejected setup: {}",
            String::from_utf8_lossy(&frame[1..])
        )));
    }
    if frame.first() != Some(&tag) {
        return Err(invalid(format!(
            "expected setup message {tag}, received {:?}",
            frame.first()
        )));
    }
    Ok(&frame[1..])
}

fn offer_frame(id: u64, name: &str, capacity: usize, protocol: &ProtocolDescriptor) -> Vec<u8> {
    let mut offer = vec![3];
    offer.extend_from_slice(&contract(2, protocol));
    offer.extend_from_slice(&id.to_be_bytes());
    offer.extend_from_slice(&((RING_OFFSET + capacity) as u32).to_be_bytes());
    offer.extend_from_slice(&(RING_OFFSET as u32).to_be_bytes());
    offer.extend_from_slice(&(capacity as u32).to_be_bytes());
    offer.extend_from_slice(&RECORD_HEADER_SIZE.to_be_bytes());
    offer.push(name.len() as u8);
    offer.extend_from_slice(name.as_bytes());
    offer
}

fn endpoint_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| invalid("socket path must have a parent directory"))?;
    let metadata = fs::metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket parent must be a directory owned by this user with no group/other access",
        ));
    }
    // sockaddr_un has a 104-byte path on macOS and 108 bytes on Linux, including NUL.
    if path.as_os_str().as_encoded_bytes().len() >= 104 {
        return Err(invalid("socket path is too long for macOS pathname Unix sockets"));
    }
    Ok(())
}

struct UnixEndpoint(PathBuf);
impl Drop for UnixEndpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

enum SetupListener {
    Unix { _listener: UnixListener },
    Tcp { _listener: TcpListener },
}

#[cfg(target_os = "macos")]
fn check_os_version() -> io::Result<()> {
    let mut bytes = [0u8; 32];
    let mut len = bytes.len();
    let name = b"kern.osproductversion\0";
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let value =
        simdutf8::basic::from_utf8(&bytes[..len.saturating_sub(1)]).map_err(|_| invalid("invalid macOS version"))?;
    let mut parts = value.split('.');
    let major = parts.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
    if major < 14 || (major == 14 && minor < 4) {
        return Err(invalid("shared address waits require macOS 14.4 or newer"));
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn check_os_version() -> io::Result<()> {
    Ok(())
}

/// Accept one producer and initialize its shared queue.
pub(crate) fn open(config: ConsumerConfig, protocol: ProtocolDescriptor) -> io::Result<Consumer> {
    open_inner(config, protocol, None)
}

pub(crate) fn open_with_cancel(
    config: ConsumerConfig, protocol: ProtocolDescriptor, cancellation: &CancellationToken,
) -> io::Result<Consumer> {
    open_inner(config, protocol, Some(cancellation))
}

fn open_inner(
    config: ConsumerConfig, protocol: ProtocolDescriptor, cancellation: Option<&CancellationToken>,
) -> io::Result<Consumer> {
    check_cancel(cancellation)?;
    let deadline = config.validate()?;
    protocol.validate()?;
    check_os_version()?;
    let capacity = config.ring_capacity;
    let (mut stream, _listener, _endpoint): (SetupStream, SetupListener, Option<UnixEndpoint>) = match &config.endpoint
    {
        SetupEndpoint::Unix(path) => {
            endpoint_parent(path)?;
            let listener = UnixListener::bind(path).map_err(|e| phase("binding setup socket", e))?; // Never remove an existing path to make bind succeed.
            let endpoint = Some(UnixEndpoint(path.to_path_buf()));
            fs::set_permissions(path, Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let stream = accept_until(&listener, deadline, cancellation)?;
            (stream, SetupListener::Unix { _listener: listener }, endpoint)
        }
        SetupEndpoint::Tcp(address) => {
            ensure_loopback(*address)?;
            let listener = TcpListener::bind(address).map_err(|e| phase("binding TCP setup listener", e))?;
            listener.set_nonblocking(true)?;
            let stream = accept_tcp_until(&listener, deadline, cancellation)?;
            (stream, SetupListener::Tcp { _listener: listener }, None)
        }
    };
    let hello = receive(&mut stream, deadline, cancellation).map_err(|e| phase("waiting for Hello", e))?;
    let checked = expected_message(&hello, 1).and_then(|body| check_contract(body, 1, &protocol));
    if let Err(error) = checked {
        reject(&mut stream, &error, deadline);
        return Err(error);
    }
    let id = next_session_id()?;
    let (mut shared, name) =
        Shared::create(id, capacity, protocol.version).map_err(|e| phase("creating shared memory", e))?;
    let offer = offer_frame(id, &name, capacity, &protocol);
    send(&mut stream, &offer, deadline, cancellation).map_err(|e| phase("sending Offer", e))?;
    let ready = receive(&mut stream, deadline, cancellation).map_err(|e| phase("waiting for Ready", e))?;
    let ready_body = match expected_message(&ready, 4) {
        Ok(body) => body,
        Err(error) => {
            if ready.first() != Some(&2) {
                reject(&mut stream, &error, deadline);
            }
            return Err(error);
        }
    };
    if ready_body != id.to_be_bytes() {
        let error = invalid("Ready session identifier mismatch");
        reject(&mut stream, &error, deadline);
        return Err(error);
    }
    shared
        .unlink_name()
        .map_err(|e| phase("unlinking offered shared memory", e))?;
    let mut start = vec![5];
    start.extend_from_slice(&id.to_be_bytes());
    send(&mut stream, &start, deadline, cancellation).map_err(|e| phase("sending Start", e))?;
    drop(stream);
    drop(_listener);
    drop(_endpoint);
    Ok(Consumer { id, shared, protocol })
}

/// Connect once to the consumer and initialize a producer queue session.
pub(crate) fn connect(config: ProducerConfig, protocol: ProtocolDescriptor) -> io::Result<Producer> {
    connect_inner(config, protocol, None)
}

pub(crate) fn connect_with_cancel(
    config: ProducerConfig, protocol: ProtocolDescriptor, cancellation: &CancellationToken,
) -> io::Result<Producer> {
    connect_inner(config, protocol, Some(cancellation))
}

fn connect_inner(
    config: ProducerConfig, protocol: ProtocolDescriptor, cancellation: Option<&CancellationToken>,
) -> io::Result<Producer> {
    check_cancel(cancellation)?;
    let deadline = config.validate()?;
    protocol.validate()?;
    check_os_version()?;
    let mut stream = match &config.endpoint {
        SetupEndpoint::Unix(path) => {
            connect_until(path, deadline, cancellation).map_err(|e| phase("connecting to setup socket", e))?
        }
        SetupEndpoint::Tcp(address) => {
            ensure_loopback(*address)?;
            connect_tcp_until(*address, deadline, cancellation)
                .map_err(|e| phase("connecting to TCP setup endpoint", e))?
        }
    };
    let mut hello = vec![1];
    hello.extend_from_slice(&contract(1, &protocol));
    send(&mut stream, &hello, deadline, cancellation).map_err(|e| phase("sending Hello", e))?;
    let offer = receive(&mut stream, deadline, cancellation).map_err(|e| phase("waiting for Offer", e))?;
    let body = match expected_message(&offer, 3) {
        Ok(body) => body,
        Err(error) => {
            if offer.first() != Some(&2) {
                reject(&mut stream, &error, deadline);
            }
            return Err(error);
        }
    };
    let parsed = (|| -> io::Result<(u64, Shared)> {
        let contract_len = contract(2, &protocol).len();
        if body.len() < contract_len + 8 + 16 + 1 {
            return Err(invalid("Offer is truncated"));
        }
        check_contract(&body[..contract_len], 2, &protocol)?;
        let tail = &body[contract_len..];
        let id = u64::from_be_bytes(tail[..8].try_into().unwrap());
        let bounds = &tail[8..24];
        let capacity = u32::from_be_bytes(bounds[8..12].try_into().unwrap()) as usize;
        validate_capacity(capacity)?;
        let mut expected_bounds = Vec::new();
        for n in [
            (RING_OFFSET + capacity) as u32,
            RING_OFFSET as u32,
            capacity as u32,
            RECORD_HEADER_SIZE,
        ] {
            expected_bounds.extend_from_slice(&n.to_be_bytes());
        }
        if bounds != expected_bounds {
            return Err(invalid("Offer resource bounds mismatch"));
        }
        let name_len = tail[24] as usize;
        if tail.len() != 25 + name_len {
            return Err(invalid("Offer shared-memory name length mismatch"));
        }
        let name = simdutf8::basic::from_utf8(&tail[25..]).map_err(|_| invalid("Offer name is not UTF-8"))?;
        let shared = Shared::open(name, id, capacity, protocol.version)
            .map_err(|e| phase("opening offered shared memory", e))?;
        Ok((id, shared))
    })();
    let (id, shared) = match parsed {
        Ok(value) => value,
        Err(error) => {
            reject(&mut stream, &error, deadline);
            return Err(error);
        }
    };
    let mut ready = vec![4];
    ready.extend_from_slice(&id.to_be_bytes());
    send(&mut stream, &ready, deadline, cancellation).map_err(|e| phase("sending Ready", e))?;
    let start = receive(&mut stream, deadline, cancellation).map_err(|e| phase("waiting for Start", e))?;
    let start_body = match expected_message(&start, 5) {
        Ok(body) => body,
        Err(error) => {
            if start.first() != Some(&2) {
                reject(&mut stream, &error, deadline);
            }
            return Err(error);
        }
    };
    if start_body != id.to_be_bytes() {
        let error = invalid("Start session identifier mismatch");
        reject(&mut stream, &error, deadline);
        return Err(error);
    }
    drop(stream);
    Ok(Producer { id, shared, protocol })
}

fn connect_until(path: &Path, deadline: Instant, cancellation: Option<&CancellationToken>) -> io::Result<SetupStream> {
    check_cancel(cancellation)?;
    let path_bytes = path.as_os_str().as_encoded_bytes();
    if path_bytes.len() >= 104 || path_bytes.contains(&0) {
        return Err(invalid("invalid or too-long socket path"));
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        #[cfg(target_os = "macos")]
        {
            address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
        }
        for (slot, byte) in address.sun_path.iter_mut().zip(path_bytes) {
            *slot = *byte as libc::c_char;
        }
        let status = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if status != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) && error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error);
            }
            loop {
                check_cancel(cancellation)?;
                let remaining = deadline_remaining(deadline)?;
                let remaining = if cancellation.is_some() {
                    remaining.min(CANCEL_CHECK_INTERVAL)
                } else {
                    remaining
                };
                let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let count = unsafe { libc::poll(&mut pollfd, 1, millis) };
                if count == 0 {
                    continue;
                }
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                let mut socket_error: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut socket_error as *mut libc::c_int).cast(),
                        &mut len,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if socket_error != 0 {
                    return Err(io::Error::from_raw_os_error(socket_error));
                }
                break;
            }
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if let Err(error) = result {
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    Ok(SetupStream::Unix(stream))
}

fn ensure_loopback(address: std::net::SocketAddr) -> io::Result<()> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(invalid("TCP setup address must use a loopback IP"))
    }
}

fn accept_until(
    listener: &UnixListener, deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<SetupStream> {
    loop {
        check_cancel(cancellation)?;
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true)?;
                return Ok(SetupStream::Unix(stream));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                deadline_remaining(deadline)?;
                sleep_until(Duration::from_millis(10), cancellation)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn accept_tcp_until(
    listener: &TcpListener, deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<SetupStream> {
    loop {
        check_cancel(cancellation)?;
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "TCP setup connection did not come from loopback",
                    ));
                }
                stream.set_nonblocking(true)?;
                return Ok(SetupStream::Tcp(stream));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                deadline_remaining(deadline)?;
                sleep_until(Duration::from_millis(10), cancellation)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn connect_tcp_until(
    address: std::net::SocketAddr, deadline: Instant, cancellation: Option<&CancellationToken>,
) -> io::Result<SetupStream> {
    loop {
        check_cancel(cancellation)?;
        let timeout = deadline_remaining(deadline)?;
        let timeout = if cancellation.is_some() {
            timeout.min(CANCEL_CHECK_INTERVAL)
        } else {
            timeout
        };
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => {
                stream.set_nonblocking(true)?;
                return Ok(SetupStream::Tcp(stream));
            }
            Err(error)
                if error.kind() == io::ErrorKind::ConnectionRefused
                    || (cancellation.is_some() && error.kind() == io::ErrorKind::TimedOut) =>
            {
                deadline_remaining(deadline)?;
                sleep_until(Duration::from_millis(10), cancellation)?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    const TEST_PROTOCOL: ProtocolDescriptor = ProtocolDescriptor {
        id: *b"CORE0001",
        version: 2,
        message_types: &[1, 2],
    };

    #[test]
    fn concurrent_sessions_get_distinct_identifiers() {
        let workers: Vec<_> = (0..32).map(|_| thread::spawn(next_session_id)).collect();
        let mut ids: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 32);
    }

    fn unused_loopback_address() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    #[test]
    fn cancellation_stops_waiting_for_a_peer() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let address = unused_loopback_address();
        let worker = thread::spawn(move || {
            open_with_cancel(ConsumerConfig::tcp(address), TEST_PROTOCOL, &worker_token)
                .err()
                .unwrap()
        });
        thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        token.cancel().unwrap();
        assert_eq!(worker.join().unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_stops_connect_retries() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let address = unused_loopback_address();
        let worker = thread::spawn(move || {
            connect_with_cancel(ProducerConfig::tcp(address), TEST_PROTOCOL, &worker_token)
                .err()
                .unwrap()
        });
        thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        token.cancel().unwrap();
        assert_eq!(worker.join().unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_stops_a_partial_hello() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let address = unused_loopback_address();
        let worker = thread::spawn(move || {
            open_with_cancel(ConsumerConfig::tcp(address), TEST_PROTOCOL, &worker_token)
                .err()
                .unwrap()
        });
        let connect_deadline = Instant::now() + Duration::from_secs(2);
        let mut connector = loop {
            match TcpStream::connect(address) {
                Ok(stream) => break stream,
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    assert!(Instant::now() < connect_deadline, "consumer did not bind");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("unexpected connect error: {error}"),
            }
        };
        connector.write_all(&[0]).unwrap();
        thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        token.cancel().unwrap();
        assert_eq!(worker.join().unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn mismatched_protocol_is_rejected_during_setup() {
        let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = PathBuf::from(format!("/tmp/mc-mismatch-{}-{id:x}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        let socket = directory.join("setup.sock");
        let consumer_socket = socket.clone();
        let listener = thread::spawn(move || open(ConsumerConfig::new(consumer_socket), TEST_PROTOCOL).err().unwrap());
        let bind_deadline = Instant::now() + Duration::from_secs(2);
        while !socket.exists() {
            assert!(Instant::now() < bind_deadline, "consumer did not bind");
            thread::sleep(Duration::from_millis(1));
        }
        let other = ProtocolDescriptor {
            id: *b"OTHER001",
            version: 2,
            message_types: &[1],
        };
        let producer_error = connect(ProducerConfig::new(socket.clone()), other).err().unwrap();
        let consumer_error = listener.join().unwrap();
        assert!(producer_error.to_string().contains("mismatch"));
        assert!(consumer_error.to_string().contains("mismatch"));
        assert!(!socket.exists());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn configured_listener_timeout_is_honored() {
        let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = PathBuf::from(format!("/tmp/mc-timeout-{}-{id:x}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        let socket = directory.join("setup.sock");
        let mut config = ConsumerConfig::new(socket.clone());
        config.setup_timeout = Duration::from_millis(40);
        let start = Instant::now();
        let error = open(config, TEST_PROTOCOL).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!socket.exists());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn loopback_tcp_setup_closes_listener_and_transfers_records() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let consumer = thread::spawn(move || {
            let mut consumer = open(ConsumerConfig::tcp(address), TEST_PROTOCOL).unwrap();
            let session_id = consumer.session_id();
            (session_id, consumer.receive().unwrap())
        });
        // The TCP connector retries refused connections until the setup deadline, so
        // independently started peers can race during listener startup.
        let mut producer = connect(ProducerConfig::tcp(address), TEST_PROTOCOL).unwrap();
        let payload = b"tcp setup uses the same shared queue";
        let sent = producer.send_batch(&[Record { kind: 1, payload }]).unwrap();
        assert_eq!(sent.accepted, 1);
        let (consumer_id, received) = consumer.join().unwrap();
        assert_eq!(producer.session_id(), consumer_id);
        assert_eq!(received, (1, payload.to_vec()));
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err());
    }

    #[test]
    fn tcp_setup_rejects_non_loopback_addresses() {
        let address = "192.0.2.1:5101".parse().unwrap();
        let error = open(ConsumerConfig::tcp(address), TEST_PROTOCOL).err().unwrap();
        assert!(error.to_string().contains("loopback"));
    }

    #[test]
    fn eof_before_start_is_setup_failure() {
        let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        let directory = PathBuf::from(format!("/tmp/mc-eof-{}-{id:x}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        let socket = directory.join("setup.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            let mut stream = SetupStream::Unix(stream);
            let deadline = Instant::now() + crate::DEFAULT_SETUP_TIMEOUT;
            let hello = receive(&mut stream, deadline, None).unwrap();
            check_contract(expected_message(&hello, 1).unwrap(), 1, &TEST_PROTOCOL).unwrap();
            let (_shared, name) = Shared::create(id, crate::DEFAULT_RING_CAPACITY, TEST_PROTOCOL.version).unwrap();
            send(
                &mut stream,
                &offer_frame(id, &name, crate::DEFAULT_RING_CAPACITY, &TEST_PROTOCOL),
                deadline,
                None,
            )
            .unwrap();
            let ready = receive(&mut stream, deadline, None).unwrap();
            assert_eq!(expected_message(&ready, 4).unwrap(), id.to_be_bytes());
            // Closing here must never be interpreted as Start.
        });
        let result = connect(ProducerConfig::new(socket.clone()), TEST_PROTOCOL);
        server.join().unwrap();
        fs::remove_file(&socket).unwrap();
        fs::remove_dir(&directory).unwrap();
        assert!(result.is_err_and(|error| {
            error.kind() == io::ErrorKind::UnexpectedEof && error.to_string().contains("waiting for Start")
        }));
    }
}
