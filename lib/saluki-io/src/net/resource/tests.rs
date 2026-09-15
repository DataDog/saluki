use std::{
    net::{SocketAddr, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket},
    num::NonZeroUsize,
    time::Duration,
};

use bytes::BytesMut;
use saluki_core::{
    runtime::state::{AcquireError, ResourceRegistry},
    support::SubsystemIdentifier,
};
use tokio::{net::UdpSocket, time::timeout};

use super::*;
use crate::net::Stream;

fn owner(name: &str) -> SubsystemIdentifier {
    SubsystemIdentifier::from_segments(["test", name])
}

/// Reserves an ephemeral UDP port and releases it, yielding an address the test can bind explicitly.
///
/// Binding to port 0 would work, but then the effective port is only discoverable through the socket itself, and these
/// tests need to send to a known address from the outside.
fn reserved_udp_addr() -> SocketAddr {
    let socket = StdUdpSocket::bind("127.0.0.1:0").expect("should bind an ephemeral UDP port");
    socket.local_addr().expect("should have a local address")
}

fn reserved_tcp_addr() -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral TCP port");
    listener.local_addr().expect("should have a local address")
}

/// Sends `payload` to `addr` and reads it back off `stream`, proving the socket behind the stream is really bound.
async fn round_trip(stream: &mut Stream, addr: SocketAddr, payload: &[u8]) {
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client should bind");
    client.send_to(payload, addr).await.expect("client should send");

    let mut buf = BytesMut::with_capacity(64);
    let (n, _) = timeout(Duration::from_secs(5), stream.receive(&mut buf))
        .await
        .expect("receive should not time out")
        .expect("receive should succeed");

    assert_eq!(&buf[..n], payload);
}

#[tokio::test]
async fn connection_oriented_listener_keeps_its_bound_port_across_leases() {
    let registry = ResourceRegistry::new();
    let addr = reserved_tcp_addr();
    let spec = ConnectionOrientedSocketSpecification::new(ListenAddress::Tcp(addr));

    let listener = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let bound = listener.bound_listen_address();
    drop(listener);

    // The registry still holds the socket, so this is the same bound port rather than a fresh bind that happened to
    // succeed. That is the property the whole registry exists for.
    let listener = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(listener.bound_listen_address(), bound);
}

#[tokio::test]
async fn udp_listener_yields_a_working_stream_again_after_a_release() {
    let registry = ResourceRegistry::new();
    let addr = reserved_udp_addr();
    let spec = SocketSpecification::new(ListenAddress::Udp(addr));

    {
        let mut listener = registry
            .acquire(&owner("first"), spec.clone())
            .await
            .expect("should acquire");
        let mut stream = listener.accept().await.expect("should yield a stream");
        round_trip(&mut stream, addr, b"before").await;
    }

    // Both the stream and the listener are gone. For a connectionless family the bound socket *is* the stream, so this
    // is the case that would silently break if the socket were moved out of the listener rather than shared: the
    // re-acquired listener would look exhausted and yield nothing.
    let mut listener = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    let mut stream = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept should not hang after a release")
        .expect("should yield a stream");
    round_trip(&mut stream, addr, b"after").await;
}

#[tokio::test]
async fn udp_listener_yields_one_stream_per_bound_socket() {
    let registry = ResourceRegistry::new();
    let addr = reserved_udp_addr();
    let spec = SocketSpecification::new(ListenAddress::Udp(addr))
        .with_udp_streams(Some(NonZeroUsize::new(4).expect("not zero")));

    let mut listener = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");

    // How many sockets actually got bound is platform-dependent: SO_REUSEPORT load balancing only exists on Linux, and
    // elsewhere the request is downgraded to a single socket. Ask the listener rather than assuming.
    let expected = listener.min_buffer_reservation();

    let mut streams = Vec::new();
    for _ in 0..expected {
        streams.push(listener.accept().await.expect("should yield a stream"));
    }

    // Every socket is in use, so the listener goes quiet rather than yielding a duplicate.
    assert!(
        timeout(Duration::from_millis(100), listener.accept()).await.is_err(),
        "listener should have nothing left to yield"
    );

    drop(streams);
    drop(listener);

    // And the whole supply is available again to the next holder.
    let mut listener = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(listener.min_buffer_reservation(), expected);
    for _ in 0..expected {
        timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("accept should not hang after a release")
            .expect("should yield a stream");
    }
}

