use std::{collections::BTreeMap, path::Path, process::ExitCode};

use panoramic_core::artifact::{verified_artifacts, ArtifactBinding, VerifiedArtifact};
use panoramic_core::backend::RunBackends;
use panoramic_core::events::create_event_channel;
use panoramic_core::interrupts::Interrupts;
use panoramic_core::layout::{Os, Realization};
use panoramic_core::machine_output::{revision, RunReport, TestReport, RUN_REPORT_FILE_NAME};
use panoramic_core::plan::needed_artifacts;
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::{
    artifact_commands, cli,
    collector::{self, consume_run, run_teardown, RunCollector, RunDisplay, RunEnd, EXIT_HARNESS_ERROR},
    config::{
        build_runners,
        runner_artifacts::{BoundArtifacts, RunnerArtifacts},
        runtime_for_os,
        typed_case::{case_dir, needs_pod, realizations, LoadedCase, LoadedPlan},
        BuiltCase, Discovered, Execution,
    },
    console::{Console, SetupFailure},
    discovery::{prepare_discovery, Selection},
    kind::KindLifecycle,
    providability,
    runner::{self, Runner},
    test::RunnerSettings,
    text_output::{OutputFormat, Reporter},
    tui,
};

/// Splits the `-t` selection into the tests that matched and the names that matched nothing.
fn resolve_selection(requested: &str, discovered: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut selected = Vec::new();
    let mut unmatched = Vec::new();

    for name in requested.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if discovered.contains(&name) {
            selected.push(name.to_string());
        } else {
            unmatched.push(name.to_string());
        }
    }

    (selected, unmatched)
}

/// Keeps the named cases, or every case when no selection was given.
fn retain_selected<T>(cases: &mut Vec<T>, selected: Option<&[String]>, name: impl Fn(&T) -> &str) {
    cases.retain(|case| selected.is_none_or(|names| names.iter().any(|selected| selected == name(case))));
}

/// Cases and settings ready for reporting and execution.
struct PreparedRun {
    subject_os: Os,
    built: Vec<BuiltCase>,
    excluded: Vec<LoadedCase>,
    settings: RunnerSettings,
    verified: Option<Vec<VerifiedArtifact>>,
}

pub(crate) async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    let console = Console { use_tui };
    match prepare_run(&cmd, &console).await {
        Ok(prepared) => report_run(prepared, &cmd, &console).await,
        Err(failure) => failure.report(&console),
    }
}

/// Discovers, builds, selects and verifies the cases before starting a run.
async fn prepare_run(cmd: &cli::RunCommand, console: &Console) -> Result<PreparedRun, SetupFailure> {
    let Selection {
        subject_os,
        discovered: Discovered { plans, mut excluded },
    } = prepare_discovery(&cmd.test_dirs, cmd.subject_os.resolve(), cmd.artifacts.as_deref())?
        .discover_selection(console, false)?;
    let (mut built, settings, artifacts) = build_cases(cmd, subject_os, plans)?;
    check_legacy_inputs(cmd, subject_os, &built, &settings, &artifacts)?;
    select_cases(cmd, console, subject_os, &mut built, &mut excluded)?;
    let backends = RunBackends::default();
    let unprovidable = check_providability(&backends, console, &mut built).await?;
    let verified = verify_artifacts(&backends, cmd, console, subject_os, &built, &unprovidable).await?;
    Ok(PreparedRun {
        subject_os,
        built,
        excluded,
        settings,
        verified,
    })
}

/// Builds runners with settings and artifacts resolved from the command.
fn build_cases(
    cmd: &cli::RunCommand, subject_os: Os, plans: Vec<LoadedPlan>,
) -> Result<(Vec<BuiltCase>, RunnerSettings, RunnerArtifacts<'_>), SetupFailure> {
    let mut settings = cmd.runner_settings();
    let artifacts = if cmd.artifacts.is_some() {
        let bound = BoundArtifacts::new(needed_artifacts(&plans));
        bound.apply_runner_settings(&mut settings);
        RunnerArtifacts::Bound(bound)
    } else {
        RunnerArtifacts::Overrides(&cmd.image_overrides)
    };
    let built = build_runners(plans, subject_os, &artifacts).map_err(SetupFailure::discovery)?;
    Ok((built, settings, artifacts))
}

/// Checks explicit image and binary inputs against artifact bindings and logs resolved settings.
fn check_legacy_inputs(
    cmd: &cli::RunCommand, subject_os: Os, built: &[BuiltCase], settings: &RunnerSettings,
    artifacts: &RunnerArtifacts<'_>,
) -> Result<(), SetupFailure> {
    if let RunnerArtifacts::Bound(bound) = artifacts {
        let runners: Vec<_> = built.iter().filter_map(BuiltCase::runner).collect();
        bound
            .check_legacy_inputs(&cmd.legacy_artifact_inputs(), subject_os, &runners)
            .map_err(SetupFailure::discovery)?;
    }
    debug!(
        adp_binary_path = %settings.adp_binary_path.display(),
        core_agent_binary_path = %settings.core_agent_binary_path.display(),
        mounts_dir = %settings.mounts_dir.display(),
        alpine_image = %settings.alpine_image,
        "Resolved runner settings."
    );
    Ok(())
}

