use std::{borrow::Borrow, collections::BTreeMap, fmt::Write as _, process::ExitCode};

use panoramic_core::{layout::Os, machine_output::skip_reason};
use saluki_error::GenericError;
use tracing::info;

use crate::{
    cli,
    config::{
        build_runners,
        runner_artifacts::RunnerArtifacts,
        typed_case::{layout_entries, LoadedCase, LoadedPlan},
        BuiltCase, Discovered,
    },
    console::{Console, SetupFailure},
    discovery::{prepare_discovery, Selection},
};

pub(crate) async fn list_tests(cmd: cli::ListCommand) -> ExitCode {
    let console = Console { use_tui: false };
    let Selection {
        subject_os,
        discovered: Discovered { plans, excluded },
    } = match prepare_discovery(&cmd.test_dirs, cmd.subject_os.resolve(), cmd.artifacts.as_deref())
        .and_then(|prepared| prepared.discover_selection(&console, !cmd.json))
    {
        Ok(selection) => selection,
        Err(failure) => return failure.report(&console),
    };

    if cmd.json {
        let entries = match json_listing(&plans, &excluded, subject_os) {
            Ok(entries) => entries,
            Err(error) => return SetupFailure::discovery(error).report(&console),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).expect("Unable to serialize a map of the tests")
        )
    } else {
        if plans.is_empty() && excluded.is_empty() {
            info!("No test cases found.");
            return ExitCode::SUCCESS;
        }

        let count = plans.len() + excluded.len();
        let skipped = if excluded.is_empty() {
            String::new()
        } else {
            format!(" ({} skipped)", excluded.len())
        };
        info!(
            "Discovered {count} test case(s) for subject OS '{}'{skipped}.",
            subject_os.name()
        );

        print!("{}", listing_report(&plans, &excluded));
    }
    ExitCode::SUCCESS
}

/// Formats every enabled case in name order under a total count.
fn listing_report(plans: &[LoadedPlan], excluded: &[LoadedCase]) -> String {
    let count = plans.len() + excluded.len();
    let mut text = format!("\nAvailable tests ({count}):\n\n");
    let mut entries: Vec<_> = plans
        .iter()
        .map(|plan| (plan.case().name(), listing_text(plan)))
        .chain(excluded.iter().map(|case| (case.name(), excluded_listing_text(case))))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (_, entry) in entries {
        text.push_str(&entry);
    }
    text
}

/// Formats a case's name, description, timeout and status.
fn listing_header(case: &LoadedCase, status: &str) -> String {
    let mut text = format!("  {}\n", case.name());
    if let Some(description) = case.description() {
        writeln!(text, "    {description}").unwrap();
    }
    writeln!(text, "    Timeout: {:?}", case.timeout()).unwrap();
    writeln!(text, "    Status: {status}").unwrap();
    text
}

/// Formats one case's text listing, including its layout and resolved artifact references with their sources.
fn listing_text(plan: &LoadedPlan) -> String {
    let mut text = listing_header(plan.case(), "runnable");
    if let Some(layout) = plan.layout() {
        let participants: Vec<_> = layout
            .iter()
            .map(|(name, realization)| format!("{name} on {} as {}", realization.os.name(), realization.kind.name()))
            .collect();
        writeln!(text, "    Layout: {}", participants.join("; ")).unwrap();
    }
    let artifacts = plan.artifacts();
    if artifacts.is_empty() {
        writeln!(text, "    Artifact roles: none").unwrap();
    } else {
        writeln!(text, "    Artifact roles:").unwrap();
        for binding in artifacts {
            let source = binding.source.name();
            writeln!(text, "      {} = {} ({source})", binding.role.name, binding.reference).unwrap();
        }
    }
    if plan.layout().is_some() {
        for (label, order) in [("Create", plan.create_order()), ("Start", plan.start_order())] {
            let levels = order
                .expect("participant layouts have dependency orders")
                .iter()
                .map(|level| level.join(", "))
                .collect::<Vec<_>>();
            writeln!(text, "    {label} order: {}", levels.join(" then ")).unwrap();
        }
    }
    text.push('\n');
    text
}

