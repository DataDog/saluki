//! Participant types and typed-case declarations for loader tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;

use super::{
    case_name, CaseDuration, CaseLoader, ConnectionTarget, Registries, StepTemplate, TypedCase, TypedCaseLoader,
};
use crate::{
    artifact::ArtifactRole,
    backend::LateFact,
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, EndpointOperation, Port},
    handle::HandleError,
    layout::{Os, Realization, RealizationKind},
    participant::{
        CaseValues, EffectiveConfig, ExitChannel, FactDeclaration, OpContext, Operation, OutputChannel, OutputStream,
        ParticipantType, ProcessSpec,
    },
    probe::{AttemptLimits, ProbeAnswer, POLL_INTERVAL},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, ExitObservation, Need, OutputEnd, Step, StepContext, StepFactory, StepFields, StepKind},
};

pub(crate) fn step_timeout() -> CaseDuration {
    "30s".parse().unwrap()
}

fn handle_failure(error: HandleError) -> (AssertionStatus, String) {
    match error {
        HandleError::Harness { message } => (AssertionStatus::Errored, message),
        HandleError::Product { message } => (AssertionStatus::Failed, message),
    }
}

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
    pub(crate) path: Option<StepTemplate>,
    #[serde(default = "step_timeout")]
    pub(crate) timeout: CaseDuration,
}

#[async_trait]
impl Step for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    fn needs(&self) -> Vec<Need> {
        let participant = match &self.process {
            Some(process) => Need::Process(self.on.clone(), process.clone()),
            None => Need::Handle(self.on.clone()),
        };
        std::iter::once(participant)
            .chain(self.path.iter().flat_map(StepTemplate::needs))
            .collect()
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let result = async {
            if let Some(path) = &self.path {
                let path = cx
                    .resolve(path)
                    .map_err(|message| (AssertionStatus::Errored, message))?;
                match cx
                    .handle(&self.on)
                    .expect("probe needs its participant")
                    .read_file(&path)
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => return Err((AssertionStatus::Failed, format!("file '{path}' is missing"))),
                    Err(error) => return Err(handle_failure(error)),
                }
            }
            if let Some(process) = &self.process {
                match cx
                    .handle(&self.on)
                    .expect("probe needs its participant")
                    .wait_for_exit(process)
                    .await
                {
                    ExitObservation::Exited(status) if status.code == Some(0) => {}
                    ExitObservation::Exited(status) => {
                        return Err((AssertionStatus::Failed, format!("process exited: {status:?}")));
                    }
                    ExitObservation::Undetermined(reason) => return Err((AssertionStatus::Errored, reason)),
                    ExitObservation::Running => unreachable!("wait_for_exit waits for a terminal observation"),
                }
            }
            Ok(())
        }
        .await;
        let (status, message) = result.err().unwrap_or((AssertionStatus::Passed, "probe passed".into()));
        StepReport::new(started, status, message)
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
    #[serde(default = "step_timeout")]
    pub(crate) timeout: CaseDuration,
}

#[async_trait]
impl Step for Marker {
    fn name(&self) -> &'static str {
        "marker"
    }
    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Handle(self.on.clone())]
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let handle = cx.handle(&self.on).expect("marker needs its participant");
        let mut output = handle.output(None);
        loop {
            let batch = output.read();
            if batch.lines.iter().any(|line| line == "marker") {
                return StepReport::new(started, AssertionStatus::Passed, "marker observed");
            }
            if let OutputEnd::Failed(reason) = batch.end {
                return StepReport::new(started, AssertionStatus::Errored, reason);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
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

/// A passing step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Noop {
    #[serde(default = "step_timeout")]
    pub(crate) timeout: CaseDuration,
}

#[async_trait]
impl Step for Noop {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    fn needs(&self) -> Vec<Need> {
        Vec::new()
    }
    async fn run(&self, _cx: &StepContext<'_>) -> StepReport {
        StepReport::new(Instant::now(), AssertionStatus::Passed, "noop passed")
    }
}

struct NoopFactory;

impl StepFactory for NoopFactory {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Noop>(fields)?))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    on: String,
}

