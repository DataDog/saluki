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
use stringtheory::{
    interning::{GenericMapInterner, Interner as _},
    MetaString,
};
use tracing::{debug, error, trace, warn};

use crate::features::{Feature, FeatureDetector};

const DEFAULT_PROCFS_ROOT: &str = "/proc";
const DEFAULT_CGROUPFS_ROOT: &str = "/sys/fs/cgroup";
const DEFAULT_HOST_MAPPED_PROCFS_ROOT: &str = "/host/proc";
const DEFAULT_HOST_MAPPED_CGROUPFS_ROOT: &str = "/host/sys/fs/cgroup";
const CGROUPS_V1_BASE_CONTROLLER_NAME: &str = "memory";
const CGROUPS_V2_CONTROLLERS_FILE: &str = "cgroup.controllers";
const SELF_CGROUP_PATH: &str = "/proc/self/cgroup";

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
    /// If any of the paths in the configuration aren't valid, an error will be returned. This doesn't include,
    /// however, if any of the configured paths don't _exist_.
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
///
/// This reader is capable of querying both cgroups v1 and v2 hierarchies, and can be used to find cgroups -- either
/// within the entire hierarchy, or for a specific process ID -- that are mapped specifically to containers. A simple
/// naming heuristic is used to both identify and extract container IDs from cgroup names.
#[derive(Clone)]
pub struct CgroupsReader {
    procfs_path: PathBuf,
    hierarchy_reader: HierarchyReader,
    interner: GenericMapInterner,
}

impl CgroupsReader {
    /// Creates a new `CgroupsReader` from the given configuration and interner.
    ///
    /// If either a valid cgroups v1 or v2 hierarchy is found, `Ok(Some)` is returned with the reader. Otherwise,
    /// `Ok(None)` is returned.
    ///
    /// The provided interner will be used exclusively for handling container IDs.
    ///
    /// # Errors
    ///
    /// If there is an I/O error while attempting to query the current cgroups hierarchy, an error will be returned.
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
                    ino: Some(metadata.ino()),
                    container_id,
                });
            }
        }

        None
    }

    /// Gets a cgroup for the given process ID.
    ///
    /// This method will attempt to find the cgroup for the given process ID by looking at the `/proc/<pid>/cgroup`
    /// file. If the process ID doesn't exist or isn't attached to a cgroup, `None` will be returned.
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
                // We explicitly try to extract the container ID from the reported cgroup controller path, rather than
                // trying to stick it on the end of our configured root cgroups path. This is because unless we're in the
                // host's cgroup namespace, the path we get here will be the leaf directory -- the part with the
                // container ID in it -- but it will be relative in a way that doesn't allow it to be appended to the
                // root cgroups path, and so trying to query it to get the controller inode, and all of that, will fail.
                //
                // All we need is the leaf directory anyways.
                if let Some(proc_cgroup_path) = entry.path.file_name().and_then(|s| s.to_str()) {
                    if let Some(container_id) = extract_container_id(proc_cgroup_path, &self.interner) {
                        return Some(Cgroup {
                            ino: None,
                            container_id,
                        });
                    }
                }
            } else {
                debug!(pid, cgroup_lookup_path = %proc_pid_cgroup_path.display(), base_controller_name, "Found cgroup controller for process, but it doesn't match the base controller.");
            }
        }

        debug!(pid, cgroup_lookup_path = %proc_pid_cgroup_path.display(), base_controller_name, "Could not find matching base cgroup controller for process.");

        None
    }

    /// Gets all child cgroups in the current cgroups hierarchy.
    ///
    /// Individual paths that can't be traversed -- most commonly because a container exited and its cgroup was removed
    /// while we were walking the hierarchy -- are skipped rather than aborting the traversal. When that happens, the
    /// returned traversal is marked as incomplete: the cgroups it contains are all valid, but the set is not
    /// necessarily exhaustive. See [`CgroupsTraversal::complete`] for why that distinction matters.
    pub fn get_child_cgroups(&self) -> CgroupsTraversal {
        let mut cgroups = Vec::new();
        let mut visit = |path: &Path| {
            if let Some(cgroup) = self.try_cgroup_from_path(path) {
                cgroups.push(cgroup);
            }
        };

        // Walk the cgroups hierarchy and collect all cgroups that we can find that are related to containers..
        let root_path = self.hierarchy_reader.root_path();

        let skipped = match visit_subdirectories(root_path, &mut visit) {
            Ok(outcome) => outcome.skipped,
            Err(e) => {
                // We only get here if the hierarchy root itself couldn't be read, which generally points at a
                // misconfigured cgroupfs path rather than a transient condition.
                warn!(error = %e, cgroups_root = %root_path.display(), "Failed to visit cgroups hierarchy.");

                return CgroupsTraversal {
                    cgroups,
                    complete: false,
                    skipped: 0,
                };
            }
        };

        CgroupsTraversal {
            cgroups,
            complete: skipped == 0,
            skipped,
        }
    }
}

