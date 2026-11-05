//! Pod and Service manifests with node-backed shared files and a read-only courier.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::{
    api::core::v1::{
        Container, ContainerPort, EnvVar, HostPathVolumeSource, Pod, PodSpec, Service, ServicePort, ServiceSpec,
        Volume, VolumeMount,
    },
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};
use saluki_error::{generic_error, GenericError};
use serde_json::Value;

use super::{node::Node, CRI_SOCKET, CRI_SOCKET_PATH, HOST_PID};
use crate::{
    backend::runtime::LateFacts,
    case::template::{Reference, Template},
    endpoint::{RunnerHostPort, Transport},
    handle::FileKind,
    recipe::PodRecipe,
};

pub(super) const COURIER_PATH: &str = "/panoramic/courier";

pub(super) struct ManifestInput<'a> {
    pub(super) namespace: &'a str,
    pub(super) root: &'a str,
    pub(super) node: &'a str,
    pub(super) name: &'a str,
    pub(super) recipe: &'a PodRecipe,
    pub(super) images: &'a [String],
    pub(super) options: &'a BTreeMap<String, Value>,
    pub(super) routes: &'a [RunnerHostPort],
}

pub(super) struct PlacedFile {
    pub(super) node_path: String,
    pub(super) mode: u32,
    pub(super) contents: Vec<u8>,
}

pub(super) struct Manifests {
    pub(super) pod: Pod,
    pub(super) service: Service,
    pub(super) files: Vec<PlacedFile>,
    pub(super) readable_files: BTreeMap<String, String>,
    late_files: Vec<LateFile>,
}

struct LateFile {
    path: String,
    node_path: String,
    template: Template,
}

