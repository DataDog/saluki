use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use saluki_error::GenericError;

use super::{CasePlan, Planner};
use crate::{
    artifact::ArtifactRole,
    backend,
    case::{
        fixtures::{minimal_procedure, write_case, Client, Configured, FixtureConfig, FixtureTypes, Server, STEPS},
        CaseLoader, Registries,
    },
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Evidence, Port},
    layout::{Os, Realization, RealizationKind},
    participant::{CaseValues, EffectiveConfig, FactDeclaration, ParticipantType},
};

pub(super) struct ReadyServer;

impl ParticipantType for ReadyServer {
    fn name(&self) -> &'static str {
        "ready-server"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        EveryKindServer.supports()
    }

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
    }

    fn params(&self) -> &'static [&'static str] {
        &["mode"]
    }

    fn endpoints(
        &self, _realization: Realization, config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        let port = Port::Fixed(
            config
                .env
                .get("PORT")
                .map(|value| value.parse().unwrap())
                .unwrap_or(8080),
        );
        let api = match config.env.get("READY_KIND").map(String::as_str).unwrap_or("http") {
            "tcp" => Endpoint::tcp(port),
            "udp" => Endpoint::udp(port),
            "http" => Endpoint::http(port),
            "grpc" => Endpoint::grpc(port),
            "unix-datagram" => Endpoint::unix_datagram("/run/api.sock"),
            "unix-stream" => Endpoint::unix_stream("/run/api.sock"),
            "named-pipe" => Endpoint::named_pipe("api"),
            kind => panic!("unknown fixture endpoint kind: {kind}"),
        };
        let mut endpoints = BTreeMap::from([
            ("api".into(), api),
            ("metrics".into(), Endpoint::udp(Port::Fixed(9000))),
        ]);
        let owner = config.env.get("READY_ON").map(String::as_str).unwrap_or("api");
        if let Some(endpoint) = config.env.get("READY_TCP") {
            endpoints.get_mut(owner).unwrap().ready = Some(Evidence::tcp_accepts(endpoint));
        }
        if let Some(path) = config.env.get("READY_HTTP") {
            endpoints.get_mut(owner).unwrap().ready = Some(Evidence::http_ok("api", path));
        }
        if let Some(path) = config.env.get("READY_SOCKET") {
            let endpoint = config
                .env
                .get("READY_SOCKET_TARGET")
                .map(String::as_str)
                .unwrap_or("log");
            endpoints.insert(
                "log".into(),
                Endpoint::unix_datagram(path).with_ready(Evidence::socket_exists(endpoint)),
            );
        }
        if let Some(within) = config.env.get("READY_WITHIN") {
            for endpoint in endpoints.values_mut() {
                endpoint.ready = endpoint
                    .ready
                    .take()
                    .map(|evidence| evidence.within(Duration::from_secs(within.parse().unwrap())));
            }
        }
        Ok(endpoints)
    }

    fn readiness(&self, _realization: Realization, config: &EffectiveConfig) -> Vec<Evidence> {
        ["READY_FILE", "READY_FILE_2"]
            .into_iter()
            .filter_map(|key| {
                config.env.get(key).map(|path| {
                    let path = match config.params.get("mode").and_then(serde_json::Value::as_str) {
                        Some("late-path") => "{{self.late.ip}}/ready",
                        Some("malformed-path") => "{{unfinished",
                        _ => path,
                    };
                    let evidence = Evidence::file_exists(path);
                    match config.env.get("READY_WITHIN") {
                        Some(within) => evidence.within(Duration::from_secs(within.parse().unwrap())),
                        None => evidence,
                    }
                })
            })
            .collect()
    }

    fn consumes(&self, realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        let mut endpoint = EveryKindClient.consumes(realization, config).remove("target").unwrap();
        endpoint.delivery = Delivery::Env {
            keys: BTreeSet::new(),
            value: |_| BTreeMap::new(),
        };
        ["a", "b", "c", "z"]
            .into_iter()
            .map(|name| (name.into(), endpoint.clone()))
            .collect()
    }

    fn processes(&self) -> &'static [&'static str] {
        &[]
    }
}

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

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
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

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
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

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
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
    steps: STEPS,
    ..crate::case::fixtures::REGISTRIES
};
pub(super) const FILE_CONFIG: &str = "{files: {settings.yaml: {from: source.yaml, template: true}}}";

