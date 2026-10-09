//! Broadcast session lifecycle.
//!
//! Unlike SPSC setup, the publisher keeps its listener and shared-memory name
//! for the whole session so subscribers can join at any time. Each join or
//! graceful leave uses one short-lived control connection. The publisher's
//! application thread only writes the ring; a library-owned control worker
//! performs the handshakes and slot bookkeeping.

use crate::broadcast::{BroadcastInner, SharedInner, Subscription};
use crate::cancellation::CancellationToken;
use crate::config::{BroadcastPublisherConfig, SetupEndpoint, SubscriberConfig};
use crate::contract::{
    broadcast_contract, check_broadcast_contract, BROADCAST_SLOTS_OFFSET, BROADCAST_SLOT_STRIDE, RECORD_HEADER_SIZE,
};
use crate::mapping::Shared;
use crate::setup::{
    check_cancel, check_os_version, connect_tcp_until, connect_until, endpoint_parent, ensure_loopback,
    expected_message, phase, receive, reject, send, SetupStream, UnixEndpoint,
};
use crate::{invalid, ProtocolDescriptor};
use std::fs::{self, Permissions};
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Broadcast control frame tags. They are deliberately separate from the SPSC
// handshake bytes; only the framing helper is shared.
const JOIN: u8 = 1;
const REJECT: u8 = 2;
const OFFER: u8 = 3;
const READY: u8 = 4;
const START: u8 = 5;
const UNSUBSCRIBE: u8 = 6;
const UNSUBSCRIBED: u8 = 7;

const MAX_PENDING_HANDSHAKES: usize = 8;
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Live publisher handle. Dropping it stops the control worker, unlinks the
/// shared-memory name, and removes the owned endpoint.
pub struct BroadcastPublisher {
    inner: SharedInner,
    control: Option<JoinHandle<()>>,
}

impl BroadcastPublisher {
    /// Creates the shared mapping, binds the long-lived listener, and starts
    /// the control worker. It returns without waiting for a subscriber.
    pub fn open(config: BroadcastPublisherConfig, protocol: ProtocolDescriptor) -> io::Result<Self> {
        open_broadcast_publisher(config, protocol)
    }

    pub fn session_id(&self) -> u64 {
        self.inner.session
    }

    /// Diagnostic active-subscriber count. Do not use it as proof that
    /// publication is safe; subscribers may leave at any moment.
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }

    /// Publishes the fitting prefix, blocking for capacity and for a first
    /// subscriber. Cancellation is not available on this variant.
    pub fn send_batch(&mut self, records: &[crate::Record<'_>]) -> crate::BroadcastOutcome {
        self.inner.send_batch(records, None)
    }

    /// Publishes with a local cancellation token. On cancellation the outcome
    /// reports exactly how many records were already published.
    pub fn send_batch_with_cancel(
        &mut self, records: &[crate::Record<'_>], cancellation: &CancellationToken,
    ) -> crate::BroadcastOutcome {
        self.inner.send_batch(records, Some(cancellation))
    }
}

impl Drop for BroadcastPublisher {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.subscribers_changed.notify_all();
        if let Some(handle) = self.control.take() {
            let _ = handle.join();
        }
    }
}

#[allow(dead_code)] // the UnixEndpoint guard removes the socket on drop
enum Listener {
    Unix(UnixListener, UnixEndpoint),
    Tcp(TcpListener),
}

impl Listener {
    fn bind(endpoint: &SetupEndpoint) -> io::Result<Self> {
        match endpoint {
            SetupEndpoint::Unix(path) => {
                endpoint_parent(path)?;
                let listener = UnixListener::bind(path).map_err(|e| phase("binding setup socket", e))?;
                let guard = UnixEndpoint(path.clone());
                fs::set_permissions(path, Permissions::from_mode(0o600))?;
                listener.set_nonblocking(true)?;
                Ok(Self::Unix(listener, guard))
            }
            SetupEndpoint::Tcp(address) => {
                ensure_loopback(*address)?;
                let listener = TcpListener::bind(address).map_err(|e| phase("binding TCP setup listener", e))?;
                listener.set_nonblocking(true)?;
                Ok(Self::Tcp(listener))
            }
        }
    }

