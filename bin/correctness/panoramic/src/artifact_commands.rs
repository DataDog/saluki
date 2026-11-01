//! Commands for checking the artifacts selected participant cases need.

use std::{
    fmt::Write as _,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use panoramic_core::{
    artifact::{self, ArtifactManifest, ArtifactVerification},
    layout::Os,
    plan::{needed_artifacts, Planner},
};
use tracing::{error, info};

use crate::{cli::VerifyCommand, collector::EXIT_HARNESS_ERROR, config::discover_in_scope};

/// Checks directory selection, subject OS, manifest and role declarations before discovery, in that order.
pub(crate) fn artifact_planner(
    test_dirs: &[PathBuf], subject_os: Result<Os, String>, artifacts: Option<&Path>,
) -> Result<(Planner<'static>, Os), ExitCode> {
    if test_dirs.is_empty() {
        error!("No test directories specified. Use -d <path> to specify one or more directories.");
        return Err(ExitCode::from(EXIT_HARNESS_ERROR));
    }
    let subject_os = subject_os.map_err(|message| {
        error!("{message}");
        ExitCode::from(EXIT_HARNESS_ERROR)
    })?;
    let manifest = artifacts
        .map(ArtifactManifest::load)
        .transpose()
        .map_err(|error| {
            error!("{error:#}");
            ExitCode::from(EXIT_HARNESS_ERROR)
        })?
        .unwrap_or_default();
    let planner = Planner::new(crate::REGISTRIES, manifest).map_err(|error| {
        error!("Invalid artifact roles: {error:#}");
        ExitCode::from(EXIT_HARNESS_ERROR)
    })?;
    Ok((planner, subject_os))
}

pub(crate) async fn verify(cmd: VerifyCommand, output: &mut impl Write) -> ExitCode {
    let (mut planner, subject_os) = match artifact_planner(&cmd.test_dirs, Ok(cmd.subject_os), cmd.artifacts.as_deref())
    {
        Ok(selection) => selection,
        Err(exit) => return exit,
    };
    let base = match std::env::current_dir() {
        Ok(base) => base,
        Err(error) => {
            error!("Cannot read the current directory: {error}");
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };
    let directories: Vec<_> = cmd.test_dirs.iter().map(|dir| dir.display().to_string()).collect();
    info!("Discovering test cases from: {}...", directories.join(", "));
    let plans = match discover_in_scope(&mut planner, &cmd.test_dirs, subject_os) {
        Ok(plans) => plans,
        Err(error) => {
            error!("Failed to discover tests: {error:#}");
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };
    let results = artifact::verify(&needed_artifacts(&plans), &base).await;
    if let Err(error) = output.write_all(verification_text(subject_os, &results).as_bytes()) {
        error!("Cannot write artifact verification report: {error}");
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }
    let n = results.len();
    let failed = results.iter().filter(|result| result.identity.is_err()).count();
    if failed == 0 {
        info!("Verified {n} artifact(s).");
        ExitCode::SUCCESS
    } else {
        error!("{failed} of {n} artifact(s) failed verification.");
        ExitCode::from(EXIT_HARNESS_ERROR)
    }
}

fn verification_text(subject_os: Os, results: &[ArtifactVerification]) -> String {
    if results.is_empty() {
        return format!("No artifacts needed on {}.\n", subject_os.name());
    }
    let mut text = format!("Artifacts needed on {}:\n", subject_os.name());
    for result in results {
        let binding = &result.binding;
        let identity = match &result.identity {
            Ok(identity) => identity.clone(),
            Err(error) => format!("FAILED: {error}"),
        };
        writeln!(
            text,
            "  {} = {} ({}): {} {identity}",
            binding.role.name,
            binding.reference,
            binding.source.name(),
            binding.role.kind.name(),
        )
        .unwrap();
    }
    text
}

#[cfg(test)]
mod tests {
    use panoramic_core::artifact::{
        test_util::write_executable, ArtifactBinding, ArtifactKind, ArtifactRole, BindingSource,
    };
    use tracing::instrument::WithSubscriber as _;

    use super::*;
    use crate::config::tests::{write_case, PARTICIPANT_FIELDS};

    async fn verify_with_logs(dirs: Vec<PathBuf>, artifacts: Option<PathBuf>) -> (ExitCode, String, String) {
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        let mut report = Vec::new();
        let exit = verify(
            VerifyCommand {
                test_dirs: dirs,
                subject_os: Os::MacOs,
                artifacts,
            },
            &mut report,
        )
        .with_subscriber(subscriber)
        .await;
        (
            exit,
            String::from_utf8(report).unwrap(),
            std::fs::read_to_string(output.path()).unwrap(),
        )
    }

    #[tokio::test]
    async fn artifacts_verify_succeeds_for_all_needed_roles_and_fails_if_any_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]");
        write_case(&cases, "first", &yaml);
        write_case(&cases, "second", &yaml);
        let binary = dir.path().join("tool");
        let install = dir.path().join("install");
        write_executable(&binary);
        write_executable(&install.join("bin/agent/agent"));
        let manifest = dir.path().join("artifacts.toml");
        std::fs::write(
            &manifest,
            format!(
                "schema = 1\n[roles]\nadp-host-binary = '{}'\nagent-host-install = '{}'\n",
                binary.display(),
                install.display(),
            ),
        )
        .unwrap();
        for missing in 0..=2 {
            match missing {
                1 => std::fs::remove_file(&binary).unwrap(),
                2 => std::fs::remove_file(install.join("bin/agent/agent")).unwrap(),
                _ => {}
            }
            let (exit, report, log) = verify_with_logs(vec![cases.clone()], Some(manifest.clone())).await;
            let expected_exit = if missing == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
            assert_eq!(exit, expected_exit, "{log}");
            let summary = if missing == 0 {
                "Verified 2 artifact(s).".to_string()
            } else {
                format!("{missing} of 2 artifact(s) failed verification.")
            };
            assert!(log.contains(&summary), "{log}");
            let lines: Vec<_> = report.lines().collect();
            assert_eq!(lines.len(), 3, "{report}");
            assert_eq!(lines[0], "Artifacts needed on macos:");
            for (line, (role, path, kind, failed)) in lines[1..].iter().zip([
                ("adp-host-binary", &binary, "binary", missing >= 1),
                ("agent-host-install", &install, "install", missing >= 2),
            ]) {
                let prefix = format!("  {role} = {} (manifest): {kind} ", path.display());
                let outcome = line.strip_prefix(&prefix).unwrap_or_else(|| panic!("{line}"));
                if failed {
                    assert!(outcome.starts_with("FAILED: "), "{line}");
                    assert!(outcome.contains(path.to_str().unwrap()), "{line}");
                } else {
                    assert_eq!(
                        outcome,
                        "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn artifacts_verify_validates_manifests_before_discovery_even_without_cases() {
        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        std::fs::create_dir(&cases).unwrap();
        for populated in [false, true] {
            if populated {
                write_case(&cases, "invalid", r#"type: bogus"#);
            }
            for (file, content, problem) in [
                ("missing.toml", None, "failed to read artifact manifest"),
                ("malformed.toml", Some("not toml"), "TOML parse error"),
                (
                    "unknown.toml",
                    Some("schema = 1\n[roles]\nbogus = 'x'"),
                    "no participant type or backend declares role 'bogus'",
                ),
            ] {
                let manifest = dir.path().join(file);
                if let Some(content) = content {
                    std::fs::write(&manifest, content).unwrap();
                }
                let (exit, report, log) = verify_with_logs(vec![cases.clone()], Some(manifest.clone())).await;
                assert_eq!(exit, ExitCode::from(2));
                assert!(report.is_empty(), "{report}");
                assert!(log.contains(problem), "{log}");
                assert!(log.contains(manifest.to_str().unwrap()), "{log}");
                assert!(!log.contains("Discovering"), "{log}");
            }
        }
    }

    #[tokio::test]
    async fn artifacts_verify_requires_test_directories() {
        let (exit, report, log) = verify_with_logs(vec![], None).await;
        assert_eq!(exit, ExitCode::from(2));
        assert!(report.is_empty(), "{report}");
        assert!(log.contains("No test directories specified."), "{log}");
        assert!(!log.contains("Discovering"), "{log}");
    }

    #[tokio::test]
    async fn artifacts_verify_needs_nothing_for_typed_cases_or_no_cases() {
        let dir = tempfile::tempdir().unwrap();
        for populated in [false, true] {
            if populated {
                write_case(
                    dir.path(),
                    "typed",
                    r#"
type: integration
name: typed
timeout: 1s
runtimes: [mac]
procedure: []
"#,
                );
            }
            let (exit, report, log) = verify_with_logs(vec![dir.path().to_path_buf()], None).await;
            assert_eq!(exit, ExitCode::SUCCESS, "{log}");
            assert_eq!(report, "No artifacts needed on macos.\n");
            assert!(log.contains("Verified 0 artifact(s)."), "{log}");
        }
    }

    #[tokio::test]
    async fn artifacts_verify_exits_2_on_discovery_and_compilation_errors() {
        let dir = tempfile::tempdir().unwrap();
        for yaml in [
            r#"type: bogus"#.to_string(),
            PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}"),
        ] {
            write_case(dir.path(), "invalid", &yaml);
            let (exit, report, log) = verify_with_logs(vec![dir.path().to_path_buf()], None).await;
            assert_eq!(exit, ExitCode::from(2));
            assert!(report.is_empty(), "{report}");
            assert!(log.contains("Failed to discover tests:"), "{log}");
            assert!(!log.contains("Verified"), "{log}");
        }
    }

    #[tokio::test]
    async fn artifacts_verify_exits_2_if_the_report_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut output = std::io::Cursor::new(&mut [] as &mut [u8]);
        assert_eq!(
            verify(
                VerifyCommand {
                    test_dirs: vec![dir.path().to_path_buf()],
                    subject_os: Os::MacOs,
                    artifacts: None,
                },
                &mut output
            )
            .await,
            ExitCode::from(2)
        );
    }

    #[test]
    fn artifact_verification_text_prints_references_sources_kinds_and_outcomes() {
        let results = [
            ArtifactVerification {
                binding: ArtifactBinding {
                    role: ArtifactRole {
                        name: "adp-host-binary",
                        kind: ArtifactKind::Binary,
                        local_default: "target/release/agent-data-plane",
                    },
                    reference: "target/release/agent-data-plane".into(),
                    source: BindingSource::LocalDefault,
                },
                identity: Ok("sha256:3b1f".into()),
            },
            ArtifactVerification {
                binding: ArtifactBinding {
                    role: ArtifactRole {
                        name: "agent-host-install",
                        kind: ArtifactKind::Install {
                            main_binary: "bin/agent/agent",
                        },
                        local_default: "/tmp/saluki-dda/datadog-agent",
                    },
                    reference: "/opt/x".into(),
                    source: BindingSource::Manifest,
                },
                identity: Err("'/opt/x' is not a directory".into()),
            },
            ArtifactVerification {
                binding: ArtifactBinding {
                    role: ArtifactRole {
                        name: "fixup-image",
                        kind: ArtifactKind::Image,
                        local_default: "alpine:latest",
                    },
                    reference: "alpine:latest".into(),
                    source: BindingSource::LocalDefault,
                },
                identity: Ok("sha256:123".into()),
            },
        ];
        assert_eq!(
            verification_text(Os::MacOs, &results),
            concat!(
                "Artifacts needed on macos:\n",
                "  adp-host-binary = target/release/agent-data-plane (local default): binary sha256:3b1f\n",
                "  agent-host-install = /opt/x (manifest): install FAILED: '/opt/x' is not a directory\n",
                "  fixup-image = alpine:latest (local default): image sha256:123\n",
            )
        );
        assert_eq!(verification_text(Os::Linux, &[]), "No artifacts needed on linux.\n");
    }
}
