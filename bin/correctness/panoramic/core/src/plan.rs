//! Compiles loaded cases for a selected subject operating system.

use std::collections::BTreeMap;

use saluki_error::{generic_error, GenericError};

use crate::{
    backend,
    case::{
        config_references::FileTemplates, declared_names, CaseConfig, CaseFile, CaseSchema, ProcedureNode, Registries,
    },
    layout::{describe_participant, describe_realizations, lay_out, Layout, Os, Realization},
    participant::{find_type, CaseValues, EffectiveConfig, ParticipantType, SHARED_DIR_FACT},
    record::step_label,
    step::{Need, Step},
};

mod connections;
mod endpoints;
mod order;
mod ports;
mod references;
mod routes;
mod scope;

pub(crate) use self::scope::ScopeLayout;
use self::{
    connections::check_connections,
    endpoints::derive_endpoints,
    order::{dependency_order, DependencyOrder},
    ports::{check_host_listeners, RunnerHostPorts},
    scope::participant_shared_dir,
};
pub use self::{ports::PortRegistry, scope::ScopeRegistry};

/// A loaded case with the layout selected for execution.
pub struct CasePlan<C> {
    case: CaseConfig<C>,
    subject_os: Os,
    layout: Option<Layout>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Read when backends prepare isolation scopes.")
    )]
    scope: Option<ScopeLayout>,
    order: Option<DependencyOrder>,
    participants: BTreeMap<String, PlannedParticipant>,
    /// Ports the runner host publishes for Linux container endpoints reached by host process groups,
    /// one per endpoint for the selected subject OS, keyed by serving participant and endpoint.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Read when the Linux container backend publishes runner-host ports."
        )
    )]
    runner_host_ports: BTreeMap<(String, String), u16>,
}

struct PlannedCase {
    participants: BTreeMap<String, PlannedParticipant>,
    order: DependencyOrder,
    runner_host_ports: BTreeMap<(String, String), u16>,
}

struct PlannedParticipant {
    facts: BTreeMap<String, String>,
    config: EffectiveConfig,
}

impl<C> CasePlan<C> {
    /// Validates every declared subject OS, returning a plan only for a declared selection.
    /// Participant cases carry a layout; typed cases retain their loader-supplied data.
    /// A participant case declaring the selected OS draws one isolation scope from `scopes`.
    /// Only the selected OS draws runner-host ports from `ports`; other OSes use separate registries.
    /// Ports are drawn in check order: assigned listeners per participant and endpoint in name order,
    /// then connections per consumer, then references per participant (`env`, `params`, template files).
    /// Connections and references to one container endpoint share a runner-host port.
    ///
    /// # Errors
    ///
    /// Returns the first failing check, naming the case and source, and for per-OS checks the failing subject OS.
    /// Participant cases are checked in this order; typed cases are not checked:
    ///
    /// 1. Isolation scope allocation, when the case declares the selected OS.
    /// 2. On each declared subject OS in the order the case lists them, even when the selected OS is excluded:
    ///    1. Layout.
    ///    2. Participant facts: no type may declare `shared_dir`.
    ///    3. Per participant in name order: fact and late-fact references in `env`, `params`, then template files;
    ///       configuration; endpoint derivation; port assignment and re-derivation; endpoint overrides; then
    ///       case-supplied ports and socket paths in endpoint name order.
    ///    4. Runner-host listener range and transport uniqueness, per participant and endpoint in name order.
    ///    5. Connections per consumer in name order. For each `connect` key in name order: consumed endpoint,
    ///       served endpoint, accepted kind, route, socket location and length, delivered env keys, then address
    ///       allocation and delivered values. Then required endpoints in name order.
    ///    6. Endpoint and connection references in configuration, per participant in name order (`env`, `params`,
    ///       then template files): served endpoint, route, socket location and length, address part, then
    ///       runner-host port allocation.
    ///    7. Create order, then start order.
    ///    8. Step fact and late-fact references in procedure order, naming the step.
    pub fn compile(
        case: CaseConfig<C>, subject_os: Os, registries: &Registries<'_>, ports: &mut PortRegistry,
        scopes: &mut ScopeRegistry,
    ) -> Result<Option<Self>, GenericError> {
        let selected = case.subject_os().contains(&subject_os);
        let mut layout = None;
        let mut scope = None;
        let mut order = None;
        let mut participants = BTreeMap::new();
        let mut runner_host_ports = BTreeMap::new();
        if let CaseSchema::Participants { file, procedure } = case.schema() {
            let candidate_scope = if selected {
                scopes.allocate().map_err(|error| {
                    generic_error!(
                        "case '{}' in {}: cannot allocate an isolation scope: {error}",
                        case.name(),
                        case.source().display()
                    )
                })?
            } else {
                scopes.unallocated()
            };
            for &os in case.subject_os() {
                let error = |message: String| {
                    generic_error!(
                        "case '{}' in {}: on subject OS '{}', {}",
                        case.name(),
                        case.source().display(),
                        os.name(),
                        message
                    )
                };
                let candidate = lay_out(&file.participants, registries.participant_types, os).map_err(&error)?;
                let mut scratch_ports = PortRegistry::new();
                let os_ports = if os == subject_os {
                    &mut *ports
                } else {
                    &mut scratch_ports
                };
                let planned = plan_participants(
                    file,
                    case.file_templates(),
                    &candidate,
                    &candidate_scope,
                    registries.participant_types,
                    os_ports,
                )
                .map_err(&error)?;
                check_reference_needs(
                    file,
                    procedure,
                    &candidate,
                    registries.participant_types,
                    &planned.participants,
                )
                .map_err(error)?;
                if os == subject_os {
                    layout = Some(candidate);
                    order = Some(planned.order);
                    participants = planned.participants;
                    runner_host_ports = planned.runner_host_ports;
                }
            }
            if selected {
                scope = Some(candidate_scope);
            }
        }
        if !selected {
            return Ok(None);
        }
        Ok(Some(Self {
            case,
            subject_os,
            layout,
            scope,
            order,
            participants,
            runner_host_ports,
        }))
    }

    /// Returns the loaded case and its metadata.
    pub fn case(&self) -> &CaseConfig<C> {
        &self.case
    }

    /// Returns the subject OS selected for execution.
    pub fn subject_os(&self) -> Os {
        self.subject_os
    }

    /// Returns `Some(layout)` for participant cases and `None` for typed cases.
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Returns create dependency levels for participant cases, or `None` for typed cases.
    ///
    /// A participant follows every other participant whose late fact, known once created, its configuration embeds.
    /// Each level lists independent participants in name order.
    pub fn create_order(&self) -> Option<&[Vec<String>]> {
        self.order.as_ref().map(|order| order.create.as_slice())
    }

    /// Returns start dependency levels for participant cases, or `None` for typed cases.
    ///
    /// A participant follows every participant it connects to. A pod also follows every other participant
    /// whose late fact its template files reference when that fact becomes available only after start.
    /// Each level lists independent participants in name order.
    pub fn start_order(&self) -> Option<&[Vec<String>]> {
        self.order.as_ref().map(|order| order.start.as_slice())
    }

    /// Returns a fact from the participant's planned realization, including `shared_dir`.
    /// Returns `None` for unknown participants or facts, and for typed cases.
    pub fn fact(&self, participant: &str, fact: &str) -> Option<&str> {
        self.participants.get(participant)?.facts.get(fact).map(String::as_str)
    }

    /// Returns the participant's effective configuration for its planned realization.
    /// Returns `None` for unknown participants and typed cases.
    pub fn effective_config(&self, participant: &str) -> Option<&EffectiveConfig> {
        self.participants
            .get(participant)
            .map(|participant| &participant.config)
    }
}

/// Computes every participant's facts, including `shared_dir`, rejecting types that declare it.
/// Then builds effective configurations (assigning host process group ports), checks endpoints, runner-host listeners
/// and connections, merges connection deliveries and checks configuration address references in participant name order.
/// Then computes create and start orders.
fn plan_participants(
    file: &CaseFile, templates: &FileTemplates, layout: &Layout, scope: &ScopeLayout, types: &[&dyn ParticipantType],
    ports: &mut PortRegistry,
) -> Result<PlannedCase, String> {
    let mut facts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (name, realization) in layout.iter() {
        let entry = &file.participants[name];
        let participant_type =
            find_type(types, &entry.participant_type).expect("laid-out participants have registered types");
        let mut participant_facts = BTreeMap::new();
        for fact in participant_type.facts(realization) {
            if fact.name == SHARED_DIR_FACT {
                return Err(format!(
                    "{} declares fact 'shared_dir', which panoramic provides for every participant",
                    describe_participant(name, &entry.participant_type, realization)
                ));
            }
            participant_facts.insert(fact.name.into(), fact.value.into());
        }
        let root = backend::shared_root(realization.kind, realization.os, scope);
        participant_facts.insert(
            SHARED_DIR_FACT.into(),
            participant_shared_dir(&root, name, realization.os),
        );
        facts.insert(name.to_string(), participant_facts);
    }
    let mut configs = BTreeMap::new();
    let mut participant_endpoints = BTreeMap::new();
    for (name, realization) in layout.iter() {
        let entry = &file.participants[name];
        let participant_type =
            find_type(types, &entry.participant_type).expect("laid-out participants have registered types");
        let holder = describe_participant(name, &entry.participant_type, realization);
        let mut values = CaseValues::for_os(&entry.config, realization.os);
        references::resolve_config_references(file, layout, types, &facts, name, &mut values)?;
        references::check_file_references(file, templates, layout, types, &facts, name, &values)?;
        let (config, endpoints) = derive_endpoints(
            &holder,
            participant_type,
            realization,
            values,
            facts[name][SHARED_DIR_FACT].clone(),
            ports,
        )?;
        participant_endpoints.insert(name.to_string(), endpoints);
        configs.insert(name.to_string(), config);
    }
    let mut participants: BTreeMap<_, _> = facts
        .into_iter()
        .zip(configs.into_values())
        .map(|((name, facts), config)| (name, PlannedParticipant { facts, config }))
        .collect();
    check_host_listeners(file, layout, &participant_endpoints)?;
    let mut runner_host_ports = RunnerHostPorts::new(ports);
    let deliveries = check_connections(file, layout, &participant_endpoints, &mut runner_host_ports)?;
    for (name, env) in deliveries {
        participants
            .get_mut(&name)
            .expect("consumers are planned participants")
            .config
            .env
            .extend(env);
    }
    references::check_address_references(file, templates, layout, &participant_endpoints, &mut runner_host_ports)?;
    let order = dependency_order(file, templates, layout, &participant_endpoints)?;
    Ok(PlannedCase {
        participants,
        order,
        runner_host_ports: runner_host_ports.into_ports(),
    })
}

