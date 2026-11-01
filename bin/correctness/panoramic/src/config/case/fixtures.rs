//! Participant and typed-case declarations for loader tests.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use serde_json::Value;

use super::super::{
    participant_check::{FactDeclaration, ParticipantDeclaration},
    step_check::{decode_fields, Need, Step, StepDeclaration},
    template::Template,
};
use super::{case_name, CaseLoader, Os, Registries, TypedCase, TypedCaseLoader};
use crate::test::TestSuite;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Probe {
    pub(crate) on: String,
    pub(crate) process: Option<String>,
    #[serde(default)]
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

const PROBE: StepDeclaration = StepDeclaration {
    name: "probe",
    template_fields: &["path"],
    build: |fields| Ok(Box::new(decode_fields::<Probe>(fields)?)),
};
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

const MARKER: StepDeclaration = StepDeclaration {
    name: "marker",
    template_fields: &[],
    build: |fields| Ok(Box::new(decode_fields::<Marker>(fields)?)),
};
pub(crate) const STEPS: &[&StepDeclaration] = &[&MARKER, &PROBE];
pub(crate) const SERVER: ParticipantDeclaration = ParticipantDeclaration {
    name: "server",
    config_files: &[],
    endpoints: &[],
    consumes: &[],
    params: &[],
    processes: &["server", "worker"],
    facts: &[FactDeclaration {
        name: "file",
        values: &[(Os::Linux, "/logs/output.log"), (Os::Windows, r"C:\logs\output.log")],
    }],
};
const CLIENT: ParticipantDeclaration = ParticipantDeclaration {
    name: "client",
    facts: &[],
    processes: &[],
    ..SERVER
};
pub(crate) const TYPES: &[&ParticipantDeclaration] = &[&CLIENT, &SERVER];

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
            suite: TestSuite::Correctness,
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

pub(crate) const LOADER: CaseLoader<'static, FixtureTypes> = CaseLoader::new(REGISTRIES, FixtureTypes);

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
