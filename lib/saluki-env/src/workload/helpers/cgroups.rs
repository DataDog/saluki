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

/// Highest inode number that can't refer to a specific cgroup controller.
///
/// Inodes 0 and 1 are never valid, and inode 2 is conventionally the root of a filesystem.
const MAX_RESERVED_INODE: u64 = 2;

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
                // Detected separately from procfs: the two are independent mounts, and a deployment can map one
                // without the other. Keying this off the procfs feature would point us at a cgroupfs path that isn't
                // there, or make us miss the host's hierarchy in favor of our own container's.
                if feature_detector.is_feature_available(Feature::HostMappedCgroupfs) {
                    PathBuf::from(DEFAULT_HOST_MAPPED_CGROUPFS_ROOT)
                } else {
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
        let container_id = extract_container_id_from_path(cgroup_path, &self.interner)?;

        let metadata = match cgroup_path.metadata() {
            Ok(metadata) => metadata,
            Err(e) => {
                trace!(error = %e, cgroup_controller_path = %cgroup_path.display(), "Failed to get metadata for possible cgroup controller path.");
                return None;
            }
        };

        // A reserved inode can't be attributed to this controller specifically, so we have nothing usable to key an
        // alias on. Drop the cgroup rather than reporting it with an inode that would resolve the wrong workload -- or
        // none at all.
        let controller_inode = metadata.ino();
        if !is_usable_controller_inode(controller_inode) {
            debug!(
                controller_inode,
                %container_id,
                cgroup_controller_path = %cgroup_path.display(),
                "Ignoring cgroup controller with reserved inode.",
            );
            return None;
        }

        trace!(
            controller_inode,
            %container_id,
            cgroup_controller_path = %cgroup_path.display(),
            "Found valid cgroups controller for container.",
        );

        Some(Cgroup {
            ino: Some(controller_inode),
            container_id,
        })
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
                // The names in that path are all we need: matching them doesn't touch the filesystem, so it works just
                // as well for a relative path as an absolute one.
                if let Some(container_id) = extract_container_id_from_path(entry.path, &self.interner) {
                    return Some(Cgroup {
                        ino: None,
                        container_id,
                    });
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
    /// while we were walking the hierarchy -- are skipped rather than aborting the traversal. If any of those skips
    /// could have hidden a cgroup that still exists, the returned traversal is marked as incomplete. See
    /// [`TraversalResult::is_complete`] for why that distinction matters.
    pub fn get_child_cgroups(&self) -> TraversalResult {
        // Walk the cgroups hierarchy and collect all cgroups that we can find that are related to containers..
        let root_path = self.hierarchy_reader.root_path();

        match visit_subdirectories(root_path, |path| self.try_cgroup_from_path(path)) {
            Ok(traversal) => traversal,
            Err(e) => {
                // We only get here if the hierarchy root itself couldn't be read, which generally points at a
                // misconfigured cgroupfs path rather than a transient condition.
                warn!(error = %e, cgroups_root = %root_path.display(), "Failed to visit cgroups hierarchy.");

                TraversalResult::unreadable()
            }
        }
    }
}

/// The result of traversing the cgroups hierarchy.
///
/// This accumulates as the traversal runs: [`visit_subdirectories`] creates one, records each cgroup it's handed and
/// each path it couldn't read, and returns it.
#[derive(Default)]
pub struct TraversalResult {
    cgroups: Vec<Cgroup>,
    skipped: usize,
    obscured: usize,
}

impl TraversalResult {
    /// Creates a traversal representing a hierarchy that couldn't be read at all.
    ///
    /// The result is empty and not [complete][Self::is_complete], since failing to read the root hides everything
    /// beneath it.
    fn unreadable() -> Self {
        Self {
            cgroups: Vec::new(),
            skipped: 0,
            obscured: 1,
        }
    }

    /// Records a container cgroup found during the traversal.
    fn record_cgroup(&mut self, cgroup: Cgroup) {
        self.cgroups.push(cgroup);
    }

    /// Records a path that couldn't be read, classifying whether skipping it may have hidden existing cgroups.
    fn record_skip(&mut self, e: &io::Error, path: &Path) {
        self.skipped += 1;

        match e.kind() {
            // The directory is gone, so everything beneath it is gone too. There's nothing left for the skip to hide,
            // and callers tracking those cgroups are right to consider them removed.
            //
            // This is routine rather than exceptional: cgroups are removed as the workloads attached to them exit, and
            // we have no way to hold the hierarchy still while we walk it.
            io::ErrorKind::NotFound => {
                trace!(error = %e, path = %path.display(), "Path disappeared during traversal. Skipping.");
            }

            // We can't read this subtree, so we've never reported anything from it, so there's nothing a caller could
            // be tracking for us to hide from them.
            //
            // This assumes the permissions aren't changing underneath us: a directory that was readable and becomes
            // unreadable would be misclassified here. That's rare enough to accept, and the alternative -- treating
            // every permission error as obscuring -- would permanently mark traversals unreliable whenever some part
            // of the tree is simply not ours to read.
            io::ErrorKind::PermissionDenied => {
                debug!(error = %e, path = %path.display(), "Path is not readable. Skipping.");
            }

            // The subtree is still there and we just failed to read it this time, so anything beneath it is now
            // invisible to us despite still existing.
            _ => {
                self.obscured += 1;
                debug!(error = %e, path = %path.display(), "Failed to traverse path. Skipping.");
            }
        }
    }

    /// Returns whether absence from this traversal can be taken to mean a cgroup no longer exists.
    ///
    /// When this is `false`, part of the hierarchy that may still hold live cgroups couldn't be read, so the cgroups
    /// reported are not exhaustive. The entries present are still valid, and callers can safely treat them as live, but
    /// callers **MUST NOT** infer that a previously known cgroup was removed simply because it's absent here.
    ///
    /// Note that this can be `true` even when [`skipped`][Self::skipped] is non-zero: a skipped path that couldn't have
    /// hidden a live cgroup -- because the path is gone, or because we've never been able to read it -- doesn't make
    /// the set unreliable.
    pub fn is_complete(&self) -> bool {
        self.obscured == 0
    }

    /// Returns the number of paths skipped due to recoverable errors during the traversal.
    ///
    /// This counts every skip, including those that don't affect [`is_complete`][Self::is_complete], and is intended
    /// for telemetry rather than for deciding how much to trust the result.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Consumes `self` and returns the container cgroups found during the traversal.
    pub fn into_cgroups(self) -> Vec<Cgroup> {
        self.cgroups
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

/// Visits every subdirectory beneath the given path, collecting the cgroups that `visit` identifies.
///
/// Subdirectories that can't be read are skipped, along with everything beneath them, and recorded in the returned
/// [`TraversalResult`]. Callers that need to distinguish "this subdirectory is gone" from "we couldn't see this
/// subdirectory" **MUST** check [`TraversalResult::is_complete`].
///
/// # Errors
///
/// If the given path itself can't be queried or listed, an error is returned: nothing was seen, so there's no result
/// worth reporting. Failures below the given path are never fatal.
fn visit_subdirectories<P, F>(path: P, mut visit: F) -> Result<TraversalResult, GenericError>
where
    P: AsRef<Path>,
    F: FnMut(&Path) -> Option<Cgroup>,
{
    let root = path.as_ref();

    // We can only visit directories, so if the initial path we're given isn't a directory, then we can't do anything.
    let metadata = fs::metadata(root)
        .with_error_context(|| format!("Failed to query metadata for traversal root ({}).", root.display()))?;
    if !metadata.is_dir() {
        return Ok(TraversalResult::default());
    }

    let mut traversal = TraversalResult::default();

    // Do an initial pass on our path to get all of its subdirectories, which we'll visit, and then also use as the seed
    // for further visiting.
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        // A directory can be removed between the point where we discovered it and the point where we pop it off the
        // stack to read it, so failing here costs us that subtree but shouldn't stop us from walking the rest.
        let dir_reader = match fs::read_dir(&path) {
            Ok(dir_reader) => dir_reader,
            Err(e) => {
                // Failing on the root is fatal, unlike failing anywhere below it. Every other path costs us one
                // subtree, but if we can't list the root then we haven't seen anything at all -- and an empty result
                // that claims to be complete tells callers every cgroup they know about has gone away.
                //
                // Note that this is reachable even though we successfully stat'd the root above: listing a directory
                // needs read permission, while stat'ing it only needs to traverse its parent.
                if path.as_path() == root {
                    return Err(e)
                        .with_error_context(|| format!("Failed to read traversal root ({}).", root.display()));
                }

                traversal.record_skip(&e, &path);
                continue;
            }
        };

        for entry in dir_reader {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    traversal.record_skip(&e, &path);
                    continue;
                }
            };

            let entry_path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) => {
                    traversal.record_skip(&e, &entry_path);
                    continue;
                }
            };

            if file_type.is_dir() {
                if let Some(cgroup) = visit(&entry_path) {
                    traversal.record_cgroup(cgroup);
                }

                stack.push(entry_path);
            }
        }
    }

    Ok(traversal)
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

