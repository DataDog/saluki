use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::{
    artifact::{ArtifactRole, VerifiedArtifact},
    backend::COURIER_LINUX_BINARY,
    case::{fixtures, CaseLoader},
    endpoint::Transport,
    lifecycle::NativeInputs,
    recipe::{ContainerRecipe, PodContainer, PodRecipe},
};

pub(super) const IMAGE: ArtifactRole = ArtifactRole {
    name: "pod-test-image",
    kind: ArtifactKind::Image { os: Os::Linux },
    local_default: "alpine:latest",
};

pub(super) fn recipe(script: &str) -> PodRecipe {
    PodRecipe {
        containers: vec![PodContainer {
            name: "main".into(),
            recipe: ContainerRecipe::new(IMAGE)
                .with_entrypoint(["sh", "-c"])
                .with_command([script]),
            ports: vec![(Transport::Tcp, 8080)],
        }],
        late_files: Default::default(),
    }
}

fn build(recipe: &PodRecipe, options: &BTreeMap<String, serde_json::Value>) -> Result<Manifests, GenericError> {
    Manifests::build(ManifestInput {
        namespace: "scope",
        root: "/tmp/panoramic-run/0000",
        node: "node-control-plane",
        name: "server",
        recipe,
        images: &vec!["local:test".into(); recipe.containers.len()],
        options,
        routes: &[],
    })
}

#[test]
fn manifests_keep_commands_files_ports_and_named_service_routing() {
    let mut recipe = recipe("sleep 60");
    recipe.containers[0].recipe = ContainerRecipe::new(IMAGE)
        .with_entrypoint(["sh"])
        .with_command(["-c", "exec sleep 60"])
        .with_env([("CUSTOM".into(), "value".into())].into())
        .with_file("/etc/settings", 0o640, b"\0\xffconfig".to_vec());
    recipe.containers[0].ports.push((Transport::Udp, 8080));
    let built = build(&recipe, &BTreeMap::new()).unwrap();
    let pod = built.pod.spec.unwrap();
    assert_eq!(pod.node_name.as_deref(), Some("node-control-plane"));
    assert_eq!(pod.host_pid, Some(false));
    assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
    let container = &pod.containers[0];
    assert_eq!(container.image.as_deref(), Some("local:test"));
    assert_eq!(container.image_pull_policy.as_deref(), Some("Never"));
    assert_eq!(container.command.as_ref().unwrap(), &["sh", "-eu", "-c"]);
    assert_eq!(
        container.args.as_ref().unwrap(),
        &[
            "until [ -f \"$1\" ]; do sleep 1; done; shift; exec \"$@\"",
            "pod-files",
            "/airlock/server/files-ready",
            "sh",
            "-c",
            "exec sleep 60",
        ]
    );
    let env: BTreeMap<_, _> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .map(|entry| (entry.name.as_str(), entry.value.as_deref().unwrap()))
        .collect();
    assert_eq!(
        env,
        [
            ("CUSTOM", "value"),
            ("PANORAMIC_PARTICIPANT", "server"),
            ("PANORAMIC_EXIT_DIR", "/airlock/server/exit"),
            ("PANORAMIC_LOG_DIR", "/airlock/server/log")
        ]
        .into()
    );
    let mounts = container.volume_mounts.as_ref().unwrap();
    assert_eq!(
        mounts
            .iter()
            .map(|mount| (mount.mount_path.as_str(), mount.read_only))
            .collect::<Vec<_>>(),
        [
            ("/airlock", Some(false)),
            (COURIER_PATH, Some(true)),
            ("/etc/settings", Some(false))
        ]
    );
    assert_eq!(built.files.len(), 1);
    assert_eq!(built.files[0].node_path, "/tmp/panoramic-run/0000/server/files/0/0");
    assert_eq!(built.files[0].mode, 0o640);
    assert_eq!(built.files[0].contents, b"\0\xffconfig");
    assert_eq!(built.readable_files["/etc/settings"], built.files[0].node_path);
    let volumes = pod.volumes.unwrap();
    assert_eq!(volumes[0].host_path.as_ref().unwrap().path, "/tmp/panoramic-run/0000");
    assert_eq!(
        volumes[1].host_path.as_ref().unwrap().path,
        "/tmp/panoramic-run/0000/server/courier"
    );
    let service = built.service;
    assert_eq!(service.metadata.name.as_deref(), Some("server"));
    let service = service.spec.unwrap();
    assert_eq!(service.cluster_ip.as_deref(), Some("None"));
    assert_eq!(service.selector, built.pod.metadata.labels);
    let ports = service.ports.unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port, port.protocol.as_deref()))
            .collect::<Vec<_>>(),
        [(8080, Some("TCP")), (8080, Some("UDP"))]
    );
}

