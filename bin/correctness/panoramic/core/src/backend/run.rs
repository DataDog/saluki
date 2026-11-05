//! Runtime checks sharing the command's connection to its Docker daemon.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::Path,
};

use k8s_openapi::api::core::v1::Node as KubernetesNode;
use kube::{api::ListParams, config::KubeConfigOptions, Api, Client, Config};
use saluki_error::{generic_error, GenericError};
use tokio::sync::OnceCell;

use super::{docker::DockerDaemon, pod::node::Node};
use crate::{
    artifact::{self, ArtifactBinding, ArtifactVerification},
    layout::{Os, Realization, RealizationKind},
};

pub(super) struct PodConnection {
    pub(super) client: Client,
    pub(super) node: Node,
}

/// Runtime checks, artifact verification and container scopes sharing one lazily connected Docker daemon.
#[derive(Default)]
pub struct RunBackends {
    pub(super) docker: DockerDaemon,
    pub(super) pods: OnceCell<PodConnection>,
    kube_context: Option<String>,
    #[cfg(test)]
    providability_checks: std::sync::Mutex<BTreeMap<Realization, usize>>,
}

impl RunBackends {
    /// Selects the cluster context used for pod participants instead of the current context.
    pub fn with_kube_context(mut self, context: String) -> Self {
        self.kube_context = Some(context);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_docker(docker: DockerDaemon) -> Self {
        Self {
            docker,
            pods: OnceCell::new(),
            kube_context: None,
            providability_checks: std::sync::Mutex::default(),
        }
    }

    pub(super) async fn pod_connection(&self) -> Result<&PodConnection, GenericError> {
        self.pods
            .get_or_try_init(|| async { self.connect_pods(Client::try_from(self.pod_config().await?)?).await })
            .await
    }

    async fn pod_config(&self) -> Result<Config, GenericError> {
        match &self.kube_context {
            Some(context) => Ok(Config::from_kubeconfig(&KubeConfigOptions {
                context: Some(context.clone()),
                ..Default::default()
            })
            .await?),
            None => Ok(Config::infer().await?),
        }
    }

    pub(super) async fn connect_pods(&self, client: Client) -> Result<PodConnection, GenericError> {
        check_docker_context().await.map_err(|error| generic_error!(error))?;
        let docker = self
            .docker
            .client(Os::Linux)
            .await
            .map_err(|error| generic_error!(error))?;
        let nodes: Api<KubernetesNode> = Api::all(client.clone());
        let nodes = nodes.list(&ListParams::default()).await?.items;
        if nodes.len() != 1 {
            return Err(generic_error!(
                "Pod scopes require a single-node kind cluster; found {} nodes",
                nodes.len()
            ));
        }
        let container = nodes[0]
            .metadata
            .name
            .clone()
            .ok_or_else(|| generic_error!("Cluster node has no name"))?;
        let inspected = docker.inspect_container(&container, None).await?;
        if !inspected
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .is_some_and(|labels| {
                labels
                    .get("io.x-k8s.kind.role")
                    .is_some_and(|role| role == "control-plane")
            })
        {
            return Err(generic_error!(
                "Kubernetes node '{container}' is not a kind control-plane container on the verified daemon"
            ));
        }
        Ok(PodConnection {
            client,
            node: Node { docker, container },
        })
    }

    /// Removes all scope resources, including partial preparation. Cancel-safe; the caller bounds it.
    pub(crate) async fn remove_container_scope(
        &self, os: Os, scope: &crate::plan::ScopeLayout,
    ) -> Result<(), saluki_error::GenericError> {
        let client = self
            .docker
            .client(os)
            .await
            .map_err(|error| saluki_error::generic_error!(error))?;
        airlock::group::IsolationGroup::remove(&client, &scope.id()).await
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
                check_docker_context().await?;
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

async fn check_docker_context() -> Result<(), String> {
    let host = std::env::var_os("DOCKER_HOST");
    if host.as_ref().is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }
    let output = tokio::time::timeout(
        airlock::docker::DOCKER_DAEMON_TIMEOUT,
        tokio::process::Command::new("docker")
            .args(["context", "show"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "timed out checking the active Docker context".to_string())?
    .map_err(|error| format!("cannot check the active Docker context: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker context show failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    validate_docker_context(host.as_deref(), String::from_utf8_lossy(&output.stdout).trim())
}

fn validate_docker_context(host: Option<&OsStr>, context: &str) -> Result<(), String> {
    if host.is_some_and(|value| !value.is_empty()) || context == "default" {
        Ok(())
    } else {
        Err(format!("Pod setup requires DOCKER_HOST when the active Docker context is '{context}', not 'default'; set it to the daemon kind uses"))
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

    async fn rerun_in_child(test: &str, env: &[(&str, Option<&OsStr>)]) {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--nocapture"]).kill_on_drop(true);
        for (key, value) in env {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn pod_configuration_selects_the_named_context_instead_of_the_current_one() {
        const CHILD: &str = "PANORAMIC_CONTEXT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let backends = RunBackends::default().with_kube_context("selected".into());
            let config = backends.pod_config().await.unwrap();
            assert_eq!(config.cluster_url.to_string(), "https://127.0.0.1:6444/");
            assert_eq!(config.default_namespace, "selected-namespace");
            assert_eq!(
                RunBackends::default()
                    .pod_config()
                    .await
                    .unwrap()
                    .cluster_url
                    .to_string(),
                "https://127.0.0.1:6443/"
            );
            assert!(RunBackends::default()
                .with_kube_context("missing".into())
                .pod_config()
                .await
                .is_err());
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap();
        let dir = tempfile::tempdir_in(root.join("target")).unwrap();
        let path = dir.path().join("kubeconfig");
        std::fs::write(
            &path,
            r"apiVersion: v1
kind: Config
current-context: current
clusters:
- name: first
  cluster: {server: 'https://127.0.0.1:6443'}
- name: second
  cluster: {server: 'https://127.0.0.1:6444'}
contexts:
- name: current
  context: {cluster: first, user: test}
- name: selected
  context: {cluster: second, user: test, namespace: selected-namespace}
users:
- name: test
  user: {token: test-token}
",
        )
        .unwrap();
        // Isolate kubeconfig discovery from tests that use the runner's environment.
        rerun_in_child(
            "backend::run::tests::pod_configuration_selects_the_named_context_instead_of_the_current_one",
            &[(CHILD, Some(OsStr::new("1"))), ("KUBECONFIG", Some(path.as_os_str()))],
        )
        .await;
    }

    #[test]
    fn pod_setup_rejects_an_implicit_nondefault_docker_context() {
        for host in [None, Some(OsStr::new("")), Some(OsStr::new("unix:///socket"))] {
            for context in ["default", "other", ""] {
                let result = validate_docker_context(host, context);
                if host.is_some_and(|value| !value.is_empty()) || context == "default" {
                    assert_eq!(result, Ok(()));
                } else {
                    let message = result.unwrap_err();
                    assert!(
                        message.contains("DOCKER_HOST") && message.contains(context),
                        "{message}"
                    );
                }
            }
        }
    }

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
                .prepare_linux_scope(&scope, &["p"], "local:latest", bytes::Bytes::new())
                .await
                .err()
                .unwrap();
            assert_eq!(error.to_string(), expected);
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
    }

    #[tokio::test]
    async fn a_windows_scope_needs_a_daemon_running_windows_containers() {
        let scope = crate::plan::ScopeLayout {
            run: "abcd1234".into(),
            index: 0,
        };
        for (os, expected) in [
            ("linux", "the Docker daemon runs linux containers, not windows"),
            ("windows", "this Docker connection cannot create containers"),
        ] {
            let api = FakeDocker::new(Some(os));
            let backends = RunBackends::with_docker(api.daemon());
            let error = backends
                .prepare_windows_scope(&scope, &["p"], bytes::Bytes::new())
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
        if std::env::var_os("DOCKER_HOST").is_none_or(|value| value.is_empty()) {
            rerun_in_child(
                "backend::run::tests::containers_and_pods_need_a_daemon_running_their_os",
                &[("DOCKER_HOST", Some(OsStr::new("test-daemon")))],
            )
            .await;
            return;
        }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_nondefault_context_fails_providability_before_contacting_the_daemon() {
        const CHILD: &str = "PANORAMIC_DOCKER_CONTEXT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let api = FakeDocker::new(Some("linux"));
            let backends = RunBackends::with_docker(api.daemon());
            let pod = Realization {
                os: Os::Linux,
                kind: RealizationKind::KubernetesPod,
            };
            let errors = backends.unprovidable([pod]).await;
            assert!(
                errors[&pod].contains("DOCKER_HOST") && errors[&pod].contains("other-context"),
                "{errors:?}"
            );
            assert!(api.calls.lock().unwrap().is_empty());
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap();
        let dir = tempfile::tempdir_in(root.join("target")).unwrap();
        let docker = dir.path().join("docker");
        std::fs::write(&docker, "#!/bin/sh\nprintf 'other-context\\n'\n").unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("kind"), "").unwrap();
        rerun_in_child(
            "backend::run::tests::a_nondefault_context_fails_providability_before_contacting_the_daemon",
            &[
                (CHILD, Some(OsStr::new("1"))),
                ("DOCKER_HOST", None),
                ("PATH", Some(dir.path().as_os_str())),
            ],
        )
        .await;
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
