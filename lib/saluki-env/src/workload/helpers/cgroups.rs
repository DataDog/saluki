use std::path::{Path, PathBuf};

use saluki_config::GenericConfiguration;
use saluki_error::GenericError;

use crate::features::{Feature, FeatureDetector};

const DEFAULT_PROCFS_ROOT: &str = "/proc";
const DEFAULT_CGROUPFS_ROOT: &str = "/sys/fs/cgroup";
const DEFAULT_HOST_MAPPED_PROCFS_ROOT: &str = "/host/proc";
const DEFAULT_HOST_MAPPED_CGROUPFS_ROOT: &str = "/host/sys/fs/cgroup";

/// Linux Control Groups-specific configuration.
///
/// Provides environment-specific paths to both "procfs" and "cgroupfs" filesystems, necessary for querying the Linux
/// Control Groups v2 unified hierarchy.
pub struct CGroupsConfiguration {
    procfs_root: PathBuf,
    cgroupfs_root: PathBuf,
}

impl CGroupsConfiguration {
    /// Creates a new `CGroupsConfiguration` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If any of the paths in the configuration are not valid, an error will be returned. This does not include,
    /// however, if any of the configured paths do not _exist_.
    pub fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector,
    ) -> Result<Self, GenericError> {
        let procfs_root = match config.try_get_typed::<PathBuf>("container_proc_root")? {
            Some(procfs_root) => procfs_root,
            None => {
                if feature_detector.is_feature_available(Feature::HostMappedProcfs) {
                    PathBuf::from(DEFAULT_HOST_MAPPED_PROCFS_ROOT)
                } else {
                    PathBuf::from(DEFAULT_PROCFS_ROOT)
                }
            }
        };

        let cgroupfs_root = match config.try_get_typed::<PathBuf>("container_cgroup_root")? {
            Some(procfs_root) => procfs_root,
            None => {
                if feature_detector.is_feature_available(Feature::HostMappedProcfs) {
                    PathBuf::from(DEFAULT_HOST_MAPPED_CGROUPFS_ROOT)
                } else {
                    // TODO: Consider if we need to do anything specific for Amazon Linux [1] or does the referenced code only
                    // matter for cgroupsv1?
                    //
                    // [1]: https://github.com/DataDog/datadog-agent/blob/fe75b815c2f135f0d2ea85d7a57a8fc8cbf56bd9/pkg/config/setup/config.go#L1172-L1173
                    PathBuf::from(DEFAULT_CGROUPFS_ROOT)
                }
            }
        };

        Ok(Self {
            procfs_root,
            cgroupfs_root,
        })
    }

    /// Returns the path to the "procfs" filesystem.
    pub fn procfs_path(&self) -> &Path {
        self.procfs_root.as_path()
    }

    /// Returns the path to the "cgroupfs" filesystem.
    pub fn cgroupfs_path(&self) -> &Path {
        self.cgroupfs_root.as_path()
    }
}
