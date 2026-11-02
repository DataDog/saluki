//! Resolves consumer-visible endpoint addresses through the realization route table.
//!
//! Each supported cell selects one route. Filesystem routes between participants require a shared namespace.

use super::{endpoints::ParticipantEndpoints, ports::RunnerHostPorts, scope::check_consumed_socket};
use crate::{
    endpoint::{Endpoint, EndpointKind, Listener, Port, ResolvedAddress},
    layout::{Realization, RealizationKind},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    ByName,
    Loopback,
    RunnerHostPort,
    SharedFilesystem,
    NamedPipe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RunnerHostAccess {
    Direct(Route),
    Courier,
}

/// Chooses runner-host access from the endpoint kind and server realization.
pub(super) fn runner_host_route(kind: EndpointKind, server: Realization) -> Result<RunnerHostAccess, &'static str> {
    use EndpointKind::{Grpc, Http, NamedPipe, Tcp, Udp, UnixDatagram, UnixStream};
    use RealizationKind::{HostProcessGroup, KubernetesPod, LinuxContainer, WindowsContainer};
    use RunnerHostAccess::{Courier, Direct};

    match (kind, server.kind) {
        (Tcp | Udp | Http | Grpc, LinuxContainer) | (Tcp | Http | Grpc, KubernetesPod) => {
            Ok(Direct(Route::RunnerHostPort))
        }
        (Tcp | Udp | Http | Grpc, HostProcessGroup) => Ok(Direct(Route::Loopback)),
        (Tcp | Udp | Http | Grpc, WindowsContainer) | (Udp, KubernetesPod) => Ok(Courier),
        (UnixDatagram | UnixStream, HostProcessGroup) => Ok(Direct(Route::SharedFilesystem)),
        (UnixDatagram | UnixStream, LinuxContainer | KubernetesPod) | (NamedPipe, WindowsContainer) => Ok(Courier),
        (UnixDatagram | UnixStream, WindowsContainer) => Err("unix sockets are not supported in a windows-container"),
        (NamedPipe, _) => Err("a named pipe is reachable only in a windows-container"),
    }
}

pub(super) enum Unreachable {
    NoRoute(&'static str),
    Socket { path: String, reason: String },
}

/// Chooses a route and checks socket location and length when reaching another participant.
pub(super) fn reach(
    holder: &str, holder_realization: Realization, server: &str, server_realization: Realization,
    server_endpoints: &ParticipantEndpoints, endpoint: &Endpoint,
) -> Result<Route, Unreachable> {
    if holder == server {
        match endpoint.kind {
            EndpointKind::UnixDatagram | EndpointKind::UnixStream
                if holder_realization.kind != RealizationKind::WindowsContainer =>
            {
                return Ok(Route::SharedFilesystem)
            }
            EndpointKind::NamedPipe => return Ok(Route::NamedPipe),
            _ => {}
        }
    }
    let route = route(endpoint.kind, holder_realization, server_realization).map_err(Unreachable::NoRoute)?;
    if route == Route::SharedFilesystem {
        let Listener::Path(path) = &endpoint.listener else {
            unreachable!("shared-filesystem routes use socket paths");
        };
        check_consumed_socket(path, &server_endpoints.shared_dir, server_endpoints.os).map_err(|reason| {
            Unreachable::Socket {
                path: path.clone(),
                reason,
            }
        })?;
    }
    Ok(route)
}

/// Allocates any runner-host port a checked route needs and renders the consumer-visible address.
pub(super) fn resolve(
    route: Route, server: &str, endpoint_name: &str, endpoint: &Endpoint, ports: &mut RunnerHostPorts<'_>,
) -> Result<ResolvedAddress, String> {
    let runner_host_port = if route == Route::RunnerHostPort {
        Some(ports.for_endpoint(server, endpoint_name)?)
    } else {
        None
    };
    Ok(address(route, server, endpoint, runner_host_port))
}

/// Returns the cell's route, or the constraint preventing the connection.
fn route(kind: EndpointKind, consumer: Realization, server: Realization) -> Result<Route, &'static str> {
    use EndpointKind::{Grpc, Http, NamedPipe, Tcp, Udp, UnixDatagram, UnixStream};
    use RealizationKind::{HostProcessGroup, KubernetesPod, LinuxContainer, WindowsContainer};

