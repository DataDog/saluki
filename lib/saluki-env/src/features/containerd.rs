use std::path::PathBuf;

#[cfg(unix)]
use tracing::debug;

#[cfg(unix)]
use super::{
    find_first_available_unix_socket, is_running_inside_docker, path_contains, path_empty, with_host_mount_prefixes,
};

#[cfg(unix)]
const DEFAULT_CONTAINERD_SOCKET_PATH_LINUX: &str = "/var/run/containerd/containerd.sock";

/// Helper type for detecting if containerd is available.
pub struct ContainerdDetector;

impl ContainerdDetector {
    /// Tries to detect the containerd gRPC socket path.
    ///
    /// When a socket path is configured, it is used as-is. Otherwise, default paths are checked for the presence of the
    /// socket path.
    ///
    /// If the socket path is configured or detected, and is a valid Unix domain socket, `Some` is returned with the
    /// socket path. Otherwise, `None` is returned.
    #[cfg(unix)]
    pub fn detect_grpc_socket_path(configured_socket_path: Option<PathBuf>) -> Option<PathBuf> {
        let detected_socket_path = match configured_socket_path {
            Some(socket_path) => Some(socket_path),
            None => {
                if is_running_inside_docker() {
                    None
                } else {
                    debug!("No containerd socket path configured. Trying to detect at default paths...");

                    let default_socket_paths = with_host_mount_prefixes([DEFAULT_CONTAINERD_SOCKET_PATH_LINUX]);
                    find_first_available_unix_socket(default_socket_paths)
                }
            }
        }?;

        // If the path isn't empty, and it contains "containerd", we can assume it's the containerd socket.
        if !path_empty(&detected_socket_path) && path_contains(&detected_socket_path, "containerd") {
            debug!(socket_path = %detected_socket_path.to_string_lossy(), "Detected containerd socket path.");
            Some(detected_socket_path)
        } else {
            None
        }
    }

    /// Returns `None` because containerd Unix socket detection isn't supported on this platform.
    #[cfg(not(unix))]
    pub fn detect_grpc_socket_path(_: Option<PathBuf>) -> Option<PathBuf> {
        None
    }
}
