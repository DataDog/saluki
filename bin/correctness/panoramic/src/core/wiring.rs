//! Endpoints, connections and routes: the addresses the harness owns, and how consumers and the runner reach them.
//!
//! An endpoint is a name, a protocol and an address: a port, socket path or pipe name. Which interface a listener binds
//! is software configuration, and the harness never reads it. A participant type declares its endpoints and wires them
//! by referring to them in the default environment it returns. An endpoint it declares is usable only if the
//! participant's own configuration refers to it after the merge: its environment, its command line, or a file it
//! places as a template. An endpoint the case declares is usable as declared.
//!
//! `connect` is written on the consumer. A connection checks that the server's endpoint exists, is usable and has a
//! route from the consumer. It orders the server's start before the consumer's, gates the consumer's start on the
//! endpoint's readiness evidence, and delivers the address as the consumer reaches it through `self.connections`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;

use super::case::LoadedCase;
use super::participant::{ConnectionTarget, Enclosure, Os, Realization};
use super::plan::{PlannedParticipant, Problems};
use super::spec::{Address, Consumed, EndpointSpec, Evidence, ParticipantSpec, Protocol, Wiring};
use super::step::EndpointOperation;
use super::template::{IdentityPart, Reference, Template};

/// An endpoint with the address the harness gave it.
#[derive(Debug, Serialize)]
pub(crate) struct PlannedEndpoint {
    pub(crate) protocol: Protocol,
    pub(crate) address: PlannedAddress,
    pub(crate) tls: bool,
    /// What a consumer waits for before it starts.
    pub(crate) evidence: Evidence,
    /// How the runner observes the evidence, when it observes the endpoint itself.
    pub(crate) reach: Option<Reach>,
    /// Whether the runner makes HTTP and gRPC requests through a port the backend publishes at start.
    pub(crate) published: bool,
    /// Why the participant does not wire the endpoint, when it does not. Using it is a plan error.
    pub(crate) unwired: Option<Unwired>,
}

/// The part of an endpoint the harness owns, with every reference filled in.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlannedAddress {
    Port(u16),
    Path(String),
    Pipe(String),
}

/// Why an endpoint is not wired, and the line that would wire it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Unwired {
    pub(crate) why: String,
    pub(crate) write: Option<String>,
}

/// A connection with the address the consumer reaches.
#[derive(Debug, Serialize)]
pub(crate) struct PlannedConnection {
    pub(crate) target: ConnectionTarget,
    pub(crate) protocol: Protocol,
    pub(crate) route: Route,
    /// The address as the consumer reaches it, as `self.connections.<name>.url`.
    pub(crate) url: String,
    pub(crate) reached: Reached,
    /// Environment keys the connection adds to the consumer, because its participant type knows the name.
    pub(crate) delivers: Vec<&'static str>,
}

/// The parts of a connected address other than its URL.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reached {
    Host { host: String, port: u16 },
    Path(String),
}

/// What a consumer waits for before it starts: an endpoint it connects to, and its readiness evidence.
#[derive(Debug, Serialize)]
pub(crate) struct Gate {
    pub(crate) target: ConnectionTarget,
    pub(crate) evidence: Evidence,
    pub(crate) reach: Option<Reach>,
}

/// How a consumer reaches a server of the same realization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Route {
    /// By the server's participant name, on the case's network or through its headless Service.
    ByName,
    /// On `127.0.0.1`, between host processes.
    Loopback,
    /// Through a socket under the server's own directory, which every participant of the case sees at one path.
    SharedDirectory,
}

/// How the runner reaches an endpoint, for a step, a readiness gate or an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reach {
    /// The harness runs the courier inside the server, and the courier reaches the listener as a peer would.
    CourierInServer,
    /// Through a port published on the runner's loopback address.
    PublishedPort,
    /// Through a port forwarded to the pod.
    ForwardedPort,
    /// From the runner, with the courier's library.
    InRunner,
}

