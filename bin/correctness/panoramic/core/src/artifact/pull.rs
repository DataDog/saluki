//! Pulls bound images and records their identities from the Docker daemon.

use std::time::Duration;

use tracing::info;

use super::{ArtifactBinding, ArtifactKind, ArtifactVerification};
use crate::backend::docker::{image_id_within_deadline, DockerApi, DockerDaemon};

// Bounds each pull so stalled transfers become reported failures.
// A timeout fails the remaining images; at most one pull per call waits this long.
const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Pulls only image bindings, in input order, including images already present, and records their IDs.
/// A pull or inspection timeout fails all remaining images; other failures affect only that image.
pub(crate) async fn pull(docker: &DockerDaemon, bindings: &[ArtifactBinding]) -> Vec<ArtifactVerification> {
    let mut timeout: Option<String> = None;
    let mut results = Vec::new();
    for binding in bindings {
        let ArtifactKind::Image { os } = binding.role.kind else {
            continue;
        };
        let reference = &binding.reference;
        let identity = if reference.is_empty() {
            Err(format!(
                "role '{}' requires a binding in an artifact manifest",
                binding.role.name
            ))
        } else if let Some(error) = &timeout {
            Err(error.clone())
        } else {
            match docker.for_os(os).await {
                Ok(images) => {
                    info!("Pulling image '{reference}' for role '{}'...", binding.role.name);
                    match pull_one(images, reference).await {
                        Ok(identity) => Ok(identity),
                        Err(Failure::Image(error)) => Err(error),
                        Err(Failure::Daemon(error)) => {
                            timeout = Some(error.clone());
                            Err(error)
                        }
                    }
                }
                Err(error) => Err(error),
            }
        };
        results.push(ArtifactVerification {
            binding: binding.clone(),
            identity,
        });
    }
    results
}

enum Failure {
    Image(String),
    Daemon(String),
}

