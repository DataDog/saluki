use std::path::PathBuf;

use saluki_config::GenericConfiguration;
use tracing::{debug, error};

use super::{is_running_inside_docker, path_contains, path_empty};
use crate::features::{find_first_available_unix_socket, with_host_mount_prefixes};

const DEFAULT_CONTAINERD_SOCKET_PATH_LINUX: &str = "/var/run/containerd/containerd.sock";

/// Helper type for detecting if containerd is available.
pub struct ContainerdDetector;

impl ContainerdDetector {
    /// Tries to detect the containerd gRPC socket path.
    ///
    /// The socket path can be specified in the configuration, or if it's not present, default paths will be checked for
    /// the presence of the socket path.
    ///
    /// If the socket path is configured or detected, and is a valid Unix domain socket, `Some` is returned with the
    /// socket path. Otherwise, `None` is returned.
    pub fn detect_grpc_socket_path(config: &GenericConfiguration) -> Option<PathBuf> {
        // Try and read the socket path from either the configuration, or if it's not present there, from the possible
        // default paths we would expect it to be listening at.
        let detected_socket_path = match config.try_get_typed::<PathBuf>("cri_socket_path") {
            Ok(Some(cri_socket_path)) => Some(cri_socket_path),
            Ok(None) => {
                if is_running_inside_docker() {
                    None
                } else {
                    debug!("Containerd socket path (`cri_socket_path`) not present in configuration. Trying to detect at default paths...");

                    let default_socket_paths = with_host_mount_prefixes([DEFAULT_CONTAINERD_SOCKET_PATH_LINUX]);
                    find_first_available_unix_socket(default_socket_paths)
                }
            }
            Err(e) => {
                error!(error = %e, "Value for `cri_socket_path` could not be parsed as a valid path.");
                None
            }
        }?;

        // If the path isn't empty, and it contains "containerd", we can assume it's the containerd socket.
        if !path_empty(&detected_socket_path) && path_contains(&detected_socket_path, "containerd") {
            Some(detected_socket_path)
        } else {
            None
        }
    }
}