/// Resolves the requested test names and announces cases excluded by the subject OS.
fn select_cases(
    cmd: &cli::RunCommand, console: &Console, subject_os: Os, built: &mut Vec<BuiltCase>,
    excluded: &mut Vec<LoadedCase>,
) -> Result<(), SetupFailure> {
    if built.is_empty() && excluded.is_empty() {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        let msg = format!("No test cases found in: {}", dirs_str.join(", "));
        return Err(SetupFailure::harness(msg));
    }

    // Resolve the `-t` selection before doing any work: a name that matches nothing, or a
    // selection that leaves nothing to run, is an error rather than a green run of zero tests.
    let selected_tests = match cmd.tests.as_deref() {
        Some(requested) => {
            let names: Vec<_> = built
                .iter()
                .map(|built| built.plan.case().name())
                .chain(excluded.iter().map(LoadedCase::name))
                .collect();
            let (selected, unmatched) = resolve_selection(requested, &names);
            if !unmatched.is_empty() {
                let msg = format!(
                    "No test matches: {}. Run 'panoramic list --subject-os {}' to see the tests discovered for subject OS '{}'.",
                    unmatched.join(", "),
                    subject_os.name(),
                    subject_os.name()
                );
                return Err(SetupFailure::no_tests_selected(msg));
            }
            if selected.is_empty() {
                let msg = "No tests selected. Drop -t, or name at least one test.";
                return Err(SetupFailure::no_tests_selected(msg));
            }
            Some(selected)
        }
        None => None,
    };
    // Register only the selected tests so the runner and `run.json` share one selection.
    retain_selected(built, selected_tests.as_deref(), |built| built.plan.case().name());
    retain_selected(excluded, selected_tests.as_deref(), LoadedCase::name);

    if !excluded.is_empty() {
        let names = excluded.iter().map(LoadedCase::name).collect::<Vec<_>>().join(", ");
        let message = format!(
            "Skipping {} test(s) whose subject_os excludes {}: {names}",
            excluded.len(),
            subject_os.name()
        );
        console.info(message);
    }

    Ok(())
}

/// Checks whether the host can provide each selected case's realizations.
async fn check_providability(
    backends: &RunBackends, console: &Console, built: &mut [BuiltCase],
) -> Result<BTreeMap<Realization, String>, SetupFailure> {
    let unprovidable = if built.is_empty() {
        BTreeMap::new()
    } else {
        console.info("Checking that this host can provide the selected cases...");
        providability::check(backends, built).await
    };
    Ok(unprovidable)
}

/// Verifies artifacts for selected cases the host can run when a manifest was supplied.
async fn verify_artifacts(
    backends: &RunBackends, cmd: &cli::RunCommand, console: &Console, subject_os: Os, built: &[BuiltCase],
    unprovidable: &BTreeMap<Realization, String>,
) -> Result<Option<Vec<VerifiedArtifact>>, SetupFailure> {
    if cmd.artifacts.is_some() {
        let bindings = run_artifacts(built, unprovidable);
        let base = std::env::current_dir()
            .map_err(|error| SetupFailure::harness(format!("Cannot read the current directory: {error}")))?;
        Ok(Some(
            verify_run_artifacts(backends, &bindings, subject_os, &base, console).await?,
        ))
    } else {
        Ok(None)
    }
}

