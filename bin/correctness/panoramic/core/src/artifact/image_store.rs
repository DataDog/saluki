//! Shared Docker access for artifact verification and pulling.

use airlock::docker::DOCKER_DAEMON_TIMEOUT;
use async_trait::async_trait;

#[async_trait]
pub(super) trait ImageStore: Send + Sync {
    async fn image_id(&self, reference: &str) -> Result<Option<String>, String>;
    async fn pull(&self, reference: &str) -> Result<(), String>;
}

struct DockerStore(airlock::docker::Docker);

#[async_trait]
impl ImageStore for DockerStore {
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

/// An outer error means the daemon timed out; an inner error means inspection failed.
pub(super) async fn image_id_within_deadline(
    images: &dyn ImageStore, reference: &str,
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

pub(super) fn connect() -> Result<Box<dyn ImageStore>, String> {
    airlock::docker::connect()
        .map(|docker| Box::new(DockerStore(docker)) as Box<dyn ImageStore>)
        .map_err(|error| format!("{error:#}"))
}
