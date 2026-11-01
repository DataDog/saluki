//! Run-wide isolation scope allocation and shared filesystem paths.

use crate::{layout::Os, ledger::random_id};

/// Allocates case isolation scopes under one run identity.
///
/// Each run has up to 10000 scopes. Fixed-width paths keep Unix socket path budgets independent of allocation order.
pub struct ScopeRegistry {
    run: String,
    next: u16,
}

impl ScopeRegistry {
    /// Creates a registry with a random eight-character run identity.
    pub fn new() -> Self {
        Self {
            run: random_id(),
            next: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_run(run: &str) -> Self {
        assert_eq!(run.len(), 8);
        Self {
            run: run.into(),
            next: 0,
        }
    }

    /// Allocates the next scope, or fails when this run's 10000 scopes are allocated.
    pub(crate) fn allocate(&mut self) -> Result<ScopeLayout, String> {
        if self.next == 10000 {
            return Err("all 10000 isolation scopes are allocated".into());
        }
        let scope = ScopeLayout {
            run: self.run.clone(),
            index: self.next,
        };
        self.next += 1;
        Ok(scope)
    }

    /// Returns index 0 of this run for validating a case excluded by OS selection.
    /// Its paths have the same length as allocated scopes.
    pub(crate) fn unallocated(&self) -> ScopeLayout {
        ScopeLayout {
            run: self.run.clone(),
            index: 0,
        }
    }
}

impl Default for ScopeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A case's run identity and isolation scope index.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScopeLayout {
    pub(crate) run: String,
    pub(crate) index: u16,
}

impl ScopeLayout {
    /// Renders the scope's host state directory in the participant OS's path style.
    pub(crate) fn state_dir(&self, os: Os) -> String {
        // Fixed roots leave room for socket names even on hosts with long temporary-directory paths.
        match os {
            Os::Linux | Os::MacOs => format!("/tmp/panoramic-{}/{:04}", self.run, self.index),
            Os::Windows => format!(r"C:\Windows\Temp\panoramic-{}\{:04}", self.run, self.index),
        }
    }
}

/// Renders a participant's socket directory beneath the shared filesystem root.
pub(super) fn participant_shared_dir(root: &str, participant: &str, os: Os) -> String {
    let separator = if os == Os::Windows { '\\' } else { '/' };
    format!("{root}{separator}{participant}{separator}shared")
}

/// Checks that a socket another participant consumes lies under its server's `shared_dir` and fits the OS's
/// socket path limit, checking location first. On failure, returns the text following the path in the caller's error.
pub(super) fn check_consumed_socket(path: &str, shared_dir: &str, os: Os) -> Result<(), String> {
    let separator = if os == Os::Windows { '\\' } else { '/' };
    let under = path
        .strip_prefix(shared_dir)
        .and_then(|rest| rest.strip_prefix(separator))
        .is_some_and(|rest| {
            let valid = |component: &str| !matches!(component, "" | "." | "..");
            if os == Os::Windows {
                rest.split(['\\', '/']).all(valid)
            } else {
                rest.split(separator).all(valid)
            }
        });
    if !under {
        return Err(format!(
            ", outside its shared_dir '{shared_dir}'; consumed sockets must lie under their server's shared_dir"
        ));
    }
    let limit = match os {
        Os::MacOs => 103,
        Os::Linux | Os::Windows => 107,
    };
    if path.len() > limit {
        return Err(format!(
            " ({} bytes); a socket path on {} holds at most {limit} bytes",
            path.len(),
            os.name()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_numbered_in_allocation_order_under_one_run_directory() {
        let mut scopes = ScopeRegistry::with_run("testrun0");
        for index in 0..3 {
            let scope = scopes.allocate().unwrap();
            assert_eq!(
                scope,
                ScopeLayout {
                    run: "testrun0".into(),
                    index
                }
            );
            assert_eq!(
                scope.state_dir(Os::Linux),
                format!("/tmp/panoramic-testrun0/{index:04}")
            );
        }
    }

    #[test]
    fn an_exhausted_scope_registry_reports_its_capacity() {
        let mut scopes = ScopeRegistry {
            run: "testrun0".into(),
            next: 9999,
        };
        assert_eq!(scopes.allocate().unwrap().index, 9999);
        for _ in 0..2 {
            assert_eq!(
                scopes.allocate(),
                Err("all 10000 isolation scopes are allocated".into())
            );
        }
    }

    #[test]
    fn a_new_registry_uses_an_eight_character_lowercase_run_id() {
        for mut scopes in [ScopeRegistry::new(), ScopeRegistry::default()] {
            let scope = scopes.allocate().unwrap();
            assert_eq!(scope.run.len(), 8);
            assert!(scope
                .run
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()));
            assert_eq!(scope.index, 0);
        }
    }

    #[test]
    fn host_state_directories_use_the_participant_os_path_style() {
        let scope = ScopeLayout {
            run: "testrun0".into(),
            index: 3,
        };
        for (os, expected) in [
            (Os::Linux, "/tmp/panoramic-testrun0/0003"),
            (Os::MacOs, "/tmp/panoramic-testrun0/0003"),
            (Os::Windows, r"C:\Windows\Temp\panoramic-testrun0\0003"),
        ] {
            assert_eq!(scope.state_dir(os), expected);
        }
    }

    #[test]
    fn the_unix_state_directory_is_28_bytes_for_every_index() {
        let mut scopes = ScopeRegistry::with_run("testrun0");
        for _ in 0..10000 {
            let scope = scopes.allocate().unwrap();
            for os in [Os::Linux, Os::MacOs] {
                assert_eq!(scope.state_dir(os).len(), 28);
            }
        }
        assert!(scopes.allocate().is_err());
    }

    #[test]
    fn socket_path_limits_are_103_bytes_on_macos_and_107_elsewhere() {
        for (os, dir, separator, limit) in [
            (Os::MacOs, "/shared", '/', 103),
            (Os::Linux, "/shared", '/', 107),
            (Os::Windows, r"C:\shared", '\\', 107),
        ] {
            let path = format!("{dir}{separator}{}", "x".repeat(limit - dir.len() - 1));
            assert_eq!(check_consumed_socket(&path, dir, os), Ok(()));
            for suffix in ["x", "é"] {
                let path = format!("{path}{suffix}");
                assert_eq!(
                    check_consumed_socket(&path, dir, os),
                    Err(format!(
                        " ({} bytes); a socket path on {} holds at most {limit} bytes",
                        path.len(),
                        os.name()
                    ))
                );
            }
        }
    }

    #[test]
    fn a_path_lies_under_a_directory_only_below_a_separator_without_dot_components() {
        for (os, dir, path, valid) in [
            (Os::Linux, "/shared", "/shared/x.sock", true),
            (Os::Linux, "/shared", "/shared/nested/x.sock", true),
            (Os::Linux, "/shared", "/shared", false),
            (Os::Linux, "/shared", "/shared/", false),
            (Os::Linux, "/shared", "/shared2/x.sock", false),
            (Os::Linux, "/shared", "/shared/../x.sock", false),
            (Os::Linux, "/shared", "/shared/./x.sock", false),
            (Os::Linux, "/shared", "/shared//x.sock", false),
            (Os::Linux, "/shared", "/shared/x.sock/", false),
            (Os::Windows, r"C:\shared", r"C:\shared\x.sock", true),
            (Os::Windows, r"C:\shared", r"C:\shared/x.sock", false),
            (Os::Windows, r"C:\shared", r"C:\shared\..\x.sock", false),
            // Mixed Windows separators can hide parent traversal from a backslash-only split.
            (Os::Windows, r"C:\shared", r"C:\shared\a/..\x.sock", false),
            (Os::Windows, r"C:\shared", r"C:\shared\a/b.sock", true),
            (Os::Windows, r"C:\shared", r"C:\shared\.\x.sock", false),
            (Os::Windows, r"C:\shared", r"C:\shared\\x.sock", false),
        ] {
            let expected = if valid {
                Ok(())
            } else {
                Err(format!(
                    ", outside its shared_dir '{dir}'; consumed sockets must lie under their server's shared_dir"
                ))
            };
            assert_eq!(check_consumed_socket(path, dir, os), expected, "{path}");
        }
        let path = format!("/elsewhere/{}", "x".repeat(108));
        assert!(check_consumed_socket(&path, "/shared", Os::Linux)
            .unwrap_err()
            .contains("outside its shared_dir"));
    }
}
