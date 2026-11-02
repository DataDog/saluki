//! Compiles loaded cases for a selected subject operating system.

use std::collections::BTreeMap;

use saluki_error::{generic_error, GenericError};

use crate::{
    artifact::{ArtifactBinding, ArtifactManifest, ArtifactRole},
    backend,
    case::{config_references::FileTemplates, CaseConfig, CaseFile, CaseSchema, Registries},
    layout::{describe_participant, lay_out, Layout, LayoutEntry, Os},
    participant::{find_type, CaseValues, EffectiveConfig, ParticipantType, SHARED_DIR_FACT},
};

mod artifacts;
mod connections;
mod endpoints;
mod order;
mod ports;
mod procedure;
mod references;
mod routes;
mod scope;

pub use self::artifacts::{check_artifact_manifest, needed_artifacts};
pub(crate) use self::scope::ScopeLayout;
use self::{
    connections::check_connections,
    endpoints::derive_endpoints,
    order::{dependency_order, DependencyOrder},
    ports::{check_host_listeners, PortRegistry, RunnerHostPorts},
    scope::{participant_shared_dir, ScopeRegistry},
};

/// Holds the registries and artifact bindings shared across one run's plans.
///
/// Compile every case in a run through one planner to give cases distinct isolation scopes and runner-host ports.
pub struct Planner<'a> {
    registries: Registries<'a>,
    ports: PortRegistry,
    scopes: ScopeRegistry,
    bindings: BTreeMap<String, String>,
    catalog: BTreeMap<&'static str, ArtifactRole>,
}

/// The result of compiling a case for one subject OS.
pub enum Compiled<C> {
    /// The case declares the selected OS.
    Planned(CasePlan<C>),
    /// The case's `subject_os` omits the selected OS.
    Excluded(CaseConfig<C>),
}

impl<C> Compiled<C> {
    /// Returns the plan when the case declares the selected OS.
    #[cfg(any(test, feature = "test-util"))]
    pub fn into_plan(self) -> Option<CasePlan<C>> {
        match self {
            Self::Planned(plan) => Some(plan),
            Self::Excluded(_) => None,
        }
    }
}

