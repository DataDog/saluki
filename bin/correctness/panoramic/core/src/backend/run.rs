//! Runtime checks sharing the command's connection to its Docker daemon.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::Path,
};

use super::docker::DockerDaemon;
use crate::{
    artifact::{self, ArtifactBinding, ArtifactVerification},
    layout::{Os, Realization, RealizationKind},
};

/// Runtime checks, artifact verification and Linux container scopes sharing one lazily connected Docker daemon.
#[derive(Default)]
pub struct RunBackends {
    pub(super) docker: DockerDaemon,
    #[cfg(test)]
    providability_checks: std::sync::Mutex<BTreeMap<Realization, usize>>,
}

impl RunBackends {
    #[cfg(test)]
    fn with_docker(docker: DockerDaemon) -> Self {
        Self {
            docker,
            providability_checks: std::sync::Mutex::default(),
        }
    }

    /// Returns each realization this runner host cannot provide, with the reason.
    /// Checks each distinct realization once; an empty input contacts nothing.
    pub async fn unprovidable(&self, needed: impl IntoIterator<Item = Realization>) -> BTreeMap<Realization, String> {
        self.unprovidable_on_path(needed, std::env::var_os("PATH").as_deref())
            .await
    }

    async fn unprovidable_on_path(
        &self, needed: impl IntoIterator<Item = Realization>, path: Option<&OsStr>,
    ) -> BTreeMap<Realization, String> {
        let needed: BTreeSet<_> = needed.into_iter().collect();
        let mut unavailable = BTreeMap::new();
        for realization in needed {
            if let Err(reason) = self.providable(realization, path).await {
                unavailable.insert(realization, reason);
            }
        }
        unavailable
    }

    async fn providable(&self, realization: Realization, path: Option<&OsStr>) -> Result<(), String> {
        #[cfg(test)]
        {
            *self
                .providability_checks
                .lock()
                .unwrap()
                .entry(realization)
                .or_default() += 1;
        }
        let Realization { os, kind } = realization;
        match kind {
            RealizationKind::LinuxContainer => {
                if os != Os::Linux {
                    return Err(format!("{} runs only on {}", kind.name(), Os::Linux.name()));
                }
                self.docker.for_os(Os::Linux).await.map(|_| ())
            }
            RealizationKind::WindowsContainer => {
                if os != Os::Windows {
                    return Err(format!("{} runs only on {}", kind.name(), Os::Windows.name()));
                }
                self.docker.for_os(Os::Windows).await.map(|_| ())
            }
            RealizationKind::HostProcessGroup => {
                if os == Os::host() {
                    Ok(())
                } else {
                    Err(format!("this host runs {}, not {}", Os::host().name(), os.name()))
                }
            }
            RealizationKind::KubernetesPod => {
                if os != Os::Linux {
                    return Err(format!("{} runs only on {}", kind.name(), Os::Linux.name()));
                }
                check_tools(path)?;
                self.docker.for_os(Os::Linux).await.map(|_| ())
            }
        }
    }

    /// Verifies bindings in input order, resolving relative file paths against `base`.
    /// Container binaries must match the Docker daemon's OS and architecture.
    /// A daemon timeout fails the remaining images in this call; file checks continue.
    pub async fn verify(&self, bindings: &[ArtifactBinding], base: &Path) -> Vec<ArtifactVerification> {
        artifact::verify(&self.docker, bindings, base).await
    }

    /// Pulls image bindings in input order, including images already present, and records their IDs.
    /// A pull or inspection timeout fails the remaining images in this call.
    pub async fn pull(&self, bindings: &[ArtifactBinding]) -> Vec<ArtifactVerification> {
        artifact::pull(&self.docker, bindings).await
    }
}

fn check_tools(path: Option<&OsStr>) -> Result<(), String> {
    for name in ["kind", "docker"] {
        if !on_path(name, path) {
            return Err(format!("'{name}' is not on PATH"));
        }
    }
    Ok(())
}

