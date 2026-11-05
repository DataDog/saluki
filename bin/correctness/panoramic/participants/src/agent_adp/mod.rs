//! The `agent-adp` participant type: the Core Agent and ADP running together.
//!
//! Runs under s6 in Linux containers, under PowerShell in Windows containers, or as a native macOS host process group.
//! Uses the default ten-second stop grace.

use std::collections::BTreeMap;

use panoramic_core::artifact::{ArtifactKind, ArtifactRole};
use panoramic_core::backend::LateFact;
use panoramic_core::endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind};
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::participant::{
    CaseValues, EffectiveConfig, FactDeclaration, Operation, ParticipantType, ProcessSpec, RenderedConfig,
};
use panoramic_core::recipe::Recipe;

pub(crate) mod config_file;
mod late;
pub(crate) mod linux;
pub(crate) mod listeners;
mod macos;
pub(crate) mod operations;
pub(crate) mod pod;
mod windows;

/// The Core Agent and ADP running together.
pub struct AgentAdp;

impl AgentAdp {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "agent-adp";

    /// Image providing the Core Agent and ADP in Linux containers.
    pub const LINUX_IMAGE: ArtifactRole = ArtifactRole {
        name: "agent-adp-linux-image",
        kind: ArtifactKind::Image { os: Os::Linux },
        local_default: "saluki-images/datadog-agent:testing-devel",
    };

    /// Image providing the Core Agent and ADP in Windows containers.
    pub const WINDOWS_IMAGE: ArtifactRole = ArtifactRole {
        name: "agent-adp-windows-image",
        kind: ArtifactKind::Image { os: Os::Windows },
        local_default: "saluki-images/agent-data-plane:testing-windows",
    };

    /// ADP executable for macOS host processes.
    pub const ADP_HOST_BINARY: ArtifactRole = ArtifactRole {
        name: "adp-host-binary",
        kind: ArtifactKind::Binary,
        local_default: "target/release/agent-data-plane",
    };

    /// Core Agent install and main binary for macOS host processes.
    /// Defaults to the sandbox install `make provision-macos-test-env` writes.
    pub const AGENT_HOST_INSTALL: ArtifactRole = ArtifactRole {
        name: "agent-host-install",
        kind: ArtifactKind::Install {
            main_binary: "bin/agent/agent",
        },
        local_default: "/tmp/saluki-dda/datadog-agent",
    };
}

