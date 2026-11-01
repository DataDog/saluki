//! Run-wide allocation of runner-host ports.

use std::ops::RangeInclusive;

const RUNNER_HOST_PORTS: RangeInclusive<u16> = 20000..=29999;

/// Records runner-host port numbers handed out during planning.
///
/// Ports come from `20000..=29999`, below every default ephemeral range (Linux 32768+,
/// macOS and Windows 49152+), so the OS doesn't hand these ports to outgoing connections.
pub struct PortRegistry {
    available: RangeInclusive<u16>,
    capacity: usize,
}

impl PortRegistry {
    /// Creates an empty allocation registry over `20000..=29999`.
    pub fn new() -> Self {
        Self::over(RUNNER_HOST_PORTS)
    }

    pub(crate) fn allocate(&mut self) -> Result<u16, String> {
        self.available
            .next()
            .ok_or_else(|| format!("all {} runner-host ports are allocated", self.capacity))
    }

    pub(super) fn over(range: RangeInclusive<u16>) -> Self {
        let capacity = range.clone().count();
        Self {
            available: range,
            capacity,
        }
    }
}

impl Default for PortRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_never_hands_out_a_port_twice() {
        let mut registry = PortRegistry::new();
        for expected in 20000..=29999 {
            assert_eq!(registry.allocate(), Ok(expected));
        }
    }

    #[test]
    fn the_registry_allocates_below_every_default_ephemeral_range() {
        let registry = PortRegistry::default();
        for start in [32768, 49152] {
            assert!(*registry.available.start() < start);
            assert!(*registry.available.end() < start);
        }
    }

    #[test]
    fn an_exhausted_registry_reports_how_many_ports_it_had() {
        let mut registry = PortRegistry::over(21000..=21001);
        assert_eq!(registry.allocate(), Ok(21000));
        assert_eq!(registry.allocate(), Ok(21001));
        for _ in 0..2 {
            assert_eq!(registry.allocate(), Err("all 2 runner-host ports are allocated".into()));
        }
    }
}