/// Largest Unix socket path in bytes on macOS.
const MACOS_SOCKET_PATH_MAX: usize = 103;
/// Largest Unix socket path in bytes on every other OS.
const SOCKET_PATH_MAX: usize = 107;

impl Protocol {
    /// The evidence a consumer waits for when the participant names none.
    pub(crate) fn default_evidence(self) -> Evidence {
        match self {
            Self::Tcp | Self::Http | Self::Grpc => Evidence::Accepts,
            Self::UnixDatagram | Self::UnixStream => Evidence::Exists,
            Self::Udp | Self::NamedPipe => Evidence::None,
        }
    }

    fn is_socket(self) -> bool {
        matches!(self, Self::UnixDatagram | Self::UnixStream)
    }

    /// The scheme of the URL a consumer receives. No participant connects to a named pipe.
    fn scheme(self, tls: bool) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Http if tls => "https",
            Self::Http => "http",
            Self::Grpc => "grpc",
            Self::UnixDatagram => "unixgram",
            Self::UnixStream => "unix",
            Self::NamedPipe => "pipe",
        }
    }
}

impl fmt::Display for PlannedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Port(port) => write!(f, "{}", port),
            Self::Path(path) => f.write_str(path),
            Self::Pipe(name) => f.write_str(name),
        }
    }
}

impl PlannedEndpoint {
    /// Plans one endpoint of `participant`: its address, its readiness evidence and how the runner reaches it.
    pub(super) fn plan(
        spec: EndpointSpec, ready: Option<Evidence>, participant: &str, realization: Realization,
    ) -> Result<Self, String> {
        let facts = realization.enclosure.facts();
        if !facts.listens.contains(&spec.protocol) {
            return Err(format!(
                "a {} cannot listen on a {} endpoint",
                realization.enclosure,
                spec.protocol.as_str()
            ));
        }
        let address = match spec.address {
            Address::Port(port) => PlannedAddress::Port(port),
            Address::Pipe(name) => PlannedAddress::Pipe(name),
            Address::Path(path) => PlannedAddress::Path(socket_path(&path, participant, realization)?),
        };
        let evidence = ready.unwrap_or_else(|| spec.protocol.default_evidence());
        let probe = |operation| reach(operation, spec.protocol, spec.tls, realization.enclosure).map(Some);
        let reach = match &evidence {
            Evidence::Accepts => probe(EndpointOperation::Probe)?,
            Evidence::Exists if spec.protocol.is_socket() => probe(EndpointOperation::Probe)?,
            Evidence::Exists => return Err("'exists' is evidence for a Unix socket".to_string()),
            Evidence::Http { .. } if spec.protocol == Protocol::Http => probe(EndpointOperation::Request)?,
            Evidence::Http { .. } => return Err("'http' evidence is for an http endpoint".to_string()),
            Evidence::Output { .. } | Evidence::None => None,
        };
        Ok(Self {
            protocol: spec.protocol,
            address,
            tls: spec.tls,
            evidence,
            reach,
            published: facts.publishes && matches!(spec.protocol, Protocol::Http | Protocol::Grpc),
            unwired: None,
        })
    }

    /// The address space the endpoint's listener holds its address in, and the address as that space compares it.
    ///
    /// Two listeners cannot hold one address in one space, so every endpoint counts, wired or not. `tcp`, `http` and
    /// `grpc` endpoints all hold TCP ports, and `udp` endpoints hold UDP ports. A socket path is one file, whichever
    /// Unix protocol listens on it. A pipe name is its own space, and Windows compares pipe names without regard to
    /// letter case.
    fn held(&self) -> (&'static str, String) {
        match (&self.address, self.protocol) {
            (PlannedAddress::Port(port), Protocol::Udp) => ("udp port", port.to_string()),
            (PlannedAddress::Port(port), _) => ("tcp port", port.to_string()),
            (PlannedAddress::Path(path), _) => ("socket path", path.clone()),
            (PlannedAddress::Pipe(name), _) => ("pipe name", name.to_ascii_lowercase()),
        }
    }
}

