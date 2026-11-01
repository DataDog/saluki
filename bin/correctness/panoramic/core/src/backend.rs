//! Checks whether the runner host can provide the realizations selected by case layouts.
//!
//! A backend creates participants of one realization kind and checks host availability for their OS.
//! Layouts select realizations independently of host availability.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    time::Duration,
};

use async_trait::async_trait;
use saluki_error::GenericError;

use crate::layout::{Os, Realization, RealizationKind};

const PROVIDABILITY_TIMEOUT: Duration = Duration::from_secs(30);

/// An execution mechanism's link to the runner host.
#[async_trait]
pub(crate) trait Backend: Send + Sync {
    /// The realization kind this backend creates.
    fn kind(&self) -> RealizationKind;

    /// Checks that this host can create this kind for participants on `os`.
    async fn providable(&self, os: Os) -> Result<(), String>;
}

pub(crate) const BACKENDS: &[&dyn Backend] = &[
    &LinuxContainers,
    &WindowsContainers,
    &HostProcessGroups,
    &KubernetesPods,
];

/// Returns each realization this runner host cannot provide, with the reason.
///
/// Asks each backend once per distinct realization; an empty input contacts nothing.
pub async fn unprovidable(needed: impl IntoIterator<Item = Realization>) -> BTreeMap<Realization, String> {
    unprovidable_with(needed, BACKENDS).await
}

async fn unprovidable_with(
    needed: impl IntoIterator<Item = Realization>, backends: &[&dyn Backend],
) -> BTreeMap<Realization, String> {
    let needed: BTreeSet<_> = needed.into_iter().collect();
    let mut unavailable = BTreeMap::new();
    for realization in needed {
        let result = match backends.iter().find(|backend| backend.kind() == realization.kind) {
            Some(backend) => backend.providable(realization.os).await,
            None => Err(format!("no backend creates {}", realization.kind.name())),
        };
        if let Err(reason) = result {
            unavailable.insert(realization, reason);
        }
    }
    unavailable
}

pub(crate) struct LinuxContainers;
pub(crate) struct WindowsContainers;
pub(crate) struct HostProcessGroups;
pub(crate) struct KubernetesPods;

#[async_trait]
impl Backend for LinuxContainers {
    fn kind(&self) -> RealizationKind {
        RealizationKind::LinuxContainer
    }

    async fn providable(&self, os: Os) -> Result<(), String> {
        if os != Os::Linux {
            return Err(format!("{} runs only on linux", self.kind().name()));
        }
        daemon_reports("linux").await
    }
}

#[async_trait]
impl Backend for WindowsContainers {
    fn kind(&self) -> RealizationKind {
        RealizationKind::WindowsContainer
    }

    async fn providable(&self, os: Os) -> Result<(), String> {
        if os != Os::Windows {
            return Err(format!("{} runs only on windows", self.kind().name()));
        }
        daemon_reports("windows").await
    }
}

#[async_trait]
impl Backend for HostProcessGroups {
    fn kind(&self) -> RealizationKind {
        RealizationKind::HostProcessGroup
    }

    async fn providable(&self, os: Os) -> Result<(), String> {
        if os == Os::host() {
            Ok(())
        } else {
            Err(format!("this host runs {}, not {}", Os::host().name(), os.name()))
        }
    }
}

#[async_trait]
impl Backend for KubernetesPods {
    fn kind(&self) -> RealizationKind {
        RealizationKind::KubernetesPod
    }

    async fn providable(&self, os: Os) -> Result<(), String> {
        if os != Os::Linux {
            return Err(format!("{} runs only on linux", self.kind().name()));
        }
        Self::check_tools(std::env::var_os("PATH").as_deref())?;
        daemon_reports("linux").await
    }
}

impl KubernetesPods {
    fn check_tools(path: Option<&OsStr>) -> Result<(), String> {
        for name in ["kind", "docker"] {
            if !on_path(name, path) {
                return Err(format!("'{name}' is not on PATH"));
            }
        }
        Ok(())
    }
}

