//! Backends: what runs a participant, and what planning needs to know about them first.
//!
//! A backend is the part of the harness that runs participants in one enclosure: a Linux container, a Windows
//! container, a Kubernetes pod or a host process group. The plan says what a participant runs, and the backend makes
//! it run there.
//!
//! Planning happens before any backend runs, so the fixed answers of every backend are kept here as data, one
//! [`Facts`] row per enclosure, such as where a participant's own directory is and which protocols it can listen on.
//! At run time, a backend prepares the scope of a case, creates each participant in it from its plan, and starts it.
//! A started participant is [`Running`]: it reports its exit and output to the lifecycle, and, as [`Realized`], runs
//! commands in the participant and reads its files for steps.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use saluki_error::{generic_error, GenericError};
use tokio::io::AsyncWrite;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::handle::HandleError;
use super::ledger::ResourceLedger;
use super::observe::{ProcessExit, Sink};
use super::participant::Enclosure;
use super::plan::PlannedParticipant;
use super::reference::Values;
use super::spec::{ProcessSpec, Protocol};
use super::tasks::CaseTasks;

/// The environment variable that names the participant's [`Facts::exit_dir`].
pub(crate) const EXIT_DIR_ENV: &str = "PANORAMIC_EXIT_DIR";
/// The environment variable that names the participant's [`Facts::log_dir`].
pub(crate) const LOG_DIR_ENV: &str = "PANORAMIC_LOG_DIR";

/// The realization option that puts a Linux container or a pod in the host's PID namespace.
pub(crate) const HOST_PID: &str = "host_pid";
/// The realization option that puts a Linux container in the host's cgroup namespace.
pub(crate) const HOST_CGROUP_NAMESPACE: &str = "host_cgroup_namespace";
/// The realization option that mounts the container runtime's socket of the kind node in a pod.
pub(crate) const CRI_SOCKET: &str = "cri_socket";
/// The realization option that lets a pod's service account read the kubelet API of the kind node.
pub(crate) const KUBELET: &str = "kubelet";

/// What planning knows of one realization backend.
pub(crate) struct Facts {
    /// What the backend makes for each case.
    pub(crate) per_case: &'static str,
    /// The directory under which each participant gets its own directories, or `None` where it is not fixed at plan.
    pub(crate) root: Option<&'static str>,
    /// Whether the backend gives each participant a supervisor's exit and log directories. Without them, processes are
    /// observed through the realization alone.
    pub(crate) supervised: bool,
    /// The separator of file paths in the participant.
    pub(crate) separator: char,
    /// Where the backend places the courier in the participant.
    pub(crate) courier: Option<&'static str>,
    /// Where the backend places files that only exist once the participant has started, or `None` where it writes
    /// every file before the participant starts.
    pub(crate) late: Option<&'static str>,
    /// The protocols a participant can listen on.
    pub(crate) listens: &'static [Protocol],
    /// Whether HTTP and gRPC endpoints are published on the runner's loopback address, at a host port the backend
    /// chooses at start.
    pub(crate) publishes: bool,
    /// Whether a participant may mount host paths.
    pub(crate) mounts: bool,
    /// Whether a command run in the participant sees the end of its input. It does not in a Windows container, where
    /// Docker gets no end of input from the exec client.
    pub(crate) exec_input_ends: bool,
    /// The realization options a case may set.
    pub(crate) options: &'static [&'static str],
}

impl Facts {
    /// The participant's own directory, which `{{ <participant>.dir }}` names.
    pub(crate) fn dir(&self, participant: &str) -> Option<String> {
        self.root.map(|root| self.under(root, participant, "shared"))
    }

    /// The directory a supervisor in the participant writes exit records to, as [`EXIT_DIR_ENV`].
    pub(crate) fn exit_dir(&self, participant: &str) -> Option<String> {
        self.supervised_dir(participant, "exit")
    }

    /// The directory a supervisor in the participant writes log files to, as [`LOG_DIR_ENV`].
    pub(crate) fn log_dir(&self, participant: &str) -> Option<String> {
        self.supervised_dir(participant, "log")
    }

    /// Paths the harness reserves in the participant: the courier, the directory of late files, and the participant's
    /// exit and log directories.
    pub(crate) fn reserved_paths(&self, participant: &str) -> Vec<String> {
        let courier = self.courier.map(str::to_string);
        let late = self.late.map(str::to_string);
        [courier, late, self.exit_dir(participant), self.log_dir(participant)]
            .into_iter()
            .flatten()
            .collect()
    }

    fn supervised_dir(&self, participant: &str, dir: &str) -> Option<String> {
        self.root
            .filter(|_| self.supervised)
            .map(|root| self.under(root, participant, dir))
    }