/// A loaded case with the layout selected for execution.
pub struct CasePlan<C> {
    case: CaseConfig<C>,
    subject_os: Os,
    layout: Option<Layout>,
    artifacts: Vec<ArtifactBinding>,
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

impl<'a> Planner<'a> {
    /// Creates a planner with run-wide artifact bindings and a random run identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid install main-binary paths or conflicting role declarations across registered
    /// participant types, backends and typed schemas, then for unknown manifest roles.
    pub fn new(registries: Registries<'a>, manifest: ArtifactManifest) -> Result<Self, GenericError> {
        let catalog = artifacts::role_catalog(registries).map_err(|error| generic_error!(error))?;
        artifacts::check_manifest(&catalog, &manifest).map_err(|error| generic_error!(error))?;
        Ok(Self {
            registries,
            ports: PortRegistry::new(),
            scopes: ScopeRegistry::new(),
            bindings: manifest.roles,
            catalog,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_run(registries: Registries<'a>, run: &str) -> Self {
        Self {
            scopes: ScopeRegistry::with_run(run),
            ..Self::new(registries, ArtifactManifest::default())
                .expect("test registries declare consistent artifact roles")
        }
    }

    /// Validates every declared subject OS, returning a plan for a declared selection or the excluded case.
    /// Participant cases carry a layout; typed cases retain their loader-supplied data.
    /// A participant case declaring the selected OS draws one isolation scope from this planner.
    /// Only the selected OS draws runner-host ports from this planner; other OSes use separate registries.
    /// Ports are drawn in check order: assigned listeners per participant and endpoint in name order,
    /// then connections per consumer, then references per participant (`env`, `params`, template files).
    /// Connections and references to one container endpoint share a runner-host port.
    ///
    /// # Errors
    ///
    /// Returns the first failing check, naming the case and source, and for per-OS checks the failing subject OS.
    /// Typed cases must declare roles matching the catalog for the selected OS.
    /// Participant cases are checked in this order:
    ///
    /// 1. Isolation scope allocation, when the case declares the selected OS.
    /// 2. On each declared subject OS in the order the case lists them, even when the selected OS is excluded:
    ///    1. Layout.
    ///    2. Participant facts: no type may declare `shared_dir`.
    ///    3. Per participant in name order: fact and late-fact references in `env`, `params`, then template files;
    ///       configuration; endpoint derivation; port assignment and re-derivation; endpoint overrides; then
    ///       listener ports and socket paths in endpoint name order.
    ///    4. Runner-host listener range and transport uniqueness, per participant and endpoint in name order.
    ///    5. Connections per consumer in name order. For each `connect` key in name order: consumed endpoint,
    ///       served endpoint, accepted kind, route, socket location and length, delivered env keys, then address
    ///       allocation and delivered values. Then required endpoints in name order.
    ///    6. Endpoint and connection references in configuration, per participant in name order (`env`, `params`,
    ///       then template files): served endpoint, route, socket location and length, address part, then
    ///       runner-host port allocation.
    ///    7. Create order, then start order.
    ///    8. Step fact and late-fact references in procedure order, naming the step.
    pub fn compile<C>(&mut self, case: CaseConfig<C>, subject_os: Os) -> Result<Compiled<C>, GenericError> {
        let selected = case.subject_os().contains(&subject_os);
        let mut layout = None;
        let mut artifacts = Vec::new();
        let mut scope = None;
        let mut order = None;
        let mut participants = BTreeMap::new();
        let mut runner_host_ports = BTreeMap::new();
        if let CaseSchema::Participants { file, procedure } = case.schema() {
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
                    case.file_templates(),
                    &candidate,
                    &candidate_scope,
                    self.registries.participant_types,
                    os_ports,
                )
                .map_err(&error)?;
                procedure::check_step_references(
                    file,
                    procedure,
                    &candidate,
                    self.registries.participant_types,
                    &planned.participants,
                )
                .map_err(error)?;
                if os == subject_os {
                    artifacts = artifacts::bind(file, &candidate, self.registries.participant_types, &self.bindings);
                    layout = Some(candidate);
                    order = Some(planned.order);
                    participants = planned.participants;
                    runner_host_ports = planned.runner_host_ports;
                }
            }
            if selected {
                scope = Some(candidate_scope);
            }
        } else if selected {
            let roles = case.typed_artifact_roles(subject_os);
            for role in roles {
                let mismatch = match self.catalog.get(role.name) {
                    None => "is not declared by any participant type, backend or typed case schema".to_string(),
                    Some(declared) if role != declared => {
                        format!("is {} but declared as {}", role.describe(), declared.describe())
                    }
                    Some(_) => continue,
                };
                return Err(generic_error!(
                    "case '{}' in {}: on subject OS '{}', artifact role '{}' {mismatch}",
                    case.name(),
                    case.source().display(),
                    subject_os.name(),
                    role.name,
                ));
            }
            artifacts = artifacts::bind_roles(roles.iter().copied(), &self.bindings);
        }
        if !selected {
            return Ok(Compiled::Excluded(case));
        }
        Ok(Compiled::Planned(CasePlan {
            case,
            subject_os,
            layout,
            artifacts,
            scope,
            order,
            participants,
            runner_host_ports,
        }))
    }
}

impl<C> CasePlan<C> {
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

    /// Reports each participant's type, OS and realization in name order.
    /// Returns `None` for cases with a top-level `type`, which declare no participants.
    pub fn layout_entries(&self) -> Option<Vec<LayoutEntry>> {
        let CaseSchema::Participants { file, .. } = self.case.schema() else {
            return None;
        };
        Some(
            self.layout
                .as_ref()?
                .iter()
                .map(|(name, realization)| LayoutEntry {
                    participant: Some(name.to_string()),
                    participant_type: Some(file.participants[name].participant_type.clone()),
                    os: realization.os,
                    realization: realization.kind,
                })
                .collect(),
        )
    }

    /// Returns the case's artifact bindings for the selected OS, sorted by role name.
    pub fn artifacts(&self) -> &[ArtifactBinding] {
        &self.artifacts
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

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        fixtures::{compile, compile_with, host_to_container_case, planner, CONFIG_LOADER, FACT_CASE, FILE_REGISTRIES},
        *,
    };
    use crate::{
        case::{
            fixtures::{FixtureTypes, LOADER, PARTICIPANT_FIELDS, REGISTRIES, STEPS},
            CaseLoader,
        },
        layout::{Realization, RealizationKind},
    };

