//! Planning checks for reserved variables and process declarations.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use super::*;
use crate::{
    case::{
        fixtures::{FixtureConfig, FixtureTypes, REGISTRIES},
        CaseLoader,
    },
    endpoint::{ConsumedEndpoint, Delivery, EndpointKind},
    layout::{Realization, RealizationKind},
    participant::{ExitChannel, OutputChannel, ProcessSpec},
};

#[derive(Default)]
struct Declarations {
    processes: &'static [&'static str],
    specs: &'static [ProcessSpec],
    reserved_default: bool,
    reserved_delivery: bool,
}

impl ParticipantType for Declarations {
    fn name(&self) -> &'static str {
        "checked"
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
        ]
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[]
    }
    fn default_env(&self, _: Realization, _: &CaseValues) -> BTreeMap<String, String> {
        if self.reserved_default {
            BTreeMap::from([("PANORAMIC_FUTURE".into(), "default".into())])
        } else {
            BTreeMap::new()
        }
    }
    fn endpoints(&self, _: Realization, _: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        Ok(BTreeMap::from([("api".into(), Endpoint::http(Port::Fixed(8080)))]))
    }
    fn consumes(&self, _: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        if !self.reserved_delivery {
            return BTreeMap::new();
        }
        BTreeMap::from([(
            "upstream".into(),
            ConsumedEndpoint {
                accepts: vec![EndpointKind::Http],
                required: false,
                delivery: Delivery::Env {
                    keys: BTreeSet::from(["PANORAMIC_TARGET".into()]),
                    value: |address| BTreeMap::from([("PANORAMIC_TARGET".into(), address.url.clone())]),
                },
            },
        )])
    }
    fn processes(&self) -> &'static [&'static str] {
        self.processes
    }
    fn process_specs(&self, realization: Realization) -> &'static [ProcessSpec] {
        if realization.os == Os::Linux {
            self.specs
        } else {
            &[]
        }
    }
}

const CASE: &str = "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: checked, subject: true}\nprocedure: [{step: noop}]";

fn compile(declarations: &Declarations, yaml: &str, os: Os) -> Result<CasePlan<FixtureConfig>, GenericError> {
    let registries = Registries {
        participant_types: &[declarations],
        ..REGISTRIES
    };
    let case = CaseLoader::new(registries, FixtureTypes)
        .load_content("cases", yaml, "alpha", Path::new("cases/alpha/config.yaml"))?
        .remove(0);
    Ok(Planner::new(registries, Default::default())?
        .compile(case, os)?
        .into_plan()
        .unwrap())
}

#[test]
fn a_case_reserved_variable_is_rejected_on_the_subject_os_that_sets_it() {
    let yaml = CASE.replace("[linux]", "[linux, macos]").replace(
        "subject: true",
        "subject: true, config: {os: {macos: {env: {PANORAMIC_FUTURE: value}}}}",
    );
    for selected in [Os::Linux, Os::MacOs] {
        let error = compile(&Declarations::default(), &yaml, selected)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', participant 'subject' of type 'checked' as host-process-group on macos sets reserved env 'PANORAMIC_FUTURE' through its case configuration; PANORAMIC_ is reserved for the harness");
    }
    let allowed = yaml.replace("PANORAMIC_FUTURE", "PRODUCT_SETTING");
    assert_eq!(
        compile(&Declarations::default(), &allowed, Os::Linux)
            .unwrap()
            .effective_config("subject")
            .unwrap()
            .env,
        BTreeMap::new()
    );
    assert_eq!(
        compile(&Declarations::default(), &allowed, Os::MacOs)
            .unwrap()
            .effective_config("subject")
            .unwrap()
            .env["PRODUCT_SETTING"],
        "value"
    );
}

#[test]
fn reserved_variables_name_whether_the_case_or_a_type_default_set_them() {
    let declarations = Declarations {
        reserved_default: true,
        ..Default::default()
    };
    for (yaml, source) in [
        (CASE.to_owned(), "type default"),
        (
            CASE.replace(
                "subject: true",
                "subject: true, config: {env: {PANORAMIC_FUTURE: case}}",
            ),
            "case configuration",
        ),
    ] {
        let error = compile(&declarations, &yaml, Os::Linux).err().unwrap().to_string();
        assert!(
            error.ends_with(&format!(
                "sets reserved env 'PANORAMIC_FUTURE' through its {source}; PANORAMIC_ is reserved for the harness"
            )),
            "{error}"
        );
    }
}

#[test]
fn a_connection_cannot_deliver_a_reserved_variable() {
    let declarations = Declarations {
        reserved_delivery: true,
        ..Default::default()
    };
    let yaml = CASE
        .replace("subject: true", "subject: true, connect: {upstream: server.api}")
        .replace("participants:\n", "participants:\n  server: {type: checked}\n");
    let error = compile(&declarations, &yaml, Os::Linux).err().unwrap().to_string();
    assert_eq!(error, "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'subject' of type 'checked' as linux-container on linux delivers reserved env 'PANORAMIC_TARGET' through connection 'upstream'; PANORAMIC_ is reserved for the harness");
}

#[test]
fn process_names_are_plain_file_names_even_without_declared_channels() {
    const INVALID: &[&[&str]] = &[
        &[""],
        &["."],
        &[".."],
        &["../escape"],
        &["nested/name"],
        &["nested\\name"],
    ];
    for &processes in INVALID {
        let declarations = Declarations {
            processes,
            ..Default::default()
        };
        let error = compile(&declarations, CASE, Os::Linux).err().unwrap().to_string();
        assert!(
            error.ends_with(&format!(
                "declares process '{}'; process names must be plain file names",
                processes[0]
            )),
            "{error}"
        );
    }
    let declarations = Declarations {
        processes: &["main"],
        ..Default::default()
    };
    assert!(compile(&declarations, CASE, Os::Linux).is_ok());
}

#[test]
fn process_specifications_are_checked_on_the_realization_that_declares_them() {
    const MAIN: ProcessSpec = ProcessSpec {
        name: "main",
        exit: ExitChannel::Container,
        output: OutputChannel::Container,
    };
    const CASES: &[(&[ProcessSpec], &str)] = &[
        (
            &[ProcessSpec { name: "other", ..MAIN }],
            "declares no exit channel for process 'main'",
        ),
        (&[MAIN, MAIN], "declares two exit channels for process 'main'"),
        (
            &[ProcessSpec {
                exit: ExitChannel::Record { file: "../exit" },
                ..MAIN
            }],
            "declares exit record file '../exit' for process 'main'; record files must be plain file names",
        ),
    ];
    for &(specs, expected) in CASES {
        let declarations = Declarations {
            processes: &["main"],
            specs,
            ..Default::default()
        };
        let error = compile(&declarations, CASE, Os::Linux).err().unwrap().to_string();
        assert!(error.ends_with(expected), "{error}");
        assert!(compile(&declarations, &CASE.replace("[linux]", "[macos]"), Os::MacOs).is_ok());
    }
    let declarations = Declarations {
        processes: &["main"],
        specs: &[MAIN],
        ..Default::default()
    };
    assert!(compile(&declarations, CASE, Os::Linux).is_ok());
}
