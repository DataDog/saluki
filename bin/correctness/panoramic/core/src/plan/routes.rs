//! Checks which consumer and server realizations a connection can join for each endpoint kind.
//!
//! Network endpoints join equal realizations, or a host process group to a Linux container.
//! Unix sockets need equal realizations other than Windows containers to share a filesystem.
//! Named pipes are reachable only inside their own container.

use crate::{
    endpoint::EndpointKind,
    layout::{Realization, RealizationKind},
};

/// Returns the constraint preventing a connection, if its realizations have no route.
pub(super) fn check_route(kind: EndpointKind, consumer: Realization, server: Realization) -> Result<(), &'static str> {
    match kind {
        EndpointKind::Tcp | EndpointKind::Udp | EndpointKind::Http | EndpointKind::Grpc => {
            if consumer == server
                || (consumer.kind == RealizationKind::HostProcessGroup
                    && server.kind == RealizationKind::LinuxContainer)
            {
                Ok(())
            } else {
                Err("network endpoints connect only participants with the same realization, or a host-process-group to a linux-container")
            }
        }
        EndpointKind::UnixDatagram | EndpointKind::UnixStream => {
            if consumer == server && consumer.kind != RealizationKind::WindowsContainer {
                Ok(())
            } else {
                Err("unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem")
            }
        }
        EndpointKind::NamedPipe => Err("a named pipe is reachable only inside its own container"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Os;

    const LINUX_CONTAINER: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };
    const LINUX_HOST: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::HostProcessGroup,
    };
    const MACOS_HOST: Realization = Realization {
        os: Os::MacOs,
        kind: RealizationKind::HostProcessGroup,
    };
    const WINDOWS_CONTAINER: Realization = Realization {
        os: Os::Windows,
        kind: RealizationKind::WindowsContainer,
    };
    const LINUX_POD: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::KubernetesPod,
    };
    const REALIZATIONS: [Realization; 5] = [LINUX_CONTAINER, LINUX_HOST, MACOS_HOST, WINDOWS_CONTAINER, LINUX_POD];

    #[test]
    fn network_endpoints_connect_only_the_same_realization_or_a_host_process_group_to_a_linux_container() {
        let supported = [
            (LINUX_CONTAINER, LINUX_CONTAINER),
            (LINUX_HOST, LINUX_HOST),
            (MACOS_HOST, MACOS_HOST),
            (WINDOWS_CONTAINER, WINDOWS_CONTAINER),
            (LINUX_POD, LINUX_POD),
            (LINUX_HOST, LINUX_CONTAINER),
            (MACOS_HOST, LINUX_CONTAINER),
        ];
        for kind in [
            EndpointKind::Tcp,
            EndpointKind::Udp,
            EndpointKind::Http,
            EndpointKind::Grpc,
        ] {
            for consumer in REALIZATIONS {
                for server in REALIZATIONS {
                    assert_eq!(
                        check_route(kind, consumer, server).is_ok(),
                        supported.contains(&(consumer, server)),
                        "{kind:?}: {consumer:?} -> {server:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn unix_sockets_connect_only_the_same_realization_other_than_a_windows_container() {
        let supported = [
            (LINUX_CONTAINER, LINUX_CONTAINER),
            (LINUX_HOST, LINUX_HOST),
            (MACOS_HOST, MACOS_HOST),
            (LINUX_POD, LINUX_POD),
        ];
        for kind in [EndpointKind::UnixDatagram, EndpointKind::UnixStream] {
            for consumer in REALIZATIONS {
                for server in REALIZATIONS {
                    assert_eq!(
                        check_route(kind, consumer, server).is_ok(),
                        supported.contains(&(consumer, server)),
                        "{kind:?}: {consumer:?} -> {server:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn named_pipes_connect_no_two_participants() {
        for consumer in REALIZATIONS {
            for server in REALIZATIONS {
                assert!(
                    check_route(EndpointKind::NamedPipe, consumer, server).is_err(),
                    "{consumer:?} -> {server:?}",
                );
            }
        }
    }

    #[test]
    fn route_errors_name_the_constraint() {
        assert_eq!(
            check_route(EndpointKind::Tcp, LINUX_CONTAINER, MACOS_HOST),
            Err("network endpoints connect only participants with the same realization, or a host-process-group to a linux-container"),
        );
        assert_eq!(
            check_route(EndpointKind::UnixStream, MACOS_HOST, LINUX_CONTAINER),
            Err("unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"),
        );
        assert_eq!(
            check_route(EndpointKind::NamedPipe, WINDOWS_CONTAINER, WINDOWS_CONTAINER),
            Err("a named pipe is reachable only inside its own container"),
        );
    }
}