/// Returns `true` if the given inode can identify a specific cgroup controller.
///
/// Reserved inodes -- see [`MAX_RESERVED_INODE`] -- are reported by some filesystems for paths that aren't a distinct
/// object, so they can't be used to tell one controller apart from another.
fn is_usable_controller_inode(inode: u64) -> bool {
    inode > MAX_RESERVED_INODE
}

/// Matches a container ID anywhere within a cgroup name.
///
/// This regular expression is meant to capture:
/// - 64 character hexadecimal strings (standard format for container IDs almost everywhere)
/// - 32 character hexadecimal strings followed by a dash and a number (used by AWS ECS)
/// - 8 character hexadecimal strings followed by up to four groups of 4 character hexadecimal strings separated by
///   dashes (essentially a UUID, used by Pivotal Cloud Foundry's Garden technology)
static CONTAINER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("([0-9a-f]{64})|([0-9a-f]{32}-\\d+)|([0-9a-f]{8}(-[0-9a-f]{4}){4}$)").unwrap());

fn extract_container_id(cgroup_name: &str, interner: &GenericMapInterner) -> Option<MetaString> {
    match match_container_id(cgroup_name, interner) {
        ContainerIdMatch::Container(container_id) => Some(container_id),
        ContainerIdMatch::Excluded | ContainerIdMatch::Uninternable | ContainerIdMatch::NoMatch => None,
    }
}

