//! Docker daemon connection, image inspection and pull helpers.

use std::time::Duration;
#[cfg(unix)]
use std::{env, path::PathBuf};

pub use bollard::Docker;
#[cfg(unix)]
use bollard::API_DEFAULT_VERSION;
use bollard::{
    errors::Error,
    models::{CreateImageInfo, ImageInspect},
    query_parameters::CreateImageOptions,
};
use futures::{Stream, StreamExt as _};
#[cfg(unix)]
use home::home_dir;
#[cfg(unix)]
use saluki_error::ErrorContext as _;
use saluki_error::{generic_error, GenericError};
#[cfg(unix)]
use tracing::debug;
use tracing::trace;

/// Response deadline for daemon availability and artifact checks.
pub const DOCKER_DAEMON_TIMEOUT: Duration = Duration::from_secs(30);

/// Default connection timeout, in seconds, matching bollard.
#[cfg(unix)]
const DEFAULT_TIMEOUT: u64 = 120;

/// The standard Docker socket path on Linux.
#[cfg(unix)]
const STANDARD_SOCKET: &str = "/var/run/docker.sock";

/// Connects to the Docker daemon.
///
/// On Windows, this uses Bollard's platform default connection logic.
///
/// On Unix hosts, this searches non-standard socket locations before falling back to Bollard's default connection logic.
/// Resolution order:
///
/// 1. If `DOCKER_HOST` is set **or** `/var/run/docker.sock` exists on disk, defer to Bollard's default connection
///    logic.
/// 2. Otherwise, try each candidate socket path in order and use the first that exists.
/// 3. If no candidate is found, fall back to `Docker::connect_with_defaults()` so the error message comes from Bollard.
///
/// # Unix candidate paths
///
/// These are checked in order when the standard socket is absent:
///
/// | Path | Environment |
/// |------|-------------|
/// | `$HOME/.docker/run/docker.sock` | Docker Desktop |
/// | `$HOME/.orbstack/run/docker.sock` | OrbStack |
/// | `$HOME/.rd/docker.sock` | Rancher Desktop |
/// | `$HOME/.lima/default/sock/docker.sock` | Lima (default instance) |
/// | `$HOME/.lima/docker/sock/docker.sock` | Lima (docker instance) |
/// | `$HOME/.colima/default/docker.sock` | Colima |
/// | `$HOME/.colima/docker/docker.sock` | Colima (docker profile) |
/// | `$HOME/.local/share/containers/podman/machine/podman.sock` | Podman Desktop (macOS) |
///
/// # Errors
///
/// Returns an error if no reachable Docker daemon can be found. When connecting through a discovered non-standard Unix
/// socket path, the error includes the path that was attempted.
pub fn connect() -> Result<Docker, GenericError> {
    connect_inner()
}

/// Inspects a local image, returning its ID or `None` if the daemon has no such image.
///
/// # Errors
///
/// Returns an error if inspection fails or the daemon's response has no image ID.
pub async fn image_id(docker: &Docker, reference: &str) -> Result<Option<String>, GenericError> {
    inspected_image_id(reference, docker.inspect_image(reference).await)
}

/// Pulls an image, reading progress until completion or the first error.
///
/// A name without a tag or digest pulls `latest`, rather than every tag as the Engine API would.
/// Registry access is unauthenticated. Callers must set a deadline.
///
/// # Errors
///
/// Returns an error if the daemon rejects the pull or reports an error in the progress stream.
pub async fn pull_image(docker: &Docker, reference: &str) -> Result<(), GenericError> {
    finish_pull(docker.create_image(Some(pull_options(reference)), None, None)).await
}

fn pull_options(reference: &str) -> CreateImageOptions {
    let last = reference.rsplit_once('/').map_or(reference, |(_, last)| last);
    let name_only = !reference.contains('@') && !last.contains(':');
    CreateImageOptions {
        from_image: Some(reference.to_string()),
        tag: name_only.then(|| "latest".to_string()),
        ..Default::default()
    }
}

async fn finish_pull(stream: impl Stream<Item = Result<CreateImageInfo, Error>>) -> Result<(), GenericError> {
    futures::pin_mut!(stream);
    while let Some(progress) = stream.next().await {
        trace!(?progress, "Image pull progress.");
        progress?;
    }
    Ok(())
}