#[test]
fn shared_recipe_files_use_the_scope_mount() {
    let mut recipe = recipe("sleep 60");
    recipe.containers[0].recipe.files.push(crate::recipe::RecipeFile {
        path: "/airlock/server/shared/settings".into(),
        mode: 0o600,
        contents: b"shared".to_vec(),
    });
    let built = build(&recipe, &BTreeMap::new()).unwrap();
    assert_eq!(
        built.files[0].node_path,
        "/tmp/panoramic-run/0000/server/shared/settings"
    );
    assert_eq!(
        built.readable_files["/airlock/server/shared/settings"],
        built.files[0].node_path
    );
    assert_eq!(built.pod.spec.unwrap().volumes.unwrap().len(), 2);
}

#[test]
fn pod_options_set_pid_sharing_and_require_a_declared_cri_mount() {
    for host_pid in [false, true] {
        for cri_socket in [false, true] {
            let options = [
                (HOST_PID.into(), json!(host_pid)),
                (CRI_SOCKET.into(), json!(cri_socket)),
            ]
            .into();
            let mut recipe = recipe("sleep 60");
            if cri_socket {
                let error = build(&recipe, &options).err().unwrap();
                assert!(error.to_string().contains("requires a host mount"), "{error}");
                recipe.containers[0].recipe.mounts.push(crate::recipe::HostMount {
                    host_path: CRI_SOCKET_PATH.into(),
                    container_path: "/node/socket".into(),
                    read_only: true,
                });
            }
            let built = build(&recipe, &options).unwrap();
            let pod = built.pod.spec.unwrap();
            assert_eq!(pod.host_pid, Some(host_pid));
            let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
            assert_eq!(mounts.len(), if cri_socket { 3 } else { 2 });
            let volumes = pod.volumes.unwrap();
            assert_eq!(volumes.len(), mounts.len());
            if cri_socket {
                assert_eq!(volumes[2].host_path.as_ref().unwrap().type_.as_deref(), Some("Socket"));
                assert_eq!(mounts[2].mount_path, "/node/socket");
            }
        }
    }
}

#[test]
fn pod_host_mounts_come_from_recipes_and_reject_file_or_mount_overlap() {
    let mut recipe = recipe("true");
    recipe.containers[0].recipe = ContainerRecipe::new(IMAGE)
        .with_entrypoint(["true"])
        .with_command(Vec::<String>::new())
        .with_host_mount("/node/data", "/inside/data", true)
        .with_host_mount("/node/socket", "/inside/socket", false);
    let built = build(&recipe, &BTreeMap::new()).unwrap();
    let pod = built.pod.spec.unwrap();
    let volumes = pod.volumes.unwrap();
    let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
    for (index, source, destination, read_only) in [
        (2, "/node/data", "/inside/data", true),
        (3, "/node/socket", "/inside/socket", false),
    ] {
        assert_eq!(volumes[index].host_path.as_ref().unwrap().path, source);
        assert_eq!(mounts[index].mount_path, destination);
        assert_eq!(mounts[index].read_only, Some(read_only));
        assert_eq!(mounts[index].name, volumes[index].name);
    }
    recipe.containers[0].recipe.files.push(crate::recipe::RecipeFile {
        path: "/inside/data/settings".into(),
        mode: 0o644,
        contents: vec![],
    });
    assert!(build(&recipe, &BTreeMap::new())
        .err()
        .unwrap()
        .to_string()
        .contains("overlaps"));
    recipe.containers[0].recipe.files.clear();
    for destination in ["/inside/data", "/inside", "/airlock", "/airlock/shared", COURIER_PATH] {
        recipe.containers[0].recipe.mounts.push(crate::recipe::HostMount {
            host_path: "/node/other".into(),
            container_path: destination.into(),
            read_only: true,
        });
        assert!(
            build(&recipe, &BTreeMap::new())
                .err()
                .unwrap()
                .to_string()
                .contains("overlaps"),
            "{destination}"
        );
        recipe.containers[0].recipe.mounts.pop();
    }
    for (source, destination) in [
        ("relative", "/valid"),
        ("/node/../bad", "/valid"),
        ("/valid", "relative"),
    ] {
        recipe.containers[0].recipe.mounts.push(crate::recipe::HostMount {
            host_path: source.into(),
            container_path: destination.into(),
            read_only: true,
        });
        assert!(build(&recipe, &BTreeMap::new()).is_err());
        recipe.containers[0].recipe.mounts.pop();
    }
}

