//! Images the Docker daemon must have before a container can be created from them.

use bollard::{query_parameters::CreateImageOptions, Docker};
use futures::TryStreamExt as _;
use saluki_error::{generic_error, GenericError};
use tokio::time::{timeout_at, Instant};
use tracing::debug;

use crate::removal::is_not_found;

/// Pulls `image` unless the Docker daemon already has it.
///
/// # Errors
///
/// Returns an error if Docker cannot inspect the image, if the pull fails, or if `deadline` passes first.
pub async fn ensure_image(docker: &Docker, image: &str, deadline: Instant) -> Result<(), GenericError> {
    timeout_at(deadline, ensure_image_unbounded(docker, image))
        .await
        .map_err(|_| generic_error!("The deadline passed before image '{}' was available.", image))?
}

async fn ensure_image_unbounded(docker: &Docker, image: &str) -> Result<(), GenericError> {
    match docker.inspect_image(image).await {
        Ok(_) => return Ok(()),
        Err(e) if is_not_found(&e) => {}
        Err(e) => return Err(generic_error!("Failed to inspect image '{}': {}", image, e)),
    }

    debug!("Pulling image '{}'...", image);
    let options = CreateImageOptions {
        from_image: Some(image.to_string()),
        // Without a tag, Docker pulls every tag.
        tag: (!names_tag_or_digest(image)).then(|| "latest".to_string()),
        ..Default::default()
    };
    docker
        .create_image(Some(options), None, None)
        .try_for_each(|_| async { Ok(()) })
        .await
        .map_err(|e| generic_error!("Failed to pull image '{}': {}", image, e))
}

/// Whether `image` names a tag or a digest. A colon before the last slash is a registry port, not a tag.
fn names_tag_or_digest(image: &str) -> bool {
    let name = image.rsplit_once('/').map_or(image, |(_, name)| name);
    name.contains([':', '@'])
}

/// The image of the tests that need a Docker daemon.
///
/// CI cannot reach Docker Hub, so this comes from the internal registry's mirror. It is multi-arch and has a shell.
#[cfg(any(test, feature = "test-util"))]
pub const DAEMON_TEST_IMAGE: &str = "registry.ddbuild.io/images/mirror/alpine:3.20";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_or_digest_follows_the_last_slash() {
        assert!(!names_tag_or_digest("alpine"));
        assert!(!names_tag_or_digest("localhost:5000/img"));
        assert!(names_tag_or_digest("localhost:5000/img:1.0"));
        assert!(names_tag_or_digest("alpine@sha256:0123"));
    }
}
