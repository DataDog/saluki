//! Backends: what runs a participant, and what planning needs to know about them first.
//!
//! A backend is the part of the harness that runs participants in one enclosure: a Linux container, a Windows
//! container, a Kubernetes pod or a host process group. The plan says what a participant runs, and the backend makes
//! it run there.
//!
//! Planning happens before any backend runs, so the fixed answers of every backend are kept here as data, one
//! [`Facts`] row per enclosure, such as where a participant's own directory is and which protocols it can listen on.
//! Once a participant has started, its backend hands steps a [`Realized`], which runs commands in it and reads its
//! files.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncWrite;
use tokio::time::Instant;

use super::handle::HandleError;
use super::participant::Enclosure;
use super::spec::Protocol;

/// The environment variable that names the participant's [`Facts::exit_dir`].
pub(crate) const EXIT_DIR_ENV: &str = "PANORAMIC_EXIT_DIR";
/// The environment variable that names the participant's [`Facts::log_dir`].
pub(crate) const LOG_DIR_ENV: &str = "PANORAMIC_LOG_DIR";

/// The realization option that puts a Linux container in the host's PID namespace.
pub(crate) const HOST_PID: &str = "host_pid";

/// What planning knows of one realization backend.
pub(crate) struct Facts {
    /// What the backend makes for each case.
    pub(crate) per_case: &'static str,
    /// The directory under which each participant gets its own directories, or `None` where it is not fixed at plan.
    pub(crate) root: Option<&'static str>,
    /// The separator of file paths in the participant.
    pub(crate) separator: char,
    /// Where the backend places the courier in the participant.
    pub(crate) courier: Option<&'static str>,
    /// The protocols a participant can listen on.
    pub(crate) listens: &'static [Protocol],
    /// Whether HTTP and gRPC endpoints are published on the runner's loopback address, at a host port the backend
    /// chooses at start.
    pub(crate) publishes: bool,
    /// Whether a participant may mount host paths.
    pub(crate) mounts: bool,
    /// The realization options a case may set.
    // TODO: add each option with the case that first needs it: `host_cgroup_namespace` on a Linux container, and
    // `host_pid` and `cri_socket` on a pod.
    pub(crate) options: &'static [&'static str],
}

impl Facts {
    /// The participant's own directory, which `{{ <participant>.dir }}` names.
    pub(crate) fn dir(&self, participant: &str) -> Option<String> {
        self.root.map(|root| self.under(root, participant, "shared"))
    }

    /// The directory a supervisor in the participant writes exit records to, as [`EXIT_DIR_ENV`].
    pub(crate) fn exit_dir(&self, participant: &str) -> Option<String> {
        self.root.map(|root| self.under(root, participant, "exit"))
    }

    /// The directory a supervisor in the participant writes log files to, as [`LOG_DIR_ENV`].
    pub(crate) fn log_dir(&self, participant: &str) -> Option<String> {
        self.root.map(|root| self.under(root, participant, "log"))
    }

    /// Paths the harness reserves in the participant: the courier, and the participant's exit and log directories.
    pub(crate) fn reserved_paths(&self, participant: &str) -> Vec<String> {
        let courier = self.courier.map(str::to_string);
        [courier, self.exit_dir(participant), self.log_dir(participant)]
            .into_iter()
            .flatten()
            .collect()
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
    separator: '/',
    courier: Some("/panoramic/courier"),
    listens: ALL_BUT_PIPES,
    publishes: true,
    mounts: true,
    // `host_pid`: the container shares the host's PID namespace.
    options: &[HOST_PID],
};

const WINDOWS_CONTAINER: Facts = Facts {
    per_case: "one nat network",
    root: Some("C:\\airlock"),
    separator: '\\',
    courier: Some("C:\\panoramic\\courier.exe"),
    listens: &[
        Protocol::Tcp,
        Protocol::Udp,
        Protocol::Http,
        Protocol::Grpc,
        Protocol::NamedPipe,
    ],
    publishes: false,
    mounts: false,
    options: &[],
};

const KUBERNETES_POD: Facts = Facts {
    per_case: "one namespace and one directory on the kind node",
    root: Some("/airlock"),
    separator: '/',
    courier: Some("/panoramic/courier"),
    listens: ALL_BUT_PIPES,
    publishes: false,
    mounts: true,
    options: &[],
};

// TODO: fix the state directory at plan with the host process backend, so `{{ self.dir }}` has a value.
const HOST_PROCESS_GROUP: Facts = Facts {
    per_case: "one state directory under /tmp",
    root: None,
    separator: '/',
    courier: None,
    listens: ALL_BUT_PIPES,
    publishes: false,
    mounts: false,
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

    /// The runner host's port for the participant's published `endpoint`, or `None` if it has none. A participant that
    /// exited before its ports were read back has none.
    fn host_port(&self, endpoint: &str) -> Option<u16>;
}
