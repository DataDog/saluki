//! Planning facts for each realization kind: artifacts, shared roots and late facts.
//!
//! The static backend list describes what each kind needs for planning.
//! [`RunBackends`] checks and uses the runner host.

mod container;
pub(crate) mod courier;
pub(crate) mod docker;
#[cfg(unix)]
pub(crate) mod host_process;
pub(crate) mod http;
pub(crate) mod linux_container;
pub(crate) mod pod;
mod run;
pub(crate) mod runtime;
pub(crate) mod windows_container;
pub use run::RunBackends;
pub(crate) use runtime::{ParticipantRequest, RuntimeParticipant, ScopeRuntime};

use crate::{
    artifact::{ArtifactKind, ArtifactRole},
    layout::{Os, RealizationKind},
    plan::ScopeLayout,
};

/// Image that creates each participant's directories on the shared volume before Linux containers start;
/// it must provide `sh`, `mkdir` and `chmod`.
pub const FIXUP_IMAGE: ArtifactRole = ArtifactRole {
    name: "fixup-image",
    kind: ArtifactKind::Image { os: Os::Linux },
    local_default: airlock::driver::DEFAULT_ALPINE_IMAGE,
};

/// Static `panoramic-courier` executable (`bin/correctness/courier`) for Linux containers.
///
/// On aarch64 and x86_64, the default is the build for panoramic's own architecture. Verification fails
/// if the Docker daemon runs another architecture; bind a matching build in a manifest.
pub(crate) const COURIER_LINUX_BINARY: ArtifactRole = ArtifactRole {
    name: "courier-linux-binary",
    kind: ArtifactKind::ContainerBinary { os: Os::Linux },
    local_default: if cfg!(target_arch = "aarch64") {
        "target/aarch64-unknown-linux-musl/release/panoramic-courier"
    } else if cfg!(target_arch = "x86_64") {
        "target/x86_64-unknown-linux-musl/release/panoramic-courier"
    } else {
        ""
    },
};

/// Native Windows courier executable, verified against the container daemon's architecture.
pub(crate) const COURIER_WINDOWS_BINARY: ArtifactRole = ArtifactRole {
    name: "courier-windows-binary",
    kind: ArtifactKind::ContainerBinary { os: Os::Windows },
    local_default: "target/release/panoramic-courier.exe",
};

/// A value known once a participant exists or runs.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LateFact {
    /// Runtime container identifier, known after create for containers or after start for pods.
    ContainerId,
    /// Network address, known after start for containers and pods; loopback for host process groups.
    Ip,
    /// Kubernetes pod identifier, known after start.
    PodUid,
}

impl LateFact {
    /// Canonical placeholder for this participant's own late fact in its rendered environment.
    /// The participant type's init mechanism replaces it after start, before the product reads it.
    pub fn substituted_reference(self) -> &'static str {
        match self {
            Self::ContainerId => "{{ self.late.container_id }}",
            Self::Ip => "{{ self.late.ip }}",
            Self::PodUid => "{{ self.late.pod_uid }}",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ContainerId => "container_id",
            Self::Ip => "ip",
            Self::PodUid => "pod_uid",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "container_id" => Some(Self::ContainerId),
            "ip" => Some(Self::Ip),
            "pod_uid" => Some(Self::PodUid),
            _ => None,
        }
    }
}

/// When a backend can supply a late fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LateFactTiming {
    /// The same value for every participant of this kind, known at plan time.
    Fixed(&'static str),
    /// Known once the participant is created, before it starts.
    AfterCreate,
    /// Known once the participant has started.
    AfterStart,
}

/// A boolean option accepted by a realization backend.
pub(crate) struct RealizationOption {
    pub(crate) name: &'static str,
}

/// Planning requirements and facts for one realization kind.
pub(crate) trait Backend: Send + Sync {
    /// The realization kind this backend creates.
    fn kind(&self) -> RealizationKind;

    /// Artifact roles every participant of this realization kind needs, whatever its type.
    fn artifacts(&self) -> &'static [ArtifactRole];

    /// Boolean options accepted by this realization kind.
    fn options(&self) -> &'static [RealizationOption] {
        &[]
    }

    /// Root of participant state; Windows containers each keep their own filesystem.
    fn shared_root(&self, os: Os, scope: &ScopeLayout) -> String;

    /// Late facts and their availability, sorted by fact.
    fn late_facts(&self) -> &'static [(LateFact, LateFactTiming)];

    /// Whether this backend writes template files after start, once their late facts are known.
    fn writes_late_files(&self) -> bool {
        false
    }
}