async fn pull_one(images: &dyn DockerApi, reference: &str) -> Result<String, Failure> {
    tokio::time::timeout(IMAGE_PULL_TIMEOUT, images.pull(reference))
        .await
        .map_err(|_| {
            Failure::Daemon(format!(
                "the Docker daemon did not finish pulling image '{reference}' within {}s",
                IMAGE_PULL_TIMEOUT.as_secs(),
            ))
        })?
        .map_err(Failure::Image)?;
    image_id_within_deadline(images, reference)
        .await
        .map_err(Failure::Daemon)?
        .map_err(Failure::Image)?
        .ok_or_else(|| {
            Failure::Image(format!(
                "image '{reference}' was pulled but the Docker daemon does not report it"
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use airlock::docker::DOCKER_DAEMON_TIMEOUT;
    use async_trait::async_trait;
    use tracing::instrument::WithSubscriber as _;

    use super::*;
    use crate::{
        artifact::{ArtifactRole, BindingSource},
        backend::docker::test_util::recording_daemon,
        layout::Os,
    };

    fn binding(name: &'static str, kind: ArtifactKind, reference: &str) -> ArtifactBinding {
        ArtifactBinding {
            role: ArtifactRole {
                name,
                kind,
                local_default: "unused",
            },
            reference: reference.into(),
            source: BindingSource::Manifest,
        }
    }

    #[derive(Clone, Default)]
    struct FakeImages {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeImages {
        fn daemon(&self) -> DockerDaemon {
            recording_daemon(self.clone(), self.calls.clone())
        }
    }

    #[async_trait]
    impl DockerApi for FakeImages {
        async fn os_type(&self) -> Result<Option<String>, String> {
            self.calls.lock().unwrap().push("os_type".into());
            Ok(Some("linux".into()))
        }

        // Ordinary references are present, so skipping pulls or falling back to local images changes the call log.
        async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
            self.calls.lock().unwrap().push(format!("inspect:{reference}"));
            match reference {
                "tool:missing" => Ok(None),
                "tool:inspect-error" => Err("inspection failed".into()),
                "tool:inspect-timeout" => std::future::pending().await,
                _ => Ok(Some(format!("sha256:{reference}"))),
            }
        }

        async fn pull(&self, reference: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("pull:{reference}"));
            match reference {
                "tool:pull-error" => Err("cannot pull image 'tool:pull-error': pull access denied".into()),
                "tool:pull-timeout" => std::future::pending().await,
                _ => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn images_are_pulled_then_inspected_in_input_order_and_files_are_ignored() {
        let bindings = [
            binding("z", ArtifactKind::Image { os: Os::Linux }, "tool:first"),
            binding("binary", ArtifactKind::Binary, "bin/tool"),
            binding("a", ArtifactKind::Image { os: Os::Linux }, "tool:second"),
            binding(
                "install",
                ArtifactKind::Install {
                    main_binary: "bin/tool",
                },
                "install",
            ),
        ];
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(Mutex::new(output.reopen().unwrap()))
            .finish();
        let results = pull(&images.daemon(), &bindings).with_subscriber(subscriber).await;
        let log = std::fs::read_to_string(output.path()).unwrap();
        assert!(log.contains("Pulling image 'tool:first' for role 'z'..."), "{log}");
        assert!(log.contains("Pulling image 'tool:second' for role 'a'..."), "{log}");
        assert_eq!(results.len(), 2);
        for (result, binding) in results.iter().zip([&bindings[0], &bindings[2]]) {
            assert_eq!(&result.binding, binding);
            assert_eq!(result.identity, Ok(format!("sha256:{}", binding.reference)));
        }
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "connect",
                "os_type",
                "pull:tool:first",
                "inspect:tool:first",
                "pull:tool:second",
                "inspect:tool:second",
            ]
        );
    }

    #[tokio::test]
    async fn an_image_already_present_is_pulled_again() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let results = pull(
            &images.daemon(),
            &[binding("image", ArtifactKind::Image { os: Os::Linux }, "tool:present")],
        )
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].identity, Ok("sha256:tool:present".into()));
        assert_eq!(
            *calls.lock().unwrap(),
            ["connect", "os_type", "pull:tool:present", "inspect:tool:present"]
        );
    }

    #[tokio::test]
    async fn a_failed_pull_fails_that_image_even_if_a_local_copy_exists_and_the_rest_are_pulled() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let bindings = [
            binding("bad", ArtifactKind::Image { os: Os::Linux }, "tool:pull-error"),
            binding("good", ArtifactKind::Image { os: Os::Linux }, "tool:present"),
        ];
        let results = pull(&images.daemon(), &bindings).await;
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].identity,
            Err("cannot pull image 'tool:pull-error': pull access denied".into())
        );
        assert_eq!(results[1].identity, Ok("sha256:tool:present".into()));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "connect",
                "os_type",
                "pull:tool:pull-error",
                "pull:tool:present",
                "inspect:tool:present"
            ]
        );
    }

    #[tokio::test]
    async fn a_pulled_image_the_daemon_does_not_report_fails() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let results = pull(
            &images.daemon(),
            &[binding("image", ArtifactKind::Image { os: Os::Linux }, "tool:missing")],
        )
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].identity,
            Err("image 'tool:missing' was pulled but the Docker daemon does not report it".into())
        );
        assert_eq!(
            *calls.lock().unwrap(),
            ["connect", "os_type", "pull:tool:missing", "inspect:tool:missing"]
        );
    }

    #[tokio::test]
    async fn an_inspection_error_fails_only_that_image_and_pulling_continues() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let bindings = [
            binding("bad", ArtifactKind::Image { os: Os::Linux }, "tool:inspect-error"),
            binding("good", ArtifactKind::Image { os: Os::Linux }, "tool:present"),
        ];
        let results = pull(&images.daemon(), &bindings).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].identity, Err("inspection failed".into()));
        assert_eq!(results[1].identity, Ok("sha256:tool:present".into()));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "connect",
                "os_type",
                "pull:tool:inspect-error",
                "inspect:tool:inspect-error",
                "pull:tool:present",
                "inspect:tool:present"
            ]
        );
    }

    async fn assert_timeout(reference: &str, timeout: Duration, message: &str, expected_calls: &[&str]) {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let bindings = [
            binding("one", ArtifactKind::Image { os: Os::Linux }, reference),
            binding("file", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image { os: Os::Linux }, "tool:second"),
            binding("three", ArtifactKind::Image { os: Os::Linux }, "tool:third"),
        ];
        let start = tokio::time::Instant::now();
        let results = pull(&images.daemon(), &bindings).await;
        assert_eq!(start.elapsed(), timeout);
        assert_eq!(results.len(), 3);
        for (result, binding) in results.iter().zip([&bindings[0], &bindings[2], &bindings[3]]) {
            assert_eq!(&result.binding, binding);
            assert_eq!(result.identity.as_ref().unwrap_err(), message);
        }
        assert_eq!(*calls.lock().unwrap(), expected_calls);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pull_timeout_fails_the_remaining_images_without_contacting_the_daemon_again() {
        assert_timeout(
            "tool:pull-timeout",
            IMAGE_PULL_TIMEOUT,
            "the Docker daemon did not finish pulling image 'tool:pull-timeout' within 300s",
            &["connect", "os_type", "pull:tool:pull-timeout"],
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_inspection_timeout_after_a_pull_fails_the_remaining_images() {
        assert_timeout(
            "tool:inspect-timeout",
            DOCKER_DAEMON_TIMEOUT,
            "the Docker daemon did not answer within 30s inspecting image 'tool:inspect-timeout'",
            &[
                "connect",
                "os_type",
                "pull:tool:inspect-timeout",
                "inspect:tool:inspect-timeout",
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_connection_failure_fails_every_image() {
        let bindings = [
            binding("one", ArtifactKind::Image { os: Os::Linux }, "tool:first"),
            binding("binary", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image { os: Os::Linux }, "tool:second"),
        ];
        let connections = Arc::new(Mutex::new(0));
        let docker = DockerDaemon::with_connect({
            let connections = connections.clone();
            move || {
                *connections.lock().unwrap() += 1;
                Err("unavailable".into())
            }
        });
        let results = pull(&docker, &bindings).await;
        assert_eq!(*connections.lock().unwrap(), 1);
        assert_eq!(results.len(), 2);
        for (result, binding) in results.iter().zip([&bindings[0], &bindings[2]]) {
            assert_eq!(&result.binding, binding);
            assert_eq!(
                result.identity,
                Err("cannot connect to a Docker daemon: unavailable".into())
            );
        }
    }

    #[tokio::test]
    async fn bindings_without_images_need_no_daemon() {
        let files = [
            binding("binary", ArtifactKind::Binary, "bin/tool"),
            binding(
                "install",
                ArtifactKind::Install {
                    main_binary: "bin/tool",
                },
                "install",
            ),
        ];
        for bindings in [&[][..], &files[..]] {
            assert!(pull(
                &DockerDaemon::with_connect(|| panic!("no images must not connect")),
                bindings
            )
            .await
            .is_empty());
        }
    }

    #[tokio::test]
    async fn an_image_for_another_os_is_not_pulled_and_the_remaining_images_are_pulled() {
        let images = FakeImages::default();
        let bindings = [
            binding("windows", ArtifactKind::Image { os: Os::Windows }, "tool:windows"),
            binding("linux", ArtifactKind::Image { os: Os::Linux }, "tool:present"),
        ];
        let results = pull(&images.daemon(), &bindings).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].binding, bindings[0]);
        assert_eq!(
            results[0].identity,
            Err("the Docker daemon runs linux containers, not windows".into())
        );
        assert_eq!(results[1].binding, bindings[1]);
        assert_eq!(results[1].identity, Ok("sha256:tool:present".into()));
        assert_eq!(
            *images.calls.lock().unwrap(),
            ["connect", "os_type", "pull:tool:present", "inspect:tool:present"]
        );
    }

    #[tokio::test]
    async fn missing_image_references_require_manifest_bindings_without_io() {
        let bindings = [binding("required", ArtifactKind::Image { os: Os::Linux }, "")];
        let docker = DockerDaemon::with_connect(|| panic!("unbound roles must not connect"));
        let results = pull(&docker, &bindings).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].binding, bindings[0]);
        assert_eq!(
            results[0].identity,
            Err("role 'required' requires a binding in an artifact manifest".into())
        );
    }
}
