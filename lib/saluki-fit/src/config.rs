use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_RING_CAPACITY: usize = 1 << 20;
pub const MAX_RING_CAPACITY: usize = 1 << 30;
/// Default subscriber slots reserved by a broadcast mapping.
pub const DEFAULT_MAX_SUBSCRIBERS: usize = 64;
/// Absolute allocation bound for the subscriber slot table. Rejecting larger
/// values keeps the mapping size finite and the slot stride arithmetic checked.
pub const MAX_SUBSCRIBERS: usize = 4096;

/// Endpoint used only to establish a shared-memory session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupEndpoint {
    /// A pathname Unix-domain socket, restricted by its parent directory permissions.
    Unix(PathBuf),
    /// A loopback TCP address, used only for the setup handshake.
    Tcp(SocketAddr),
}

impl SetupEndpoint {
    /// Parses `unix:/path` or `tcp:127.0.0.1:5101`. A bare path is treated as Unix for compatibility.
    pub fn parse(value: &str) -> io::Result<Self> {
        if let Some(address) = value.strip_prefix("tcp:") {
            let address = address.parse::<SocketAddr>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid TCP setup address: {error}"),
                )
            })?;
            if !address.ip().is_loopback() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TCP setup address must use a loopback IP",
                ));
            }
            return Ok(Self::Tcp(address));
        }
        Ok(Self::Unix(PathBuf::from(value.strip_prefix("unix:").unwrap_or(value))))
    }
}

/// Settings for the consumer, which owns the endpoint and shared memory.
#[derive(Debug, Clone)]
pub struct ConsumerConfig {
    pub endpoint: SetupEndpoint,
    pub setup_timeout: Duration,
    pub ring_capacity: usize,
}
impl ConsumerConfig {
    /// Uses a 60-second setup deadline and 1 MiB ring.
    pub fn new(socket_path: PathBuf) -> Self {
        Self::for_endpoint(SetupEndpoint::Unix(socket_path))
    }
    /// Uses the given setup endpoint with the default timeout and ring capacity.
    pub fn for_endpoint(endpoint: SetupEndpoint) -> Self {
        Self {
            endpoint,
            setup_timeout: DEFAULT_SETUP_TIMEOUT,
            ring_capacity: DEFAULT_RING_CAPACITY,
        }
    }
    /// Uses the given loopback TCP address for setup.
    pub fn tcp(address: SocketAddr) -> Self {
        Self::for_endpoint(SetupEndpoint::Tcp(address))
    }
    pub(crate) fn validate(&self) -> io::Result<Instant> {
        validate_capacity(self.ring_capacity)?;
        deadline(self.setup_timeout)
    }
}

/// Settings for the producer; the consumer offers the ring capacity.
#[derive(Debug, Clone)]
pub struct ProducerConfig {
    pub endpoint: SetupEndpoint,
    pub setup_timeout: Duration,
}
impl ProducerConfig {
    /// Uses a 60-second setup deadline.
    pub fn new(socket_path: PathBuf) -> Self {
        Self::for_endpoint(SetupEndpoint::Unix(socket_path))
    }
    /// Uses the given setup endpoint with the default timeout.
    pub fn for_endpoint(endpoint: SetupEndpoint) -> Self {
        Self {
            endpoint,
            setup_timeout: DEFAULT_SETUP_TIMEOUT,
        }
    }
    /// Uses the given loopback TCP address for setup.
    pub fn tcp(address: SocketAddr) -> Self {
        Self::for_endpoint(SetupEndpoint::Tcp(address))
    }
    pub(crate) fn validate(&self) -> io::Result<Instant> {
        deadline(self.setup_timeout)
    }
}

pub(crate) fn validate_capacity(capacity: usize) -> io::Result<()> {
    if !(16..=MAX_RING_CAPACITY).contains(&capacity) || !capacity.is_multiple_of(8) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ring capacity must be a multiple of 8 from 16 bytes through 1 GiB",
        ));
    }
    Ok(())
}

/// Validates a broadcast capacity and subscriber count with checked arithmetic.
/// The derived mapping must stay within the 32-bit wire field used by setup.
pub(crate) fn validate_broadcast_layout(capacity: usize, max_subscribers: usize) -> io::Result<()> {
    validate_capacity(capacity)?;
    if max_subscribers == 0 || max_subscribers > MAX_SUBSCRIBERS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("maximum subscribers must be from 1 through {MAX_SUBSCRIBERS}"),
        ));
    }
    let slots_bytes = crate::contract::BROADCAST_SLOT_STRIDE
        .checked_mul(max_subscribers)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "slot table overflow"))?;
    let ring_offset = crate::contract::BROADCAST_SLOTS_OFFSET
        .checked_add(slots_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ring offset overflow"))?;
    let region = ring_offset
        .checked_add(capacity)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapping size overflow"))?;
    if region > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broadcast mapping exceeds the 32-bit wire size",
        ));
    }
    Ok(())
}