/// The result of traversing the cgroups hierarchy.
pub struct CgroupsTraversal {
    /// Container cgroups found during the traversal.
    pub cgroups: Vec<Cgroup>,

    /// Whether the traversal covered the entire hierarchy.
    ///
    /// When this is `false`, [`cgroups`][Self::cgroups] holds only a subset of the container cgroups that exist. The
    /// entries present are still valid, so callers can safely treat them as live, but callers **MUST NOT** infer that a
    /// previously-known cgroup was removed simply because it's absent here.
    pub complete: bool,

    /// Number of paths skipped due to recoverable errors during the traversal.
    ///
    /// This is always `0` when [`complete`][Self::complete] is `true`. It can also be `0` for an incomplete traversal,
    /// if the hierarchy root itself couldn't be read.
    pub skipped: usize,
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
    fn try_from_config(config: &CgroupsConfiguration) -> Result<Option<Self>, GenericError> {
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
                // When we're inside a container that has a host-mapped cgroupfs path, the `mounts` file might end up
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

/// A container cgroup.
pub struct Cgroup {
    ino: Option<u64>,
    container_id: MetaString,
}

impl Cgroup {
    /// Returns the inode of the cgroup controller, if available.
    pub fn inode(&self) -> Option<u64> {
        self.ino
    }

    /// Consumes `self` and returns the container ID.
    pub fn into_container_id(self) -> MetaString {
        self.container_id
    }
}

struct CgroupControllerEntry<'a> {
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

/// Summary of a completed call to [`visit_subdirectories`].
struct TraversalOutcome {
    /// Number of paths skipped due to recoverable errors.
    skipped: usize,
}

/// Logs a path that was skipped during traversal, at a level matching how expected the failure is.
fn log_skipped_path(e: &io::Error, path: &Path) {
    // Paths disappearing mid-traversal is routine rather than exceptional: cgroups are removed as the workloads
    // attached to them exit, and we have no way to hold the hierarchy still while we walk it. Anything else is more
    // surprising and worth a louder log, but is still only a partial loss.
    if e.kind() == io::ErrorKind::NotFound {
        trace!(error = %e, path = %path.display(), "Path disappeared during traversal. Skipping.");
    } else {
        debug!(error = %e, path = %path.display(), "Failed to traverse path. Skipping.");
    }
}

/// Visits every subdirectory beneath the given path.
///
/// Subdirectories that can't be read are skipped, along with everything beneath them, and counted in the returned
/// [`TraversalOutcome`]. Callers that need an exhaustive view of the tree **MUST** check that
/// [`skipped`][TraversalOutcome::skipped] is zero.
///
/// # Errors
///
/// If the given path itself can't be queried, an error is returned. Failures below the given path are never fatal.
fn visit_subdirectories<P, F>(path: P, mut visit: F) -> Result<TraversalOutcome, GenericError>
where
    P: AsRef<Path>,
    F: FnMut(&Path),
{
    let root = path.as_ref();

    // We can only visit directories, so if the initial path we're given isn't a directory, then we can't do anything.
    let metadata = fs::metadata(root)
        .with_error_context(|| format!("Failed to query metadata for traversal root ({}).", root.display()))?;
    if !metadata.is_dir() {
        return Ok(TraversalOutcome { skipped: 0 });
    }

    let mut skipped = 0;

    // Do an initial pass on our path to get all of its subdirectories, which we'll visit, and then also use as the seed
    // for further visiting.
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        // A directory can be removed between the point where we discovered it and the point where we pop it off the
        // stack to read it, so failing here costs us that subtree but shouldn't stop us from walking the rest.
        let dir_reader = match fs::read_dir(&path) {
            Ok(dir_reader) => dir_reader,
            Err(e) => {
                skipped += 1;
                log_skipped_path(&e, &path);
                continue;
            }
        };

        for entry in dir_reader {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    skipped += 1;
                    log_skipped_path(&e, &path);
                    continue;
                }
            };

            let entry_path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) => {
                    skipped += 1;
                    log_skipped_path(&e, &entry_path);
                    continue;
                }
            };

            if file_type.is_dir() {
                visit(&entry_path);
                stack.push(entry_path);
            }
        }
    }

    Ok(TraversalOutcome { skipped })
}

