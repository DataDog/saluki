//! The run-time half of the pod backend: the namespace of each case, and the pod of each participant.
//!
//! Per case, the backend makes one namespace and one directory on the single node of the kind cluster. The directory
//! holds everything the pods mount: each participant's own directories under `airlock/`, which every pod mounts at
//! `/airlock`, the courier, and each participant's files. Per participant, it writes the files on the node, makes a
//! headless Service named after the participant, and, at start, submits one pod with one container, also named after
//! the participant, and waits for it to run. Every resource is entered in the ledger before it is made, and the ledger
//! reclaims the case: the namespaces, then the node directory.
//!
//! A file that holds a value only the started pod has, such as its UID, is written to the node after the pod starts. The
//! pod's command runs behind a shell loop that waits for the harness to say the files are there, and then copies each
//! into place. Every other pod runs its image's own program.
//!
//! A pod's files are read through the node, and so are its log files, which the courier follows from the node.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use airlock::container::{Container, Output, OutputStream};
use airlock::files::Archive;
use async_trait::async_trait;
use bollard::Docker;
use bytes::Bytes;
use futures::io::AsyncReadExt as _;
use k8s_openapi::api::core::v1::{
    Container as PodContainer, ContainerStatus, EnvVar, EnvVarSource, HostPathVolumeSource, Namespace,
    ObjectFieldSelector, Pod, PodSpec, Service, ServiceAccount, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference, Status};
