//! Case-file grammar for named participants and their procedure.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use saluki_error::{generic_error, GenericError};
use serde::{
    de::{value::MapAccessDeserializer, Error as _, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::Value;

use super::duration::CaseDuration;
use crate::layout::{Os, RealizationKind};

/// A case's identity, participants and procedure.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseFile {
    /// Reporting name. Defaults to the directory name and must match it when supplied.
    pub name: Option<String>,
    /// Reporting description. Defaults to none.
    pub description: Option<String>,
    /// Required case deadline, greater than zero; has no default.
    pub timeout: CaseDuration,
    /// Required, nonempty list of supported subject operating systems, without duplicates; has no default.
    pub subject_os: Vec<Os>,
    /// Required participant instances keyed by name. Must contain at least one; has no default.
    pub participants: BTreeMap<String, ParticipantEntry>,
    /// Required, nonempty list of ordered steps and parallel blocks; has no default.
    #[serde(deserialize_with = "deserialize_procedure")]
    pub procedure: Vec<ProcedureNode>,
}

impl CaseFile {
    /// Checks the subject OS list and participant count, returning the supported OS set.
    ///
    /// # Errors
    ///
    /// Returns an error naming the case and source if the OS list is empty or repeats an OS,
    /// or the case declares no participants.
    pub(super) fn check(&self, name: &str, source: &Path) -> Result<BTreeSet<Os>, GenericError> {
        if self.subject_os.is_empty() {
            return Err(generic_error!(
                "case '{}' in {} has an empty subject_os list; name at least one of 'linux', 'macos' or 'windows'",
                name,
                source.display()
            ));
        }
        let mut subject_os = BTreeSet::new();
        for &os in &self.subject_os {
            if !subject_os.insert(os) {
                return Err(generic_error!(
                    "case '{}' in {} lists subject OS '{}' more than once",
                    name,
                    source.display(),
                    os.name()
                ));
            }
        }
        if self.participants.is_empty() {
            return Err(generic_error!(
                "case '{}' in {} declares no participants",
                name,
                source.display()
            ));
        }
        Ok(subject_os)
    }
}

/// One named instance of a participant type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantEntry {
    /// Required participant type name; has no default.
    #[serde(rename = "type")]
    pub participant_type: String,
    /// Whether this participant is the test subject; exactly one participant sets it. Defaults to false.
    #[serde(default)]
    pub subject: bool,
    /// Participant OS for a non-subject participant; the subject must not set it. Defaults to the subject OS.
    pub os: Option<Os>,
    /// Requested execution mechanism. Defaults to the layout's choice.
    pub realization: Option<RequestedRealization>,
    /// Product configuration. Defaults to empty maps.
    #[serde(default)]
    pub config: ParticipantConfig,
    /// Consumed endpoint names mapped to other participants' endpoints. Defaults to empty.
    #[serde(default)]
    pub connect: BTreeMap<String, ConnectionTarget>,
}

/// An execution mechanism explicitly requested by the case.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedRealization {
    /// Required execution mechanism; has no default.
    pub kind: RealizationKind,
    /// Options for the realization kind; each kind defines which it accepts, and none defines any. Defaults to empty.
    #[serde(default)]
    pub(super) options: BTreeMap<String, Value>,
}

/// Product configuration supplied to a participant.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantConfig {
    /// Process environment variables; values must be YAML strings. Defaults to empty.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: BTreeMap<String, String>,
    /// Declared configuration file names mapped to case-relative sources. Defaults to empty.
    #[serde(default)]
    pub files: BTreeMap<String, FileSource>,
    /// Listener overrides keyed by declared endpoint name. Defaults to empty.
    #[serde(default)]
    pub endpoints: BTreeMap<String, EndpointOverride>,
    /// Participant-type-specific parameters, preserving YAML value types. Defaults to empty.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
    /// Configuration overlays selected by participant OS. Defaults to empty.
    #[serde(default)]
    pub os: BTreeMap<Os, ConfigOverlay>,
}

/// Product configuration overlaid for one participant OS.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigOverlay {
    /// Environment overrides; values must be YAML strings. Defaults to empty.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: BTreeMap<String, String>,
    /// Case-relative configuration file sources, replacing base entries by name. Defaults to empty.
    #[serde(default)]
    pub files: BTreeMap<String, FileSource>,
    /// Participant-type-specific parameters, replacing whole base values by name. Defaults to empty.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