impl Manifests {
    pub(super) fn build(input: ManifestInput<'_>) -> Result<Self, GenericError> {
        let ManifestInput {
            namespace,
            root,
            node,
            name,
            recipe,
            images,
            options,
            routes,
        } = input;
        check_name(name)?;
        for (key, value) in options {
            if !matches!(key.as_str(), HOST_PID | CRI_SOCKET) || !value.is_boolean() {
                return Err(generic_error!(
                    "Invalid pod option '{key}': expected {HOST_PID} or {CRI_SOCKET} with a boolean value"
                ));
            }
        }
        if recipe.containers.is_empty() || recipe.containers.len() != images.len() {
            return Err(generic_error!(
                "Pod '{name}' needs at least one container and a verified image per container"
            ));
        }
        if routes.iter().any(|port| port.transport != Transport::Tcp) {
            return Err(generic_error!("Pod '{name}' supports only TCP port-forwards"));
        }
        let cri_socket = options.get(CRI_SOCKET).and_then(Value::as_bool).unwrap_or(false);
        if cri_socket
            && !recipe.containers.iter().any(|container| {
                container
                    .recipe
                    .mounts
                    .iter()
                    .any(|mount| mount.host_path == CRI_SOCKET_PATH)
            })
        {
            return Err(generic_error!(
                "Pod '{name}' with cri_socket: true requires a host mount of '{CRI_SOCKET_PATH}'"
            ));
        }
        let labels: BTreeMap<String, String> = [("panoramic-participant".into(), name.into())].into();
        let mut volumes = vec![
            host_volume("shared", root, "Directory"),
            host_volume("courier", &format!("{root}/{name}/courier"), "File"),
        ];
        let common_mounts = vec![mount("shared", "/airlock", false), mount("courier", COURIER_PATH, true)];
        let mut names = BTreeSet::new();
        let mut ports = BTreeSet::new();
        let mut containers = Vec::new();
        let mut files: Vec<PlacedFile> = Vec::new();
        let mut readable_files = BTreeMap::new();
        let mut late_files = Vec::new();
        let mut unplaced: BTreeSet<_> = recipe.late_files.keys().cloned().collect();
        for (index, (container, image)) in recipe.containers.iter().zip(images).enumerate() {
            check_name(&container.name)?;
            if !names.insert(&container.name) {
                return Err(generic_error!("Pod '{name}' repeats container '{}'", container.name));
            }
            let late_paths = &recipe.late_files;
            let recipe = &container.recipe;
            let mut env = recipe.env.clone();
            for (key, value) in [
                ("PANORAMIC_PARTICIPANT", name.to_string()),
                ("PANORAMIC_EXIT_DIR", format!("/airlock/{name}/exit")),
                ("PANORAMIC_LOG_DIR", format!("/airlock/{name}/log")),
            ] {
                if env.insert(key.into(), value).is_some() {
                    return Err(generic_error!("Pod '{name}' sets reserved variable '{key}'"));
                }
            }
            let mut mounts = common_mounts.clone();
            for (mount_index, host) in recipe.mounts.iter().enumerate() {
                check_path(&host.host_path)?;
                check_path(&host.container_path)?;
                if mounts
                    .iter()
                    .any(|mount| overlaps(&host.container_path, &mount.mount_path))
                {
                    return Err(generic_error!(
                        "Pod host mount '{}' overlaps another mount or a reserved path",
                        host.container_path
                    ));
                }
                let volume_name = format!("host-{index}-{mount_index}");
                let kind = if cri_socket && host.host_path == CRI_SOCKET_PATH {
                    "Socket"
                } else {
                    ""
                };
                volumes.push(host_volume(&volume_name, &host.host_path, kind));
                mounts.push(mount(&volume_name, &host.container_path, host.read_only));
            }
            let mut paths = BTreeSet::new();
            for (file_index, file) in recipe.files.iter().enumerate() {
                check_path(&file.path)?;
                if file.mode & !0o7777 != 0 {
                    return Err(generic_error!("Invalid mode for pod file '{}'", file.path));
                }
                let shared = file.path.strip_prefix(&format!("/airlock/{name}/shared/"));
                if !paths.insert(&file.path)
                    || mounts.iter().any(|mount| {
                        !(mount.name == "shared" && shared.is_some()) && overlaps(&file.path, &mount.mount_path)
                    })
                {
                    return Err(generic_error!(
                        "Pod file '{}' overlaps another file or a reserved mount",
                        file.path
                    ));
                }
                if recipe
                    .files
                    .iter()
                    .any(|other| other.path != file.path && overlaps(&file.path, &other.path))
                {
                    return Err(generic_error!("Pod file '{}' overlaps another recipe file", file.path));
                }
                let node_path = if let Some(relative) = shared {
                    format!("{root}/{name}/shared/{relative}")
                } else {
                    let node_path = format!("{root}/{name}/files/{index}/{file_index}");
                    let volume_name = format!("file-{index}-{file_index}");
                    volumes.push(host_volume(&volume_name, &node_path, "File"));
                    mounts.push(mount(&volume_name, &file.path, false));
                    node_path
                };
                if files.iter().any(|placed| placed.node_path == node_path) {
                    return Err(generic_error!("Pod containers both place shared file '{}'", file.path));
                }
                if index == 0 {
                    readable_files.insert(file.path.clone(), node_path.clone());
                }
                let contents = if late_paths.contains_key(&file.path) {
                    unplaced.remove(&file.path);
                    let template = String::from_utf8(file.contents.clone())?
                        .parse::<Template>()
                        .map_err(|error| generic_error!(error))?;
                    if template
                        .references()
                        .any(|reference| !matches!(reference, Reference::Late { .. }))
                    {
                        return Err(generic_error!(
                            "Pod late file '{}' has an unresolved non-late reference",
                            file.path
                        ));
                    }
                    late_files.push(LateFile {
                        path: file.path.clone(),
                        node_path: node_path.clone(),
                        template,
                    });
                    Vec::new()
                } else {
                    file.contents.clone()
                };
                files.push(PlacedFile {
                    node_path,
                    mode: file.mode,
                    contents,
                });
            }
            let mut container_ports = Vec::new();
            for &(transport, port) in &container.ports {
                if port == 0 || !ports.insert((transport, port)) {
                    return Err(generic_error!(
                        "Pod '{name}' has a zero or duplicate {} port {port}",
                        transport.name()
                    ));
                }
                container_ports.push(ContainerPort {
                    container_port: port.into(),
                    protocol: Some(transport.name().to_ascii_uppercase()),
                    ..Default::default()
                });
            }
            let entrypoint = recipe
                .entrypoint
                .as_ref()
                .filter(|argv| !argv.is_empty())
                .ok_or_else(|| {
                    generic_error!(
                        "Pod container '{}' needs an explicit entrypoint for its file gate",
                        container.name
                    )
                })?;
            let command = recipe.command.as_ref().ok_or_else(|| {
                generic_error!(
                    "Pod container '{}' needs explicit command arguments (which may be empty)",
                    container.name
                )
            })?;
            let mut args = vec![
                "until [ -f \"$1\" ]; do sleep 1; done; shift; exec \"$@\"".into(),
                "pod-files".into(),
                format!("/airlock/{name}/files-ready"),
            ];
            args.extend(entrypoint.iter().chain(command).cloned());
            containers.push(Container {
                name: container.name.clone(),
                image: Some(image.clone()),
                image_pull_policy: Some("Never".into()),
                command: Some(vec!["sh".into(), "-eu".into(), "-c".into()]),
                args: Some(args),
                env: Some(
                    env.into_iter()
                        .map(|(name, value)| EnvVar {
                            name,
                            value: Some(value),
                            ..Default::default()
                        })
                        .collect(),
                ),
                volume_mounts: Some(mounts),
                ports: Some(container_ports),
                ..Default::default()
            });
        }
        if !unplaced.is_empty() {
            return Err(generic_error!(
                "Pod '{name}' declares late files absent from its recipes: {unplaced:?}"
            ));
        }
        ports.extend(routes.iter().map(|route| (route.transport, route.container_port)));
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(namespace.into()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: Some(node.into()),
                restart_policy: Some("Never".into()),
                host_pid: Some(options.get(HOST_PID).and_then(Value::as_bool).unwrap_or(false)),
                containers,
                volumes: Some(volumes),
                ..Default::default()
            }),
            ..Default::default()
        };
        let service = Service {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(namespace.into()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                cluster_ip: Some("None".into()),
                selector: Some(labels),
                ports: (!ports.is_empty()).then(|| {
                    ports
                        .iter()
                        .map(|&(transport, port)| ServicePort {
                            name: Some(format!("{}-{port}", transport.name())),
                            port: port.into(),
                            protocol: Some(transport.name().to_ascii_uppercase()),
                            ..Default::default()
                        })
                        .collect()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(Self {
            pod,
            service,
            files,
            readable_files,
            late_files,
        })
    }

    pub(super) async fn checked_pod(&self, node: &Node) -> Result<Pod, GenericError> {
        let mut pod = self.pod.clone();
        for volume in pod.spec.as_mut().unwrap().volumes.as_mut().unwrap() {
            if !volume.name.starts_with("host-") {
                continue;
            }
            let source = volume.host_path.as_mut().unwrap();
            let status = node
                .stat(&source.path)
                .await?
                .ok_or_else(|| generic_error!("pod host mount source '{}' is missing on the node", source.path))?;
            let kind = match status.kind {
                FileKind::Directory => "Directory",
                FileKind::File => "File",
                FileKind::Socket => "Socket",
                kind => {
                    return Err(generic_error!(
                        "pod host mount source '{}' has unsupported kind {kind:?}",
                        source.path
                    ))
                }
            };
            if source
                .type_
                .as_deref()
                .is_some_and(|expected| !expected.is_empty() && expected != kind)
            {
                return Err(generic_error!(
                    "pod host mount source '{}' must be {}, found {kind}",
                    source.path,
                    source.type_.as_deref().unwrap()
                ));
            }
            // Pin the observed kind so kubelet also checks for replacement before mounting.
            source.type_ = Some(kind.into());
        }
        Ok(pod)
    }

    pub(super) fn render_late_files<'a>(
        &'a self, holder: &str, facts: &LateFacts,
    ) -> Result<Vec<(&'a str, String)>, GenericError> {
        self.late_files
            .iter()
            .map(|late| {
                late.template
                    .substitute(|written, reference| {
                        let Reference::Late { participant, fact } = reference else {
                            unreachable!("late files are validated before creation")
                        };
                        let participant = participant.name(holder);
                        facts
                            .get(participant)
                            .and_then(|values| values.get(fact))
                            .cloned()
                            .map(Some)
                            .ok_or_else(|| format!("late file '{}' needs unavailable reference '{written}'", late.path))
                    })
                    .map(|contents| (late.node_path.as_str(), contents))
                    .map_err(|error| generic_error!(error))
            })
            .collect()
    }
}

fn host_volume(name: &str, path: &str, kind: &str) -> Volume {
    Volume {
        name: name.into(),
        host_path: Some(HostPathVolumeSource {
            path: path.into(),
            type_: Some(kind.into()),
        }),
        ..Default::default()
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.into(),
        mount_path: path.into(),
        read_only: Some(read_only),
        ..Default::default()
    }
}

fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

pub(super) fn check_name(name: &str) -> Result<(), GenericError> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(generic_error!(
            "Invalid pod or container name '{name}': expected a lowercase DNS label"
        ));
    }
    Ok(())
}

pub(super) fn check_path(path: &str) -> Result<(), GenericError> {
    if !path.starts_with('/')
        || path.contains('\0')
        || path.split('/').skip(1).any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(generic_error!(
            "Pod path '{path}' must be absolute with no empty, '.' or '..' components"
        ));
    }
    Ok(())
}