/// What a single cgroup name turned out to be.
enum ContainerIdMatch {
    /// The cgroup belongs to a container, with the given ID.
    Container(MetaString),

    /// The cgroup is named after a container but doesn't represent one.
    Excluded,

    /// The cgroup belongs to a container, but interning its ID failed.
    ///
    /// We know which container this is and simply can't name it, which is different from not knowing: a caller walking
    /// a path **MUST NOT** keep searching outwards, since the answer it found would be a different container.
    Uninternable,

    /// The cgroup isn't named after a container at all.
    NoMatch,
}

/// Matches a single cgroup name against the container ID heuristic.
///
/// [`ContainerIdMatch::Excluded`] is reported separately from [`ContainerIdMatch::NoMatch`] because the two mean
/// different things to a caller walking a path: a name that isn't a container tells you nothing about its ancestors,
/// but a name that is deliberately excluded is a definitive answer for that cgroup.
fn match_container_id(cgroup_name: &str, interner: &GenericMapInterner) -> ContainerIdMatch {
    let container_id = match CONTAINER_REGEX.find(cgroup_name) {
        Some(container_id) => container_id,
        None => return ContainerIdMatch::NoMatch,
    };

    // Note that this is checked against the full cgroup name, not against the ID we just matched out of it: the match
    // is a bare hexadecimal string, which can never carry any of these prefixes or suffixes.
    if is_container_named_but_not_a_container(cgroup_name) {
        return ContainerIdMatch::Excluded;
    }

    match interner.try_intern(container_id.as_str()) {
        Some(interned) => ContainerIdMatch::Container(MetaString::from(interned)),
        None => {
            error!(container_id = %container_id.as_str(), "Failed to intern container ID.");
            ContainerIdMatch::Uninternable
        }
    }
}

