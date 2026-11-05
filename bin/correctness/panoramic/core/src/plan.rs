//! Compiles loaded cases for a selected subject operating system.

use std::{collections::BTreeMap, path::Path};

use saluki_error::{generic_error, GenericError};

use crate::{
    artifact::{ArtifactBinding, ArtifactManifest, ArtifactRole},
    backend,
    case::{config_references::FileTemplates, CaseConfig, CaseFile, FileSource, ProcedureNode, Registries},
    endpoint::{Endpoint, EndpointOperation, Listener, Port, ResolvedAddress, RunnerHostPort, RunnerHostRoute},
    layout::{describe_participant, lay_out, Layout, LayoutEntry, Os, RealizationKind},
    participant::{
        check_process_names, check_process_specs, find_type, CaseValues, EffectiveConfig, Operation, ParticipantType,
        SHARED_DIR_FACT,
    },
    step::Step,
};

mod artifacts;
mod connections;
mod endpoints;
pub(crate) mod files;
mod order;
mod ports;
mod procedure;
mod readiness;
mod references;
mod routes;
mod scope;

pub use self::artifacts::{check_artifact_manifest, needed_artifacts};
use self::{
    connections::check_connections,
    endpoints::derive_endpoints,
    order::{dependency_order, DependencyOrder},
    ports::{check_host_listeners, PortRegistry, RunnerHostPorts},
    scope::{participant_shared_dir, ScopeRegistry},
};
pub(crate) use self::{
    readiness::{PlannedCheck, StartGate},
    scope::{participant_dir, ScopeLayout},
};

/// Holds the registries and artifact bindings shared across one run's plans.
///
/// Compile every case in a run through one planner to give cases distinct isolation scopes and runner-host ports.
pub struct Planner<'a> {
    registries: Registries<'a>,
    ports: PortRegistry,
    scopes: ScopeRegistry,
    bindings: BTreeMap<String, String>,
}

/// The result of compiling a case for one subject OS.
pub enum Compiled {
    /// The case declares the selected OS.
    Planned(Box<CasePlan>),
    /// The case's `subject_os` omits the selected OS.
    Excluded(Box<CaseConfig>),
}

impl Compiled {
    /// Returns the plan when the case declares the selected OS.
    #[cfg(any(test, feature = "test-util"))]
    pub fn into_plan(self) -> Option<CasePlan> {
        match self {
            Self::Planned(plan) => Some(*plan),
            Self::Excluded(_) => None,
        }
    }
}

/// A loaded case with the layout selected for execution.
pub struct CasePlan {
    case: CaseConfig,
    subject_os: Os,
    artifacts: Vec<ArtifactBinding>,
    image_roles: BTreeMap<RealizationKind, Vec<ArtifactRole>>,
    participants: ParticipantData,
}

struct ParticipantData {
    layout: Layout,
    scope: ScopeLayout,
    planned: PlannedCase,
}

/// A compiled participant case's declarations, procedure and realization-specific values.
#[derive(Clone, Copy)]
pub struct ParticipantPlan<'a> {
    file: &'a CaseFile,
    procedure: &'a [ProcedureNode<Box<dyn Step>>],
    templates: &'a FileTemplates,
    data: &'a ParticipantData,
}

struct PlannedCase {
    participants: BTreeMap<String, PlannedParticipant>,
    order: DependencyOrder,
    runner_host_ports: BTreeMap<(String, String), u16>,
    runner_host_routes: BTreeMap<(String, String, EndpointOperation), RunnerHostRoute>,
    start_gates: BTreeMap<String, Vec<StartGate>>,
}

struct PlannedParticipant {
    operations: &'static [&'static dyn Operation],
    case_values: CaseValues,
    facts: BTreeMap<String, String>,
    config: EffectiveConfig,
    served: BTreeMap<String, Endpoint>,
    connections: BTreeMap<String, ResolvedAddress>,
    endpoint_references: BTreeMap<(String, String), ResolvedAddress>,
}

