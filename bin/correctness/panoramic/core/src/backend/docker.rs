//! One lazy Docker connection and info query shared by a command's runtime checks.
//!
//! Container binaries are checked against the daemon's OS and architecture.

use airlock::docker::DOCKER_DAEMON_TIMEOUT;
use async_trait::async_trait;
use saluki_error::GenericError;
use tokio::sync::OnceCell;

use crate::{artifact::elf::Architecture, layout::Os};

/// The fields of the daemon's `GET /info` answer that runtime checks use.
#[derive(Clone, Debug)]
pub(crate) struct DaemonInfo {
    /// Container OS reported by the daemon.
    pub(crate) os_type: Option<String>,
    /// Native CPU architecture reported by the daemon.
    pub(crate) architecture: Option<String>,
}

/// The Docker daemon operations runtime checks need, replaceable in tests.
#[async_trait]
pub(crate) trait DockerApi: Send + Sync {
    async fn info(&self) -> Result<DaemonInfo, String>;
    async fn image_id(&self, reference: &str) -> Result<Option<String>, String>;
    async fn pull(&self, reference: &str) -> Result<(), String>;
}

struct Docker(airlock::docker::Docker);

#[async_trait]
impl DockerApi for Docker {
    async fn info(&self) -> Result<DaemonInfo, String> {
        self.0
            .info()
            .await
            .map(|info| DaemonInfo {
                os_type: info.os_type,
                architecture: info.architecture,
            })
            .map_err(|error| format!("{:#}", GenericError::new(error)))
    }

    async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
        airlock::docker::image_id(&self.0, reference)
            .await
            .map_err(|error| format!("cannot inspect image '{reference}': {error:#}"))
    }

    async fn pull(&self, reference: &str) -> Result<(), String> {
        airlock::docker::pull_image(&self.0, reference)
            .await
            .map_err(|error| format!("cannot pull image '{reference}': {error:#}"))
    }
}

/// The Docker connection shared by a command's runtime checks.
///
/// Connects and asks the daemon's OS type and architecture on first use, caching the result, including failures.
/// An unanswered info query costs one timeout across all checks sharing this connection.
pub(crate) struct DockerDaemon {
    connect: Box<dyn Fn() -> Result<Box<dyn DockerApi>, String> + Send + Sync>,
    connection: OnceCell<Result<Connection, String>>,
}

struct Connection {
    api: Box<dyn DockerApi>,
    info: DaemonInfo,
}

impl Default for DockerDaemon {
    fn default() -> Self {
        Self {
            connect: Box::new(|| {
                airlock::docker::connect()
                    .map(|docker| Box::new(Docker(docker)) as Box<dyn DockerApi>)
                    .map_err(|error| format!("{error:#}"))
            }),
            connection: OnceCell::new(),
        }
    }
}

impl DockerDaemon {
    #[cfg(test)]
    pub(crate) fn with_connect(
        connect: impl Fn() -> Result<Box<dyn DockerApi>, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            connect: Box::new(connect),
            connection: OnceCell::new(),
        }
    }

    /// Returns the connection if the daemon runs containers of `os`.
    /// macOS is refused without connecting; an OS mismatch affects only that caller.
    pub(crate) async fn for_os(&self, os: Os) -> Result<&dyn DockerApi, String> {
        Ok(self.connection(os).await?.api.as_ref())
    }

    /// Returns the daemon's native architecture if it runs containers of `os`.
    pub(crate) async fn architecture(&self, os: Os) -> Result<Architecture, String> {
        let connection = self.connection(os).await?;
        match connection.info.architecture.as_deref() {
            None | Some("") => Err("the Docker daemon did not report its architecture".into()),
            Some(value) => parse_architecture(value).ok_or_else(|| {
                format!(
                    "the Docker daemon runs on '{value}'; \
                     container binaries can be verified only for x86_64 and aarch64"
                )
            }),
        }
    }

    async fn connection(&self, os: Os) -> Result<&Connection, String> {
        if os == Os::MacOs {
            return Err("no Docker daemon runs macos containers".into());
        }
        let connection = self
            .connection
            .get_or_init(|| async {
                let api = (self.connect)().map_err(|error| format!("cannot connect to a Docker daemon: {error}"))?;
                let info = tokio::time::timeout(DOCKER_DAEMON_TIMEOUT, api.info())
                    .await
                    .map_err(|_| {
                        format!(
                            "the Docker daemon did not answer within {}s",
                            DOCKER_DAEMON_TIMEOUT.as_secs()
                        )
                    })?
                    .map_err(|error| format!("the Docker daemon did not answer: {error}"))?;
                Ok(Connection { api, info })
            })
            .await
            .as_ref()
            .map_err(Clone::clone)?;
        check_daemon_os(connection.info.os_type.as_deref(), os.name())?;
        Ok(connection)
    }
}