fn inspected_image_id(reference: &str, response: Result<ImageInspect, Error>) -> Result<Option<String>, GenericError> {
    match response {
        Ok(image) => image
            .id
            .map(Some)
            .ok_or_else(|| generic_error!("image '{reference}' has no ID")),
        Err(Error::DockerResponseServerError { status_code: 404, .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn connect_inner() -> Result<Docker, GenericError> {
    Ok(Docker::connect_with_defaults()?)
}

#[cfg(unix)]
fn connect_inner() -> Result<Docker, GenericError> {
    // Fast path: DOCKER_HOST is explicitly configured, or the standard socket exists.
    if env::var("DOCKER_HOST").is_ok() || PathBuf::from(STANDARD_SOCKET).exists() {
        return Ok(Docker::connect_with_defaults()?);
    }

    // Build candidate list from well-known non-standard locations.
    if let Some(home) = home_dir() {
        let candidates = [
            home.join(".docker/run/docker.sock"),
            home.join(".orbstack/run/docker.sock"),
            home.join(".rd/docker.sock"),
            home.join(".lima/default/sock/docker.sock"),
            home.join(".lima/docker/sock/docker.sock"),
            home.join(".colima/default/docker.sock"),
            home.join(".colima/docker/docker.sock"),
            home.join(".local/share/containers/podman/machine/podman.sock"),
        ];

        for path in &candidates {
            if path.exists() {
                let path = path.display();
                debug!("using non-standard Docker socket: {path}");
                return Docker::connect_with_unix(&path.to_string(), DEFAULT_TIMEOUT, API_DEFAULT_VERSION)
                    .with_error_context(|| format!("Failed to connect to Docker using socket discovered at {path}"));
            }
        }
    }

    // Nothing found: let bollard try (and fail with its own error message).
    Ok(Docker::connect_with_defaults()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_without_a_tag_or_digest_pulls_latest() {
        for (reference, tag) in [
            ("alpine", Some("latest")),
            ("library/alpine", Some("latest")),
            ("localhost:5000/tool", Some("latest")),
            ("registry.example/a/b", Some("latest")),
            ("alpine:3", None),
            ("localhost:5000/tool:1", None),
            ("tool@sha256:abc", None),
            ("registry.example/a/b:1", None),
        ] {
            let options = pull_options(reference);
            assert_eq!(options.from_image.as_deref(), Some(reference));
            assert_eq!(options.tag.as_deref(), tag, "{reference}");
        }
    }

    #[tokio::test]
    async fn a_pull_stream_that_ends_without_an_error_succeeds() {
        let progress = ["Pulling", "Downloading", "Complete"].map(|status| {
            Ok(CreateImageInfo {
                status: Some(status.into()),
                ..Default::default()
            })
        });
        assert!(finish_pull(futures::stream::iter(progress)).await.is_ok());
        assert!(finish_pull(futures::stream::empty()).await.is_ok());
    }

    #[tokio::test]
    async fn a_pull_fails_on_the_first_stream_error_and_reads_no_further() {
        let mut consumed = 0;
        let stream = futures::stream::iter([
            Ok(CreateImageInfo::default()),
            Err(Error::DockerStreamError {
                error: "layer download failed".into(),
            }),
            Ok(CreateImageInfo::default()),
        ])
        .inspect(|_| consumed += 1);
        let error = finish_pull(stream).await.unwrap_err().to_string();
        assert!(error.contains("layer download failed"), "{error}");
        assert_eq!(consumed, 2);
    }

    #[tokio::test]
    async fn a_pull_the_daemon_rejects_reports_its_status_and_message() {
        let error = finish_pull(futures::stream::iter([Err(Error::DockerResponseServerError {
            status_code: 404,
            message: "pull access denied for local/tool".into(),
        })]))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("404"), "{error}");
        assert!(error.contains("pull access denied for local/tool"), "{error}");
    }

    #[test]
    fn image_inspection_treats_only_a_404_as_missing() {
        assert_eq!(
            inspected_image_id(
                "tool:latest",
                Err(Error::DockerResponseServerError {
                    status_code: 404,
                    message: "image missing".into()
                }),
            )
            .unwrap(),
            None
        );
        for status_code in [401, 403, 500] {
            let message = inspected_image_id(
                "tool:latest",
                Err(Error::DockerResponseServerError {
                    status_code,
                    message: "inspection failed".into(),
                }),
            )
            .unwrap_err()
            .to_string();
            assert!(message.contains("inspection failed"), "{message}");
            assert!(message.contains(&status_code.to_string()), "{message}");
        }
    }

    #[test]
    fn image_inspection_requires_an_id_and_preserves_it() {
        assert_eq!(
            inspected_image_id(
                "tool:latest",
                Ok(ImageInspect {
                    id: Some("sha256:abc".into()),
                    ..Default::default()
                })
            )
            .unwrap(),
            Some("sha256:abc".into())
        );
        assert_eq!(
            inspected_image_id("tool:latest", Ok(ImageInspect::default()))
                .unwrap_err()
                .to_string(),
            "image 'tool:latest' has no ID"
        );
    }
}