/// Gets the current process's container ID from its local cgroup membership.
///
/// This intentionally reads the process namespace's `/proc/self/cgroup` instead of a configured procfs root, which may
/// refer to the host namespace.
pub(crate) fn get_self_container_id(interner: &GenericMapInterner) -> Option<MetaString> {
    let lines = read_lines(Path::new(SELF_CGROUP_PATH)).ok()?;
    get_container_id_from_cgroup_lines(&lines, interner)
}

fn get_container_id_from_cgroup_lines(lines: &[String], interner: &GenericMapInterner) -> Option<MetaString> {
    lines
        .iter()
        .filter_map(|line| CgroupControllerEntry::try_from_str(line))
        .filter_map(|entry| entry.path.file_name().and_then(|name| name.to_str()))
        .find_map(|cgroup_name| extract_container_id(cgroup_name, interner))
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
    use std::{
        collections::HashSet,
        fs,
        num::NonZeroUsize,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
    };

    use stringtheory::{interning::GenericMapInterner, MetaString};
    use tempfile::tempdir;

    use super::{
        extract_container_id, get_container_id_from_cgroup_lines, visit_subdirectories, CgroupControllerEntry,
        CgroupsReader, HierarchyReader, DEFAULT_PROCFS_ROOT,
    };

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

    fn extract(raw: &str) -> Option<MetaString> {
        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());
        extract_container_id(raw, &interner)
    }

    #[test]
    fn resolves_container_id_from_current_process_cgroup_format() {
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let cgroup_lines = vec![format!("0::/system.slice/cri-containerd-{container_id}.scope")];
        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());

        assert_eq!(
            get_container_id_from_cgroup_lines(&cgroup_lines, &interner),
            Some(MetaString::from(container_id))
        );
    }

    #[test]
    fn does_not_resolve_self_container_from_non_container_cgroup_fixture() {
        let cgroup_lines = include_str!("testdata/non-container-proc-self-cgroup")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());

        assert_eq!(get_container_id_from_cgroup_lines(&cgroup_lines, &interner), None);
    }

    #[test]
    fn extract_container_id_cri_containerd() {
        let expected_container_id =
            MetaString::from("06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe");
        let raw = format!("cri-containerd-{}.scope", expected_container_id);

        assert_eq!(extract(&raw), Some(expected_container_id));
    }

    // NOTE: `extract_container_id`'s documented intent is to exclude systemd `.mount` cgroups and CRI-O
    // `crio-conmon-` cgroups, since neither represents an actual container. As currently written, though, the
    // `.ends_with(".mount")`/`.starts_with("crio-conmon-")` checks are applied to the regex *match* -- which is a
    // bare hexadecimal container ID -- rather than to the full cgroup name. A hex string can never end with
    // `.mount` or start with `crio-conmon-`, so these two exclusion filters never actually fire. The two tests
    // below pin that real, current behavior (the container ID is still extracted) rather than the documented
    // intent, so a future fix that makes the filters effective will visibly flip these assertions.

    #[test]
    fn extract_container_id_does_not_exclude_dot_mount_cgroups() {
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let raw = format!("{}.mount", container_id);

        // Documented intent is exclusion (`None`); the filter is applied to the hex match, so it never fires.
        assert_eq!(extract(&raw), Some(MetaString::from(container_id)));
    }

    #[test]
    fn extract_container_id_does_not_exclude_crio_conmon_cgroups() {
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let raw = format!("crio-conmon-{}.scope", container_id);

        // Documented intent is exclusion (`None`); the filter is applied to the hex match, so it never fires.
        assert_eq!(extract(&raw), Some(MetaString::from(container_id)));
    }

    const TEST_CONTAINER_ID: &str = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";

    /// Collects the names of every visited path, relative to `root`.
    fn visited_names(root: &Path, visited: &[PathBuf]) -> HashSet<String> {
        visited
            .iter()
            .map(|path| path.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn names(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Makes `path` unreadable, returning `false` if the caller can still read it anyway.
    ///
    /// Tests running as root can read a directory regardless of its mode, and so have nothing to assert.
    fn make_unreadable(path: &Path) -> bool {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();

        if fs::read_dir(path).is_ok() {
            make_readable(path);
            return false;
        }

        true
    }

    /// Restores `path` to a readable mode, so that its parent temporary directory can be cleaned up.
    fn make_readable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn reader_rooted_at(root: &Path) -> CgroupsReader {
        CgroupsReader {
            procfs_path: PathBuf::from(DEFAULT_PROCFS_ROOT),
            hierarchy_reader: HierarchyReader::V2 {
                root: root.to_path_buf(),
                controllers: Vec::new(),
            },
            interner: GenericMapInterner::new(NonZeroUsize::new(1024).unwrap()),
        }
    }

    #[test]
    fn visit_subdirectories_visits_every_subdirectory() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/aa")).unwrap();
        fs::create_dir(root.path().join("b")).unwrap();
        fs::write(root.path().join("b/file"), "not a directory").unwrap();

        let mut visited = Vec::new();
        let outcome = visit_subdirectories(root.path(), |path| visited.push(path.to_path_buf())).unwrap();

        assert_eq!(outcome.skipped, 0);
        assert_eq!(visited_names(root.path(), &visited), names(&["a", "a/aa", "b"]));
    }

    #[test]
    fn visit_subdirectories_errors_when_root_is_missing() {
        let root = tempdir().unwrap();

        assert!(visit_subdirectories(root.path().join("missing"), |_| {}).is_err());
    }

    #[test]
    fn visit_subdirectories_ignores_non_directory_root() {
        let root = tempdir().unwrap();
        let file_path = root.path().join("file");
        fs::write(&file_path, "not a directory").unwrap();

        let mut visited = Vec::new();
        let outcome = visit_subdirectories(&file_path, |path| visited.push(path.to_path_buf())).unwrap();

        assert_eq!(outcome.skipped, 0);
        assert!(visited.is_empty());
    }

    #[test]
    fn visit_subdirectories_skips_directories_removed_mid_traversal() {
        let root = tempdir().unwrap();
        for name in ["a", "b", "c"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }

        // Every subdirectory of `root` is visited and pushed onto the traversal stack before any of them is read back,
        // so removing one that was already visited guarantees that reading it later fails with `ENOENT`. That's the
        // same race we lose in production when a container exits mid-traversal, but without depending on any timing.
        let mut visited = Vec::new();
        let outcome = visit_subdirectories(root.path(), |path| {
            visited.push(path.to_path_buf());
            if visited.len() == 2 {
                fs::remove_dir(&visited[0]).unwrap();
            }
        })
        .unwrap();

        // The removed directory was still visited -- we saw it before it went away -- but reading it was skipped
        // rather than aborting the traversal, so its two siblings were still read.
        assert_eq!(outcome.skipped, 1);
        assert_eq!(visited_names(root.path(), &visited), names(&["a", "b", "c"]));
    }

    #[test]
    fn visit_subdirectories_skips_unreadable_directories() {
        let root = tempdir().unwrap();
        let unreadable = root.path().join("unreadable");
        fs::create_dir(&unreadable).unwrap();
        fs::create_dir_all(root.path().join("readable/nested")).unwrap();

        if !make_unreadable(&unreadable) {
            return;
        }

        let mut visited = Vec::new();
        let outcome = visit_subdirectories(root.path(), |path| visited.push(path.to_path_buf()));

        make_readable(&unreadable);

        assert_eq!(outcome.unwrap().skipped, 1);
        assert_eq!(
            visited_names(root.path(), &visited),
            names(&["readable", "readable/nested", "unreadable"])
        );
    }

    #[test]
    fn get_child_cgroups_reports_complete_traversal() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(format!("cri-containerd-{}.scope", TEST_CONTAINER_ID))).unwrap();

        let traversal = reader_rooted_at(root.path()).get_child_cgroups();

        assert!(traversal.complete);
        assert_eq!(traversal.skipped, 0);
        assert_eq!(traversal.cgroups.len(), 1);
        assert_eq!(traversal.cgroups[0].container_id, MetaString::from(TEST_CONTAINER_ID));
    }

    #[test]
    fn get_child_cgroups_reports_incomplete_traversal() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(format!("cri-containerd-{}.scope", TEST_CONTAINER_ID))).unwrap();

        let unreadable = root.path().join("unreadable");
        fs::create_dir(&unreadable).unwrap();
        if !make_unreadable(&unreadable) {
            return;
        }

        let traversal = reader_rooted_at(root.path()).get_child_cgroups();

        make_readable(&unreadable);

        // The container cgroup we did manage to see is still reported, but the traversal is flagged so that callers
        // don't mistake an unseen cgroup for a removed one.
        assert!(!traversal.complete);
        assert_eq!(traversal.skipped, 1);
        assert_eq!(traversal.cgroups.len(), 1);
        assert_eq!(traversal.cgroups[0].container_id, MetaString::from(TEST_CONTAINER_ID));
    }

    #[test]
    fn get_child_cgroups_reports_incomplete_traversal_when_root_is_missing() {
        let root = tempdir().unwrap();

        let traversal = reader_rooted_at(&root.path().join("missing")).get_child_cgroups();

        assert!(!traversal.complete);
        assert_eq!(traversal.skipped, 0);
        assert!(traversal.cgroups.is_empty());
    }
}