    fn accept(&self) -> io::Result<SetupStream> {
        match self {
            Self::Unix(listener, _) => {
                let (stream, _) = listener.accept()?;
                stream.set_nonblocking(true)?;
                Ok(SetupStream::Unix(stream))
            }
            Self::Tcp(listener) => {
                let (stream, peer) = listener.accept()?;
                if !peer.ip().is_loopback() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "TCP setup connection did not come from loopback",
                    ));
                }
                stream.set_nonblocking(true)?;
                Ok(SetupStream::Tcp(stream))
            }
        }
    }
}

/// Monotonic per-process session counter. Combined with the clock it keeps the
/// shared-memory name unique even when several publishers open in the same
/// instant, which parallel tests and short-lived sessions can trigger.
static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn new_session_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    ((nanos as u32 as u64) << 32) | (counter as u32 as u64)
}

/// Creates the broadcast mapping, binds the long-lived listener, and starts the
/// control worker. It returns without waiting for any subscriber.
pub(crate) fn open_broadcast_publisher(
    config: BroadcastPublisherConfig, protocol: ProtocolDescriptor,
) -> io::Result<BroadcastPublisher> {
    let setup_timeout = config.validate()?;
    protocol.validate()?;
    check_os_version()?;
    let id = new_session_id();
    let (shared, name) = Shared::create_broadcast(id, config.ring_capacity, protocol.version, config.max_subscribers)
        .map_err(|e| phase("creating broadcast shared memory", e))?;
    let listener = Listener::bind(&config.endpoint)?;
    let inner = Arc::new(BroadcastInner {
        shared,
        protocol,
        session: id,
        capacity: config.ring_capacity,
        max_subscribers: config.max_subscribers,
        name,
        setup_timeout,
        registry: Mutex::new(crate::broadcast::Registry::new(config.max_subscribers)),
        subscribers_changed: std::sync::Condvar::new(),
        shutdown: std::sync::atomic::AtomicBool::new(false),
        handshakes: AtomicUsize::new(0),
    });
    let worker_inner = inner.clone();
    let control = thread::Builder::new()
        .name("fit-broadcast-control".into())
        .spawn(move || control_loop(worker_inner, listener))
        .map_err(|e| phase("starting broadcast control worker", e))?;
    Ok(BroadcastPublisher {
        inner,
        control: Some(control),
    })
}