/// Fills in a socket path, which may refer only to the participant's own directory, and checks its parts and that it
/// is short enough for a Unix socket address.
fn socket_path(path: &Template, participant: &str, realization: Realization) -> Result<String, String> {
    let enclosure = realization.enclosure;
    let facts = enclosure.facts();
    let own_dir = |reference: &Reference| match reference {
        Reference::Identity {
            holder,
            part: IdentityPart::Dir,
        } => holder.names(participant),
        _ => false,
    };
    let filled = path.resolve(false, |reference| {
        own_dir(reference).then(|| facts.dir(participant)).flatten()
    });
    if let Some(reference) = filled.references().next() {
        let why = if own_dir(reference) {
            format!("a {} has no directory at plan yet", enclosure)
        } else {
            "a socket path may refer only to the participant's own directory".to_string()
        };
        return Err(format!("socket path '{}' holds '{}': {}", path, reference, why));
    }
    let filled = filled.text();
    let parts = filled.strip_prefix(facts.separator).unwrap_or(filled);
    if parts
        .split(facts.separator)
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("socket path '{}' has an empty, '.' or '..' part", filled));
    }
    let max = if realization.os == Os::Macos {
        MACOS_SOCKET_PATH_MAX
    } else {
        SOCKET_PATH_MAX
    };
    if filled.len() > max {
        return Err(format!(
            "socket path '{}' is {} bytes long, and a Unix socket path on {} holds at most {}",
            filled,
            filled.len(),
            realization.os,
            max
        ));
    }
    Ok(filled.to_string())
}

/// How a consumer of `protocol` reaches a server: the cells where both ends have the same realization.
pub(crate) fn route(protocol: Protocol, consumer: Realization, server: Realization) -> Result<Route, String> {
    if protocol == Protocol::NamedPipe {
        return Err("no participant can connect to a named pipe: only steps reach one".to_string());
    }
    if consumer != server {
        return Err(format!(
            "a {} on {} cannot connect to a {} on {}: participants of different realizations cannot connect",
            consumer.enclosure, consumer.os, server.enclosure, server.os
        ));
    }
    match (protocol.is_socket(), server.enclosure) {
        (false, Enclosure::HostProcessGroup) => Ok(Route::Loopback),
        (false, _) => Ok(Route::ByName),
        (true, Enclosure::WindowsContainer) => {
            Err("Windows containers share no directory, so no Unix socket connects them".to_string())
        }
        (true, _) => Ok(Route::SharedDirectory),
    }
}

/// How the runner reaches an endpoint of `protocol` on a server of realization `server` to do `operation`.
pub(crate) fn reach(
    operation: EndpointOperation, protocol: Protocol, tls: bool, server: Enclosure,
) -> Result<Reach, String> {
    use Enclosure::{HostProcessGroup, KubernetesPod, LinuxContainer, WindowsContainer};

    match (operation, protocol, server) {
        (EndpointOperation::Probe, Protocol::Udp, _) => Err(
            "a probe cannot confirm a UDP listener: wait for its log line, {output: <pattern>, process: <name>}"
                .to_string(),
        ),
        (EndpointOperation::Request, Protocol::Http | Protocol::Grpc, LinuxContainer) => Ok(Reach::PublishedPort),
        (EndpointOperation::Request, Protocol::Http | Protocol::Grpc, KubernetesPod) => Ok(Reach::ForwardedPort),
        (EndpointOperation::Request, Protocol::Http | Protocol::Grpc, HostProcessGroup) => Ok(Reach::InRunner),
        (EndpointOperation::Request, Protocol::Http | Protocol::Grpc, WindowsContainer) if tls => {
            Err("a request to a Windows container goes through the courier, which does not speak TLS".to_string())
        }
        (EndpointOperation::Request, Protocol::Http | Protocol::Grpc, WindowsContainer) => Ok(Reach::CourierInServer),
        (EndpointOperation::Request, _, _) => Err("a request goes to an http or grpc endpoint".to_string()),
        (_, Protocol::NamedPipe, WindowsContainer) => Ok(Reach::CourierInServer),
        (_, Protocol::NamedPipe, _) => Err("a named pipe exists only in a Windows container".to_string()),
        (EndpointOperation::Probe | EndpointOperation::Send, _, HostProcessGroup) => Ok(Reach::InRunner),
        (EndpointOperation::Send, _, KubernetesPod) => Err("bytes cannot be sent to a pod".to_string()),
        (EndpointOperation::Probe | EndpointOperation::Send, _, _) => Ok(Reach::CourierInServer),
    }
}

