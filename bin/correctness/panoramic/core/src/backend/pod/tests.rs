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
    assert_eq!(container.command.as_ref().unwrap(), &["sh"]);
    assert_eq!(container.args.as_ref().unwrap(), &["-c", "exec sleep 60"]);
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
fn pod_options_control_only_their_host_views() {
    for host_pid in [false, true] {
        for cri_socket in [false, true] {
            let built = build(
                &recipe("sleep 60"),
                &[
                    ("host_pid".into(), json!(host_pid)),
                    ("cri_socket".into(), json!(cri_socket)),
                ]
                .into(),
            )
            .unwrap();
            let pod = built.pod.spec.unwrap();
            assert_eq!(pod.host_pid, Some(host_pid));
            let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
            for (path, expected) in [
                ("/host/proc", host_pid),
                ("/host/sys/fs/cgroup", host_pid),
                ("/var/run/containerd/containerd.sock", cri_socket),
            ] {
                assert_eq!(
                    mounts
                        .iter()
                        .find(|mount| mount.mount_path == path)
                        .map(|mount| mount.read_only),
                    expected.then_some(Some(true))
                );
            }
        }
    }
}

#[test]
fn pod_options_are_validated_by_the_loader_and_manifest_builder() {
    let loader = CaseLoader::new(fixtures::REGISTRIES, fixtures::FixtureTypes);
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
    assert!(build(&PodRecipe { containers: vec![] }, &BTreeMap::new()).is_err());
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