    fn under(&self, root: &str, participant: &str, dir: &str) -> String {
        let separator = self.separator;
        format!("{root}{separator}{participant}{separator}{dir}")
    }
}

const ALL_BUT_PIPES: &[Protocol] = &[
    Protocol::Tcp,
    Protocol::Udp,
    Protocol::Http,
    Protocol::Grpc,
    Protocol::UnixDatagram,
    Protocol::UnixStream,
];

const LINUX_CONTAINER: Facts = Facts {
    per_case: "one bridge network and one volume",
    root: Some("/airlock"),
    supervised: true,
    separator: '/',
    courier: Some("/panoramic/courier"),
    late: None,
    listens: ALL_BUT_PIPES,
    publishes: true,
    mounts: true,
    exec_input_ends: true,
    // `host_pid`: the container shares the host's PID namespace. `host_cgroup_namespace`: it shares the host's cgroup
    // namespace.
    options: &[HOST_PID, HOST_CGROUP_NAMESPACE],
};

const WINDOWS_CONTAINER: Facts = Facts {
    per_case: "one nat network",
    root: Some("C:\\airlock"),
    supervised: true,
    separator: '\\',
    courier: Some("C:\\panoramic\\courier.exe"),
    late: None,
    listens: &[
        Protocol::Tcp,
        Protocol::Udp,
        Protocol::Http,
        Protocol::Grpc,
        Protocol::NamedPipe,
    ],
    publishes: false,
    mounts: false,
    exec_input_ends: false,
    options: &[],
};

const KUBERNETES_POD: Facts = Facts {
    per_case: "one namespace and one directory on the kind node",
    root: Some("/airlock"),
    supervised: true,
    separator: '/',
    courier: Some("/panoramic/courier"),
    late: Some("/panoramic/late"),
    listens: ALL_BUT_PIPES,
    publishes: false,
    mounts: true,
    exec_input_ends: true,
    // `host_pid`: the pod shares the node's PID namespace. `cri_socket`: the pod mounts the node's containerd socket.
    // `kubelet`: the pod runs as a service account that may read the node's kubelet API.
    options: &[HOST_PID, CRI_SOCKET, KUBELET],
};

// One path for every case and runner host, so it is short enough for a macOS socket address, which the system's
// temporary directory is not. Host process cases run one at a time, and their ports are fixed anyway.
const HOST_PROCESS_GROUP: Facts = Facts {
    per_case: "the state directory /tmp/panoramic",
    root: Some("/tmp/panoramic"),
    supervised: false,
    separator: '/',
    courier: None,
    late: None,
    listens: ALL_BUT_PIPES,
    publishes: false,
    mounts: false,
    exec_input_ends: true,
    options: &[],
};

impl Enclosure {
    /// What planning knows of this enclosure's backend.
    pub(crate) fn facts(self) -> &'static Facts {
        match self {
            Self::LinuxContainer => &LINUX_CONTAINER,
            Self::WindowsContainer => &WINDOWS_CONTAINER,
            Self::KubernetesPod => &KUBERNETES_POD,
            Self::HostProcessGroup => &HOST_PROCESS_GROUP,
        }
    }
}

/// A command to run in a started participant.
#[derive(Clone, Debug, Default)]
pub(crate) struct Exec {
    pub(crate) argv: Vec<String>,
    /// Environment for this command, over the participant's.
    pub(crate) env: BTreeMap<String, String>,
    /// Written to the command's standard input, which is then closed. Without it, the command has no standard input.
    pub(crate) stdin: Option<Bytes>,
}

/// A started participant, as its backend realized it: what steps, readiness gates and output capture act on. Each
/// failure says whether it is the software's or the harness's.
#[async_trait]
pub(crate) trait Realized: fmt::Debug + Send + Sync {
    /// Runs `exec` in the participant, writes its output to `stdout` and `stderr` as it arrives, and returns its exit
    /// code. A nonzero exit code is a result, not an error.
    ///
    /// # Errors
    ///
    /// Returns a software error if the participant has exited, and a harness error if the command cannot be run
    /// otherwise, its output cannot be passed on, or `deadline` passes before it exits.
    async fn exec(
        &self, exec: Exec, stdout: &mut (dyn AsyncWrite + Send + Unpin), stderr: &mut (dyn AsyncWrite + Send + Unpin),
        deadline: Instant,
    ) -> Result<i64, HandleError>;

    /// Whether the participant has exited, as far as the backend can tell. It is not known to have exited when the
    /// backend cannot tell.
    async fn has_exited(&self) -> bool;

    /// Reads the file at `path` in the participant, or returns `None` if nothing is there.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the file cannot be read, or `deadline` passes first.
    async fn read_file(&self, path: &str, deadline: Instant) -> Result<Option<Vec<u8>>, HandleError>;