impl<'a> Planner<'a> {
    /// Creates a planner with run-wide artifact bindings and a random run identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid install main-binary paths or conflicting role declarations across registered
    /// participant types and backends, then for unknown manifest roles.
    pub fn new(registries: Registries<'a>, manifest: ArtifactManifest) -> Result<Self, GenericError> {
        let catalog = artifacts::role_catalog(registries).map_err(|error| generic_error!(error))?;
        artifacts::check_manifest(&catalog, &manifest).map_err(|error| generic_error!(error))?;
        Ok(Self {
            registries,
            ports: PortRegistry::new(),
            scopes: ScopeRegistry::new(),
            bindings: manifest.roles,
        })
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn with_port_range(mut self, ports: std::ops::RangeInclusive<u16>) -> Self {
        self.ports = PortRegistry::over(ports);
        self
    }

    /// Creates a planner with default artifact bindings and a fixed run identity for tests.
    ///
    /// # Panics
    ///
    /// Panics if the registries declare inconsistent artifact roles.
    #[cfg(test)]
    pub(crate) fn with_run(registries: Registries<'a>, run: &str) -> Self {
        Self {
            scopes: ScopeRegistry::with_run(run),
            ..Self::new(registries, ArtifactManifest::default())
                .expect("test registries declare consistent artifact roles")
        }
    }

    /// Validates every declared subject OS, returning a plan for a declared selection or the excluded case.
    /// A participant case declaring the selected OS draws one isolation scope from this planner.
    /// Only the selected OS draws runner-host ports from this planner; other OSes use separate registries.
    /// Ports are drawn in check order: assigned listeners per participant and endpoint in name order,
    /// then connections per consumer, then references per participant (`env`, `params`, template files),
    /// then start gates per consumer, then step request/response endpoint needs in procedure order.
    /// Connections, references, start gates and step requests reaching one endpoint share its runner-host port.
    ///
    /// # Errors
    ///
    /// Returns the first failing check, naming the case and source, and for per-OS checks the failing subject OS.
    /// Participant cases are checked in this order:
    ///
    /// 1. Isolation scope allocation, when the case declares the selected OS.
    /// 2. On each declared subject OS in the order the case lists them, even when the selected OS is excluded:
    ///    1. Layout.
    ///    2. Participant facts: no type may declare `shared_dir`.
    ///    3. Per participant in name order: process names and any declared process specifications;
    ///       fact and late-fact references in `env`, `params`, then template files;
    ///       configuration; endpoint derivation; port assignment and re-derivation; endpoint overrides; then
    ///       listener ports and socket paths in endpoint name order, then readiness evidence (endpoint evidence
    ///       in endpoint name order, then participant evidence); then reserved environment variables.
    ///    4. Runner-host listener range and transport uniqueness, per participant and endpoint in name order.
    ///    5. Connections per consumer in name order. For each `connect` key in name order: consumed endpoint,
    ///       served endpoint, accepted kind, route, socket location and length, delivered env keys (including reserved
    ///       names), then address allocation and delivered values. Then required endpoints in name order.
    ///    6. Endpoint and connection references in configuration, per participant in name order (`env`, `params`,
    ///       then template files): served endpoint, route, socket location and length, address part, then
    ///       runner-host port allocation and address rendering.
    ///    7. Create order, then start order.
    ///    8. Start gates per consumer in name order: connected endpoint evidence in connection-key order, then
    ///       distinct servers in name order and their participant evidence in declaration order. Each resolves
    ///       its operation's route and draws any runner-host port. Self-connections add no gates; equal gates
    ///       within a consumer are kept once.
    ///    9. Step fact, late-fact, operation and endpoint needs in procedure order, naming the step. Endpoint needs
    ///       check the served endpoint and operation-specific route, then allocate any port it needs and render its address.
    ///       Operation needs check the names the type offers on the participant's realization.
    pub fn compile(&mut self, case: CaseConfig, subject_os: Os) -> Result<Compiled, GenericError> {
        let selected = case.subject_os().contains(&subject_os);
        let file = case.file();
        let procedure = case.procedure();
        let mut selected_participants = None;
        let candidate_scope = if selected {
            self.scopes.allocate().map_err(|error| {
                generic_error!(
                    "case '{}' in {}: cannot allocate an isolation scope: {error}",
                    case.name(),
                    case.source().display()
                )
            })?
        } else {
            self.scopes.unallocated()
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
            let candidate = lay_out(&file.participants, self.registries.participant_types, os).map_err(&error)?;
            let mut scratch_ports = PortRegistry::new();
            let os_ports = if os == subject_os {
                &mut self.ports
            } else {
                &mut scratch_ports
            };
            let planned = plan_participants(
                file,
                procedure,
                case.file_templates(),
                case.source(),
                &candidate,
                &candidate_scope,
                self.registries.participant_types,
                os_ports,
            )
            .map_err(error)?;
            if os == subject_os {
                let (artifacts, image_roles) =
                    artifacts::bind(file, &candidate, self.registries.participant_types, &self.bindings);
                selected_participants = Some((candidate, planned, artifacts, image_roles));
            }
        }
        if !selected {
            return Ok(Compiled::Excluded(Box::new(case)));
        }
        let (layout, planned, artifacts, image_roles) = selected_participants.expect("the selected OS was planned");
        Ok(Compiled::Planned(Box::new(CasePlan {
            case,
            subject_os,
            artifacts,
            image_roles,
            participants: ParticipantData {
                layout,
                scope: candidate_scope,
                planned,
            },
        })))
    }
}

impl CasePlan {
    /// Returns the loaded case and its metadata.
    pub fn case(&self) -> &CaseConfig {
        &self.case
    }

