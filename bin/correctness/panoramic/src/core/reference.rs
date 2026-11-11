//! References: checking where each may sit, and the one resolver that fills them in.
//!
//! A reference is text written in a case field, inside double braces, that stands for a value the harness owns:
//! `{{ self.endpoints.api.port }}` in an environment value, `{{ self.connections.intake.url }}` in a template file,
//! `{{ self.dir }}` in a socket path. It names an endpoint or an identity of a participant (`self` is the one that
//! holds the text), or one of its own connections: a [`Reference`], with the syntax in [`super::template`]. Resolving
//! replaces it with the value as text: a port number, a path, a URL, an IP address.
//!
//! Planning knows every endpoint, connection and directory, so plan compile fills those in. A participant's IP address
//! and container ID exist only once it is created or started. A reference is accepted in a place only when its value
//! exists by the time the place is fixed: at create for the command line, environment, files and readiness patterns,
//! and when a step runs for its named commands. A participant is created after every participant whose container ID it
//! names in a place fixed at create.
//!
//! The resolver runs at plan compile, at create and when a step runs, each time with more values known. The plan
//! records each template it cannot fill yet, with the participants it waits for: once the last of them is created or
//! has started, every reference in the template has a value.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;

use super::participant::{Enclosure, Realization};
use super::plan::{Launch, PlannedParticipant, Problems};
use super::spec::{Contents, Evidence, Protocol};
use super::template::{ConnectionPart, EndpointPart, IdentityPart, Reference, Template};
use super::wiring::{PlannedAddress, Reached};

/// Where one template sits in a participant.
#[derive(Clone, Debug)]
pub(crate) enum Place {
    /// An element of `command`, by index.
    Command(usize),
    /// An element of `args`, by index.
    Args(usize),
    /// A program's argument, by program name and index.
    ProgramArg(String, usize),
    /// A program's environment entry, by program name and key.
    ProgramEnv(String, String),
    Env(String),
    /// A template file, by destination.
    File(String),
    /// The output pattern that is an endpoint's readiness evidence, by endpoint.
    Ready(String),
    /// A named command's argument, by command name and index.
    CommandArg(String, usize),
    /// A named command's environment entry, by command name and key.
    CommandEnv(String, String),
}

impl Place {
    /// Whether a reference here to the participant's own endpoint wires it.
    pub(crate) fn wires(&self) -> bool {
        !matches!(self, Self::Ready(_) | Self::CommandArg(..) | Self::CommandEnv(..))
    }

    /// Whether the place is fixed when the participant is created, rather than when a step runs.
    fn fixed_at_create(&self) -> bool {
        !matches!(self, Self::CommandArg(..) | Self::CommandEnv(..))
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(index) => write!(f, "command[{}]", index),
            Self::Args(index) => write!(f, "args[{}]", index),
            Self::ProgramArg(name, index) => write!(f, "programs.{}.args[{}]", name, index),
            Self::ProgramEnv(name, key) => write!(f, "programs.{}.env.{}", name, key),
            Self::Env(key) => write!(f, "env.{}", key),
            Self::File(destination) => write!(f, "files.{}", destination),
            Self::Ready(endpoint) => write!(f, "ready.{}", endpoint),
            Self::CommandArg(name, index) => write!(f, "commands.{}.argv[{}]", name, index),
            Self::CommandEnv(name, key) => write!(f, "commands.{}.env.{}", name, key),
        }
    }
}

/// What a participant must have done before a value of it that planning does not know becomes known.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Later {
    Created,
    Started,
}

