use std::{collections::BTreeMap, fmt::Write as _, process::ExitCode};

use panoramic_core::{case::CaseConfig, layout::Os, machine_output::skip_reason, plan::CasePlan};
use tracing::info;

use crate::{
    cli,
    config::Discovered,
    console::Console,
    discovery::{prepare_discovery, Selection},
};

pub(crate) async fn list_tests(cmd: cli::ListCommand) -> ExitCode {
    let console = Console { use_tui: false };
    let Selection {
        subject_os,
        discovered: Discovered { plans, excluded },
    } = match prepare_discovery(&cmd.test_dirs, cmd.subject_os, cmd.artifacts.as_deref())
        .and_then(|prepared| prepared.discover_selection(&console, !cmd.json))
    {
        Ok(selection) => selection,
        Err(failure) => return failure.report(&console),
    };

    if cmd.json {
        let entries = listing(&plans, &excluded, subject_os);
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

/// Formats every discovered case in name order under a total count.
fn listing_report(plans: &[CasePlan], excluded: &[CaseConfig]) -> String {
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
fn listing_header(case: &CaseConfig, status: &str) -> String {
    let mut text = format!("  {}\n", case.name());
    if let Some(description) = case.description() {
        writeln!(text, "    {description}").unwrap();
    }
    writeln!(text, "    Timeout: {:?}", case.timeout()).unwrap();
    writeln!(text, "    Status: {status}").unwrap();
    text
}

/// Formats one case's text listing, including its layout and resolved artifact references with their sources.
fn listing_text(plan: &CasePlan) -> String {
    let mut text = listing_header(plan.case(), "runnable");
    let participants: Vec<_> = plan
        .layout()
        .iter()
        .map(|(name, realization)| format!("{name} on {} as {}", realization.os.name(), realization.kind.name()))
        .collect();
    writeln!(text, "    Layout: {}", participants.join("; ")).unwrap();
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
    for (label, order) in [("Create", plan.create_order()), ("Start", plan.start_order())] {
        let levels = order.iter().map(|level| level.join(", ")).collect::<Vec<_>>();
        writeln!(text, "    {label} order: {}", levels.join(" then ")).unwrap();
    }
    text.push('\n');
    text
}

/// Formats an excluded case's metadata and skip status.
fn excluded_listing_text(case: &CaseConfig) -> String {
    let mut text = listing_header(case, "skipped (subject_os)");
    text.push('\n');
    text
}

/// Lists every discovered case's status, layout and artifact roles.
pub(crate) fn listing(
    plans: &[CasePlan], excluded: &[CaseConfig], subject_os: Os,
) -> BTreeMap<String, serde_json::Value> {
    plans
        .iter()
        .map(|plan| {
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
            let row = serde_json::json!({
                "type": case.suite(),
                "timeout": case.timeout(),
                "status": "runnable",
                "layout": plan.layout_entries(),
                "roles": roles,
            });
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
            tests::{planner, write_case, PARTICIPANT_FIELDS},
        },
        REGISTRIES,
    };

    #[test]
    fn listing_json_reports_status_layout_and_roles_for_runnable_cases() {
        let plan = crate::config::tests::plan(PARTICIPANT_FIELDS, Os::Linux);
        let expected_layout = serde_json::json!([{
            "participant": "adp", "type": "agent-adp", "os": "linux", "realization": "linux-container",
        }]);
        let expected_keys = vec!["layout", "roles", "status", "timeout", "type"];
        let entries = listing(&[plan], &[], Os::Linux);
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
        let expected_roles = serde_json::json!([
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
        assert_eq!(entry["roles"], expected_roles);
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
        let entries = listing(&plans, &[], Os::Linux);
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
        write_case(dir.path(), "correctness", PARTICIPANT_FIELDS);
        let Discovered { plans, excluded } =
            discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::MacOs).unwrap();
        assert!(plans.is_empty());
        let entries = listing(&plans, &excluded, Os::MacOs);
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
    fn listing_json_roles_follow_artifact_bindings() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            "name: integration\ntimeout: 1s\nsubject_os: [linux, windows, macos]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        write_case(dir.path(), "correctness", PARTICIPANT_FIELDS);
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
                    listing(&plans, &excluded, os)
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
    async fn list_and_list_json_validate_directories_then_manifest() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("missing.toml");
        for json in [false, true] {
            for (has_dirs, expected) in [
                (
                    false,
                    "No test directories specified. Use -d <path> to specify one or more directories.".to_string(),
                ),
                (
                    true,
                    format!("failed to read artifact manifest {}:", manifest.display()),
                ),
            ] {
                let mut args = vec!["panoramic", "list", "--artifacts", manifest.to_str().unwrap()];
                if has_dirs {
                    args.extend(["-d", dir.path().to_str().unwrap()]);
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
            .load_content("cases", &yaml, "participant", std::path::Path::new("config.yaml"))
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
    }

    #[tokio::test]
    async fn listing_text_reports_runnable_and_skipped_cases_with_their_status() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "linux-only", PARTICIPANT_FIELDS);
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
            (Os::MacOs, vec!["participant"], vec!["integration", "linux-only"]),
            (Os::Windows, vec!["integration", "participant"], vec!["linux-only"]),
        ] {
            let Discovered { plans, excluded } =
                discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os).unwrap();
            assert_eq!(
                plans.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
                runnable
            );
            assert_eq!(excluded.iter().map(CaseConfig::name).collect::<Vec<_>>(), skipped);
            for plan in &plans {
                assert!(listing_text(plan).contains("    Status: runnable\n"));
            }
            for case in &excluded {
                let expected = match case.name() {
                    "integration" => {
                        "  integration\n    Windows only\n    Timeout: 7s\n    Status: skipped (subject_os)\n\n"
                    }
                    "linux-only" => "  linux-only\n    Timeout: 1s\n    Status: skipped (subject_os)\n\n",
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
            assert_eq!(names, ["integration", "linux-only", "participant"]);
            if os == Os::MacOs {
                assert_eq!(
                    report,
                    concat!(
                        "\nAvailable tests (3):\n\n",
                        "  integration\n    Windows only\n    Timeout: 7s\n    Status: skipped (subject_os)\n\n",
                        "  linux-only\n    Timeout: 1s\n    Status: skipped (subject_os)\n\n",
                        "  participant\n    Timeout: 1s\n    Status: runnable\n",
                        "    Layout: adp on macos as host-process-group\n",
                        "    Artifact roles:\n",
                        "      adp-host-binary = target/release/agent-data-plane (local default)\n",
                        "      agent-host-install = /tmp/saluki-dda/datadog-agent (local default)\n",
                        "    Create order: adp\n    Start order: adp\n\n",
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
    fn listing_reports_suite_timeout_and_windows_artifact_roles() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            &dir.path().join("integration/cases"),
            "participant",
            &PARTICIPANT_FIELDS.replace("1s", "9s").replace("[linux]", "[windows]"),
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let plans = cases
            .into_iter()
            .map(|case| planner().compile(case, Os::Windows).unwrap().into_plan().unwrap())
            .collect::<Vec<_>>();
        let entries = listing(&plans, &[], Os::Windows);
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
