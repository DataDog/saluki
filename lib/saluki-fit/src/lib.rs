//! Generic FIT shared-memory IPC transports: single-producer/single-consumer rings and
//! a single-publisher broadcast ring with dynamic subscribers.
//!
//! This crate has no API on other targets. Consumers must gate their FIT use
//! to the supported platforms, so unrelated Saluki targets can still build.
#![cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_endian = "little",
    target_has_atomic = "32",
    any(target_os = "linux", target_os = "macos")
))]
mod broadcast;
mod broadcast_setup;
mod cancellation;
mod config;
mod contract;
mod mapping;
mod ring;
mod setup;
mod wait;
pub use broadcast::{BroadcastOutcome, Subscription};
pub use broadcast_setup::BroadcastPublisher;
pub use cancellation::CancellationToken;
pub use config::{
    BroadcastPublisherConfig, ConsumerConfig, ProducerConfig, SetupEndpoint, SubscriberConfig, DEFAULT_MAX_SUBSCRIBERS,
    DEFAULT_RING_CAPACITY, DEFAULT_SETUP_TIMEOUT, MAX_RING_CAPACITY, MAX_SUBSCRIBERS,
};
pub use contract::ProtocolDescriptor;
use mapping::Shared;
pub use ring::{Record, Rejection, SendResult};
use std::io;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Returns an identifier tests can use for a shared-memory object or directory.
///
/// A clock reading alone is not unique: on macOS the clock is coarser than the
/// nanosecond, so parallel test threads can read the same value and collide on a
/// shared-memory name or a directory. The per-process counter removes that race.
#[cfg(test)]
pub(crate) fn test_unique_id() -> u64 {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    nanos.wrapping_add(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// One producer handle. It cannot receive records.
pub struct Producer {
    id: u64,
    shared: Shared,
    protocol: ProtocolDescriptor,
}
impl Producer {
    pub fn connect(config: ProducerConfig, protocol: ProtocolDescriptor) -> io::Result<Self> {
        setup::connect(config, protocol)
    }
    /// Connects while allowing the local supervisor to cancel setup.
    pub fn connect_with_cancel(
        config: ProducerConfig, protocol: ProtocolDescriptor, cancellation: &CancellationToken,
    ) -> io::Result<Self> {
        setup::connect_with_cancel(config, protocol, cancellation)
    }
    pub fn session_id(&self) -> u64 {
        self.id
    }
    /// Returns the configured ring capacity in bytes.
    pub fn ring_capacity(&self) -> usize {
        self.shared.capacity
    }
    /// Publishes the fitting prefix and reports notification failures after publication.
    pub fn send_batch(&mut self, records: &[Record<'_>]) -> io::Result<SendResult> {
        self.shared.send_batch(records, &self.protocol)
    }
}
/// One consumer handle. It cannot send records.
pub struct Consumer {
    id: u64,
    shared: Shared,
    protocol: ProtocolDescriptor,
}
impl Consumer {
    pub fn open(config: ConsumerConfig, protocol: ProtocolDescriptor) -> io::Result<Self> {
        setup::open(config, protocol)
    }
    /// Opens while allowing the local supervisor to cancel setup.
    pub fn open_with_cancel(
        config: ConsumerConfig, protocol: ProtocolDescriptor, cancellation: &CancellationToken,
    ) -> io::Result<Self> {
        setup::open_with_cancel(config, protocol, cancellation)
    }
    pub fn session_id(&self) -> u64 {
        self.id
    }
    /// Returns an owned payload and releases its queue bytes.
    pub fn receive(&mut self) -> io::Result<(u32, Vec<u8>)> {
        self.shared.receive(&self.protocol)
    }
    /// Receives one record, or `None` after local cancellation.
    /// A record already being copied may finish before cancellation is observed.
    pub fn receive_with_cancel(&mut self, cancellation: &CancellationToken) -> io::Result<Option<(u32, Vec<u8>)>> {
        self.shared.receive_with_cancel(&self.protocol, cancellation)
    }
}
