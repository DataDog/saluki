//! Participant types and typed-case declarations for loader tests.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use serde_json::Value;

use super::{case_name, CaseLoader, Registries, Template, TypedCase, TypedCaseLoader};
use crate::{
    layout::{Os, Realization, RealizationKind},
    participant::{FactDeclaration, ParticipantType},
    step::{decode_fields, Need, Step, StepFactory, StepFields},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Probe {
    pub(crate) on: String,
    pub(crate) process: Option<String>,
    #[serde(default)]
    #[expect(
        dead_code,
        reason = "Only read through derived Debug, which dead-code analysis ignores."
    )]
    pub(crate) values: Vec<Value>,
    pub(crate) path: Option<Template>,
}

impl Step for Probe {
    fn needs(&self) -> Vec<Need> {
        let mut needs = match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Handle(self.on.clone())],
        };
        if let Some(path) = &self.path {
            needs.extend(
                path.references()
                    .map(|(participant, fact)| Need::Fact(participant.into(), fact.into())),
            );
        }
        needs
    }
}

struct ProbeFactory;

impl StepFactory for ProbeFactory {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn template_fields(&self) -> &'static [&'static str] {
        &["path"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Probe>(fields)?))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Marker {
    pub(super) on: String,
}

impl Step for Marker {
    fn needs(&self) -> Vec<Need> {
        vec![Need::Handle(self.on.clone())]
    }
}

struct MarkerFactory;

impl StepFactory for MarkerFactory {
    fn name(&self) -> &'static str {
        "marker"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Marker>(fields)?))
    }
}

pub(crate) const STEPS: &[&dyn StepFactory] = &[&MarkerFactory, &ProbeFactory];
pub(crate) struct Server;

impl ParticipantType for Server {
    fn name(&self) -> &'static str {
        "server"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ]
    }

    fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[FactDeclaration {
                name: "file",
                value: "/logs/output.log",
            }],
            (Os::Windows, RealizationKind::WindowsContainer) => &[FactDeclaration {
                name: "file",
                value: r"C:\logs\output.log",
            }],
            _ => &[],
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["server", "worker"]
    }
}

pub(crate) struct PodServer;

impl ParticipantType for PodServer {
    fn name(&self) -> &'static str {
        "pod-server"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Linux, RealizationKind::KubernetesPod),
        ]
    }

    fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[FactDeclaration {
                name: "file",
                value: "/logs/output.log",
            }],
            _ => &[],
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["server"]
    }
}

struct Client;

impl ParticipantType for Client {
    fn name(&self) -> &'static str {
        "client"
    }

    fn processes(&self) -> &'static [&'static str] {
        &[]
    }
}

pub(crate) const TYPES: &[&dyn ParticipantType] = &[&Client, &Server];

pub(crate) const REGISTRIES: Registries<'static> = Registries {
    participant_types: TYPES,
    steps: STEPS,
};

pub(crate) struct FixtureTypes;

pub(crate) struct FixtureConfig {
    pub(super) case_type: Value,
    pub(super) content: String,
    pub(super) directory: String,
    pub(super) source: PathBuf,
}

impl TypedCaseLoader for FixtureTypes {
    type Config = FixtureConfig;

    fn load(
        &self, case_type: &Value, content: &str, directory: &str, source: &Path,
    ) -> Result<Vec<TypedCase<Self::Config>>, GenericError> {
        if case_type == "empty" {
            return Ok(Vec::new());
        }
        if case_type != "fixture" {
            return Err(generic_error!("Unsupported fixture type: {}", case_type));
        }
        Ok(vec![TypedCase {
            name: case_name(None, directory, source)?,
            description: Some(format!("{directory} fixture")),
            suite: "fixture-typed".to_string(),
            timeout: Duration::from_secs(3),
            subject_os: BTreeSet::from([Os::MacOs, Os::Windows]),
            disabled: true,
            config: FixtureConfig {
                case_type: case_type.clone(),
                content: content.into(),
                directory: directory.into(),
                source: source.into(),
            },
        }])
    }
}

pub(crate) const LOADER: CaseLoader<'static, FixtureTypes> =
    CaseLoader::new(REGISTRIES, FixtureTypes, "fixture-participants");

pub(crate) const PARTICIPANT_FIELDS: &str =
    "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: []\n";
pub(crate) const NO_PARTICIPANT_FIELDS: &str = "timeout: 1s\nsubject_os: [linux]\nparticipants: {}\nprocedure: []\n";

pub(crate) fn write_case(base: &Path, name: &str, content: &str) -> PathBuf {
    let directory = base.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("config.yaml");
    std::fs::write(&path, content).unwrap();
    path
}