#[async_trait]
impl Step for Action {
    fn name(&self) -> &'static str {
        "action"
    }
    fn needs(&self) -> Vec<Need> {
        vec![Need::Handle(self.on.clone())]
    }
    fn budget(&self) -> Duration {
        step_timeout().into()
    }
    async fn run(&self, _: &StepContext<'_>) -> StepReport {
        StepReport::new(Instant::now(), AssertionStatus::Passed, "action completed")
    }
}

pub(crate) struct ActionFactory;

impl StepFactory for ActionFactory {
    fn name(&self) -> &'static str {
        "action"
    }
    fn kind(&self) -> StepKind {
        StepKind::Action
    }
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Action>(fields)?))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reach {
    endpoint: ConnectionTarget,
    operation: Option<EndpointOperation>,
    #[serde(default = "step_timeout")]
    pub(crate) timeout: CaseDuration,
}

#[async_trait]
impl Step for Reach {
    fn name(&self) -> &'static str {
        "reach"
    }
    fn budget(&self) -> Duration {
        self.timeout.into()
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.endpoint.participant.clone(),
            self.endpoint.endpoint.clone(),
            self.operation.unwrap_or(EndpointOperation::Probe),
        )]
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let server = &self.endpoint.participant;
        let result = cx
            .handle(server)
            .expect("reach needs its server")
            .probe(&self.endpoint.endpoint, &AttemptLimits::new(self.budget()))
            .await;
        let (status, message) = match result {
            Ok(ProbeAnswer::Listening) => (AssertionStatus::Passed, "endpoint is listening".into()),
            Ok(ProbeAnswer::NotListening { reason }) => (AssertionStatus::Failed, reason),
            Ok(ProbeAnswer::Addressable) => (AssertionStatus::Failed, "listener availability is unknown".into()),
            Err(error) => handle_failure(error),
        };
        StepReport::new(started, status, message)
    }
}

struct ReachFactory;

impl StepFactory for ReachFactory {
    fn name(&self) -> &'static str {
        "reach"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Reach>(fields)?))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    on: String,
    operation: String,
    #[serde(default)]
    args: Value,
    #[serde(default = "step_timeout")]
    timeout: CaseDuration,
}

#[async_trait]
impl Step for Call {
    fn name(&self) -> &'static str {
        "call"
    }
    fn needs(&self) -> Vec<Need> {
        vec![Need::Operation(self.on.clone(), self.operation.clone())]
    }
    fn budget(&self) -> Duration {
        self.timeout.into()
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let handle = cx.handle(&self.on).expect("call needs its participant");
        let (status, message) = match handle.op(&self.operation, self.args.clone(), self.budget()).await {
            Ok(value) => (AssertionStatus::Passed, value.to_string()),
            Err(error) => handle_failure(error),
        };
        StepReport::new(started, status, message)
    }
}

pub(crate) struct CallFactory;

impl StepFactory for CallFactory {
    fn name(&self) -> &'static str {
        "call"
    }
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Call>(fields)?))
    }
}

pub(crate) struct OperationServer;

impl ParticipantType for OperationServer {
    fn name(&self) -> &'static str {
        "server"
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        Server.supports()
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[]
    }
    fn operations(&self, realization: Realization) -> &'static [&'static dyn Operation] {
        match realization.kind {
            RealizationKind::LinuxContainer => &[&crate::step::test_support::ConfigDump],
            _ => &[],
        }
    }
    fn processes(&self) -> &'static [&'static str] {
        Server.processes()
    }
}

pub(crate) struct HttpOperationServer(pub &'static [&'static str]);