pub(super) fn compile_files(yaml: &str, sources: &[(&str, &str)], os: Os) -> Result<CasePlan<FixtureConfig>, String> {
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
    Planner::with_run(FILE_REGISTRIES, "testrun0")
        .compile(case, os)
        .map_err(normalize)
        .map(|compiled| compiled.into_plan().unwrap())
}

pub(super) struct AssignedServer;

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

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
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

    fn readiness(&self, _realization: Realization, config: &EffectiveConfig) -> Vec<Evidence> {
        if config.params.get("mode").and_then(serde_json::Value::as_str) == Some("readiness-after-assignment") {
            vec![Evidence::file_exists(format!("/ports/{}", config.env["API_PORT"]))]
        } else {
            Vec::new()
        }
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

pub(super) struct SharedDeliveryClient;

impl ParticipantType for SharedDeliveryClient {
    fn name(&self) -> &'static str {
        "shared-delivery-client"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        Client.supports()
    }

    fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
        &[]
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

pub(super) const FACT_CASE: &str = "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: [{step: probe, on: subject, path: '{{ subject.facts.file }}'}]\n";

pub(super) const CONFIG_REGISTRIES: Registries<'static> = Registries {
    participant_types: &[
        &Client,
        &Configured,
        &Server,
        &EveryKindClient,
        &EveryKindServer,
        &FileServer,
        &SharedDeliveryClient,
        &AssignedServer,
        &ReadyServer,
    ],
    steps: STEPS,
    ..crate::case::fixtures::REGISTRIES
};
pub(super) const CONFIG_LOADER: CaseLoader<'static, FixtureTypes> =
    CaseLoader::new(CONFIG_REGISTRIES, FixtureTypes, "fixture-participants");

pub(super) fn planner() -> Planner<'static> {
    Planner::with_run(CONFIG_REGISTRIES, "testrun0")
}

pub(super) fn compile(yaml: &str, os: Os) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
    compile_with(&mut planner(), yaml, os)
}

pub(super) fn compile_with(
    planner: &mut Planner<'static>, yaml: &str, os: Os,
) -> Result<Option<CasePlan<FixtureConfig>>, GenericError> {
    let case = CONFIG_LOADER
        .load_content(yaml, "alpha", Path::new("cases/alpha/config.yaml"))
        .unwrap()
        .remove(0);
    planner.compile(case, os).map(super::Compiled::into_plan)
}

pub(super) const CONNECTION_CASE: &str = concat!(
    r#"timeout: 1s
subject_os: [linux]
participants:
  client: {type: client, subject: true, connect: {upstream: server.api}}
  server: {type: server}
"#,
    minimal_procedure!(),
    "\n"
);

pub(super) fn socket_case(config: &str) -> String {
    format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  client: {{type: every-kind-client, subject: true, connect: {{target: server.stream}}}}\n  server: {{type: every-kind-server, config: {config}}}\n{}", minimal_procedure!())
}

pub(super) fn assigned_server_case(config: &str) -> String {
    CONNECTION_CASE
        .replace("[linux]", "[macos]")
        .replace("type: server}", &format!("type: assigned-server, config: {config}}}"))
}

pub(super) fn host_to_container_case() -> String {
    CONNECTION_CASE
        .replace("[linux]", "[macos]")
        .replace("type: server}", "type: server, os: linux}")
}

pub(super) fn host_to_container_reference_case(config: &str) -> String {
    format!("timeout: 1s\nsubject_os: [macos]\nparticipants:\n  subject: {{type: file-server, subject: true, config: {config}}}\n  server: {{type: server, os: linux}}\n{}\n", minimal_procedure!())
}
