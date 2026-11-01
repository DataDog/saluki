//! Run-wide allocation of runner-host ports.

use std::{
    collections::{btree_map::Entry, BTreeMap},
    ops::RangeInclusive,
};

use super::ParticipantEndpoints;
use crate::{
    case::CaseFile,
    endpoint::{EndpointKind, Listener, Port},
    layout::{describe_participant, Layout, RealizationKind},
};

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

/// Every host-process-group connection or configuration reference to one Linux container endpoint
/// shares one runner-host port within a case and OS.
pub(super) struct RunnerHostPorts<'a> {
    registry: &'a mut PortRegistry,
    ports: BTreeMap<(String, String), u16>,
}

impl<'a> RunnerHostPorts<'a> {
    pub(super) fn new(registry: &'a mut PortRegistry) -> Self {
        Self {
            registry,
            ports: BTreeMap::new(),
        }
    }

    pub(super) fn for_endpoint(&mut self, server: &str, endpoint: &str) -> Result<u16, String> {
        let entry = self.ports.entry((server.into(), endpoint.into()));
        match entry {
            Entry::Occupied(entry) => Ok(*entry.get()),
            Entry::Vacant(entry) => Ok(*entry.insert(self.registry.allocate()?)),
        }
    }

    pub(super) fn into_ports(self) -> BTreeMap<(String, String), u16> {
        self.ports
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

impl EndpointKind {
    pub(super) fn runner_host_transport(self) -> Option<Transport> {
        match self {
            Self::Tcp | Self::Http | Self::Grpc => Some(Transport::Tcp),
            Self::Udp => Some(Transport::Udp),
            Self::UnixDatagram | Self::UnixStream | Self::NamedPipe => None,
        }
    }
}

/// Checks host-process-group network listeners in participant and endpoint name order.
/// Rejects unassigned ports inside `RUNNER_HOST_PORTS` and listeners sharing a transport and port.
pub(super) fn check_host_listeners(
    file: &CaseFile, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
) -> Result<(), String> {
    let mut listeners = BTreeMap::new();
    for (name, realization) in layout.iter() {
        if realization.kind != RealizationKind::HostProcessGroup {
            continue;
        }
        let holder = describe_participant(name, &file.participants[name].participant_type, realization);
        let participant = &endpoints[name];
        for (endpoint_name, endpoint) in &participant.served {
            let Some(transport) = endpoint.kind.runner_host_transport() else {
                continue;
            };
            let Listener::Port(Port::Fixed(port)) = endpoint.listener else {
                unreachable!("network listener ports are resolved before host listener checks");
            };
            let transport_name = transport.name();
            if RUNNER_HOST_PORTS.contains(&port) && !participant.assigned_ports.contains_key(endpoint_name) {
                return Err(format!(
                    "{holder} serves endpoint '{endpoint_name}' on runner-host {transport_name} port {port}, inside {}-{}, where panoramic assigns ports; use a port outside that range",
                    RUNNER_HOST_PORTS.start(), RUNNER_HOST_PORTS.end(),
                ));
            }
            if let Some((earlier_name, earlier_endpoint)) = listeners.insert((transport, port), (name, endpoint_name)) {
                return Err(format!(
                    "{holder} serves endpoint '{endpoint_name}' on runner-host {transport_name} port {port}, which participant '{earlier_name}' already serves as endpoint '{earlier_endpoint}'"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_endpoint_kinds_map_to_their_runner_host_transport() {
        for (kind, expected) in [
            (EndpointKind::Tcp, Some(Transport::Tcp)),
            (EndpointKind::Http, Some(Transport::Tcp)),
            (EndpointKind::Grpc, Some(Transport::Tcp)),
            (EndpointKind::Udp, Some(Transport::Udp)),
            (EndpointKind::UnixDatagram, None),
            (EndpointKind::UnixStream, None),
            (EndpointKind::NamedPipe, None),
        ] {
            assert_eq!(kind.runner_host_transport(), expected);
        }
    }

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