#[tokio::test]
async fn a_stream_outliving_its_lease_holds_the_listener() {
    // For a connectionless family the stream *is* the listener's bound socket, so handing the listener to the next
    // acquirer while a stream is still alive would leave both reading the same socket, splitting incoming datagrams
    // between them. The sublease the stream carries is what prevents that.
    let registry = ResourceRegistry::new();
    let addr = reserved_udp_addr();
    let spec = SocketSpecification::new(ListenAddress::Udp(addr));

    let mut listener = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let mut stream = listener.accept().await.expect("should yield a stream");
    let bound = listener.bound_listen_address();

    // Release the head lease but keep the stream. The listener is back with the registry, and still not available.
    drop(listener);

    assert!(
        timeout(
            Duration::from_millis(100),
            registry.acquire(&owner("second"), spec.clone())
        )
        .await
        .is_err(),
        "the listener should not be handed over while a stream is still reading its socket"
    );

    // And the stream keeps working throughout, which is why waiting is the right answer here rather than revoking it
    // out from under whoever is still using it.
    round_trip(&mut stream, addr, b"still mine").await;
    drop(stream);

    let listener = timeout(Duration::from_secs(5), registry.acquire(&owner("second"), spec))
        .await
        .expect("acquisition should complete once the stream is dropped")
        .expect("should reacquire");
    assert_eq!(listener.bound_listen_address(), bound);
}

#[tokio::test]
async fn one_address_cannot_be_held_as_two_listener_types() {
    let registry = ResourceRegistry::new();
    let address = ListenAddress::Tcp(reserved_tcp_addr());

    let _held = registry
        .acquire(&owner("first"), SocketSpecification::new(address.clone()))
        .await
        .expect("should acquire");

    // One bound socket can't be reinterpreted as a different listener type, and both specifications name the same
    // address under the same kind, so the collision is caught rather than double-binding.
    let result = registry
        .acquire(&owner("second"), ConnectionOrientedSocketSpecification::new(address))
        .await;
    let Err(err) = result else {
        panic!("the same address as a different listener type should be refused");
    };
    assert!(
        matches!(err, AcquireError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[tokio::test]
async fn a_leased_address_is_refused_to_a_second_acquirer() {
    let registry = ResourceRegistry::new();
    let spec = SocketSpecification::new(ListenAddress::Tcp(reserved_tcp_addr()));

    let _held = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");

    let Err(err) = registry.acquire(&owner("second"), spec).await else {
        panic!("should be refused while leased");
    };
    assert!(
        matches!(err, AcquireError::AlreadyLeased { .. }),
        "expected AlreadyLeased, got {err:?}"
    );
}

#[tokio::test]
async fn an_address_held_outside_the_process_fails_to_bind() {
    let registry = ResourceRegistry::new();
    let addr = reserved_tcp_addr();

    // Something else owns the address. This is the conflict the registry can't arbitrate, so it has to surface as a
    // creation failure rather than as an in-process lease conflict.
    let _external = StdTcpListener::bind(addr).expect("test should hold the address");

    let result = registry
        .acquire(&owner("first"), SocketSpecification::new(ListenAddress::Tcp(addr)))
        .await;
    let Err(err) = result else {
        panic!("binding an externally held address should fail");
    };
    assert!(
        matches!(err, AcquireError::CreationFailed { .. }),
        "expected CreationFailed, got {err:?}"
    );

    // The failed attempt must not leave the key claimed, or the address would stay unusable even once it frees up.
    assert!(registry.snapshot().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn unixgram_listener_yields_a_working_stream_again_after_a_release() {
    use tokio::net::UnixDatagram;

    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path().join("dsd.sock");
    let spec = SocketSpecification::new(ListenAddress::Unixgram(path.clone()));
    let registry = ResourceRegistry::new();

    async fn send(path: &std::path::Path, payload: &[u8]) {
        let client = UnixDatagram::unbound().expect("client should be created");
        client.send_to(payload, path).await.expect("client should send");
    }

    async fn recv(stream: &mut Stream, expected: &[u8]) {
        let mut buf = BytesMut::with_capacity(64);
        let (n, _) = timeout(Duration::from_secs(5), stream.receive(&mut buf))
            .await
            .expect("receive should not time out")
            .expect("receive should succeed");
        assert_eq!(&buf[..n], expected);
    }

    {
        let mut listener = registry
            .acquire(&owner("first"), spec.clone())
            .await
            .expect("should acquire");
        let mut stream = listener.accept().await.expect("should yield a stream");
        send(&path, b"before").await;
        recv(&mut stream, b"before").await;
    }

    let mut listener = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    let mut stream = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept should not hang after a release")
        .expect("should yield a stream");
    send(&path, b"after").await;
    recv(&mut stream, b"after").await;
}