#[test]
fn pod_options_are_validated_by_the_loader_and_manifest_builder() {
    let loader = CaseLoader::new(fixtures::REGISTRIES);
    for (options, valid) in [
        ("host_pid: true, cri_socket: false", true),
        ("unknown: true", false),
        ("host_pid: 1", false),
        ("cri_socket: 'true'", false),
    ] {
        let yaml = format!("timeout: 10s\nsubject_os: [linux]\nparticipants:\n  server:\n    type: server\n    subject: true\n    realization: {{kind: kubernetes-pod, options: {{{options}}}}}\nprocedure:\n  - step: noop\n");
        let result = loader.load_content("cases", &yaml, "case", Path::new("config.yaml"));
        assert_eq!(result.is_ok(), valid, "{options}: {:?}", result.err());
        let options = serde_saphyr::from_str(&format!("{{{options}}}")).unwrap();
        assert_eq!(build(&recipe("sleep 60"), &options).is_ok(), valid);
    }
}

#[test]
fn invalid_pod_files_names_ports_and_reserved_environment_fail_before_creation() {
    for path in [
        "relative",
        "/a/../b",
        "/a//b",
        "/a/./b",
        "/",
        COURIER_PATH,
        "/panoramic",
        "/airlock",
    ] {
        let mut recipe = recipe("sleep 60");
        recipe.containers[0].recipe.files.push(crate::recipe::RecipeFile {
            path: path.into(),
            mode: 0o600,
            contents: vec![],
        });
        assert!(build(&recipe, &BTreeMap::new()).is_err(), "{path}");
    }
    for name in ["", "UPPER", "bad_name", "-bad", "bad-", &"x".repeat(64)] {
        let mut recipe = recipe("sleep 60");
        recipe.containers[0].name = name.into();
        assert!(build(&recipe, &BTreeMap::new()).is_err(), "{name}");
    }
    for port in [0, 8080] {
        let mut recipe = recipe("sleep 60");
        recipe.containers[0].ports.push((Transport::Tcp, port));
        assert!(build(&recipe, &BTreeMap::new()).is_err());
    }
    for key in ["PANORAMIC_PARTICIPANT", "PANORAMIC_EXIT_DIR", "PANORAMIC_LOG_DIR"] {
        let mut recipe = recipe("sleep 60");
        recipe.containers[0].recipe.env.insert(key.into(), "override".into());
        assert!(build(&recipe, &BTreeMap::new())
            .err()
            .unwrap()
            .to_string()
            .contains(key));
    }
    assert!(build(
        &PodRecipe {
            containers: vec![],
            late_files: Default::default(),
        },
        &BTreeMap::new()
    )
    .is_err());
}

#[test]
fn native_inputs_preserve_image_identity_and_reference_separately() {
    let verified = [VerifiedArtifact {
        role: IMAGE.name,
        reference: "local:test".into(),
        identity: "sha256:identity".into(),
        kind: IMAGE.kind,
    }];
    let inputs = NativeInputs::new(Arc::new(RunBackends::default()), &verified, Path::new(".")).unwrap();
    assert_eq!(inputs.image(IMAGE).unwrap(), "sha256:identity");
    assert_eq!(inputs.image_reference(IMAGE).unwrap(), "local:test");
    assert!(inputs.image_reference(COURIER_LINUX_BINARY).is_err());
    assert_eq!(Recipe::Pod(recipe("sleep 60")).roles(), vec![IMAGE]);
}