/// Resolves the container ID for a cgroup path, falling back to the path's ancestors.
///
/// A container's workload can sit in a cgroup nested below the one named for the container, in which case the leaf
/// doesn't carry the ID but one of its ancestors does. The deepest ancestor that names a container wins, so the most
/// specific enclosing container is the one reported.
///
/// The search stops early, returning `None`, in two cases where continuing outwards would answer with some *other*
/// container:
///
/// - The leaf is named after a container but isn't one. Such a cgroup isn't part of the container's workload at all.
/// - A container is identified but its ID can't be interned. We know which container it is and just can't name it,
///   which is not the same as not knowing.
fn extract_container_id_from_path(cgroup_path: &Path, interner: &GenericMapInterner) -> Option<MetaString> {
    let leaf_name = cgroup_path.file_name().and_then(|s| s.to_str())?;

    match match_container_id(leaf_name, interner) {
        ContainerIdMatch::Container(container_id) => return Some(container_id),
        ContainerIdMatch::Excluded | ContainerIdMatch::Uninternable => return None,
        ContainerIdMatch::NoMatch => {}
    }

    // `ancestors` yields the path itself first, which we've already checked, so skip it.
    for ancestor in cgroup_path.ancestors().skip(1) {
        let ancestor_name = match ancestor.file_name().and_then(|s| s.to_str()) {
            Some(ancestor_name) => ancestor_name,
            None => continue,
        };

        match match_container_id(ancestor_name, interner) {
            ContainerIdMatch::Container(container_id) => return Some(container_id),

            // An excluded ancestor is a statement about that cgroup, not about the one we're resolving, so the
            // container enclosing it can still be the right answer.
            ContainerIdMatch::Excluded | ContainerIdMatch::NoMatch => {}

            // Reporting the next container out would attribute this cgroup to the wrong one, so stop here.
            ContainerIdMatch::Uninternable => return None,
        }
    }

    None
}

