//! Docker daemon connection with non-standard socket discovery.

use std::{env, path::PathBuf};

use bollard::{Docker, API_DEFAULT_VERSION};
use home::home_dir;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::debug;

/// Default connection timeout, in seconds, matching bollard.
const DEFAULT_TIMEOUT: u64 = 120;

/// The standard Docker socket path on Linux.
const STANDARD_SOCKET: &str = "/var/run/docker.sock";

/// Connect to the Docker daemon, searching non-standard socket locations on macOS.
///
/// Resolution order:
///
/// 1. If `DOCKER_HOST` is set **or** `/var/run/docker.sock` exists on disk, defer to bollard's
///    default connection logic — no extra work needed.
/// 2. Otherwise, try each candidate socket path in order and use the first that exists.
/// 3. If no candidate is found, fall back to `Docker::connect_with_defaults()` so the error
///    message comes from bollard rather than from us.
///
/// # Candidate paths
///
/// These are checked (in order) when the standard socket is absent:
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
/// Returns an error if no reachable Docker daemon can be found. When connecting through a
/// discovered non-standard path, the error includes the path that was attempted.
pub fn connect() -> Result<Docker, GenericError> {
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

    // Nothing found — let bollard try (and fail with its own error message).
    Ok(Docker::connect_with_defaults()?)
}