    /// Returns the participant plan.
    pub fn participants(&self) -> ParticipantPlan<'_> {
        ParticipantPlan {
            file: self.case.file(),
            procedure: self.case.procedure(),
            templates: self.case.file_templates(),
            data: &self.participants,
        }
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> &ScopeLayout {
        self.participants().scope()
    }

    /// Returns the subject OS selected for execution.
    pub fn subject_os(&self) -> Os {
        self.subject_os
    }

    /// Returns the participants' layout for the selected subject OS.
    pub fn layout(&self) -> &Layout {
        self.participants().layout()
    }

    /// Reports each participant's type, OS and realization in name order.
    pub fn layout_entries(&self) -> Vec<LayoutEntry> {
        self.layout()
            .iter()
            .map(|(name, realization)| LayoutEntry {
                participant: name.to_string(),
                participant_type: self.case.file().participants[name].participant_type.clone(),
                os: realization.os,
                realization: realization.kind,
            })
            .collect()
    }

    /// Returns the case's artifact bindings for the selected OS, sorted by role name.
    pub fn artifacts(&self) -> &[ArtifactBinding] {
        &self.artifacts
    }

    /// Returns participant and backend image roles for a realization kind, sorted and deduplicated by name.
    pub fn image_roles_for(&self, kind: RealizationKind) -> &[ArtifactRole] {
        self.image_roles.get(&kind).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Returns create dependency levels.
    ///
    /// A participant follows every other participant whose late fact, known once created, its configuration embeds.
    /// Each level lists independent participants in name order.
    pub fn create_order(&self) -> &[Vec<String>] {
        self.participants().create_order()
    }

    /// Returns start dependency levels.
    ///
    /// A participant follows every other participant it connects to. A pod also follows every other participant
    /// whose late fact its template files reference when that fact becomes available only after start.
    /// Each level lists independent participants in name order.
    pub fn start_order(&self) -> &[Vec<String>] {
        self.participants().start_order()
    }

    /// Returns a fact from the participant's planned realization, including `shared_dir`.
    pub fn fact(&self, participant: &str, fact: &str) -> Option<&str> {
        self.participants().fact(participant, fact)
    }

    /// Returns the participant's effective configuration for its planned realization.
    pub fn effective_config(&self, participant: &str) -> Option<&EffectiveConfig> {
        self.participants().effective_config(participant)
    }

    /// Returns the substituted case values after the participant OS overlay, before defaults and deliveries.
    #[cfg(test)]
    pub(crate) fn case_values(&self, participant: &str) -> Option<&CaseValues> {
        self.participants
            .planned
            .participants
            .get(participant)
            .map(|participant| &participant.case_values)
    }

    #[cfg(test)]
    pub(crate) fn start_gates(&self, participant: &str) -> &[StartGate] {
        self.participants().start_gates(participant)
    }

    #[cfg(test)]
    pub(crate) fn connection_address(&self, participant: &str, key: &str) -> Option<&ResolvedAddress> {
        self.participants().connection_address(participant, key)
    }

    #[cfg(test)]
    pub(crate) fn endpoint_address(&self, holder: &str, server: &str, endpoint: &str) -> Option<&ResolvedAddress> {
        self.participants().endpoint_address(holder, server, endpoint)
    }

    #[cfg(test)]
    pub(crate) fn runner_host_route(
        &self, server: &str, endpoint: &str, operation: EndpointOperation,
    ) -> Option<&RunnerHostRoute> {
        self.participants().runner_host_route(server, endpoint, operation)
    }

    #[cfg(test)]
    pub(crate) fn runner_host_ports(&self, participant: &str) -> Vec<RunnerHostPort> {
        self.participants().runner_host_ports(participant)
    }
}

impl<'a> ParticipantPlan<'a> {
    pub(crate) fn file(self) -> &'a CaseFile {
        self.file
    }
    pub(crate) fn procedure(self) -> &'a [ProcedureNode<Box<dyn Step>>] {
        self.procedure
    }
    pub(crate) fn templates(self) -> &'a FileTemplates {
        self.templates
    }
    pub(crate) fn file_sources(self, participant: &str) -> &'a BTreeMap<String, FileSource> {
        &self.data.planned.participants[participant].case_values.files
    }
    pub(crate) fn scope(self) -> &'a ScopeLayout {
        &self.data.scope
    }
    pub(crate) fn layout(self) -> &'a Layout {
        &self.data.layout
    }
    pub(crate) fn create_order(self) -> &'a [Vec<String>] {
        &self.data.planned.order.create
    }
    pub(crate) fn start_order(self) -> &'a [Vec<String>] {
        &self.data.planned.order.start
    }

    /// Returns the readiness checks a consumer waits for before starting.
    /// Returns an empty list for unknown participants.
    pub(crate) fn start_gates(self, participant: &str) -> &'a [StartGate] {
        self.data
            .planned
            .start_gates
            .get(participant)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Returns a fact from the participant's planned realization, including `shared_dir`.
    /// Returns `None` for unknown participants or facts.
    pub(crate) fn fact(self, participant: &str, fact: &str) -> Option<&'a str> {
        self.data
            .planned
            .participants
            .get(participant)?
            .facts
            .get(fact)
            .map(String::as_str)
    }

    /// Returns the address delivered for a participant's connection.
    pub(crate) fn connection_address(self, participant: &str, key: &str) -> Option<&'a ResolvedAddress> {
        self.data.planned.participants.get(participant)?.connections.get(key)
    }

    /// Returns a referenced endpoint's address as seen by the holder, using resolved participant names.
    pub(crate) fn endpoint_address(self, holder: &str, server: &str, endpoint: &str) -> Option<&'a ResolvedAddress> {
        self.data
            .planned
            .participants
            .get(holder)?
            .endpoint_references
            .get(&(server.into(), endpoint.into()))
    }

    /// Returns the operations offered by the planned realization, or an empty slice for an unknown participant.
    pub(crate) fn operations(self, participant: &str) -> &'static [&'static dyn Operation] {
        self.data
            .planned
            .participants
            .get(participant)
            .map(|participant| participant.operations)
            .unwrap_or(&[])
    }

    /// Returns the runner-host route for a step-needed endpoint operation.
    pub(crate) fn runner_host_route(
        self, server: &str, endpoint: &str, operation: EndpointOperation,
    ) -> Option<&'a RunnerHostRoute> {
        self.data
            .planned
            .runner_host_routes
            .get(&(server.into(), endpoint.into(), operation))
    }

    /// Lists the participant's published or forwarded ports in endpoint-name order.
    /// Returns an empty list when the participant is unknown or needs no runner-host ports.
    pub(crate) fn runner_host_ports(self, participant: &str) -> Vec<RunnerHostPort> {
        self.data
            .planned
            .runner_host_ports
            .iter()
            .filter(|((server, _), _)| server == participant)
            .map(|((_, name), &host_port)| {
                let endpoint = &self.data.planned.participants[participant].served[name];
                let Listener::Port(Port::Fixed(container_port)) = endpoint.listener else {
                    unreachable!("runner-host ports map to resolved network listeners");
                };
                RunnerHostPort {
                    transport: endpoint
                        .kind
                        .runner_host_transport()
                        .expect("network endpoints have a transport"),
                    container_port,
                    host_port,
                }
            })
            .collect()
    }

    /// Returns the participant's effective configuration for its planned realization.
    /// Returns `None` for unknown participants.
    pub(crate) fn effective_config(self, participant: &str) -> Option<&'a EffectiveConfig> {
        self.data
            .planned
            .participants
            .get(participant)
            .map(|participant| &participant.config)
    }
}