/// Returns `true` if a cgroup is named after a container but doesn't represent that container's workload.
fn is_container_named_but_not_a_container(cgroup_name: &str) -> bool {
    // With the systemd cgroup driver, a `.mount` cgroup can sit alongside a container's own cgroup. It exists, but no
    // process is ever attached to it, so it holds no stats.
    //
    // The `conmon` cgroups belong to the CRI-O/Podman monitor process supervising a container, rather than to the
    // container itself.
    cgroup_name.ends_with(".mount")
        || cgroup_name.starts_with("crio-conmon-")
        || cgroup_name.starts_with("libpod-conmon-")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs, io,
        num::NonZeroUsize,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
    };

    use saluki_config::ConfigurationLoader;
    use stringtheory::{
        interning::{GenericMapInterner, InternedString, Interner as _},
        MetaString,
    };
    use tempfile::tempdir;

    use super::{
        extract_container_id, extract_container_id_from_path, get_container_id_from_cgroup_lines,
        is_usable_controller_inode, visit_subdirectories, CgroupControllerEntry, CgroupsConfiguration, CgroupsReader,
        Feature, FeatureDetector, HierarchyReader, TraversalResult, DEFAULT_CGROUPFS_ROOT,
        DEFAULT_HOST_MAPPED_CGROUPFS_ROOT, DEFAULT_HOST_MAPPED_PROCFS_ROOT, DEFAULT_PROCFS_ROOT,
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

    fn extract_from_path(raw: &str) -> Option<MetaString> {
        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());
        extract_container_id_from_path(Path::new(raw), &interner)
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

    // The exclusions below have to be checked against the full cgroup name. Checking them against the matched
    // container ID -- a bare hexadecimal string -- can never fire, which is precisely the bug these tests guard.

    #[test]
    fn extract_container_id_excludes_dot_mount_cgroups() {
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let raw = format!("{}.mount", container_id);

        assert_eq!(extract(&raw), None);
    }

    #[test]
    fn extract_container_id_excludes_crio_conmon_cgroups() {
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let raw = format!("crio-conmon-{}.scope", container_id);

        assert_eq!(extract(&raw), None);
    }

    #[test]
    fn extract_container_id_excludes_libpod_conmon_cgroups() {
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let raw = format!("libpod-conmon-{}.scope", container_id);

        assert_eq!(extract(&raw), None);
    }

    #[test]
    fn extract_container_id_includes_libpod_container_cgroups() {
        // Only the `conmon` monitor cgroup is excluded -- the container's own Podman cgroup shares the `libpod-`
        // prefix and must still resolve.
        let container_id = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
        let raw = format!("libpod-{}.scope", container_id);

        assert_eq!(extract(&raw), Some(MetaString::from(container_id)));
    }

    #[test]
    fn reserved_inodes_are_not_usable_controller_inodes() {
        // 0 and 1 are never valid inodes, and 2 is conventionally the root of a filesystem.
        assert!(!is_usable_controller_inode(0));
        assert!(!is_usable_controller_inode(1));
        assert!(!is_usable_controller_inode(2));
    }

    #[test]
    fn ordinary_inodes_are_usable_controller_inodes() {
        assert!(is_usable_controller_inode(3));
        assert!(is_usable_controller_inode(4_026_531_835));
        assert!(is_usable_controller_inode(u64::MAX));
    }

    const CONTAINER_ID_A: &str = "06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe";
    const CONTAINER_ID_B: &str = "1a2b3c4d5e6f70819293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    #[test]
    fn extract_from_path_prefers_the_leaf() {
        let path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/cri-containerd-{}.scope",
            CONTAINER_ID_A, CONTAINER_ID_B
        );

        // The deepest container wins, so a nested container isn't attributed to the one enclosing it.
        assert_eq!(extract_from_path(&path), Some(MetaString::from(CONTAINER_ID_B)));
    }

    #[test]
    fn extract_from_path_falls_back_to_ancestors() {
        // A container's workload can live in a cgroup nested below the one named for the container.
        let path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/init",
            CONTAINER_ID_A
        );

        assert_eq!(extract_from_path(&path), Some(MetaString::from(CONTAINER_ID_A)));
    }

    #[test]
    fn extract_from_path_uses_the_deepest_matching_ancestor() {
        let path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/cri-containerd-{}.scope/init",
            CONTAINER_ID_A, CONTAINER_ID_B
        );

        assert_eq!(extract_from_path(&path), Some(MetaString::from(CONTAINER_ID_B)));
    }

    #[test]
    fn extract_from_path_returns_none_without_any_container_segment() {
        let path = "/sys/fs/cgroup/system.slice/systemd-journald.service";

        assert_eq!(extract_from_path(path), None);
    }

    #[test]
    fn extract_from_path_does_not_rescue_excluded_leaves_from_ancestors() {
        // A `.mount` or `conmon` cgroup isn't part of the container's workload, so even though an ancestor names a
        // container, attributing it there would be wrong.
        let mount_path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/{}.mount",
            CONTAINER_ID_A, CONTAINER_ID_B
        );
        let conmon_path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/crio-conmon-{}.scope",
            CONTAINER_ID_A, CONTAINER_ID_B
        );

        assert_eq!(extract_from_path(&mount_path), None);
        assert_eq!(extract_from_path(&conmon_path), None);
    }

    #[test]
    fn extract_from_path_skips_excluded_ancestors() {
        // An excluded ancestor doesn't claim the cgroup either; the search continues past it.
        let path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/crio-conmon-{}.scope/init",
            CONTAINER_ID_A, CONTAINER_ID_B
        );

        assert_eq!(extract_from_path(&path), Some(MetaString::from(CONTAINER_ID_A)));
    }

    /// Builds an interner that can still resolve `held_id` but has no room left for `blocked_id`.
    ///
    /// The returned handles have to be kept alive for as long as the interner is used: an entry is reclaimed as soon
    /// as its last handle drops, so dropping them would un-fill the interner.
    fn interner_holding_but_blocking(held_id: &str, blocked_id: &str) -> (GenericMapInterner, Vec<InternedString>) {
        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());
        let mut held = vec![interner.try_intern(held_id).expect("interner starts out empty")];

        // The interner is sharded, so "full" is per-shard: we have to keep adding distinct strings until the shard
        // that `blocked_id` hashes to is the one that fills up. Probing with `blocked_id` itself is harmless, since
        // dropping the handle immediately gives the entry back.
        for filler in 0.. {
            if interner.try_intern(blocked_id).is_none() {
                break;
            }

            assert!(filler < 100_000, "interner never filled up");

            if let Some(interned) = interner.try_intern(&format!("filler-{:060}", filler)) {
                held.push(interned);
            }
        }

        // `held_id` has to still be resolvable, otherwise the tests below would pass for the wrong reason: they need
        // the ancestor fallback to be *capable* of succeeding, so that declining to take it means something.
        assert!(interner.try_intern(held_id).is_some());

        (interner, held)
    }

    #[test]
    fn extract_from_path_does_not_fall_back_when_the_leaf_id_cannot_be_interned() {
        let (interner, _held) = interner_holding_but_blocking(CONTAINER_ID_A, CONTAINER_ID_B);

        // The leaf names a container we can identify but can't name. Walking out to the enclosing container would
        // attribute the inner container's workload to the outer one, which is worse than reporting nothing.
        let path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/cri-containerd-{}.scope",
            CONTAINER_ID_A, CONTAINER_ID_B
        );

        assert_eq!(extract_container_id_from_path(Path::new(&path), &interner), None);
    }

    #[test]
    fn extract_from_path_stops_at_an_ancestor_whose_id_cannot_be_interned() {
        let (interner, _held) = interner_holding_but_blocking(CONTAINER_ID_A, CONTAINER_ID_B);

        // Same reasoning one level up: the nearest enclosing container is the right answer, so failing to name it
        // means we have no answer, not that we should keep searching outwards.
        let path = format!(
            "/sys/fs/cgroup/system.slice/cri-containerd-{}.scope/cri-containerd-{}.scope/init",
            CONTAINER_ID_A, CONTAINER_ID_B
        );

        assert_eq!(extract_container_id_from_path(Path::new(&path), &interner), None);
    }

    #[test]
    fn extract_from_path_handles_relative_paths() {
        // `/proc/<pid>/cgroup` reports a path that's relative to the cgroup namespace root, so resolution can't depend
        // on the path being absolute or on it existing on disk.
        let path = format!("kubepods.slice/cri-containerd-{}.scope/init", CONTAINER_ID_A);

        assert_eq!(extract_from_path(&path), Some(MetaString::from(CONTAINER_ID_A)));
    }

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
        let traversal = visit_subdirectories(root.path(), |path| {
            visited.push(path.to_path_buf());
            None
        })
        .unwrap();

        assert_eq!(traversal.skipped, 0);
        assert_eq!(traversal.obscured, 0);
        assert_eq!(visited_names(root.path(), &visited), names(&["a", "a/aa", "b"]));
    }

    #[test]
    fn record_skip_classifies_by_error_kind() {
        let mut traversal = TraversalResult::default();

        // A path that's gone takes its subdirectories with it, and a path we can't read never showed us any, so
        // neither can be hiding anything from us.
        traversal.record_skip(&io::Error::from(io::ErrorKind::NotFound), Path::new("/gone"));
        traversal.record_skip(&io::Error::from(io::ErrorKind::PermissionDenied), Path::new("/denied"));

        assert_eq!(traversal.skipped, 2);
        assert_eq!(traversal.obscured, 0);
        assert!(traversal.is_complete());

        // Any other failure leaves a subtree that still exists but that we couldn't see into.
        traversal.record_skip(&io::Error::from(io::ErrorKind::Other), Path::new("/unreadable"));

        assert_eq!(traversal.skipped, 3);
        assert_eq!(traversal.obscured, 1);
        assert!(!traversal.is_complete());
    }

    #[test]
    fn visit_subdirectories_errors_when_root_is_missing() {
        let root = tempdir().unwrap();

        assert!(visit_subdirectories(root.path().join("missing"), |_| None).is_err());
    }

    #[test]
    fn visit_subdirectories_errors_when_root_is_unreadable() {
        // Nest the traversal root inside the temporary directory so its mode can be restored for cleanup.
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        fs::create_dir_all(root.join("child")).unwrap();

        if !make_unreadable(&root) {
            return;
        }

        // Stat'ing the root still succeeds -- that only needs to traverse its parent -- so this exercises the
        // `read_dir` failure specifically, which is the path that used to be recorded as an ordinary skip.
        assert!(fs::metadata(&root).is_ok());

        let result = visit_subdirectories(&root, |_| None);

        make_readable(&root);

        // An unreadable root has to be an error rather than an empty-but-complete traversal: we saw nothing, so we
        // can't let a caller conclude that everything it knew about has gone away.
        assert!(result.is_err());
    }

    #[test]
    fn visit_subdirectories_ignores_non_directory_root() {
        let root = tempdir().unwrap();
        let file_path = root.path().join("file");
        fs::write(&file_path, "not a directory").unwrap();

        let mut visited = Vec::new();
        let traversal = visit_subdirectories(&file_path, |path| {
            visited.push(path.to_path_buf());
            None
        })
        .unwrap();

        assert_eq!(traversal.skipped, 0);
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
        let traversal = visit_subdirectories(root.path(), |path| {
            visited.push(path.to_path_buf());
            if visited.len() == 2 {
                fs::remove_dir(&visited[0]).unwrap();
            }
            None
        })
        .unwrap();

        // The removed directory was still visited -- we saw it before it went away -- but reading it was skipped
        // rather than aborting the traversal, so its two siblings were still read.
        assert_eq!(traversal.skipped, 1);

        // A directory that's gone can't be hiding anything, so the traversal is still trustworthy.
        assert_eq!(traversal.obscured, 0);
        assert!(traversal.is_complete());
        assert_eq!(visited_names(root.path(), &visited), names(&["a", "b", "c"]));
    }

    #[test]
    fn visit_subdirectories_reports_obscured_paths_for_unexpected_errors() {
        let root = tempdir().unwrap();
        for name in ["a", "b"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }

        // Same trick as the removal test, but the already-visited directory is replaced with a regular file instead of
        // being deleted, so reading it back fails with `ENOTDIR` rather than `ENOENT`.
        let mut visited = Vec::new();
        let traversal = visit_subdirectories(root.path(), |path| {
            visited.push(path.to_path_buf());
            if visited.len() == 2 {
                fs::remove_dir(&visited[0]).unwrap();
                fs::write(&visited[0], "no longer a directory").unwrap();
            }
            None
        })
        .unwrap();

        // Unlike a removal, this is a path we can't account for, so it counts against the traversal's reliability.
        assert_eq!(traversal.skipped, 1);
        assert_eq!(traversal.obscured, 1);
        assert!(!traversal.is_complete());
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
        let traversal = visit_subdirectories(root.path(), |path| {
            visited.push(path.to_path_buf());
            None
        });

        make_readable(&unreadable);

        let traversal = traversal.unwrap();
        assert_eq!(traversal.skipped, 1);

        // We've never been able to see into this subtree, so skipping it doesn't hide anything we'd previously
        // reported. Counting it as obscuring would permanently taint every traversal on a host where part of the tree
        // simply isn't ours to read.
        assert_eq!(traversal.obscured, 0);
        assert!(traversal.is_complete());

        // The unreadable directory itself is still visited -- we only fail on its contents.
        assert_eq!(
            visited_names(root.path(), &visited),
            names(&["readable", "readable/nested", "unreadable"])
        );
    }

    #[test]
    fn get_child_cgroups_reports_complete_traversal() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(format!("cri-containerd-{}.scope", CONTAINER_ID_A))).unwrap();

        let traversal = reader_rooted_at(root.path()).get_child_cgroups();

        assert!(traversal.is_complete());
        assert_eq!(traversal.skipped, 0);
        assert_eq!(traversal.cgroups.len(), 1);
        assert_eq!(traversal.cgroups[0].container_id, MetaString::from(CONTAINER_ID_A));
    }

    #[test]
    fn get_child_cgroups_stays_complete_when_subdirectory_is_unreadable() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(format!("cri-containerd-{}.scope", CONTAINER_ID_A))).unwrap();

        let unreadable = root.path().join("unreadable");
        fs::create_dir(&unreadable).unwrap();
        if !make_unreadable(&unreadable) {
            return;
        }

        let traversal = reader_rooted_at(root.path()).get_child_cgroups();

        make_readable(&unreadable);

        // The skip is reported for telemetry, but it can't have hidden a live cgroup, so callers can still act on
        // what's absent. Marking this incomplete would stop the collector from ever reaping cgroups on a host where
        // some part of the hierarchy is permanently unreadable.
        assert!(traversal.is_complete());
        assert_eq!(traversal.skipped, 1);
        assert_eq!(traversal.cgroups.len(), 1);
        assert_eq!(traversal.cgroups[0].container_id, MetaString::from(CONTAINER_ID_A));
    }

    #[test]
    fn get_child_cgroups_reports_incomplete_traversal_when_root_is_missing() {
        let root = tempdir().unwrap();

        let traversal = reader_rooted_at(&root.path().join("missing")).get_child_cgroups();

        assert!(!traversal.is_complete());
        assert_eq!(traversal.skipped, 0);
        assert!(traversal.cgroups.is_empty());
    }

    #[test]
    fn get_child_cgroups_reports_incomplete_traversal_when_root_is_unreadable() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join(format!("cri-containerd-{}.scope", CONTAINER_ID_A))).unwrap();

        if !make_unreadable(&root) {
            return;
        }

        let traversal = reader_rooted_at(&root).get_child_cgroups();

        make_readable(&root);

        // The container cgroup underneath is real but invisible to us, so reporting this as complete would have the
        // collector reap every alias it holds.
        assert!(!traversal.is_complete());
        assert!(traversal.cgroups.is_empty());
    }

    async fn cgroups_config_with(detected: Feature) -> CgroupsConfiguration {
        let (config, _updates_tx) = ConfigurationLoader::for_tests(None, None, false).await;

        CgroupsConfiguration::from_configuration(&config, FeatureDetector::from_detected_features(detected))
            .expect("configuration should load")
    }

    #[tokio::test]
    async fn cgroupfs_root_defaults_to_local_when_nothing_is_host_mapped() {
        let config = cgroups_config_with(Feature::none()).await;

        assert_eq!(config.procfs_path(), Path::new(DEFAULT_PROCFS_ROOT));
        assert_eq!(config.cgroupfs_path(), Path::new(DEFAULT_CGROUPFS_ROOT));
    }

    #[tokio::test]
    async fn cgroupfs_root_follows_host_mapped_cgroupfs() {
        let config = cgroups_config_with(Feature::HostMappedCgroupfs).await;

        assert_eq!(config.cgroupfs_path(), Path::new(DEFAULT_HOST_MAPPED_CGROUPFS_ROOT));
    }

    #[tokio::test]
    async fn cgroupfs_root_ignores_host_mapped_procfs() {
        // procfs and cgroupfs are independent mounts. A deployment that maps one without the other used to get the
        // host cgroupfs path off the back of the procfs mount, pointing the reader at a path that isn't there.
        let config = cgroups_config_with(Feature::HostMappedProcfs).await;

        assert_eq!(config.procfs_path(), Path::new(DEFAULT_HOST_MAPPED_PROCFS_ROOT));
        assert_eq!(config.cgroupfs_path(), Path::new(DEFAULT_CGROUPFS_ROOT));
    }

    #[tokio::test]
    async fn procfs_root_ignores_host_mapped_cgroupfs() {
        let config = cgroups_config_with(Feature::HostMappedCgroupfs).await;

        assert_eq!(config.procfs_path(), Path::new(DEFAULT_PROCFS_ROOT));
    }
}
