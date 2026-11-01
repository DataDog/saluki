//! Checks directories, subject OS and artifact manifests, then discovers cases for CLI commands.

use std::path::{Path, PathBuf};

use panoramic_core::{layout::Os, plan::Planner};

use crate::{
    config::{discover_in_scope, load_planner, Discovered},
    console::{Console, SetupFailure},
};

/// Message shown when a command is given no `-d` directories.
pub(crate) const NO_TEST_DIRECTORIES: &str =
    "No test directories specified. Use -d <path> to specify one or more directories.";

/// Directories, subject OS and planner that passed their checks, ready to discover cases.
pub(crate) struct PreparedDiscovery<'a> {
    dirs: &'a [PathBuf],
    subject_os: Os,
    planner: Planner<'static>,
}

/// Cases discovered and compiled for the selected subject OS.
pub(crate) struct Selection {
    pub(crate) subject_os: Os,
    pub(crate) discovered: Discovered,
}

/// Checks the directories, then the subject OS, then the artifact manifest.
pub(crate) fn prepare_discovery<'a>(
    dirs: &'a [PathBuf], subject_os: Result<Os, String>, manifest: Option<&Path>,
) -> Result<PreparedDiscovery<'a>, SetupFailure> {
    if dirs.is_empty() {
        return Err(SetupFailure::harness(NO_TEST_DIRECTORIES));
    }
    let subject_os = subject_os.map_err(SetupFailure::harness)?;
    let planner = load_planner(manifest).map_err(SetupFailure::harness)?;
    Ok(PreparedDiscovery {
        dirs,
        subject_os,
        planner,
    })
}

impl PreparedDiscovery<'_> {
    /// Optionally announces the directories, then discovers and compiles enabled cases.
    pub(crate) fn discover_selection(mut self, console: &Console, announce: bool) -> Result<Selection, SetupFailure> {
        if announce {
            let directories: Vec<_> = self.dirs.iter().map(|dir| dir.display().to_string()).collect();
            console.info(format!("Discovering test cases from: {}...", directories.join(", ")));
        }
        let discovered =
            discover_in_scope(&mut self.planner, self.dirs, self.subject_os).map_err(SetupFailure::discovery)?;
        Ok(Selection {
            subject_os: self.subject_os,
            discovered,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use super::*;
    use crate::config::tests::write_case;

    #[test]
    fn discovery_preparation_checks_directories_then_subject_os_then_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = [dir.path().to_path_buf()];
        let manifest = dir.path().join("missing.toml");
        let os_error = "conflicting subject OS";
        for (dirs, subject_os, expected) in [
            (&[][..], Err(os_error.to_string()), NO_TEST_DIRECTORIES.to_string()),
            (&dirs[..], Err(os_error.to_string()), os_error.to_string()),
            (&dirs[..], Ok(Os::Linux), load_planner(Some(&manifest)).err().unwrap()),
        ] {
            let checking_manifest = subject_os.is_ok();
            let failure = prepare_discovery(dirs, subject_os, Some(&manifest)).err().unwrap();
            assert_eq!(failure.message, expected);
            assert_eq!(failure.exit, ExitCode::from(2));
            if checking_manifest {
                assert!(
                    failure.message.starts_with("failed to read artifact manifest"),
                    "{}",
                    failure.message
                );
            }
        }
    }

    #[test]
    fn discover_selection_announces_the_directories_only_when_asked() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let dirs = [first.path().to_path_buf(), second.path().to_path_buf()];
        for announce in [false, true] {
            let output = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                .finish();
            let selection = tracing::subscriber::with_default(subscriber, || {
                prepare_discovery(&dirs, Ok(Os::Windows), None)
                    .unwrap()
                    .discover_selection(&Console { use_tui: false }, announce)
                    .unwrap()
            });
            assert_eq!(selection.subject_os, Os::Windows);
            assert!(selection.discovered.plans.is_empty());
            assert!(selection.discovered.excluded.is_empty());
            let log = std::fs::read_to_string(output.path()).unwrap();
            let expected = if announce {
                format!(
                    "INFO Discovering test cases from: {}, {}...",
                    first.path().display(),
                    second.path().display()
                )
            } else {
                String::new()
            };
            assert_eq!(log.trim(), expected);
        }
    }

    #[test]
    fn discover_selection_reports_a_malformed_case_as_failed_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "bad", "type: bogus\n");
        let failure = prepare_discovery(&[dir.path().to_path_buf()], Ok(Os::Linux), None)
            .unwrap()
            .discover_selection(&Console { use_tui: false }, false)
            .err()
            .unwrap();
        assert!(
            failure.message.starts_with("Failed to discover tests:"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("Unknown test type 'bogus'"),
            "{}",
            failure.message
        );
        assert!(failure.message.contains(path.canonicalize().unwrap().to_str().unwrap()));
        assert_eq!(failure.exit, ExitCode::from(2));
    }
}