/// Computes every participant's facts, including `shared_dir`, rejecting types that declare it.
/// Then builds effective configurations (assigning host process group ports), checks endpoints, runner-host listeners
/// and connections, merges connection deliveries and resolves configuration address references in participant name order.
/// Then computes create and start orders, plans readiness gates and checks step needs,
/// storing their runner-host routes.
fn plan_participants(
    file: &CaseFile, procedure: &[ProcedureNode<Box<dyn Step>>], templates: &FileTemplates, source: &Path,
    layout: &Layout, scope: &ScopeLayout, types: &[&dyn ParticipantType], ports: &mut PortRegistry,
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
        check_process_names(&holder, participant_type.processes())?;
        if !participant_type.process_specs(realization).is_empty() {
            check_process_specs(name, participant_type, realization)?;
        }
        let mut values = CaseValues::for_os(&entry.config, realization.os);
        references::resolve_config_references(file, layout, types, &facts, name, &mut values)?;
        references::resolve_file_references(file, templates, layout, types, &facts, name, &mut values)?;
        for (file_name, file_source) in values.files.iter().filter(|(_, source)| !source.template) {
            files::read_source(name, file_name, file_source, source, &mut values.file_contents)?;
        }
        let (config, endpoints) = derive_endpoints(
            &holder,
            participant_type,
            realization,
            values.clone(),
            facts[name][SHARED_DIR_FACT].clone(),
            ports,
            entry
                .realization
                .as_ref()
                .map(|realization| &realization.options)
                .unwrap_or(&BTreeMap::new()),
        )?;
        if let Some(key) = config.env.keys().find(|key| key.starts_with("PANORAMIC_")) {
            let source = if values.env.contains_key(key) {
                "case configuration"
            } else {
                "type default"
            };
            return Err(format!(
                "{holder} sets reserved env '{key}' through its {source}; PANORAMIC_ is reserved for the harness"
            ));
        }
        participant_endpoints.insert(name.to_string(), endpoints);
        configs.insert(
            name.to_string(),
            (values, config, participant_type.operations(realization)),
        );
    }
    let mut participants: BTreeMap<_, _> = facts
        .into_iter()
        .zip(configs.into_values())
        .map(|((name, facts), (case_values, config, operations))| {
            let served = participant_endpoints[&name].served.clone();
            (
                name,
                PlannedParticipant {
                    operations,
                    case_values,
                    facts,
                    config,
                    served,
                    connections: BTreeMap::new(),
                    endpoint_references: BTreeMap::new(),
                },
            )
        })
        .collect();
    check_host_listeners(file, layout, &participant_endpoints)?;
    let mut runner_host_ports = RunnerHostPorts::new(ports);
    check_connections(
        file,
        layout,
        &participant_endpoints,
        &mut participants,
        &mut runner_host_ports,
    )?;
    references::check_address_references(
        file,
        templates,
        layout,
        &participant_endpoints,
        &mut participants,
        &mut runner_host_ports,
    )?;
    let order = dependency_order(file, templates, layout, &participant_endpoints)?;
    let start_gates = readiness::plan_start_gates(file, layout, &participant_endpoints, &mut runner_host_ports)?;
    let runner_host_routes = procedure::check_step_references(
        file,
        procedure,
        layout,
        types,
        &participants,
        &participant_endpoints,
        &mut runner_host_ports,
    )?;
    Ok(PlannedCase {
        participants,
        order,
        runner_host_ports: runner_host_ports.into_ports(),
        runner_host_routes,
        start_gates,
    })
}

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod validation_tests;