/// What a participant type wired, kept from before the case's fields are laid over its spec.
pub(super) struct TypeWiring {
    endpoints: BTreeSet<String>,
    env: BTreeMap<String, Template>,
    hints: BTreeMap<String, Wiring>,
}

impl TypeWiring {
    /// Takes the wiring of a participant type's spec. A participant with no participant type wires nothing.
    pub(super) fn take(spec: &mut ParticipantSpec) -> Self {
        Self {
            endpoints: spec.endpoints.keys().cloned().collect(),
            env: spec.env.clone(),
            hints: std::mem::take(&mut spec.wiring),
        }
    }
}

impl PlannedParticipant {
    /// Checks that no two endpoints of the participant hold the same address. A port the participant's configuration
    /// holds as a literal is not an endpoint, so it is not checked.
    ///
    /// Returns the problems found.
    pub(super) fn check_addresses(&self) -> Vec<String> {
        let mut holders: BTreeMap<(&str, String), &str> = BTreeMap::new();
        let mut problems = Vec::new();
        for (name, endpoint) in &self.endpoints {
            let (space, address) = endpoint.held();
            let holder = *holders.entry((space, address)).or_insert(name);
            if holder != name {
                problems.push(format!(
                    "endpoints '{}' and '{}' share {} {}.\n  Each endpoint of a participant needs an address of its own.",
                    holder, name, space, endpoint.address
                ));
            }
        }
        problems
    }

    /// Marks each endpoint the participant type declares that the participant's configuration does not refer to, and
    /// checks that each connection is used. `name` is the participant's own name, and `consumes` the connection names
    /// its participant type knows.
    ///
    /// Returns the problems found.
    pub(super) fn check_wiring(
        &mut self, name: &str, connect: &BTreeMap<String, ConnectionTarget>, typed: TypeWiring,
        consumes: &BTreeMap<&'static str, Consumed>,
    ) -> Vec<String> {
        let mut wired = BTreeSet::new();
        let mut connections = BTreeSet::new();
        self.visit_templates(|place, template| {
            for reference in template.references() {
                match reference {
                    Reference::Endpoint { holder, endpoint, .. } if place.wires() && holder.names(name) => {
                        wired.insert(endpoint.clone());
                    }
                    Reference::Connection { name: connection, .. } => {
                        connections.insert(connection.clone());
                    }
                    _ => {}
                }
            }
        });

        for (endpoint, planned_endpoint) in &mut self.endpoints {
            if !typed.endpoints.contains(endpoint) {
                continue;
            }
            let hint = typed.hints.get(endpoint);
            if let Some(why) = hint.and_then(|hint| hint.overridden.as_ref()) {
                planned_endpoint.unwired = Some(Unwired {
                    why: why.clone(),
                    write: hint.map(|hint| hint.write.clone()),
                });
                continue;
            }
            if wired.contains(endpoint) {
                continue;
            }
            let replaced = typed
                .env
                .iter()
                .find(|(_, value)| refers_to_own_endpoint(value, name, endpoint));
            planned_endpoint.unwired = Some(match (hint, replaced) {
                (
                    Some(Wiring {
                        withdrawn: Some(withdrawn),
                        write,
                        ..
                    }),
                    _,
                ) => Unwired {
                    why: format!("{}, and nothing references the endpoint.", withdrawn),
                    write: Some(write.clone()),
                },
                (_, Some((key, value))) => Unwired {
                    why: format!(
                    "env sets {} without a reference to it, which replaces the default, and nothing references the \
                     endpoint.",
                    key
                ),
                    write: Some(format!("{}: \"{}\"", key, value)),
                },
                (hint, None) => Unwired {
                    why: "Nothing references the endpoint.".to_string(),
                    write: hint.map(|hint| hint.write.clone()),
                },
            });
        }

        let mut problems = Vec::new();
        for connection in connect.keys() {
            let delivers = consumes
                .get(connection.as_str())
                .is_some_and(|consumed| !consumed.env.is_empty());
            if delivers || connections.contains(connection) {
                continue;
            }
            let because = match &self.participant_type {
                None => "a participant with no participant type is delivered nothing by name".to_string(),
                Some(participant_type) => {
                    let known: Vec<&str> = consumes.keys().copied().collect();
                    format!(
                        "participant type '{}' knows {}",
                        participant_type,
                        if known.is_empty() {
                            "no connection names".to_string()
                        } else {
                            super::case::join(&known)
                        }
                    )
                }
            };
            problems.push(format!(
                "connect.{}: nothing uses this connection: {}, and nothing refers to '{{{{ self.connections.{}.<part> \
             }}}}'",
                connection, because, connection
            ));
        }
        problems
    }
}