    #[test]
    fn layout_entries_list_each_participant_with_its_type_os_and_realization_in_name_order() {
        let yaml = host_to_container_case().replace("  client:", "  z-caller:");
        let plan = compile(&yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(
            plan.layout_entries(),
            Some(vec![
                LayoutEntry {
                    participant: Some("server".into()),
                    participant_type: Some("server".into()),
                    os: Os::Linux,
                    realization: RealizationKind::LinuxContainer,
                },
                LayoutEntry {
                    participant: Some("z-caller".into()),
                    participant_type: Some("client".into()),
                    os: Os::MacOs,
                    realization: RealizationKind::HostProcessGroup,
                },
            ])
        );
        let typed = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(typed.layout_entries(), None);
    }

    #[test]
    fn each_participant_case_declaring_the_selected_os_draws_one_scope() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]");
        let mut planner = planner();
        for (index, os) in [Os::Linux, Os::MacOs, Os::Windows].into_iter().enumerate() {
            let plan = compile_with(&mut planner, &yaml, os).unwrap().unwrap();
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
        assert_eq!(planner.scopes.allocate().unwrap().index, 3);
    }

    #[test]
    fn a_case_excluding_the_selected_os_and_a_typed_case_draw_no_scope() {
        let mut planner = planner();
        assert!(compile_with(&mut planner, PARTICIPANT_FIELDS, Os::MacOs)
            .unwrap()
            .is_none());
        let plan = compile_with(&mut planner, "type: fixture", Os::MacOs).unwrap().unwrap();
        assert!(plan.scope.is_none());
        assert_eq!(plan.fact("subject", SHARED_DIR_FACT), None);
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
        assert!(compile_with(&mut planner, "type: fixture", Os::MacOs)
            .unwrap()
            .unwrap()
            .scope
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
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
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

    #[test]
    fn a_typed_plan_has_no_effective_configs() {
        let plan = compile("type: fixture", Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.effective_config("subject"), None);
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
    fn a_participant_case_is_planned_only_for_a_subject_os_it_declares() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        for (os, kind) in [
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Windows, RealizationKind::WindowsContainer),
        ] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let mut planner = Planner::with_run(REGISTRIES, "testrun0");
            let Compiled::Excluded(case) = planner.compile(case, Os::MacOs).unwrap() else {
                panic!("macOS is excluded");
            };
            assert_eq!(case.name(), "alpha");
            let plan = planner.compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.case().name(), "alpha");
            assert_eq!(plan.subject_os(), os);
            assert_eq!(
                plan.layout().unwrap().iter().collect::<Vec<_>>(),
                [("subject", Realization { os, kind })]
            );
        }
    }

    #[test]
    fn an_excluded_participant_case_allocates_no_isolation_scope_or_runner_host_ports() {
        let case = CONFIG_LOADER
            .load_content(&host_to_container_case(), "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let mut planner = planner();
        let Compiled::Excluded(case) = planner.compile(case, Os::Linux).unwrap() else {
            panic!("Linux is excluded");
        };
        let plan = planner.compile(case, Os::MacOs).unwrap().into_plan().unwrap();
        assert_eq!(
            plan.scope.as_ref().unwrap().state_dir(Os::MacOs),
            "/tmp/panoramic-testrun0/0000"
        );
        assert_eq!(
            plan.runner_host_ports,
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
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
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
    fn a_typed_case_is_planned_without_a_layout_for_its_declared_subject_os() {
        for os in [Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content("type: fixture", "alpha", Path::new("config.yaml"))
                .unwrap()
                .remove(0);
            let mut planner = Planner::with_run(REGISTRIES, "testrun0");
            let Compiled::Excluded(case) = planner.compile(case, Os::Linux).unwrap() else {
                panic!("Linux is excluded");
            };
            let plan = planner.compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.subject_os(), os);
            assert_eq!(plan.case().name(), "alpha");
            assert!(plan.layout().is_none());
            assert!(plan.runner_host_ports.is_empty());
        }
    }

    #[test]
    fn compile_errors_name_the_case_source_and_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        let case = LOADER
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
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