/// A case-relative file or directory supplied to a participant.
#[derive(Clone, Debug, PartialEq)]
pub struct FileSource {
    /// Required nonempty path within the case directory, stored as written. Must resolve to an existing file or
    /// directory; symlink targets may be outside the case directory. Has no default.
    pub from: PathBuf,
    /// Whether the source holds template references. When true, it must be a regular UTF-8 file.
    /// The loader reads only template sources, once each, and checks their references like `env` values.
    /// Defaults to false.
    pub template: bool,
}

impl<'de> Deserialize<'de> for FileSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FileSourceVisitor;

        impl<'de> Visitor<'de> for FileSourceVisitor {
            type Value = FileSource;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a case-relative path or a {from, template} map")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(FileSource {
                    from: PathBuf::from(value),
                    template: false,
                })
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Source {
                    from: PathBuf,
                    #[serde(default)]
                    template: bool,
                }

                let source = Source::deserialize(MapAccessDeserializer::new(map))?;
                Ok(FileSource {
                    from: source.from,
                    template: source.template,
                })
            }
        }

        deserializer.deserialize_any(FileSourceVisitor)
    }
}

/// A listener port supplied by the case.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EndpointOverride {
    /// Required nonzero listener port, checked during planning; has no default.
    pub(crate) port: u16,
}

/// A connection target written as `participant.endpoint`.
#[derive(Debug, PartialEq)]
pub struct ConnectionTarget {
    /// Required name of the participant serving the endpoint; has no default.
    pub(crate) participant: String,
    /// Required endpoint name within the participant; has no default.
    pub(crate) endpoint: String,
}

impl<'de> Deserialize<'de> for ConnectionTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.split_once('.') {
            Some((participant, endpoint))
                if !participant.is_empty() && !endpoint.is_empty() && !endpoint.contains('.') =>
            {
                Ok(Self {
                    participant: participant.to_string(),
                    endpoint: endpoint.to_string(),
                })
            }
            _ => Err(D::Error::custom(format!(
                "connection '{value}' must have the form 'participant.endpoint'"
            ))),
        }
    }
}

/// One sequential step or a block of concurrent steps.
#[derive(Debug)]
pub enum ProcedureNode<S = StepNode> {
    /// A single step, with its declaration-specific fields.
    Step(S),
    /// A nonempty block of steps to run concurrently.
    Parallel(Vec<S>),
}

impl<'de> Deserialize<'de> for ProcedureNode<StepNode> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProcedureVisitor;

        impl<'de> Visitor<'de> for ProcedureVisitor {
            type Value = ProcedureNode;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a procedure node mapping")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut step = None;
                let mut parallel = None;
                let mut fields = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "step" => step = Some(map.next_value::<String>()?),
                        "parallel" => parallel = Some(map.next_value::<Vec<StepNode>>()?),
                        _ => {
                            fields.insert(key, map.next_value::<Value>()?);
                        }
                    }
                }
                match (step, parallel) {
                    (Some(_), Some(_)) => Err(A::Error::custom(
                        "a procedure node must have 'step' or 'parallel', not both",
                    )),
                    (None, None) => Err(A::Error::custom("a procedure node must have 'step' or 'parallel'")),
                    (Some(name), None) => Ok(ProcedureNode::Step(StepNode { name, fields })),
                    (None, Some(steps)) => {
                        if let Some(key) = fields.keys().next() {
                            return Err(A::Error::custom(format!(
                                "a 'parallel' block takes no other fields; found '{key}'"
                            )));
                        }
                        if steps.is_empty() {
                            return Err(A::Error::custom("a 'parallel' block must contain at least one step"));
                        }
                        Ok(ProcedureNode::Parallel(steps))
                    }
                }
            }
        }

        deserializer.deserialize_any(ProcedureVisitor)
    }
}

fn deserialize_procedure<'de, D>(deserializer: D) -> Result<Vec<ProcedureNode>, D::Error>
where
    D: Deserializer<'de>,
{
    let procedure = Vec::<ProcedureNode>::deserialize(deserializer)?;
    if procedure.is_empty() {
        return Err(D::Error::custom("a procedure must contain at least one step"));
    }
    Ok(procedure)
}

/// A step name and the YAML fields its declaration decodes.
#[derive(Debug)]
pub struct StepNode {
    /// Required step declaration name; has no default.
    pub name: String,
    /// Declaration-specific fields, preserving YAML value types; decoded by the step's declaration. Defaults to empty.
    pub fields: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for StepNode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StepVisitor;

