use std::{
    io::ErrorKind,
    net::Shutdown,
    os::unix::fs::FileTypeExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_config::GenericConfiguration;
use socket2::{Domain, SockAddr, Socket, Type};
use tracing::{debug, info};

use super::Feature;

const DEFAULT_DOCKER_SOCKET_PATH_LINUX: &str = "/var/run/docker.sock";
const DEFAULT_CONTAINERD_SOCKET_PATH_LINUX: &str = "/var/run/containerd/containerd.sock";
const DEFAULT_CRIO_SOCKET_PATH_LINUX: &str = "/var/run/crio/crio.sock";
const DEFAULT_PODMAN_CONTAINER_STORAGE_PATH_LINUX: &str = "/var/lib/containers/storage";
const DOCKER_HOST_MOUNT_PATH: &str = "/host";

/// Detects workload features.
///
/// The feature detector is used to determine which features are present in the workload, such as if the application is
/// running in a Kubernetes environment or directly on Docker. This information is used to drive the collection of
/// workload metadata that powers entity tagging.
///
/// `FeatureDetector` can be used in an automatic detection mode, where it will attempt to detect all features that are
/// present, or it can be used to assert that a specific feature is present for scenarios where the feature is required,
/// and the workload is already known upfront.
#[derive(Clone)]
pub struct FeatureDetector {
    detected_features: Feature,
}

impl FeatureDetector {
    /// Creates a new `FeatureDetector` that checks for all possible features.
    pub fn automatic(config: &GenericConfiguration) -> Self {
        Self::for_feature(config, Feature::all())
    }

    /// Creates a new `FeatureDetector` that checks for the given feature(s).
    ///
    /// Multiple features can be checked for by combining the feature variants in a bitwise OR fashion, such as
    /// `Feature::Kubernetes | Feature::EKSFargate`.
    pub fn for_feature(config: &GenericConfiguration, feature_mask: Feature) -> Self {
        let detected_features = Self::detect_features(config, feature_mask);

        Self { detected_features }
    }

    /// Checks if the given feature was detected and is available.
    pub fn is_feature_available(&self, feature: Feature) -> bool {
        self.detected_features.contains(feature)
    }

    fn detect_features(config: &GenericConfiguration, feature_mask: Feature) -> Feature {
        let mut detected_features = Feature::none();

        if feature_mask.contains(Feature::CloudFoundry) && config.get_typed_or_default::<bool>("cloud_foundry") {
            info!("Detected configuration for running in Cloud Foundry.");
            detected_features |= Feature::CloudFoundry;
        }

        if feature_mask.contains(Feature::Containerd) && is_containerd_present(config) {
            info!("Detected presence of containerd.");
            detected_features |= Feature::Containerd;
        }

        if feature_mask.contains(Feature::CRI) && is_cri_present(config) {
            info!("Detected presence of CRI-compatible container runtime.");
            detected_features |= Feature::CRI;
        }

        if feature_mask.contains(Feature::Docker) && is_docker_present() {
            info!("Detected presence of Docker.");
            detected_features |= Feature::Docker;
        }

        if feature_mask.contains(Feature::ECS) && is_ecs_present() {
            info!("Detected presence of ECS.");
            detected_features |= Feature::ECS;
        }

        if feature_mask.contains(Feature::ECSFargate) && is_ecs_fargate_present() {
            info!("Detected presence of ECS on Fargate.");
            detected_features |= Feature::ECSFargate;
        }

        if feature_mask.contains(Feature::EKSFargate) && config.get_typed_or_default::<bool>("eks_fargate") {
            info!("Detected configuration for running in EKS on Fargate.");
            detected_features |= Feature::EKSFargate;
            detected_features |= Feature::Kubernetes;
        }

        if feature_mask.contains(Feature::Kubernetes) && is_kubernetes_present() {
            info!("Detected presence of Kubernetes.");
            detected_features |= Feature::Kubernetes;
        }

        if feature_mask.contains(Feature::Podman) && is_podman_present(config) {
            info!("Detected presence of Podman.");
            detected_features |= Feature::Podman;
        }

        detected_features
    }
}

fn get_env_var_or_empty(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| String::new())
}