pub(crate) const BACKENDS: &[&dyn Backend] = &[
    &LinuxContainers,
    &WindowsContainers,
    &HostProcessGroups,
    &KubernetesPods,
];

pub(crate) fn shared_root(kind: RealizationKind, os: Os, scope: &ScopeLayout) -> String {
    backend(kind).shared_root(os, scope)
}

/// Returns the artifact roles required by a realization's backend.
pub fn artifacts(kind: RealizationKind) -> &'static [ArtifactRole] {
    backend(kind).artifacts()
}

pub(crate) fn late_facts(kind: RealizationKind) -> &'static [(LateFact, LateFactTiming)] {
    backend(kind).late_facts()
}

pub(crate) fn writes_late_files(kind: RealizationKind) -> bool {
    backend(kind).writes_late_files()
}

pub(crate) fn backend(kind: RealizationKind) -> &'static dyn Backend {
    BACKENDS
        .iter()
        .copied()
        .find(|backend| backend.kind() == kind)
        .expect("every realization kind has a backend")
}

pub(crate) struct LinuxContainers;
pub(crate) struct WindowsContainers;
pub(crate) struct HostProcessGroups;
pub(crate) struct KubernetesPods;

impl Backend for LinuxContainers {
    fn kind(&self) -> RealizationKind {
        RealizationKind::LinuxContainer
    }

    fn artifacts(&self) -> &'static [ArtifactRole] {
        &[FIXUP_IMAGE, COURIER_LINUX_BINARY]
    }

    fn options(&self) -> &'static [RealizationOption] {
        &[RealizationOption {
            name: linux_container::HOST_CGROUP_NAMESPACE,
        }]
    }

    fn shared_root(&self, _os: Os, _scope: &ScopeLayout) -> String {
        airlock::group::SHARED_ROOT.into()
    }

    fn late_facts(&self) -> &'static [(LateFact, LateFactTiming)] {
        &[
            (LateFact::ContainerId, LateFactTiming::AfterCreate),
            (LateFact::Ip, LateFactTiming::AfterStart),
        ]
    }
}

impl Backend for WindowsContainers {
    fn kind(&self) -> RealizationKind {
        RealizationKind::WindowsContainer
    }

    fn artifacts(&self) -> &'static [ArtifactRole] {
        &[COURIER_WINDOWS_BINARY]
    }

    fn shared_root(&self, _os: Os, _scope: &ScopeLayout) -> String {
        windows_container::SHARED_ROOT.into()
    }

    fn late_facts(&self) -> &'static [(LateFact, LateFactTiming)] {
        &[
            (LateFact::ContainerId, LateFactTiming::AfterCreate),
            (LateFact::Ip, LateFactTiming::AfterStart),
        ]
    }
}

impl Backend for HostProcessGroups {
    fn kind(&self) -> RealizationKind {
        RealizationKind::HostProcessGroup
    }

    fn artifacts(&self) -> &'static [ArtifactRole] {
        &[]
    }

    fn shared_root(&self, os: Os, scope: &ScopeLayout) -> String {
        scope.state_dir(os)
    }

    fn late_facts(&self) -> &'static [(LateFact, LateFactTiming)] {
        &[(LateFact::Ip, LateFactTiming::Fixed("127.0.0.1"))]
    }
}

impl Backend for KubernetesPods {
    fn kind(&self) -> RealizationKind {
        RealizationKind::KubernetesPod
    }