impl PlannedParticipant {
    /// Calls `visit` with every piece of text of the participant that may hold a reference, and its place.
    pub(crate) fn visit_templates<F>(&mut self, mut visit: F)
    where
        F: FnMut(&Place, &mut Template),
    {
        match &mut self.launch {
            Launch::Image { command, args, .. } => {
                for (index, template) in command.iter_mut().flatten().enumerate() {
                    visit(&Place::Command(index), template);
                }
                for (index, template) in args.iter_mut().flatten().enumerate() {
                    visit(&Place::Args(index), template);
                }
            }
            Launch::Programs(programs) => {
                for program in programs {
                    for (index, template) in program.args.iter_mut().enumerate() {
                        visit(&Place::ProgramArg(program.name.clone(), index), template);
                    }
                    for (key, template) in &mut program.env {
                        visit(&Place::ProgramEnv(program.name.clone(), key.clone()), template);
                    }
                }
            }
        }
        for (key, template) in &mut self.env {
            visit(&Place::Env(key.clone()), template);
        }
        for file in &mut self.files {
            if let Contents::Template(template) = &mut file.contents {
                visit(&Place::File(file.destination.clone()), template);
            }
        }
        for (name, endpoint) in &mut self.endpoints {
            if let Evidence::Output { pattern, .. } = &mut endpoint.evidence {
                visit(&Place::Ready(name.clone()), pattern);
            }
        }
        for (name, command) in &mut self.commands {
            for (index, template) in command.argv.iter_mut().enumerate() {
                visit(&Place::CommandArg(name.clone(), index), template);
            }
            for (key, template) in &mut command.env {
                visit(&Place::CommandEnv(name.clone(), key.clone()), template);
            }
        }
    }
}

/// The values references name, as far as they are known.
pub(crate) struct Values {
    participants: BTreeMap<String, Known>,
}

/// What is known of one participant.
struct Known {
    realization: Realization,
    endpoints: BTreeMap<String, (Protocol, PlannedAddress)>,
    connections: BTreeMap<String, (String, Reached)>,
}

/// A value as far as it is known.
enum Lookup {
    Known(String),
    /// Known once the named participant has been created or has started.
    Later(String, Later),
}

impl Values {
    /// The values a plan knows: addresses, connections and directories.
    fn of_plan(participants: &BTreeMap<String, PlannedParticipant>) -> Self {
        let participants = participants
            .iter()
            .map(|(name, participant)| {
                let known = Known {
                    realization: participant.realization,
                    endpoints: participant
                        .endpoints
                        .iter()
                        .map(|(name, endpoint)| (name.clone(), (endpoint.protocol, endpoint.address.clone())))
                        .collect(),
                    connections: participant
                        .connections
                        .iter()
                        .map(|(name, connection)| (name.clone(), (connection.url.clone(), connection.reached.clone())))
                        .collect(),
                };
                (name.clone(), known)
            })
            .collect();
        Self { participants }
    }

    /// The text of `reference` held by `holder`, if it is known.
    fn text(&self, holder: &str, reference: &Reference) -> Option<String> {
        match self.lookup(holder, reference) {
            Ok(Lookup::Known(text)) => Some(text),
            Ok(Lookup::Later(..)) | Err(_) => None,
        }
    }