/// Maps Docker Engine (`uname -m`) and Go (`GOARCH`) spellings to an architecture.
fn parse_architecture(value: &str) -> Option<Architecture> {
    match value {
        "x86_64" | "amd64" => Some(Architecture::X86_64),
        "aarch64" | "arm64" => Some(Architecture::Aarch64),
        _ => None,
    }
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

/// An outer error means the daemon timed out; an inner error means inspection failed.
pub(crate) async fn image_id_within_deadline(
    images: &dyn DockerApi, reference: &str,
) -> Result<Result<Option<String>, String>, String> {
    tokio::time::timeout(DOCKER_DAEMON_TIMEOUT, images.image_id(reference))
        .await
        .map_err(|_| {
            format!(
                "the Docker daemon did not answer within {}s inspecting image '{reference}'",
                DOCKER_DAEMON_TIMEOUT.as_secs(),
            )
        })
}

#[cfg(test)]
pub(crate) mod test_util {
    use std::sync::{Arc, Mutex};

    use super::*;

    pub(crate) fn recording_daemon<A>(api: A, calls: Arc<Mutex<Vec<String>>>) -> DockerDaemon
    where
        A: DockerApi + Clone + 'static,
    {
        DockerDaemon::with_connect(move || {
            calls.lock().unwrap().push("connect".into());
            Ok(Box::new(api.clone()))
        })
    }

    #[derive(Clone)]
    pub(crate) struct FakeDocker {
        pub(crate) calls: Arc<Mutex<Vec<String>>>,
        pub(crate) info: Result<DaemonInfo, String>,
        pub(crate) pending_info: bool,
    }

    impl FakeDocker {
        /// Reports architecture `x86_64`; override it in `info`.
        pub(crate) fn new(os_type: Option<&str>) -> Self {
            Self {
                calls: Arc::default(),
                info: Ok(DaemonInfo {
                    os_type: os_type.map(str::to_string),
                    architecture: Some("x86_64".into()),
                }),
                pending_info: false,
            }
        }

        pub(crate) fn daemon(&self) -> DockerDaemon {
            recording_daemon(self.clone(), self.calls.clone())
        }
    }

    #[async_trait]
    impl DockerApi for FakeDocker {
        async fn info(&self) -> Result<DaemonInfo, String> {
            self.calls.lock().unwrap().push("info".into());
            if self.pending_info {
                std::future::pending().await
            } else {
                self.info.clone()
            }
        }

        async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
            self.calls.lock().unwrap().push(format!("inspect:{reference}"));
            Ok(Some(format!("sha256:{reference}")))
        }

        async fn pull(&self, reference: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("pull:{reference}"));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{test_util::FakeDocker, *};

    #[test]
    fn docker_and_go_architecture_names_map_to_architectures() {
        for (value, expected) in [
            ("x86_64", Some(Architecture::X86_64)),
            ("amd64", Some(Architecture::X86_64)),
            ("aarch64", Some(Architecture::Aarch64)),
            ("arm64", Some(Architecture::Aarch64)),
            ("riscv64", None),
            ("", None),
            ("X86_64", None),
        ] {
            assert_eq!(parse_architecture(value), expected, "{value}");
        }
    }

    #[tokio::test]
    async fn the_architecture_comes_from_the_same_info_query_as_the_os_type() {
        for (value, expected) in [
            ("x86_64", Architecture::X86_64),
            ("amd64", Architecture::X86_64),
            ("aarch64", Architecture::Aarch64),
            ("arm64", Architecture::Aarch64),
        ] {
            let mut api = FakeDocker::new(Some("linux"));
            api.info.as_mut().unwrap().architecture = Some(value.into());
            let docker = api.daemon();
            let (os, architecture) = tokio::join!(docker.for_os(Os::Linux), docker.architecture(Os::Linux));
            assert!(os.is_ok());
            assert_eq!(architecture.unwrap(), expected);
            assert_eq!(docker.architecture(Os::Linux).await.unwrap(), expected);
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
    }

    #[tokio::test]
    async fn a_daemon_without_a_supported_architecture_cannot_verify_container_binaries() {
        for (value, expected) in [
            (None, "the Docker daemon did not report its architecture"),
            (Some(""), "the Docker daemon did not report its architecture"),
            (
                Some("riscv64"),
                "the Docker daemon runs on 'riscv64'; container binaries can be verified only for x86_64 and aarch64",
            ),
        ] {
            let mut api = FakeDocker::new(Some("linux"));
            api.info.as_mut().unwrap().architecture = value.map(str::to_string);
            let docker = api.daemon();
            for _ in 0..2 {
                assert_eq!(docker.architecture(Os::Linux).await, Err(expected.into()));
            }
            assert!(docker.for_os(Os::Linux).await.is_ok());
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
    }

    #[tokio::test]
    async fn architecture_applies_the_os_checks_of_for_os() {
        for (actual, expected) in [
            (Some("windows"), "the Docker daemon runs windows containers, not linux"),
            (Some("unknown"), "the Docker daemon runs unknown containers, not linux"),
            (None, "the Docker daemon did not report its OS type"),
            (Some(""), "the Docker daemon did not report its OS type"),
        ] {
            let api = FakeDocker::new(actual);
            assert_eq!(api.daemon().architecture(Os::Linux).await, Err(expected.into()));
        }
        let docker = DockerDaemon::with_connect(|| panic!("macos must not connect"));
        assert_eq!(
            docker.architecture(Os::MacOs).await,
            Err("no Docker daemon runs macos containers".into())
        );
    }

    #[tokio::test]
    async fn callers_share_one_connection_and_one_os_query() {
        let api = FakeDocker::new(Some("linux"));
        let docker = api.daemon();
        let (first, second) = tokio::join!(docker.for_os(Os::Linux), docker.for_os(Os::Linux));
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert!(docker.for_os(Os::Linux).await.is_ok());
        assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
    }

    #[tokio::test]
    async fn a_connection_failure_is_reported_without_connecting_again() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let docker = DockerDaemon::with_connect({
            let calls = calls.clone();
            move || {
                calls.lock().unwrap().push("connect");
                Err("unavailable".into())
            }
        });
        for os in [Os::Linux, Os::Windows, Os::Linux] {
            assert_eq!(
                docker.for_os(os).await.err().unwrap(),
                "cannot connect to a Docker daemon: unavailable"
            );
            assert_eq!(
                docker.architecture(os).await,
                Err("cannot connect to a Docker daemon: unavailable".into())
            );
        }
        assert_eq!(*calls.lock().unwrap(), ["connect"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_info_query_is_reported_without_waiting_again() {
        let mut api = FakeDocker::new(Some("linux"));
        api.pending_info = true;
        let docker = api.daemon();
        let start = tokio::time::Instant::now();
        for os in [Os::Linux, Os::Windows, Os::Linux] {
            assert_eq!(
                docker.for_os(os).await.err().unwrap(),
                "the Docker daemon did not answer within 30s"
            );
            assert_eq!(
                docker.architecture(os).await,
                Err("the Docker daemon did not answer within 30s".into())
            );
        }
        assert_eq!(start.elapsed(), DOCKER_DAEMON_TIMEOUT);
        assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
    }

    #[tokio::test]
    async fn a_failed_info_query_is_reported_without_querying_again() {
        let mut api = FakeDocker::new(None);
        api.info = Err("info failed".into());
        let docker = api.daemon();
        for os in [Os::Linux, Os::Windows] {
            assert_eq!(
                docker.for_os(os).await.err().unwrap(),
                "the Docker daemon did not answer: info failed"
            );
            assert_eq!(
                docker.architecture(os).await,
                Err("the Docker daemon did not answer: info failed".into())
            );
        }
        assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
    }

    #[tokio::test]
    async fn a_docker_daemon_of_another_or_unknown_os_type_cannot_serve_that_os() {
        for (actual, os, expected) in [
            (Some("linux"), Os::Linux, Ok(())),
            (
                Some("linux"),
                Os::Windows,
                Err("the Docker daemon runs linux containers, not windows"),
            ),
            (
                Some("windows"),
                Os::Linux,
                Err("the Docker daemon runs windows containers, not linux"),
            ),
            (Some("windows"), Os::Windows, Ok(())),
            (
                Some("unknown"),
                Os::Linux,
                Err("the Docker daemon runs unknown containers, not linux"),
            ),
            (
                Some("unknown"),
                Os::Windows,
                Err("the Docker daemon runs unknown containers, not windows"),
            ),
            (Some(""), Os::Linux, Err("the Docker daemon did not report its OS type")),
            (
                Some(""),
                Os::Windows,
                Err("the Docker daemon did not report its OS type"),
            ),
            (None, Os::Linux, Err("the Docker daemon did not report its OS type")),
            (None, Os::Windows, Err("the Docker daemon did not report its OS type")),
        ] {
            let api = FakeDocker::new(actual);
            let docker = api.daemon();
            for _ in 0..2 {
                assert_eq!(docker.for_os(os).await.map(|_| ()), expected.map_err(str::to_string));
            }
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
    }

    #[tokio::test]
    async fn an_os_mismatch_does_not_prevent_a_matching_caller_using_the_connection() {
        let api = FakeDocker::new(Some("windows"));
        let docker = api.daemon();
        assert_eq!(
            docker.for_os(Os::Linux).await.err().unwrap(),
            "the Docker daemon runs windows containers, not linux"
        );
        assert!(docker.for_os(Os::Windows).await.is_ok());
        assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
    }

    #[tokio::test]
    async fn macos_containers_are_refused_without_contacting_a_daemon() {
        let docker = DockerDaemon::with_connect(|| panic!("macos must not connect"));
        assert_eq!(
            docker.for_os(Os::MacOs).await.err().unwrap(),
            "no Docker daemon runs macos containers"
        );
    }
}
