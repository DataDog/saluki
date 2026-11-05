//! The Agent and ADP in a single pod container with Linux service overlays.

use std::collections::BTreeMap;

use panoramic_core::{
    participant::{CaseValues, RenderedConfig},
    recipe::{PodRecipe, Recipe},
};

use super::linux;

pub(crate) fn validate_config(config: &CaseValues) -> Result<(), String> {
    if config
        .params
        .get("extra_hosts")
        .is_some_and(|hosts| hosts.as_object().is_none_or(|hosts| !hosts.is_empty()))
    {
        return Err("extra_hosts is not supported in pods".into());
    }
    if config
        .files
        .keys()
        .chain(config.file_contents.keys())
        .any(|name| name != "datadog.yaml")
    {
        return Err("pods accept only the datadog.yaml case file".into());
    }
    Ok(())
}

pub(crate) fn validate_realization_options(
    options: &BTreeMap<String, serde_json::Value>, config: &CaseValues,
) -> Result<(), String> {
    if linux::host_view(&config.params)? && options.get("host_pid").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("pod host_view: true requires realization.options.host_pid: true".into());
    }
    Ok(())
}

pub(crate) fn default_env(config: &CaseValues, defaults: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = linux::default_env(config, defaults);
    // Pods use the node's containerd socket rather than Docker's.
    env.remove("DOCKER_HOST");
    env
}

