//! Docker daemon connection helpers.

#[cfg(unix)]
use std::{env, path::PathBuf};

use bollard::Docker;
#[cfg(unix)]
use bollard::API_DEFAULT_VERSION;
#[cfg(unix)]
use home::home_dir;
#[cfg(unix)]
use saluki_error::ErrorContext as _;
use saluki_error::GenericError;
#[cfg(unix)]
use tracing::debug;

/// Default connection timeout, in seconds, matching bollard.
#[cfg(unix)]
const DEFAULT_TIMEOUT: u64 = 120;

/// The standard Docker socket path on Linux.
#[cfg(unix)]
const STANDARD_SOCKET: &str = "/var/run/docker.sock";

/// Connects to the Docker daemon.
///
/// On Unix hosts, this searches common non-standard Docker socket locations when `DOCKER_HOST` is unset and the
/// standard `/var/run/docker.sock` path is absent. On Windows, this uses Bollard's platform default connection logic.
///
/// # Errors
///
/// Returns an error if no reachable Docker daemon can be found. When connecting through a discovered non-standard Unix
/// socket path, the error includes the path that was attempted.
pub fn connect() -> Result<Docker, GenericError> {
    connect_inner()
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