    match (kind, consumer.kind, server.kind) {
        (Tcp | Udp | Http | Grpc, LinuxContainer, LinuxContainer)
        | (Tcp | Udp | Http | Grpc, WindowsContainer, WindowsContainer)
        | (Tcp | Udp | Http | Grpc, KubernetesPod, KubernetesPod) if consumer == server => Ok(Route::ByName),
        (Tcp | Udp | Http | Grpc, HostProcessGroup, HostProcessGroup) if consumer == server => Ok(Route::Loopback),
        (Tcp | Udp | Http | Grpc, HostProcessGroup, LinuxContainer) => Ok(Route::RunnerHostPort),
        (UnixDatagram | UnixStream, LinuxContainer, LinuxContainer)
        | (UnixDatagram | UnixStream, HostProcessGroup, HostProcessGroup)
        | (UnixDatagram | UnixStream, KubernetesPod, KubernetesPod) if consumer == server => Ok(Route::SharedFilesystem),
        (Tcp | Udp | Http | Grpc, _, _) => Err("network endpoints connect only participants with the same realization, or a host-process-group to a linux-container"),
        (UnixDatagram | UnixStream, _, _) => Err("unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"),
        (NamedPipe, _, _) => Err("a named pipe is reachable only inside its own container"),
    }
}

/// Renders a checked route using the server's resolved listener and any allocated runner-host port.
fn address(route: Route, server_name: &str, endpoint: &Endpoint, runner_host_port: Option<u16>) -> ResolvedAddress {
    let (url, host, port, path) = match route {
        Route::ByName | Route::Loopback | Route::RunnerHostPort => {
            let scheme = match endpoint.kind {
                EndpointKind::Tcp => "tcp",
                EndpointKind::Udp => "udp",
                EndpointKind::Http => "http",
                EndpointKind::Grpc => "grpc",
                _ => unreachable!("network routes use network endpoint kinds"),
            };
            let Listener::Port(Port::Fixed(server_port)) = endpoint.listener else {
                unreachable!("network routes use ports resolved during endpoint planning");
            };
            let host = if route == Route::ByName {
                server_name
            } else {
                "127.0.0.1"
            };
            let port = if route == Route::RunnerHostPort {
                runner_host_port.expect("runner-host routes have an allocated port")
            } else {
                server_port
            };
            (format!("{scheme}://{host}:{port}"), Some(host.into()), Some(port), None)
        }
        Route::SharedFilesystem => {
            let scheme = match endpoint.kind {
                EndpointKind::UnixDatagram => "unixgram",
                EndpointKind::UnixStream => "unix",
                _ => unreachable!("shared-filesystem routes use Unix socket endpoint kinds"),
            };
            let Listener::Path(path) = &endpoint.listener else {
                unreachable!("shared-filesystem routes use socket paths");
            };
            (format!("{scheme}://{path}"), None, None, Some(path.clone()))
        }
        Route::NamedPipe => {
            let Listener::Pipe(name) = &endpoint.listener else {
                unreachable!("named-pipe routes use pipe names");
            };
            let path = format!(r"\\.\pipe\{name}");
            (path.clone(), None, None, Some(path))
        }
    };
    ResolvedAddress {
        kind: endpoint.kind,
        url,
        host,
        port,
        path,
    }
}

#[cfg(test)]
mod tests {
    use panoramic_courier::Target;

    use super::*;
    use crate::{backend::courier, layout::Os};

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
    fn every_endpoint_kind_and_server_realization_has_exactly_the_runner_host_route_the_table_names() {
        use RunnerHostAccess::{Courier, Direct};

        let network = [
            Some(Direct(Route::RunnerHostPort)),
            Some(Direct(Route::Loopback)),
            Some(Direct(Route::Loopback)),
            Some(Courier),
            Some(Direct(Route::RunnerHostPort)),
        ];
        let udp = [network[0], network[1], network[2], network[3], Some(Courier)];
        let sockets = [
            Some(Courier),
            Some(Direct(Route::SharedFilesystem)),
            Some(Direct(Route::SharedFilesystem)),
            None,
            Some(Courier),
        ];
        for (kind, expected) in [
            (EndpointKind::Tcp, network),
            (EndpointKind::Http, network),
            (EndpointKind::Grpc, network),
            (EndpointKind::Udp, udp),
            (EndpointKind::UnixDatagram, sockets),
            (EndpointKind::UnixStream, sockets),
            (EndpointKind::NamedPipe, [None, None, None, Some(Courier), None]),
        ] {
            for (server, expected) in REALIZATIONS.into_iter().zip(expected) {
                assert_eq!(runner_host_route(kind, server).ok(), expected, "{kind:?}: {server:?}");
            }
        }
    }