impl ParticipantType for HttpOperationServer {
    fn name(&self) -> &'static str {
        "server"
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        Server.supports()
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[]
    }
    fn endpoints(&self, _: Realization, _: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        Ok(self
            .0
            .iter()
            .enumerate()
            .map(|(index, &name)| (name.into(), Endpoint::http(Port::Fixed(1003 + index as u16))))
            .collect())
    }
    fn operations(&self, _: Realization) -> &'static [&'static dyn Operation] {
        &[&HttpDump]
    }
    fn processes(&self) -> &'static [&'static str] {
        Server.processes()
    }
}

struct HttpDump;

#[async_trait]
impl Operation for HttpDump {
    fn name(&self) -> &'static str {
        "http_dump"
    }
    fn http_endpoints(&self) -> &'static [&'static str] {
        &["api"]
    }
    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let endpoint = args.get("endpoint").and_then(Value::as_str).unwrap_or("api");
        let response = cx.http_get(endpoint, "/dump").await?;
        Ok(serde_json::json!({"status": response.status, "body": response.body}))
    }
}

pub(crate) const STEPS: &[&dyn StepFactory] = &[&MarkerFactory, &ProbeFactory, &NoopFactory, &ReachFactory];
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

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
    }

    fn params(&self) -> &'static [&'static str] {
        &["mode"]
    }

    fn default_env(&self, realization: Realization, _config: &CaseValues) -> BTreeMap<String, String> {
        match (realization.os, realization.kind) {
            (Os::Windows, RealizationKind::WindowsContainer) => BTreeMap::from([("METRICS".into(), "off".into())]),
            _ => BTreeMap::new(),
        }
    }

    fn endpoints(
        &self, realization: Realization, config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        assert!(self.supports().contains(&(realization.os, realization.kind)));
        if let Some(port) = config.env.get("PORT") {
            if port.parse::<u16>().is_err() {
                return Err(format!("PORT '{port}' is not a number"));
            }
        }
        let port = config
            .env
            .get("PORT")
            .and_then(|port| port.parse().ok())
            .unwrap_or(8080);
        let mut endpoints = BTreeMap::from([("api".into(), Endpoint::http(Port::Fixed(port)))]);
        if config.env.get("METRICS").map(String::as_str) != Some("off") {
            endpoints.insert("metrics".into(), Endpoint::udp(Port::Fixed(9000)));
        }
        if config.endpoints.contains_key("extra") {
            endpoints.insert("extra".into(), Endpoint::tcp(Port::FromCase));
        }
        Ok(endpoints)
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

    fn substituted_late_facts(&self, realization: Realization) -> &'static [LateFact] {
        assert!(self.supports().contains(&(realization.os, realization.kind)));
        if realization.kind == RealizationKind::LinuxContainer {
            &[LateFact::Ip]
        } else {
            &[]
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["server", "worker"]
    }

    fn process_specs(&self, realization: Realization) -> &'static [ProcessSpec] {
        if realization.kind != RealizationKind::LinuxContainer {
            return &[];
        }
        &[
            ProcessSpec {
                name: "server",
                exit: ExitChannel::Container,
                output: OutputChannel::Container,
            },
            ProcessSpec {
                name: "worker",
                exit: ExitChannel::Record { file: "worker" },
                output: OutputChannel::File {
                    file: "output",
                    stream: OutputStream::Merged,
                },
            },
        ]
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

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
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

pub(crate) struct Configured;

impl ParticipantType for Configured {
    fn name(&self) -> &'static str {
        "configured"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Windows, RealizationKind::WindowsContainer),
        ]
    }

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
    }

    fn config_files(&self) -> &'static [&'static str] {
        &["settings.yaml"]
    }

    fn params(&self) -> &'static [&'static str] {
        &["mode"]
    }

    fn validate_config(&self, realization: Realization, config: &CaseValues) -> Result<(), String> {
        if realization.os == Os::Windows && config.params.get("mode") == Some(&Value::from("broken")) {
            return Err("mode 'broken' is unavailable on windows".into());
        }
        Ok(())
    }

    fn default_env(&self, realization: Realization, config: &CaseValues) -> BTreeMap<String, String> {
        assert!(self.supports().contains(&(realization.os, realization.kind)));
        assert!(self.validate_config(realization, config).is_ok());
        let mut env = BTreeMap::from([("LEVEL".into(), "info".into())]);
        if !config.files.contains_key("settings.yaml") {
            env.insert("PORT".into(), "1".into());
        }
        env
    }

    fn processes(&self) -> &'static [&'static str] {
        &[]
    }
}