    /// Looks up `reference`, held by `holder`.
    fn lookup(&self, holder: &str, reference: &Reference) -> Result<Lookup, String> {
        let known = |name: &str| {
            self.participants
                .get(name)
                .ok_or_else(|| format!("names participant '{}', which the case does not have", name))
        };
        match reference {
            Reference::Endpoint {
                holder: owner,
                endpoint,
                part,
            } => {
                let owner = owner.resolve(holder);
                let (protocol, address) = known(owner)?.endpoints.get(endpoint).ok_or_else(|| {
                    format!(
                        "names endpoint '{}', which participant '{}' does not have",
                        endpoint, owner
                    )
                })?;
                match (part, address) {
                    (EndpointPart::Port, PlannedAddress::Port(port)) => Ok(Lookup::Known(port.to_string())),
                    (EndpointPart::Path, PlannedAddress::Path(path)) => Ok(Lookup::Known(path.clone())),
                    (EndpointPart::Path, PlannedAddress::Pipe(name)) => {
                        Ok(Lookup::Known(format!("\\\\.\\pipe\\{}", name)))
                    }
                    (EndpointPart::Name, PlannedAddress::Pipe(name)) => Ok(Lookup::Known(name.clone())),
                    _ => Err(format!("names a part that {} endpoints do not have", protocol.as_str())),
                }
            }
            Reference::Connection { name, part } => {
                let (url, reached) = known(holder)?
                    .connections
                    .get(name)
                    .ok_or_else(|| format!("names connection '{}', which the participant does not make", name))?;
                match (part, reached) {
                    (ConnectionPart::Url, _) => Ok(Lookup::Known(url.clone())),
                    (ConnectionPart::Host, Reached::Host { host, .. }) => Ok(Lookup::Known(host.clone())),
                    (ConnectionPart::Port, Reached::Host { port, .. }) => Ok(Lookup::Known(port.to_string())),
                    (ConnectionPart::Path, Reached::Path(path)) => Ok(Lookup::Known(path.clone())),
                    _ => Err("names a part that the connected endpoint does not have".to_string()),
                }
            }
            Reference::Identity { holder: owner, part } => {
                let owner = owner.resolve(holder);
                let enclosure = known(owner)?.realization.enclosure;
                let started = || Ok(Lookup::Later(owner.to_string(), Later::Started));
                match (part, enclosure) {
                    (IdentityPart::Dir, enclosure) => enclosure
                        .facts()
                        .dir(owner)
                        .map(Lookup::Known)
                        .ok_or_else(|| format!("has no value: a {} has no directory at plan yet", enclosure)),
                    (IdentityPart::Ip, Enclosure::HostProcessGroup) => Ok(Lookup::Known("127.0.0.1".to_string())),
                    (IdentityPart::Ip, _) => started(),
                    (IdentityPart::ContainerId, Enclosure::LinuxContainer | Enclosure::WindowsContainer) => {
                        Ok(Lookup::Later(owner.to_string(), Later::Created))
                    }
                    (IdentityPart::ContainerId, Enclosure::KubernetesPod) => started(),
                    (IdentityPart::PodUid, Enclosure::KubernetesPod) => started(),
                    (IdentityPart::ContainerId | IdentityPart::PodUid, enclosure) => {
                        Err(format!("never has a value: participant '{}' is a {}", owner, enclosure))
                    }
                }
            }
        }
    }
}

/// Checks every reference of every participant against its place, fills in what planning knows, and records the rest.
///
/// Returns the create order's edges: each pair is a participant created before another.
pub(super) fn fill_references(
    participants: &mut BTreeMap<String, PlannedParticipant>, problems: &mut Problems<'_>,
) -> BTreeSet<(String, String)> {
    let values = Values::of_plan(participants);
    let mut edges = BTreeSet::new();
    for (name, participant) in participants.iter_mut() {
        let mut errors = Vec::new();
        let mut later = BTreeMap::new();
        participant.visit_templates(|place, template| {
            let mut waits: BTreeMap<String, Later> = BTreeMap::new();
            for reference in template.references() {
                if let Reference::Endpoint { holder, part, .. } = reference {
                    if !holder.names(name) {
                        let part = match part {
                            EndpointPart::Port => "port",
                            EndpointPart::Path | EndpointPart::Name => "path",
                        };
                        errors.push(format!(
                            "{}: '{}' names another participant's endpoint: connect to it and write \
                             '{{{{ self.connections.<name>.{} }}}}'",
                            place, reference, part
                        ));
                        continue;
                    }
                }
                let (of, when) = match values.lookup(name, reference) {
                    Ok(Lookup::Known(_)) => continue,
                    Ok(Lookup::Later(of, when)) => (of, when),
                    Err(message) => {
                        errors.push(format!("{}: '{}' {}", place, reference, message));
                        continue;
                    }
                };
                if place.fixed_at_create() {
                    match when {
                        Later::Created if of != *name => {
                            edges.insert((of.clone(), name.clone()));
                        }
                        Later::Created => errors.push(format!(
                            "{}: '{}' has a value once the participant is created, and {} is fixed when it is created",
                            place, reference, place
                        )),
                        Later::Started => errors.push(format!(
                            "{}: '{}' has a value once participant '{}' starts, and {} is fixed when participant \
                             '{}' is created",
                            place, reference, of, place, name
                        )),
                    }
                }
                let wait = waits.entry(of).or_insert(when);
                *wait = (*wait).max(when);
            }
            *template = template.resolve(false, |reference| values.text(name, reference));
            if !waits.is_empty() {
                later.insert(place.to_string(), waits);
            }
        });
        participant.later = later;
        for message in errors {
            problems.push(Some(format!("participant '{}'", name)), message);
        }
    }
    edges
}