        impl<'de> Visitor<'de> for StepVisitor {
            type Value = StepNode;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a procedure node mapping")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut name = None;
                let mut fields = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "step" => name = Some(map.next_value::<String>()?),
                        "parallel" => {
                            return Err(A::Error::custom(
                                "a 'parallel' block cannot contain another 'parallel' block",
                            ));
                        }
                        _ => {
                            fields.insert(key, map.next_value::<Value>()?);
                        }
                    }
                }
                let name = name.ok_or_else(|| A::Error::custom("a step in a 'parallel' block must have 'step'"))?;
                Ok(StepNode { name, fields })
            }
        }

        deserializer.deserialize_any(StepVisitor)
    }
}

/// Deserializes environment variables as YAML strings.
///
/// Checks scalar types before string conversion; quote values such as `"true"` and `"8125"`.
///
/// # Errors
///
/// Returns an error naming the first variable whose value is not a YAML string.
pub fn deserialize_env_map<'de, D, M>(deserializer: D) -> Result<M, D::Error>
where
    D: Deserializer<'de>,
    M: FromIterator<(String, String)>,
{
    BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?
        .into_iter()
        .map(|(name, value)| match value {
            serde_json::Value::String(value) => Ok((name, value)),
            _ => Err(D::Error::custom(format!(
                "environment variable '{}' must be a YAML string: quote any value that would otherwise \
                 parse as a boolean or a number (\"true\", \"8125\")",
                name
            ))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::super::fixtures::{self, minimal_procedure, write_case, NO_PARTICIPANT_FIELDS, PARTICIPANT_FIELDS};
    use super::*;

    fn load_error(yaml: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "alpha", yaml);
        let error = fixtures::LOADER.load_file(&path).err().expect("invalid case must fail");
        let message = format!("{error:#}");
        assert!(
            message.contains(&path.canonicalize().unwrap().display().to_string()),
            "{message}"
        );
        message
    }

    #[test]
    fn a_case_file_parses_every_documented_field() {
        let file: CaseFile = serde_saphyr::from_str(
            r#"
name: alpha
description: A portable case
timeout: 60s
subject_os: [linux, macos, windows]
participants:
  subject:
    type: server
    subject: true
    realization:
      kind: linux-container
      options: {host_pid: true}
    config:
      env: {LISTEN_EXTERNAL: "true"}
      files:
        scripts: ./scripts/
        settings.yaml: {from: settings-base.yaml, template: true}
      endpoints: {grpc: {port: 4317}}
      params: {nested: {enabled: true, count: 3}}
      os:
        windows:
          env: {OS_NAME: windows}
          files: {scripts: windows/scripts}
          params: {command: [pwsh, -File, send.ps1]}
    connect: {other: other.api}
  other:
    type: client
    os: linux
procedure:
  - step: wait_ready
    on: subject
  - parallel:
      - step: log_not_contains
        on: subject
        pattern: "panic|PANIC"
        regex: true
        during: 10s
"#,
        )
        .unwrap();
        assert_eq!(file.name.as_deref(), Some("alpha"));
        assert_eq!(file.description.as_deref(), Some("A portable case"));
        assert_eq!(Duration::from(file.timeout), Duration::from_secs(60));
        assert_eq!(file.subject_os, [Os::Linux, Os::MacOs, Os::Windows]);
        assert_eq!(
            file.check("alpha", Path::new("config.yaml")).unwrap(),
            BTreeSet::from([Os::Linux, Os::MacOs, Os::Windows])
        );
        assert_eq!(file.participants.len(), 2);
        let subject = &file.participants["subject"];
        assert_eq!(subject.participant_type, "server");
        assert!(subject.subject);
        assert_eq!(subject.os, None);
        let realization = subject.realization.as_ref().unwrap();
        assert_eq!(realization.kind, RealizationKind::LinuxContainer);
        assert_eq!(realization.options, BTreeMap::from([("host_pid".into(), json!(true))]));
        assert_eq!(
            subject.config.env,
            BTreeMap::from([("LISTEN_EXTERNAL".into(), "true".into())])
        );
        assert_eq!(
            subject.config.files,
            BTreeMap::from([
                (
                    "scripts".into(),
                    FileSource {
                        from: "./scripts/".into(),
                        template: false
                    }
                ),
                (
                    "settings.yaml".into(),
                    FileSource {
                        from: "settings-base.yaml".into(),
                        template: true
                    }
                ),
            ])
        );
        assert_eq!(subject.config.endpoints.len(), 1);
        assert_eq!(subject.config.endpoints["grpc"].port, 4317);
        assert_eq!(
            subject.config.params,
            BTreeMap::from([("nested".into(), json!({"enabled": true, "count": 3}))])
        );
        assert_eq!(subject.config.os.len(), 1);
        let overlay = &subject.config.os[&Os::Windows];
        assert_eq!(overlay.env, BTreeMap::from([("OS_NAME".into(), "windows".into())]));
        assert_eq!(
            overlay.files,
            BTreeMap::from([(
                "scripts".into(),
                FileSource {
                    from: "windows/scripts".into(),
                    template: false
                }
            )])
        );
        assert_eq!(
            overlay.params,
            BTreeMap::from([("command".into(), json!(["pwsh", "-File", "send.ps1"]))])
        );
        assert_eq!(
            subject.connect,
            BTreeMap::from([(
                "other".into(),
                ConnectionTarget {
                    participant: "other".into(),
                    endpoint: "api".into()
                }
            )])
        );
        let other = &file.participants["other"];
        assert_eq!(other.participant_type, "client");
        assert!(!other.subject);
        assert_eq!(other.os, Some(Os::Linux));
        assert!(other.realization.is_none());
        assert!(other.config.env.is_empty());
        assert!(other.config.files.is_empty());
        assert!(other.config.endpoints.is_empty());
        assert!(other.config.params.is_empty());
        assert!(other.config.os.is_empty());
        assert!(other.connect.is_empty());
        let [ProcedureNode::Step(step), ProcedureNode::Parallel(steps)] = file.procedure.as_slice() else {
            panic!("expected one step followed by a parallel block");
        };
        assert_eq!(step.name, "wait_ready");
        assert_eq!(step.fields, BTreeMap::from([("on".into(), json!("subject"))]));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "log_not_contains");
        assert_eq!(
            steps[0].fields,
            BTreeMap::from([
                ("on".into(), json!("subject")),
                ("pattern".into(), json!("panic|PANIC")),
                ("regex".into(), json!(true)),
                ("during".into(), json!("10s")),
            ])
        );
    }

    #[test]
    fn an_unknown_field_is_rejected_at_every_level() {
        for (yaml, key) in [
            (format!("{PARTICIPANT_FIELDS}timout: 2s\n"), "timout"),
            (PARTICIPANT_FIELDS.replace("subject: true", "subjet: true"), "subjet"),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "realization: {kind: linux-container, optons: {}}"),
                "optons",
            ),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "config: {environ: {}}"),
                "environ",
            ),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "config: {files: {settings: {from: x, templat: true}}}"),
                "templat",
            ),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "config: {endpoints: {api: {port: 4317, prot: 4318}}}"),
                "prot",
            ),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "config: {os: {windows: {prams: {}}}}"),
                "prams",
            ),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "config: {os: {windows: {endpoints: {}}}}"),
                "endpoints",
            ),
            (
                PARTICIPANT_FIELDS.replace("subject: true", "config: {os: {windows: {os: {}}}}"),
                "os",
            ),
            (
                PARTICIPANT_FIELDS.replace(
                    minimal_procedure!(),
                    "procedure: [{parallel: [{step: wait_ready}], timout: 1s}]",
                ),
                "timout",
            ),
        ] {
            let message = load_error(&yaml);
            assert!(
                message.contains(&format!("'{key}'")) || message.contains(&format!("`{key}`")),
                "{message}"
            );
        }
    }

    #[test]
    fn subject_os_must_be_nonempty_unique_and_spelled_as_in_the_schema() {
        for (list, expected) in [
            (
                "[]",
                "has an empty subject_os list; name at least one of 'linux', 'macos' or 'windows'",
            ),
            ("[linux, linux]", "lists subject OS 'linux' more than once"),
            ("[mac]", "unknown variant `mac`"),
        ] {
            let message = load_error(&PARTICIPANT_FIELDS.replace("[linux]", list));
            assert!(message.contains(expected), "{message}");
        }
        for (spelling, expected) in [("linux", Os::Linux), ("macos", Os::MacOs), ("windows", Os::Windows)] {
            let os: Os = serde_saphyr::from_str(spelling).unwrap();
            assert_eq!(os, expected);
            assert_eq!(os.name(), spelling);
        }
    }

    #[test]
    fn a_case_file_must_declare_a_participant() {
        let message = load_error(NO_PARTICIPANT_FIELDS);
        assert!(message.contains("case 'alpha'"), "{message}");
        assert!(message.contains("declares no participants"), "{message}");
    }

    #[test]
    fn a_participant_name_declared_twice_is_rejected() {
        let message = load_error(&PARTICIPANT_FIELDS.replace(
            "  subject: {type: server, subject: true}",
            "  subject: {type: server, subject: true}\n  subject: {type: client}",
        ));
        assert!(message.contains("duplicate mapping key: subject"), "{message}");
    }

    #[test]
    fn connections_must_name_one_participant_and_one_endpoint() {
        for value in ["other", "other.", ".api", "a.b.c", ""] {
            let yaml = PARTICIPANT_FIELDS.replace("subject: true", &format!("connect: {{other: '{value}'}}"));
            let message = load_error(&yaml);
            assert!(
                message.contains(&format!(
                    "connection '{value}' must have the form 'participant.endpoint'"
                )),
                "{message}"
            );
        }
        let endpoint: ConnectionTarget = serde_saphyr::from_str("other.api").unwrap();
        assert_eq!(
            endpoint,
            ConnectionTarget {
                participant: "other".into(),
                endpoint: "api".into()
            }
        );
    }

    #[test]
    fn an_empty_procedure_is_a_load_error() {
        let message = load_error(&PARTICIPANT_FIELDS.replace(minimal_procedure!(), "procedure: []"));
        assert!(
            message.contains("a procedure must contain at least one step"),
            "{message}"
        );
        assert!(message.contains("line "), "expected a YAML location: {message}");
    }

    #[test]
    fn procedure_nodes_are_one_step_or_one_parallel_block_of_steps() {
        for (node, expected) in [
            (
                "{step: wait_ready, parallel: [{step: wait_ready}]}",
                "a procedure node must have 'step' or 'parallel', not both",
            ),
            ("{on: subject}", "a procedure node must have 'step' or 'parallel'"),
            (
                "{parallel: [{step: wait_ready}], on: subject}",
                "a 'parallel' block takes no other fields; found 'on'",
            ),
            ("{parallel: []}", "a 'parallel' block must contain at least one step"),
            (
                "{parallel: [{parallel: [{step: wait_ready}]}]}",
                "a 'parallel' block cannot contain another 'parallel' block",
            ),
            (
                "{parallel: [{on: subject}]}",
                "a step in a 'parallel' block must have 'step'",
            ),
            ("wait_ready", "a procedure node mapping"),
            ("[wait_ready]", "a procedure node mapping"),
            ("true", "a procedure node mapping"),
            ("null", "a procedure node mapping"),
            ("{parallel: [wait_ready]}", "a procedure node mapping"),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace(minimal_procedure!(), &format!("procedure:\n  - {node}"));
            let message = load_error(&yaml);
            assert!(message.contains(expected), "{message}");
            if node == "{on: subject}" {
                assert!(!message.contains("not both"), "{message}");
            }
            assert!(message.contains("line "), "expected a YAML location: {message}");
        }
        let nodes: Vec<ProcedureNode> =
            serde_saphyr::from_str("[{step: wait_ready}, {parallel: [{step: wait_ready}, {step: log_contains}]}]")
                .unwrap();
        let [ProcedureNode::Step(step), ProcedureNode::Parallel(steps)] = nodes.as_slice() else {
            panic!("expected one step and one parallel block");
        };
        assert_eq!(step.name, "wait_ready");
        assert!(step.fields.is_empty());
        assert_eq!(
            steps.iter().map(|step| step.name.as_str()).collect::<Vec<_>>(),
            ["wait_ready", "log_contains"]
        );
    }

    #[test]
    fn step_fields_keep_their_yaml_scalar_types() {
        for yaml in [
            r#"{step: log_not_contains, pattern: "true", regex: true, count: 3, during: 10s}"#,
            r#"{parallel: [{step: log_not_contains, pattern: "true", regex: true, count: 3, during: 10s}]}"#,
        ] {
            let node: ProcedureNode = serde_saphyr::from_str(yaml).unwrap();
            let step = match &node {
                ProcedureNode::Step(step) => step,
                ProcedureNode::Parallel(steps) => &steps[0],
            };
            assert_eq!(
                step.fields,
                BTreeMap::from([
                    ("pattern".into(), json!("true")),
                    ("regex".into(), json!(true)),
                    ("count".into(), json!(3)),
                    ("during".into(), json!("10s")),
                ])
            );
        }
    }

    #[test]
    fn participant_env_and_overlay_env_reject_non_string_values() {
        for config in ["{env: {A: true}}", "{os: {linux: {env: {A: true}}}}"] {
            let message = load_error(&PARTICIPANT_FIELDS.replace("subject: true", &format!("config: {config}")));
            assert!(
                message.contains("environment variable 'A' must be a YAML string"),
                "{message}"
            );
        }
    }

    #[test]
    fn realization_kinds_use_their_schema_spellings() {
        for (spelling, expected) in [
            ("host-process-group", RealizationKind::HostProcessGroup),
            ("linux-container", RealizationKind::LinuxContainer),
            ("windows-container", RealizationKind::WindowsContainer),
            ("kubernetes-pod", RealizationKind::KubernetesPod),
        ] {
            let realization: RequestedRealization = serde_saphyr::from_str(&format!("kind: {spelling}")).unwrap();
            assert_eq!(realization.kind, expected);
            assert!(realization.options.is_empty());
        }
        for spelling in ["kubernetes_pod", "kind"] {
            let error = serde_saphyr::from_str::<RealizationKind>(spelling).unwrap_err();
            assert!(
                error.to_string().contains(&format!("unknown variant `{spelling}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn files_accept_a_path_or_a_from_and_template_map() {
        for (yaml, template) in [
            ("./x", false),
            ("{from: ./x}", false),
            ("{from: ./x, template: true}", true),
        ] {
            let file: FileSource = serde_saphyr::from_str(yaml).unwrap();
            assert_eq!(
                file,
                FileSource {
                    from: "./x".into(),
                    template
                }
            );
        }
        for (yaml, expected) in [
            ("[x]", "a case-relative path or a {from, template} map"),
            ("true", "a case-relative path or a {from, template} map"),
            ("3", "a case-relative path or a {from, template} map"),
            ("null", "a case-relative path or a {from, template} map"),
            ("{template: true}", "missing field `from`"),
        ] {
            let error = serde_saphyr::from_str::<FileSource>(yaml).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn required_case_and_participant_fields_must_be_present() {
        for field in ["timeout", "subject_os", "participants", "procedure"] {
            let yaml = match field {
                "participants" => {
                    PARTICIPANT_FIELDS.replace("participants:\n  subject: {type: server, subject: true}\n", "")
                }
                _ => PARTICIPANT_FIELDS
                    .lines()
                    .filter(|line| !line.starts_with(&format!("{field}:")))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            let message = load_error(&yaml);
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        let message = load_error(&PARTICIPANT_FIELDS.replace("type: server, ", ""));
        assert!(message.contains("missing field `type`"), "{message}");
    }

    #[test]
    fn duplicate_procedure_keys_are_rejected() {
        for node in [
            "{step: wait_ready, step: wait_exit}",
            "{parallel: [{step: wait_ready}], parallel: [{step: wait_exit}]}",
            "{step: wait_ready, on: subject, on: other}",
            "{parallel: [{step: wait_ready, step: wait_exit}]}",
            "{parallel: [{step: wait_ready, on: subject, on: other}]}",
        ] {
            let error = serde_saphyr::from_str::<ProcedureNode>(node).unwrap_err();
            assert!(error.to_string().contains("duplicate"), "{error}");
        }
    }

    #[test]
    fn configuration_overlays_use_os_keys_and_default_to_empty_maps() {
        let config: ParticipantConfig = serde_saphyr::from_str("os: {linux: {}, macos: {}, windows: {}}").unwrap();
        assert_eq!(
            config.os.keys().copied().collect::<Vec<_>>(),
            [Os::Linux, Os::MacOs, Os::Windows]
        );
        for overlay in config.os.values() {
            assert!(overlay.env.is_empty());
            assert!(overlay.files.is_empty());
            assert!(overlay.params.is_empty());
        }
        let message = load_error(&PARTICIPANT_FIELDS.replace("subject: true", "config: {os: {mac: {}}}"));
        assert!(message.contains("unknown variant `mac`"), "{message}");
    }
}
