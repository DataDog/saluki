//! Compiles loaded cases for a selected subject operating system.

use std::collections::BTreeMap;

use saluki_error::{generic_error, GenericError};

use crate::{
    backend,
    case::{
        config_references::FileTemplates, declared_names, CaseConfig, CaseFile, CaseSchema, ProcedureNode, Registries,
    },
    layout::{describe_participant, describe_realizations, lay_out, Layout, Os, Realization},
    participant::{find_type, CaseValues, EffectiveConfig, FactDeclaration, ParticipantType},
    record::step_label,
    step::{Need, Step},
};

mod connections;
mod order;
mod references;
mod routes;

use self::{
    connections::{check_connections, ParticipantEndpoints},
    order::{dependency_order, DependencyOrder},
};

/// A loaded case with the layout selected for execution.
pub struct CasePlan<C> {
    case: CaseConfig<C>,
    subject_os: Os,
    layout: Option<Layout>,
    order: Option<DependencyOrder>,
    participants: BTreeMap<String, PlannedParticipant>,
}

struct PlannedParticipant {
    facts: &'static [FactDeclaration],
    config: EffectiveConfig,
}

impl<C> CasePlan<C> {
    /// Validates every declared subject OS, returning a plan only for a declared selection.
    /// Participant cases carry a layout; typed cases retain their loader-supplied data.
    ///
    /// # Errors
    ///
    /// Returns the first error on any declared subject OS, naming the case, source and failing OS,
    /// even when the selected OS is excluded. On each OS, checks run in this order: layout;
    /// configuration fact and late-fact references (`env`, `params`, then template files), configuration,
    /// endpoint derivation and overrides
    /// per participant in name order;
    /// connections per consumer in name order (per `connect` key in name order: consumed, served,
    /// accepted kind, route, delivered env keys; then required endpoints in name order);
    /// configuration endpoint and connection references per participant in name order (`env`, then `params`,
    /// then template files);
    /// create order, then start order;
    /// step fact and late-fact references in procedure order with step labels.
    pub fn compile(
        case: CaseConfig<C>, subject_os: Os, registries: &Registries<'_>,
    ) -> Result<Option<Self>, GenericError> {
        let mut layout = None;
        let mut order = None;
        let mut participants = BTreeMap::new();
        if let CaseSchema::Participants { file, procedure } = case.schema() {
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
                let (candidate_participants, candidate_order) =
                    plan_participants(file, case.file_templates(), &candidate, registries.participant_types)
                        .map_err(&error)?;
                check_reference_needs(
                    file,
                    procedure,
                    &candidate,
                    registries.participant_types,
                    &candidate_participants,
                )
                .map_err(error)?;
                if os == subject_os {
                    layout = Some(candidate);
                    order = Some(candidate_order);
                    participants = candidate_participants;
                }
            }
        }
        if !case.subject_os().contains(&subject_os) {
            return Ok(None);
        }
        Ok(Some(Self {
            case,
            subject_os,
            layout,
            order,
            participants,
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

    /// Returns a fact from the participant's planned realization.
    /// Returns `None` for unknown participants or facts, and for typed cases.
    pub fn fact(&self, participant: &str, fact: &str) -> Option<&str> {
        self.participants
            .get(participant)?
            .facts
            .iter()
            .find(|declaration| declaration.name == fact)
            .map(|declaration| declaration.value)
    }

    /// Returns the participant's effective configuration for its planned realization.
    /// Returns `None` for unknown participants and typed cases.
    pub fn effective_config(&self, participant: &str) -> Option<&EffectiveConfig> {
        self.participants
            .get(participant)
            .map(|participant| &participant.config)
    }
}

/// Builds effective configurations, checks endpoints, connections and configuration address references
/// in participant name order, then computes create and start orders.
fn plan_participants(
    file: &CaseFile, templates: &FileTemplates, layout: &Layout, types: &[&dyn ParticipantType],
) -> Result<(BTreeMap<String, PlannedParticipant>, DependencyOrder), String> {
    let mut participants = BTreeMap::new();
    let mut participant_endpoints = BTreeMap::new();
    for (name, realization) in layout.iter() {
        let entry = &file.participants[name];
        let participant_type =
            find_type(types, &entry.participant_type).expect("laid-out participants have registered types");
        let holder = describe_participant(name, &entry.participant_type, realization);
        let invalid_config = |message| format!("{holder} has invalid configuration: {message}");
        let mut values = CaseValues::for_os(&entry.config, realization.os);
        references::resolve_config_references(file, layout, types, name, &mut values)?;
        references::check_file_references(file, templates, layout, types, name, &values)?;
        participant_type
            .validate_config(realization, &values)
            .map_err(&invalid_config)?;
        let defaults = participant_type.default_env(realization, &values);
        let config = EffectiveConfig::assemble(defaults, values.clone());
        let endpoints = participant_type
            .endpoints(realization, &config)
            .map_err(invalid_config)?;
        let consumed = participant_type.consumes(realization, &config);
        for endpoint in config.endpoints.keys() {
            if !endpoints.contains_key(endpoint) {
                return Err(format!(
                    "{holder} overrides endpoint '{endpoint}', which it does not serve; it serves {}",
                    declared_names(&endpoints.keys().map(String::as_str).collect::<Vec<_>>()),
                ));
            }
        }
        participant_endpoints.insert(
            name.to_string(),
            ParticipantEndpoints {
                served: endpoints,
                consumed,
                values,
            },
        );
        participants.insert(
            name.to_string(),
            PlannedParticipant {
                facts: participant_type.facts(realization),
                config,
            },
        );
    }
    check_connections(file, layout, &participant_endpoints)?;
    references::check_address_references(file, templates, layout, &participant_endpoints)?;
    let order = dependency_order(file, templates, layout, &participant_endpoints)?;
    Ok((participants, order))
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
                if participants[&participant]
                    .facts
                    .iter()
                    .any(|declaration| declaration.name == fact)
                {
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
        endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind},
        layout::{Realization, RealizationKind},
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
            &self, _realization: Realization, _config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            Ok([
                ("tcp", EndpointKind::Tcp),
                ("udp", EndpointKind::Udp),
                ("http", EndpointKind::Http),
                ("grpc", EndpointKind::Grpc),
                ("datagram", EndpointKind::UnixDatagram),
                ("stream", EndpointKind::UnixStream),
                ("pipe", EndpointKind::NamedPipe),
            ]
            .into_iter()
            .map(|(name, kind)| (name.into(), Endpoint { kind }))
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

        fn consumes(&self, _realization: Realization, _config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
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
                    delivery: Delivery::Env(BTreeSet::from(["TARGET".into()])),
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
        participant_types: &[&FileServer],
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
        CasePlan::compile(case, os, &FILE_REGISTRIES)
            .map_err(normalize)
            .map(Option::unwrap)
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
                            delivery: Delivery::Env(BTreeSet::from(["SHARED".into()])),
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
            &SharedDeliveryClient,
        ],
        steps: STEPS,
    };
    const CONFIG_LOADER: CaseLoader<'static, FixtureTypes> =
        CaseLoader::new(CONFIG_REGISTRIES, FixtureTypes, "fixture-participants");

    pub(super) fn compile(yaml: &str, os: Os) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
        let case = CONFIG_LOADER
            .load_content(yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        CasePlan::compile(case, os, &CONFIG_REGISTRIES)
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
            let plan = CasePlan::compile(case, os, &CONFIG_REGISTRIES).unwrap().unwrap();
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
  d: {type: server, config: {endpoints: {z-typo: {port: 80}, a-typo: {port: 80}}}}
  e: {type: client, config: {env: {SIDE_PORT: '80', SIDE_HOST: literal}}, connect: {z-typo: subject.typo, a-typo: subject.typo}}
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
                "z-typo: {port: 80}",
                "",
                "participant 'd' of type 'server' as windows-container on windows overrides endpoint 'z-typo'",
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
    config: {env: {ADDRESS: '{{fixture.endpoints.missing.url}}', ID: '{{fixture.late.container_id}}'}}
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
            let message = CasePlan::compile(case, Os::Linux, &registries)
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
        let plan = CasePlan::compile(case, Os::Linux, &registries).unwrap().unwrap();
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

    const CONNECTION_CASE: &str = r#"timeout: 1s
subject_os: [linux]
participants:
  client: {type: client, subject: true, connect: {upstream: server.api}}
  server: {type: server}
procedure: []
"#;

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
            .replace("type: server", "type: every-kind-server, os: linux")
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
                ("SIDE_PORT".into(), "80".into())
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
            BTreeMap::from([("SHARED".into(), "default".into())])
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
    fn an_endpoint_override_must_name_an_endpoint_the_type_serves_with_that_override() {
        let yaml = CONNECTION_CASE.replace(
            "type: server}",
            "type: server, config: {endpoints: {extra: {port: 90}}}}",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(plan.effective_config("server").unwrap().endpoints["extra"].port, 90);
        let message = compile(&yaml.replace("extra:", "typo:"), Os::Linux)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'server' of type 'server' as linux-container on linux overrides endpoint 'typo', which it does not serve; it serves api, metrics"
        );
    }

    #[test]
    fn an_endpoint_derivation_error_fails_compile_naming_the_participant_and_realization() {
        let yaml = CONNECTION_CASE.replace("type: server}", "type: server, config: {env: {PORT: x}}}");
        let message = compile(&yaml, Os::Linux).err().unwrap().to_string();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'server' of type 'server' as linux-container on linux has invalid configuration: PORT 'x' is not a number"
        );
        assert!(compile(&yaml.replace("PORT: x", "PORT: '80'"), Os::Linux)
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
            let message = CasePlan::compile(case, Os::Linux, &registries).err().unwrap().to_string();
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
        let plan = CasePlan::compile(case, Os::Linux, &registries).unwrap().unwrap();
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
        assert_eq!(CasePlan::compile(case, Os::Linux, &registries).err().unwrap().to_string(),
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
        let message = CasePlan::compile(case, Os::Linux, &registries)
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
        let plan = CasePlan::compile(case, Os::Linux, &registries).unwrap().unwrap();
        assert_eq!(plan.fact("subject", "file"), Some("/logs/output.log"));
    }

    #[test]
    fn a_participant_with_its_own_os_needs_facts_only_from_its_realization_on_that_os() {
        let yaml = FACT_CASE
            .replace("[linux]", "[macos]")
            .replace("participants:", "participants:\n  other: {type: server, os: linux}")
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
            "participants:\n  other: {type: server, realization: {kind: linux-container}}",
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
            .replace("participants:", "participants:\n  other: {type: server, os: macos}")
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
            let plan = CasePlan::compile(case, os, &REGISTRIES).unwrap();
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
            let message = CasePlan::compile(case, os, &REGISTRIES).err().unwrap().to_string();
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
            let plan = CasePlan::compile(case, os, &REGISTRIES).unwrap();
            if os == Os::Linux {
                assert!(plan.is_none());
            } else {
                let plan = plan.unwrap();
                assert_eq!(plan.subject_os(), os);
                assert_eq!(plan.case().name(), "alpha");
                assert!(plan.layout().is_none());
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
        let message = CasePlan::compile(case, Os::Linux, &REGISTRIES)
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
