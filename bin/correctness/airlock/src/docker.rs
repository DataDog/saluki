//! Docker daemon connection and image inspection helpers.

use std::time::Duration;
#[cfg(unix)]
use std::{env, path::PathBuf};

pub use bollard::Docker;
#[cfg(unix)]
use bollard::API_DEFAULT_VERSION;
use bollard::{errors::Error, models::ImageInspect};
#[cfg(unix)]
use home::home_dir;
#[cfg(unix)]
use saluki_error::ErrorContext as _;
use saluki_error::{generic_error, GenericError};
#[cfg(unix)]
use tracing::debug;

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