fn refers_to_own_endpoint(template: &Template, participant: &str, endpoint: &str) -> bool {
    template.references().any(|reference| {
        matches!(reference, Reference::Endpoint { holder, endpoint: named, .. }
            if holder.names(participant) && named == endpoint)
    })
}

/// Plans every connection of the case: checks it, gives it its route and its address, and reports each endpoint
/// that connections use but its participant does not wire. `consumes` holds, by participant, the connection names its
/// participant type knows.
pub(super) fn plan_connections(
    case: &LoadedCase, participants: &mut BTreeMap<String, PlannedParticipant>,
    consumes: &BTreeMap<String, BTreeMap<&'static str, Consumed>>, problems: &mut Problems<'_>,
) {
    let mut planned = Vec::new();
    let mut unwired: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (consumer_name, consumer) in participants.iter() {
        for (name, target) in &case.participants[consumer_name].placement.connect {
            // A server that failed to plan has reported its own problems.
            let Some(server) = participants.get(&target.participant) else {
                continue;
            };
            let consumed = consumes.get(consumer_name).and_then(|known| known.get(name.as_str()));
            match PlannedConnection::plan(consumer, name, consumed, target, server) {
                Ok(connection) => planned.push((consumer_name.clone(), name.clone(), connection)),
                Err(message) => problems.push(
                    Some(format!("participant '{}'", consumer_name)),
                    format!("connect.{}: {}", name, message),
                ),
            }
            if server
                .endpoints
                .get(&target.endpoint)
                .is_some_and(|endpoint| endpoint.unwired.is_some())
            {
                unwired
                    .entry((target.participant.clone(), target.endpoint.clone()))
                    .or_default()
                    .push(format!("participant '{}' connect.{}", consumer_name, name));
            }
        }
    }
    for (consumer, name, connection) in planned {
        if let Some(participant) = participants.get_mut(&consumer) {
            participant.connections.insert(name, connection);
        }
    }

    for ((server_name, endpoint), uses) in unwired {
        let server = &participants[&server_name];
        let Some(unwired) = &server.endpoints[&endpoint].unwired else {
            continue;
        };
        let mut message = format!("endpoint '{}' is not wired.\n  {}", endpoint, unwired.why);
        if let Some(write) = &unwired.write {
            message.push_str(&format!("\n  Write: {}", write));
        }
        message.push_str(&format!("\n  used by: {}", uses.join(", ")));
        let at = format!(
            "participant '{}' ({}, {})",
            server_name,
            server.participant_type.as_deref().unwrap_or("no participant type"),
            server.realization.enclosure
        );
        problems.push(Some(at), message);
    }
}