/// Creates run reports, executes the cases and returns the run's exit code.
async fn report_run(prepared: PreparedRun, cmd: &cli::RunCommand, console: &Console) -> ExitCode {
    let PreparedRun {
        subject_os,
        built,
        excluded,
        settings,
        verified,
    } = prepared;
    // Create log directory.
    let log_dir = match std::path::absolute(cmd.log_dir()) {
        Ok(path) => path,
        Err(e) => {
            error!("Failed to resolve log directory: {}", e);
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        console.error(format!("Failed to create log directory: {}", e));
        return ExitCode::from(2);
    }

    // Install interrupt handlers before starting background work.
    let mut interrupts = Interrupts::install();
    let report = RunReport::new(
        build_revision(),
        cmd.test_dirs.clone(),
        runtime_for_os(subject_os).to_string(),
        subject_os,
        cmd.parallelism.get(),
        log_dir.clone(),
        built.iter().map(|built| built.plan.case().name().to_string()).collect(),
        excluded
            .into_iter()
            .map(|case| TestReport::skipped(case.name().to_string(), case_dir(&case), subject_os))
            .collect(),
    );
    let report = match verified {
        Some(verified) => report.with_artifacts(verified),
        None => report,
    };
    let mut collector = RunCollector::start(report, cmd.fail_fast);

    let display_error = if !built.is_empty() {
        execute(built, settings, cmd, console, &log_dir, &mut collector, &mut interrupts).await
    } else {
        collector.finish(Ok(()));
        let message = format!(
            "Every selected test is skipped for subject OS '{}'; wrote {}.",
            subject_os.name(),
            log_dir.join(RUN_REPORT_FILE_NAME).display(),
        );
        console.warn(message);
        None
    };

    // Print after teardown so the report includes an interrupt during teardown.
    let mut json_output_failed = false;
    if matches!(cmd.output, OutputFormat::Json) {
        match serde_json::to_string_pretty(collector.report()) {
            Ok(json) => println!("{}", json),
            Err(e) => {
                error!("Failed to serialize the machine-readable run report: {}", e);
                json_output_failed = true;
            }
        }
    }

    // Report output failures as harness errors, even when the tests passed.
    if json_output_failed || display_error.is_some() {
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }

    collector::exit_code(collector.report())
}

/// Runs the selected cases, collects their results and handles kind cluster cleanup.
async fn execute(
    built: Vec<BuiltCase>, settings: RunnerSettings, cmd: &cli::RunCommand, console: &Console, log_dir: &Path,
    collector: &mut RunCollector, interrupts: &mut Interrupts,
) -> Option<std::io::Error> {
    // Create the event channel early so the kind setup task can emit status messages.
    let (tx, rx) = create_event_channel();

    // Spawn kind cluster setup in the background so non-kind tests start immediately.
    // Kind tests wait on `kind_rx` before doing any work.
    let kind_images = collect_kind_images(&built);
    let kind_lifecycle_slot = std::sync::Arc::new(Mutex::new(None::<KindLifecycle>));
    let kind_rx = if kind_images.is_empty() {
        None
    } else {
        let (kind_tx, kind_rx) = watch::channel::<Option<Result<(), String>>>(None);
        let slot = kind_lifecycle_slot.clone();
        let cluster_name = cmd.kind_cluster_name.clone();
        let event_tx = tx.clone();
        tokio::spawn(async move {
            match KindLifecycle::ensure(cluster_name, kind_images, event_tx).await {
                Ok(lc) => {
                    *slot.lock().await = Some(lc);
                    let _ = kind_tx.send(Some(Ok(())));
                }
                Err(e) => {
                    let _ = kind_tx.send(Some(Err(format!("{:?}", e))));
                }
            }
        });
        Some(kind_rx)
    };

    let mut registry = Runner::new(log_dir, settings);
    if let Some(ref rx) = kind_rx {
        registry = registry.with_kind_ready(rx.clone());
    }
    for built in built {
        registry.register(built).expect("failure to register test");
    }

    // The consume loop cancels this on the first interrupt.
    let cancel_all = CancellationToken::new();

    let args = runner::RunArgs::new(cancel_all.clone(), tx)
        .with_parallelism(cmd.parallelism)
        .with_fail_fast(cmd.fail_fast);

    let runner_handle = tokio::spawn(async move { registry.run_tests(args).await });

    let mut display_error = None;
    let display = if console.use_tui {
        match tui::Tui::new(log_dir) {
            Ok(tui) => Some(RunDisplay::Tui(tui)),
            Err(e) => {
                display_error = Some(e);
                None
            }
        }
    } else {
        info!("Starting test run with parallelism of {}...", cmd.parallelism);
        info!("Container logs will be written to '{}'.", log_dir.display());
        if cmd.fail_fast {
            info!("Fail-fast mode enabled; will stop on first failure.");
        }
        Some(RunDisplay::Plain(Reporter::new(
            cmd.output,
            cmd.verbose,
            Some(log_dir.to_path_buf()),
        )))
    };

    let consumed = consume_run(rx, runner_handle, collector, display, interrupts, &cancel_all).await;
    display_error = display_error.or(consumed.display_error);
    if let Some(e) = &display_error {
        eprintln!("The TUI failed: {}", e);
    }

    // Tear down the kind cluster unless the caller asked to keep it.
    if kind_rx.is_some() {
        let cluster_name = &cmd.kind_cluster_name;

        if consumed.end == RunEnd::Abandoned {
            console.warn(format!(
                "Skipping kind cluster teardown after a second interrupt. Cluster '{}' may still be running; \
                 delete it with 'kind delete cluster --name {}'.",
                cluster_name, cluster_name
            ));
        } else {
            let lifecycle: Option<KindLifecycle> = kind_lifecycle_slot.lock().await.take();
            if let Some(lifecycle) = lifecycle {
                if cmd.no_delete_kind_cluster {
                    info!(
                        "Skipping kind cluster teardown (--no-delete-kind-cluster). \
                         Cluster '{}' is still running.",
                        cluster_name
                    );
                } else if let Some(source) = run_teardown(lifecycle.teardown(), collector, interrupts).await {
                    console.warn(format!(
                        "Received {}; abandoned kind cluster teardown. A 'kind delete' already started may still \
                         finish. If cluster '{}' is still running, delete it with 'kind delete cluster --name {}'.",
                        source, cluster_name, cluster_name
                    ));
                }
            } else {
                // Failed or unfinished setup can leave a cluster behind.
                console.warn(format!(
                    "Kind cluster setup did not complete successfully. \
                     A kind cluster named '{}' may still be running — run 'kind delete cluster --name {}' to clean it up.",
                    cluster_name, cluster_name
                ));
            }
        }
    }
    display_error
}

/// Selects artifacts only from cases this host can run, after test-name selection.
fn run_artifacts(built: &[BuiltCase], unprovidable: &BTreeMap<Realization, String>) -> Vec<ArtifactBinding> {
    needed_artifacts(built.iter().filter_map(|built| {
        realizations(&built.plan)
            .iter()
            .all(|(_, realization)| !unprovidable.contains_key(realization))
            .then_some(&built.plan)
    }))
}

/// Reports artifact verification results and returns their verified identities.
async fn verify_run_artifacts(
    backends: &RunBackends, bindings: &[ArtifactBinding], subject_os: Os, base: &Path, console: &Console,
) -> Result<Vec<VerifiedArtifact>, SetupFailure> {
    console.info("Verifying the artifacts the selected cases need…");
    let results = backends.verify(bindings, base).await;
    let verified = verified_artifacts(&results);
    let report = artifact_commands::verification_text(subject_os, &results, ("Artifacts", "artifacts"));
    let report = report.trim_end();
    if verified.is_ok() {
        console.info(report);
    } else {
        console.error(report);
    }
    verified.map_err(|failed| {
        let n = bindings.len();
        SetupFailure::harness(format!("{failed} of {n} artifact(s) failed verification."))
    })
}

/// Collects the unique images of cases that need Kubernetes pods.
fn collect_kind_images(built: &[BuiltCase]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut images = BTreeSet::new();
    for BuiltCase { plan, execution } in built {
        if let Execution::Runner(runner) = execution {
            if needs_pod(plan) {
                for (_, image) in runner.images() {
                    images.insert(image);
                }
            }
        }
    }
    images.into_iter().collect()
}

/// Revision of the repository this binary was built from, as recorded by the build script.
fn build_revision() -> String {
    revision(env!("VERGEN_GIT_SHA"), env!("VERGEN_GIT_DIRTY"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser as _;
    use panoramic_core::{artifact::ArtifactManifest, layout::Os, plan::Planner};

    use super::*;
    use crate::{
        cli::{Cli, Command},
        collector::EXIT_NO_TESTS_SELECTED,
        config::{
            discover_in_scope,
            tests::{planner, write_case, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
        },
        discovery::NO_TEST_DIRECTORIES,
        list_command::listing,
        test, REGISTRIES,
    };

    async fn run_skipped_selection(requested: Option<&str>, manifest: Option<ArtifactManifest>) {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "windows",
            "type: integration\nname: windows\ntimeout: 1s\nruntimes: [windows]\nprocedure: []\n",
        );
        write_case(
            &cases,
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let manifest = manifest.map(|manifest| {
            let path = dir.path().join("artifacts.toml");
            std::fs::write(&path, manifest.render()).unwrap();
            path
        });
        let logs = dir.path().join("logs");
        let mut args = vec![
            "panoramic",
            "run",
            "-d",
            cases.to_str().unwrap(),
            "--subject-os",
            "macos",
            "--no-tui",
            "-l",
            logs.to_str().unwrap(),
        ];
        if let Some(name) = requested {
            args.extend(["-t", name]);
        }
        if let Some(path) = &manifest {
            args.extend(["--artifacts", path.to_str().unwrap()]);
        }
        let Command::Run(cmd) = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected run")
        };
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        assert_eq!(
            run_tests(cmd, false).with_subscriber(subscriber).await,
            ExitCode::from(3)
        );
        let diagnostic = std::fs::read_to_string(output.path()).unwrap();
        assert!(!diagnostic.contains("Starting test run"), "{diagnostic}");
        assert!(
            !diagnostic.contains("Checking that this host can provide"),
            "{diagnostic}"
        );
        let run_dirs = std::fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(run_dirs.len(), 1);
        let run_json = run_dirs[0].join(RUN_REPORT_FILE_NAME);
        let report: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&run_json).unwrap()).unwrap();
        assert_eq!(report["schema_version"], 3);
        assert_eq!(report["state"], "complete");
        assert_eq!(report["subject_os"], "macos");
        assert_eq!(report["runtime"], "mac");
        let expected_artifacts = if manifest.is_some() {
            serde_json::json!([])
        } else {
            serde_json::Value::Null
        };
        assert_eq!(report.get("artifacts"), Some(&expected_artifacts));
        let expected = requested.map_or_else(|| vec!["correctness", "windows"], |name| vec![name]);
        assert_eq!(report["totals"]["skipped"], expected.len());
        assert_eq!(report["running"], serde_json::json!([]));
        assert_eq!(report["not_run"], serde_json::json!([]));
        assert_eq!(report["tests"].as_array().unwrap().len(), expected.len());
        for (test, name) in report["tests"].as_array().unwrap().iter().zip(&expected) {
            assert_eq!(test["name"], *name);
            assert_eq!(test["outcome"], "skipped");
            assert_eq!(test["skip_reason"], "subject_os excludes macos");
            assert_eq!(test.get("layout"), Some(&serde_json::Value::Null));
            assert_eq!(
                test["case_path"],
                serde_json::to_value(cases.join(name).canonicalize().unwrap()).unwrap()
            );
            assert!(test["log_dir"].is_null());
        }
        assert_eq!(
            std::fs::read_dir(&run_dirs[0])
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&run_json)
        );
        let notice = format!(
            "Skipping {} test(s) whose subject_os excludes macos: {}",
            expected.len(),
            expected.join(", ")
        );
        assert_eq!(diagnostic.matches(&notice).count(), 1, "{diagnostic}");
        assert!(
            diagnostic.contains(&format!(
                "Every selected test is skipped for subject OS 'macos'; wrote {}.",
                run_json.display()
            )),
            "{diagnostic}"
        );
    }

    #[tokio::test]
    async fn a_run_whose_every_selected_case_is_skipped_exits_3_and_writes_run_json_saying_why() {
        run_skipped_selection(None, None).await;
    }

    #[tokio::test]
    async fn naming_a_skipped_case_selects_it_and_the_run_exits_3() {
        run_skipped_selection(Some("windows"), None).await;
    }

    #[tokio::test]
    async fn a_run_with_artifacts_and_only_skipped_cases_records_an_empty_artifact_list() {
        run_skipped_selection(None, Some(ArtifactManifest::new("test", BTreeMap::new()).unwrap())).await;
    }

    #[tokio::test]
    async fn an_unmatched_test_name_names_the_subject_os() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "available",
            "type: integration\nname: available\ntimeout: 1s\nruntimes: [mac]\nprocedure: []\n",
        );
        let logs = dir.path().join("logs");
        let Command::Run(cmd) = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            dir.path().to_str().unwrap(),
            "--subject-os",
            "macos",
            "-t",
            "missing",
            "--no-tui",
            "-l",
            logs.to_str().unwrap(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected run")
        };
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        assert_eq!(
            run_tests(cmd, false).with_subscriber(subscriber).await,
            ExitCode::from(3)
        );
        assert!(!logs.exists());
        let diagnostic = std::fs::read_to_string(output.path()).unwrap();
        assert!(
            diagnostic.contains(
                "No test matches: missing. Run 'panoramic list --subject-os macos' \
                 to see the tests discovered for subject OS 'macos'."
            ),
            "{diagnostic}"
        );
    }

    #[tokio::test]
    async fn run_validates_directories_then_subject_os_then_manifest() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("missing.toml");
        let logs = dir.path().join("logs");
        for (has_dirs, conflicts, expected) in [
            (false, true, NO_TEST_DIRECTORIES.to_string()),
            (
                true,
                true,
                "--runtime linux selects subject OS 'linux', which conflicts with --subject-os windows".into(),
            ),
            (
                true,
                false,
                format!("failed to read artifact manifest {}:", manifest.display()),
            ),
        ] {
            let mut args = vec![
                "panoramic",
                "run",
                "--artifacts",
                manifest.to_str().unwrap(),
                "--no-tui",
                "-l",
                logs.to_str().unwrap(),
            ];
            if has_dirs {
                args.extend(["-d", dir.path().to_str().unwrap()]);
            }
            if conflicts {
                args.extend(["--runtime", "linux", "--subject-os", "windows"]);
            }
            let Command::Run(cmd) = Cli::try_parse_from(args).unwrap().command else {
                panic!("expected run")
            };
            let output = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                .finish();
            assert_eq!(
                run_tests(cmd, false).with_subscriber(subscriber).await,
                ExitCode::from(2)
            );
            let diagnostic = std::fs::read_to_string(output.path()).unwrap();
            assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
            assert!(diagnostic.contains(&expected), "{diagnostic}");
            assert!(!logs.exists());
        }
    }

    #[test]
    fn run_artifacts_verify_only_the_selected_providable_cases() {
        use panoramic_core::layout::RealizationKind;

        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        write_case(
            dir.path(),
            "host",
            "type: integration\nname: host\ntimeout: 1s\nruntimes: [mac]\nprocedure: []\n",
        );
        let host = Realization {
            os: Os::MacOs,
            kind: RealizationKind::HostProcessGroup,
        };
        for (selected, unavailable, expected) in [
            (None, vec![], vec!["adp-host-binary", "agent-host-install"]),
            (Some("correctness"), vec![], vec![]),
            (Some("correctness,host"), vec![host], vec![]),
            (Some("host"), vec![host], vec![]),
            (
                Some("correctness,host"),
                vec![],
                vec!["adp-host-binary", "agent-host-install"],
            ),
        ] {
            let Discovered { plans, excluded } =
                discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::MacOs).unwrap();
            assert_eq!(
                excluded.iter().map(LoadedCase::name).collect::<Vec<_>>(),
                ["correctness"]
            );
            let mut built = build_runners(plans, Os::MacOs, &RunnerArtifacts::Overrides(&[])).unwrap();
            let names: Vec<_> = built
                .iter()
                .map(|built| built.plan.case().name())
                .chain(excluded.iter().map(LoadedCase::name))
                .collect();
            let selected = selected.map(|requested| {
                let (selected, unmatched) = resolve_selection(requested, &names);
                assert!(unmatched.is_empty());
                selected
            });
            retain_selected(&mut built, selected.as_deref(), |built| built.plan.case().name());
            let unprovidable = unavailable
                .into_iter()
                .map(|realization| (realization, "unavailable".into()))
                .collect();
            let bindings = run_artifacts(&built, &unprovidable);
            assert_eq!(
                bindings.iter().map(|binding| binding.role.name).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn run_artifacts_report_every_failed_binding_and_count_failures() {
        use panoramic_core::artifact::{test_util::write_executable, ArtifactKind, ArtifactRole, BindingSource};

        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("present"));
        write_executable(&dir.path().join("install/bin/main"));
        let bindings: Vec<_> = [
            ("missing-binary", ArtifactKind::Binary, "missing"),
            ("present-binary", ArtifactKind::Binary, "present"),
            (
                "missing-install",
                ArtifactKind::Install {
                    main_binary: "bin/main",
                },
                "missing-install",
            ),
            (
                "present-install",
                ArtifactKind::Install {
                    main_binary: "bin/main",
                },
                "install",
            ),
        ]
        .into_iter()
        .map(|(name, kind, reference)| ArtifactBinding {
            role: ArtifactRole {
                name,
                kind,
                local_default: "unused",
            },
            reference: reference.into(),
            source: BindingSource::Manifest,
        })
        .collect();
        let results = RunBackends::default().verify(&bindings, dir.path()).await;
        let report = artifact_commands::verification_text(Os::MacOs, &results, ("Artifacts", "artifacts"));
        assert_eq!(verified_artifacts(&results), Err(2), "{report}");
        assert_eq!(report.matches("FAILED:").count(), 2, "{report}");
        assert_eq!(
            report
                .matches("sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d")
                .count(),
            2,
            "{report}"
        );
        assert_eq!(report.lines().count(), bindings.len() + 1, "{report}");
        for (line, binding) in report.lines().skip(1).zip(&bindings) {
            assert!(
                line.starts_with(&format!(
                    "  {} = {} (manifest): {} ",
                    binding.role.name,
                    binding.reference,
                    binding.role.kind.name()
                )),
                "{line}"
            );
        }
    }

    #[tokio::test]
    async fn run_artifact_verification_returns_identities_for_verified_bindings_and_nothing_after_any_failure() {
        use panoramic_core::artifact::{test_util::write_executable, ArtifactKind, ArtifactRole, BindingSource};

        let dir = tempfile::tempdir().unwrap();
        let bindings: Vec<_> = [
            ("binary", ArtifactKind::Binary, "./present"),
            (
                "install",
                ArtifactKind::Install {
                    main_binary: "bin/main",
                },
                "install",
            ),
        ]
        .into_iter()
        .map(|(name, kind, reference)| ArtifactBinding {
            role: ArtifactRole {
                name,
                kind,
                local_default: "unused",
            },
            reference: reference.into(),
            source: BindingSource::Manifest,
        })
        .collect();
        write_executable(&dir.path().join("install/bin/main"));
        for use_tui in [false, true] {
            write_executable(&dir.path().join("present"));
            let verified = verify_run_artifacts(
                &RunBackends::default(),
                &bindings,
                Os::MacOs,
                dir.path(),
                &Console { use_tui },
            )
            .await
            .unwrap();
            assert_eq!(
                serde_json::to_value(verified).unwrap(),
                serde_json::json!([
                    {
                        "role": "binary",
                        "reference": "./present",
                        "identity": "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d",
                        "kind": "binary",
                    },
                    {
                        "role": "install",
                        "reference": "install",
                        "identity": "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d",
                        "kind": "install",
                    },
                ])
            );
            std::fs::remove_file(dir.path().join("present")).unwrap();
            let failure = verify_run_artifacts(
                &RunBackends::default(),
                &bindings,
                Os::MacOs,
                dir.path(),
                &Console { use_tui },
            )
            .await
            .unwrap_err();
            assert_eq!(failure.exit, ExitCode::from(2));
            assert_eq!(failure.message, "1 of 2 artifact(s) failed verification.");
        }
    }

    #[tokio::test]
    async fn a_run_whose_artifacts_fail_verification_exits_2_before_creating_any_log_directory() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "host",
            "type: integration\nname: host\ntimeout: 1s\nruntimes: [mac]\nprocedure: []\n",
        );
        let manifest = dir.path().join("artifacts.toml");
        let missing = dir.path().join("missing");
        std::fs::write(
            &manifest,
            format!(
                "schema = 1\n[roles]\nadp-host-binary = '{}'\nagent-host-install = '{}'\n",
                missing.display(),
                missing.display()
            ),
        )
        .unwrap();
        let logs = dir.path().join("logs");
        let Command::Run(cmd) = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            cases.to_str().unwrap(),
            "--subject-os",
            "macos",
            "--artifacts",
            manifest.to_str().unwrap(),
            "--no-tui",
            "-l",
            logs.to_str().unwrap(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected run")
        };
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        assert_eq!(
            run_tests(cmd, false).with_subscriber(subscriber).await,
            ExitCode::from(2)
        );
        let diagnostic = std::fs::read_to_string(output.path()).unwrap();
        assert!(
            diagnostic.contains("Verifying the artifacts the selected cases need…"),
            "{diagnostic}"
        );
        if Os::host() == Os::MacOs {
            assert!(!logs.exists());
            assert!(!diagnostic.contains("\n\n"), "{diagnostic}");
            assert!(
                diagnostic.contains("2 of 2 artifact(s) failed verification."),
                "{diagnostic}"
            );
            assert!(diagnostic.contains("adp-host-binary ="), "{diagnostic}");
            assert!(diagnostic.contains("agent-host-install ="), "{diagnostic}");
            assert!(!diagnostic.contains("Starting test run"), "{diagnostic}");
        } else {
            let run_dir = std::fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
            let report: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
            let tests = report["tests"].as_array().unwrap();
            assert_eq!(tests.len(), 1);
            assert_eq!(tests[0]["name"], "host");
            assert_eq!(tests[0]["outcome"], "errored");
            assert_eq!(tests[0]["error"]["kind"], "setup");
            assert_eq!(report["artifacts"], serde_json::json!([]));
            assert!(diagnostic.contains("No artifacts needed on macos."), "{diagnostic}");
            assert!(!diagnostic.contains("failed verification"), "{diagnostic}");
        }
    }

    async fn run_with_legacy_host_path(dir: &Path, agrees: bool) -> (ExitCode, String, PathBuf) {
        let cases = dir.join("cases");
        write_case(
            &cases,
            "host",
            "type: integration\nname: host\ntimeout: 1s\nruntimes: [mac]\nprocedure: []\n",
        );
        let manifest = dir.join("artifacts.toml");
        let missing = dir.join("missing");
        std::fs::write(
            &manifest,
            format!(
                "schema = 1\n[roles]\nadp-host-binary = '{}'\nagent-host-install = '{}'\n",
                missing.display(),
                missing.display()
            ),
        )
        .unwrap();
        let legacy = if agrees { missing } else { dir.join("different") };
        let logs = dir.join("logs");
        let Command::Run(cmd) = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            cases.to_str().unwrap(),
            "--subject-os",
            "macos",
            "--artifacts",
            manifest.to_str().unwrap(),
            "--adp-binary-path",
            legacy.to_str().unwrap(),
            "--no-tui",
            "-l",
            logs.to_str().unwrap(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected run")
        };
        let run_json = cmd.log_dir().join("run.json");
        let (exit, diagnostic) = run_with_diagnostics(cmd, dir).await;
        (exit, diagnostic, run_json)
    }

    async fn run_with_diagnostics(cmd: cli::RunCommand, dir: &Path) -> (ExitCode, String) {
        use tracing::instrument::WithSubscriber as _;

        let output = std::fs::File::create(dir.join("diagnostic.log")).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output))
            .finish();
        let exit = run_tests(cmd, false).with_subscriber(subscriber).await;
        (exit, std::fs::read_to_string(dir.join("diagnostic.log")).unwrap())
    }

    #[tokio::test]
    async fn a_run_whose_legacy_inputs_disagree_with_its_artifacts_exits_2_before_verifying_or_creating_any_log_directory(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (exit, diagnostic, run_json) = run_with_legacy_host_path(dir.path(), false).await;
        assert_eq!(exit, ExitCode::from(2));
        assert!(!dir.path().join("logs").exists());
        assert!(!run_json.exists());
        assert!(
            diagnostic.contains("Legacy artifact inputs disagree with the --artifacts bindings:"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains(&format!(
            "--adp-binary-path (PANORAMIC_ADP_BINARY_PATH) names '{}' for role 'adp-host-binary', bound to '{}' (manifest)",
            dir.path().join("different").display(), dir.path().join("missing").display()
        )), "{diagnostic}");
        assert!(!diagnostic.contains("Verifying the artifacts"), "{diagnostic}");
        assert!(!diagnostic.contains("Checking that this host"), "{diagnostic}");
    }

    #[tokio::test]
    async fn a_run_whose_legacy_inputs_agree_with_its_artifacts_proceeds_to_verification() {
        let dir = tempfile::tempdir().unwrap();
        let (exit, diagnostic, run_json) = run_with_legacy_host_path(dir.path(), true).await;
        assert_eq!(exit, ExitCode::from(2));
        assert!(diagnostic.contains("Verifying the artifacts"), "{diagnostic}");
        assert!(!diagnostic.contains("Legacy artifact inputs disagree"), "{diagnostic}");
        if Os::host() == Os::MacOs {
            assert!(
                diagnostic.contains("2 of 2 artifact(s) failed verification."),
                "{diagnostic}"
            );
            assert!(!dir.path().join("logs").exists());
            assert!(!run_json.exists());
        } else {
            let report: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(run_json).unwrap()).unwrap();
            assert_eq!(report["artifacts"], serde_json::json!([]));
            assert_eq!(report["tests"][0]["outcome"], "errored");
            assert_eq!(report["tests"][0]["error"]["kind"], "setup");
            assert!(diagnostic.contains("No artifacts needed on macos."), "{diagnostic}");
        }
    }

    #[tokio::test]
    async fn image_override_name_errors_have_the_same_prefix_with_or_without_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "linux",
            "type: integration\nname: linux\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
        );
        let manifest = dir.path().join("artifacts.toml");
        std::fs::write(&manifest, "schema = 1\n[roles]\n").unwrap();
        let logs = dir.path().join("logs");
        for (overrides, expected) in [
            ("container=x,container=y", "Failed to discover tests: Image override 'container' is given more than once. Name each container at most once."),
            ("unknown=x", "Failed to discover tests: No test in this run uses a container named 'unknown'. Available: container."),
        ] {
            for use_artifacts in [false, true] {
                let mut args = vec![
                    "panoramic", "run", "-d", cases.to_str().unwrap(),
                    "--subject-os", "linux", "--image-override", overrides,
                    "-l", logs.to_str().unwrap(),
                ];
                if use_artifacts {
                    args.extend(["--artifacts", manifest.to_str().unwrap()]);
                }
                let Command::Run(cmd) = Cli::try_parse_from(args).unwrap().command else {
                    panic!("expected run")
                };
                let (exit, diagnostic) = run_with_diagnostics(cmd, dir.path()).await;
                assert_eq!(exit, ExitCode::from(2));
                assert!(diagnostic.contains(expected), "{diagnostic}");
                assert!(!logs.exists());
            }
        }
    }

    #[test]
    fn runners_built_from_bindings_report_bound_images_to_kind_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            "type: integration\nname: integration\ntimeout: 1s\nruntimes: [linux, windows]\nprocedure: []\n",
        );
        for (name, runtime) in [("docker", "docker"), ("kind", "kubernetes_in_docker")] {
            write_case(
                dir.path(),
                name,
                &format!(
                    "type: correctness\n{}",
                    CORRECTNESS_FIELDS.replace("runtime: docker", &format!("runtime: {runtime}"))
                ),
            );
        }
        for (os, target) in [(Os::Linux, "linux:bound"), (Os::Windows, "windows:bound")] {
            let manifest = ArtifactManifest::new(
                "test",
                BTreeMap::from([
                    ("agent-adp-linux-image".into(), "linux:bound".into()),
                    ("agent-adp-windows-image".into(), "windows:bound".into()),
                    ("tools-linux-image".into(), "tools:bound".into()),
                ]),
            )
            .unwrap();
            let plans = discover_in_scope(
                &mut Planner::new(REGISTRIES, manifest).unwrap(),
                &[dir.path().to_path_buf()],
                os,
            )
            .unwrap()
            .plans;
            let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
            let built = build_runners(plans, os, &artifacts).unwrap();
            let listing = listing(built.iter().map(|built| &built.plan), &built, &[], os);
            assert_eq!(
                listing["integration"]["images"],
                serde_json::json!({"container": target})
            );
            for name in ["docker", "kind"] {
                if os != Os::Linux {
                    assert!(!listing.contains_key(name));
                    continue;
                }
                assert_eq!(
                    listing[name]["images"],
                    serde_json::json!({
                        "baseline": "linux:bound", "comparison": "linux:bound", "datadog-intake": "tools:bound", "millstone": "tools:bound",
                    })
                );
            }
            assert_eq!(
                collect_kind_images(&built),
                if os == Os::Linux {
                    vec!["linux:bound", "tools:bound"]
                } else {
                    vec![]
                }
            );
        }
    }

    #[tokio::test]
    async fn a_case_this_host_cannot_provide_is_an_errored_setup_row_and_the_run_exits_2() {
        let (os, kind) = if Os::host() == Os::MacOs {
            (Os::Windows, "windows-container")
        } else {
            (Os::MacOs, "host-process-group")
        };
        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "unprovidable",
            &PARTICIPANT_FIELDS.replace("[linux]", &format!("[{}]", os.name())),
        );
        let logs = dir.path().join("logs");
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            cases.to_str().unwrap(),
            "--subject-os",
            os.name(),
            "-t",
            "unprovidable",
            "--no-tui",
            "-o",
            "json",
            "-l",
            logs.to_str().unwrap(),
        ])
        .unwrap();
        let Command::Run(cmd) = cli.command else {
            panic!("expected run")
        };
        let console = Console { use_tui: false };
        let prepared = prepare_run(&cmd, &console)
            .await
            .unwrap_or_else(|_| panic!("run should prepare"));
        assert_eq!(prepared.built.len(), 1);
        let Execution::Unprovidable(message) = &prepared.built[0].execution else {
            panic!("unavailable realization must be an explicit execution state");
        };
        let message = message.clone();
        assert_eq!(report_run(prepared, &cmd, &console).await, ExitCode::from(2));
        let run_dir = std::fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
        let tests = report["tests"].as_array().unwrap();
        assert_eq!(tests.len(), 1);
        let result = &tests[0];
        assert_eq!(result["name"], "unprovidable");
        assert_eq!(result["outcome"], "errored");
        assert_eq!(result["error"]["kind"], "setup");
        assert_eq!(result["error"]["message"], message);
        assert!(result["error"]["message"].as_str().unwrap().starts_with(&format!(
            "realization {kind} on {} for participant 'adp' cannot be provided on this host: ",
            os.name()
        )));
        let layout = serde_json::json!([{
            "participant": "adp", "type": "agent-adp", "os": os.name(), "realization": kind,
        }]);
        assert_eq!(result["layout"], layout);
        let result_path = std::path::Path::new(result["log_dir"].as_str().unwrap()).join("result.json");
        let result_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(result_path).unwrap()).unwrap();
        assert_eq!(result_json["layout"], layout);
    }

    #[test]
    fn kind_images_come_only_from_cases_that_need_a_kubernetes_pod() {
        // Reporting the default runtime makes image collection rely on the plan's realization.
        struct DefaultRuntime(Box<dyn test::Test>);

        #[async_trait::async_trait]
        impl test::Test for DefaultRuntime {
            fn images(&self) -> BTreeMap<&str, String> {
                self.0.images()
            }

            async fn run(&self, _tctx: test::TestContext) -> panoramic_core::result::TestResult {
                panic!("collecting images must not run a case")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "docker",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        write_case(
            dir.path(),
            "pod",
            &format!(
                "type: correctness\n{}",
                CORRECTNESS_FIELDS
                    .replace("runtime: docker", "runtime: kubernetes_in_docker")
                    .replace("base:file", "base:pod")
                    .replace("comp:file", "comp:pod")
            ),
        );
        let plans = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
            .unwrap()
            .plans;
        let mut built: Vec<_> = build_runners(plans, Os::Linux, &RunnerArtifacts::Overrides(&[]))
            .unwrap()
            .into_iter()
            .map(|BuiltCase { plan, execution }| {
                let Execution::Runner(runner) = execution else {
                    panic!("expected a runner")
                };
                BuiltCase {
                    plan,
                    execution: Execution::Runner(Box::new(DefaultRuntime(runner))),
                }
            })
            .collect();
        let expected: std::collections::BTreeSet<_> = built
            .iter()
            .find(|built| built.plan.case().name() == "pod")
            .unwrap()
            .runner()
            .unwrap()
            .images()
            .into_values()
            .collect();
        let images = collect_kind_images(&built);
        assert_eq!(images, expected.into_iter().collect::<Vec<_>>());
        assert!(images.contains(&"base:pod".to_string()));
        assert!(images.contains(&"comp:pod".to_string()));
        assert!(!images.contains(&"base:file".to_string()));
        assert!(!images.contains(&"comp:file".to_string()));
        built
            .iter_mut()
            .find(|built| built.plan.case().name() == "pod")
            .unwrap()
            .execution = Execution::Unprovidable("kind unavailable".to_string());
        assert!(collect_kind_images(&built).is_empty());
    }

    #[test]
    fn build_revision_is_a_commit_sha_or_unknown() {
        let build_revision = build_revision();
        let revision = build_revision.trim_end_matches("-dirty");
        assert!(!revision.is_empty());
        assert!(
            revision == "unknown" || revision.chars().all(|c| c.is_ascii_hexdigit()),
            "revision was: {}",
            build_revision
        );
    }

    #[tokio::test]
    async fn image_override_validation_precedes_name_selection() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("unselected");
        std::fs::create_dir(&case_dir).unwrap();
        std::fs::write(
            case_dir.join("config.yaml"),
            r#"
type: correctness
runtime: docker
analysis_mode: metrics
baseline: {image: base}
comparison: {image: comp}
"#,
        )
        .unwrap();
        for (entry, expected) in [
            ("millstone=tools:custom", EXIT_NO_TESTS_SELECTED),
            ("typo=tools:custom", EXIT_HARNESS_ERROR),
        ] {
            let cli = Cli::try_parse_from([
                "panoramic",
                "run",
                "--subject-os",
                "linux",
                "-d",
                dir.path().to_str().unwrap(),
                "-t",
                "not-discovered",
                "--image-override",
                entry,
            ])
            .unwrap();
            let Command::Run(cmd) = cli.command else {
                panic!("expected run")
            };
            assert_eq!(run_tests(cmd, false).await, ExitCode::from(expected));
        }
    }

    #[tokio::test]
    async fn a_malformed_case_fails_run_before_any_report_even_when_another_case_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "good",
            "type: integration\nname: good\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
        );
        let bad = write_case(dir.path(), "bad", "type: bogus\n");
        let error = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
            .err()
            .expect("the malformed case must fail discovery");
        assert!(
            format!("{error:#}").contains(&bad.canonicalize().unwrap().display().to_string()),
            "{error:#}"
        );
        let logs = dir.path().join("logs");
        for selection in [Some("good"), None] {
            let mut args = vec![
                "panoramic",
                "run",
                "-d",
                dir.path().to_str().unwrap(),
                "--runtime",
                "linux",
                "-l",
                logs.to_str().unwrap(),
                "--no-tui",
            ];
            if let Some(name) = selection {
                args.extend(["-t", name]);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Run(cmd) = cli.command else {
                panic!("expected run");
            };
            assert_eq!(run_tests(cmd, false).await, ExitCode::from(EXIT_HARNESS_ERROR));
            assert!(!logs.exists(), "discovery must fail before creating the log directory");
        }
    }

    #[test]
    fn selection_matches_loaded_case_names_including_matrix_variants() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            "type: integration\nname: integration\ntimeout: 7s\nruntimes: [linux]\nprocedure: []\n",
        );
        write_case(
            dir.path(),
            "matrix",
            &format!("type: correctness_matrix\n{CORRECTNESS_FIELDS}variants: [{{name: second}}, {{name: first}}]\n"),
        );
        let cases = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
            .unwrap()
            .plans;
        let names: Vec<_> = cases.iter().map(|plan| plan.case().name()).collect();
        let (selected, unmatched) = resolve_selection("matrix/first, missing, integration", &names);
        assert_eq!(selected, ["matrix/first", "integration"]);
        assert_eq!(unmatched, ["missing"]);
    }

    #[tokio::test]
    async fn selection_rejects_disabled_cases() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "windows-only",
            "type: integration\nname: windows-only\ntimeout: 7s\nruntimes: [windows]\nprocedure: []\n",
        );
        write_case(
            dir.path(),
            "linux-only",
            "type: integration\nname: linux-only\ntimeout: 7s\nruntimes: [linux]\nprocedure: []\n",
        );
        write_case(
            dir.path(),
            "disabled",
            &format!("type: correctness\ndisabled: true\n{CORRECTNESS_FIELDS}"),
        );
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            dir.path().to_str().unwrap(),
            "--runtime",
            "windows",
            "-t",
            "disabled",
        ])
        .unwrap();
        let Command::Run(cmd) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(run_tests(cmd, false).await, ExitCode::from(EXIT_NO_TESTS_SELECTED));
    }

    #[test]
    fn selection_reports_names_that_match_nothing() {
        let discovered: Vec<&str> = Vec::new();
        let (selected, unmatched) = resolve_selection("no-such-test", &discovered);
        assert!(selected.is_empty());
        assert_eq!(unmatched, vec!["no-such-test".to_string()]);

        // An empty or whitespace-only selection matches nothing and names nothing, which the caller
        // treats as "no test selected" rather than a green run.
        let (selected, unmatched) = resolve_selection(" , ", &discovered);
        assert!(selected.is_empty());
        assert!(unmatched.is_empty());
    }
}