impl ParticipantType for AgentAdp {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Linux, RealizationKind::KubernetesPod),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ]
    }

    fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => &[Self::LINUX_IMAGE],
            (Os::MacOs, RealizationKind::HostProcessGroup) => &[Self::ADP_HOST_BINARY, Self::AGENT_HOST_INSTALL],
            (Os::Windows, RealizationKind::WindowsContainer) => &[Self::WINDOWS_IMAGE],
            _ => &[],
        }
    }

    fn recipe(&self, realization: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::recipe(config, true),
            (Os::Linux, RealizationKind::KubernetesPod) => pod::recipe(config, true),
            (Os::MacOs, RealizationKind::HostProcessGroup) => macos::recipe(config),
            (Os::Windows, RealizationKind::WindowsContainer) => windows::recipe(config),
            _ => Err(format!(
                "participant type '{}' has no recipe for {} on {}",
                Self::NAME,
                realization.kind.name(),
                realization.os.name()
            )),
        }
    }

    fn config_files(&self) -> &'static [&'static str] {
        &["datadog.yaml", "cont-init.d/", "files/"]
    }

    fn params(&self) -> &'static [&'static str] {
        &["extra_hosts", "host_view"]
    }

    fn validate_config(&self, realization: Realization, config: &CaseValues) -> Result<(), String> {
        if !matches!(
            (realization.os, realization.kind),
            (Os::Linux, RealizationKind::LinuxContainer) | (Os::Linux, RealizationKind::KubernetesPod),
        ) && linux::host_view(&config.params)?
        {
            return Err("host_view is supported in Linux containers and pods only".into());
        }
        if !matches!(
            (realization.os, realization.kind),
            (
                Os::Linux,
                RealizationKind::LinuxContainer | RealizationKind::KubernetesPod
            )
        ) && (config.files.contains_key("datadog.yaml") || config.file_contents.contains_key("datadog.yaml"))
        {
            return Err("agent-adp datadog.yaml is supported in Linux containers and pods only".into());
        }
        if realization.kind == RealizationKind::KubernetesPod {
            pod::validate_config(config)?;
        } else if !matches!(
            (realization.os, realization.kind),
            (Os::Linux, RealizationKind::LinuxContainer) | (Os::Windows, RealizationKind::WindowsContainer)
        ) {
            if config
                .params
                .get("extra_hosts")
                .is_some_and(|hosts| hosts.as_object().is_none_or(|hosts| !hosts.is_empty()))
            {
                return Err("agent-adp extra_hosts is supported in containers only".into());
            }
            if !config.files.is_empty() {
                return Err("agent-adp case files are supported in containers only".into());
            }
        }
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => {
                linux::validate_config(config)?;
                let file = config_file::ConfigFile::parse(&config.file_contents)?;
                let mut env = self.default_env(realization, config);
                env.extend(config.env.clone());
                listeners::endpoints(realization, &env, Some(&file), listeners::Product::AgentAdp)?;
            }
            (Os::MacOs, RealizationKind::HostProcessGroup) => macos::validate_config(realization, config)?,
            (Os::Windows, RealizationKind::WindowsContainer) => {
                windows::validate_config(config)?;
                late::validate(&config.env, &config.params)?;
                let mut env = container_default_env();
                env.extend(config.env.clone());
                listeners::endpoints(realization, &env, None, listeners::Product::AgentAdp)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_realization_options(
        &self, realization: Realization, options: &BTreeMap<String, serde_json::Value>, config: &CaseValues,
    ) -> Result<(), String> {
        if realization.kind == RealizationKind::KubernetesPod {
            pod::validate_realization_options(options, config)?;
        }
        Ok(())
    }

    fn default_env(&self, realization: Realization, config: &CaseValues) -> BTreeMap<String, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::default_env(config, container_default_env()),
            (Os::Linux, RealizationKind::KubernetesPod) => pod::default_env(config, container_default_env()),
            (Os::Windows, RealizationKind::WindowsContainer) => container_default_env(),
            (Os::MacOs, RealizationKind::HostProcessGroup) => macos::default_env(),
            _ => BTreeMap::new(),
        }
    }

    fn endpoints(
        &self, realization: Realization, config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => {
                let file = config_file::ConfigFile::parse(&config.files)?;
                listeners::endpoints(realization, &config.env, Some(&file), listeners::Product::AgentAdp)
            }
            (Os::MacOs, RealizationKind::HostProcessGroup) | (Os::Windows, RealizationKind::WindowsContainer) => {
                listeners::endpoints(realization, &config.env, None, listeners::Product::AgentAdp)
            }
            _ => Ok(BTreeMap::new()),
        }
    }

    fn consumes(&self, realization: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::consumes(),
            (Os::Linux, RealizationKind::KubernetesPod)
            | (Os::MacOs, RealizationKind::HostProcessGroup)
            | (Os::Windows, RealizationKind::WindowsContainer) => BTreeMap::from([("intake".into(), intake())]),
            _ => BTreeMap::new(),
        }
    }

    /// `default_log_file` follows ADP's `PlatformSettings::get_default_log_file_path()` in
    /// Linux and Windows containers.
    fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[
                FactDeclaration {
                    name: "default_log_file",
                    value: "/var/log/datadog/agent-data-plane.log",
                },
                FactDeclaration {
                    name: "files_dir",
                    value: crate::CASE_FILES_DIR,
                },
            ],
            (Os::Windows, RealizationKind::WindowsContainer) => &[
                FactDeclaration {
                    name: "default_log_file",
                    value: r"C:\ProgramData\Datadog\logs\agent-data-plane.log",
                },
                FactDeclaration {
                    name: "files_dir",
                    value: crate::WINDOWS_CASE_FILES_DIR,
                },
            ],
            _ => &[],
        }
    }

    fn substituted_late_facts(&self, realization: Realization) -> &'static [LateFact] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) | (Os::Windows, RealizationKind::WindowsContainer) => {
                &[LateFact::Ip]
            }
            _ => &[],
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["adp", "agent"]
    }

    fn operations(&self, realization: Realization) -> &'static [&'static dyn Operation] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => operations::LINUX_OPERATIONS,
            (Os::MacOs, RealizationKind::HostProcessGroup) => operations::MACOS_OPERATIONS,
            (Os::Windows, RealizationKind::WindowsContainer) => operations::WINDOWS_OPERATIONS,
            _ => &[],
        }
    }

    fn process_specs(&self, realization: Realization) -> &'static [ProcessSpec] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod) => &linux::PROCESS_SPECS,
            (Os::MacOs, RealizationKind::HostProcessGroup) => &macos::PROCESS_SPECS,
            (Os::Windows, RealizationKind::WindowsContainer) => &windows::PROCESS_SPECS,
            _ => &[],
        }
    }
}

