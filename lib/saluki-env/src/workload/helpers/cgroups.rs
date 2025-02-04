#![allow(dead_code)]

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, BufRead as _, BufReader},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stringtheory::{interning::GenericMapInterner, MetaString};
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
    /// # Errors
    ///
    /// If any of the paths in the configuration are not valid, an error will be returned. This does not include,
    /// however, if any of the configured paths do not _exist_.
    pub fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector,
    ) -> Result<Self, GenericError> {
        let procfs_root = match config.try_get_typed::<PathBuf>("container_proc_root")? {
            Some(path) => path,
            None => {
                if feature_detector.is_feature_available(Feature::HostMappedProcfs) {
                    PathBuf::from(DEFAULT_HOST_MAPPED_PROCFS_ROOT)
                } else {
                    PathBuf::from(DEFAULT_PROCFS_ROOT)
                }
            }
        };

        let cgroupfs_root = match config.try_get_typed::<PathBuf>("container_cgroup_root")? {
            Some(path) => path,
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

/// Reader for querying control groups being used for containerization.
#[derive(Clone)]
pub struct CgroupsReader {
    procfs_path: PathBuf,
    hierarchy_reader: HierarchyReader,
    interner: GenericMapInterner,
}

impl CgroupsReader {
    pub fn try_from_config(
        config: &CgroupsConfiguration, interner: GenericMapInterner,
    ) -> Result<Option<Self>, GenericError> {
        let hierarchy_reader = HierarchyReader::try_from_config(config)?;
        Ok(hierarchy_reader.map(|hierarchy_reader| Self {
            procfs_path: config.procfs_path().to_path_buf(),
            hierarchy_reader,
            interner,
        }))
    }

    fn try_cgroup_from_path(&self, cgroup_path: &Path) -> Option<Cgroup> {
        if let Some(name) = cgroup_path.file_name().and_then(|s| s.to_str()) {
            if let Some(container_id) = extract_container_id(name, &self.interner) {
                let metadata = match cgroup_path.metadata() {
                    Ok(metadata) => metadata,
                    Err(e) => {
                        trace!(error = %e, cgroup_controller_path = %cgroup_path.display(), "Failed to get metadata for possible cgroup controller path.");
                        return None;
                    }
                };

                trace!(
                    controller_inode = metadata.ino(),
                    %container_id,
                    cgroup_controller_path = %cgroup_path.display(),
                    "Found valid cgroups controller for container.",
                );

                return Some(Cgroup {
                    name: name.to_string(),
                    path: cgroup_path.to_path_buf(),
                    ino: metadata.ino(),
                    container_id,
                });
            }
        }

        None
    }

    pub fn get_cgroup_by_pid(&self, pid: u32) -> Option<Cgroup> {
        // See if the given process ID exists in the proc filesystem _and_ if there's a cgroup path for it.
        let proc_pid_cgroup_path = self.procfs_path.join(pid.to_string()).join("cgroup");
        let lines = match read_lines(&proc_pid_cgroup_path) {
            Ok(lines) => lines,
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    debug!(pid, cgroup_lookup_path = %proc_pid_cgroup_path.display(), "Process does not exist or is not attached to a cgroup.");
                    return None;
                }
                _ => {
                    debug!(error = %e, pid, cgroup_lookup_path = %proc_pid_cgroup_path.display(), "Failed to read cgroup file for process.");
                    return None;
                }
            },
        };

        let base_controller_name = self.hierarchy_reader.base_controller();

        // We're looking for the first line that matches our base controller name, and then we'll see if it's attached
        // to the container based on the name, and if so, return it.
        for entry in lines.iter().filter_map(|s| CgroupControllerEntry::try_from_str(s)) {
            if entry.name == base_controller_name {
                let full_entry_path = self
                    .hierarchy_reader
                    .root_path()
                    .join(entry.path.strip_prefix("/").unwrap_or(entry.path));
                if let Some(cgroup) = self.try_cgroup_from_path(&full_entry_path) {
                    return Some(cgroup);
                }
            }
        }

        debug!(pid, cgroup_lookup_path = %proc_pid_cgroup_path.display(), base_controller_name, "Could not find matching base cgroup controller for process.");

        None
    }

    pub fn get_child_cgroups(&self) -> Vec<Cgroup> {
        let mut cgroups = Vec::new();
        let mut visit = |path: &Path| {
            if let Some(cgroup) = self.try_cgroup_from_path(path) {
                cgroups.push(cgroup);
            }
        };

        // Walk the cgroups hierarchy and collect all cgroups that we can find that are related to containers..
        let root_path = match &self.hierarchy_reader {
            HierarchyReader::V1 {
                base_controller_path, ..
            } => base_controller_path.as_path(),
            HierarchyReader::V2 { root, .. } => root.as_path(),
        };

        if let Err(e) = visit_subdirectories(root_path, &mut visit) {
            error!(error = %e, "Failed to visit cgroups hierarchy.");
        }

        cgroups
    }
}