impl PlannedConnection {
    /// Plans the connection `name` from `consumer` to `target` on `server`. `consumed` is what the consumer's
    /// participant type delivers for the name, if it knows it.
    fn plan(
        consumer: &PlannedParticipant, name: &str, consumed: Option<&Consumed>, target: &ConnectionTarget,
        server: &PlannedParticipant,
    ) -> Result<Self, String> {
        let endpoint = server.endpoints.get(&target.endpoint).ok_or_else(|| {
            format!(
                "participant '{}' has no endpoint '{}' as a {}",
                target.participant, target.endpoint, server.realization.enclosure
            )
        })?;
        if let Some(consumed) = consumed.filter(|consumed| !consumed.accepts.contains(&endpoint.protocol)) {
            let accepts: Vec<&str> = consumed.accepts.iter().map(|protocol| protocol.as_str()).collect();
            return Err(format!(
                "participant type '{}' takes {} for '{}', and {} is {}",
                consumer.participant_type.as_deref().unwrap_or_default(),
                accepts.join(" or "),
                name,
                target,
                endpoint.protocol.as_str()
            ));
        }
        let route = route(endpoint.protocol, consumer.realization, server.realization)?;
        let scheme = endpoint.protocol.scheme(endpoint.tls);
        let (url, reached) = match (route, &endpoint.address) {
            (Route::ByName | Route::Loopback, PlannedAddress::Port(port)) => {
                let host = match route {
                    Route::Loopback => "127.0.0.1".to_string(),
                    _ => target.participant.clone(),
                };
                (
                    format!("{}://{}:{}", scheme, host, port),
                    Reached::Host { host, port: *port },
                )
            }
            (Route::SharedDirectory, PlannedAddress::Path(path)) => {
                check_shared_socket(path, &target.participant, server.realization)?;
                (format!("{}://{}", scheme, path), Reached::Path(path.clone()))
            }
            (_, address) => {
                return Err(format!(
                    "endpoint '{}' at '{}' has no address its route reaches",
                    target.endpoint, address
                ))
            }
        };
        Ok(Self {
            target: target.clone(),
            protocol: endpoint.protocol,
            route,
            url,
            reached,
            delivers: consumed
                .map(|consumed| consumed.env.iter().map(|(key, _)| *key).collect())
                .unwrap_or_default(),
        })
    }
}

/// Checks that a connected socket is under its server's own directory.
fn check_shared_socket(path: &str, server: &str, realization: Realization) -> Result<(), String> {
    let facts = realization.enclosure.facts();
    let Some(dir) = facts.dir(server) else {
        return Err(format!("a {} has no shared directory at plan", realization.enclosure));
    };
    if !path.starts_with(&format!("{}{}", dir, facts.separator)) {
        return Err(format!(
            "socket '{}' is not under {}, the server's own directory, which is the one every participant of the case \
             sees",
            path, dir
        ));
    }
    Ok(())
}

/// What each consumer waits for before it starts, from its connections.
pub(super) fn gates(participants: &BTreeMap<String, PlannedParticipant>) -> BTreeMap<String, Vec<Gate>> {
    let mut gates: BTreeMap<String, Vec<Gate>> = BTreeMap::new();
    for (consumer, participant) in participants {
        let targets: BTreeSet<&ConnectionTarget> = participant
            .connections
            .values()
            .map(|connection| &connection.target)
            .collect();
        for target in targets {
            let Some(endpoint) = participants
                .get(&target.participant)
                .and_then(|server| server.endpoints.get(&target.endpoint))
            else {
                continue;
            };
            gates.entry(consumer.clone()).or_default().push(Gate {
                target: target.clone(),
                evidence: endpoint.evidence.clone(),
                reach: endpoint.reach,
            });
        }
    }
    gates
}
