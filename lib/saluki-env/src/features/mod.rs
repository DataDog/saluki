//! Feature detection.
//!
//! This module provides helpers for detecting the presence of various "features" in the environment, such as if
//! containerd is running, and so on. Feature detection is useful for knowing what capabilities are available, and what
//! code should or shouldn't be run.
use std::{
    io::ErrorKind,
    net::Shutdown,
    os::unix::fs::FileTypeExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use bitmask_enum::bitmask;
use socket2::{Domain, SockAddr, Socket, Type};
use tracing::debug;

mod containerd;
pub use self::containerd::ContainerdDetector;

mod detector;
pub use self::detector::FeatureDetector;

const CONTAINER_HOST_MOUNT_PATH: &str = "/host";
const SOCKET_CHECK_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Features are distinct markers or indicators of a particular technology or platform that is present.
///
/// In general, features represent the type of environment that the application is running in, such as the Kubernetes
/// feature being present indicating that the application is running in Kubernetes, and so on.
#[bitmask(u16)]
#[bitmask_config(vec_debug)]
pub enum Feature {
    /// Host-mapped procfs.
    ///
    /// This implies that we're in a containerized environment and the host's procfs (`/proc`) has been mapped into the
    /// container using a `/host` prefix, resulting in a `/host/proc` path.
    HostMappedProcfs,

    /// Host-mapped cgroupfs.
    ///
    /// This implies that we're in a containerized environment and the host's cgroupfs (`/sys/fs/cgroup`) has been
    /// mapped into the container using a `/host` prefix, resulting in a `/host/sys/fs/cgroup` path.
    HostMappedCgroupfs,

    /// Containerd.
    Containerd,
}

fn get_env_var_or_empty(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| String::new())
}

fn is_env_var_present(name: &str) -> bool {
    !get_env_var_or_empty(name).is_empty()
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

/// Checks to see if a Unix domain sockets exists, and is reachable, at the given path.
///
/// If there is no file at the given path, or if the file is not of the socket type, `None` is returned. If the socket
/// at the given path is not reachable, `Some(false)` is returned. Otherwise, `Some(true)` is returned.
///
/// Reachability is determined as either being able to connect or having the permissions to do so. We do this because it
/// is likely that if no process is _currently_ listening on the socket, one will likely be in the future.
fn check_unix_socket(path: &Path) -> Option<bool> {
    // Make sure the path exists and is a socket.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) => {
            debug!(socket_path = %path.to_string_lossy(), error = %e, "Failed to get metadata for socket path.");
            return None;
        }
    };
    if !metadata.file_type().is_socket() {
        return None;
    }

    // Check to see if we can connect to the socket.
    //
    // We treat every error other than `PermissionDenied` as a non-failure, with the thought being that if the file
    // exists and is a socket, the process is potentially not listening _yet_, but likely will in the future.
    let socket = match Socket::new_raw(Domain::UNIX, Type::STREAM, None) {
        Ok(socket) => socket,
        Err(e) => {
            debug!(socket_path = %path.to_string_lossy(), error = %e, "Failed to create socket.");
            return Some(false);
        }
    };

    let socket_addr = match SockAddr::unix(path) {
        Ok(socket_addr) => socket_addr,
        Err(e) => {
            debug!(socket_path = %path.to_string_lossy(), error = %e, "Failed to create socket address.");
            return Some(false);
        }
    };

    Some(
        match socket.connect_timeout(&socket_addr, SOCKET_CHECK_CONNECT_TIMEOUT) {
            Ok(_) => {
                // Shutdown the socket, ignoring any errors.
                let _ = socket.shutdown(Shutdown::Both);

                true
            }
            Err(e) => e.kind() != ErrorKind::PermissionDenied,
        },
    )
}

fn find_first_available_unix_socket<I, P>(socket_paths: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    for socket_path in socket_paths {
        let socket_path = socket_path.as_ref();
        let socket_path_str = socket_path.to_string_lossy();

        match check_unix_socket(socket_path) {
            None => {
                debug!(socket_path = %socket_path_str, "No file at socket path or not a socket.");
                continue;
            }
            Some(false) => {
                debug!(socket_path = %socket_path_str, "Found file at socket path but unreachable. (permissions?)");
                continue;
            }
            Some(true) => {
                debug!(socket_path = %socket_path_str, "Found reachable socket.");
                return Some(socket_path.to_path_buf());
            }
        }
    }

    None
}

fn is_running_inside_container() -> bool {
    // `DOCKER_DD_AGENT` is set by the official Datadog Agent container image.
    let is_containerized = is_env_var_present("DOCKER_DD_AGENT");
    if is_containerized {
        debug!("Found non-empty DOCKER_DD_AGENT environment variable. Likely running in a container.");
    } else {
        debug!("Did not find DOCKER_DD_AGENT environment variable. Likely not running in a container.");
    }
    is_containerized
}

fn is_running_inside_docker() -> bool {
    // This file is mounted into a container's filesystem by Docker itself, and implies that we're currently _inside_
    // the Docker runtime. `is_docker_present` detects the presence of the Docker runtime on the host itself, at the OS
    // level.
    file_exists("/.dockerenv")
}

fn with_host_mount_prefixes<I, P>(paths: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: AsRef<str>,
{
    let mut prefixes = vec![];

    // Add the provided paths as they are, joined with a leading slash, which will absolute-ize them if they are
    // relative. If we're in a containerized environment, we'll also add a "/host"-anchored version of each path.
    //
    // We do this to avoid clobbering paths in the container, such that if we mount a host path into the container, such
    // as "/var/run", it ends up as "/host/var/run" in the container, which doesn't shadow the existing "/var/run".
    let root = PathBuf::from("/");
    let host_root = PathBuf::from(CONTAINER_HOST_MOUNT_PATH);
    let is_containerized = is_running_inside_container();

    for path in paths {
        let path = path.as_ref().trim_start_matches('/');

        prefixes.push(root.join(path));

        if is_containerized {
            prefixes.push(host_root.join(path));
        }
    }

    prefixes
}

fn has_host_mapped_procfs() -> bool {
    let path = PathBuf::from(CONTAINER_HOST_MOUNT_PATH).join("proc");
    is_running_inside_container() && file_exists(&path)
}

fn has_host_mapped_cgroupfs() -> bool {
    let path = PathBuf::from(CONTAINER_HOST_MOUNT_PATH).join("sys/fs/cgroup");
    is_running_inside_container() && file_exists(&path)
}
