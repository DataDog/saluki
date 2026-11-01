//! Pulls bound images and records their identities from the Docker daemon.

use std::time::Duration;

use tracing::info;

use super::{
    image_store::{connect, image_id_within_deadline, ImageStore},
    ArtifactBinding, ArtifactKind, ArtifactVerification,
};

// Leave time within the ten-minute correctness job to report a stuck image.
const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Pulls only image bindings, in input order, including images already present, and records their IDs.
/// A pull or inspection timeout fails all remaining images; other failures affect only that image.
pub async fn pull(bindings: &[ArtifactBinding]) -> Vec<ArtifactVerification> {
    pull_with(bindings, connect).await
}

async fn pull_with(
    bindings: &[ArtifactBinding], connect: impl FnOnce() -> Result<Box<dyn ImageStore>, String>,
) -> Vec<ArtifactVerification> {
    let mut bindings = bindings
        .iter()
        .filter(|binding| binding.role.kind == ArtifactKind::Image)
        .peekable();
    if bindings.peek().is_none() {
        return Vec::new();
    }
    let mut store = connect().map_err(|error| format!("cannot connect to a Docker daemon: {error}"));
    let mut results = Vec::new();
    for binding in bindings {
        let reference = &binding.reference;
        let identity = match &store {
            Ok(images) => {
                info!("Pulling image '{reference}' for role '{}'...", binding.role.name);
                match pull_one(images.as_ref(), reference).await {
                    Ok(identity) => Ok(identity),
                    Err(Failure::Image(error)) => Err(error),
                    Err(Failure::Daemon(error)) => {
                        store = Err(error.clone());
                        Err(error)
                    }
                }
            }
            Err(error) => Err(error.clone()),
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

async fn pull_one(images: &dyn ImageStore, reference: &str) -> Result<String, Failure> {
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
    use crate::artifact::{ArtifactRole, BindingSource};

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

    #[derive(Default)]
    struct FakeImages {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ImageStore for FakeImages {
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
            binding("z", ArtifactKind::Image, "tool:first"),
            binding("binary", ArtifactKind::Binary, "bin/tool"),
            binding("a", ArtifactKind::Image, "tool:second"),
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
        let mut connections = 0;
        let results = pull_with(&bindings, || {
            connections += 1;
            Ok(Box::new(images))
        })
        .with_subscriber(subscriber)
        .await;
        let log = std::fs::read_to_string(output.path()).unwrap();
        assert!(log.contains("Pulling image 'tool:first' for role 'z'..."), "{log}");
        assert!(log.contains("Pulling image 'tool:second' for role 'a'..."), "{log}");
        assert_eq!(connections, 1);
        assert_eq!(results.len(), 2);
        for (result, binding) in results.iter().zip([&bindings[0], &bindings[2]]) {
            assert_eq!(&result.binding, binding);
            assert_eq!(result.identity, Ok(format!("sha256:{}", binding.reference)));
        }
        assert_eq!(
            *calls.lock().unwrap(),
            [
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
        let results = pull_with(&[binding("image", ArtifactKind::Image, "tool:present")], || {
            Ok(Box::new(images))
        })
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].identity, Ok("sha256:tool:present".into()));
        assert_eq!(*calls.lock().unwrap(), ["pull:tool:present", "inspect:tool:present"]);
    }

    #[tokio::test]
    async fn a_failed_pull_fails_that_image_even_if_a_local_copy_exists_and_the_rest_are_pulled() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let bindings = [
            binding("bad", ArtifactKind::Image, "tool:pull-error"),
            binding("good", ArtifactKind::Image, "tool:present"),
        ];
        let results = pull_with(&bindings, || Ok(Box::new(images))).await;
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].identity,
            Err("cannot pull image 'tool:pull-error': pull access denied".into())
        );
        assert_eq!(results[1].identity, Ok("sha256:tool:present".into()));
        assert_eq!(
            *calls.lock().unwrap(),
            ["pull:tool:pull-error", "pull:tool:present", "inspect:tool:present"]
        );
    }

    #[tokio::test]
    async fn a_pulled_image_the_daemon_does_not_report_fails() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let results = pull_with(&[binding("image", ArtifactKind::Image, "tool:missing")], || {
            Ok(Box::new(images))
        })
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].identity,
            Err("image 'tool:missing' was pulled but the Docker daemon does not report it".into())
        );
        assert_eq!(*calls.lock().unwrap(), ["pull:tool:missing", "inspect:tool:missing"]);
    }

    #[tokio::test]
    async fn an_inspection_error_fails_only_that_image_and_pulling_continues() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let bindings = [
            binding("bad", ArtifactKind::Image, "tool:inspect-error"),
            binding("good", ArtifactKind::Image, "tool:present"),
        ];
        let results = pull_with(&bindings, || Ok(Box::new(images))).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].identity, Err("inspection failed".into()));
        assert_eq!(results[1].identity, Ok("sha256:tool:present".into()));
        assert_eq!(
            *calls.lock().unwrap(),
            [
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
            binding("one", ArtifactKind::Image, reference),
            binding("file", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image, "tool:second"),
            binding("three", ArtifactKind::Image, "tool:third"),
        ];
        let start = tokio::time::Instant::now();
        let results = pull_with(&bindings, || Ok(Box::new(images))).await;
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
            &["pull:tool:pull-timeout"],
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_inspection_timeout_after_a_pull_fails_the_remaining_images() {
        assert_timeout(
            "tool:inspect-timeout",
            DOCKER_DAEMON_TIMEOUT,
            "the Docker daemon did not answer within 30s inspecting image 'tool:inspect-timeout'",
            &["pull:tool:inspect-timeout", "inspect:tool:inspect-timeout"],
        )
        .await;
    }

    #[tokio::test]
    async fn a_connection_failure_fails_every_image() {
        let bindings = [
            binding("one", ArtifactKind::Image, "tool:first"),
            binding("binary", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image, "tool:second"),
        ];
        let mut connections = 0;
        let results = pull_with(&bindings, || {
            connections += 1;
            Err("unavailable".into())
        })
        .await;
        assert_eq!(connections, 1);
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
    async fn bindings_without_images_need_no_image_store() {
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
            assert!(pull_with(bindings, || panic!("no images must not connect"))
                .await
                .is_empty());
        }
    }
}