    #[test]
    fn runner_host_route_errors_name_the_constraint() {
        for kind in [EndpointKind::UnixDatagram, EndpointKind::UnixStream] {
            assert_eq!(
                runner_host_route(kind, WINDOWS_CONTAINER),
                Err("unix sockets are not supported in a windows-container")
            );
        }
        for server in [LINUX_CONTAINER, LINUX_HOST, MACOS_HOST, LINUX_POD] {
            assert_eq!(
                runner_host_route(EndpointKind::NamedPipe, server),
                Err("a named pipe is reachable only in a windows-container")
            );
        }
    }

    #[test]
    fn every_kind_and_realization_pair_has_exactly_the_route_the_table_names() {
        let network = [
            (LINUX_CONTAINER, LINUX_CONTAINER, Route::ByName),
            (LINUX_HOST, LINUX_HOST, Route::Loopback),
            (MACOS_HOST, MACOS_HOST, Route::Loopback),
            (WINDOWS_CONTAINER, WINDOWS_CONTAINER, Route::ByName),
            (LINUX_POD, LINUX_POD, Route::ByName),
            (LINUX_HOST, LINUX_CONTAINER, Route::RunnerHostPort),
            (MACOS_HOST, LINUX_CONTAINER, Route::RunnerHostPort),
        ];
        let sockets = [
            (LINUX_CONTAINER, LINUX_CONTAINER, Route::SharedFilesystem),
            (LINUX_HOST, LINUX_HOST, Route::SharedFilesystem),
            (MACOS_HOST, MACOS_HOST, Route::SharedFilesystem),
            (LINUX_POD, LINUX_POD, Route::SharedFilesystem),
        ];
        for (kind, supported) in [
            (EndpointKind::Tcp, network.as_slice()),
            (EndpointKind::Udp, network.as_slice()),
            (EndpointKind::Http, network.as_slice()),
            (EndpointKind::Grpc, network.as_slice()),
            (EndpointKind::UnixDatagram, sockets.as_slice()),
            (EndpointKind::UnixStream, sockets.as_slice()),
            (EndpointKind::NamedPipe, &[]),
        ] {
            for consumer in REALIZATIONS {
                for server in REALIZATIONS {
                    let expected = supported
                        .iter()
                        .find(|&&(c, s, _)| c == consumer && s == server)
                        .map(|&(_, _, route)| route);
                    assert_eq!(
                        route(kind, consumer, server).ok(),
                        expected,
                        "{kind:?}: {consumer:?} -> {server:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn route_errors_name_the_constraint() {
        assert_eq!(
            route(EndpointKind::Tcp, LINUX_CONTAINER, MACOS_HOST),
            Err("network endpoints connect only participants with the same realization, or a host-process-group to a linux-container"),
        );
        assert_eq!(
            route(EndpointKind::UnixStream, MACOS_HOST, LINUX_CONTAINER),
            Err("unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"),
        );
        assert_eq!(
            route(EndpointKind::NamedPipe, WINDOWS_CONTAINER, WINDOWS_CONTAINER),
            Err("a named pipe is reachable only inside its own container"),
        );
    }

    #[test]
    fn addresses_use_their_endpoint_kinds_url_scheme() {
        for (endpoint, route, expected) in [
            (Endpoint::tcp(Port::Fixed(80)), Route::ByName, "tcp://server:80"),
            (Endpoint::udp(Port::Fixed(80)), Route::ByName, "udp://server:80"),
            (Endpoint::http(Port::Fixed(80)), Route::ByName, "http://server:80"),
            (Endpoint::grpc(Port::Fixed(80)), Route::ByName, "grpc://server:80"),
            (
                Endpoint::unix_datagram("/run/x.sock"),
                Route::SharedFilesystem,
                "unixgram:///run/x.sock",
            ),
            (
                Endpoint::unix_stream("/run/x.sock"),
                Route::SharedFilesystem,
                "unix:///run/x.sock",
            ),
        ] {
            let resolved = address(route, "server", &endpoint, None);
            assert_eq!(resolved.kind, endpoint.kind);
            assert_eq!(resolved.url, expected);
        }
    }

    #[test]
    fn rendered_addresses_agree_with_courier_targets() {
        for route in [Route::ByName, Route::Loopback, Route::RunnerHostPort] {
            for endpoint in [
                Endpoint::tcp(Port::Fixed(80)),
                Endpoint::udp(Port::Fixed(80)),
                Endpoint::http(Port::Fixed(80)),
                Endpoint::grpc(Port::Fixed(80)),
            ] {
                let resolved = address(route, "server", &endpoint, Some(20000));
                let expected = if matches!(endpoint.kind, EndpointKind::Http | EndpointKind::Grpc) {
                    assert!(Target::parse(&resolved.url).is_err());
                    Target::parse(&format!(
                        "tcp://{}:{}",
                        resolved.host.as_ref().unwrap(),
                        resolved.port.unwrap(),
                    ))
                    .unwrap()
                } else {
                    Target::parse(&resolved.url).unwrap()
                };
                assert_eq!(courier::target(&resolved).unwrap(), expected, "{route:?}");
            }
        }
        for endpoint in [
            Endpoint::unix_datagram("/airlock/srv/shared/a.sock"),
            Endpoint::unix_stream("/airlock/srv/shared/a.sock"),
        ] {
            let resolved = address(Route::SharedFilesystem, "server", &endpoint, None);
            assert_eq!(
                courier::target(&resolved).unwrap(),
                Target::parse(&resolved.url).unwrap()
            );
        }
    }

    #[test]
    fn a_by_name_address_uses_the_server_name_and_its_port() {
        assert_eq!(
            address(Route::ByName, "server", &Endpoint::http(Port::Fixed(8080)), None),
            ResolvedAddress {
                kind: EndpointKind::Http,
                url: "http://server:8080".into(),
                host: Some("server".into()),
                port: Some(8080),
                path: None,
            }
        );
    }

    #[test]
    fn a_loopback_address_uses_127_0_0_1_and_the_servers_port() {
        assert_eq!(
            address(Route::Loopback, "server", &Endpoint::tcp(Port::Fixed(8080)), None),
            ResolvedAddress {
                kind: EndpointKind::Tcp,
                url: "tcp://127.0.0.1:8080".into(),
                host: Some("127.0.0.1".into()),
                port: Some(8080),
                path: None,
            }
        );
    }

    #[test]
    fn a_runner_host_port_address_uses_127_0_0_1_and_the_allocated_port() {
        assert_eq!(
            address(
                Route::RunnerHostPort,
                "server",
                &Endpoint::grpc(Port::Fixed(8080)),
                Some(20000)
            ),
            ResolvedAddress {
                kind: EndpointKind::Grpc,
                url: "grpc://127.0.0.1:20000".into(),
                host: Some("127.0.0.1".into()),
                port: Some(20000),
                path: None,
            }
        );
    }

    #[test]
    fn a_named_pipe_address_is_its_pipe_path() {
        assert_eq!(
            address(Route::NamedPipe, "server", &Endpoint::named_pipe("events"), None),
            ResolvedAddress {
                kind: EndpointKind::NamedPipe,
                url: r"\\.\pipe\events".into(),
                host: None,
                port: None,
                path: Some(r"\\.\pipe\events".into()),
            }
        );
    }

    #[test]
    fn a_socket_address_is_the_servers_path_with_no_host_or_port() {
        assert_eq!(
            address(
                Route::SharedFilesystem,
                "server",
                &Endpoint::unix_stream("/run/server.sock"),
                None
            ),
            ResolvedAddress {
                kind: EndpointKind::UnixStream,
                url: "unix:///run/server.sock".into(),
                host: None,
                port: None,
                path: Some("/run/server.sock".into()),
            }
        );
    }
}