use kube::api::{Api, AttachParams, LogParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::OnceCell;
use tokio::time::{sleep_until, timeout_at, Instant};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::backend::{
    Backend, Created, Exec, Identities, Preparing, Realized, Running, Scope, CRI_SOCKET, EXIT_DIR_ENV, HOST_PID,
    KUBELET, LOG_DIR_ENV,
};
use super::container::exec_in;
use super::handle::HandleError;
use super::ledger::{random_id, ResourceLedger};
use super::observe::Sink;
use super::participant::Enclosure;
use super::plan::{Launch, PlannedFile, PlannedParticipant};
use super::reference::{Later, Place, Values};
use super::spec::{Contents, OutputChannel, ProcessSpec, Protocol};
use super::tasks::CaseTasks;
use super::template::Template;
use super::wiring::PlannedAddress;

/// The mode of each of a participant's own directories, writable by a process that does not run as root.
const OWN_DIR_MODE: u32 = 0o777;
const OTHER_DIR_MODE: u32 = 0o755;
const COURIER_MODE: u32 = 0o755;
/// The mode of a placed file that the plan gives none.
const DEFAULT_FILE_MODE: u32 = 0o644;
/// The root of the participants' directories, as every pod sees it.
const POD_ROOT: &str = "/airlock";
/// The directory on the kind node that holds every case's directory.
const NODE_ROOT: &str = "/panoramic";
/// The label `make clean-kind` selects namespaces by.
const NAMESPACE_LABEL: (&str, &str) = ("created-by", "panoramic-kind");
/// The label a participant's Service selects its pod by.
const PARTICIPANT_LABEL: &str = "panoramic-participant";
/// Where the kind node has the container runtime's socket, and where a pod that asks for it gets it.
const NODE_CRI_SOCKET: &str = "/run/containerd/containerd.sock";
const POD_CRI_SOCKET: &str = "/var/run/containerd/containerd.sock";
/// The service account of a pod with the realization option `kubelet`, one in each case's namespace.
const KUBELET_ACCOUNT: &str = "kubelet";
/// What the service account may read: the kubelet API, as the Datadog Helm chart's ClusterRole for the node Agent grants
/// it. The kubelet authorizes `/pods` and the other paths without a resource of their own as `nodes/proxy`.
const KUBELET_RESOURCES: &[&str] = &["nodes/metrics", "nodes/spec", "nodes/proxy", "nodes/stats"];
/// The file in a participant's late directory that tells its pod's command that every late file is in place.
const READY_FILE: &str = "ready";
/// How long a pod's termination may take after the case has no more use of it, which bounds namespace deletion.
const GRACE_SECONDS: i64 = 5;
/// How long a pod may take to have its container running or finished, as pulling and starting is part of it.
const RUNNING_TIME: Duration = Duration::from_secs(120);
const RUNNING_POLL: Duration = Duration::from_millis(500);
/// How often the exit watch reads the container's state.
const EXIT_POLL: Duration = Duration::from_secs(1);
/// How long a quick question about the pod may take.
const STATUS_LIMIT: Duration = Duration::from_secs(5);
/// How long connecting to the cluster may take.
const CONNECT_TIME: Duration = Duration::from_secs(30);
/// Reasons a container waits for that no wait will change.
const FATAL_WAITING: &[&str] = &[
    "ErrImagePull",
    "ImagePullBackOff",
    "ErrImageNeverPull",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
];

/// Creates the pods of cases on the one node of a kind cluster.
#[derive(Clone)]
pub(crate) struct PodBackend {
    pub(super) client: Client,
    pub(super) node: Container,
    /// The courier binary, placed on the node for every pod to mount.
    courier: Bytes,
}

impl PodBackend {
    /// Connects to the kind cluster `cluster`: its one node, through Docker, and its API, through the context
    /// of the cluster in the Kubernetes config file, whatever context is current.
    ///
    /// # Errors
    ///
    /// Returns an error if the cluster does not have exactly one node, which is the host requirement of pod cases, if
    /// Docker has no container for the node, or if the Kubernetes config file has no context for the cluster.
    pub(crate) async fn connect(cluster: &str, docker: Docker, courier: Bytes) -> Result<Self, GenericError> {
        let deadline = Instant::now() + CONNECT_TIME;
        let nodes = timeout_at(
            deadline,
            Command::new("kind")
                .args(["get", "nodes", "--name", cluster])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| generic_error!("'kind get nodes' did not answer within {:?}", CONNECT_TIME))?
        .error_context("Failed to run 'kind get nodes'")?;
        let stdout = String::from_utf8_lossy(&nodes.stdout);
        let names: Vec<&str> = stdout.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
        let [node] = names[..] else {
            return Err(generic_error!(
                "Pod cases need a kind cluster with exactly one node, and cluster '{}' has {}: {}",
                cluster,
                names.len(),
                String::from_utf8_lossy(&nodes.stderr).trim()
            ));
        };
        let node = Container::existing(&docker, node, deadline)
            .await
            .with_error_context(|| format!("Failed to open the node of kind cluster '{}'", cluster))?;

        let context = format!("kind-{}", cluster);
        let kubeconfig = Kubeconfig::read().error_context("Failed to read the kubeconfig")?;
        let options = KubeConfigOptions {
            context: Some(context.clone()),
            ..KubeConfigOptions::default()
        };
        let config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .with_error_context(|| format!("The kubeconfig has no usable context '{}'", context))?;
        let client = Client::try_from(config)
            .with_error_context(|| format!("Failed to build a client for context '{}'", context))?;
        Ok(Self { client, node, courier })
    }
}

impl fmt::Debug for PodBackend {
    /// Omits the client, which isn't `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PodBackend")
            .field("node", &self.node.name())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Backend for PodBackend {
    async fn prepare(&self, cx: &Preparing<'_>) -> Result<Box<dyn Scope>, GenericError> {
        let namespace = format!("airlock-case-{}", random_id());
        let dir = format!("{}/{}", NODE_ROOT, namespace);
        // The directory goes first, so reclaim removes it after the namespace.
        cx.ledger.node_directory(self.node.clone(), dir.clone());
        cx.ledger.namespace(self.client.clone(), namespace.clone());
        let case = CasePods {
            client: self.client.clone(),
            node: self.node.clone(),
            namespace,
            namespace_uid: Arc::new(OnceCell::new()),
            kubelet: Arc::new(OnceCell::new()),
            dir,
            tasks: cx.tasks.clone(),
            ledger: cx.ledger.clone(),
        };

        let mut archive = Archive::new();
        archive.add_dir(&archive_path(&case.dir), OTHER_DIR_MODE)?;
        archive.add_dir(&archive_path(&case.node_path(POD_ROOT)), OTHER_DIR_MODE)?;
        archive.add_file(&archive_path(&case.courier()), &self.courier, COURIER_MODE)?;
        case.place(
            format!("place the directory '{}' on the node", case.dir),
            archive,
            cx.cancel,
            cx.deadline,
        )
        .await?;

        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let object = Namespace {
            metadata: ObjectMeta {
                name: Some(case.namespace.clone()),
                labels: Some([(NAMESPACE_LABEL.0.to_string(), NAMESPACE_LABEL.1.to_string())].into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let created = cx
            .ledger
            .create_by(&case.namespace, cx.cancel, cx.deadline, async {
                namespaces
                    .create(&PostParams::default(), &object)
                    .await
                    .map_err(|e| generic_error!("Failed to create namespace '{}': {}", case.namespace, e))
            })
            .await
            .ok_or_else(|| generic_error!("Cancelled while creating namespace '{}'.", case.namespace))??;
        if let Some(uid) = created.metadata.uid {
            let _ = case.namespace_uid.set(uid);
        }
        Ok(Box::new(case))
    }
}

/// The namespace and node directory of one case.
#[derive(Clone)]
struct CasePods {
    client: Client,
    node: Container,
    namespace: String,
    /// The UID of the namespace, which owns the case's cluster-wide objects.
    namespace_uid: Arc<OnceCell<String>>,
    /// Set once the service account of the pods with the realization option `kubelet` may read the kubelet API.
    kubelet: Arc<OnceCell<()>>,
    /// The case's directory on the node.
    dir: String,
    tasks: CaseTasks,
    ledger: ResourceLedger,
}

impl CasePods {
    /// The path on the node of `path`, a path under `/airlock` in every pod, or of the case's own files.
    fn node_path(&self, path: &str) -> String {
        format!("{}{}", self.dir, path)
    }

    fn courier(&self) -> String {
        self.node_path("/courier")
    }

    /// The path on the node of `path` if it is under `/airlock`, where every pod sees the same files.
    fn shared_path(&self, path: &str) -> Option<String> {
        let rest = path.strip_prefix(POD_ROOT)?.strip_prefix('/')?;
        (!rest.split('/').any(|part| part == "..")).then(|| self.node_path(&format!("{}/{}", POD_ROOT, rest)))
    }

    /// The path on the node of file number `index` of `participant`.
    fn file(&self, participant: &str, index: usize) -> String {
        self.node_path(&format!("/files/{}/{}", participant, index))
    }

    /// The path on the node of the directory that holds `participant`'s late files.
    fn late_dir(&self, participant: &str) -> String {
        self.node_path(&format!("/late/{}", participant))
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Places `archive` at the root of the node, for `what`. A placement cut short leaves the case's directory
    /// unsettled, as Docker may go on writing into it.
    async fn place(
        &self, what: String, archive: Archive, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<(), GenericError> {
        self.ledger
            .create_by(&self.dir, cancel, deadline, async {
                self.node
                    .place("/", archive, deadline)
                    .await
                    .map_err(|e| generic_error!("Failed to {}: {:#}", what, e))
            })
            .await
            .ok_or_else(|| generic_error!("Cancelled while trying to {}.", what))?
    }
}

#[async_trait]
impl Scope for CasePods {
    async fn create(
        &self, name: &str, planned: &PlannedParticipant, values: &Values, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<Box<dyn Created>, GenericError> {
        if planned.realization.enclosure != Enclosure::KubernetesPod {
            return Err(generic_error!(
                "Participant '{}' is a {}, and this backend creates {}s.",
                name,
                planned.realization.enclosure,
                Enclosure::KubernetesPod
            ));
        }
        if planned.options.get(KUBELET) == Some(&true) {
            self.kubelet
                .get_or_try_init(|| self.grant_kubelet(cancel, deadline))
                .await?;
        }
        let PodPlan { pod, files, late } = self.plan(name, planned, values)?;
        let mut forwards = BTreeMap::new();
        for (endpoint_name, endpoint) in &planned.endpoints {
            if let (Protocol::Http, PlannedAddress::Port(port)) = (endpoint.protocol, &endpoint.address) {
                forwards.insert(endpoint_name.clone(), *port);
            }
        }

        let mut archive = Archive::new();
        let facts = Enclosure::KubernetesPod.facts();
        for dir in [facts.dir(name), facts.exit_dir(name), facts.log_dir(name)]
            .into_iter()
            .flatten()
        {
            archive.add_dir(&archive_path(&self.node_path(&dir)), OWN_DIR_MODE)?;
        }
        for file in &files {
            archive.add_file(&archive_path(&file.node_path), &file.contents, file.mode)?;
        }
        if !late.is_empty() {
            archive.add_dir(&archive_path(&self.late_dir(name)), OTHER_DIR_MODE)?;
        }
        self.place(
            format!("place the files of participant '{}' on the node", name),
            archive,
            cancel,
            deadline,
        )
        .await?;

        let service = Service {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                cluster_ip: Some("None".to_string()),
                selector: Some(participant_labels(name)),
                publish_not_ready_addresses: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        self.ledger
            .create_by(&self.namespace, cancel, deadline, async {
                services
                    .create(&PostParams::default(), &service)
                    .await
                    .map_err(|e| generic_error!("Failed to create the service of participant '{}': {}", name, e))
            })
            .await
            .ok_or_else(|| generic_error!("Cancelled while creating the service of participant '{}'.", name))??;
        Ok(Box::new(CreatedPod {
            case: self.clone(),
            name: name.to_string(),
            pod,
            late,
            forwards,
        }))
    }
}

impl CasePods {
    /// Makes the service account of the case's pods with the realization option `kubelet`, and lets it read the
    /// kubelet API through a ClusterRole and a ClusterRoleBinding named after the case's namespace.
    ///
    /// The ledger deletes the two cluster-wide objects. The namespace owns them too, so the garbage collector deletes
    /// them with it when `make clean-kind` deletes a namespace that a stopped run left behind.
    async fn grant_kubelet(&self, cancel: &CancellationToken, deadline: Instant) -> Result<(), GenericError> {
        let account = ServiceAccount {
            metadata: ObjectMeta {
                name: Some(KUBELET_ACCOUNT.to_string()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let accounts: Api<ServiceAccount> = Api::namespaced(self.client.clone(), &self.namespace);
        self.ledger
            .create_by(&self.namespace, cancel, deadline, async {
                accounts
                    .create(&PostParams::default(), &account)
                    .await
                    .map_err(|e| generic_error!("Failed to create service account '{}': {}", KUBELET_ACCOUNT, e))
            })
            .await
            .ok_or_else(|| generic_error!("Cancelled while creating service account '{}'.", KUBELET_ACCOUNT))??;

        let name = format!("{}-kubelet", self.namespace);
        let metadata = ObjectMeta {
            name: Some(name.clone()),
            labels: Some([(NAMESPACE_LABEL.0.to_string(), NAMESPACE_LABEL.1.to_string())].into()),
            owner_references: self.namespace_uid.get().map(|uid| {
                vec![OwnerReference {
                    api_version: "v1".to_string(),
                    kind: "Namespace".to_string(),
                    name: self.namespace.clone(),
                    uid: uid.clone(),
                    ..Default::default()
                }]
            }),
            ..Default::default()
        };
        let role = ClusterRole {
            metadata: metadata.clone(),
            rules: Some(vec![PolicyRule {
                api_groups: Some(vec![String::new()]),
                resources: Some(KUBELET_RESOURCES.iter().map(|resource| resource.to_string()).collect()),
                verbs: vec!["get".to_string()],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let binding = ClusterRoleBinding {
            metadata,
            role_ref: RoleRef {
                api_group: "rbac.authorization.k8s.io".to_string(),
                kind: "ClusterRole".to_string(),
                name: name.clone(),
            },
            subjects: Some(vec![Subject {
                kind: "ServiceAccount".to_string(),
                name: KUBELET_ACCOUNT.to_string(),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            }]),
        };
        self.ledger.cluster_role(self.client.clone(), name.clone());
        let roles: Api<ClusterRole> = Api::all(self.client.clone());
        let bindings: Api<ClusterRoleBinding> = Api::all(self.client.clone());
        self.ledger
            .create_by(&name, cancel, deadline, async {
                roles
                    .create(&PostParams::default(), &role)
                    .await
                    .map_err(|e| generic_error!("Failed to create ClusterRole '{}': {}", name, e))?;
                bindings
                    .create(&PostParams::default(), &binding)
                    .await
                    .map_err(|e| generic_error!("Failed to create ClusterRoleBinding '{}': {}", name, e))
            })
            .await
            .ok_or_else(|| generic_error!("Cancelled while creating ClusterRole '{}'.", name))??;
        Ok(())
    }
}

/// The pod of one participant as the plan describes it, and its files.
struct PodPlan {
    pod: Pod,
    /// The files placed on the node before the pod exists: in the shared tree if under `/airlock`, and mounted at their
    /// destinations otherwise.
    files: Vec<NodeFile>,
    /// The files placed after the pod starts, and copied to their destinations by the pod's command.
    late: Vec<LateFile>,
}

/// A file written on the node before its pod exists.
struct NodeFile {
    node_path: String,
    contents: Vec<u8>,
    mode: u32,
}

/// A file written on the node after its pod starts.
struct LateFile {
    index: usize,
    destination: String,
    mode: u32,
    contents: Template,
}

impl CasePods {
    /// What the pod of participant `name` runs, and what it mounts.
    fn plan(&self, name: &str, planned: &PlannedParticipant, values: &Values) -> Result<PodPlan, GenericError> {
        let Launch::Image { image, command, args } = &planned.launch else {
            return Err(generic_error!(
                "Participant '{}' runs host programs, and a pod runs an image.",
                name
            ));
        };
        let fill = |template: &Template| {
            values
                .fill(name, template)
                .map_err(|e| generic_error!("Participant '{}': {}", name, e))
        };
        let fill_all =
            |templates: &Option<Vec<Template>>| templates.iter().flatten().map(fill).collect::<Result<Vec<_>, _>>();
        let facts = Enclosure::KubernetesPod.facts();
        let (Some(exit_dir), Some(log_dir)) = (facts.exit_dir(name), facts.log_dir(name)) else {
            return Err(generic_error!(
                "A {} has no directories of its own.",
                Enclosure::KubernetesPod
            ));
        };

        let mut env = Vec::new();
        for (key, value) in &planned.env {
            env.push(env_var(key, fill(value)?));
        }
        for (key, field) in &planned.field_env {
            env.push(field_env_var(key, field));
        }
        env.push(env_var(EXIT_DIR_ENV, exit_dir));
        env.push(env_var(LOG_DIR_ENV, log_dir));

        let mut mounts = Mounts::default();
        mounts.add("airlock", self.node_path(POD_ROOT), Some("Directory"), POD_ROOT, false);
        if let Some(courier) = facts.courier {
            mounts.add("courier", self.courier(), Some("File"), courier, true);
        }

        let mut files = Vec::new();
        let mut late = Vec::new();
        for (index, file) in planned.files.iter().enumerate() {
            let mode = file.mode.unwrap_or(DEFAULT_FILE_MODE);
            if is_late(name, planned, file) {
                let Contents::Template(template) = &file.contents else {
                    return Err(generic_error!(
                        "Participant '{}': file '{}' is not a template, and is written after the pod starts.",
                        name,
                        file.destination
                    ));
                };
                late.push(LateFile {
                    index,
                    destination: file.destination.clone(),
                    mode,
                    contents: template.clone(),
                });
                continue;
            }
            let contents = match &file.contents {
                Contents::Text(text) => text.clone().into_bytes(),
                Contents::Bytes(bytes) => bytes.clone(),
                Contents::Template(template) => fill(template)?.into_bytes(),
            };
            // A mount over the shared tree would hide the file from every other reader of it.
            let node_path = match self.shared_path(&file.destination) {
                Some(path) => path,
                None => {
                    let path = self.file(name, index);
                    mounts.add(
                        &format!("file-{}", index),
                        path.clone(),
                        Some("File"),
                        &file.destination,
                        false,
                    );
                    path
                }
            };
            files.push(NodeFile {
                node_path,
                contents,
                mode,
            });
        }
        for (index, mount) in planned.mounts.iter().enumerate() {
            mounts.add(
                &format!("mount-{}", index),
                mount.host.clone(),
                None,
                &mount.path,
                mount.read_only,
            );
        }
        if !late.is_empty() {
            let Some(path) = facts.late else {
                return Err(generic_error!(
                    "A {} has no directory for late files.",
                    Enclosure::KubernetesPod
                ));
            };
            mounts.add("late", self.late_dir(name), Some("Directory"), path, true);
        }
        if planned.options.get(CRI_SOCKET) == Some(&true) {
            mounts.add(
                "cri-socket",
                NODE_CRI_SOCKET.to_string(),
                Some("Socket"),
                POD_CRI_SOCKET,
                true,
            );
        }

        let (command, args) = match (fill_all(command)?, fill_all(args)?) {
            (command, args) if late.is_empty() => (
                (!command.is_empty()).then_some(command),
                (!args.is_empty()).then_some(args),
            ),
            (command, args) => {
                let mut wrapped = vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    wait_script(&late),
                    "panoramic-late".to_string(),
                ];
                wrapped.extend(command);
                wrapped.extend(args);
                (Some(wrapped), None)
            }
        };

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(self.namespace.clone()),
                labels: Some(participant_labels(name)),
                ..Default::default()
            },
            spec: Some(PodSpec {
                host_pid: (planned.options.get(HOST_PID) == Some(&true)).then_some(true),
                service_account_name: (planned.options.get(KUBELET) == Some(&true))
                    .then(|| KUBELET_ACCOUNT.to_string()),
                restart_policy: Some("Never".to_string()),
                termination_grace_period_seconds: Some(GRACE_SECONDS),
                volumes: Some(mounts.volumes),
                containers: vec![PodContainer {
                    name: name.to_string(),
                    image: Some(image.used().to_string()),
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    command,
                    args,
                    env: Some(env),
                    volume_mounts: Some(mounts.mounts),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(PodPlan { pod, files, late })
    }
}

/// Whether `file` of participant `name` is written after the pod starts, because it holds a value only the started pod
/// has.
fn is_late(name: &str, planned: &PlannedParticipant, file: &PlannedFile) -> bool {
    planned
        .later
        .get(&Place::File(file.destination.clone()).to_string())
        .is_some_and(|waits| waits.get(name) == Some(&Later::Started))
}

/// The shell script that waits until the harness has written the late files, copies each to its destination, and then
/// runs the pod's own command, which it gets as its arguments.
fn wait_script(late: &[LateFile]) -> String {
    let late_dir = Enclosure::KubernetesPod.facts().late.unwrap_or_default();
    let mut script = format!(
        "set -e; until [ -e {} ]; do sleep 1; done;",
        quoted(&format!("{}/{}", late_dir, READY_FILE))
    );
    for file in late {
        let source = quoted(&format!("{}/{}", late_dir, file.index));
        let destination = quoted(&file.destination);
        if let Some((parent, _)) = file
            .destination
            .rsplit_once('/')
            .filter(|(parent, _)| !parent.is_empty())
        {
            script.push_str(&format!(" mkdir -p {};", quoted(parent)));
        }
        script.push_str(&format!(
            " cp {} {}; chmod {:o} {};",
            source, destination, file.mode, destination
        ));
    }
    script.push_str(" exec \"$@\"");
    script
}

/// `text` as one word of a shell script.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The volumes of a pod and where its container mounts them.
#[derive(Default)]
struct Mounts {
    volumes: Vec<Volume>,
    mounts: Vec<VolumeMount>,
}

impl Mounts {
    /// Mounts `host`, a path on the node of kind `type_` unless it is `None`, at `path` in the container.
    fn add(&mut self, name: &str, host: String, type_: Option<&str>, path: &str, read_only: bool) {
        self.volumes.push(Volume {
            name: name.to_string(),
            host_path: Some(HostPathVolumeSource {
                path: host,
                type_: type_.map(str::to_string),
            }),
            ..Default::default()
        });
        self.mounts.push(VolumeMount {
            name: name.to_string(),
            mount_path: path.to_string(),
            read_only: Some(read_only),
            ..Default::default()
        });
    }
}

fn env_var(name: &str, value: String) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value),
        ..Default::default()
    }
}

fn field_env_var(name: &str, field: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: field.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn participant_labels(name: &str) -> BTreeMap<String, String> {
    [(PARTICIPANT_LABEL.to_string(), name.to_string())].into()
}

/// `path` on the node, as a path in an archive placed at `/`.
fn archive_path(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

/// A participant's pod, submitted at start.
struct CreatedPod {
    case: CasePods,
    name: String,
    pod: Pod,
    late: Vec<LateFile>,
    /// The pod port of each `http` endpoint, which start forwards.
    forwards: BTreeMap<String, u16>,
}

#[async_trait]
impl Created for CreatedPod {
    fn identities(&self) -> Identities {
        Identities::default()
    }

    /// Submits the pod, waits until its container runs, writes the late files, and forwards a local port to each `http`
    /// endpoint.
    async fn start(
        self: Box<Self>, values: &Values, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<Arc<dyn Running>, GenericError> {
        let Self {
            case,
            name,
            pod,
            late,
            forwards,
        } = *self;
        let pods = case.pods();
        request(
            cancel,
            deadline,
            format!("create pod '{}'", name),
            pods.create(&PostParams::default(), &pod),
        )
        .await?;
        let pod = wait_until_running(&pods, &name, cancel, deadline).await?;

        let status = container_status(&pod, &name);
        let identities = Identities {
            container_id: status
                .and_then(|status| status.container_id.as_deref())
                .map(|id| id.split_once("://").map_or(id, |(_, id)| id).to_string()),
            pod_uid: pod.metadata.uid.clone(),
            ip: pod.status.as_ref().and_then(|status| status.pod_ip.clone()),
        };

        if !late.is_empty() {
            let mut known = values.clone();
            known.identify(&name, &identities);
            let mut files = Archive::new();
            for file in &late {
                let contents = known
                    .fill(&name, &file.contents)
                    .map_err(|e| generic_error!("Participant '{}': {}", name, e))?;
                let path = format!("{}/{}", case.late_dir(&name), file.index);
                files.add_file(&archive_path(&path), contents.as_bytes(), file.mode)?;
            }
            let place_files = format!("place the late files of participant '{}' on the node", name);
            case.place(place_files.clone(), files, cancel, deadline).await?;
            // The command waits for this file, so it is placed once every other file is there.
            let mut ready = Archive::new();
            ready.add_file(
                &archive_path(&format!("{}/{}", case.late_dir(&name), READY_FILE)),
                b"",
                DEFAULT_FILE_MODE,
            )?;
            case.place(place_files, ready, cancel, deadline).await?;
        }

        let mut host_ports = BTreeMap::new();
        for (endpoint, port) in forwards {
            let local = forward(&case, &name, port).await?;
            host_ports.insert(endpoint, local);
        }
        Ok(Arc::new(PodRunning {
            pods,
            case,
            name,
            host_ports,
            identities,
        }))
    }
}

/// Waits until the container of pod `name` runs or has finished, and returns the pod then.
///
/// # Errors
///
/// Returns an error if the container waits for a reason no wait changes, such as an image that cannot be pulled, if it
/// does not run in time, if `cancel` fires first, or if `deadline` passes first.
async fn wait_until_running(
    pods: &Api<Pod>, name: &str, cancel: &CancellationToken, deadline: Instant,
) -> Result<Pod, GenericError> {
    let limit = (Instant::now() + RUNNING_TIME).min(deadline);
    let mut last = "the container was not created yet".to_string();
    loop {
        let pod = request(cancel, limit, format!("get pod '{}'", name), pods.get(name)).await?;
        if let Some(state) = container_status(&pod, name).and_then(|status| status.state.as_ref()) {
            if state.running.is_some() || state.terminated.is_some() {
                return Ok(pod);
            }
            if let Some(waiting) = &state.waiting {
                let reason = waiting.reason.as_deref().unwrap_or("waiting");
                let message = waiting.message.as_deref().unwrap_or_default();
                last = format!("{}: {}", reason, message);
                if FATAL_WAITING.contains(&reason) {
                    return Err(generic_error!("Pod '{}' cannot start its container: {}", name, last));
                }
            }
        }
        if Instant::now() >= limit {
            return Err(generic_error!(
                "The container of pod '{}' was not running within {:?}. Last: {}",
                name,
                RUNNING_TIME,
                last
            ));
        }
        let next = Instant::now() + RUNNING_POLL;
        if cancel.run_until_cancelled(sleep_until(next.min(limit))).await.is_none() {
            return Err(generic_error!("Cancelled while waiting for pod '{}' to run.", name));
        }
    }
}

/// The status of the container named `name` in `pod`.
fn container_status<'a>(pod: &'a Pod, name: &str) -> Option<&'a ContainerStatus> {
    pod.status
        .as_ref()?
        .container_statuses
        .iter()
        .flatten()
        .find(|status| status.name == name)
}

/// Forwards a port on the runner's loopback address to `port` of pod `name`, and returns it. The case's tasks keep the
/// listener and each connection's forwarding.
async fn forward(case: &CasePods, name: &str, port: u16) -> Result<u16, GenericError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .error_context("Failed to bind a local port to forward")?;
    let local = listener
        .local_addr()
        .error_context("Failed to read the local port to forward")?
        .port();
    let pods = case.pods();
    let connections = case.tasks.clone();
    let name = name.to_string();
    case.tasks.spawn(async move {
        loop {
            let mut connection = match listener.accept().await {
                Ok((connection, _)) => connection,
                Err(e) => {
                    warn!(pod = %name, port, error = %e, "Stopped forwarding a local port to a pod.");
                    return;
                }
            };
            let (pods, name) = (pods.clone(), name.clone());
            connections.spawn(async move {
                let mut forwarder = match pods.portforward(&name, &[port]).await {
                    Ok(forwarder) => forwarder,
                    Err(e) => {
                        warn!(pod = %name, port, error = %e, "Failed to forward a connection to a pod.");
                        return;
                    }
                };
                let Some(mut stream) = forwarder.take_stream(port) else {
                    warn!(pod = %name, port, "Failed to forward a connection to a pod: the forward has no stream.");
                    return;
                };
                if let Err(e) = tokio::io::copy_bidirectional(&mut connection, &mut stream).await {
                    warn!(pod = %name, port, error = %e, "A connection forwarded to a pod failed.");
                }
            });
        }
    });
    Ok(local)
}

/// A participant's started pod.
struct PodRunning {
    pods: Api<Pod>,
    case: CasePods,
    /// The name of the participant, its pod and its container.
    name: String,
    /// The local port forwarded to each `http` endpoint.
    host_ports: BTreeMap<String, u16>,
    identities: Identities,
}

impl fmt::Debug for PodRunning {
    /// Omits the client, which isn't `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PodRunning")
            .field("namespace", &self.case.namespace)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PodRunning {
    /// The exit code of the container if it has finished, or `None` while it runs.
    async fn terminated(&self) -> Result<Option<i64>, kube::Error> {
        let pod = self.pods.get(&self.name).await?;
        Ok(container_status(&pod, &self.name)
            .and_then(|status| status.state.as_ref())
            .and_then(|state| state.terminated.as_ref())
            .map(|terminated| i64::from(terminated.exit_code)))
    }

    /// A command that did not return an exit code: the software's fault if the pod has exited, the harness's otherwise.
    async fn failed(&self, message: String) -> HandleError {
        if self.has_exited().await {
            HandleError::Software(format!("pod '{}' has exited: {}", self.name, message))
        } else {
            HandleError::Harness(message)
        }
    }

    /// The path on the node of `path` in the pod.
    ///
    /// # Errors
    ///
    /// Returns a harness error if `path` is not under `/airlock`, which is the only place a pod's files are read.
    fn read_path(&self, path: &str) -> Result<String, HandleError> {
        self.case.shared_path(path).ok_or_else(|| {
            HandleError::Harness(format!(
                "'{}' is not readable: a pod's files are read through the node, and only under /airlock",
                path
            ))
        })
    }
}

/// The exit code a failed exec's `status` reports.
fn exit_code(status: &Status) -> Result<i64, String> {
    if status.status.as_deref() == Some("Success") {
        return Ok(0);
    }
    if status.reason.as_deref() == Some("NonZeroExitCode") {
        let code = status
            .details
            .iter()
            .flat_map(|details| details.causes.iter().flatten())
            .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
            .and_then(|cause| cause.message.as_deref())
            .and_then(|message| message.parse().ok());
        if let Some(code) = code {
            return Ok(code);
        }
    }
    Err(status
        .message
        .clone()
        .unwrap_or_else(|| "the exec ended with a status that has no exit code".to_string()))
}

#[async_trait]
impl Realized for PodRunning {
    async fn exec(
        &self, exec: Exec, stdout: &mut (dyn AsyncWrite + Send + Unpin), stderr: &mut (dyn AsyncWrite + Send + Unpin),
        deadline: Instant,
    ) -> Result<i64, HandleError> {
        let Exec { argv, env, stdin } = exec;
        // TODO: pass the environment, for instance through `env`, once a step runs a participant's command.
        if !env.is_empty() {
            return Err(HandleError::Harness(
                "an exec in a pod takes no environment".to_string(),
            ));
        }
        let params = AttachParams::default()
            .container(&self.name)
            .stdin(stdin.is_some())
            .stdout(true)
            .stderr(true)
            .max_stdout_buf_size(64 * 1024)
            .max_stderr_buf_size(64 * 1024);
        let mut process = match timeout_at(deadline, self.pods.exec(&self.name, argv, &params)).await {
            Ok(Ok(process)) => process,
            Ok(Err(e)) => {
                return Err(self
                    .failed(format!("Failed to exec in pod '{}': {}", self.name, e))
                    .await)
            }
            Err(_) => {
                return Err(HandleError::Harness(format!(
                    "the deadline passed before the exec in pod '{}' started",
                    self.name
                )))
            }
        };
        // The status is taken first and kept, because the process task fails if nothing receives it.
        let status = process.take_status();
        let (Some(status), Some(mut out), Some(mut err)) = (status, process.stdout(), process.stderr()) else {
            return Err(HandleError::Harness(
                "an exec in a pod gave no output streams".to_string(),
            ));
        };
        let input = process.stdin();
        let run = async {
            let write = async {
                if let (Some(bytes), Some(mut input)) = (stdin, input) {
                    // A command that exits before it reads everything says so in its status.
                    let _ = input.write_all(&bytes).await;
                    let _ = input.shutdown().await;
                }
            };
            let (copied_out, copied_err, ()) = tokio::join!(
                tokio::io::copy(&mut out, stdout),
                tokio::io::copy(&mut err, stderr),
                write
            );
            copied_out?;
            copied_err?;
            Ok::<_, std::io::Error>(status.await)
        };
        let status = match timeout_at(deadline, run).await {
            Err(_) => {
                process.abort();
                return Err(HandleError::Harness(format!(
                    "the deadline passed before the exec in pod '{}' exited",
                    self.name
                )));
            }
            Ok(Err(e)) => {
                return Err(HandleError::Harness(format!(
                    "Failed to pass on the output of an exec in pod '{}': {}",
                    self.name, e
                )))
            }
            Ok(Ok(status)) => status,
        };
        let _ = timeout_at(deadline, process.join()).await;
        match status.as_ref().map(exit_code) {
            Some(Ok(code)) => Ok(code),
            Some(Err(why)) => Err(self.failed(why).await),
            None => Err(self.failed("the exec ended without a status".to_string()).await),
        }
    }

    async fn has_exited(&self) -> bool {
        matches!(
            timeout_at(Instant::now() + STATUS_LIMIT, self.terminated()).await,
            Ok(Ok(Some(_)))
        )
    }

    async fn read_file(&self, path: &str, deadline: Instant) -> Result<Option<Vec<u8>>, HandleError> {
        let path = self.read_path(path)?;
        self.case
            .node
            .read_file(&path, deadline)
            .await
            .map_err(|e| HandleError::Harness(format!("{:#}", e)))
    }

    fn host_port(&self, endpoint: &str) -> Option<u16> {
        self.host_ports.get(endpoint).copied()
    }
}

/// The kind node as a participant of its own, to run the courier on the node against a pod's files there.
#[derive(Debug)]
struct NodeView<'a>(&'a PodRunning);

#[async_trait]
impl Realized for NodeView<'_> {
    async fn exec(
        &self, exec: Exec, stdout: &mut (dyn AsyncWrite + Send + Unpin), stderr: &mut (dyn AsyncWrite + Send + Unpin),
        deadline: Instant,
    ) -> Result<i64, HandleError> {
        exec_in(&self.0.case.node, exec, stdout, stderr, deadline)
            .await
            .map_err(|failure| HandleError::Harness(failure.message))
    }

    /// Whether the pod has exited, which is what ends a follow of its log file.
    async fn has_exited(&self) -> bool {
        self.0.has_exited().await
    }

    async fn read_file(&self, path: &str, deadline: Instant) -> Result<Option<Vec<u8>>, HandleError> {
        self.0
            .case
            .node
            .read_file(path, deadline)
            .await
            .map_err(|e| HandleError::Harness(format!("{:#}", e)))
    }

    fn host_port(&self, _endpoint: &str) -> Option<u16> {
        None
    }
}

#[async_trait]
impl Running for PodRunning {
    fn identities(&self) -> Identities {
        self.identities.clone()
    }

    /// Polls the container's state. A read that fails or stalls ends the wait, so the exit is never taken as running
    /// when it cannot be observed.
    async fn wait(&self, deadline: Instant) -> Result<i64, GenericError> {
        loop {
            match timeout_at((Instant::now() + STATUS_LIMIT).min(deadline), self.terminated()).await {
                Ok(Ok(Some(code))) => return Ok(code),
                Ok(Ok(None)) => {}
                Ok(Err(kube::Error::Api(status))) if status.is_not_found() => {
                    return Err(generic_error!("pod '{}' is gone", self.name));
                }
                Ok(Err(e)) => return Err(generic_error!("Failed to read the state of pod '{}': {}", self.name, e)),
                Err(_) => return Err(generic_error!("Reading the state of pod '{}' timed out.", self.name)),
            }
            sleep_until((Instant::now() + EXIT_POLL).min(deadline)).await;
            if Instant::now() >= deadline {
                return Err(generic_error!("the deadline passed"));
            }
        }
    }

    async fn exit_now(&self, deadline: Instant) -> Result<Option<i64>, GenericError> {
        match timeout_at(deadline, self.terminated()).await {
            Ok(Ok(code)) => Ok(code),
            Ok(Err(e)) => Err(generic_error!("Failed to read the state of pod '{}': {}", self.name, e)),
            Err(_) => Err(generic_error!(
                "The deadline passed while reading the state of pod '{}'.",
                self.name
            )),
        }
    }

    fn check_capture(&self, participant: &str, process: &ProcessSpec) -> Result<(), GenericError> {
        match process.output() {
            OutputChannel::Container => Ok(()),
            // The case supplies the courier, so only the log directory can be missing.
            OutputChannel::File { .. } => Enclosure::KubernetesPod
                .facts()
                .log_dir(participant)
                .map(|_| ())
                .ok_or_else(|| {
                    generic_error!(
                        "Participant '{}' is a {}, which has no courier or log directory.",
                        participant,
                        Enclosure::KubernetesPod
                    )
                }),
        }
    }

    async fn capture(
        &self, participant: &str, process: &ProcessSpec, sink: Sink, limit: Duration, finish: CancellationToken,
    ) {
        match process.output() {
            OutputChannel::Container => self.capture_log(sink, finish).await,
            OutputChannel::File { file } => {
                let facts = Enclosure::KubernetesPod.facts();
                let Some(log_dir) = facts.log_dir(participant) else {
                    sink.failed(format!("a {} has no log directory", Enclosure::KubernetesPod));
                    return;
                };
                let path = self.case.node_path(&format!("{}{}{}", log_dir, facts.separator, file));
                sink.follow(&NodeView(self), &self.case.courier(), &path, limit, finish)
                    .await;
            }
        }
    }

    async fn capture_log(&self, sink: Sink, finish: CancellationToken) {
        // Opening the stream is a request to the Kubernetes API that can stall, and the capture task is cooperative, so
        // only `finish` can end the wait.
        let Some(logs) = finish.run_until_cancelled(self.logs()).await else {
            sink.failed("Stopped opening the pod log stream during cleanup.".to_string());
            return;
        };
        match logs {
            Ok(logs) => sink.capture(logs, finish).await,
            Err(e) => sink.failed(format!("{:#}", e)),
        }
    }
}

impl PodRunning {
    /// Follows the container's log from its start until it ends.
    async fn logs(&self) -> Result<OutputStream, GenericError> {
        let params = LogParams {
            container: Some(self.name.clone()),
            follow: true,
            ..Default::default()
        };
        let reader = self
            .pods
            .log_stream(&self.name, &params)
            .await
            .with_error_context(|| format!("Failed to read the log of pod '{}'", self.name))?;
        let name = self.name.clone();
        let chunks = futures::stream::try_unfold(Box::pin(reader), move |mut reader| {
            let name = name.clone();
            async move {
                let mut chunk = vec![0; 8192];
                let read = reader
                    .read(&mut chunk)
                    .await
                    .map_err(|e| generic_error!("Failed to read the log of pod '{}': {}", name, e))?;
                chunk.truncate(read);
                Ok((read > 0).then(|| (Output::Stdout(Bytes::from(chunk)), reader)))
            }
        });
        Ok(Box::pin(chunks))
    }
}

/// Runs `future`, a request to the cluster or the node, and says what it was for when it fails, is cancelled or
/// outlasts `deadline`.
async fn request<T, E>(
    cancel: &CancellationToken, deadline: Instant, what: String, future: impl Future<Output = Result<T, E>>,
) -> Result<T, GenericError>
where
    E: fmt::Display,
{
    match timeout_at(deadline, cancel.run_until_cancelled(future)).await {
        Err(_) => Err(generic_error!("The deadline passed while trying to {}.", what)),
        Ok(None) => Err(generic_error!("Cancelled while trying to {}.", what)),
        Ok(Some(Err(e))) => Err(generic_error!("Failed to {}: {}", what, e)),
        Ok(Some(Ok(value))) => Ok(value),
    }
}
