#![allow(dead_code)]

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stringtheory::{interning::FixedSizeInterner, MetaString};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncBufReadExt as _, BufReader},
};
use tracing::{debug, error, trace};

use crate::features::{Feature, FeatureDetector};

const DEFAULT_PROCFS_ROOT: &str = "/proc";
const DEFAULT_CGROUPFS_ROOT: &str = "/sys/fs/cgroup";
const DEFAULT_HOST_MAPPED_PROCFS_ROOT: &str = "/host/proc";
const DEFAULT_HOST_MAPPED_CGROUPFS_ROOT: &str = "/host/sys/fs/cgroup";
const CGROUPS_V1_BASE_CONTROLLER_NAME: &str = "memory";
const CGROUPS_V2_CONTROLLERS_FILE: &str = "cgroup.controllers";

/// Linux Control Groups-specific configuration.
///
/// Provides environment-specific paths to both "procfs" and "cgroupfs" filesystems, necessary for querying the Linux
/// Control Groups v2 unified hierarchy.
pub struct CgroupsConfiguration {
    procfs_root: PathBuf,
    cgroupfs_root: PathBuf,
}

impl CgroupsConfiguration {
    /// Creates a new `CgroupsConfiguration` from the given configuration.
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
                    // matter for cgroups v1?
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

pub struct CgroupsReader {
    hierarchy_reader: HierarchyReader,
    interner: FixedSizeInterner<1>,
}

impl CgroupsReader {
    pub async fn try_from_config(
        config: &CgroupsConfiguration, interner: FixedSizeInterner<1>,
    ) -> Result<Option<Self>, GenericError> {
        let hierarchy_reader = HierarchyReader::try_from_config(config).await?;
        Ok(hierarchy_reader.map(|hierarchy_reader| Self {
            hierarchy_reader,
            interner,
        }))
    }

    pub async fn get_child_cgroups(&self) -> Vec<Cgroup> {
        // TODO: Probably don't do the container ID extraction here, just the returning of `Cgroup` with the cgroup path
        // and inode. Would make things a little bit cleaner in the future if we just specifically handled cgroup data
        // without any comingling of concerns.
        let mut cgroups = Vec::new();
        let mut visit = |path: &Path, ino: u64| {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if let Some(container_id) = extract_container_id(name, &self.interner) {
                    cgroups.push(Cgroup {
                        name: name.to_string(),
                        path: path.to_path_buf(),
                        ino,
                        container_id,
                    });
                }
            }
        };

        let root_path = match &self.hierarchy_reader {
            HierarchyReader::V1 {
                base_controller_path, ..
            } => base_controller_path.as_path(),
            HierarchyReader::V2 { root, .. } => root.as_path(),
        };

        if let Err(e) = visit_subdirectories(root_path, &mut visit).await {
            error!(error = %e, "Failed to visit cgroups v1 hierarchy.");
        }

        cgroups
    }
}

enum HierarchyReader {
    V1 {
        base_controller_path: PathBuf,
        controllers: HashMap<String, PathBuf>,
    },

    V2 {
        root: PathBuf,
        controllers: Vec<String>,
    },
}

impl HierarchyReader {
    pub async fn try_from_config(config: &CgroupsConfiguration) -> Result<Option<Self>, GenericError> {
        // Open the mount file from procfs to scan through and find any cgroups subsystems.
        let mounts_path = config.procfs_path().join("mounts");
        let mount_entries = read_lines(&mounts_path)
            .await
            .with_error_context(|| format!("Failed to read mount entries from procfs ({})", mounts_path.display()))?;

        let mut controllers = HashMap::new();
        let mut maybe_cgroups_v2 = None;

        // For each mount line, check if its of the `cgroup` or `cgroup2` type. Skip everything else.
        for mount_entry in mount_entries {
            // Split the line into fields, and take the second and third values. We always expect at least three fields
            // in a line if it's a line that might possibly be a cgroup mount.
            let mut fields = mount_entry.split_whitespace();
            let maybe_cgroup_path = fields.nth(1);
            let maybe_fs_type = fields.nth(0);

            if let (Some(cgroup_path), Some(fs_type)) = (maybe_cgroup_path, maybe_fs_type) {
                match fs_type {
                    // For cgroups v1, we have to go through all mounts we see to build a full list of enabled controlled.
                    "cgroup" => process_cgroupv1_mount_entry(cgroup_path, &mut controllers),
                    // For cgroups v2, we only need to find the unified root mountpoint, and then we can create our reader.
                    "cgroup2" => maybe_cgroups_v2 = process_cgroupv2_mount_entry(cgroup_path).await?,
                    _ => {}
                }
            }
        }

        // If we didn't find any cgroups v1 controllers, then we potentially return the cgroups v2 hierarchy if found...
        // otherwise, this will just return `None`.
        if controllers.is_empty() {
            if maybe_cgroups_v2.is_some() {
                debug!("Using cgroups v2 hierarchy.");
            }

            return Ok(maybe_cgroups_v2);
        }

        // If we're here, we potentially have a cgroups v1 hierarchy.  Find our base controller -- the memory controller
        // -- and once we do that, we can create our reader.
        let base_controller_path = controllers
            .get(CGROUPS_V1_BASE_CONTROLLER_NAME)
            .cloned()
            .map(PathBuf::from)
            .ok_or_else(|| {
                generic_error!(
                    "Failed to find base controller ({}) in cgroups v1 hierarchy.",
                    CGROUPS_V1_BASE_CONTROLLER_NAME
                )
            })?;

        debug!(root = %base_controller_path.display(), controllers_len = controllers.len(), "Using cgroups v1 hierarchy.");

        Ok(Some(HierarchyReader::V1 {
            base_controller_path,
            controllers,
        }))
    }
}