#[test]
fn late_facts_wait_for_all_containers_and_choose_the_primary_identity() {
    let mut pod: Pod = serde_json::from_value(json!({
        "metadata": {"uid": "pod-uid"},
        "spec": {"containers": [{"name": "main"}, {"name": "helper"}]},
        "status": {"phase": "Running", "podIP": "10.0.0.2", "containerStatuses": [
            {"name": "helper", "image": "local", "imageID": "id", "ready": true, "restartCount": 0, "containerID": "containerd://helper-id"},
            {"name": "main", "image": "local", "imageID": "id", "ready": true, "restartCount": 0, "containerID": "containerd://primary-id"}
        ]}
    })).unwrap();
    assert_eq!(
        started_facts(&pod, "main").unwrap().unwrap(),
        vec![
            (LateFact::ContainerId, "primary-id".into()),
            (LateFact::Ip, "10.0.0.2".into()),
            (LateFact::PodUid, "pod-uid".into())
        ]
    );
    pod.status.as_mut().unwrap().container_statuses.as_mut().unwrap()[1].container_id = None;
    assert!(started_facts(&pod, "main").unwrap().is_none());
}

#[test]
fn late_files_require_all_referenced_facts_before_any_file_can_be_released() {
    let mut recipe = recipe("cat /settings");
    recipe.containers[0].recipe.files.extend([
        crate::recipe::RecipeFile {
            path: "/settings".into(),
            mode: 0o640,
            contents: b"own={{ self.late.pod_uid }} id={{server.late.container_id}} peer={{ peer.late.ip }}".to_vec(),
        },
        crate::recipe::RecipeFile {
            path: "/airlock/server/shared/peer".into(),
            mode: 0o600,
            contents: b"{{peer.late.pod_uid}}".to_vec(),
        },
        crate::recipe::RecipeFile {
            path: "/literal".into(),
            mode: 0o600,
            contents: b"{{not-a-template}}\0\xff".to_vec(),
        },
    ]);
    recipe.late_files = [
        ("/settings".into(), "settings".into()),
        ("/airlock/server/shared/peer".into(), "peer".into()),
    ]
    .into();
    let built = build(&recipe, &BTreeMap::new()).unwrap();
    assert!(built.files[0].contents.is_empty());
    assert!(built.files[1].contents.is_empty());
    assert_eq!(built.files[2].contents, b"{{not-a-template}}\0\xff");
    assert_eq!(built.files[0].mode, 0o640);
    let mut facts = super::super::runtime::LateFacts::new();
    assert!(built
        .render_late_files("server", &facts)
        .unwrap_err()
        .to_string()
        .contains("self.late.pod_uid"));
    facts.insert(
        "server".into(),
        [
            (LateFact::PodUid, "own-uid".into()),
            (LateFact::ContainerId, "own-id".into()),
        ]
        .into(),
    );
    assert!(built
        .render_late_files("server", &facts)
        .unwrap_err()
        .to_string()
        .contains("peer.late.ip"));
    facts.insert("peer".into(), [(LateFact::Ip, "10.0.0.7".into())].into());
    assert!(built
        .render_late_files("server", &facts)
        .unwrap_err()
        .to_string()
        .contains("peer.late.pod_uid"));
    facts
        .get_mut("peer")
        .unwrap()
        .insert(LateFact::PodUid, "peer-uid".into());
    assert_eq!(
        built.render_late_files("server", &facts).unwrap(),
        [
            (
                "/tmp/panoramic-run/0000/server/files/0/0",
                "own=own-uid id=own-id peer=10.0.0.7".into()
            ),
            ("/tmp/panoramic-run/0000/server/shared/peer", "peer-uid".into()),
        ]
    );
}

#[test]
fn late_file_and_gate_errors_are_rejected_before_pod_creation() {
    for contents in [
        &b"{{ self.facts.path }}"[..],
        b"{{ self.connections.upstream.url }}",
        b"{{broken}}",
        b"\xff",
    ] {
        let mut recipe = recipe("true");
        recipe.containers[0].recipe.files.push(crate::recipe::RecipeFile {
            path: "/settings".into(),
            mode: 0o600,
            contents: contents.into(),
        });
        recipe.late_files.insert("/settings".into(), "settings".into());
        assert!(build(&recipe, &BTreeMap::new()).is_err());
    }
    let mut absent = recipe("true");
    absent.late_files.insert("/missing".into(), "missing".into());
    assert!(build(&absent, &BTreeMap::new())
        .err()
        .unwrap()
        .to_string()
        .contains("absent"));
    let mut implicit = recipe("true");
    implicit.containers[0].recipe.entrypoint = None;
    assert!(build(&implicit, &BTreeMap::new())
        .err()
        .unwrap()
        .to_string()
        .contains("explicit entrypoint"));
    implicit.containers[0].recipe.entrypoint = Some(vec!["true".into()]);
    implicit.containers[0].recipe.command = None;
    assert!(build(&implicit, &BTreeMap::new())
        .err()
        .unwrap()
        .to_string()
        .contains("explicit command"));
}

