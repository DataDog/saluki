use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_RING_CAPACITY: usize = 1 << 20;
pub const MAX_RING_CAPACITY: usize = 1 << 30;

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
    if !(16..=MAX_RING_CAPACITY).contains(&capacity) || capacity % 8 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ring capacity must be a multiple of 8 from 16 bytes through 1 GiB",
        ));
    }
    Ok(())
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