pub(crate) fn recipe(config: &RenderedConfig, with_adp: bool) -> Result<Recipe, String> {
    let mut container = linux::base_recipe(config, with_adp)?;
    if linux::host_view(config.params())? {
        container = container
            .with_host_mount("/proc", "/host/proc", true)
            .with_host_mount("/sys/fs/cgroup", "/host/sys/fs/cgroup", true)
            .with_host_mount(
                "/run/containerd/containerd.sock",
                "/var/run/containerd/containerd.sock",
                true,
            );
    }
    // Mirror the image's default command explicitly for the pod's file-ready wrapper.
    let mut pod = PodRecipe::new(
        "agent",
        container
            .with_entrypoint(["/bin/entrypoint.sh"])
            .with_command(Vec::<String>::new()),
    );
    if config.file_needs_late_write("datadog.yaml") {
        pod = pod.with_late_file("/etc/datadog-agent/datadog.yaml", "datadog.yaml");
    }
    Ok(Recipe::Pod(pod))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use panoramic_core::{
        endpoint::{
            test_support::{assert_network_endpoint, assert_socket_endpoint, http_address},
            Delivery, EndpointKind, Evidence,
        },
        layout::{Os, Realization, RealizationKind},
        participant::{
            CaseValues, EffectiveConfig, ExitChannel, OutputChannel, OutputStream, ParticipantType, ProcessSpec,
        },
        recipe::test_support::{assert_pod, files, host_mounts, rendered, rendered_with_params},
    };
    use serde_json::json;

    use crate::{agent_adp::linux, Agent, AgentAdp};

    const POD: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::KubernetesPod,
    };
    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    #[test]
    fn both_agent_pods_use_file_listener_values_and_preserve_only_templated_late_files() {
        use panoramic_core::{
            case::FileSource,
            recipe::test_support::{late_files, with_deferred_files},
        };
        let yaml = b"hostname: '{{ self.late.pod_uid }}'\ncmd_port: 56001\ndogstatsd_port: 0\ndogstatsd_socket: /airlock/example/shared/metrics.sock\n";
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            for deferred in [false, true] {
                let values = CaseValues {
                    files: BTreeMap::from([(
                        "datadog.yaml".into(),
                        FileSource {
                            from: "datadog.yaml".into(),
                            template: deferred,
                        },
                    )]),
                    file_contents: BTreeMap::from([("datadog.yaml".into(), yaml.to_vec())]),
                    ..Default::default()
                };
                participant.validate_config(POD, &values).unwrap();
                let mut env = participant.default_env(POD, &values);
                assert!(!env.contains_key("DD_CMD_PORT"));
                assert!(!env.contains_key("DOCKER_HOST"));
                let Delivery::Env { value, .. } =
                    &participant.consumes(POD, &EffectiveConfig::default())["intake"].delivery;
                env.extend(value(&http_address("intake", 2049)));
                let effective = EffectiveConfig {
                    env: env.clone(),
                    files: values.file_contents.clone(),
                    ..Default::default()
                };
                let endpoints = participant.endpoints(POD, &effective).unwrap();
                assert_network_endpoint(
                    &endpoints["agent_cmd"],
                    EndpointKind::Tcp,
                    56001,
                    Some(Evidence::tcp_accepts("agent_cmd")),
                );
                assert_socket_endpoint(
                    &endpoints["dogstatsd_uds"],
                    EndpointKind::UnixDatagram,
                    "/airlock/example/shared/metrics.sock",
                    Some(Evidence::socket_exists("dogstatsd_uds")),
                );
                let mut config = rendered(env.clone(), values.file_contents);
                if deferred {
                    config = with_deferred_files(config, &["datadog.yaml"]);
                }
                let recipe = participant.recipe(POD, &config).unwrap();
                assert_pod(
                    &recipe,
                    "agent",
                    AgentAdp::LINUX_IMAGE,
                    &["/bin/entrypoint.sh"],
                    &[],
                    &env,
                );
                assert_eq!(
                    late_files(&recipe),
                    if deferred {
                        vec!["/etc/datadog-agent/datadog.yaml"]
                    } else {
                        vec![]
                    }
                );
                assert!(files(&recipe).contains(&("/etc/datadog-agent/datadog.yaml", 0o644, yaml.as_slice())));
                let expected_files = if participant.name() == "agent" { 4 } else { 7 };
                assert_eq!(files(&recipe).len(), expected_files);
                if participant.name() == "agent" {
                    assert_eq!(env["DD_DATA_PLANE_ENABLED"], "false");
                    assert!(!files(&recipe).iter().any(|(path, _, _)| path.contains("data-plane")));
                    assert_eq!(participant.process_specs(POD), &linux::AGENT_PROCESS_SPECS);
                }
                assert!(participant.operations(POD).is_empty());
                assert!(participant.facts(POD).is_empty());
                assert!(participant.substituted_late_facts(POD).is_empty());
            }
            let values = CaseValues {
                file_contents: BTreeMap::from([(
                    "datadog.yaml".into(),
                    b"dogstatsd_socket: '{{ self.late.ip }}'".to_vec(),
                )]),
                ..Default::default()
            };
            let error = participant.validate_config(POD, &values).unwrap_err();
            assert_eq!(
                error,
                "configuration file 'datadog.yaml' key 'dogstatsd_socket' contains reference '{{ self.late.ip }}'; only early facts are substituted before planning"
            );
        }
    }

    #[test]
    fn both_agent_pods_restrict_case_files_and_host_aliases_and_keep_cri_host_defaults() {
        for participant in [&Agent as &dyn ParticipantType, &AgentAdp] {
            for name in ["files/script", "cont-init.d/hook"] {
                let config = CaseValues {
                    file_contents: BTreeMap::from([(name.into(), vec![])]),
                    ..Default::default()
                };
                assert_eq!(
                    participant.validate_config(POD, &config).unwrap_err(),
                    "pods accept only the datadog.yaml case file"
                );
            }
            let config = CaseValues {
                params: BTreeMap::from([("extra_hosts".into(), json!({"other": "self"}))]),
                ..Default::default()
            };
            assert_eq!(
                participant.validate_config(POD, &config).unwrap_err(),
                "extra_hosts is not supported in pods"
            );
            let config = CaseValues {
                params: BTreeMap::from([("host_view".into(), json!(true))]),
                ..Default::default()
            };
            let env = participant.default_env(POD, &config);
            assert_eq!(env["DD_PROC_ROOT"], "/host/proc");
            assert_eq!(env["DD_CGROUP_ROOT"], "/host/sys/fs/cgroup");
            assert!(!env.contains_key("DOCKER_HOST"));
            assert_eq!(
                host_mounts(
                    &participant
                        .recipe(POD, &rendered_with_params(env, config.params, BTreeMap::new()))
                        .unwrap()
                ),
                vec![
                    ("/proc", "/host/proc", true),
                    ("/sys/fs/cgroup", "/host/sys/fs/cgroup", true),
                    (
                        "/run/containerd/containerd.sock",
                        "/var/run/containerd/containerd.sock",
                        true
                    ),
                ]
            );
        }
    }

    #[test]
    fn pod_preserves_linux_defaults_case_environment_intake_delivery_and_service_overlays() {
        let mut env = AgentAdp.default_env(POD, &CaseValues::default());
        assert_eq!(env, AgentAdp.default_env(LINUX, &CaseValues::default()));
        assert_eq!(env["DD_CMD_PORT"], "55001");
        env.extend([
            ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
            ("DD_CMD_PORT".into(), "56001".into()),
        ]);
        let consumes = AgentAdp.consumes(POD, &EffectiveConfig::default());
        let Delivery::Env { value, .. } = &consumes["intake"].delivery;
        env.extend(value(&http_address("intake-adp", 2049)));
        assert_eq!(env["DD_DD_URL"], "http://intake-adp:2049");
        assert_eq!(env["DD_APM_DD_URL"], "http://intake-adp:2049");
        let config = rendered(env.clone(), BTreeMap::new());
        let recipe = AgentAdp.recipe(POD, &config).unwrap();
        assert_pod(
            &recipe,
            "agent",
            AgentAdp::LINUX_IMAGE,
            &["/bin/entrypoint.sh"],
            &[],
            &env,
        );
        let linux = AgentAdp.recipe(LINUX, &config).unwrap();
        assert_eq!(files(&recipe), files(&linux));
        let files = files(&recipe);
        assert_eq!(files.len(), 6);
        for service in ["data-plane", "agent"] {
            for suffix in ["run", "finish", "log/run"] {
                let path = format!("/etc/services.d/{service}/{suffix}");
                let (_, mode, contents) = files.iter().find(|(name, _, _)| *name == path).unwrap();
                assert_eq!(*mode, 0o755);
                assert!(!contents.is_empty());
            }
        }
    }

    #[test]
    fn pod_host_view_is_opt_in_and_mounts_the_node_read_only() {
        for (value, enabled) in [(None, false), (Some(json!(false)), false), (Some(json!(true)), true)] {
            let params = value
                .into_iter()
                .map(|value| ("host_view".into(), value))
                .collect::<BTreeMap<_, _>>();
            AgentAdp
                .validate_config(
                    POD,
                    &CaseValues {
                        params: params.clone(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let recipe = AgentAdp
                .recipe(POD, &rendered_with_params(BTreeMap::new(), params, BTreeMap::new()))
                .unwrap();
            let expected = if enabled {
                vec![
                    ("/proc", "/host/proc", true),
                    ("/sys/fs/cgroup", "/host/sys/fs/cgroup", true),
                    (
                        "/run/containerd/containerd.sock",
                        "/var/run/containerd/containerd.sock",
                        true,
                    ),
                ]
            } else {
                vec![]
            };
            assert_eq!(host_mounts(&recipe), expected);
        }
    }

    #[test]
    fn pod_host_view_requires_a_boolean() {
        for value in [
            json!(null),
            json!("true"),
            json!("false"),
            json!(0),
            json!(1),
            json!([]),
            json!({}),
        ] {
            let params = BTreeMap::from([("host_view".into(), value)]);
            let error = AgentAdp
                .validate_config(
                    POD,
                    &CaseValues {
                        params: params.clone(),
                        ..Default::default()
                    },
                )
                .unwrap_err();
            assert_eq!(error, "host_view must be a boolean");
            assert_eq!(
                AgentAdp
                    .recipe(POD, &rendered_with_params(BTreeMap::new(), params, BTreeMap::new()))
                    .err()
                    .unwrap(),
                error
            );
        }
    }

    #[test]
    fn host_view_is_rejected_outside_linux_realizations() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::WindowsContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::KubernetesPod,
            ] {
                let linux =
                    os == Os::Linux && matches!(kind, RealizationKind::LinuxContainer | RealizationKind::KubernetesPod);
                for value in [json!(false), json!(true), json!(null), json!("true")] {
                    // A false host view is a no-op everywhere; true needs a Linux container or pod.
                    let expected = if !value.is_boolean() {
                        Some("host_view must be a boolean")
                    } else if linux || value == json!(false) {
                        None
                    } else {
                        Some("host_view is supported in Linux containers and pods only")
                    };
                    let config = CaseValues {
                        params: BTreeMap::from([("host_view".into(), value.clone())]),
                        ..Default::default()
                    };
                    assert_eq!(
                        AgentAdp
                            .validate_config(Realization { os, kind }, &config)
                            .err()
                            .as_deref(),
                        expected,
                        "{os:?} {kind:?} {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn pod_processes_observe_linux_supervisor_exit_records_and_merged_logs() {
        assert_eq!(AgentAdp.process_specs(POD), AgentAdp.process_specs(LINUX));
        assert_eq!(
            AgentAdp.process_specs(POD),
            &[
                ProcessSpec {
                    name: "adp",
                    exit: ExitChannel::Record { file: "data-plane" },
                    output: OutputChannel::File {
                        file: "adp.log",
                        stream: OutputStream::Merged,
                    },
                },
                ProcessSpec {
                    name: "agent",
                    exit: ExitChannel::Record { file: "agent" },
                    output: OutputChannel::File {
                        file: "agent.log",
                        stream: OutputStream::Merged,
                    },
                },
            ]
        );
        assert!(AgentAdp.readiness(POD, &EffectiveConfig::default()).is_empty());
        assert_eq!(AgentAdp.stop_grace(POD), std::time::Duration::from_secs(10));
    }

    #[test]
    fn pod_listeners_use_fixed_linux_ports_socket_defaults_and_readiness() {
        let mut env = AgentAdp.default_env(POD, &CaseValues::default());
        env.extend([
            ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
            ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC".into(), "true".into()),
        ]);
        AgentAdp
            .validate_config(
                POD,
                &CaseValues {
                    env: env.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        let config = EffectiveConfig {
            env,
            ..Default::default()
        };
        let endpoints = AgentAdp.endpoints(POD, &config).unwrap();
        assert_eq!(endpoints.len(), 6);
        for (name, kind, port) in [
            ("api", EndpointKind::Http, 55100),
            ("secure_api", EndpointKind::Tcp, 55101),
            ("agent_cmd", EndpointKind::Tcp, 55001),
            ("agent_ipc", EndpointKind::Tcp, 55004),
        ] {
            assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::tcp_accepts(name)));
        }
        assert_network_endpoint(
            &endpoints["dogstatsd_udp"],
            EndpointKind::Udp,
            58125,
            Some(Evidence::output_matches(
                "adp",
                r#"listen_addr:"udp://[^"]+:58125" \| DogStatsD listener started\."#,
            )),
        );
        assert_socket_endpoint(
            &endpoints["dogstatsd_uds"],
            EndpointKind::UnixDatagram,
            "/var/run/datadog/dsd.socket",
            Some(Evidence::socket_exists("dogstatsd_uds")),
        );
    }

    #[test]
    fn pod_validation_rejects_invalid_listener_configuration() {
        for (key, value) in [
            ("DD_DOGSTATSD_PORT", "not-a-port"),
            ("DD_CMD_PORT", "65536"),
            ("DD_DATA_PLANE_API_LISTEN_ADDRESS", "tcp://127.0.0.1:55100"),
        ] {
            let config = CaseValues {
                env: BTreeMap::from([
                    ("DD_DATA_PLANE_ENABLED".into(), "true".into()),
                    (key.into(), value.into()),
                ]),
                ..Default::default()
            };
            let error = AgentAdp.validate_config(POD, &config).unwrap_err();
            assert!(error.contains(key), "{error}");
            assert_eq!(error, AgentAdp.validate_config(LINUX, &config).unwrap_err());
        }
    }
}
