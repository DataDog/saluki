//! Fixtures for artifact verification tests.

use std::path::Path;

/// Writes `b"tool"`, creating parent directories and setting Unix execute permissions.
///
/// # Panics
///
/// Panics if `path` has no parent or a filesystem operation fails.
pub fn write_executable(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"tool").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