fn is_env_var_empty(name: &str) -> bool {
    get_env_var_or_empty(name).is_empty()
}

fn file_exists<P>(path: P) -> bool
where
    P: AsRef<Path>,
{
    std::fs::metadata(path).is_ok()
}

fn path_empty<P>(path: P) -> bool
where
    P: AsRef<Path>,
{
    path.as_ref().as_os_str().is_empty()
}

fn path_contains<P>(path: P, fragment: &str) -> bool
where
    P: AsRef<Path>,
{
    path.as_ref().to_string_lossy().contains(fragment)
}

fn check_unix_socket(path: &Path) -> (bool, bool) {
    const SOCKET_CHECK_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

    // Make sure the path exists and is a socket.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) => {
            debug!(socket_path = %path.to_string_lossy(), error = %e, "Failed to get metadata for socket path.");
            return (false, false);
        }
    };
    if !metadata.file_type().is_socket() {
        return (false, false);
    }

    // Check to see if we can connect to the socket.
    //
    // We treat every error other than `PermissionDenied` as a non-failure, with the thought being that if the file
    // exists and is a socket, the process is potentially not listening _yet_, but likely will in the future.
    let socket = match Socket::new_raw(Domain::UNIX, Type::STREAM, None) {
        Ok(socket) => socket,
        Err(e) => {
            debug!(socket_path = %path.to_string_lossy(), error = %e, "Failed to create socket.");
            return (true, false);
        }
    };

    let socket_addr = match SockAddr::unix(path) {
        Ok(socket_addr) => socket_addr,
        Err(e) => {
            debug!(socket_path = %path.to_string_lossy(), error = %e, "Failed to create socket address.");
            return (true, false);
        }
    };

    match socket.connect_timeout(&socket_addr, SOCKET_CHECK_CONNECT_TIMEOUT) {
        Ok(_) => {
            // Shutdown the socket, ignoring any errors.
            let _ = socket.shutdown(Shutdown::Both);

            (true, true)
        }
        Err(e) => (true, e.kind() != ErrorKind::PermissionDenied),
    }
}

fn is_containerized() -> bool {
    // `DOCKER_DD_AGENT` is set by the official Datadog Agent container image.
    !is_env_var_empty("DOCKER_DD_AGENT")
}

fn is_kubernetes_present() -> bool {
    !is_env_var_empty("KUBERNETES_SERVICE_HOST") || !is_env_var_empty("KUBERNETES")
}

fn is_docker_present() -> bool {
    if !is_env_var_empty("DOCKER_HOST") {
        debug!("Found non-empty DOCKER_HOST environment variable.");
        return true;
    }

    let socket_paths = default_docker_socket_paths();
    for socket_path in socket_paths {
        let (socket_exists, socket_listening) = check_unix_socket(&socket_path);
        if socket_exists && !socket_listening {
            debug!(socket_path = %socket_path.to_string_lossy(), "Found Docker socket but unreachable. (permissions?)");
            continue;
        }

        if socket_exists && socket_listening {
            debug!(socket_path = %socket_path.to_string_lossy(), "Found likely Docker socket.");
            return true;
        }

        // TODO: In the Datadog Agent, they take the tack of backfilling the configuration with the discovered socket
        // path if it wasn't set in the specific environment variable. It seems likely that we will want/need to do the
        // same, since that socket path will be required for any Docker calls in a collector, but it's not clear to me
        // how sane it is to pass in a configuration object that allows for mutation.
    }

    false
}

fn is_cri_present(config: &GenericConfiguration) -> bool {
    is_kubernetes_present() && is_cri_containerd_present(config, false)
}

fn is_containerd_present(config: &GenericConfiguration) -> bool {
    is_cri_containerd_present(config, true)
}