/// Settings for a broadcast publisher, which owns the endpoint, the listener,
/// and the shared mapping for the whole session.
#[derive(Debug, Clone)]
pub struct BroadcastPublisherConfig {
    pub endpoint: SetupEndpoint,
    /// Per-handshake deadline. Unlike SPSC, it does not bound the listener's
    /// total lifetime, because subscribers may join at any time.
    pub setup_timeout: Duration,
    pub ring_capacity: usize,
    pub max_subscribers: usize,
}
impl BroadcastPublisherConfig {
    /// Uses a 60-second per-handshake deadline, a 1 MiB ring, and 64 slots.
    pub fn new(socket_path: PathBuf) -> Self {
        Self::for_endpoint(SetupEndpoint::Unix(socket_path))
    }
    /// Uses the given setup endpoint with the default deadline and bounds.
    pub fn for_endpoint(endpoint: SetupEndpoint) -> Self {
        Self {
            endpoint,
            setup_timeout: DEFAULT_SETUP_TIMEOUT,
            ring_capacity: DEFAULT_RING_CAPACITY,
            max_subscribers: DEFAULT_MAX_SUBSCRIBERS,
        }
    }
    /// Uses the given loopback TCP address for setup.
    pub fn tcp(address: SocketAddr) -> Self {
        Self::for_endpoint(SetupEndpoint::Tcp(address))
    }
    pub(crate) fn validate(&self) -> io::Result<Duration> {
        validate_broadcast_layout(self.ring_capacity, self.max_subscribers)?;
        deadline(self.setup_timeout)?;
        Ok(self.setup_timeout)
    }
}

/// Settings for one broadcast subscriber. The publisher owns resource bounds,
/// so the subscriber only configures the endpoint and connect deadline.
#[derive(Debug, Clone)]
pub struct SubscriberConfig {
    pub endpoint: SetupEndpoint,
    pub setup_timeout: Duration,
}
impl SubscriberConfig {
    /// Uses a 60-second connect deadline.
    pub fn new(socket_path: PathBuf) -> Self {
        Self::for_endpoint(SetupEndpoint::Unix(socket_path))
    }
    /// Uses the given setup endpoint with the default deadline.
    pub fn for_endpoint(endpoint: SetupEndpoint) -> Self {
        Self {
            endpoint,
            setup_timeout: DEFAULT_SETUP_TIMEOUT,
        }
    }
    /// Uses the given loopback TCP address for setup.
    pub fn tcp(address: SocketAddr) -> Self {
        Self::for_endpoint(SetupEndpoint::Tcp(address))
    }
    pub(crate) fn validate(&self) -> io::Result<Instant> {
        deadline(self.setup_timeout)
    }
}
fn deadline(timeout: Duration) -> io::Result<Instant> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "setup timeout must be positive",
        ));
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "setup timeout is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    #[test]
    fn invalid_configuration_is_rejected_before_resources() {
        for capacity in [0, 8, 17, MAX_RING_CAPACITY + 8] {
            let mut config = ConsumerConfig::new(PathBuf::from("/unused"));
            config.ring_capacity = capacity;
            assert_eq!(config.validate().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        for timeout in [Duration::ZERO, Duration::MAX] {
            let mut config = ProducerConfig::new(PathBuf::from("/unused"));
            config.setup_timeout = timeout;
            assert_eq!(config.validate().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        assert!(ConsumerConfig::new(PathBuf::from("/unused")).validate().is_ok());
    }

    #[test]
    fn setup_endpoint_parses_unix_and_loopback_tcp_forms() {
        assert_eq!(
            SetupEndpoint::parse("unix:/tmp/fit.sock").unwrap(),
            SetupEndpoint::Unix(PathBuf::from("/tmp/fit.sock"))
        );
        assert_eq!(
            SetupEndpoint::parse("tcp:127.0.0.1:5101").unwrap(),
            SetupEndpoint::Tcp("127.0.0.1:5101".parse::<SocketAddr>().unwrap())
        );
        assert!(SetupEndpoint::parse("tcp:0.0.0.0:5101").is_err());
        assert!(SetupEndpoint::parse("tcp:localhost:5101").is_err());
    }
}
