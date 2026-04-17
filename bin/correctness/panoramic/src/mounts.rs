//! Read-only file overlays applied to every target container panoramic launches.
//!
//! Panoramic's `--mounts-dir` is treated as a virtual container root: every regular file
//! found beneath it is bind-mounted read-only at the same path inside the target
//! container. For example, `<mounts-dir>/etc/cont-init.d/00-foo.sh` is mounted at
//! `/etc/cont-init.d/00-foo.sh`.
//!
//! Mounts are applied to *target* containers only — not to the millstone or
//! datadog-intake containers panoramic spawns as test infrastructure.
//!
//! If the mounts directory does not exist, no mounts are applied (with a warning); this
//! lets the panoramic binary run in environments where the compile-time default path
//! isn't present.

use std::path::{Path, PathBuf};

use airlock::driver::DriverConfig;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{debug, warn};

/// Apply every regular file under `mounts_dir` as a read-only bind mount on `config`,
/// re-rooted at `/` inside the container.
pub fn apply_target_mounts(mut config: DriverConfig, mounts_dir: &Path) -> Result<DriverConfig, GenericError> {
    if !mounts_dir.exists() {
        warn!(
            mounts_dir = %mounts_dir.display(),
            "Mounts directory does not exist; no read-only overlays will be applied to target containers."
        );
        return Ok(config);
    }

    let mut stack = vec![mounts_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .with_error_context(|| format!("Failed to read mounts directory '{}'.", dir.display()))?;

        for entry in entries {
            let entry = entry
                .with_error_context(|| format!("Failed to read entry in mounts directory '{}'.", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_error_context(|| format!("Failed to stat '{}'.", path.display()))?;

            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let container_path = container_path_for(mounts_dir, &path)?;
                debug!(
                    host_path = %path.display(),
                    container_path = %container_path.display(),
                    "Mounting overlay file into target container."
                );
                config = config.with_readonly_bind_mount(path, container_path);
            }
        }
    }

    Ok(config)
}

/// Compute the in-container path for a host file under `mounts_dir`, by stripping the
/// mounts-dir prefix and re-rooting at `/`.
fn container_path_for(mounts_dir: &Path, host_path: &Path) -> Result<PathBuf, GenericError> {
    let relative = host_path.strip_prefix(mounts_dir).with_error_context(|| {
        format!(
            "Host path '{}' is not under mounts directory '{}'.",
            host_path.display(),
            mounts_dir.display()
        )
    })?;
    Ok(Path::new("/").join(relative))
}