fn control_loop(inner: SharedInner, listener: Listener) {
    while !inner.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok(stream) => {
                if inner.handshakes.load(Ordering::SeqCst) >= MAX_PENDING_HANDSHAKES {
                    // Bounded pending handshakes: drop the extra connection.
                    continue;
                }
                inner.handshakes.fetch_add(1, Ordering::SeqCst);
                let worker = inner.clone();
                let spawned = thread::Builder::new()
                    .name("fit-broadcast-handshake".into())
                    .spawn(move || {
                        let deadline = Instant::now() + worker.setup_timeout;
                        let _ = handle_control(worker.clone(), stream, deadline);
                        worker.handshakes.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    inner.handshakes.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
}

fn handle_control(inner: SharedInner, mut stream: SetupStream, deadline: Instant) -> io::Result<()> {
    let frame = receive(&mut stream, deadline, None)?;
    match frame.first() {
        Some(&JOIN) => handle_join(&inner, &mut stream, &frame, deadline),
        Some(&UNSUBSCRIBE) => handle_unsubscribe(&inner, &mut stream, &frame, deadline),
        _ => {
            let error = invalid("unexpected broadcast control message");
            reject(&mut stream, &error, deadline);
            Err(error)
        }
    }
}

fn handle_join(inner: &SharedInner, stream: &mut SetupStream, hello: &[u8], deadline: Instant) -> io::Result<()> {
    if let Err(error) =
        expected_message(hello, JOIN).and_then(|body| check_broadcast_contract(body, 1, &inner.protocol))
    {
        reject(stream, &error, deadline);
        return Err(error);
    }
    let (slot, generation) = {
        let mut registry = inner.registry.lock().unwrap();
        match inner.reserve_slot(&mut registry) {
            Ok(value) => value,
            Err(error) => {
                reject(stream, &error, deadline);
                return Err(error);
            }
        }
    };
    let offer = offer_frame(inner, slot, generation);
    if let Err(error) = send(stream, &offer, deadline, None) {
        let mut registry = inner.registry.lock().unwrap();
        inner.release_reservation(&mut registry, slot);
        return Err(error);
    }
    let ready = match receive(stream, deadline, None) {
        Ok(frame) => frame,
        Err(error) => {
            let mut registry = inner.registry.lock().unwrap();
            inner.release_reservation(&mut registry, slot);
            return Err(error);
        }
    };
    if let Err(error) = check_ready(inner, slot, generation, &ready) {
        let mut registry = inner.registry.lock().unwrap();
        inner.release_reservation(&mut registry, slot);
        reject(stream, &error, deadline);
        return Err(error);
    }
    let activation = {
        let mut registry = inner.registry.lock().unwrap();
        match inner.activate_slot(&mut registry, slot, generation) {
            Ok(position) => position,
            Err(error) => {
                reject(stream, &error, deadline);
                return Err(error);
            }
        }
    };
    let start = start_frame(inner.session, slot, generation, activation);
    // Activation already happened, so the subscription may be live. Never
    // reclaim a possibly active reader; retain it conservatively.
    send(stream, &start, deadline, None)?;
    Ok(())
}

fn handle_unsubscribe(
    inner: &SharedInner, stream: &mut SetupStream, frame: &[u8], deadline: Instant,
) -> io::Result<()> {
    let body = match expected_message(frame, UNSUBSCRIBE) {
        Ok(body) => body,
        Err(error) => {
            reject(stream, &error, deadline);
            return Err(error);
        }
    };
    if body.len() != 29 + 16 {
        let error = invalid("Unsubscribe frame length mismatch");
        reject(stream, &error, deadline);
        return Err(error);
    }
    if let Err(error) = check_broadcast_contract(&body[..29], 1, &inner.protocol) {
        reject(stream, &error, deadline);
        return Err(error);
    }
    let session = u64::from_be_bytes(body[29..37].try_into().unwrap());
    let slot = u32::from_be_bytes(body[37..41].try_into().unwrap());
    let generation = u32::from_be_bytes(body[41..45].try_into().unwrap());
    if session != inner.session {
        let error = invalid("Unsubscribe session identifier mismatch");
        reject(stream, &error, deadline);
        return Err(error);
    }
    let result = {
        let mut registry = inner.registry.lock().unwrap();
        inner.retire_slot(&mut registry, slot, generation)
    };
    if let Err(error) = result {
        reject(stream, &error, deadline);
        return Err(error);
    }
    let ack = unsubscribed_frame(inner.session, slot, generation);
    send(stream, &ack, deadline, None)
}

fn check_ready(inner: &SharedInner, slot: u32, generation: u32, frame: &[u8]) -> io::Result<()> {
    let body = expected_message(frame, READY)?;
    if body.len() != 16 {
        return Err(invalid("Ready frame length mismatch"));
    }
    let session = u64::from_be_bytes(body[..8].try_into().unwrap());
    let ready_slot = u32::from_be_bytes(body[8..12].try_into().unwrap());
    let ready_generation = u32::from_be_bytes(body[12..16].try_into().unwrap());
    if session != inner.session || ready_slot != slot || ready_generation != generation {
        return Err(invalid("Ready does not match the offered subscription"));
    }
    Ok(())
}

fn offer_frame(inner: &BroadcastInner, slot: u32, generation: u32) -> Vec<u8> {
    let region_size = inner.shared.region_len;
    let mut offer = vec![OFFER];
    offer.extend_from_slice(&broadcast_contract(2, &inner.protocol));
    offer.extend_from_slice(&inner.session.to_be_bytes());
    for value in [
        region_size as u32,
        inner.shared.ring_offset as u32,
        inner.capacity as u32,
        RECORD_HEADER_SIZE,
        BROADCAST_SLOT_STRIDE as u32,
        inner.max_subscribers as u32,
        BROADCAST_SLOTS_OFFSET as u32,
        slot,
        generation,
    ] {
        offer.extend_from_slice(&value.to_be_bytes());
    }
    offer.push(inner.name.len() as u8);
    offer.extend_from_slice(inner.name.as_bytes());
    offer
}

fn start_frame(session: u64, slot: u32, generation: u32, activation: usize) -> Vec<u8> {
    let mut start = vec![START];
    start.extend_from_slice(&session.to_be_bytes());
    start.extend_from_slice(&slot.to_be_bytes());
    start.extend_from_slice(&generation.to_be_bytes());
    start.extend_from_slice(&(activation as u32).to_be_bytes());
    start
}

fn unsubscribed_frame(session: u64, slot: u32, generation: u32) -> Vec<u8> {
    let mut ack = vec![UNSUBSCRIBED];
    ack.extend_from_slice(&session.to_be_bytes());
    ack.extend_from_slice(&slot.to_be_bytes());
    ack.extend_from_slice(&generation.to_be_bytes());
    ack
}

/// Subscribes to a running publisher. The subscriber maps the existing mapping
/// and starts at its activation boundary, so it never receives history.
pub(crate) fn subscribe(config: SubscriberConfig, protocol: ProtocolDescriptor) -> io::Result<Subscription> {
    subscribe_inner(config, protocol, None)
}

pub(crate) fn subscribe_with_cancel(
    config: SubscriberConfig, protocol: ProtocolDescriptor, cancellation: &CancellationToken,
) -> io::Result<Subscription> {
    subscribe_inner(config, protocol, Some(cancellation))
}

fn subscribe_inner(
    config: SubscriberConfig, protocol: ProtocolDescriptor, cancellation: Option<&CancellationToken>,
) -> io::Result<Subscription> {
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
    let mut hello = vec![JOIN];
    hello.extend_from_slice(&broadcast_contract(1, &protocol));
    send(&mut stream, &hello, deadline, cancellation).map_err(|e| phase("sending Hello", e))?;
    let offer = receive(&mut stream, deadline, cancellation).map_err(|e| phase("waiting for Offer", e))?;
    let body = match expected_message(&offer, OFFER) {
        Ok(body) => body,
        Err(error) => {
            if offer.first() != Some(&REJECT) {
                reject(&mut stream, &error, deadline);
            }
            return Err(error);
        }
    };
    let parsed = parse_offer(body, &protocol);
    let (session, shared, slot, generation) = match parsed {
        Ok(value) => value,
        Err(error) => {
            reject(&mut stream, &error, deadline);
            return Err(error);
        }
    };
    let mut ready = vec![READY];
    ready.extend_from_slice(&session.to_be_bytes());
    ready.extend_from_slice(&slot.to_be_bytes());
    ready.extend_from_slice(&generation.to_be_bytes());
    send(&mut stream, &ready, deadline, cancellation).map_err(|e| phase("sending Ready", e))?;
    let start = receive(&mut stream, deadline, cancellation).map_err(|e| phase("waiting for Start", e))?;
    let start_body = match expected_message(&start, START) {
        Ok(body) => body,
        Err(error) => {
            if start.first() != Some(&REJECT) {
                reject(&mut stream, &error, deadline);
            }
            return Err(error);
        }
    };
    if start_body.len() != 20 {
        let error = invalid("Start frame length mismatch");
        reject(&mut stream, &error, deadline);
        return Err(error);
    }
    let start_session = u64::from_be_bytes(start_body[..8].try_into().unwrap());
    let start_slot = u32::from_be_bytes(start_body[8..12].try_into().unwrap());
    let start_generation = u32::from_be_bytes(start_body[12..16].try_into().unwrap());
    if start_session != session || start_slot != slot || start_generation != generation {
        let error = invalid("Start does not match the offered subscription");
        reject(&mut stream, &error, deadline);
        return Err(error);
    }
    drop(stream);
    let capacity = shared.capacity;
    let mut subscription = Subscription::new(shared, protocol, session, slot, generation, capacity);
    subscription.set_control(config.endpoint.clone(), config.setup_timeout);
    Ok(subscription)
}

fn parse_offer(body: &[u8], protocol: &ProtocolDescriptor) -> io::Result<(u64, Shared, u32, u32)> {
    let contract_len = broadcast_contract(2, protocol).len();
    if body.len() < contract_len + 8 + 4 * 9 + 1 {
        return Err(invalid("Offer is truncated"));
    }
    check_broadcast_contract(&body[..contract_len], 2, protocol)?;
    let tail = &body[contract_len..];
    let session = u64::from_be_bytes(tail[..8].try_into().unwrap());
    let mut index = 8;
    let region_size = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let ring_offset = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let capacity = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let _record_header = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let slot_stride = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let max_subscribers = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let slots_offset = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let slot = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    let generation = u32::from_be_bytes(tail[index..index + 4].try_into().unwrap());
    index += 4;
    if slot_stride as usize != BROADCAST_SLOT_STRIDE || slots_offset as usize != BROADCAST_SLOTS_OFFSET {
        return Err(invalid("Offer broadcast layout mismatch"));
    }
    let name_len = tail[index] as usize;
    index += 1;
    if tail.len() != index + name_len {
        return Err(invalid("Offer shared-memory name length mismatch"));
    }
    let name = simdutf8::basic::from_utf8(&tail[index..]).map_err(|_| invalid("Offer name is not UTF-8"))?;
    let shared = Shared::open_broadcast(
        name,
        session,
        capacity as usize,
        protocol.version,
        max_subscribers as usize,
        region_size as usize,
        ring_offset as usize,
    )
    .map_err(|e| phase("opening offered shared memory", e))?;
    Ok((session, shared, slot, generation))
}

/// Sends a graceful unsubscribe and waits for the publisher's acknowledgment.
pub(crate) fn unsubscribe(subscription: &mut Subscription) -> io::Result<()> {
    if subscription.unsubscribed() {
        return Ok(());
    }
    let Some(endpoint) = subscription.control_endpoint() else {
        return Err(invalid("subscription has no control endpoint"));
    };
    let mut config = SubscriberConfig::for_endpoint(endpoint);
    config.setup_timeout = subscription.control_timeout();
    let deadline = config.validate()?;
    let mut stream = match &config.endpoint {
        SetupEndpoint::Unix(path) => {
            connect_until(path, deadline, None).map_err(|e| phase("connecting to setup socket", e))?
        }
        SetupEndpoint::Tcp(address) => {
            ensure_loopback(*address)?;
            connect_tcp_until(*address, deadline, None).map_err(|e| phase("connecting to TCP setup endpoint", e))?
        }
    };
    let mut frame = vec![UNSUBSCRIBE];
    frame.extend_from_slice(&broadcast_contract(1, &subscription.protocol_descriptor()));
    frame.extend_from_slice(&subscription.session_id().to_be_bytes());
    frame.extend_from_slice(&subscription.slot_id().to_be_bytes());
    frame.extend_from_slice(&subscription.generation().to_be_bytes());
    send(&mut stream, &frame, deadline, None).map_err(|e| phase("sending Unsubscribe", e))?;
    let ack = receive(&mut stream, deadline, None).map_err(|e| phase("waiting for Unsubscribed", e))?;
    let body = expected_message(&ack, UNSUBSCRIBED)?;
    if body.len() != 16 {
        return Err(invalid("Unsubscribed frame length mismatch"));
    }
    let session = u64::from_be_bytes(body[..8].try_into().unwrap());
    let slot = u32::from_be_bytes(body[8..12].try_into().unwrap());
    let generation = u32::from_be_bytes(body[12..16].try_into().unwrap());
    if session != subscription.session_id() || slot != subscription.slot_id() || generation != subscription.generation()
    {
        return Err(invalid("Unsubscribed does not match the subscription"));
    }
    subscription.mark_unsubscribed();
    Ok(())
}

/// Best-effort unsubscribe used by `Subscription::drop`. Failures leave the
/// subscription conservatively pinned; the PoC accepts that a crashed or
/// misbehaving subscriber can stall the ring.
pub(crate) fn unsubscribe_best_effort(subscription: &mut Subscription) {
    let _ = unsubscribe(subscription);
}

impl Subscription {
    /// Subscribes to a running publisher.
    pub fn subscribe(config: SubscriberConfig, protocol: ProtocolDescriptor) -> io::Result<Self> {
        subscribe(config, protocol)
    }

    /// Subscribes while allowing the local supervisor to cancel setup.
    pub fn subscribe_with_cancel(
        config: SubscriberConfig, protocol: ProtocolDescriptor, cancellation: &CancellationToken,
    ) -> io::Result<Self> {
        subscribe_with_cancel(config, protocol, cancellation)
    }

    /// Performs a graceful unsubscribe. After this call the slot no longer
    /// pins publication. Calling it again is a successful no-op.
    pub fn unsubscribe(&mut self) -> io::Result<()> {
        unsubscribe(self)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    use std::path::{Path, PathBuf};

    const PROTOCOL: ProtocolDescriptor = ProtocolDescriptor {
        id: *b"BCAST001",
        version: 1,
        message_types: &[1, 2],
    };

    fn socket(name: &str) -> (PathBuf, PathBuf) {
        let id = crate::test_unique_id();
        let directory = PathBuf::from(format!("/tmp/fit-bc-{name}-{}-{id:x}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        (directory.clone(), directory.join("setup.sock"))
    }

    fn publisher(socket: &Path) -> BroadcastPublisher {
        let mut config = BroadcastPublisherConfig::new(socket.to_path_buf());
        config.ring_capacity = 4096;
        config.max_subscribers = 4;
        config.setup_timeout = Duration::from_secs(5);
        BroadcastPublisher::open(config, PROTOCOL).unwrap()
    }

    fn subscribe(socket: &Path) -> Subscription {
        let mut config = SubscriberConfig::new(socket.to_path_buf());
        config.setup_timeout = Duration::from_secs(5);
        Subscription::subscribe(config, PROTOCOL).unwrap()
    }

    fn record(kind: u32, payload: &[u8]) -> Record<'_> {
        Record { kind, payload }
    }

    #[test]
    fn late_join_and_graceful_leave_over_a_socket() {
        let (directory, socket) = socket("late-join");
        let mut publisher = publisher(&socket);
        let mut first = subscribe(&socket);
        assert_eq!(publisher.send_batch(&[record(1, b"e1")]).accepted, 1);
        assert_eq!(first.receive().unwrap().1, b"e1");

        let mut second = subscribe(&socket);
        assert_eq!(publisher.subscriber_count(), 2);
        assert_eq!(publisher.send_batch(&[record(1, b"e2")]).accepted, 1);
        assert_eq!(first.receive().unwrap().1, b"e2");
        assert_eq!(second.receive().unwrap().1, b"e2");

        second.unsubscribe().unwrap();
        assert_eq!(publisher.subscriber_count(), 1);
        assert_eq!(publisher.send_batch(&[record(1, b"e3")]).accepted, 1);
        assert_eq!(first.receive().unwrap().1, b"e3");
        first.unsubscribe().unwrap();
        drop(publisher);
        drop(first);
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn mismatched_broadcast_protocol_is_rejected() {
        let (directory, socket) = socket("mismatch");
        let publisher = publisher(&socket);
        let other = ProtocolDescriptor {
            id: *b"OTHER001",
            version: 1,
            message_types: &[1],
        };
        let mut config = SubscriberConfig::new(socket.clone());
        config.setup_timeout = Duration::from_secs(2);
        let error = Subscription::subscribe(config, other).err().unwrap();
        assert!(error.to_string().contains("mismatch"), "{error}");
        drop(publisher);
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn a_cancelled_receive_leaves_the_slot_active_for_the_next_receive() {
        let (directory, socket) = socket("cancel-recv");
        let mut publisher = publisher(&socket);
        let mut subscriber = subscribe(&socket);
        let token = CancellationToken::new();
        token.cancel().unwrap();
        assert!(subscriber.receive_with_cancel(&token).unwrap().is_none());
        assert_eq!(publisher.send_batch(&[record(1, b"after")]).accepted, 1);
        assert_eq!(subscriber.receive().unwrap().1, b"after");
        subscriber.unsubscribe().unwrap();
        drop(publisher);
        drop(subscriber);
        fs::remove_dir(directory).unwrap();
    }
}