/// Formats an excluded case's metadata and skip status.
fn excluded_listing_text(case: &LoadedCase) -> String {
    let mut text = listing_header(case, "skipped (subject_os)");
    text.push('\n');
    text
}

fn json_listing(
    plans: &[LoadedPlan], excluded: &[LoadedCase], subject_os: Os,
) -> Result<BTreeMap<String, serde_json::Value>, GenericError> {
    let built = build_runners(plans, &RunnerArtifacts::Overrides(&[]))?;
    Ok(listing(&built, excluded, subject_os))
}

/// Lists every enabled case's status, layout and roles, with runtime and images for cases using runners.
pub(crate) fn listing<P>(
    built: &[BuiltCase<P>], excluded: &[LoadedCase], subject_os: Os,
) -> BTreeMap<String, serde_json::Value>
where
    P: Borrow<LoadedPlan>,
{
    built
        .iter()
        .map(|built| {
            let plan = built.plan.borrow();
            let case = plan.case();
            let roles: Vec<_> = plan
                .artifacts()
                .iter()
                .map(|binding| {
                    serde_json::json!({
                        "role": binding.role.name,
                        "kind": binding.role.kind.name(),
                        "reference": binding.reference,
                        "source": binding.source,
                    })
                })
                .collect();
            let mut row = serde_json::json!({
                "type": case.suite(),
                "timeout": case.timeout(),
                "status": "runnable",
                "layout": layout_entries(plan),
                "roles": roles,
            });
            if let Some(runner) = built.runner() {
                row["runtime"] = serde_json::json!(runner.runtime());
                row["images"] = serde_json::json!(runner.images());
            }
            (case.name().to_string(), row)
        })
        .chain(excluded.iter().map(|case| {
            (
                case.name().to_string(),
                serde_json::json!({
                    "type": case.suite(),
                    "timeout": case.timeout(),
                    "status": "skipped",
                    "skip_reason": skip_reason(subject_os),
                    "layout": null,
                    "roles": null,
                }),
            )
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use panoramic_core::{artifact::ArtifactManifest, plan::Planner};

    use super::*;
    use crate::{
        cli::{Cli, Command},
        collector::EXIT_HARNESS_ERROR,
        config::{
            discover_in_scope, load_planner,
            tests::{build_case_runners, planner, write_case, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
        },
        REGISTRIES,
    };

    #[test]
    fn listing_json_reports_status_layout_and_roles_for_runnable_cases() {
        let participant = crate::config::tests::plan(PARTICIPANT_FIELDS, Os::Linux);
        let typed = crate::config::tests::plan(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux);
        for (plan, expected_layout, expected_keys) in [
            (
                participant,
                serde_json::json!([{
                    "participant": "adp", "type": "agent-adp", "os": "linux", "realization": "linux-container",
                }]),
                vec!["layout", "roles", "status", "timeout", "type"],
            ),
            (
                typed,
                serde_json::json!([{
                    "participant": null, "type": null, "os": "linux", "realization": "linux-container",
                }]),
                vec!["images", "layout", "roles", "runtime", "status", "timeout", "type"],
            ),
        ] {
            let built = build_runners([&plan], &RunnerArtifacts::Overrides(&[])).unwrap();
            let entries = listing(&built, &[], Os::Linux);
            let entry = &entries["case"];
            assert_eq!(entry["status"], "runnable");
            assert_eq!(entry["layout"], expected_layout);
            assert_eq!(
                entry
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                expected_keys
            );
            let mut expected_roles = serde_json::json!([
                {
                    "role": "agent-adp-linux-image",
                    "kind": "image",
                    "reference": "saluki-images/datadog-agent:testing-devel",
                    "source": "local_default",
                },
                {
                    "role": "courier-linux-binary",
                    "kind": "binary",
                    "reference": match std::env::consts::ARCH {
                        "aarch64" => "target/aarch64-unknown-linux-musl/release/panoramic-courier",
                        "x86_64" => "target/x86_64-unknown-linux-musl/release/panoramic-courier",
                        _ => "",
                    },
                    "source": "local_default",
                },
                {
                    "role": "fixup-image",
                    "kind": "image",
                    "reference": "alpine:latest",
                    "source": "local_default",
                },
            ]);
            if plan.case().suite() == "correctness" {
                expected_roles.as_array_mut().unwrap().push(serde_json::json!({
                    "role": "tools-linux-image", "kind": "image",
                    "reference": "saluki-images/correctness-tools:latest", "source": "local_default",
                }));
            }
            assert_eq!(entry["roles"], expected_roles);
        }
    }

    #[test]
    fn list_json_reports_native_rows_without_runtime_or_images() {
        let plans = [
            crate::config::tests::plan_with_plain(
                &PARTICIPANT_FIELDS.replace("agent-adp", "plain"),
                "native",
                Os::Linux,
            ),
            crate::config::tests::plan(PARTICIPANT_FIELDS, Os::Linux),
        ];
        let entries = json_listing(&plans, &[], Os::Linux).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries["case"].get("runtime").is_none());
        assert!(entries["case"].get("images").is_none());
        let native = &entries["native"];
        assert_eq!(
            native
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["layout", "roles", "status", "timeout", "type"]
        );
        assert_eq!(native["status"], "runnable");
        assert_eq!(native["type"], "integration");
        assert_eq!(native["timeout"], serde_json::json!({"secs": 1, "nanos": 0}));
        assert_eq!(
            native["layout"],
            serde_json::json!([{
                "participant": "adp", "type": "plain", "os": "linux", "realization": "linux-container",
            }])
        );
        assert_eq!(
            native["roles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|role| role["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["courier-linux-binary", "fixup-image"]
        );
    }

    #[test]
    fn listing_json_reports_skipped_cases_with_their_reason_and_without_runtime_or_images() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "participant", PARTICIPANT_FIELDS);
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let Discovered { plans, excluded } =
            discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::MacOs).unwrap();
        assert!(plans.is_empty());
        let built = build_runners(&plans, &RunnerArtifacts::Overrides(&[])).unwrap();
        let entries = listing(&built, &excluded, Os::MacOs);
        assert_eq!(entries.len(), 2);
        for case in &excluded {
            assert_eq!(
                entries[case.name()],
                serde_json::json!({
                    "type": case.suite(),
                    "timeout": case.timeout(),
                    "status": "skipped",
                    "skip_reason": "subject_os excludes macos",
                    "layout": null,
                    "roles": null,
                })
            );
        }
    }

    #[test]
    fn listing_json_images_ignore_artifact_bindings_while_roles_follow_them() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            "name: integration\ntimeout: 1s\nsubject_os: [linux, windows, macos]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let bindings = BTreeMap::from([
            ("agent-adp-linux-image".into(), "changed:linux".into()),
            ("agent-adp-windows-image".into(), "changed:windows".into()),
            ("tools-linux-image".into(), "changed:tools".into()),
            ("adp-host-binary".into(), "changed:adp".into()),
            ("agent-host-install".into(), "changed:agent".into()),
        ]);
        let manifest = dir.path().join("artifacts.toml");
        std::fs::write(
            &manifest,
            ArtifactManifest::new("test", bindings.clone()).unwrap().render(),
        )
        .unwrap();
        for os in [Os::Linux, Os::Windows, Os::MacOs] {
            let listings: Vec<_> = [None, Some(manifest.as_path())]
                .into_iter()
                .map(|manifest| {
                    let Discovered { plans, excluded } =
                        discover_in_scope(&mut load_planner(manifest).unwrap(), &[dir.path().to_path_buf()], os)
                            .unwrap();
                    let built = build_runners(&plans, &RunnerArtifacts::Overrides(&[])).unwrap();
                    listing(&built, &excluded, os)
                })
                .collect();
            for (name, before) in &listings[0] {
                let after = &listings[1][name];
                for field in ["runtime", "images"] {
                    assert_eq!(before.get(field), after.get(field));
                    assert!(!before[field].to_string().contains("changed:"));
                }
                if before["status"] == "skipped" {
                    assert_eq!(before, after);
                    continue;
                }
                let before_roles = before["roles"].as_array().unwrap();
                let after_roles = after["roles"].as_array().unwrap();
                assert_eq!(before_roles.len(), after_roles.len());
                let mut manifest_roles = 0;
                for (before, after) in before_roles.iter().zip(after_roles) {
                    assert_eq!(before["source"], "local_default");
                    assert_eq!(before["role"], after["role"]);
                    assert_eq!(before["kind"], after["kind"]);
                    if let Some(reference) = bindings.get(after["role"].as_str().unwrap()) {
                        assert_eq!(after["reference"], reference.as_str());
                        assert_eq!(after["source"], "manifest");
                        manifest_roles += 1;
                    } else {
                        assert_eq!(before, after);
                    }
                }
                assert!(manifest_roles > 0);
            }
            assert_eq!(
                listings[0]["correctness"]["images"]["baseline"] == "base:file",
                os == Os::Linux
            );
        }
    }

    #[tokio::test]
    async fn a_malformed_case_fails_list_and_list_json() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "bad", "type: bogus\n");
        for json in [false, true] {
            let mut args = vec!["panoramic", "list", "-d", dir.path().to_str().unwrap()];
            if json {
                args.push("--json");
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::List(cmd) = cli.command else {
                panic!("expected list");
            };
            assert_eq!(
                list_tests(cmd).await,
                ExitCode::from(EXIT_HARNESS_ERROR),
                "json: {json}"
            );
        }
    }

    #[tokio::test]
    async fn list_and_list_json_validate_directories_then_subject_os_then_manifest() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("missing.toml");
        for json in [false, true] {
            for (has_dirs, conflicts, expected) in [
                (
                    false,
                    true,
                    "No test directories specified. Use -d <path> to specify one or more directories.".to_string(),
                ),
                (
                    true,
                    true,
                    "--runtime linux selects subject OS 'linux', which conflicts with --subject-os windows".to_string(),
                ),
                (
                    true,
                    false,
                    format!("failed to read artifact manifest {}:", manifest.display()),
                ),
            ] {
                let mut args = vec!["panoramic", "list", "--artifacts", manifest.to_str().unwrap()];
                if has_dirs {
                    args.extend(["-d", dir.path().to_str().unwrap()]);
                }
                if conflicts {
                    args.extend(["--runtime", "linux", "--subject-os", "windows"]);
                }
                if json {
                    args.push("--json");
                }
                let Command::List(cmd) = Cli::try_parse_from(args).unwrap().command else {
                    panic!("expected list")
                };
                let output = tempfile::NamedTempFile::new().unwrap();
                let subscriber = tracing_subscriber::fmt()
                    .with_ansi(false)
                    .without_time()
                    .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                    .finish();
                assert_eq!(list_tests(cmd).with_subscriber(subscriber).await, ExitCode::from(2));
                let diagnostic = std::fs::read_to_string(output.path()).unwrap();
                assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
                assert!(diagnostic.contains(&expected), "{diagnostic}");
            }
        }
    }

    #[tokio::test]
    async fn list_and_list_json_validate_manifests_before_discovery_even_without_cases() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        std::fs::create_dir(&cases).unwrap();
        for populated in [false, true] {
            if populated {
                write_case(&cases, "participant", PARTICIPANT_FIELDS);
            }
            for json in [false, true] {
                for (file, content, expected, problem) in [
                    (None, None, 0, ""),
                    (Some("missing.toml"), None, 2, "failed to read artifact manifest"),
                    (Some("malformed.toml"), Some("not toml"), 2, "TOML parse error"),
                    (
                        Some("unknown.toml"),
                        Some("schema = 1\n[roles]\nbogus = 'x'"),
                        2,
                        "no participant type or backend declares role 'bogus'",
                    ),
                    (
                        Some("valid.toml"),
                        Some("schema = 1\n[roles]\nfixup-image = 'registry.example/alpine:3'"),
                        0,
                        "",
                    ),
                ] {
                    let manifest_path = file.map(|file| dir.path().join(file));
                    if let Some(content) = content {
                        std::fs::write(manifest_path.as_ref().unwrap(), content).unwrap();
                    }
                    let mut args = vec![
                        "panoramic",
                        "list",
                        "-d",
                        cases.to_str().unwrap(),
                        "--subject-os",
                        "linux",
                    ];
                    if json {
                        args.push("--json");
                    }
                    if let Some(path) = &manifest_path {
                        args.extend(["--artifacts", path.to_str().unwrap()]);
                    }
                    let Command::List(cmd) = Cli::try_parse_from(args).unwrap().command else {
                        panic!("expected list");
                    };
                    let output = tempfile::NamedTempFile::new().unwrap();
                    let subscriber = tracing_subscriber::fmt()
                        .with_ansi(false)
                        .without_time()
                        .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                        .finish();
                    assert_eq!(
                        list_tests(cmd).with_subscriber(subscriber).await,
                        ExitCode::from(expected),
                        "manifest {file:?}, json {json}, populated {populated}"
                    );
                    if expected == 2 {
                        let diagnostic = std::fs::read_to_string(output.path()).unwrap();
                        assert!(diagnostic.contains(problem), "{diagnostic}");
                        assert!(
                            diagnostic.contains(manifest_path.as_ref().unwrap().to_str().unwrap()),
                            "{diagnostic}"
                        );
                        assert!(!diagnostic.contains("Discovering test cases"), "{diagnostic}");
                    }
                }
            }
        }
    }

    #[test]
    fn listing_text_prints_participant_layouts_and_artifact_bindings_for_every_case() {
        let courier = match std::env::consts::ARCH {
            "aarch64" => "target/aarch64-unknown-linux-musl/release/panoramic-courier",
            "x86_64" => "target/x86_64-unknown-linux-musl/release/panoramic-courier",
            _ => "",
        };
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "  adp:",
            "  zebra: {type: agent-adp}\n  alpha: {type: agent-adp, os: linux}\n  adp:",
        );
        let case = crate::CASE_LOADER
            .load_content(&yaml, "participant", std::path::Path::new("config.yaml"))
            .unwrap()
            .remove(0);
        let manifest = ArtifactManifest::parse(
            "schema = 1\n[roles]\nfixup-image = 'registry.example/alpine:3'",
            std::path::Path::new("artifacts.toml"),
        )
        .unwrap();
        let plan = Planner::new(REGISTRIES, manifest)
            .unwrap()
            .compile(case, Os::MacOs)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(
            listing_text(&plan),
            format!(
                "  participant\n    Timeout: 1s\n    Status: runnable\n    \
             Layout: adp on macos as host-process-group; alpha on linux as linux-container; \
             zebra on macos as host-process-group\n    \
             Artifact roles:\n      \
             adp-host-binary = target/release/agent-data-plane (local default)\n      \
             agent-adp-linux-image = saluki-images/datadog-agent:testing-devel (local default)\n      \
             agent-host-install = /tmp/saluki-dda/datadog-agent (local default)\n      \
             courier-linux-binary = {courier} (local default)\n      \
             fixup-image = registry.example/alpine:3 (manifest)\n    \
             Create order: adp, alpha, zebra\n    \
             Start order: adp, alpha, zebra\n\n"
            )
        );
        let case = crate::CASE_LOADER
            .load_content(
                &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
                "typed",
                std::path::Path::new("config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
        assert_eq!(
            listing_text(&plan),
            format!(
                "  typed\n    Timeout: 1200s\n    Status: runnable\n    Artifact roles:\n      \
             agent-adp-linux-image = saluki-images/datadog-agent:testing-devel (local default)\n      \
             courier-linux-binary = {courier} (local default)\n      \
             fixup-image = alpine:latest (local default)\n      \
             tools-linux-image = saluki-images/correctness-tools:latest (local default)\n\n"
            )
        );
    }

    #[tokio::test]
    async fn listing_text_reports_runnable_and_skipped_cases_with_their_status() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "typed", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        write_case(
            dir.path(),
            "participant",
            &PARTICIPANT_FIELDS.replace("[linux]", "[macos, windows]"),
        );
        write_case(
            dir.path(),
            "integration",
            &format!(
                "description: Windows only\n{}",
                PARTICIPANT_FIELDS.replace("1s", "7s").replace("[linux]", "[windows]")
            ),
        );
        for (os, runnable, skipped) in [
            (Os::MacOs, vec!["participant"], vec!["integration", "typed"]),
            (Os::Windows, vec!["integration", "participant"], vec!["typed"]),
        ] {
            let Discovered { plans, excluded } =
                discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os).unwrap();
            assert_eq!(
                plans.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
                runnable
            );
            assert_eq!(excluded.iter().map(LoadedCase::name).collect::<Vec<_>>(), skipped);
            for plan in &plans {
                assert!(listing_text(plan).contains("    Status: runnable\n"));
            }
            for case in &excluded {
                let expected = match case.name() {
                    "integration" => {
                        "  integration\n    Windows only\n    Timeout: 7s\n    Status: skipped (subject_os)\n\n"
                    }
                    "typed" => "  typed\n    Timeout: 1200s\n    Status: skipped (subject_os)\n\n",
                    name => panic!("unexpected skipped case: {name}"),
                };
                assert_eq!(excluded_listing_text(case), expected);
            }
            let report = listing_report(&plans, &excluded);
            assert!(report.starts_with("\nAvailable tests (3):\n\n"), "{report}");
            let names = report
                .lines()
                .filter_map(|line| line.strip_prefix("  ").filter(|line| !line.starts_with(' ')))
                .collect::<Vec<_>>();
            assert_eq!(names, ["integration", "participant", "typed"]);
            if os == Os::MacOs {
                assert_eq!(
                    report,
                    concat!(
                        "\nAvailable tests (3):\n\n",
                        "  integration\n    Windows only\n    Timeout: 7s\n    Status: skipped (subject_os)\n\n",
                        "  participant\n    Timeout: 1s\n    Status: runnable\n",
                        "    Layout: adp on macos as host-process-group\n",
                        "    Artifact roles:\n",
                        "      adp-host-binary = target/release/agent-data-plane (local default)\n",
                        "      agent-host-install = /tmp/saluki-dda/datadog-agent (local default)\n",
                        "    Create order: adp\n    Start order: adp\n\n",
                        "  typed\n    Timeout: 1200s\n    Status: skipped (subject_os)\n\n",
                    )
                );
            }
            let Command::List(cmd) = Cli::try_parse_from([
                "panoramic",
                "list",
                "-d",
                dir.path().to_str().unwrap(),
                "--subject-os",
                os.name(),
            ])
            .unwrap()
            .command
            else {
                panic!("expected list")
            };
            let output = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                .finish();
            assert_eq!(list_tests(cmd).with_subscriber(subscriber).await, ExitCode::SUCCESS);
            let diagnostic = std::fs::read_to_string(output.path()).unwrap();
            assert!(
                diagnostic.contains(&format!(
                    "Discovered 3 test case(s) for subject OS '{}' ({} skipped).",
                    os.name(),
                    skipped.len(),
                )),
                "{diagnostic}"
            );
        }
    }

    #[test]
    fn listing_reports_suite_runtime_timeout_and_declared_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let runners = build_case_runners(cases, Os::Linux, &[]).unwrap();
        let entries = listing(&runners, &[], Os::Linux);
        assert_eq!(entries.len(), 1);
        let entry = &entries["correctness"];
        assert_eq!(entry["type"], "correctness");
        assert_eq!(entry["runtime"], "docker");
        assert_eq!(entry["timeout"], serde_json::json!({"secs": 1200, "nanos": 0}));
        assert_eq!(entry["images"]["baseline"], "base:file");

        let path = write_case(
            dir.path(),
            "participant",
            &PARTICIPANT_FIELDS.replace("1s", "9s").replace("[linux]", "[windows]"),
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let runners = build_case_runners(cases, Os::Windows, &[]).unwrap();
        let entries = listing(&runners, &[], Os::Windows);
        assert_eq!(entries.len(), 1);
        let entry = &entries["participant"];
        assert_eq!(entry["type"], "integration");
        assert_eq!(entry["status"], "runnable");
        assert!(entry.get("runtime").is_none());
        assert!(entry.get("images").is_none());
        assert_eq!(entry["timeout"], serde_json::json!({"secs": 9, "nanos": 0}));
        assert_eq!(
            entry["layout"],
            serde_json::json!([{
                "participant": "adp", "type": "agent-adp", "os": "windows", "realization": "windows-container",
            }])
        );
        assert_eq!(
            entry["roles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|role| (role["role"].as_str().unwrap(), role["reference"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            [
                (
                    "agent-adp-windows-image",
                    "saluki-images/agent-data-plane:testing-windows"
                ),
                ("courier-windows-binary", "target/release/panoramic-courier.exe"),
            ]
        );
    }
}