pub(crate) fn intake() -> ConsumedEndpoint {
    ConsumedEndpoint {
        accepts: vec![EndpointKind::Http],
        required: false,
        delivery: Delivery::Env {
            keys: ["DD_DD_URL".into(), "DD_APM_DD_URL".into()].into(),
            value: |address| {
                BTreeMap::from([
                    ("DD_DD_URL".into(), address.url().into()),
                    ("DD_APM_DD_URL".into(), address.url().into()),
                ])
            },
        },
    }
}

fn container_default_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        // The Core Agent listens on this port; ADP uses it for IPC.
        ("DD_CMD_PORT".to_string(), "55001".to_string()),
        ("DD_GUI_PORT".to_string(), "-1".to_string()),
        // Isolate the other Agent listeners as well as those the procedure exercises.
        ("DD_EXPVAR_PORT".to_string(), "55000".to_string()),
        ("DD_APM_RECEIVER_PORT".to_string(), "58126".to_string()),
        ("DD_PROCESS_CONFIG_CMD_PORT".to_string(), "56062".to_string()),
        ("DD_AGENT_IPC_PORT".to_string(), "55004".to_string()),
        // DogStatsD can run in either process, depending on the pipeline switches.
        ("DD_DOGSTATSD_PORT".to_string(), "58125".to_string()),
        // The Core Agent streams these listen addresses to ADP.
        (
            "DD_DATA_PLANE_API_LISTEN_ADDRESS".to_string(),
            "tcp://0.0.0.0:55100".to_string(),
        ),
        (
            "DD_DATA_PLANE_SECURE_API_LISTEN_ADDRESS".to_string(),
            "tcp://0.0.0.0:55101".to_string(),
        ),
        (
            "DD_DATA_PLANE_TELEMETRY_LISTEN_ADDR".to_string(),
            "tcp://0.0.0.0:55102".to_string(),
        ),
        // Core Agent OTLP listeners; ADP uses its own endpoint settings.
        (
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT".to_string(),
            "0.0.0.0:54317".to_string(),
        ),
        (
            "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT".to_string(),
            "0.0.0.0:54318".to_string(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_adp_substitutes_its_own_ip_in_linux_and_windows_containers() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::WindowsContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::KubernetesPod,
            ] {
                let expected: &[LateFact] = if matches!(
                    (os, kind),
                    (Os::Linux, RealizationKind::LinuxContainer) | (Os::Windows, RealizationKind::WindowsContainer)
                ) {
                    &[LateFact::Ip]
                } else {
                    &[]
                };
                assert_eq!(AgentAdp.substituted_late_facts(Realization { os, kind }), expected);
            }
        }
    }

    #[test]
    fn extra_hosts_are_rejected_on_macos_host_process_groups_and_pods() {
        assert_eq!(AgentAdp.params(), &["extra_hosts", "host_view"]);
        for &(os, kind) in AgentAdp
            .supports()
            .iter()
            .filter(|&&(_, kind)| matches!(kind, RealizationKind::HostProcessGroup | RealizationKind::KubernetesPod))
        {
            for (hosts, accepted) in [
                (serde_json::json!({}), true),
                (serde_json::json!({"foo.local": "self"}), false),
                (serde_json::json!("self"), false),
            ] {
                let config = CaseValues {
                    params: BTreeMap::from([("extra_hosts".into(), hosts)]),
                    ..Default::default()
                };
                let result = AgentAdp.validate_config(Realization { os, kind }, &config);
                if accepted {
                    result.unwrap();
                } else {
                    assert_eq!(
                        result.unwrap_err(),
                        if kind == RealizationKind::KubernetesPod {
                            "extra_hosts is not supported in pods"
                        } else {
                            "agent-adp extra_hosts is supported in containers only"
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn agent_adp_supports_linux_containers_and_pods_macos_hosts_and_windows_containers() {
        assert_eq!(
            AgentAdp.supports(),
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::Linux, RealizationKind::KubernetesPod),
                (Os::MacOs, RealizationKind::HostProcessGroup),
                (Os::Windows, RealizationKind::WindowsContainer)
            ]
        );
    }

    #[test]
    fn agent_adp_declares_an_image_per_container_and_a_host_binary_and_install_on_macos() {
        let table = [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                vec![ArtifactRole {
                    name: "agent-adp-linux-image",
                    kind: ArtifactKind::Image { os: Os::Linux },
                    local_default: "saluki-images/datadog-agent:testing-devel",
                }],
            ),
            (Os::Linux, RealizationKind::KubernetesPod, vec![AgentAdp::LINUX_IMAGE]),
            (
                Os::MacOs,
                RealizationKind::HostProcessGroup,
                vec![
                    ArtifactRole {
                        name: "adp-host-binary",
                        kind: ArtifactKind::Binary,
                        local_default: "target/release/agent-data-plane",
                    },
                    ArtifactRole {
                        name: "agent-host-install",
                        kind: ArtifactKind::Install {
                            main_binary: "bin/agent/agent",
                        },
                        local_default: "/tmp/saluki-dda/datadog-agent",
                    },
                ],
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                vec![ArtifactRole {
                    name: "agent-adp-windows-image",
                    kind: ArtifactKind::Image { os: Os::Windows },
                    local_default: "saluki-images/agent-data-plane:testing-windows",
                }],
            ),
        ];
        for (os, kind, expected) in &table {
            assert_eq!(
                AgentAdp.artifacts(Realization { os: *os, kind: *kind }),
                expected.as_slice(),
                "{os:?} {kind:?}"
            );
        }
        assert_eq!(
            table.iter().map(|(os, kind, _)| (*os, *kind)).collect::<Vec<_>>(),
            AgentAdp.supports()
        );
    }

    #[test]
    fn agent_adp_offers_the_same_seven_operations_on_supported_realizations_except_pods() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::WindowsContainer,
                RealizationKind::KubernetesPod,
            ] {
                let names = AgentAdp
                    .operations(Realization { os, kind })
                    .iter()
                    .map(|operation| operation.name())
                    .collect::<Vec<_>>();
                let expected = if AgentAdp.supports().contains(&(os, kind)) && kind != RealizationKind::KubernetesPod {
                    vec![
                        "cli_adp",
                        "cli_agent",
                        "config_dump",
                        "config_set",
                        "dogstatsd_capture",
                        "dogstatsd_replay",
                        "dogstatsd_stats",
                    ]
                } else {
                    vec![]
                };
                assert_eq!(names, expected, "{os:?} {kind:?}");
            }
        }
    }

    #[test]
    fn every_realization_consumes_intake_and_only_linux_containers_consume_forwarding_and_docker() {
        use panoramic_core::{
            endpoint::test_support::http_address,
            recipe::test_support::{host_processes, rendered},
        };

        for &(os, kind) in AgentAdp.supports() {
            let realization = Realization { os, kind };
            let consumes = AgentAdp.consumes(realization, &EffectiveConfig::default());
            let expected = if (os, kind) == (Os::Linux, RealizationKind::LinuxContainer) {
                vec!["docker_api", "dogstatsd_forward", "intake"]
            } else {
                vec!["intake"]
            };
            assert_eq!(consumes.keys().map(String::as_str).collect::<Vec<_>>(), expected);
            let intake = &consumes["intake"];
            assert_eq!(intake.accepts, [EndpointKind::Http]);
            assert!(!intake.required);
            let Delivery::Env { keys, value } = &intake.delivery;
            assert_eq!(keys, &["DD_DD_URL".into(), "DD_APM_DD_URL".into()].into());
            let address = http_address("intake", 2049);
            let env = value(&address);
            assert_eq!(
                env,
                BTreeMap::from([
                    ("DD_DD_URL".into(), "http://intake:2049".into()),
                    ("DD_APM_DD_URL".into(), "http://intake:2049".into()),
                ])
            );
            if os == Os::MacOs {
                let recipe = AgentAdp.recipe(realization, &rendered(env, BTreeMap::new())).unwrap();
                let processes = host_processes(&recipe);
                assert_eq!(processes.len(), 2);
                for (_, _, _, process_env) in processes {
                    assert_eq!(process_env["DD_DD_URL"], "http://intake:2049");
                    assert_eq!(process_env["DD_APM_DD_URL"], "http://intake:2049");
                }
            }
        }
    }

    #[test]
    fn case_files_are_supported_in_containers_and_init_hooks_only_on_linux() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::WindowsContainer,
                RealizationKind::KubernetesPod,
            ] {
                let realization = Realization { os, kind };
                AgentAdp.validate_config(realization, &CaseValues::default()).unwrap();
                for name in ["files/", "cont-init.d/"] {
                    let config = CaseValues {
                        files: BTreeMap::from([(
                            name.into(),
                            panoramic_core::case::FileSource {
                                from: "case-files".into(),
                                template: false,
                            },
                        )]),
                        ..Default::default()
                    };
                    let result = AgentAdp.validate_config(realization, &config);
                    if (os, kind) == (Os::Linux, RealizationKind::LinuxContainer)
                        || ((os, kind) == (Os::Windows, RealizationKind::WindowsContainer) && name == "files/")
                    {
                        result.unwrap();
                    } else if (os, kind) == (Os::Windows, RealizationKind::WindowsContainer) {
                        assert_eq!(
                            result.unwrap_err(),
                            "agent-adp cont-init.d is supported in Linux containers only"
                        );
                    } else {
                        assert_eq!(
                            result.unwrap_err(),
                            if kind == RealizationKind::KubernetesPod {
                                "pods accept only the datadog.yaml case file"
                            } else {
                                "agent-adp case files are supported in containers only"
                            },
                            "{os:?} {kind:?}: {name}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn agent_adp_provides_realization_specific_log_and_case_file_facts() {
        for (os, kind, expected) in [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                vec![
                    ("default_log_file", "/var/log/datadog/agent-data-plane.log"),
                    ("files_dir", "/var/lib/panoramic/files"),
                ],
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                vec![
                    ("default_log_file", r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
                    ("files_dir", r"C:\panoramic\files"),
                ],
            ),
            (Os::MacOs, RealizationKind::HostProcessGroup, vec![]),
            (Os::Linux, RealizationKind::KubernetesPod, vec![]),
        ] {
            assert_eq!(
                AgentAdp
                    .facts(Realization { os, kind })
                    .iter()
                    .map(|fact| (fact.name, fact.value))
                    .collect::<Vec<_>>(),
                expected,
                "{os:?} {kind:?}"
            );
        }
    }
}