/// Plan assertions and resource identities for tests.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    /// Checks the scheme of a direct HTTP request route.
    ///
    /// # Panics
    ///
    /// Panics if the route is missing, indirect or has a different scheme.
    #[track_caller]
    pub fn assert_request_scheme(plan: &super::CasePlan, participant: &str, endpoint: &str, scheme: &str) {
        let route = plan
            .participants()
            .runner_host_route(participant, endpoint, crate::endpoint::EndpointOperation::Request)
            .expect("planned request route");
        let crate::endpoint::RunnerHostRoute::Direct(address) = route else {
            panic!("expected a direct request route");
        };
        assert_eq!(address.url().split_once("://").unwrap().0, scheme);
    }

    /// Returns the isolation-group label for a case.
    pub fn scope_id(plan: &super::CasePlan) -> String {
        plan.participants().scope().id()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        fixtures::{compile, compile_with, host_to_container_case, planner, CONFIG_LOADER, FACT_CASE, FILE_REGISTRIES},
        *,
    };
    use crate::{
        case::{
            fixtures::{minimal_procedure, LOADER, PARTICIPANT_FIELDS, REGISTRIES, STEPS},
            CaseLoader,
        },
        endpoint::Transport,
        layout::{Realization, RealizationKind},
    };

    #[test]
    fn runner_host_ports_list_transport_container_port_and_host_port_per_endpoint() {
        let yaml = host_to_container_case()
            .replace(
                "subject: true",
                "subject: true, config: {env: {REF: '{{server.endpoints.metrics.url}}', TCP: '{{server.endpoints.extra.url}}'}}",
            )
            .replace(
                "type: server, os: linux}",
                "type: server, os: linux, config: {env: {PORT: '9000'}, endpoints: {extra: {port: 9000}}}}",
            )
            .replace("participants:", "participants:\n  unused: {type: server, os: linux}")
            .replace(
                minimal_procedure!(),
                "procedure: [{step: reach, endpoint: server.extra}]",
            );
        let mut planner = planner();
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.runner_host_ports("server"),
            vec![
                RunnerHostPort {
                    transport: Transport::Tcp,
                    container_port: 9000,
                    host_port: 20000
                },
                RunnerHostPort {
                    transport: Transport::Tcp,
                    container_port: 9000,
                    host_port: 20002
                },
                RunnerHostPort {
                    transport: Transport::Udp,
                    container_port: 9000,
                    host_port: 20001
                },
            ]
        );
        for participant in ["client", "unused", "missing"] {
            assert!(plan.runner_host_ports(participant).is_empty());
        }
        assert_eq!(plan.runner_host_route("server", "api", EndpointOperation::Probe), None);
        assert_eq!(
            plan.runner_host_route("server", "metrics", EndpointOperation::Probe),
            None
        );
        assert_eq!(planner.ports.allocate(), Ok(20003));
    }

    #[test]
    fn layout_entries_list_each_participant_with_its_type_os_and_realization_in_name_order() {
        let yaml = host_to_container_case().replace("  client:", "  z-caller:");
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.layout_entries(),
            vec![
                LayoutEntry {
                    participant: "server".into(),
                    participant_type: "server".into(),
                    os: Os::Linux,
                    realization: RealizationKind::LinuxContainer,
                },
                LayoutEntry {
                    participant: "z-caller".into(),
                    participant_type: "client".into(),
                    os: Os::MacOs,
                    realization: RealizationKind::HostProcessGroup,
                },
            ]
        );
    }

    #[test]
    fn each_participant_case_declaring_the_selected_os_draws_one_scope() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]");
        let mut planner = planner();
        for (index, os) in [Os::Linux, Os::MacOs, Os::Windows].into_iter().enumerate() {
            let plan = compile_with(&mut planner, &yaml, os).unwrap().unwrap();
            let scope = plan.scope();
            assert_eq!(usize::from(scope.index), index);
            assert_eq!(scope.run, "testrun0");
            if os == Os::MacOs {
                assert_eq!(
                    plan.participants.planned.participants["subject"].facts[SHARED_DIR_FACT],
                    "/tmp/panoramic-testrun0/0001/subject/shared"
                );
            }
        }
        assert_eq!(planner.scopes.allocate().unwrap().index, 3);
    }

    #[test]
    fn a_case_excluding_the_selected_os_draws_no_scope() {
        let mut planner = planner();
        assert!(compile_with(&mut planner, PARTICIPANT_FIELDS, Os::MacOs)
            .unwrap()
            .is_none());
        assert_eq!(planner.scopes.allocate().unwrap().index, 0);
    }

    #[test]
    fn scope_exhaustion_names_the_case_and_precedes_layout_checks() {
        let mut planner = planner();
        for _ in 0..10000 {
            planner.scopes.allocate().unwrap();
        }
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        assert_eq!(compile_with(&mut planner, &yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: cannot allocate an isolation scope: all 10000 isolation scopes are allocated");
        assert!(compile_with(&mut planner, PARTICIPANT_FIELDS, Os::MacOs)
            .unwrap()
            .is_none());
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
                "step 1 'probe' references fact 'file' of participant 'other'",
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
        let loader = CaseLoader::new(registries);
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
                "step 1 'probe' references fact 'file' of participant 'observer'",
            ),
        ] {
            let case = loader
                .load_content("cases", &yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap();
            let message = Planner::with_run(registries, "testrun0")
                .compile(case, Os::Linux)
                .err()
                .unwrap()
                .to_string();
            assert!(message.contains(expected), "{message}");
            assert!(yaml.contains(bad));
            yaml = yaml.replace(bad, fix);
        }
        let case = loader
            .load_content("cases", &yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap();
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(plan.create_order(), [vec!["observer", "subject"], vec!["fixture"]]);
        assert_eq!(plan.start_order(), [vec!["observer", "subject"], vec!["fixture"]]);
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
                .contains("step 1 'probe' references fact 'file'")
        );
    }

    #[test]
    fn a_participant_case_is_planned_only_for_a_subject_os_it_declares() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        for (os, kind) in [
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Windows, RealizationKind::WindowsContainer),
        ] {
            let case = LOADER
                .load_content("cases", &yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap();
            let mut planner = Planner::with_run(REGISTRIES, "testrun0");
            let Compiled::Excluded(case) = planner.compile(case, Os::MacOs).unwrap() else {
                panic!("macOS is excluded");
            };
            assert_eq!(case.name(), "alpha");
            let plan = planner.compile(*case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.case().name(), "alpha");
            assert_eq!(plan.subject_os(), os);
            assert_eq!(
                plan.layout().iter().collect::<Vec<_>>(),
                [("subject", Realization { os, kind })]
            );
        }
    }

    #[test]
    fn an_excluded_participant_case_allocates_no_isolation_scope_or_runner_host_ports() {
        let case = CONFIG_LOADER
            .load_content(
                "cases",
                &host_to_container_case(),
                "alpha",
                Path::new("cases/alpha/config.yaml"),
            )
            .unwrap();
        let mut planner = planner();
        let Compiled::Excluded(case) = planner.compile(case, Os::Linux).unwrap() else {
            panic!("Linux is excluded");
        };
        let plan = planner.compile(*case, Os::MacOs).unwrap().into_plan().unwrap();
        assert_eq!(plan.scope().state_dir(Os::MacOs), "/tmp/panoramic-testrun0/0000");
        assert_eq!(
            plan.participants.planned.runner_host_ports,
            BTreeMap::from([(("server".into(), "api".into()), 20000)])
        );
    }

    #[test]
    fn a_layout_error_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[linux, windows]")
            .replace("subject: true", "subject: true, realization: {kind: linux-container}");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content("cases", &yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap();
            let message = Planner::with_run(REGISTRIES, "testrun0")
                .compile(case, os)
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
    fn compile_errors_name_the_case_source_and_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        let case = LOADER
            .load_content("cases", &yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap();
        let message = Planner::with_run(REGISTRIES, "testrun0")
            .compile(case, Os::Linux)
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