fn on_path(name: &str, path: Option<&OsStr>) -> bool {
    path.is_some_and(|path| {
        std::env::split_paths(path).any(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        artifact::{ArtifactKind, ArtifactRole, BindingSource},
        backend::docker::test_util::FakeDocker,
    };

    #[tokio::test]
    async fn a_linux_scope_needs_a_daemon_running_linux_containers() {
        let scope = crate::plan::ScopeLayout {
            run: "abcd1234".into(),
            index: 0,
        };
        for (os, expected) in [
            ("windows", "the Docker daemon runs windows containers, not linux"),
            ("linux", "this Docker connection cannot create containers"),
        ] {
            let api = FakeDocker::new(Some(os));
            let backends = RunBackends::with_docker(api.daemon());
            let error = backends
                .prepare_linux_scope(&scope, &["p"], "local:latest", Vec::new())
                .await
                .err()
                .unwrap();
            assert_eq!(error.to_string(), expected);
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
    }

    #[tokio::test]
    async fn one_connection_serves_providability_verification_and_pulling() {
        let api = FakeDocker::new(Some("linux"));
        let backends = RunBackends::with_docker(api.daemon());
        assert!(backends
            .unprovidable([Realization {
                os: Os::Linux,
                kind: RealizationKind::LinuxContainer
            }])
            .await
            .is_empty());
        let bindings = [ArtifactBinding {
            role: ArtifactRole {
                name: "tool",
                kind: ArtifactKind::Image { os: Os::Linux },
                local_default: "tool:latest",
            },
            reference: "tool:latest".into(),
            source: BindingSource::LocalDefault,
        }];
        let verified = backends.verify(&bindings, Path::new(".")).await;
        let pulled = backends.pull(&bindings).await;
        for results in [verified, pulled] {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].binding, bindings[0]);
            assert_eq!(results[0].identity, Ok("sha256:tool:latest".into()));
        }
        assert_eq!(
            *api.calls.lock().unwrap(),
            [
                "connect",
                "info",
                "inspect:tool:latest",
                "pull:tool:latest",
                "inspect:tool:latest"
            ]
        );
    }

    #[tokio::test]
    async fn each_needed_realization_is_checked_once_and_only_unprovidable_ones_are_returned() {
        let api = FakeDocker::new(Some("linux"));
        let backends = RunBackends::with_docker(api.daemon());
        let host = Realization {
            os: Os::host(),
            kind: RealizationKind::HostProcessGroup,
        };
        let linux = Realization {
            os: Os::Linux,
            kind: RealizationKind::LinuxContainer,
        };
        let windows = Realization {
            os: Os::Windows,
            kind: RealizationKind::WindowsContainer,
        };
        assert_eq!(
            backends
                .unprovidable([host, linux, windows, linux, windows, host])
                .await,
            BTreeMap::from([(windows, "the Docker daemon runs linux containers, not windows".into())])
        );
        assert_eq!(
            *backends.providability_checks.lock().unwrap(),
            BTreeMap::from([(host, 1), (linux, 1), (windows, 1)])
        );
        assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
    }

    #[tokio::test]
    async fn no_daemon_is_contacted_when_no_realization_is_needed() {
        let backends = RunBackends::with_docker(DockerDaemon::with_connect(|| panic!("empty input must not connect")));
        assert!(backends.unprovidable([]).await.is_empty());
    }

    #[tokio::test]
    async fn host_process_groups_are_provided_only_on_the_runner_host_os() {
        let backends =
            RunBackends::with_docker(DockerDaemon::with_connect(|| panic!("host processes must not connect")));
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let realization = Realization {
                os,
                kind: RealizationKind::HostProcessGroup,
            };
            let expected = if os == Os::host() {
                BTreeMap::new()
            } else {
                BTreeMap::from([(
                    realization,
                    format!("this host runs {}, not {}", Os::host().name(), os.name()),
                )])
            };
            assert_eq!(backends.unprovidable([realization]).await, expected);
        }
    }

    #[tokio::test]
    async fn container_backends_refuse_a_participant_os_other_than_their_own_without_contacting_a_daemon() {
        let backends = RunBackends::with_docker(DockerDaemon::with_connect(|| panic!("invalid OS must not connect")));
        for (kind, supported) in [
            (RealizationKind::LinuxContainer, Os::Linux),
            (RealizationKind::WindowsContainer, Os::Windows),
            (RealizationKind::KubernetesPod, Os::Linux),
        ] {
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                if os != supported {
                    let realization = Realization { os, kind };
                    assert_eq!(
                        backends.unprovidable([realization]).await,
                        BTreeMap::from([(
                            realization,
                            format!("{} runs only on {}", kind.name(), supported.name())
                        )])
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn containers_and_pods_need_a_daemon_running_their_os() {
        let dir = tempfile::tempdir().unwrap();
        for tool in ["kind", "docker"] {
            std::fs::write(dir.path().join(format!("{tool}{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        }
        for actual in ["linux", "windows"] {
            let api = FakeDocker::new(Some(actual));
            let backends = RunBackends::with_docker(api.daemon());
            for (kind, os) in [
                (RealizationKind::LinuxContainer, Os::Linux),
                (RealizationKind::WindowsContainer, Os::Windows),
                (RealizationKind::KubernetesPod, Os::Linux),
            ] {
                let realization = Realization { os, kind };
                let expected = if actual == os.name() {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([(
                        realization,
                        format!("the Docker daemon runs {actual} containers, not {}", os.name()),
                    )])
                };
                assert_eq!(
                    backends
                        .unprovidable_on_path([realization], Some(dir.path().as_os_str()))
                        .await,
                    expected
                );
            }
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
    }

    #[tokio::test]
    async fn missing_pod_tools_fail_before_contacting_a_daemon() {
        let backends =
            RunBackends::with_docker(DockerDaemon::with_connect(|| panic!("missing tools must not connect")));
        let pod = Realization {
            os: Os::Linux,
            kind: RealizationKind::KubernetesPod,
        };
        assert_eq!(
            backends.unprovidable_on_path([pod], None).await,
            BTreeMap::from([(pod, "'kind' is not on PATH".into())])
        );
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
        assert_eq!(check_tools(path), Err("'kind' is not on PATH".to_string()));
        std::fs::write(dir.path().join(format!("kind{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        assert_eq!(check_tools(path), Err("'docker' is not on PATH".to_string()));
        std::fs::write(dir.path().join(format!("docker{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        assert_eq!(check_tools(path), Ok(()));
    }
}