pub(crate) struct Client;

impl ParticipantType for Client {
    fn name(&self) -> &'static str {
        "client"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ]
    }

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
    }

    fn consumes(&self, realization: Realization, _config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        assert!(self.supports().contains(&(realization.os, realization.kind)));
        BTreeMap::from([
            (
                "upstream".into(),
                ConsumedEndpoint {
                    accepts: vec![EndpointKind::Http],
                    required: true,
                    delivery: Delivery::Env {
                        keys: BTreeSet::from(["UPSTREAM_URL".into()]),
                        value: |address| BTreeMap::from([("UPSTREAM_URL".into(), address.url.clone())]),
                    },
                },
            ),
            (
                "side".into(),
                ConsumedEndpoint {
                    accepts: vec![EndpointKind::Udp, EndpointKind::Tcp],
                    required: false,
                    delivery: Delivery::Env {
                        keys: BTreeSet::from(["SIDE_HOST".into(), "SIDE_PORT".into()]),
                        value: |address| {
                            BTreeMap::from([
                                ("SIDE_HOST".into(), address.host.clone().unwrap()),
                                ("SIDE_PORT".into(), address.port.unwrap().to_string()),
                            ])
                        },
                    },
                },
            ),
        ])
    }

    fn processes(&self) -> &'static [&'static str] {
        &[]
    }
}

pub(crate) const TYPES: &[&dyn ParticipantType] = &[&Client, &Server];

pub(crate) const REGISTRIES: Registries<'static> = Registries {
    participant_types: TYPES,
    steps: STEPS,
    typed_case_roles: &[],
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
            artifact_roles: BTreeMap::from([(Os::MacOs, vec![]), (Os::Windows, vec![])]),
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

pub(crate) struct FixtureTypesWithRoles(pub(crate) BTreeMap<Os, Vec<ArtifactRole>>);

impl TypedCaseLoader for FixtureTypesWithRoles {
    type Config = FixtureConfig;

    fn load(
        &self, case_type: &Value, content: &str, directory: &str, source: &Path,
    ) -> Result<Vec<TypedCase<Self::Config>>, GenericError> {
        let mut cases = FixtureTypes.load(case_type, content, directory, source)?;
        for case in &mut cases {
            case.artifact_roles = self.0.clone();
        }
        Ok(cases)
    }
}

pub(crate) const LOADER: CaseLoader<'static, FixtureTypes> = CaseLoader::new(REGISTRIES, FixtureTypes);

/// Expands to a one-step procedure line for participant cases whose procedure isn't under test.
///
/// A literal-producing macro lets `const` fixtures embed it with `concat!`. Tests replace this exact text
/// to add participants or steps, so every fixture builds its procedure from it.
macro_rules! minimal_procedure {
    () => {
        "procedure: [{step: noop}]"
    };
}
pub(crate) use minimal_procedure;

pub(crate) const PARTICIPANT_FIELDS: &str = concat!(
    "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\n",
    minimal_procedure!(),
    "\n"
);
pub(crate) const NO_PARTICIPANT_FIELDS: &str = concat!(
    "timeout: 1s\nsubject_os: [linux]\nparticipants: {}\n",
    minimal_procedure!(),
    "\n"
);

pub(crate) fn write_case(base: &Path, name: &str, content: &str) -> PathBuf {
    let directory = base.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("config.yaml");
    std::fs::write(&path, content).unwrap();
    path
}