#[derive(Clone)]
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
    pub fn try_from_config(config: &CgroupsConfiguration) -> Result<Option<Self>, GenericError> {
        // Open the mount file from procfs to scan through and find any cgroups subsystems.
        let mounts_path = config.procfs_path().join("mounts");
        let mount_entries = read_lines(&mounts_path)
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

            if let (Some(raw_cgroup_path), Some(fs_type)) = (maybe_cgroup_path, maybe_fs_type) {
                let cgroup_path = Path::new(raw_cgroup_path);

                // Make sure this path is rooted within our configured cgroupfs path.
                //
                // When we're inside a container that has a host-mapped cgroupfs path, the `mounts`` file might end up
                // having duplicate entries (like one set as `/sys/fs/cgroup` and another set as `/host/sys/fs/cgroup`,
                // etc)... and we want to use the one that matches our configured cgroupfs path as that's the one that
                // will actually have the cgroups we care about.
                if !cgroup_path.starts_with(config.cgroupfs_path()) {
                    continue;
                }

                match fs_type {
                    // For cgroups v1, we have to go through all mounts we see to build a full list of enabled controlled.
                    "cgroup" => process_cgroupv1_mount_entry(cgroup_path, &mut controllers),
                    // For cgroups v2, we only need to find the unified root mountpoint, and then we can create our reader.
                    "cgroup2" => maybe_cgroups_v2 = process_cgroupv2_mount_entry(cgroup_path)?,
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

    fn base_controller(&self) -> Option<&'static str> {
        match self {
            Self::V1 { .. } => Some(CGROUPS_V1_BASE_CONTROLLER_NAME),

            // Since cgroups v2 is "unified", there's no base controller path.
            Self::V2 { .. } => None,
        }
    }

    fn root_path(&self) -> &Path {
        match self {
            Self::V1 {
                base_controller_path, ..
            } => base_controller_path.as_path(),
            Self::V2 { root, .. } => root.as_path(),
        }
    }
}

pub struct Cgroup {
    pub name: String,
    pub path: PathBuf,
    pub ino: u64,
    pub container_id: MetaString,
}

pub struct CgroupControllerEntry<'a> {
    id: usize,
    name: Option<&'a str>,
    path: &'a Path,
}

impl<'a> CgroupControllerEntry<'a> {
    fn try_from_str(line: &'a str) -> Option<Self> {
        let mut fields = line.splitn(3, ':');

        let id = fields.next()?.parse::<usize>().ok()?;
        let name = fields.next().map(|s| if s.is_empty() { None } else { Some(s) })?;
        let path = fields.next()?;

        if path.is_empty() {
            return None;
        }

        Some(Self {
            id,
            name,
            path: Path::new(path),
        })
    }
}

fn process_cgroupv1_mount_entry(cgroup_path: &Path, controllers: &mut HashMap<String, PathBuf>) {
    // Split the cgroup path, since there can be multiple controllers mounted at the same path.
    let path_controllers = cgroup_path
        .file_name()
        .and_then(|s| s.to_str().map(|s| s.split(',')))
        .into_iter()
        .flatten();
    for path_controller in path_controllers {
        // If we have an existing path mapping for this controller, keep whichever one is the
        // shortest, as we want the more generic path.
        if let Some(existing_path) = controllers.get(path_controller) {
            if existing_path.as_os_str().len() < cgroup_path.as_os_str().len() {
                continue;
            }
        }

        controllers.insert(path_controller.to_string(), PathBuf::from(cgroup_path));
    }
}

