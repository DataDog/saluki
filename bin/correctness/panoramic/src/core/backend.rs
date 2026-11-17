//! What planning needs to know about each backend.
//!
//! A backend is the part of the harness that runs participants in one enclosure: a Linux container, a Windows
//! container, a Kubernetes pod or a host process group. The plan says what a participant runs, and the backend makes
//! it run there.
//!
//! Planning happens before any backend runs, so the fixed answers of every backend are kept here as data, one
//! [`Facts`] row per enclosure, such as where a participant's own directory is and which protocols it can listen on.

use super::participant::Enclosure;
use super::spec::Protocol;

/// What planning knows of one realization backend.
pub(crate) struct Facts {
    /// What the backend makes for each case.
    pub(crate) per_case: &'static str,
    /// The directory under which each participant gets its own directories, or `None` where it is not fixed at plan.
    root: Option<&'static str>,
    /// The separator of file paths in the participant.
    pub(crate) separator: char,
    /// Where the backend places the courier in the participant.
    courier: Option<&'static str>,
    /// The protocols a participant can listen on.
    pub(crate) listens: &'static [Protocol],
    /// Whether HTTP and gRPC endpoints are published on the runner's loopback address, at a host port the backend
    /// chooses at start.
    pub(crate) publishes: bool,
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

    /// Paths the harness reserves in the participant: the courier, and the participant's exit and log directories.
    pub(crate) fn reserved_paths(&self, participant: &str) -> Vec<String> {
        let mut paths: Vec<String> = self.courier.map(str::to_string).into_iter().collect();
        if let Some(root) = self.root {
            paths.extend(["exit", "log"].map(|dir| self.under(root, participant, dir)));
        }
        paths
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
    options: &[],
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
    options: &[],
};

const KUBERNETES_POD: Facts = Facts {
    per_case: "one namespace and one directory on the kind node",
    root: Some("/airlock"),
    separator: '/',
    courier: Some("/panoramic/courier"),
    listens: ALL_BUT_PIPES,
    publishes: false,
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