pub struct Cgroup {
    pub name: String,
    pub path: PathBuf,
    pub ino: u64,
    pub container_id: MetaString,
}

fn process_cgroupv1_mount_entry(cgroup_path: &str, controllers: &mut HashMap<String, PathBuf>) {
    // Split the cgroup path, since there can be multiple controllers mounted at the same path.
    let path_controllers = Path::new(cgroup_path)
        .file_name()
        .and_then(|s| s.to_str().map(|s| s.split(',')))
        .into_iter()
        .flatten();
    for path_controller in path_controllers {
        // If we have an existing path mapping for this controller, keep whichever one is the
        // shortest, as we want the more generic path.
        if let Some(existing_path) = controllers.get(path_controller) {
            if existing_path.as_os_str().len() < cgroup_path.len() {
                continue;
            }
        }

        trace!(
            controller = path_controller,
            controller_path = cgroup_path,
            "Detected cgroup v1 controller."
        );

        controllers.insert(path_controller.to_string(), PathBuf::from(cgroup_path));
    }
}

async fn process_cgroupv2_mount_entry(cgroup_path: &str) -> Result<Option<HierarchyReader>, GenericError> {
    let root = PathBuf::from(cgroup_path);

    // Read and get the list of active/enabled controllers.
    let controllers_path = root.join(CGROUPS_V2_CONTROLLERS_FILE);
    let controllers = read_lines(&controllers_path)
        .await
        .with_error_context(|| {
            format!(
                "Failed to read controllers from cgroups v2 hierarchy ({}).",
                controllers_path.display()
            )
        })?
        .into_iter()
        .flat_map(|s| s.split_whitespace().map(|s| s.to_string()).collect::<Vec<_>>())
        .collect::<Vec<_>>();

    trace!(root_path = %root.display(), controllers_len = controllers.len(), "Detected cgroups v2 controllers.");

    Ok(Some(HierarchyReader::V2 {
        root: PathBuf::from(cgroup_path),
        controllers,
    }))
}

async fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    let file = OpenOptions::new().read(true).open(path).await?;

    let mut reader = BufReader::new(file).lines();

    let mut lines = Vec::new();
    while let Some(line) = reader.next_line().await? {
        lines.push(line);
    }

    Ok(lines)
}

async fn visit_subdirectories<P, F>(path: P, mut visit: F) -> io::Result<()>
where
    P: AsRef<Path>,
    F: FnMut(&Path, u64),
{
    // We can only visit directories, so if the initial path we're given isn't a directory, then we can't do anything.
    let metadata = fs::metadata(path.as_ref()).await?;
    if !metadata.is_dir() {
        return Ok(());
    }

    // Do an initial pass on our path to get all of its subdirectories, which we'll visit, and then also use as the seed
    // for further visiting.
    let mut stack = vec![path.as_ref().to_path_buf()];
    while let Some(path) = stack.pop() {
        let mut dir_reader = fs::read_dir(path).await?;
        while let Some(entry) = dir_reader.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let path = entry.path();
                visit(&path, entry.ino());
                stack.push(path);
            }
        }
    }

    Ok(())
}

fn extract_container_id(cgroup_name: &str, interner: &FixedSizeInterner<1>) -> Option<MetaString> {
    // This regular expression is meant to capture:
    // - 64 character hexadecimal strings (standard format for container IDs almost everywhere)
    // - 32 character hexadecimal strings followed by a dash and a number (used by AWS ECS)
    // - 8 character hexadecimal strings followed by up to four groups of 4 character hexadecimal strings separated by
    //   dashes (essentially a UUID, used by Pivotal Cloud Foundry's Garden technology)
    static CONTAINER_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("([0-9a-f]{64})|([0-9a-f]{32}-\\d+)|([0-9a-f]{8}(-[0-9a-f]{4}){4}$)").unwrap());

    CONTAINER_REGEX
        .find(cgroup_name)
        .filter(|name| {
            // Filter out any systemd-managed cgroups, as well as CRI-O conmon cgroups, as they don't represent containers.
            !name.as_str().ends_with(".mount") && !name.as_str().starts_with("crio-conmon-")
        })
        .and_then(|name| match interner.try_intern(name.as_str()) {
            Some(interned) => Some(MetaString::from(interned)),
            None => {
                error!(container_id = %name.as_str(), "Failed to intern container ID.");
                None
            }
        })
}