#[test]
fn every_container_waits_for_the_same_file_gate_with_its_argv_preserved() {
    let mut recipe = recipe("exec sleep 60");
    recipe.containers.push(PodContainer {
        name: "helper".into(),
        recipe: ContainerRecipe::new(IMAGE)
            .with_entrypoint(["/path with spaces/tool"])
            .with_command(["$literal; 'quoted'"]),
        ports: vec![],
    });
    let built = build(&recipe, &BTreeMap::new()).unwrap();
    let pod = built.pod.spec.unwrap();
    for container in &pod.containers {
        assert_eq!(container.command.as_ref().unwrap(), &["sh", "-eu", "-c"]);
        assert_eq!(container.args.as_ref().unwrap()[2], "/airlock/server/files-ready");
    }
    assert_eq!(
        &pod.containers[1].args.as_ref().unwrap()[3..],
        &["/path with spaces/tool", "$literal; 'quoted'"]
    );
}

#[test]
fn startup_reports_terminal_wait_reasons_and_keeps_pending_diagnostics() {
    for reason in [
        "ErrImageNeverPull",
        "CreateContainerConfigError",
        "CreateContainerError",
        "InvalidImageName",
        "ContainerCreating",
    ] {
        let pod: Pod = serde_json::from_value(json!({"status": {"phase": "Pending", "containerStatuses": [
            {"name": "worker", "image": "local", "imageID": "", "ready": false, "restartCount": 0,
             "state": {"waiting": {"reason": reason, "message": "startup detail"}}}
        ]}}))
        .unwrap();
        let result = startup_status(&pod);
        if reason == "ContainerCreating" {
            assert_eq!(
                result.unwrap(),
                "phase Pending; container 'worker': ContainerCreating: startup detail"
            );
        } else {
            assert_eq!(
                result.unwrap_err().to_string(),
                format!("Pod cannot start: container 'worker': {reason}: startup detail")
            );
        }
    }
    assert_eq!(startup_status(&Pod::default()).unwrap(), "phase unknown");
}

#[test]
fn deletion_grace_rounds_up_without_exceeding_the_integer_range() {
    for (grace, remaining, expected) in [
        (Duration::ZERO, Duration::from_secs(10), 1),
        (Duration::from_millis(100), Duration::from_secs(10), 1),
        (Duration::from_millis(1500), Duration::from_secs(10), 2),
        (Duration::from_secs(10), Duration::from_millis(500), 1),
        (Duration::from_secs(10), Duration::ZERO, 0),
        (Duration::MAX, Duration::MAX, u32::MAX),
    ] {
        assert_eq!(deletion_grace(grace, remaining), expected);
    }
}

#[test]
fn exec_status_keeps_exit_data_and_reports_missing_programs_as_code_127() {
    for (status, code, stderr) in [
        (json!({"status": "Success"}), 0, "diagnostic"),
        (
            json!({"status": "Failure", "reason": "NonZeroExitCode", "details": {"causes": [{"reason": "ExitCode", "message": "7"}]}}),
            7,
            "diagnostic",
        ),
        (
            json!({"status": "Failure", "message": "executable file not found in PATH"}),
            127,
            "diagnostic\nexecutable file not found in PATH",
        ),
    ] {
        assert_eq!(
            exec_result(
                serde_json::from_value(status).unwrap(),
                b"output".to_vec(),
                b"diagnostic".to_vec()
            )
            .unwrap(),
            ExecOutput {
                stdout: b"output".to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                exit_code: code
            }
        );
    }
    for status in [
        json!({"status": "Failure", "message": "exec stream closed"}),
        json!({"status": "Failure", "reason": "NonZeroExitCode"}),
    ] {
        assert!(exec_result(serde_json::from_value(status).unwrap(), vec![], vec![]).is_err());
    }
}