fn is_cri_containerd_present(config: &GenericConfiguration, check_for_containerd: bool) -> bool {
    let mut cri_socket_path = config.get_typed_or_default::<PathBuf>("cri_socket_path");
    if path_empty(&cri_socket_path) && !is_docker_runtime_present() {
        let socket_paths = default_cri_socket_paths();
        for socket_path in socket_paths {
            let (socket_exists, socket_listening) = check_unix_socket(&socket_path);
            if socket_exists && !socket_listening {
                debug!(socket_path = %socket_path.to_string_lossy(), "Found CRI socket but unreachable. (permissions?)");
                continue;
            }

            if socket_exists && socket_listening {
                debug!(socket_path = %socket_path.to_string_lossy(), "Found likely CRI socket.");
                cri_socket_path = socket_path;
                break;
            }
        }

        // TODO: In the Datadog Agent, they take the tack of backfilling the configuration with the discovered socket
        // path if it wasn't set in the configuration. It seems likely that we will want/need to do the same, since that
        // socket path will be required for any CRI calls in a collector, but it's not clear to me how sane it is to
        // pass in a configuration object that allows for mutation.
    }

    if !path_empty(&cri_socket_path) {
        if check_for_containerd {
            if !path_contains(&cri_socket_path, "containerd") {
                debug!("Found CRI socket but socket path did not contain 'containerd'. Not likely to be containerd.");
                false
            } else {
                true
            }
        } else {
            true
        }
    } else {
        false
    }
}

fn is_docker_runtime_present() -> bool {
    // This file is mounted into a container's filesystem by Docker itself, and implies that we're currently _inside_
    // the Docker runtime. `is_docker_present` detects the presence of the Docker runtime on the host itself, at the OS
    // level.
    file_exists("/.dockerenv")
}

fn is_ecs_present() -> bool {
    if get_env_var_or_empty("AWS_EXECUTION_ENV") == "AWS_ECS_EC2" {
        return true;
    }

    if is_ecs_fargate_present() {
        return false;
    }

    if !is_env_var_empty("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        || !is_env_var_empty("ECS_CONTAINER_METADATA_URI")
        || !is_env_var_empty("ECS_CONTAINER_METADATA_URI_V4")
    {
        return true;
    }

    file_exists("/etc/ecs/ecs.config")
}

fn is_ecs_fargate_present() -> bool {
    !is_env_var_empty("ECS_FARGATE") || get_env_var_or_empty("AWS_EXECUTION_ENV") == "AWS_ECS_FARGATE"
}

fn is_podman_present(config: &GenericConfiguration) -> bool {
    if config.get_typed_or_default::<String>("podman_db_path") != "" {
        return true;
    }

    let storage_paths = default_podman_paths();
    for storage_path in storage_paths {
        if file_exists(storage_path) {
            return true;
        }
    }

    false
}

fn host_mount_prefixes() -> Vec<PathBuf> {
    let mut prefixes = vec![PathBuf::from("")];

    if is_containerized() {
        prefixes.push(PathBuf::from(DOCKER_HOST_MOUNT_PATH));
    }

    prefixes
}

fn default_docker_socket_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let prefixes = host_mount_prefixes();
    for prefix in prefixes {
        paths.push(prefix.join(DEFAULT_DOCKER_SOCKET_PATH_LINUX));
    }

    paths
}

fn default_cri_socket_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let prefixes = host_mount_prefixes();
    for prefix in prefixes {
        paths.push(prefix.join(DEFAULT_CONTAINERD_SOCKET_PATH_LINUX));
        paths.push(prefix.join(DEFAULT_CRIO_SOCKET_PATH_LINUX));
    }

    paths
}

fn default_podman_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let prefixes = host_mount_prefixes();
    for prefix in prefixes {
        paths.push(prefix.join(DEFAULT_PODMAN_CONTAINER_STORAGE_PATH_LINUX));
    }

    paths
}
