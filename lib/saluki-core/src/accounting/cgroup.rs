#[cfg(target_os = "linux")]
use std::{
    fs::{self, File},
    os::unix::fs::FileExt as _,
    path::{Path, PathBuf},
};

/// A finite cgroup v2 memory limit and the usage charged against it.
pub struct CgroupMemory {
    limit: usize,
    #[cfg(target_os = "linux")]
    levels: Vec<(usize, File)>,
}

impl CgroupMemory {
    /// Finds the lowest finite memory limit in the process's visible cgroup v2 hierarchy.
    pub fn detect() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            let groups = fs::read_to_string("/proc/self/cgroup").ok()?;
            let mounts = fs::read_to_string("/proc/self/mountinfo").ok()?;
            let levels = cgroup_limits(&groups, &mounts, |path| fs::read_to_string(path).ok());
            let limit = levels.iter().map(|(limit, _)| *limit).min()?;
            let levels = levels
                .into_iter()
                .map(|(limit, directory)| Some((limit, File::open(directory.join("memory.current")).ok()?)))
                .collect::<Option<Vec<_>>>()?;
            Some(Self { limit, levels })
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// The effective memory limit in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Reads the effective charge, including pressure from siblings in a bounded ancestor.
    pub fn current(&self) -> Option<usize> {
        #[cfg(target_os = "linux")]
        {
            let mut charge = 0;
            for (limit, current) in &self.levels {
                let mut buf = [0u8; 32];
                let len = current.read_at(&mut buf, 0).ok()?;
                let current = String::from_utf8_lossy(&buf[..len]).trim().parse().ok()?;
                charge = charge.max(charge_for_level(self.limit, *limit, current));
            }
            Some(charge)
        }
        #[cfg(not(target_os = "linux"))]
        None
    }
}

#[cfg(target_os = "linux")]
fn cgroup_limits(groups: &str, mounts: &str, read: impl Fn(&Path) -> Option<String>) -> Vec<(usize, PathBuf)> {
    let Some(group) = groups.lines().find_map(|line| line.strip_prefix("0::")).map(Path::new) else {
        return Vec::new();
    };
    let mut limits = Vec::new();
    for mount in mounts.lines() {
        let Some((fields, fs)) = mount.split_once(" - ") else {
            continue;
        };
        if !fs.starts_with("cgroup2 ") {
            continue;
        }
        let mut fields = fields.split_ascii_whitespace().skip(3);
        let (Some(root), Some(mountpoint)) = (fields.next(), fields.next()) else {
            continue;
        };
        let mountpoint = Path::new(mountpoint);
        let relative = match group.strip_prefix(root) {
            Ok(relative) => relative,
            // A cgroup namespace can report its own root, relative to a mount of a delegated subtree.
            Err(_) if group == Path::new("/") => Path::new(""),
            Err(_) => continue,
        };
        if relative
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            continue;
        }
        for directory in mountpoint
            .join(relative)
            .ancestors()
            .take_while(|dir| dir.starts_with(mountpoint))
        {
            let Some(limit) = read(&directory.join("memory.max")).and_then(|value| value.trim().parse().ok()) else {
                continue;
            };
            limits.push((limit, directory.to_path_buf()));
        }
    }
    limits
}

#[cfg(target_os = "linux")]
fn charge_for_level(effective_limit: usize, limit: usize, current: usize) -> usize {
    // Express the level's remaining bytes against the common effective limit.
    current.saturating_sub(limit - effective_limit)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn locates_nested_unified_hierarchy_and_uses_finite_ancestor() {
        let groups = "2:cpu:/service\n0::/delegated/pod/leaf\n";
        let mounts = "27 44 0:23 /delegated /cgroups rw - cgroup2 cgroup2 rw\n";
        let result = cgroup_limits(groups, mounts, |path| match path.to_str()? {
            "/cgroups/pod/leaf/memory.max" => Some("max\n".into()),
            "/cgroups/pod/memory.max" => Some(" 1073741824\n".into()),
            "/cgroups/memory.max" => Some("2147483648".into()),
            _ => panic!("read outside the cgroup mount: {path:?}"),
        });
        assert_eq!(
            result,
            vec![
                (1_073_741_824, PathBuf::from("/cgroups/pod")),
                (2_147_483_648, PathBuf::from("/cgroups")),
            ]
        );
    }

    #[test]
    fn detects_namespaced_root_and_ignores_unlimited_and_invalid_values() {
        let mounts = "27 44 0:23 /delegated /cgroups rw - cgroup2 cgroup2 rw\n";
        for (raw, expected) in [("max", None), ("invalid", None), ("65536", Some(65536))] {
            let result = cgroup_limits("0::/\n", mounts, |path| {
                assert_eq!(path, Path::new("/cgroups/memory.max"));
                Some(raw.into())
            });
            assert_eq!(result.first().map(|(limit, _)| *limit), expected);
        }
        assert!(cgroup_limits("1:memory:/unified\n", mounts, |_| panic!("not cgroup v2")).is_empty());
    }

    #[test]
    fn larger_ancestor_limit_can_have_less_headroom() {
        assert_eq!(charge_for_level(100, 100, 50), 50);
        assert_eq!(charge_for_level(100, 200, 190), 90);
        assert_eq!(charge_for_level(100, 200, 30), 0);
    }
}