    fn artifacts(&self) -> &'static [ArtifactRole] {
        &[COURIER_LINUX_BINARY]
    }

    fn options(&self) -> &'static [RealizationOption] {
        &[
            RealizationOption { name: "host_pid" },
            RealizationOption { name: "cri_socket" },
        ]
    }

    fn shared_root(&self, _os: Os, _scope: &ScopeLayout) -> String {
        "/airlock".into()
    }

    fn late_facts(&self) -> &'static [(LateFact, LateFactTiming)] {
        &[
            (LateFact::ContainerId, LateFactTiming::AfterStart),
            (LateFact::Ip, LateFactTiming::AfterStart),
            (LateFact::PodUid, LateFactTiming::AfterStart),
        ]
    }

    fn writes_late_files(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_declare_their_os_courier_and_only_linux_needs_a_fixup_image() {
        assert_eq!(
            FIXUP_IMAGE,
            ArtifactRole {
                name: "fixup-image",
                kind: ArtifactKind::Image { os: Os::Linux },
                local_default: airlock::driver::DEFAULT_ALPINE_IMAGE,
            }
        );
        assert_eq!(COURIER_LINUX_BINARY.name, "courier-linux-binary");
        assert_eq!(
            COURIER_LINUX_BINARY.kind,
            ArtifactKind::ContainerBinary { os: Os::Linux }
        );
        assert_eq!(
            COURIER_LINUX_BINARY.local_default,
            match std::env::consts::ARCH {
                "aarch64" => "target/aarch64-unknown-linux-musl/release/panoramic-courier",
                "x86_64" => "target/x86_64-unknown-linux-musl/release/panoramic-courier",
                _ => "",
            }
        );
        assert_eq!(
            COURIER_WINDOWS_BINARY,
            ArtifactRole {
                name: "courier-windows-binary",
                kind: ArtifactKind::ContainerBinary { os: Os::Windows },
                local_default: "target/release/panoramic-courier.exe",
            }
        );
        for backend in BACKENDS {
            let expected: &[ArtifactRole] = match backend.kind() {
                RealizationKind::LinuxContainer => &[FIXUP_IMAGE, COURIER_LINUX_BINARY],
                RealizationKind::WindowsContainer => &[COURIER_WINDOWS_BINARY],
                RealizationKind::KubernetesPod => &[COURIER_LINUX_BINARY],
                _ => &[],
            };
            assert_eq!(artifacts(backend.kind()), expected, "{:?}", backend.kind());
        }
    }

    #[test]
    fn every_backend_reports_its_shared_root() {
        let scope = ScopeLayout {
            run: "testrun0".into(),
            index: 3,
        };
        for (kind, os, expected) in [
            (RealizationKind::LinuxContainer, Os::Linux, "/airlock"),
            (RealizationKind::WindowsContainer, Os::Windows, r"C:\airlock"),
            (RealizationKind::KubernetesPod, Os::Linux, "/airlock"),
            (
                RealizationKind::HostProcessGroup,
                Os::Linux,
                "/tmp/panoramic-testrun0/0003",
            ),
            (
                RealizationKind::HostProcessGroup,
                Os::MacOs,
                "/tmp/panoramic-testrun0/0003",
            ),
            (
                RealizationKind::HostProcessGroup,
                Os::Windows,
                r"C:\Windows\Temp\panoramic-testrun0\0003",
            ),
        ] {
            assert_eq!(shared_root(kind, os, &scope), expected);
        }
    }

    #[test]
    fn each_realization_kind_has_one_backend_declaring_its_late_facts() {
        assert_eq!(BACKENDS.len(), 4);
        for (kind, expected) in [
            (
                RealizationKind::LinuxContainer,
                &[
                    (LateFact::ContainerId, LateFactTiming::AfterCreate),
                    (LateFact::Ip, LateFactTiming::AfterStart),
                ][..],
            ),
            (
                RealizationKind::WindowsContainer,
                &[
                    (LateFact::ContainerId, LateFactTiming::AfterCreate),
                    (LateFact::Ip, LateFactTiming::AfterStart),
                ][..],
            ),
            (
                RealizationKind::HostProcessGroup,
                &[(LateFact::Ip, LateFactTiming::Fixed("127.0.0.1"))][..],
            ),
            (
                RealizationKind::KubernetesPod,
                &[
                    (LateFact::ContainerId, LateFactTiming::AfterStart),
                    (LateFact::Ip, LateFactTiming::AfterStart),
                    (LateFact::PodUid, LateFactTiming::AfterStart),
                ][..],
            ),
        ] {
            match kind {
                RealizationKind::HostProcessGroup
                | RealizationKind::LinuxContainer
                | RealizationKind::WindowsContainer
                | RealizationKind::KubernetesPod => {}
            }
            assert_eq!(BACKENDS.iter().filter(|backend| backend.kind() == kind).count(), 1);
            let facts = late_facts(kind);
            assert!(facts.windows(2).all(|pair| pair[0].0 < pair[1].0));
            assert_eq!(facts, expected);
        }
    }

    #[test]
    fn only_kubernetes_pods_write_template_files_after_start() {
        for backend in BACKENDS {
            assert_eq!(
                writes_late_files(backend.kind()),
                backend.kind() == RealizationKind::KubernetesPod
            );
        }
    }

    #[test]
    fn host_process_groups_declare_a_fixed_loopback_ip() {
        assert_eq!(
            HostProcessGroups.late_facts(),
            &[(LateFact::Ip, LateFactTiming::Fixed("127.0.0.1"))]
        );
    }

    #[test]
    fn late_fact_names_round_trip() {
        for fact in [LateFact::ContainerId, LateFact::Ip, LateFact::PodUid] {
            assert_eq!(LateFact::from_name(fact.name()), Some(fact));
        }
        for name in ["", "unknown", "IP", "ContainerId"] {
            assert_eq!(LateFact::from_name(name), None);
        }
    }
}