    /// The exit of `process` as the realization reports it apart from the participant's own, as each program of a host
    /// process group has. `None` where the realization reports one exit, the participant's, which is its only
    /// process's.
    fn reported_exit(&self, _process: &str) -> Option<ProcessExit> {
        None
    }

    /// The runner host's port for the participant's published `endpoint`, or `None` if it has none. A participant that
    /// exited before its ports were read back has none.
    fn host_port(&self, endpoint: &str) -> Option<u16>;
}

/// The runtime identities a backend learns of a participant: some when it creates the participant, the rest when it
/// starts it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Identities {
    pub(crate) container_id: Option<String>,
    pub(crate) pod_uid: Option<String>,
    /// The participant's address on the case's network.
    pub(crate) ip: Option<String>,
}

/// What a backend gets to prepare the scope of one case.
pub(crate) struct Preparing<'a> {
    /// Takes every resource of the scope before it is made.
    pub(crate) ledger: &'a ResourceLedger,
    /// Where background work of the case runs. The runner stops it before reclaim.
    pub(crate) tasks: &'a CaseTasks,
    /// Fires when the runner stops the case.
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) deadline: Instant,
}

/// Realizes the participants of one enclosure.
#[async_trait]
pub(crate) trait Backend: fmt::Debug + Send + Sync {
    /// Prepares the scope of one case, with each resource entered in the ledger before it is made.
    ///
    /// # Errors
    ///
    /// Returns an error if the scope cannot be made, if the case is cancelled first, or if the deadline passes first.
    async fn prepare(&self, cx: &Preparing<'_>) -> Result<Box<dyn Scope>, GenericError>;
}

/// The part of one case that a backend realizes.
#[async_trait]
pub(crate) trait Scope: Send + Sync {
    /// Creates participant `name` from its plan, with everything it needs placed. Nothing runs yet.
    ///
    /// `values` fills every reference left in the plan, so each participant whose container ID `name` refers to must be
    /// created, and recorded in `values`, first.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan holds something this backend cannot honor or a reference with no value yet, if the
    /// participant or its files cannot be made, for example because the image is missing, if `cancel` fires first, or
    /// if `deadline` passes first.
    async fn create(
        &self, name: &str, planned: &PlannedParticipant, values: &Values, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<Box<dyn Created>, GenericError>;
}

/// A participant that exists and has not started.
#[async_trait]
pub(crate) trait Created: Send + Sync {
    /// What is known of the participant now that it exists.
    fn identities(&self) -> Identities;

    /// Starts the participant, and reads back what only a started participant has.
    ///
    /// # Errors
    ///
    /// Returns an error if the participant cannot be started, if `cancel` fires first, or if `deadline` passes first.
    async fn start(
        self: Box<Self>, values: &Values, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<Arc<dyn Running>, GenericError>;
}

/// A started participant, with what the lifecycle observes of it.
#[async_trait]
pub(crate) trait Running: Realized {
    /// What is known of the participant now that it has started.
    fn identities(&self) -> Identities;

    /// Waits until the participant exits, and returns its exit code, or why none can be had.
    async fn wait(&self, deadline: Instant) -> Result<i64, GenericError>;

    /// The participant's exit code if it has exited, or `None` while it runs, or why that cannot be told.
    async fn exit_now(&self, deadline: Instant) -> Result<Option<i64>, GenericError>;

    /// Whether the output of `process` of `participant` can be captured, and if not, why.
    ///
    /// # Errors
    ///
    /// When the participant's enclosure has no way to capture the output of `process`.
    fn check_capture(&self, participant: &str, process: &ProcessSpec) -> Result<(), GenericError>;

    /// Captures the output of `process` of `participant` into `sink`, for `limit` or until `finish` fires.
    async fn capture(
        &self, participant: &str, process: &ProcessSpec, sink: Sink, limit: Duration, finish: CancellationToken,
    );

    /// Captures the participant's own log into `sink`, until `finish` fires.
    async fn capture_log(&self, sink: Sink, finish: CancellationToken);
}

/// The backends of a run, by the enclosure each realizes.
#[derive(Clone, Debug, Default)]
pub(crate) struct Backends(BTreeMap<Enclosure, Arc<dyn Backend>>);

impl Backends {
    pub(crate) fn insert(&mut self, enclosure: Enclosure, backend: Arc<dyn Backend>) {
        self.0.insert(enclosure, backend);
    }

    /// The backend of `enclosure`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the enclosure if the run has no backend for it.
    pub(crate) fn get(&self, enclosure: Enclosure) -> Result<&Arc<dyn Backend>, GenericError> {
        self.0.get(&enclosure).ok_or_else(|| {
            generic_error!(
                "The run has no backend for a {}: the checks before the first case did not provide one.",
                enclosure
            )
        })
    }
}