fn process_cgroupv2_mount_entry(cgroup_path: &Path) -> Result<Option<HierarchyReader>, GenericError> {
    // Read and get the list of active/enabled controllers.
    let controllers_path = cgroup_path.join(CGROUPS_V2_CONTROLLERS_FILE);
    let controllers = read_lines(&controllers_path)
        .with_error_context(|| {
            format!(
                "Failed to read controllers from cgroups v2 hierarchy ({}).",
                controllers_path.display()
            )
        })?
        .into_iter()
        .flat_map(|s| s.split_whitespace().map(|s| s.to_string()).collect::<Vec<_>>())
        .collect::<Vec<_>>();

    Ok(Some(HierarchyReader::V2 {
        root: cgroup_path.to_path_buf(),
        controllers,
    }))
}

fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    let file = OpenOptions::new().read(true).open(path)?;

    let reader = BufReader::new(file).lines();

    let mut lines = Vec::new();
    for line in reader {
        lines.push(line?);
    }

    Ok(lines)
}

fn visit_subdirectories<P, F>(path: P, mut visit: F) -> io::Result<()>
where
    P: AsRef<Path>,
    F: FnMut(&Path),
{
    // We can only visit directories, so if the initial path we're given isn't a directory, then we can't do anything.
    let metadata = fs::metadata(path.as_ref())?;
    if !metadata.is_dir() {
        return Ok(());
    }

    // Do an initial pass on our path to get all of its subdirectories, which we'll visit, and then also use as the seed
    // for further visiting.
    let mut stack = vec![path.as_ref().to_path_buf()];
    while let Some(path) = stack.pop() {
        let dir_reader = fs::read_dir(path)?;
        for entry in dir_reader {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let path = entry.path();
                visit(&path);
                stack.push(path);
            }
        }
    }

    Ok(())
}

fn extract_container_id(cgroup_name: &str, interner: &GenericMapInterner) -> Option<MetaString> {
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

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use stringtheory::{interning::GenericMapInterner, MetaString};

    use super::{extract_container_id, CgroupControllerEntry};

    #[test]
    fn parse_controller_entry_cgroups_v1() {
        let controller_id = 12;
        let controller_name = "memory";
        let controller_path_raw = "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod095a9475_4c4f_4726_912c_65743701ef3f.slice/cri-containerd-06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe.scope";
        let controller_path = Path::new(controller_path_raw);
        let raw = format!("{}:{}:{}", controller_id, controller_name, controller_path_raw);

        let entry = CgroupControllerEntry::try_from_str(&raw).unwrap();
        assert_eq!(entry.id, controller_id);
        assert_eq!(entry.name, Some(controller_name));
        assert_eq!(entry.path, controller_path);
    }

    #[test]
    fn parse_controller_entry_cgroups_v2() {
        let controller_id = 0;
        let controller_path_raw =
            "/system.slice/docker-0b96e72f48e169638a735c0a05adcfc9d6aba2bf6697b627f1635b4f00ea011d.scope";
        let controller_path = Path::new(controller_path_raw);
        let raw = format!("{}::{}", controller_id, controller_path_raw);

        let entry = CgroupControllerEntry::try_from_str(&raw).unwrap();
        assert_eq!(entry.id, controller_id);
        assert_eq!(entry.name, None);
        assert_eq!(entry.path, controller_path);
    }

    #[test]
    fn extract_container_id_cri_containerd() {
        let expected_container_id =
            MetaString::from("06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe");
        let raw = format!("cri-containerd-{}.scope", expected_container_id);
        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());

        let actual_container_id = extract_container_id(&raw, &interner);
        assert_eq!(Some(expected_container_id), actual_container_id);
    }
}
