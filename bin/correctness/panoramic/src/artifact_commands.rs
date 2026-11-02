//! Commands for writing manifests, verifying artifacts and pulling images selected cases need.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::Write,
    path::PathBuf,
    process::ExitCode,
};

use panoramic_core::{
    artifact::{self, ArtifactBinding, ArtifactManifest, ArtifactVerification},
    backend::RunBackends,
    layout::Os,
    plan::{check_artifact_manifest, needed_artifacts},
};
use tracing::{error, info};

use crate::{
    cli::{ArtifactCasesCommand, RenderCommand},
    collector::EXIT_HARNESS_ERROR,
    console::{Console, SetupFailure},
    discovery::prepare_discovery,
};

pub(crate) fn render(cmd: RenderCommand) -> ExitCode {
    let mut roles = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for (name, reference) in cmd.set {
        if roles.insert(name.clone(), reference).is_some() {
            duplicates.insert(name);
        }
    }
    if !duplicates.is_empty() {
        let messages: Vec<_> = duplicates
            .iter()
            .map(|name| format!("--set binds role '{name}' more than once"))
            .collect();
        error!("Invalid artifact roles: {}", messages.join("; "));
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }
    let n = roles.len();
    let manifest = match ArtifactManifest::new("--set", roles).and_then(|manifest| {
        check_artifact_manifest(crate::REGISTRIES, &manifest)?;
        Ok(manifest)
    }) {
        Ok(manifest) => manifest,
        Err(error) => {
            error!("Invalid artifact roles: {error:#}");
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };
    if let Err(error) = std::fs::write(&cmd.output, manifest.render()) {
        error!("Cannot write artifact manifest {}: {error}", cmd.output.display());
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }
    info!("Wrote artifact manifest {} binding {n} role(s).", cmd.output.display());
    ExitCode::SUCCESS
}

fn needed_bindings(cmd: &ArtifactCasesCommand) -> Result<(Os, Vec<ArtifactBinding>, PathBuf), SetupFailure> {
    let prepared = prepare_discovery(&cmd.test_dirs, Ok(cmd.subject_os), cmd.artifacts.as_deref())?;
    let base = std::env::current_dir()
        .map_err(|error| SetupFailure::harness(format!("Cannot read the current directory: {error}")))?;
    let selection = prepared.discover_selection(&Console { use_tui: false }, true)?;
    Ok((
        selection.subject_os,
        needed_artifacts(&selection.discovered.plans),
        base,
    ))
}

pub(crate) async fn verify(cmd: ArtifactCasesCommand, output: &mut impl Write) -> ExitCode {
    let (subject_os, bindings, base) = match needed_bindings(&cmd) {
        Ok(selection) => selection,
        Err(failure) => return failure.report(&Console { use_tui: false }),
    };
    let results = RunBackends::default().verify(&bindings, &base).await;
    if let Err(error) = output.write_all(verification_text(subject_os, &results, ("Artifacts", "artifacts")).as_bytes())
    {
        error!("Cannot write artifact verification report: {error}");
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }
    let n = bindings.len();
    match artifact::verified_artifacts(&results) {
        Ok(_) => {
            info!("Verified {n} artifact(s).");
            ExitCode::SUCCESS
        }
        Err(failed) => {
            error!("{failed} of {n} artifact(s) failed verification.");
            ExitCode::from(EXIT_HARNESS_ERROR)
        }
    }
}

pub(crate) async fn pull(cmd: ArtifactCasesCommand, output: &mut impl Write) -> ExitCode {
    let (subject_os, bindings, _) = match needed_bindings(&cmd) {
        Ok(selection) => selection,
        Err(failure) => return failure.report(&Console { use_tui: false }),
    };
    let results = RunBackends::default().pull(&bindings).await;
    if let Err(error) = output.write_all(verification_text(subject_os, &results, ("Images", "images")).as_bytes()) {
        error!("Cannot write image pull report: {error}");
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }
    let n = results.len();
    let failed = results.iter().filter(|result| result.identity.is_err()).count();
    if failed == 0 {
        info!("Pulled {n} image(s).");
        ExitCode::SUCCESS
    } else {
        error!("{failed} of {n} image(s) failed to pull.");
        ExitCode::from(EXIT_HARNESS_ERROR)
    }
}

pub(crate) fn verification_text(subject_os: Os, results: &[ArtifactVerification], nouns: (&str, &str)) -> String {
    let (heading, noun) = nouns;
    if results.is_empty() {
        return format!("No {noun} needed on {}.\n", subject_os.name());
    }
    let mut text = format!("{heading} needed on {}:\n", subject_os.name());
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
    use crate::config::{
        discover_in_scope,
        tests::{planner, write_case, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
    };

    #[derive(Clone, Copy)]
    enum Subcommand {
        Verify,
        Pull,
    }

    async fn artifact_command_with_logs(
        dirs: Vec<PathBuf>, artifacts: Option<PathBuf>, subcommand: Subcommand,
    ) -> (ExitCode, String, String) {
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        let mut report = Vec::new();
        let cmd = ArtifactCasesCommand {
            test_dirs: dirs,
            subject_os: Os::MacOs,
            artifacts,
        };
        let exit = async {
            match subcommand {
                Subcommand::Verify => verify(cmd, &mut report).await,
                Subcommand::Pull => pull(cmd, &mut report).await,
            }
        }
        .with_subscriber(subscriber)
        .await;
        (
            exit,
            String::from_utf8(report).unwrap(),
            std::fs::read_to_string(output.path()).unwrap(),
        )
    }

    fn render_with_logs(cmd: RenderCommand) -> (ExitCode, String) {
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        let exit = tracing::subscriber::with_default(subscriber, || render(cmd));
        (exit, std::fs::read_to_string(output.path()).unwrap())
    }

    #[track_caller]
    fn assert_render_rejected(pairs: &[(&str, &str)], problem: &str) {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("artifacts.toml");
        for existing in [false, true] {
            if existing {
                std::fs::write(&output, "keep this content").unwrap();
            }
            let (exit, log) = render_with_logs(RenderCommand {
                set: pairs
                    .iter()
                    .map(|(role, reference)| (role.to_string(), reference.to_string()))
                    .collect(),
                output: output.clone(),
            });
            assert_eq!(exit, ExitCode::from(2), "{log}");
            assert!(log.contains(&format!("Invalid artifact roles: {problem}")), "{log}");
            assert!(!log.contains("Wrote artifact manifest"), "{log}");
            if existing {
                assert_eq!(std::fs::read_to_string(&output).unwrap(), "keep this content");
            } else {
                assert!(!output.exists());
            }
        }
    }

    #[tokio::test]
    async fn artifacts_render_writes_a_manifest_that_verify_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(&cases, "host", &PARTICIPANT_FIELDS.replace("[linux]", "[macos]"));
        let binary = dir.path().join("tool");
        let install = dir.path().join("install");
        write_executable(&binary);
        write_executable(&install.join("bin/agent/agent"));
        let manifest = dir.path().join("artifacts.toml");
        let (exit, log) = render_with_logs(RenderCommand {
            set: vec![
                ("adp-host-binary".into(), binary.to_str().unwrap().into()),
                ("agent-host-install".into(), install.to_str().unwrap().into()),
            ],
            output: manifest.clone(),
        });
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert!(
            log.contains(&format!(
                "Wrote artifact manifest {} binding 2 role(s).",
                manifest.display()
            )),
            "{log}"
        );
        let (exit, report, log) = artifact_command_with_logs(vec![cases], Some(manifest), Subcommand::Verify).await;
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert_eq!(
            report,
            format!(
                "Artifacts needed on macos:\n  adp-host-binary = {} (manifest): binary sha256:{hash}\n  \
                 agent-host-install = {} (manifest): install sha256:{hash}\n",
                binary.display(),
                install.display(),
                hash = "7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d",
            )
        );
        assert!(log.contains("Verified 2 artifact(s)."), "{log}");
    }

    #[test]
    fn artifacts_render_accepts_the_tools_role_declared_by_typed_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("artifacts.toml");
        let (exit, log) = render_with_logs(RenderCommand {
            set: vec![("tools-linux-image".into(), "tools:custom".into())],
            output: output.clone(),
        });
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert_eq!(
            ArtifactManifest::load(&output).unwrap().render(),
            "schema = 1\n\n[roles]\ntools-linux-image = \"tools:custom\"\n"
        );
    }

    #[test]
    fn artifacts_render_with_no_pairs_writes_a_manifest_binding_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("artifacts.toml");
        let (exit, log) = render_with_logs(RenderCommand {
            set: vec![],
            output: output.clone(),
        });
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert_eq!(
            ArtifactManifest::load(&output).unwrap().render(),
            ArtifactManifest::default().render()
        );
        assert!(log.contains("binding 0 role(s)."), "{log}");
    }

    #[test]
    fn artifacts_render_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("artifacts.toml");
        std::fs::write(&output, "old content that is not a manifest").unwrap();
        let (exit, log) = render_with_logs(RenderCommand {
            set: vec![("fixup-image".into(), "alpine:3".into())],
            output: output.clone(),
        });
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "schema = 1\n\n[roles]\nfixup-image = \"alpine:3\"\n"
        );
    }

    #[test]
    fn artifacts_render_reports_every_unknown_role_and_writes_nothing() {
        assert_render_rejected(
            &[("z", "z"), ("a", "a")],
            "--set: no participant type or backend declares role 'a', 'z'; declared roles: \
             adp-host-binary, agent-adp-linux-image, agent-adp-windows-image, agent-host-install, \
             courier-linux-binary, fixup-image",
        );
        assert_render_rejected(&[("", "x")], "--set: no participant type or backend declares role '';");
    }

    #[test]
    fn artifacts_render_rejects_a_role_bound_twice_and_writes_nothing() {
        for second in ["alpine:3", "alpine:latest"] {
            assert_render_rejected(
                &[("fixup-image", "alpine:3"), ("fixup-image", second)],
                "--set binds role 'fixup-image' more than once",
            );
        }
        assert_render_rejected(
            &[("z", ""), ("a", "x"), ("z", ""), ("a", "x"), ("z", "")],
            "--set binds role 'a' more than once; --set binds role 'z' more than once",
        );
    }

    #[test]
    fn artifacts_render_rejects_an_empty_reference_and_writes_nothing() {
        for reference in ["", "  "] {
            assert_render_rejected(
                &[("fixup-image", reference)],
                "--set: role 'fixup-image' has an empty reference",
            );
        }
        assert_render_rejected(&[("unknown", "")], "--set: role 'unknown' has an empty reference");
    }

    #[test]
    fn artifacts_render_exits_2_if_the_manifest_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("missing/artifacts.toml");
        let (exit, log) = render_with_logs(RenderCommand {
            set: vec![],
            output: output.clone(),
        });
        assert_eq!(exit, ExitCode::from(2));
        assert!(
            log.contains(&format!("Cannot write artifact manifest {}:", output.display())),
            "{log}"
        );
        assert!(!output.exists());
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
            let (exit, report, log) =
                artifact_command_with_logs(vec![cases.clone()], Some(manifest.clone()), Subcommand::Verify).await;
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
    async fn artifacts_verify_reports_typed_integration_host_roles_like_participant_cases() {
        let dir = tempfile::tempdir().unwrap();
        let participants = dir.path().join("participants");
        let typed = dir.path().join("typed");
        for name in ["first", "second"] {
            write_case(&participants, name, &PARTICIPANT_FIELDS.replace("[linux]", "[macos]"));
            write_case(
                &typed,
                name,
                &format!("type: integration\nname: {name}\ntimeout: 1s\nruntimes: [mac]\nprocedure: []\n"),
            );
        }
        let binary = dir.path().join("tool");
        let install = dir.path().join("install");
        write_executable(&binary);
        write_executable(&install.join("bin/agent/agent"));
        let manifest = dir.path().join("artifacts.toml");
        std::fs::write(
            &manifest,
            ArtifactManifest::new(
                "test",
                BTreeMap::from([
                    ("adp-host-binary".into(), binary.to_str().unwrap().to_string()),
                    ("agent-host-install".into(), install.to_str().unwrap().to_string()),
                ]),
            )
            .unwrap()
            .render(),
        )
        .unwrap();
        for missing in 0..=2 {
            match missing {
                1 => std::fs::remove_file(&binary).unwrap(),
                2 => std::fs::remove_file(install.join("bin/agent/agent")).unwrap(),
                _ => {}
            }
            let (participant_exit, participant_report, participant_log) =
                artifact_command_with_logs(vec![participants.clone()], Some(manifest.clone()), Subcommand::Verify)
                    .await;
            let (typed_exit, typed_report, typed_log) =
                artifact_command_with_logs(vec![typed.clone()], Some(manifest.clone()), Subcommand::Verify).await;
            let expected_exit = if missing == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
            assert_eq!(participant_exit, expected_exit, "{participant_log}");
            assert_eq!(typed_exit, expected_exit, "{typed_log}");
            assert_eq!(typed_report, participant_report);
            assert_eq!(typed_report.lines().count(), 3, "{typed_report}");
            assert!(typed_report.contains("adp-host-binary"), "{typed_report}");
            assert!(typed_report.contains("agent-host-install"), "{typed_report}");
            assert_eq!(typed_report.matches("FAILED:").count(), missing, "{typed_report}");
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
                let (exit, report, log) =
                    artifact_command_with_logs(vec![cases.clone()], Some(manifest.clone()), Subcommand::Verify).await;
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
        let (exit, report, log) = artifact_command_with_logs(vec![], None, Subcommand::Verify).await;
        assert_eq!(exit, ExitCode::from(2));
        assert!(report.is_empty(), "{report}");
        assert!(log.contains("No test directories specified."), "{log}");
        assert!(!log.contains("Discovering"), "{log}");
    }

    #[tokio::test]
    async fn artifacts_verify_needs_nothing_for_no_cases() {
        let dir = tempfile::tempdir().unwrap();
        let (exit, report, log) =
            artifact_command_with_logs(vec![dir.path().to_path_buf()], None, Subcommand::Verify).await;
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert_eq!(report, "No artifacts needed on macos.\n");
        assert!(log.contains("Verified 0 artifact(s)."), "{log}");
    }

    #[tokio::test]
    async fn artifacts_verify_and_pull_omit_correctness_roles_off_linux() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        for os in [Os::MacOs, Os::Windows] {
            let cmd = ArtifactCasesCommand {
                test_dirs: vec![dir.path().to_path_buf()],
                subject_os: os,
                artifacts: None,
            };
            let mut report = Vec::new();
            assert_eq!(verify(cmd, &mut report).await, ExitCode::SUCCESS);
            assert_eq!(
                String::from_utf8(report).unwrap(),
                format!("No artifacts needed on {}.\n", os.name())
            );
            let cmd = ArtifactCasesCommand {
                test_dirs: vec![dir.path().to_path_buf()],
                subject_os: os,
                artifacts: None,
            };
            let mut report = Vec::new();
            assert_eq!(pull(cmd, &mut report).await, ExitCode::SUCCESS);
            assert_eq!(
                String::from_utf8(report).unwrap(),
                format!("No images needed on {}.\n", os.name())
            );
        }
    }

    #[test]
    fn artifacts_selection_includes_typed_roles_only_for_the_selected_os() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        write_case(
            dir.path(),
            "integration",
            "type: integration\nname: integration\ntimeout: 1s\nruntimes: [linux, mac, windows]\nprocedure: []\n",
        );
        for (os, expected) in [
            (
                Os::Linux,
                vec![
                    "agent-adp-linux-image",
                    "courier-linux-binary",
                    "fixup-image",
                    "tools-linux-image",
                ],
            ),
            (Os::MacOs, vec!["adp-host-binary", "agent-host-install"]),
            (Os::Windows, vec!["agent-adp-windows-image"]),
        ] {
            let plans = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os)
                .unwrap()
                .plans;
            assert_eq!(plans.len(), if os == Os::Linux { 2 } else { 1 });
            assert_eq!(
                needed_artifacts(&plans)
                    .iter()
                    .map(|binding| binding.role.name)
                    .collect::<Vec<_>>(),
                expected
            );
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
            let (exit, report, log) =
                artifact_command_with_logs(vec![dir.path().to_path_buf()], None, Subcommand::Verify).await;
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
                ArtifactCasesCommand {
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

    #[tokio::test]
    async fn artifacts_pull_reports_no_images_needed_for_cases_needing_only_files() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "host", &PARTICIPANT_FIELDS.replace("[linux]", "[macos]"));
        let (_, report, _) = artifact_command_with_logs(vec![dir.path().to_path_buf()], None, Subcommand::Verify).await;
        assert!(report.contains("adp-host-binary"), "{report}");
        assert!(report.contains("agent-host-install"), "{report}");
        let (exit, report, log) =
            artifact_command_with_logs(vec![dir.path().to_path_buf()], None, Subcommand::Pull).await;
        assert_eq!(exit, ExitCode::SUCCESS, "{log}");
        assert_eq!(report, "No images needed on macos.\n");
        assert!(log.contains("Pulled 0 image(s)."), "{log}");
    }

    #[tokio::test]
    async fn artifacts_pull_fails_like_verify_before_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "invalid",
            &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}"),
        );
        let unknown = dir.path().join("unknown.toml");
        std::fs::write(&unknown, "schema = 1\n[roles]\nbogus = 'x'").unwrap();
        let malformed = dir.path().join("malformed.toml");
        std::fs::write(&malformed, "not toml").unwrap();
        for (dirs, manifest, problem, discovers) in [
            (vec![], None, "No test directories specified.", false),
            (
                vec![cases.clone()],
                Some(unknown),
                "no participant type or backend declares role 'bogus'",
                false,
            ),
            (vec![cases.clone()], Some(malformed), "TOML parse error", false),
            (vec![cases], None, "Failed to discover tests:", true),
        ] {
            let (verify_exit, verify_report, verify_log) =
                artifact_command_with_logs(dirs.clone(), manifest.clone(), Subcommand::Verify).await;
            let (exit, report, log) = artifact_command_with_logs(dirs, manifest, Subcommand::Pull).await;
            assert_eq!(exit, ExitCode::from(2));
            assert_eq!(exit, verify_exit);
            assert!(report.is_empty(), "{report}");
            assert_eq!(report, verify_report);
            assert_eq!(log, verify_log);
            assert!(log.contains(problem), "{log}");
            assert_eq!(log.contains("Discovering"), discovers, "{log}");
            assert!(!log.contains("Pulled"), "{log}");
        }
    }

    #[tokio::test]
    async fn artifacts_pull_exits_2_if_the_report_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut output = std::io::Cursor::new(&mut [] as &mut [u8]);
        assert_eq!(
            pull(
                ArtifactCasesCommand {
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
    fn pull_text_lists_each_image_with_its_identity_or_failure() {
        let results = [
            ArtifactVerification {
                binding: ArtifactBinding {
                    role: ArtifactRole {
                        name: "agent-adp-linux-image",
                        kind: ArtifactKind::Image { os: Os::Linux },
                        local_default: "unused",
                    },
                    reference: "busybox:1.36.1".into(),
                    source: BindingSource::Manifest,
                },
                identity: Ok("sha256:73aa".into()),
            },
            ArtifactVerification {
                binding: ArtifactBinding {
                    role: ArtifactRole {
                        name: "fixup-image",
                        kind: ArtifactKind::Image { os: Os::Linux },
                        local_default: "unused",
                    },
                    reference: "busybox:nope".into(),
                    source: BindingSource::Manifest,
                },
                identity: Err("cannot pull image 'busybox:nope': pull access denied".into()),
            },
        ];
        assert_eq!(verification_text(Os::Linux, &results, ("Images", "images")), concat!(
            "Images needed on linux:\n",
            "  agent-adp-linux-image = busybox:1.36.1 (manifest): image sha256:73aa\n",
            "  fixup-image = busybox:nope (manifest): image FAILED: cannot pull image 'busybox:nope': pull access denied\n",
        ));
        assert_eq!(
            verification_text(Os::Linux, &[], ("Images", "images")),
            "No images needed on linux.\n"
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
                        kind: ArtifactKind::Image { os: Os::Linux },
                        local_default: "alpine:latest",
                    },
                    reference: "alpine:latest".into(),
                    source: BindingSource::LocalDefault,
                },
                identity: Ok("sha256:123".into()),
            },
        ];
        assert_eq!(
            verification_text(Os::MacOs, &results, ("Artifacts", "artifacts")),
            concat!(
                "Artifacts needed on macos:\n",
                "  adp-host-binary = target/release/agent-data-plane (local default): binary sha256:3b1f\n",
                "  agent-host-install = /opt/x (manifest): install FAILED: '/opt/x' is not a directory\n",
                "  fixup-image = alpine:latest (local default): image sha256:123\n",
            )
        );
        assert_eq!(
            verification_text(Os::Linux, &[], ("Artifacts", "artifacts")),
            "No artifacts needed on linux.\n"
        );
    }
}