async fn daemon_reports(expected: &str) -> Result<(), String> {
    let docker = airlock::docker::connect().map_err(|error| format!("cannot connect to a Docker daemon: {error:#}"))?;
    let info = tokio::time::timeout(PROVIDABILITY_TIMEOUT, docker.info())
        .await
        .map_err(|_| {
            format!(
                "the Docker daemon did not answer within {}s",
                PROVIDABILITY_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| format!("the Docker daemon did not answer: {:#}", GenericError::new(error)))?;
    check_daemon_os(info.os_type.as_deref(), expected)
}

fn check_daemon_os(os_type: Option<&str>, expected: &str) -> Result<(), String> {
    match os_type {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) if !actual.is_empty() => {
            Err(format!("the Docker daemon runs {actual} containers, not {expected}"))
        }
        _ => Err("the Docker daemon did not report its OS type".to_string()),
    }
}

fn on_path(name: &str, path: Option<&OsStr>) -> bool {
    path.is_some_and(|path| {
        std::env::split_paths(path).any(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)).is_file())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[tokio::test]
    async fn host_process_groups_are_provided_only_on_the_runner_host_os() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let expected = if os == Os::host() {
                Ok(())
            } else {
                Err(format!("this host runs {}, not {}", Os::host().name(), os.name()))
            };
            assert_eq!(HostProcessGroups.providable(os).await, expected);
        }
        assert!(unprovidable([Realization {
            os: Os::host(),
            kind: RealizationKind::HostProcessGroup,
        }])
        .await
        .is_empty());
    }

    #[tokio::test]
    async fn container_backends_refuse_a_participant_os_other_than_their_own_without_contacting_a_daemon() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            if os != Os::Linux {
                assert_eq!(
                    LinuxContainers.providable(os).await,
                    Err("linux-container runs only on linux".to_string())
                );
                assert_eq!(
                    KubernetesPods.providable(os).await,
                    Err("kubernetes-pod runs only on linux".to_string())
                );
            }
            if os != Os::Windows {
                assert_eq!(
                    WindowsContainers.providable(os).await,
                    Err("windows-container runs only on windows".to_string())
                );
            }
        }
    }

    #[test]
    fn a_docker_daemon_of_another_or_unknown_os_type_cannot_provide_a_container() {
        for expected in ["linux", "windows"] {
            for actual in ["linux", "windows", "unknown", ""] {
                let result = if actual.is_empty() {
                    Err("the Docker daemon did not report its OS type".to_string())
                } else if actual == expected {
                    Ok(())
                } else {
                    Err(format!("the Docker daemon runs {actual} containers, not {expected}"))
                };
                assert_eq!(check_daemon_os(Some(actual), expected), result);
            }
            assert_eq!(
                check_daemon_os(None, expected),
                Err("the Docker daemon did not report its OS type".to_string())
            );
        }
    }

    #[test]
    fn on_path_finds_only_executables_in_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let executable = format!("tool{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.path().join(&executable), "").unwrap();
        let missing = dir.path().join("missing");
        let path = std::env::join_paths([missing.as_path(), dir.path()]).unwrap();
        assert!(on_path("tool", Some(&path)));
        assert!(!on_path("other", Some(&path)));
        assert!(!on_path("tool", Some(missing.as_os_str())));
        assert!(!on_path("panoramic-no-such-tool", Some(OsStr::new(""))));
        assert!(!on_path("tool", None));
        std::fs::remove_file(dir.path().join(&executable)).unwrap();
        std::fs::create_dir(dir.path().join(executable)).unwrap();
        assert!(!on_path("tool", Some(&path)));
    }

    #[test]
    fn kind_pods_need_kind_then_docker_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = Some(dir.path().as_os_str());
        assert_eq!(
            KubernetesPods::check_tools(path),
            Err("'kind' is not on PATH".to_string())
        );
        std::fs::write(dir.path().join(format!("kind{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        assert_eq!(
            KubernetesPods::check_tools(path),
            Err("'docker' is not on PATH".to_string())
        );
        std::fs::write(dir.path().join(format!("docker{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        assert_eq!(KubernetesPods::check_tools(path), Ok(()));
    }

    struct FakeBackend {
        kind: RealizationKind,
        calls: Mutex<Vec<Os>>,
        result: Result<(), String>,
    }

    #[async_trait]
    impl Backend for FakeBackend {
        fn kind(&self) -> RealizationKind {
            self.kind
        }

        async fn providable(&self, os: Os) -> Result<(), String> {
            self.calls.lock().unwrap().push(os);
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn each_needed_realization_is_asked_once_and_only_unprovidable_ones_are_returned() {
        let host = FakeBackend {
            kind: RealizationKind::HostProcessGroup,
            calls: Mutex::default(),
            result: Err("host unavailable".to_string()),
        };
        let container = FakeBackend {
            kind: RealizationKind::LinuxContainer,
            calls: Mutex::default(),
            result: Ok(()),
        };
        let linux_host = Realization {
            os: Os::Linux,
            kind: host.kind,
        };
        let mac_host = Realization {
            os: Os::MacOs,
            kind: host.kind,
        };
        let linux_container = Realization {
            os: Os::Linux,
            kind: container.kind,
        };
        let unavailable = unprovidable_with(
            [
                mac_host,
                linux_host,
                linux_container,
                linux_host,
                mac_host,
                linux_container,
            ],
            &[&host, &container],
        )
        .await;
        assert_eq!(
            unavailable,
            BTreeMap::from([
                (linux_host, "host unavailable".to_string()),
                (mac_host, "host unavailable".to_string()),
            ])
        );
        assert_eq!(*host.calls.lock().unwrap(), [Os::Linux, Os::MacOs]);
        assert_eq!(*container.calls.lock().unwrap(), [Os::Linux]);
    }

    #[tokio::test]
    async fn no_backend_is_asked_when_no_realization_is_needed() {
        let backend = FakeBackend {
            kind: RealizationKind::LinuxContainer,
            calls: Mutex::default(),
            result: Err("unused".to_string()),
        };
        assert!(unprovidable_with([], &[&backend]).await.is_empty());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_realization_with_no_backend_is_unprovidable() {
        let realization = Realization {
            os: Os::Linux,
            kind: RealizationKind::LinuxContainer,
        };
        assert_eq!(
            unprovidable_with([realization], &[]).await,
            BTreeMap::from([(realization, "no backend creates linux-container".to_string())])
        );
    }
}