/// Rejects the first step needing an unavailable fact or late fact.
fn check_reference_needs(
    file: &CaseFile, procedure: &[ProcedureNode<Box<dyn Step>>], layout: &Layout, types: &[&dyn ParticipantType],
    participants: &BTreeMap<String, PlannedParticipant>,
) -> Result<(), String> {
    for (index, node) in procedure.iter().enumerate() {
        let (steps, parallel) = match node {
            ProcedureNode::Step(step) => (std::slice::from_ref(step), false),
            ProcedureNode::Parallel(steps) => (steps.as_slice(), true),
        };
        for (member, step) in steps.iter().enumerate() {
            for need in step.needs() {
                let (participant, fact) = match need {
                    Need::Fact(participant, fact) => (participant, fact),
                    Need::Late(participant, fact) => {
                        let realization = layout.get(&participant).expect("step needs name laid-out participants");
                        let facts = backend::late_facts(realization.kind);
                        if !facts.iter().any(|(provided, _)| *provided == fact) {
                            let kind = realization.kind.name();
                            return Err(format!(
                                "step {} references late fact '{}' of participant '{participant}', which a {kind} does not have; a {kind} has {}",
                                step_label(index, parallel.then_some(member)),
                                fact.name(),
                                declared_names(&facts.iter().map(|(fact, _)| fact.name()).collect::<Vec<_>>()),
                            ));
                        }
                        continue;
                    }
                    Need::Handle(_) | Need::Process(_, _) => continue,
                };
                if participants[&participant].facts.contains_key(&fact) {
                    continue;
                }
                let realization = layout.get(&participant).expect("step needs name laid-out participants");
                let participant_type = find_type(types, &file.participants[&participant].participant_type)
                    .expect("laid-out participants have registered types");
                let provided =
                    describe_realizations(participant_type.supports().iter().copied().filter(|&(os, kind)| {
                        participant_type
                            .facts(Realization { os, kind })
                            .iter()
                            .any(|declaration| declaration.name == fact)
                    }));
                return Err(format!(
                    "step {} references fact '{fact}' of participant '{participant}', which type '{}' does not provide as {} on {}; it provides it as {provided}",
                    step_label(index, parallel.then_some(member)),
                    participant_type.name(),
                    realization.kind.name(),
                    realization.os.name(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use super::*;
    use crate::{
        case::{
            fixtures::{
                write_case, Client, Configured, FixtureConfig, FixtureTypes, PodServer, Server, LOADER,
                PARTICIPANT_FIELDS, REGISTRIES, STEPS,
            },
            CaseLoader, FileSource,
        },
        endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Port},
        layout::{Realization, RealizationKind},
        participant::FactDeclaration,
    };

    pub(super) struct EveryKindServer;

    impl ParticipantType for EveryKindServer {
        fn name(&self) -> &'static str {
            "every-kind-server"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::Linux, RealizationKind::HostProcessGroup),
                (Os::MacOs, RealizationKind::HostProcessGroup),
                (Os::Windows, RealizationKind::WindowsContainer),
                (Os::Linux, RealizationKind::KubernetesPod),
            ]
        }

        fn params(&self) -> &'static [&'static str] {
            &["mode"]
        }

        fn endpoints(
            &self, _realization: Realization, config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            let offset = config
                .env
                .get("PORT_OFFSET")
                .map(|value| value.parse::<u16>().unwrap())
                .unwrap_or(0);
            let socket_dir = config.env.get("SOCKET_DIR").map(String::as_str).unwrap_or("/run/every");
            Ok([
                ("tcp", Endpoint::tcp(Port::Fixed(1001 + offset))),
                ("udp", Endpoint::udp(Port::Fixed(1001 + offset))),
                ("http", Endpoint::http(Port::Fixed(1003 + offset))),
                ("grpc", Endpoint::grpc(Port::Fixed(1004 + offset))),
                (
                    "datagram",
                    Endpoint::unix_datagram(format!("{socket_dir}/datagram.sock")),
                ),
                ("stream", Endpoint::unix_stream(format!("{socket_dir}/stream.sock"))),
                ("pipe", Endpoint::named_pipe("every-pipe")),
            ]
            .into_iter()
            .map(|(name, endpoint)| (name.into(), endpoint))
            .collect())
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    pub(super) struct EveryKindClient;

    impl ParticipantType for EveryKindClient {
        fn name(&self) -> &'static str {
            "every-kind-client"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            EveryKindServer.supports()
        }

        fn consumes(&self, _realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            BTreeMap::from([(
                "target".into(),
                ConsumedEndpoint {
                    accepts: vec![
                        EndpointKind::Tcp,
                        EndpointKind::Udp,
                        EndpointKind::Http,
                        EndpointKind::Grpc,
                        EndpointKind::UnixDatagram,
                        EndpointKind::UnixStream,
                        EndpointKind::NamedPipe,
                    ],
                    required: false,
                    delivery: Delivery::Env {
                        keys: BTreeSet::from(["TARGET".into()]),
                        value: match config.env.get("DELIVERY").map(String::as_str) {
                            Some("wrong") => |address| BTreeMap::from([("OTHER".into(), address.url.clone())]),
                            Some("missing") => |_| BTreeMap::new(),
                            Some("extra") => |address| {
                                BTreeMap::from([
                                    ("TARGET".into(), address.url.clone()),
                                    ("OTHER".into(), address.url.clone()),
                                ])
                            },
                            _ => |address| BTreeMap::from([("TARGET".into(), address.url.clone())]),
                        },
                    },
                },
            )])
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    pub(super) struct FileServer;

    impl ParticipantType for FileServer {
        fn name(&self) -> &'static str {
            "file-server"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            EveryKindServer.supports()
        }

        fn config_files(&self) -> &'static [&'static str] {
            &["settings.yaml", "z.yaml"]
        }

        fn params(&self) -> &'static [&'static str] {
            &["mode"]
        }

        fn validate_config(&self, _realization: Realization, config: &CaseValues) -> Result<(), String> {
            if config.env.get("PORT").is_some_and(|value| value == "broken") {
                return Err("PORT 'broken' is not a number".into());
            }
            Ok(())
        }

        fn endpoints(
            &self, realization: Realization, config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            let mut endpoints = EveryKindServer.endpoints(realization, config)?;
            if config.env.get("TCP").is_some_and(|value| value == "off") {
                endpoints.remove("tcp");
            }
            if config.env.get("CASE_PORT").is_some_and(|value| value == "required") {
                endpoints.insert("tcp".into(), Endpoint::tcp(Port::FromCase));
            }
            if let Some(path) = config.env.get("SOCKET_PATH") {
                endpoints.insert("stream".into(), Endpoint::unix_stream(path));
            }
            if let Some(mode) = config.params.get("mode").and_then(serde_json::Value::as_str) {
                let path = match mode {
                    "late-path" => Some("{{self.late.ip}}/stream.sock"),
                    "malformed-path" => Some("{{unfinished"),
                    _ => None,
                };
                if let Some(path) = path {
                    endpoints.insert("stream".into(), Endpoint::unix_stream(path));
                }
            }
            Ok(endpoints)
        }

        fn consumes(&self, realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            EveryKindClient.consumes(realization, config)
        }

        fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
            Server.facts(realization)
        }

        fn substituted_late_facts(&self, _realization: Realization) -> &'static [backend::LateFact] {
            &[backend::LateFact::Ip]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    pub(super) const FILE_REGISTRIES: Registries<'_> = Registries {
        participant_types: &[&FileServer, &Server],
        steps: &[],
    };
    pub(super) const FILE_CONFIG: &str = "{files: {settings.yaml: {from: source.yaml, template: true}}}";

    pub(super) fn compile_files(
        yaml: &str, sources: &[(&str, &str)], os: Os,
    ) -> Result<CasePlan<FixtureConfig>, String> {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", yaml);
        for (from, text) in sources {
            std::fs::write(source.parent().unwrap().join(from), text).unwrap();
        }
        let loader = CaseLoader::new(FILE_REGISTRIES, FixtureTypes, "fixture");
        let normalize = |error: saluki_error::GenericError| {
            error
                .to_string()
                .replace(&source.display().to_string(), "cases/alpha/config.yaml")
        };
        let case = loader
            .load_content(yaml, "alpha", &source)
            .map_err(normalize)?
            .remove(0);
        // Planning uses the template contents captured at load.
        for (from, _) in sources {
            std::fs::remove_file(source.parent().unwrap().join(from)).unwrap();
        }
        CasePlan::compile(
            case,
            os,
            &FILE_REGISTRIES,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .map_err(normalize)
        .map(Option::unwrap)
    }

    struct AssignedServer;

    impl ParticipantType for AssignedServer {
        fn name(&self) -> &'static str {
            "assigned-server"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::Linux, RealizationKind::HostProcessGroup),
                (Os::MacOs, RealizationKind::HostProcessGroup),
            ]
        }

        fn params(&self) -> &'static [&'static str] {
            &["mode"]
        }

        fn default_env(&self, realization: Realization, _config: &CaseValues) -> BTreeMap<String, String> {
            if realization.kind == RealizationKind::HostProcessGroup {
                BTreeMap::from([("API_URL".into(), "default".into())])
            } else {
                BTreeMap::new()
            }
        }

        fn endpoints(
            &self, realization: Realization, config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            let mode = config.params.get("mode").and_then(serde_json::Value::as_str);
            let api_port = if matches!(mode, Some("keep-assigned" | "assign-anywhere")) {
                None
            } else {
                config.env.get("API_PORT").map(|port| port.parse::<u16>().unwrap())
            };
            let api = match api_port {
                Some(port) => Port::Fixed(port + u16::from(mode == Some("wrong-port"))),
                None if realization.kind == RealizationKind::HostProcessGroup || mode == Some("assign-anywhere") => {
                    Port::Assigned {
                        deliver: |port| {
                            BTreeMap::from([
                                ("API_PORT".into(), port.to_string()),
                                ("API_URL".into(), format!("http://0.0.0.0:{port}")),
                            ])
                        },
                    }
                }
                None => Port::Fixed(8080),
            };
            let metrics = match config.env.get("METRICS_PORT") {
                Some(port) => Port::Fixed(port.parse().unwrap()),
                None if realization.kind == RealizationKind::HostProcessGroup => Port::Assigned {
                    deliver: if mode == Some("shared-key") {
                        |port| BTreeMap::from([("API_PORT".into(), port.to_string())])
                    } else {
                        |port| BTreeMap::from([("METRICS_PORT".into(), port.to_string())])
                    },
                },
                None => Port::Fixed(9000),
            };
            let mut endpoints = BTreeMap::from([
                ("api".into(), Endpoint::http(api)),
                ("metrics".into(), Endpoint::udp(metrics)),
            ]);
            if config.env.contains_key("API_PORT") {
                match mode {
                    Some("extra-after-assignment") => {
                        endpoints.insert("extra".into(), Endpoint::tcp(Port::Fixed(8082)));
                    }
                    Some("remove-after-assignment") => {
                        endpoints.remove("api");
                    }
                    Some("change-kind") => {
                        endpoints.insert("api".into(), Endpoint::grpc(api));
                    }
                    Some("remove-metrics-after-assignment") => {
                        endpoints.remove("metrics");
                    }
                    Some("change-metrics-kind") => {
                        endpoints.insert("metrics".into(), Endpoint::tcp(metrics));
                    }
                    Some("from-case-after-assignment") => {
                        endpoints.insert("api".into(), Endpoint::http(Port::FromCase));
                    }
                    Some("error-after-assignment") => return Err("cannot read delivered port".into()),
                    Some("assign-another") => {
                        endpoints.insert(
                            "metrics".into(),
                            Endpoint::udp(Port::Assigned {
                                deliver: |_| BTreeMap::new(),
                            }),
                        );
                    }
                    _ => {}
                }
            }
            Ok(endpoints)
        }

        fn consumes(&self, _realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            let delivery = match config.params.get("mode").and_then(serde_json::Value::as_str) {
                Some("consume-assigned-key") => Delivery::Env {
                    keys: BTreeSet::from(["API_PORT".into()]),
                    value: |address| BTreeMap::from([("API_PORT".into(), address.port.unwrap().to_string())]),
                },
                Some("consume-after-assignment") if config.env.contains_key("API_PORT") => Delivery::Env {
                    keys: BTreeSet::from(["UPSTREAM_URL".into()]),
                    value: |address| BTreeMap::from([("UPSTREAM_URL".into(), address.url.clone())]),
                },
                _ => return BTreeMap::new(),
            };
            BTreeMap::from([(
                "upstream".into(),
                ConsumedEndpoint {
                    accepts: vec![EndpointKind::Http],
                    required: true,
                    delivery,
                },
            )])
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    struct SharedDeliveryClient;

    impl ParticipantType for SharedDeliveryClient {
        fn name(&self) -> &'static str {
            "shared-delivery-client"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            Client.supports()
        }

        fn default_env(&self, _realization: Realization, _config: &CaseValues) -> BTreeMap<String, String> {
            BTreeMap::from([("SHARED".into(), "default".into())])
        }

        fn consumes(&self, _realization: Realization, _config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            ["a", "z"]
                .into_iter()
                .map(|name| {
                    (
                        name.into(),
                        ConsumedEndpoint {
                            accepts: vec![EndpointKind::Http],
                            required: false,
                            delivery: Delivery::Env {
                                keys: BTreeSet::from(["SHARED".into()]),
                                value: |address| BTreeMap::from([("SHARED".into(), address.url.clone())]),
                            },
                        },
                    )
                })
                .collect()
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    const FACT_CASE: &str = "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: [{step: probe, on: subject, path: '{{ subject.facts.file }}'}]\n";

    const CONFIG_REGISTRIES: Registries<'static> = Registries {
        participant_types: &[
            &Client,
            &Configured,
            &Server,
            &EveryKindClient,
            &EveryKindServer,
            &FileServer,
            &SharedDeliveryClient,
            &AssignedServer,
        ],
        steps: STEPS,
    };
    const CONFIG_LOADER: CaseLoader<'static, FixtureTypes> =
        CaseLoader::new(CONFIG_REGISTRIES, FixtureTypes, "fixture-participants");

    pub(super) fn compile(yaml: &str, os: Os) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
        compile_with_ports(yaml, os, &mut PortRegistry::new())
    }

    pub(super) fn compile_with_ports(
        yaml: &str, os: Os, ports: &mut PortRegistry,
    ) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
        let case = CONFIG_LOADER
            .load_content(yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        CasePlan::compile(
            case,
            os,
            &CONFIG_REGISTRIES,
            ports,
            &mut ScopeRegistry::with_run("testrun0"),
        )
    }

    #[test]
    fn every_participant_has_a_shared_dir_fact_for_its_realization() {
        for (os, kind, expected) in [
            (Os::Linux, "linux-container", "/airlock/subject/shared"),
            (Os::Windows, "windows-container", r"C:\airlock\subject\shared"),
            (
                Os::MacOs,
                "host-process-group",
                "/tmp/panoramic-testrun0/0000/subject/shared",
            ),
            (
                Os::Linux,
                "host-process-group",
                "/tmp/panoramic-testrun0/0000/subject/shared",
            ),
            (Os::Linux, "kubernetes-pod", "/airlock/subject/shared"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n  subject: {{type: every-kind-server, subject: true, realization: {{kind: {kind}}}}}\n  other: {{type: every-kind-client}}\nprocedure: []", os.name());
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.fact("subject", SHARED_DIR_FACT), Some(expected));
            assert_eq!(
                plan.fact("other", SHARED_DIR_FACT),
                Some(
                    expected
                        .replace("/subject/", "/other/")
                        .replace(r"\subject\", r"\other\")
                        .as_str()
                )
            );
        }
    }

    fn compile_with_scopes(
        yaml: &str, os: Os, scopes: &mut ScopeRegistry,
    ) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
        let case = CONFIG_LOADER
            .load_content(yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        CasePlan::compile(case, os, &CONFIG_REGISTRIES, &mut PortRegistry::new(), scopes)
    }

    #[test]
    fn each_participant_case_declaring_the_selected_os_draws_one_scope() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]");
        let mut scopes = ScopeRegistry::with_run("testrun0");
        for (index, os) in [Os::Linux, Os::MacOs, Os::Windows].into_iter().enumerate() {
            let plan = compile_with_scopes(&yaml, os, &mut scopes).unwrap().unwrap();
            let scope = plan.scope.unwrap();
            assert_eq!(usize::from(scope.index), index);
            assert_eq!(scope.run, "testrun0");
            if os == Os::MacOs {
                assert_eq!(
                    plan.participants["subject"].facts[SHARED_DIR_FACT],
                    "/tmp/panoramic-testrun0/0001/subject/shared"
                );
            }
        }
        assert_eq!(scopes.allocate().unwrap().index, 3);
    }

    #[test]
    fn a_case_excluding_the_selected_os_and_a_typed_case_draw_no_scope() {
        let mut scopes = ScopeRegistry::with_run("testrun0");
        assert!(compile_with_scopes(PARTICIPANT_FIELDS, Os::MacOs, &mut scopes)
            .unwrap()
            .is_none());
        let plan = compile_with_scopes("type: fixture", Os::MacOs, &mut scopes)
            .unwrap()
            .unwrap();
        assert!(plan.scope.is_none());
        assert_eq!(plan.fact("subject", SHARED_DIR_FACT), None);
        assert_eq!(scopes.allocate().unwrap().index, 0);
    }

    #[test]
    fn scope_exhaustion_names_the_case_and_precedes_layout_checks() {
        let mut scopes = ScopeRegistry::with_run("testrun0");
        for _ in 0..10000 {
            scopes.allocate().unwrap();
        }
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        assert_eq!(compile_with_scopes(&yaml, Os::Linux, &mut scopes).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: cannot allocate an isolation scope: all 10000 isolation scopes are allocated");
        assert!(compile_with_scopes(PARTICIPANT_FIELDS, Os::MacOs, &mut scopes)
            .unwrap()
            .is_none());
        assert!(compile_with_scopes("type: fixture", Os::MacOs, &mut scopes)
            .unwrap()
            .unwrap()
            .scope
            .is_none());
    }

    #[test]
    fn a_type_declaring_shared_dir_fails_compile_naming_the_type_and_realization() {
        struct ReservedFact;
        impl ParticipantType for ReservedFact {
            fn name(&self) -> &'static str {
                "reserved"
            }
            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                EveryKindServer.supports()
            }
            fn facts(&self, _: Realization) -> &'static [FactDeclaration] {
                &[FactDeclaration {
                    name: SHARED_DIR_FACT,
                    value: "/wrong",
                }]
            }
            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }
        let registries = Registries {
            participant_types: &[&ReservedFact, &EveryKindServer],
            steps: &[],
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture");
        for (os, kind) in [
            (Os::Linux, "linux-container"),
            (Os::MacOs, "host-process-group"),
            (Os::Windows, "windows-container"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n  a: {{type: every-kind-server, subject: true, config: {{env: {{KEY: '{{{{ self.facts.missing }}}}'}}}}}}\n  z: {{type: reserved}}\nprocedure: []", os.name());
            let case = loader
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let error = CasePlan::compile(
                case,
                os,
                &registries,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .err()
            .unwrap()
            .to_string();
            assert_eq!(error, format!("case 'alpha' in cases/alpha/config.yaml: on subject OS '{}', participant 'z' of type 'reserved' as {kind} on {} declares fact 'shared_dir', which panoramic provides for every participant", os.name(), os.name()));
        }
    }

    #[test]
    fn shared_dir_substitutes_into_env_params_and_served_socket_paths() {
        let yaml = "timeout: 1s\nsubject_os: [linux, macos]\nparticipants:\n  client: {type: file-server, subject: true, connect: {target: server.stream}, config: {env: {COPY: '{{ server.facts.shared_dir }}', PORT_OFFSET: '100'}, params: {mode: [{nested: '{{ server.facts.shared_dir }}'}]}}}\n  server: {type: file-server, config: {env: {SOCKET_PATH: '{{ self.facts.shared_dir }}/stream.sock'}}}\nprocedure: []";
        for (os, dir) in [
            (Os::Linux, "/airlock/server/shared"),
            (Os::MacOs, "/tmp/panoramic-testrun0/0000/server/shared"),
        ] {
            let plan = compile(yaml, os).unwrap().unwrap();
            let config = plan.effective_config("client").unwrap();
            assert_eq!(config.env["COPY"], dir);
            assert_eq!(config.params["mode"][0]["nested"], dir);
            assert_eq!(config.env["TARGET"], format!("unix://{dir}/stream.sock"));
            assert_eq!(
                plan.effective_config("server").unwrap().env["SOCKET_PATH"],
                format!("{dir}/stream.sock")
            );
        }
        let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-server, subject: true, config: {FILE_CONFIG}}}\n  other: {{type: server}}\nprocedure: []");
        let plan = compile_files(
            &yaml,
            &[(
                "source.yaml",
                "self: {{ self.facts.shared_dir }}\nother: {{ other.facts.shared_dir }}",
            )],
            Os::Linux,
        )
        .unwrap();
        assert_eq!(plan.fact("other", SHARED_DIR_FACT), Some("/airlock/other/shared"));
    }

    #[test]
    fn a_step_may_reference_any_participants_shared_dir() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]")
            .replace("participants:", "participants:\n  other: {type: every-kind-client}")
            .replace("procedure: []", "procedure: [{step: probe, on: subject, path: '{{ subject.facts.shared_dir }}/{{ other.facts.shared_dir }}'}]");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            let CaseSchema::Participants { procedure, .. } = plan.case().schema() else {
                panic!("participant case")
            };
            let ProcedureNode::Step(step) = &procedure[0] else {
                panic!("single step")
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("subject".into()),
                    Need::Fact("subject".into(), SHARED_DIR_FACT.into()),
                    Need::Fact("other".into(), SHARED_DIR_FACT.into())
                ]
            );
        }
    }

    fn socket_case(config: &str) -> String {
        format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  client: {{type: every-kind-client, subject: true, connect: {{target: server.stream}}}}\n  server: {{type: every-kind-server, config: {config}}}\nprocedure: []")
    }

    #[test]
    fn a_connected_socket_outside_the_servers_shared_dir_fails_naming_both_participants() {
        for (endpoint, kind) in [("stream", "unix-stream"), ("datagram", "unix-datagram")] {
            let yaml = socket_case("{}").replace("server.stream", &format!("server.{endpoint}"));
            assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'target' to 'server.{endpoint}', a {kind} socket that participant 'server' of type 'every-kind-server' as linux-container on linux serves at '/run/every/{endpoint}.sock', outside its shared_dir '/airlock/server/shared'; consumed sockets must lie under their server's shared_dir"
            ));
        }
    }

    #[test]
    fn a_socket_referenced_by_another_participant_outside_its_shared_dir_fails() {
        let yaml = socket_case("{}").replace(
            "connect: {target: server.stream}",
            "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
        );
        let expected = "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'every-kind-client' as linux-container on linux environment variable 'SOCKET' '{{ server.endpoints.stream.path }}' references endpoint 'stream' of participant 'server', a unix-stream socket served at '/run/every/stream.sock', outside its shared_dir '/airlock/server/shared'; consumed sockets must lie under their server's shared_dir";
        assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(), expected);
        let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-server, subject: true, config: {FILE_CONFIG}}}\n  server: {{type: file-server}}\nprocedure: []");
        let expected = expected
            .replace(
                "'client' of type 'every-kind-client'",
                "'subject' of type 'file-server'",
            )
            .replace("environment variable 'SOCKET'", "configuration file 'settings.yaml'");
        assert_eq!(
            compile_files(
                &yaml,
                &[("source.yaml", "{{ server.endpoints.stream.path }}")],
                Os::Linux
            )
            .err()
            .unwrap(),
            expected
        );
    }

    #[test]
    fn a_participant_may_reference_its_own_socket_anywhere() {
        for participant in ["self", "subject"] {
            let yaml = PARTICIPANT_FIELDS.replace("type: server", &format!("type: every-kind-server, config: {{env: {{SOCKET_DIR: '/dev/null/{}', COPY: '{{{{ {participant}.endpoints.stream.path }}}}'}}}}", "x".repeat(108)));
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("subject").unwrap().env["COPY"],
                format!("{{{{ {participant}.endpoints.stream.path }}}}")
            );
        }
    }

    #[test]
    fn an_unconsumed_socket_may_lie_anywhere() {
        let yaml = PARTICIPANT_FIELDS
            .replace(
                "type: server",
                "type: every-kind-server, config: {env: {SOCKET_DIR: '/dev/null'}}",
            )
            .replace(
                "participants:",
                "participants:\n  other: {type: every-kind-client, connect: {target: subject.tcp}}",
            );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(plan.effective_config("subject").unwrap().env["SOCKET_DIR"], "/dev/null");
        assert_eq!(
            plan.effective_config("other").unwrap().env["TARGET"],
            "tcp://subject:1001"
        );
    }

    #[test]
    fn a_consumed_socket_path_over_the_os_limit_fails_and_one_at_the_limit_passes() {
        for (os, dir, limit) in [
            (Os::MacOs, "/tmp/panoramic-testrun0/0000/server/shared", 103),
            (Os::Linux, "/airlock/server/shared", 107),
        ] {
            for reference in [false, true] {
                for length in [limit, limit + 1] {
                    let path = format!("{dir}/{}", "x".repeat(length - dir.len() - 1));
                    let mut yaml = socket_case(&format!("{{env: {{SOCKET_PATH: '{path}'}}}}"))
                        .replace("type: every-kind-server", "type: file-server")
                        .replace("[linux]", &format!("[{}]", os.name()));
                    if reference {
                        yaml = yaml.replace(
                            "connect: {target: server.stream}",
                            "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
                        );
                    }
                    let result = compile(&yaml, os);
                    if length == limit {
                        let plan = result.unwrap().unwrap();
                        assert_eq!(plan.effective_config("server").unwrap().env["SOCKET_PATH"], path);
                    } else {
                        let message = result.err().unwrap().to_string();
                        assert!(
                            message.ends_with(&format!(
                                "at '{path}' ({length} bytes); a socket path on {} holds at most {limit} bytes",
                                os.name()
                            )),
                            "{message}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn socket_location_errors_fail_on_an_unselected_subject_os() {
        let yaml = socket_case(
            "{env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}, os: {macos: {env: {SOCKET_DIR: /elsewhere}}}}",
        )
        .replace("[linux]", "[linux, macos]");
        for reference in [false, true] {
            let yaml = if reference {
                yaml.replace(
                    "connect: {target: server.stream}",
                    "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
                )
            } else {
                yaml.clone()
            };
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let mut scopes = ScopeRegistry::with_run("testrun0");
                for _ in 0..3 {
                    scopes.allocate().unwrap();
                }
                let message = compile_with_scopes(&yaml, os, &mut scopes).err().unwrap().to_string();
                let index = if os == Os::Windows { 0 } else { 3 };
                assert!(message.contains("on subject OS 'macos'"), "{message}");
                assert!(
                    message.contains(&format!(
                        "outside its shared_dir '/tmp/panoramic-testrun0/{index:04}/server/shared'"
                    )),
                    "{message}"
                );
                assert_eq!(scopes.allocate().unwrap().index, if os == Os::Windows { 3 } else { 4 });
            }
        }
    }

    #[test]
    fn socket_length_errors_fail_on_an_unselected_subject_os() {
        let path = format!(
            "{{{{ self.facts.shared_dir }}}}/{}",
            "x".repeat(104 - "/tmp/panoramic-testrun0/0000/server/shared/".len())
        );
        let yaml = socket_case(&format!("{{env: {{SOCKET_PATH: '{path}'}}}}"))
            .replace("type: every-kind-server", "type: file-server")
            .replace("[linux]", "[linux, macos]");
        for reference in [false, true] {
            let yaml = if reference {
                yaml.replace(
                    "connect: {target: server.stream}",
                    "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
                )
            } else {
                yaml.clone()
            };
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let message = compile(&yaml, os).err().unwrap().to_string();
                assert!(message.contains("on subject OS 'macos'"), "{message}");
                assert!(
                    message.ends_with("(104 bytes); a socket path on macos holds at most 103 bytes"),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn a_plan_holds_each_participants_effective_config_for_the_selected_subject_os() {
        let yaml = r#"timeout: 1s
subject_os: [windows, linux]
participants:
  subject:
    type: configured
    subject: true
    config:
      env: {LEVEL: debug, EXTRA: base}
      os: {windows: {env: {LEVEL: trace, EXTRA: windows}}}
  other: {type: configured, config: {env: {PORT: "2"}}}
procedure: []
"#;
        for (os, level, extra) in [(Os::Linux, "debug", "base"), (Os::Windows, "trace", "windows")] {
            let plan = compile(yaml, os).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("subject"),
                Some(&EffectiveConfig {
                    env: BTreeMap::from([
                        ("LEVEL".into(), level.into()),
                        ("PORT".into(), "1".into()),
                        ("EXTRA".into(), extra.into()),
                    ]),
                    ..EffectiveConfig::default()
                })
            );
            assert_eq!(
                plan.effective_config("other"),
                Some(&EffectiveConfig {
                    env: BTreeMap::from([("LEVEL".into(), "info".into()), ("PORT".into(), "2".into())]),
                    ..EffectiveConfig::default()
                })
            );
            assert_eq!(plan.effective_config("unknown"), None);
        }
    }

    #[test]
    fn a_participant_with_its_own_os_gets_the_overlay_for_that_os() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "participants:",
            "participants:\n  other: {type: configured, os: linux, config: {env: {LEVEL: debug}, os: {linux: {env: {LEVEL: trace}}}}}",
        );
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("other").unwrap().env,
            BTreeMap::from([("LEVEL".into(), "trace".into()), ("PORT".into(), "1".into()),])
        );
        assert_eq!(plan.effective_config("subject"), Some(&EffectiveConfig::default()));
    }

    #[test]
    fn type_defaults_see_the_overlaid_case_values() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]").replace(
            "type: server, subject: true",
            "type: configured, subject: true, config: {os: {windows: {files: {settings.yaml: {from: windows.yaml, template: true}}}}}",
        );
        let source = write_case(dir.path(), "alpha", &yaml);
        std::fs::write(source.parent().unwrap().join("windows.yaml"), "port: 7").unwrap();
        for os in [Os::Linux, Os::Windows] {
            let case = CONFIG_LOADER.load_file(&source).unwrap().remove(0);
            let plan = CasePlan::compile(
                case,
                os,
                &CONFIG_REGISTRIES,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .unwrap()
            .unwrap();
            let effective = plan.effective_config("subject").unwrap();
            assert_eq!(effective.env["LEVEL"], "info");
            if os == Os::Windows {
                assert_eq!(effective.env.get("PORT"), None);
                assert_eq!(
                    effective.files,
                    BTreeMap::from([(
                        "settings.yaml".into(),
                        FileSource {
                            from: "windows.yaml".into(),
                            template: true,
                        }
                    )])
                );
            } else {
                assert_eq!(effective.env.get("PORT").map(String::as_str), Some("1"));
                assert!(effective.files.is_empty());
            }
        }
    }

    #[test]
    fn a_config_its_type_rejects_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        for config in [
            "{params: {mode: broken}}",
            "{params: {mode: working}, os: {windows: {params: {mode: broken}}}}",
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[windows, linux]").replace(
                "type: server, subject: true",
                &format!("type: configured, subject: true, config: {config}"),
            );
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'subject' of type 'configured' as windows-container on windows has invalid configuration: mode 'broken' is unavailable on windows"
                );
            }
        }
    }

    #[test]
    fn compile_reports_layout_configuration_endpoint_connection_and_fact_errors_in_that_order() {
        let mut yaml = FACT_CASE
            .replace("[linux]", "[windows]")
            .replace("subject: true", "subject: true, config: {env: {METRICS: 'on'}}")
            .replace(
                "participants:",
                r#"participants:
  z: {type: server, realization: {kind: linux-container}}
  b: {type: configured, config: {params: {mode: broken}}}
  a: {type: configured, config: {params: {mode: broken}}}
  c: {type: server, config: {env: {PORT: x}}}
  d: {type: server, config: {endpoints: {z-typo: {port: 80}, api: {port: 80}, a-typo: {port: 80}}}}
  d1: {type: file-server, config: {env: {CASE_PORT: required}, params: {mode: late-path}, endpoints: {pipe: {port: 80}, tcp: {port: 0}}}}
  e: {type: client, config: {env: {SIDE_PORT: '80', SIDE_HOST: literal}}, connect: {z-typo: subject.typo, a-typo: subject.typo}}
  e1: {type: every-kind-client, config: {env: {DELIVERY: wrong, TARGET: literal}}, connect: {target: subject.api}}
  f: {type: client, connect: {typo: subject.api}}
  g: {type: server, config: {endpoints: {typo: {port: 80}}}}
  other: {type: server, os: macos}"#,
            )
            .replace("subject.facts.file", "other.facts.file");
        for (bad, fix, expected) in [
            (
                ", realization: {kind: linux-container}",
                "",
                "participant 'z' of type 'server' cannot run as linux-container on windows",
            ),
            (
                "a: {type: configured, config: {params: {mode: broken}}}",
                "a: {type: configured}",
                "participant 'a' of type 'configured' as windows-container on windows has invalid configuration",
            ),
            (
                "b: {type: configured, config: {params: {mode: broken}}}",
                "b: {type: configured}",
                "participant 'b' of type 'configured' as windows-container on windows has invalid configuration",
            ),
            (
                "env: {PORT: x}",
                "env: {PORT: '80'}",
                "participant 'c' of type 'server' as windows-container on windows has invalid configuration: PORT 'x' is not a number",
            ),
            (
                ", a-typo: {port: 80}",
                "",
                "participant 'd' of type 'server' as windows-container on windows overrides endpoint 'a-typo'",
            ),
            (
                ", api: {port: 80}",
                "",
                "participant 'd' of type 'server' as windows-container on windows overrides endpoint 'api', whose port its type derives from its configuration",
            ),
            (
                "z-typo: {port: 80}",
                "",
                "participant 'd' of type 'server' as windows-container on windows overrides endpoint 'z-typo'",
            ),
            (
                "pipe: {port: 80}, ",
                "",
                "participant 'd1' of type 'file-server' as windows-container on windows overrides endpoint 'pipe', whose pipe name its type derives from its configuration",
            ),
            (
                "tcp: {port: 0}",
                "",
                "participant 'd1' of type 'file-server' as windows-container on windows overrides endpoint 'tcp' with port 0",
            ),
            (
                "mode: late-path",
                "mode: working",
                "participant 'd1' of type 'file-server' as windows-container on windows serves endpoint 'stream' at '{{self.late.ip}}/stream.sock', which holds a reference resolved only after planning",
            ),
            (
                "CASE_PORT: required",
                "",
                "participant 'd1' of type 'file-server' as windows-container on windows serves endpoint 'tcp', whose port the case must give as config.endpoints.tcp.port",
            ),
            (
                "g: {type: server, config: {endpoints: {typo: {port: 80}}}}",
                "g: {type: server}",
                "participant 'g' of type 'server' as windows-container on windows overrides endpoint 'typo'",
            ),
            (
                ", a-typo: subject.typo",
                "",
                "participant 'e' of type 'client' as windows-container on windows connects 'a-typo', which it does not consume",
            ),
            (
                "z-typo: subject.typo",
                "side: subject.typo",
                "participant 'e' of type 'client' as windows-container on windows connects 'z-typo', which it does not consume",
            ),
            (
                "side: subject.typo",
                "side: other.api",
                "participant 'e' connects 'side' to 'subject.typo', which participant 'subject' of type 'server' as windows-container on windows does not serve",
            ),
            (
                "side: other.api",
                "side: other.metrics",
                "participant 'e' connects 'side' to 'other.api', an endpoint of kind http; 'side' of type 'client' accepts udp, tcp",
            ),
            (
                "side: other.metrics",
                "side: subject.metrics",
                "participant 'e' connects 'side' to 'other.metrics', an endpoint of kind udp; participant 'e' of type 'client' as windows-container on windows cannot reach participant 'other' of type 'server' as host-process-group on macos: network endpoints connect only participants with the same realization, or a host-process-group to a linux-container",
            ),
            (
                ", SIDE_HOST: literal",
                "",
                "participant 'e' of type 'client' as windows-container on windows sets env 'SIDE_HOST', which its connection 'side' delivers; remove one of them",
            ),
            (
                "SIDE_PORT: '80'",
                "",
                "participant 'e' of type 'client' as windows-container on windows sets env 'SIDE_PORT', which its connection 'side' delivers; remove one of them",
            ),
            (
                "side: subject.metrics",
                "side: subject.metrics, upstream: subject.api",
                "participant 'e' of type 'client' as windows-container on windows does not connect required endpoint 'upstream'",
            ),
            (
                ", TARGET: literal",
                "",
                "participant 'e1' of type 'every-kind-client' as windows-container on windows sets env 'TARGET', which its connection 'target' delivers",
            ),
            (
                "DELIVERY: wrong",
                "",
                "participant 'e1' of type 'every-kind-client' as windows-container on windows delivers connection 'target' as env OTHER, but its type declares TARGET",
            ),
            (
                "f: {type: client, connect: {typo: subject.api}}",
                "f: {type: client}",
                "participant 'f' of type 'client' as windows-container on windows connects 'typo', which it does not consume",
            ),
            (
                "f: {type: client}",
                "f: {type: client, connect: {upstream: subject.api}}",
                "participant 'f' of type 'client' as windows-container on windows does not connect required endpoint 'upstream'",
            ),
            (
                "other.facts.file",
                "subject.facts.file",
                "step 1 references fact 'file' of participant 'other'",
            ),
        ] {
            let message = compile(&yaml, Os::Windows).err().unwrap().to_string();
            assert!(message.contains(expected), "{message}");
            assert!(yaml.contains(bad));
            yaml = yaml.replace(bad, fix);
        }
        assert_eq!(
            compile(&yaml, Os::Windows).unwrap().unwrap().fact("subject", "file"),
            Some(r"C:\logs\output.log")
        );
    }

    #[test]
    fn compile_checks_address_references_then_create_order_then_start_order_then_step_facts() {
        let registries = Registries {
            steps: STEPS,
            ..FILE_REGISTRIES
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let mut yaml = r#"timeout: 1s
subject_os: [linux]
participants:
  subject:
    type: file-server
    subject: true
    config: {env: {ADDRESS: '{{fixture.endpoints.missing.url}}', ID: '{{fixture.late.container_id}}', CASE_PORT: required, TARGET: literal, DELIVERY: wrong}, params: {mode: late-path}, endpoints: {pipe: {port: 80}}}
    connect: {target: fixture.tcp}
  fixture:
    type: file-server
    config: {params: {mode: '{{subject.late.container_id}}'}}
    connect: {target: subject.tcp}
  observer: {type: file-server, os: macos}
procedure: [{step: probe, on: subject, path: '{{observer.facts.file}}'}]
"#
        .to_string();
        for (bad, fix, expected) in [
            (
                ", endpoints: {pipe: {port: 80}}",
                "",
                "overrides endpoint 'pipe', whose pipe name its type derives",
            ),
            (
                "mode: late-path",
                "mode: working",
                "serves endpoint 'stream' at '{{self.late.ip}}/stream.sock'",
            ),
            (
                ", CASE_PORT: required",
                "",
                "serves endpoint 'tcp', whose port the case must give",
            ),
            (
                ", TARGET: literal",
                "",
                "sets env 'TARGET', which its connection 'target' delivers",
            ),
            (
                ", DELIVERY: wrong",
                "",
                "delivers connection 'target' as env OTHER, but its type declares TARGET",
            ),
            (
                "{{fixture.endpoints.missing.url}}",
                "literal",
                "references endpoint 'missing'",
            ),
            (
                "{{fixture.late.container_id}}",
                "literal",
                "participants can't be created in any order",
            ),
            (
                "connect: {target: fixture.tcp}",
                "connect: {}",
                "participants can't be started in any order",
            ),
            (
                "{{observer.facts.file}}",
                "literal",
                "step 1 references fact 'file' of participant 'observer'",
            ),
        ] {
            let case = loader
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let message = CasePlan::compile(
                case,
                Os::Linux,
                &registries,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .err()
            .unwrap()
            .to_string();
            assert!(message.contains(expected), "{message}");
            assert!(yaml.contains(bad));
            yaml = yaml.replace(bad, fix);
        }
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(
            case,
            Os::Linux,
            &registries,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.create_order().unwrap(),
            [vec!["observer", "subject"], vec!["fixture"]]
        );
        assert_eq!(
            plan.start_order().unwrap(),
            [vec!["observer", "subject"], vec!["fixture"]]
        );
    }

    #[test]
    fn a_typed_plan_has_no_dependency_orders() {
        let plan = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.create_order(), None);
        assert_eq!(plan.start_order(), None);
    }

    pub(super) const CONNECTION_CASE: &str = r#"timeout: 1s
subject_os: [linux]
participants:
  client: {type: client, subject: true, connect: {upstream: server.api}}
  server: {type: server}
procedure: []
"#;

    pub(super) fn assigned_server_case(config: &str) -> String {
        CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("type: server}", &format!("type: assigned-server, config: {config}}}"))
    }

    #[test]
    fn a_connection_delivering_an_assigned_port_key_fails() {
        let yaml = assigned_server_case("{params: {mode: consume-assigned-key}}")
            .replace(
                "type: assigned-server,",
                "type: assigned-server, connect: {upstream: other.api},",
            )
            .replace("procedure: []", "  other: {type: server}\nprocedure: []");
        assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'assigned-server' as host-process-group on macos delivers env 'API_PORT' through connection 'upstream', which also carries the assigned port of its endpoint 'api'");
        let yaml = yaml.replace(
            "mode: consume-assigned-key}",
            "mode: consume-assigned-key}, env: {API_PORT: '8081'}",
        );
        assert!(compile(&yaml, Os::MacOs)
            .err()
            .unwrap()
            .to_string()
            .contains("sets env 'API_PORT', which its connection 'upstream' delivers"));
    }

    #[test]
    fn a_fixed_case_supplied_or_pinned_host_port_inside_the_runner_host_range_fails() {
        for (participant_type, config, endpoint, port) in [
            ("server", "{env: {PORT: '20000'}}", "api", 20000),
            ("server", "{endpoints: {extra: {port: 25000}}}", "extra", 25000),
            ("assigned-server", "{env: {API_PORT: '29999'}}", "api", 29999),
        ] {
            let yaml =
                assigned_server_case(config).replace("type: assigned-server", &format!("type: {participant_type}"));
            // A custom registry must not change the reserved runner-host range.
            let mut ports = PortRegistry::over(21000..=21010);
            assert_eq!(compile_with_ports(&yaml, Os::MacOs, &mut ports).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type '{participant_type}' as host-process-group on macos serves endpoint '{endpoint}' on runner-host tcp port {port}, inside 20000-29999, where panoramic assigns ports; use a port outside that range"));
            for boundary in [19999, 30000] {
                let plan = compile(&yaml.replace(&port.to_string(), &boundary.to_string()), Os::MacOs)
                    .unwrap()
                    .unwrap();
                if endpoint == "api" {
                    assert_eq!(
                        plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                        format!("http://127.0.0.1:{boundary}")
                    );
                } else {
                    assert_eq!(
                        plan.effective_config("server").unwrap().endpoints["extra"].port,
                        boundary
                    );
                }
            }
        }
        let yaml = assigned_server_case("{env: {METRICS_PORT: '25000'}}");
        assert!(compile(&yaml, Os::MacOs)
            .err()
            .unwrap()
            .to_string()
            .contains("endpoint 'metrics' on runner-host udp port 25000, inside 20000-29999"));
    }

    #[test]
    fn container_ports_inside_the_runner_host_range_are_accepted() {
        let yaml = CONNECTION_CASE.replace(
            "type: server}",
            "type: server, config: {env: {PORT: '20000'}, endpoints: {extra: {port: 25000}}}}",
        );
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(&yaml, Os::Linux, &mut ports).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://server:20000"
        );
        assert_eq!(plan.effective_config("server").unwrap().endpoints["extra"].port, 25000);
        assert_eq!(ports.allocate(), Ok(20000));
    }

    #[test]
    fn two_host_process_groups_serving_one_tcp_port_fail_naming_both() {
        for (other, config, port, earlier) in [
            ("server", "{}", 8080, "api"),
            ("every-kind-server", "{env: {PORT: '1004'}}", 1004, "grpc"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [macos]\nparticipants:\n  z: {{type: server, subject: true, config: {config}}}\n  a: {{type: {other}}}\nprocedure: []\n");
            assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'z' of type 'server' as host-process-group on macos serves endpoint 'api' on runner-host tcp port {port}, which participant 'a' already serves as endpoint '{earlier}'"));
        }
    }

    #[test]
    fn two_endpoints_of_one_host_process_group_on_one_port_and_transport_fail() {
        let yaml = CONNECTION_CASE.replace("[linux]", "[macos]").replace(
            "type: server}",
            "type: server, config: {endpoints: {extra: {port: 8080}}}}",
        );
        assert_eq!(compile(&yaml, Os::MacOs).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'server' of type 'server' as host-process-group on macos serves endpoint 'extra' on runner-host tcp port 8080, which participant 'server' already serves as endpoint 'api'");
    }

    #[test]
    fn a_tcp_and_a_udp_host_listener_may_share_a_port_number() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("type: server}", "type: server, config: {env: {PORT: '9000'}}}")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics");
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(&yaml, Os::MacOs, &mut ports).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([
                ("UPSTREAM_URL".into(), "http://127.0.0.1:9000".into()),
                ("SIDE_HOST".into(), "127.0.0.1".into()),
                ("SIDE_PORT".into(), "9000".into()),
            ])
        );
        assert_eq!(ports.allocate(), Ok(20000));
    }

    #[test]
    fn host_port_conflicts_fail_on_an_unselected_subject_os() {
        let yaml = r#"timeout: 1s
subject_os: [linux, macos]
participants:
  a: {type: server, subject: true}
  z: {type: server, config: {env: {PORT: '8081', METRICS: 'off'}, os: {macos: {env: {PORT: '8080'}}}}}
procedure: []
"#;
        assert_eq!(compile(yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'z' of type 'server' as host-process-group on macos serves endpoint 'api' on runner-host tcp port 8080, which participant 'a' already serves as endpoint 'api'");
    }

    fn host_to_container_case() -> String {
        CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("type: server}", "type: server, os: linux}")
    }

    #[test]
    fn a_connection_between_linux_containers_delivers_the_servers_name_and_port() {
        for port in [8080, 8123] {
            let yaml = CONNECTION_CASE.replace(
                "type: server}",
                &format!("type: server, config: {{env: {{PORT: '{port}'}}}}}}"),
            );
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                format!("http://server:{port}")
            );
        }
    }

    #[test]
    fn a_connection_between_host_process_groups_delivers_a_loopback_address() {
        let plan = compile(&CONNECTION_CASE.replace("[linux]", "[macos]"), Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn a_host_process_group_reaches_a_linux_container_only_through_an_allocated_runner_host_port() {
        let plan = compile(&host_to_container_case(), Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:20000"
        );
    }

    #[test]
    fn a_linux_container_cannot_reach_a_host_process_group_and_no_other_route_is_tried() {
        let yaml = CONNECTION_CASE.replace("type: server}", "type: server, os: macos}");
        let mut ports = PortRegistry::new();
        assert_eq!(compile_with_ports(&yaml, Os::Linux, &mut ports).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'upstream' to 'server.api', an endpoint of kind http; participant 'client' of type 'client' as linux-container on linux cannot reach participant 'server' of type 'server' as host-process-group on macos: network endpoints connect only participants with the same realization, or a host-process-group to a linux-container");
        assert_eq!(ports.allocate(), Ok(20000));
    }

    #[test]
    fn consumers_of_one_container_endpoint_share_its_runner_host_port() {
        let yaml = host_to_container_case().replace(
            "participants:",
            "participants:\n  other: {type: client, connect: {upstream: server.api}}",
        );
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(&yaml, Os::MacOs, &mut ports).unwrap().unwrap();
        for name in ["client", "other"] {
            assert_eq!(
                plan.effective_config(name).unwrap().env["UPSTREAM_URL"],
                "http://127.0.0.1:20000"
            );
        }
        assert_eq!(
            plan.runner_host_ports,
            BTreeMap::from([(("server".into(), "api".into()), 20000)])
        );
        assert_eq!(ports.allocate(), Ok(20001));
    }

    #[test]
    fn tcp_and_udp_endpoints_on_one_port_get_their_own_schemes_and_runner_host_ports() {
        let yaml = "timeout: 1s\nsubject_os: [macos]\nparticipants:\n  z: {type: every-kind-client, connect: {target: server.udp}}\n  a: {type: every-kind-client, subject: true, connect: {target: server.tcp}}\n  server: {type: every-kind-server, os: linux}\nprocedure: []\n";
        let plan = compile(yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("a").unwrap().env["TARGET"],
            "tcp://127.0.0.1:20000"
        );
        assert_eq!(
            plan.effective_config("z").unwrap().env["TARGET"],
            "udp://127.0.0.1:20001"
        );
        assert_eq!(
            plan.runner_host_ports,
            BTreeMap::from([
                (("server".into(), "tcp".into()), 20000),
                (("server".into(), "udp".into()), 20001),
            ])
        );
    }

    #[test]
    fn cases_planned_with_one_registry_get_distinct_runner_host_ports() {
        let mut ports = PortRegistry::new();
        for port in [20000, 20001] {
            let plan = compile_with_ports(&host_to_container_case(), Os::MacOs, &mut ports)
                .unwrap()
                .unwrap();
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                format!("http://127.0.0.1:{port}")
            );
        }
    }

    #[test]
    fn only_the_selected_subject_os_draws_from_the_run_wide_registry() {
        let yaml = "timeout: 1s\nsubject_os: [linux, macos, windows]\nparticipants:\n  subject: {type: every-kind-server, subject: true}\n  client: {type: client, os: macos, connect: {upstream: server.api}}\n  server: {type: server, os: linux}\nprocedure: []\n";
        for selected in [Os::Linux, Os::MacOs, Os::Windows] {
            let mut ports = PortRegistry::new();
            let plan = compile_with_ports(yaml, selected, &mut ports).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
                "http://127.0.0.1:20000"
            );
            assert_eq!(
                plan.runner_host_ports,
                BTreeMap::from([(("server".into(), "api".into()), 20000)])
            );
            assert_eq!(ports.allocate(), Ok(20001));
        }
        let mut ports = PortRegistry::new();
        assert!(compile_with_ports(
            &yaml.replace("[linux, macos, windows]", "[linux, macos]"),
            Os::Windows,
            &mut ports
        )
        .unwrap()
        .is_none());
        assert_eq!(ports.allocate(), Ok(20000));
    }

    fn host_to_container_reference_case(config: &str) -> String {
        format!("timeout: 1s\nsubject_os: [macos]\nparticipants:\n  subject: {{type: file-server, subject: true, config: {config}}}\n  server: {{type: server, os: linux}}\nprocedure: []\n")
    }

    #[test]
    fn a_host_process_groups_endpoint_reference_to_a_linux_container_gets_a_runner_host_port() {
        // Address-reference checks must reserve the port that creation uses to render the reference.
        for (config, sources) in [
            ("{env: {KEY: '{{ server.endpoints.api.url }}'}}", vec![]),
            (
                "{params: {mode: [{nested: ['{{ server.endpoints.api.url }}']}]}}",
                vec![],
            ),
            (FILE_CONFIG, vec![("source.yaml", "{{ server.endpoints.api.url }}")]),
        ] {
            let plan = compile_files(&host_to_container_reference_case(config), &sources, Os::MacOs).unwrap();
            assert_eq!(
                plan.runner_host_ports,
                BTreeMap::from([(("server".into(), "api".into()), 20000)]),
                "{config}"
            );
        }
    }

    #[test]
    fn a_connection_and_a_reference_to_one_container_endpoint_share_its_runner_host_port() {
        let yaml = host_to_container_case()
            .replace("subject: true", "subject: true, config: {env: {KEY: '{{ server.endpoints.api.url }} {{ self.connections.upstream.port }}'}}")
            .replace("procedure: []", "  other: {type: server, config: {params: {mode: ['{{server.endpoints.api.host}}']}}}\nprocedure: []");
        let mut ports = PortRegistry::new();
        let plan = compile_with_ports(&yaml, Os::MacOs, &mut ports).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env["UPSTREAM_URL"],
            "http://127.0.0.1:20000"
        );
        assert_eq!(
            plan.runner_host_ports,
            BTreeMap::from([(("server".into(), "api".into()), 20000)])
        );
        assert_eq!(ports.allocate(), Ok(20001));
    }

    #[test]
    fn references_reaching_an_endpoint_by_name_on_loopback_or_on_the_holder_get_no_runner_host_port() {
        for (os, target, endpoint, part) in [
            (Os::Linux, "server", "http", "url"),
            (Os::MacOs, "server", "http", "url"),
            (Os::Linux, "self", "http", "port"),
            (Os::MacOs, "subject", "http", "host"),
            (Os::Linux, "server", "stream", "path"),
            (Os::MacOs, "server", "datagram", "url"),
            (Os::Windows, "self", "pipe", "path"),
        ] {
            let yaml = host_to_container_reference_case(&format!(
                "{{env: {{KEY: '{{{{ {target}.endpoints.{endpoint}.{part} }}}}'}}}}"
            ))
            .replace("[macos]", &format!("[{}]", os.name()))
            .replace(
                "type: server, os: linux",
                "type: every-kind-server, config: {env: {PORT_OFFSET: '100', SOCKET_DIR: '{{ self.facts.shared_dir }}'}}",
            );
            let mut ports = PortRegistry::new();
            let plan = compile_with_ports(&yaml, os, &mut ports).unwrap().unwrap();
            assert!(plan.runner_host_ports.is_empty(), "{yaml}");
            assert_eq!(ports.allocate(), Ok(20000), "{yaml}");
        }
    }

    #[test]
    fn runner_host_ports_are_drawn_for_connections_then_references_in_participant_name_order() {
        let yaml = r#"timeout: 1s
subject_os: [macos]
participants:
  z: {type: client, subject: true, connect: {upstream: server.api}}
  b: {type: server, config: {env: {PORT: '8081', METRICS: 'off', KEY: '{{ server.endpoints.extra.url }}'}}}
  a: {type: server, config: {env: {KEY: '{{ server.endpoints.metrics.url }}'}}}
  server: {type: server, os: linux, config: {endpoints: {extra: {port: 8123}}}}
procedure: []
"#;
        let plan = compile(yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.runner_host_ports,
            BTreeMap::from([
                (("server".into(), "api".into()), 20000),
                (("server".into(), "metrics".into()), 20001),
                (("server".into(), "extra".into()), 20002),
            ])
        );

        let config = "{env: {Z: '{{server.endpoints.tcp.port}}', A: '{{server.endpoints.udp.host}}'}, params: {mode: [{nested: '{{server.endpoints.grpc.url}}'}]}, files: {settings.yaml: {from: source.yaml, template: true}}}";
        let yaml = host_to_container_reference_case(config).replace("type: server", "type: file-server");
        let plan = compile_files(&yaml, &[("source.yaml", "{{server.endpoints.http.url}}")], Os::MacOs).unwrap();
        assert_eq!(
            plan.runner_host_ports,
            BTreeMap::from([
                (("server".into(), "udp".into()), 20000),
                (("server".into(), "tcp".into()), 20001),
                (("server".into(), "grpc".into()), 20002),
                (("server".into(), "http".into()), 20003),
            ])
        );
    }

    #[test]
    fn an_invalid_address_part_fails_before_a_runner_host_port_is_drawn() {
        let yaml = host_to_container_reference_case("{env: {KEY: '{{ server.endpoints.api.path }}'}}");
        let mut ports = PortRegistry::new();
        assert_eq!(
            compile_with_ports(&yaml, Os::MacOs, &mut ports).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'file-server' as host-process-group on macos environment variable 'KEY' '{{ server.endpoints.api.path }}': a http endpoint has no 'path'; use url, host or port"
        );
        assert_eq!(ports.allocate(), Ok(20000));
    }

    #[test]
    fn runner_host_port_exhaustion_in_a_reference_names_the_reference() {
        let yaml = host_to_container_case().replace(
            "subject: true",
            "subject: true, config: {env: {KEY: '{{ server.endpoints.metrics.url }}'}}",
        );
        let mut ports = PortRegistry::over(21000..=21000);
        assert_eq!(
            compile_with_ports(&yaml, Os::MacOs, &mut ports).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' of type 'client' as host-process-group on macos environment variable 'KEY' '{{ server.endpoints.metrics.url }}' references endpoint 'metrics' of participant 'server', which needs a runner-host port: all 1 runner-host ports are allocated"
        );
    }

    #[test]
    fn only_the_selected_subject_os_records_runner_host_ports() {
        let yaml = host_to_container_reference_case("{os: {macos: {env: {KEY: '{{ server.endpoints.api.url }}'}}}}")
            .replace("[macos]", "[linux, macos]");
        for selected in [Os::Linux, Os::MacOs] {
            let mut ports = PortRegistry::new();
            let plan = compile_with_ports(&yaml, selected, &mut ports).unwrap().unwrap();
            if selected == Os::Linux {
                assert!(plan.runner_host_ports.is_empty());
                assert_eq!(ports.allocate(), Ok(20000));
            } else {
                assert_eq!(
                    plan.runner_host_ports,
                    BTreeMap::from([(("server".into(), "api".into()), 20000)])
                );
                assert_eq!(ports.allocate(), Ok(20001));
            }
        }
    }

    #[test]
    fn runner_host_port_exhaustion_names_the_connection_in_allocation_order() {
        let yaml =
            host_to_container_case().replace("upstream: server.api", "upstream: server.api, side: server.metrics");
        let mut ports = PortRegistry::over(21000..=21000);
        assert_eq!(compile_with_ports(&yaml, Os::MacOs, &mut ports).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' of type 'client' as host-process-group on macos cannot resolve connection 'upstream' to 'server.api': all 1 runner-host ports are allocated");
    }

    #[test]
    fn a_unix_socket_connection_delivers_the_servers_socket_path() {
        for (endpoint, expected) in [
            ("stream", "unix:///airlock/server/shared/stream.sock"),
            ("datagram", "unixgram:///airlock/server/shared/datagram.sock"),
        ] {
            let yaml = CONNECTION_CASE
                .replace("type: client", "type: every-kind-client")
                .replace(
                    "type: server",
                    "type: every-kind-server, config: {env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}}",
                )
                .replace("upstream: server.api", &format!("target: server.{endpoint}"));
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(plan.effective_config("client").unwrap().env["TARGET"], expected);
        }
    }

    #[test]
    fn a_delivery_writing_other_keys_than_it_declares_fails_compile_on_any_declared_subject_os() {
        for (mode, got) in [("wrong", "OTHER"), ("missing", "none"), ("extra", "OTHER, TARGET")] {
            let yaml = CONNECTION_CASE
                .replace("[linux]", "[linux, windows]")
                .replace(
                    "type: client",
                    &format!("type: every-kind-client, config: {{os: {{windows: {{env: {{DELIVERY: {mode}}}}}}}}}"),
                )
                .replace("upstream:", "target:");
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                assert_eq!(compile(&yaml, os).err().unwrap().to_string(), format!(
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' of type 'every-kind-client' as windows-container on windows delivers connection 'target' as env {got}, but its type declares TARGET"));
            }
        }
    }

    #[test]
    fn a_connection_must_name_an_endpoint_its_consumer_consumes() {
        let message = compile(&CONNECTION_CASE.replace("upstream:", "typo:"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as linux-container on linux connects 'typo', which it does not consume; it consumes side, upstream"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_connection_must_name_an_endpoint_its_target_serves_with_its_configuration() {
        let yaml = CONNECTION_CASE
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics")
            .replace("type: server}", "type: server, config: {env: {METRICS: 'off'}}}");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as linux-container on linux does not serve; it serves api"
        );
        assert!(compile(&yaml.replace("METRICS: 'off'", "METRICS: 'on'"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_endpoint_turned_off_by_a_type_default_is_not_served_unless_the_case_env_turns_it_on() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[windows]")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics");
        let message = compile(&yaml, Os::Windows).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as windows-container on windows does not serve; it serves api"
        );
        let yaml = yaml.replace("type: server}", "type: server, config: {env: {METRICS: 'on'}}}");
        assert!(compile(&yaml, Os::Windows).unwrap().is_some());
    }

    #[test]
    fn an_os_overlay_can_turn_off_a_connected_endpoint_and_fail_compile_whatever_os_is_selected() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[linux, macos, windows]")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics")
            .replace(
                "type: server}",
                "type: server, config: {env: {METRICS: 'on'}, os: {windows: {env: {METRICS: 'off'}}}}}",
            );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = compile(&yaml, os).err().unwrap().to_string();
            assert_eq!(message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as windows-container on windows does not serve; it serves api"
            );
            assert!(compile(&yaml.replace("METRICS: 'off'", "METRICS: 'on'"), os)
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn a_connection_to_an_endpoint_of_a_kind_its_consumer_does_not_accept_fails_compile() {
        let message = compile(&CONNECTION_CASE.replace("server.api", "server.metrics"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'upstream' to 'server.metrics', an endpoint of kind udp; 'upstream' of type 'client' accepts http"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_connection_without_a_route_between_its_realizations_fails_compile_naming_both_participants_and_the_constraint()
    {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("type: client", "type: every-kind-client")
            .replace("type: server", "type: every-kind-server, os: linux")
            .replace("upstream: server.api", "target: server.stream");
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' connects 'target' to 'server.stream', an endpoint of kind unix-stream; participant 'client' of type 'every-kind-client' as host-process-group on macos cannot reach participant 'server' of type 'every-kind-server' as linux-container on linux: unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"
        );
        assert!(compile(&yaml.replace("server.stream", "server.tcp"), Os::MacOs)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_connection_without_a_route_on_one_declared_subject_os_fails_compile_whatever_os_is_selected() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[linux, macos]")
            .replace("type: client", "type: every-kind-client")
            .replace(
                "type: server",
                "type: every-kind-server, os: linux, config: {env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}}",
            )
            .replace("upstream: server.api", "target: server.stream");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = compile(&yaml, os).err().unwrap().to_string();
            assert_eq!(message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' connects 'target' to 'server.stream', an endpoint of kind unix-stream; participant 'client' of type 'every-kind-client' as host-process-group on macos cannot reach participant 'server' of type 'every-kind-server' as linux-container on linux: unix sockets connect only participants with the same realization, other than windows-container, because they need one shared filesystem"
            );
        }
        assert!(compile(&yaml.replace("[linux, macos]", "[linux]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_case_env_key_a_connection_delivers_fails_compile() {
        let yaml = CONNECTION_CASE.replace("type: client", "type: client, config: {env: {UPSTREAM_URL: literal}}");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as linux-container on linux sets env 'UPSTREAM_URL', which its connection 'upstream' delivers; remove one of them"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn an_os_overlay_env_key_a_connection_delivers_fails_compile_whatever_os_is_selected() {
        let yaml = CONNECTION_CASE.replace("[linux]", "[linux, windows]").replace(
            "type: client",
            "type: client, config: {os: {windows: {env: {UPSTREAM_URL: literal}}}}",
        );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = compile(&yaml, os).err().unwrap().to_string();
            assert_eq!(message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participant 'client' of type 'client' as windows-container on windows sets env 'UPSTREAM_URL', which its connection 'upstream' delivers; remove one of them"
            );
        }
        assert!(compile(&yaml.replace("UPSTREAM_URL: literal", ""), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_delivery_conflict_uses_the_consumers_own_os_overlay() {
        let yaml = CONNECTION_CASE
            .replace(
                "type: client, subject: true",
                "type: client, os: macos, config: {os: {macos: {env: {UPSTREAM_URL: literal}}}}",
            )
            .replace("type: server", "type: server, subject: true");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as host-process-group on macos sets env 'UPSTREAM_URL', which its connection 'upstream' delivers; remove one of them"
        );
        assert!(compile(&yaml.replace("UPSTREAM_URL: literal", ""), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_case_may_set_the_delivery_keys_of_an_endpoint_it_does_not_connect() {
        let yaml = CONNECTION_CASE.replace(
            "type: client",
            "type: client, config: {env: {SIDE_HOST: literal, SIDE_PORT: '80'}}",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([
                ("SIDE_HOST".into(), "literal".into()),
                ("SIDE_PORT".into(), "80".into()),
                ("UPSTREAM_URL".into(), "http://server:8080".into()),
            ])
        );
    }

    #[test]
    fn a_type_default_for_a_delivered_key_is_not_a_conflict() {
        let yaml = CONNECTION_CASE
            .replace("type: client", "type: shared-delivery-client")
            .replace("upstream:", "a:");
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            plan.effective_config("client").unwrap().env,
            BTreeMap::from([("SHARED".into(), "http://server:8080".into())])
        );
        let message = compile(
            &yaml.replace("subject: true", "subject: true, config: {env: {SHARED: literal}}"),
            Os::Linux,
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'shared-delivery-client' as linux-container on linux sets env 'SHARED', which its connection 'a' delivers; remove one of them"
        );
    }

    #[test]
    fn two_connections_of_one_participant_delivering_the_same_env_key_fail_compile() {
        let yaml = CONNECTION_CASE
            .replace("type: client", "type: shared-delivery-client")
            .replace("upstream: server.api", "z: server.api, a: server.api");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'shared-delivery-client' as linux-container on linux delivers env 'SHARED' through both 'a' and 'z'"
        );
        for connection in ["z: server.api, ", ", a: server.api"] {
            assert!(compile(&yaml.replace(connection, ""), Os::Linux).unwrap().is_some());
        }
    }

    #[test]
    fn connections_of_different_participants_may_deliver_the_same_env_keys() {
        let yaml = CONNECTION_CASE.replace(
            "participants:",
            "participants:\n  other: {type: client, connect: {upstream: server.api}}",
        );
        assert!(compile(&yaml, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_required_consumed_endpoint_must_be_connected() {
        let yaml = CONNECTION_CASE.replace(", connect: {upstream: server.api}", "");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'client' as linux-container on linux does not connect required endpoint 'upstream'; add 'upstream: <participant>.<endpoint>' to its 'connect'"
        );
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn an_optional_consumed_endpoint_may_be_left_unconnected() {
        assert!(compile(CONNECTION_CASE, Os::Linux).unwrap().is_some());
    }

    #[test]
    fn a_consumed_endpoint_accepts_each_kind_it_lists() {
        for (endpoint, config) in [("metrics", ""), ("extra", ", config: {endpoints: {extra: {port: 90}}}")] {
            let yaml = CONNECTION_CASE
                .replace(
                    "upstream: server.api",
                    &format!("upstream: server.api, side: server.{endpoint}"),
                )
                .replace("type: server}", &format!("type: server{config}}}"));
            assert!(compile(&yaml, Os::Linux).unwrap().is_some());
        }
    }

    #[test]
    fn a_connection_target_on_another_os_is_checked_with_its_own_realization() {
        let yaml = CONNECTION_CASE
            .replace("[linux]", "[macos]")
            .replace("upstream: server.api", "upstream: server.api, side: server.metrics")
            .replace(
                "type: server}",
                "type: server, os: linux, config: {os: {linux: {env: {METRICS: 'off'}}}}}",
            );
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'client' connects 'side' to 'server.metrics', which participant 'server' of type 'server' as linux-container on linux does not serve; it serves api"
        );
        assert!(compile(&yaml.replace("METRICS: 'off'", "METRICS: 'on'"), Os::MacOs)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_participant_type_that_declares_no_endpoints_serves_and_consumes_nothing() {
        struct Minimal;

        impl ParticipantType for Minimal {
            fn name(&self) -> &'static str {
                "minimal"
            }

            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                &[(Os::Linux, RealizationKind::LinuxContainer)]
            }

            fn processes(&self) -> &'static [&'static str] {
                &["main"]
            }
        }

        let registries = Registries {
            participant_types: &[&Minimal, &Client],
            steps: &[],
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        for (participants, expected) in [
            (
                "  subject: {type: minimal, subject: true, config: {endpoints: {api: {port: 80}}}}",
                "participant 'subject' of type 'minimal' as linux-container on linux overrides endpoint 'api', which it does not serve; it serves none",
            ),
            (
                "  subject: {type: minimal, subject: true, connect: {upstream: other.api}}\n  other: {type: minimal}",
                "participant 'subject' of type 'minimal' as linux-container on linux connects 'upstream', which it does not consume; it consumes none",
            ),
            (
                "  subject: {type: minimal, subject: true}\n  client: {type: client, connect: {upstream: subject.api}}",
                "participant 'client' connects 'upstream' to 'subject.api', which participant 'subject' of type 'minimal' as linux-container on linux does not serve; it serves none",
            ),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n{participants}\nprocedure: []\n");
            let case = loader.load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml")).unwrap().remove(0);
            let message = CasePlan::compile(case, Os::Linux, &registries, &mut PortRegistry::new(), &mut ScopeRegistry::with_run("testrun0")).err().unwrap().to_string();
            assert_eq!(message, format!("case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', {expected}"));
        }
    }

    #[test]
    fn a_typed_plan_has_no_effective_configs() {
        let plan = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.effective_config("subject"), None);
    }

    #[test]
    fn facts_must_be_provided_by_the_participants_realization_on_every_declared_subject_os() {
        for declared in ["[windows, macos, linux]", "[macos]"] {
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let message = compile(&FACT_CASE.replace("[linux]", declared), os)
                    .err()
                    .unwrap()
                    .to_string();
                assert_eq!(message,
                    "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 references fact 'file' of participant 'subject', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
                );
            }
        }
        assert!(compile(&FACT_CASE.replace("[linux]", "[linux, windows]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn late_facts_must_be_provided_by_the_participants_realization_on_every_declared_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, macos]")
            .replace("subject.facts.file", "subject.late.container_id");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 references late fact 'container_id' of participant 'subject', which a host-process-group does not have; a host-process-group has ip"
            );
        }
        assert!(compile(&yaml.replace("[linux, macos]", "[linux, windows]"), Os::Linux)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_late_fact_every_realization_has_compiles_on_every_declared_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, macos, windows]")
            .replace("subject.facts.file", "subject.late.ip");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.subject_os(), os);
            let CaseSchema::Participants { procedure, .. } = plan.case().schema() else {
                panic!("expected participant data");
            };
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one step");
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("subject".into()),
                    Need::Late("subject".into(), backend::LateFact::Ip)
                ]
            );
        }
    }

    #[test]
    fn pod_uid_is_available_only_to_a_kubernetes_pod() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE
            .replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}")
            .replace("subject.facts.file", "subject.late.pod_uid");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(
            case,
            Os::Linux,
            &registries,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.layout().unwrap().get("subject").unwrap().kind,
            RealizationKind::KubernetesPod
        );
        let case = loader
            .load_content(
                &yaml.replace("kubernetes-pod", "linux-container"),
                "alpha",
                Path::new("cases/alpha/config.yaml"),
            )
            .unwrap()
            .remove(0);
        assert_eq!(CasePlan::compile(case, Os::Linux, &registries, &mut PortRegistry::new(), &mut ScopeRegistry::with_run("testrun0")).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 references late fact 'pod_uid' of participant 'subject', which a linux-container does not have; a linux-container has container_id, ip"
        );
    }

    #[test]
    fn fact_and_late_fact_errors_follow_procedure_order_including_parallel_members() {
        let mut yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "procedure: []",
            r#"procedure:
  - {step: probe, on: subject, path: '{{ subject.late.container_id }}{{ subject.facts.file }}'}
  - parallel:
    - {step: probe, on: subject, path: '{{ subject.late.ip }}'}
    - {step: probe, on: subject, path: '{{ subject.facts.file }}{{ subject.late.pod_uid }}'}
  - {step: probe, on: subject, path: '{{ subject.late.container_id }}'}"#,
        );
        for (reference, label, kind, fact) in [
            ("{{ subject.late.container_id }}", "1", "late fact", "container_id"),
            ("{{ subject.facts.file }}", "1", "fact", "file"),
            ("{{ subject.facts.file }}", "2.2", "fact", "file"),
            ("{{ subject.late.pod_uid }}", "2.2", "late fact", "pod_uid"),
            ("{{ subject.late.container_id }}", "3", "late fact", "container_id"),
        ] {
            let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
            assert!(
                message.contains(&format!(
                    "on subject OS 'macos', step {label} references {kind} '{fact}' of participant 'subject'"
                )),
                "{message}"
            );
            yaml = yaml.replacen(reference, "literal", 1);
        }
        assert!(compile(&yaml, Os::MacOs).unwrap().is_some());
    }

    #[test]
    fn a_fact_provided_for_one_realization_is_missing_for_another_realization_on_the_same_os() {
        let registries = Registries {
            participant_types: &[&PodServer],
            steps: STEPS,
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture-participants");
        let yaml = FACT_CASE.replace("type: server", "type: pod-server, realization: {kind: kubernetes-pod}");
        let case = loader
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = CasePlan::compile(
            case,
            Os::Linux,
            &registries,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', step 1 references fact 'file' of participant 'subject', which type 'pod-server' does not provide as kubernetes-pod on linux; it provides it as linux-container on linux"
        );
        let case = loader
            .load_content(
                &yaml.replace("kubernetes-pod", "linux-container"),
                "alpha",
                Path::new("config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = CasePlan::compile(
            case,
            Os::Linux,
            &registries,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.fact("subject", "file"), Some("/logs/output.log"));
    }

    #[test]
    fn a_participant_with_its_own_os_needs_facts_only_from_its_realization_on_that_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace(
                "participants:",
                "participants:\n  other: {type: server, os: linux, config: {env: {PORT: '8081', METRICS: 'off'}}}",
            )
            .replace("subject.facts.file", "other.facts.file");
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.fact("other", "file"), Some("/logs/output.log"));
        assert_eq!(plan.fact("subject", "file"), None);
        let message = compile(&yaml.replace(", os: linux", ""), Os::MacOs)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', step 1 references fact 'file' of participant 'other', which type 'server' does not provide as host-process-group on macos; it provides it as linux-container on linux, windows-container on windows"
        );
    }

    #[test]
    fn a_plan_resolves_facts_from_each_participants_realization_on_the_selected_subject_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[linux, windows]")
            .replace("participants:", "participants:\n  other: {type: server, os: linux}");
        for (os, expected) in [(Os::Linux, "/logs/output.log"), (Os::Windows, r"C:\logs\output.log")] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.fact("subject", "file"), Some(expected));
            assert_eq!(plan.fact("other", "file"), Some("/logs/output.log"));
            assert_eq!(plan.fact("subject", "unknown"), None);
            assert_eq!(plan.fact("unknown", "file"), None);
        }
    }

    #[test]
    fn a_typed_plan_resolves_no_facts() {
        let plan = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.fact("subject", "file"), None);
    }

    #[test]
    fn a_layout_error_on_a_subject_os_is_reported_before_its_fact_errors() {
        let yaml = FACT_CASE.replace("[linux]", "[macos]").replace(
            "participants:",
            "participants:\n  other: {type: server, realization: {kind: linux-container}, config: {env: {PORT: '8081', METRICS: 'off'}}}",
        );
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'other' of type 'server' cannot run as linux-container on macos; it supports linux-container on linux, host-process-group on macos, windows-container on windows"
        );
        assert!(
            compile(&yaml.replace(", realization: {kind: linux-container}", ""), Os::MacOs)
                .err()
                .unwrap()
                .to_string()
                .contains("step 1 references fact 'file'")
        );
    }

    #[test]
    fn fact_errors_follow_subject_os_order_then_procedure_order_including_parallel_members() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[macos, linux]")
            .replace("participants:", "participants:\n  other: {type: server, os: macos, config: {env: {PORT: '8081', METRICS: 'off'}}}")
            .replace("procedure: []", "procedure:\n  - {step: probe, on: subject, path: '{{ subject.facts.file }}'}\n  - parallel:\n    - {step: probe, on: subject}\n    - {step: probe, on: subject, path: '{{ other.facts.file }}'}\n  - {step: probe, on: subject, path: '{{ other.facts.file }}'}");
        let message = compile(&yaml, Os::MacOs).err().unwrap().to_string();
        assert!(
            message.contains("on subject OS 'linux', step 2.2 references fact 'file' of participant 'other'"),
            "{message}"
        );
        let message = compile(&yaml.replace("{{ other.facts.file }}", "literal"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("on subject OS 'macos', step 1 references fact 'file' of participant 'subject'"),
            "{message}"
        );
    }

    #[test]
    fn a_participant_case_is_planned_only_for_a_subject_os_it_declares() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        for (os, kind) in [
            (Os::Linux, Some(RealizationKind::LinuxContainer)),
            (Os::MacOs, None),
            (Os::Windows, Some(RealizationKind::WindowsContainer)),
        ] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let plan = CasePlan::compile(
                case,
                os,
                &REGISTRIES,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .unwrap();
            if let Some(kind) = kind {
                let plan = plan.unwrap();
                assert_eq!(plan.case().name(), "alpha");
                assert_eq!(plan.subject_os(), os);
                assert_eq!(
                    plan.layout().unwrap().iter().collect::<Vec<_>>(),
                    [("subject", Realization { os, kind })]
                );
            } else {
                assert!(plan.is_none());
            }
        }
    }

    #[test]
    fn a_layout_error_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[linux, windows]")
            .replace("subject: true", "subject: true, realization: {kind: linux-container}");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let message = CasePlan::compile(
                case,
                os,
                &REGISTRIES,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .err()
            .unwrap()
            .to_string();
            assert!(
                message.contains(
                    "on subject OS 'windows', participant 'subject' of type 'server' \
                     cannot run as linux-container on windows"
                ),
                "{message}"
            );
        }
    }

    #[test]
    fn a_typed_case_is_planned_without_a_layout_for_its_declared_subject_os() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content("type: fixture", "alpha", Path::new("config.yaml"))
                .unwrap()
                .remove(0);
            let plan = CasePlan::compile(
                case,
                os,
                &REGISTRIES,
                &mut PortRegistry::new(),
                &mut ScopeRegistry::with_run("testrun0"),
            )
            .unwrap();
            if os == Os::Linux {
                assert!(plan.is_none());
            } else {
                let plan = plan.unwrap();
                assert_eq!(plan.subject_os(), os);
                assert_eq!(plan.case().name(), "alpha");
                assert!(plan.layout().is_none());
                assert!(plan.runner_host_ports.is_empty());
            }
        }
    }

    #[test]
    fn compile_errors_name_the_case_source_and_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        let case = LOADER
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = CasePlan::compile(
            case,
            Os::Linux,
            &REGISTRIES,
            &mut PortRegistry::new(),
            &mut ScopeRegistry::with_run("testrun0"),
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', \
             participant 'subject' of type 'server' cannot run as kubernetes-pod on linux; \
             it supports linux-container on linux, host-process-group on macos, windows-container on windows"
        );
    }
}
