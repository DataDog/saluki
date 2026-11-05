use std::{collections::BTreeMap, path::Path, process::ExitCode, sync::Arc};

use panoramic_core::artifact::{verified_artifacts, ArtifactBinding, VerifiedArtifact};
use panoramic_core::backend::RunBackends;
use panoramic_core::case::CaseConfig;
use panoramic_core::events::create_event_channel;
use panoramic_core::interrupts::Interrupts;
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::lifecycle::NativeInputs;
use panoramic_core::machine_output::{revision, RunReport, TestReport, RUN_REPORT_FILE_NAME};
use panoramic_core::plan::{needed_artifacts, CasePlan};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{
    artifact_commands, cli,
    collector::{self, consume_run, run_teardown, RunCollector, RunDisplay, RunEnd, EXIT_HARNESS_ERROR},
    config::{case_dir, needs_pod, Discovered},
    console::{Console, SetupFailure},
    discovery::{prepare_discovery, Selection},
    kind::KindLifecycle,
    providability,
    runner::{self, BuiltCase, NativeTest, Runner},
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

/// Selected plans and verified inputs ready for reporting and execution.
struct PreparedRun {
    subject_os: Os,
    plans: Vec<CasePlan>,
    unprovidable: BTreeMap<Realization, String>,
    excluded: Vec<CaseConfig>,
    verified: Vec<VerifiedArtifact>,
    native: Option<NativeInputs>,
}

pub(crate) async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    let console = Console { use_tui };
    match prepare_run(&cmd, &console).await {
        Ok(prepared) => report_run(prepared, &cmd, &console).await,
        Err(failure) => failure.report(&console),
    }
}

/// Discovers, selects and verifies the cases before starting a run.
async fn prepare_run(cmd: &cli::RunCommand, console: &Console) -> Result<PreparedRun, SetupFailure> {
    let Selection {
        subject_os,
        discovered: Discovered {
            mut plans,
            mut excluded,
        },
    } = prepare_discovery(&cmd.test_dirs, cmd.subject_os, cmd.artifacts.as_deref())?
        .discover_selection(console, false)?;
    select_cases(cmd, console, subject_os, &mut plans, &mut excluded)?;
    let backends = Arc::new(RunBackends::default().with_kube_context(format!("kind-{}", cmd.kind_cluster_name)));
    let unprovidable = check_providability(&backends, console, &plans).await;
    let (verified, native) = prepare_artifacts(backends, console, subject_os, &plans, &unprovidable).await?;
    Ok(PreparedRun {
        subject_os,
        plans,
        unprovidable,
        excluded,
        verified,
        native,
    })
}

/// Resolves the requested test names and announces cases excluded by the subject OS.
fn select_cases(
    cmd: &cli::RunCommand, console: &Console, subject_os: Os, plans: &mut Vec<CasePlan>, excluded: &mut Vec<CaseConfig>,
) -> Result<(), SetupFailure> {
    if plans.is_empty() && excluded.is_empty() {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        let msg = format!("No test cases found in: {}", dirs_str.join(", "));
        return Err(SetupFailure::harness(msg));
    }

    // Resolve the `-t` selection before doing any work: a name that matches nothing, or a
    // selection that leaves nothing to run, is an error rather than a green run of zero tests.
    let selected_tests = match cmd.tests.as_deref() {
        Some(requested) => {
            let names: Vec<_> = plans
                .iter()
                .map(|plan| plan.case().name())
                .chain(excluded.iter().map(CaseConfig::name))
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
    retain_selected(plans, selected_tests.as_deref(), |plan| plan.case().name());
    retain_selected(excluded, selected_tests.as_deref(), CaseConfig::name);

    if !excluded.is_empty() {
        let names = excluded.iter().map(CaseConfig::name).collect::<Vec<_>>().join(", ");
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
    backends: &RunBackends, console: &Console, plans: &[CasePlan],
) -> BTreeMap<Realization, String> {
    if plans.is_empty() {
        BTreeMap::new()
    } else {
        console.info("Checking that this host can provide the selected cases...");
        providability::check(backends, plans).await
    }
}

/// Verifies selected artifacts and loads native inputs before creating any run logs.
pub(crate) async fn prepare_artifacts(
    backends: Arc<RunBackends>, console: &Console, subject_os: Os, plans: &[CasePlan],
    unprovidable: &BTreeMap<Realization, String>,
) -> Result<(Vec<VerifiedArtifact>, Option<NativeInputs>), SetupFailure> {
    let bindings = run_artifacts(plans, unprovidable);
    let any_providable = plans
        .iter()
        .any(|plan| providability::is_providable(plan, unprovidable));
    let base = std::env::current_dir()
        .map_err(|error| SetupFailure::harness(format!("Cannot read the current directory: {error}")))?;
    let verified = verify_run_artifacts(&backends, &bindings, subject_os, &base, console).await?;
    let native = if any_providable {
        Some(NativeInputs::new(backends, &verified, &base).map_err(SetupFailure::harness)?)
    } else {
        None
    };
    Ok((verified, native))
}

/// Creates run reports, executes the cases and returns the run's exit code.
async fn report_run(prepared: PreparedRun, cmd: &cli::RunCommand, console: &Console) -> ExitCode {
    let PreparedRun {
        subject_os,
        plans,
        unprovidable,
        excluded,
        verified,
        native,
    } = prepared;
    // Create log directory.
    let log_dir = match std::path::absolute(cmd.log_dir()) {
        Ok(path) => path,
        Err(e) => {
            console.error(format!("Failed to resolve log directory: {}", e));
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
        verified,
        subject_os,
        cmd.parallelism.get(),
        log_dir.clone(),
        plans.iter().map(|plan| plan.case().name().to_string()).collect(),
        excluded
            .into_iter()
            .map(|case| TestReport::skipped(case.name().to_string(), case_dir(&case), subject_os))
            .collect(),
    );
    let mut collector = RunCollector::start(report, cmd.fail_fast);

    let display_error = if !plans.is_empty() {
        execute(
            plans,
            unprovidable,
            native,
            cmd,
            console,
            &log_dir,
            &mut collector,
            &mut interrupts,
        )
        .await
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
    plans: Vec<CasePlan>, unprovidable: BTreeMap<Realization, String>, native: Option<NativeInputs>,
    cmd: &cli::RunCommand, console: &Console, log_dir: &Path, collector: &mut RunCollector,
    interrupts: &mut Interrupts,
) -> Option<std::io::Error> {
    // Create the event channel early so the kind setup task can emit status messages.
    let (tx, rx) = create_event_channel();

    // Spawn kind cluster setup in the background so non-kind tests start immediately.
    // Kind tests wait on `kind_rx` before doing any work.
    let kind_images = collect_kind_images(&plans, &unprovidable, native.as_ref());
    let kind_lifecycle_slot = std::sync::Arc::new(Mutex::new(None::<KindLifecycle>));
    let slot = kind_lifecycle_slot.clone();
    let cluster_name = cmd.kind_cluster_name.clone();
    let event_tx = tx.clone();
    let kind_rx = start_kind_setup(&plans, &unprovidable, kind_images, move |images| async move {
        let lifecycle = KindLifecycle::ensure(cluster_name, images, event_tx)
            .await
            .map_err(|error| format!("{error:?}"))?;
        *slot.lock().await = Some(lifecycle);
        Ok(())
    });

    let mut registry = Runner::new(log_dir);
    let inputs = native.map(Arc::new);
    if let Some(ref rx) = kind_rx {
        registry = registry.with_kind_ready(rx.clone());
    }
    for plan in plans {
        let built = match providability::error(&plan, &unprovidable) {
            Some(message) => BuiltCase::unprovidable(plan, message),
            None => BuiltCase::new(
                plan,
                Box::new(NativeTest {
                    inputs: inputs.as_ref().expect("providable cases have prepared inputs").clone(),
                    types: crate::REGISTRIES.participant_types,
                }),
            ),
        };
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
                    console.info(format!(
                        "Skipping kind cluster teardown (--no-delete-kind-cluster). \
                         Cluster '{}' is still running.",
                        cluster_name
                    ));
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

/// Selects the roles needed by cases this host can provide.
fn run_artifacts(plans: &[CasePlan], unprovidable: &BTreeMap<Realization, String>) -> Vec<ArtifactBinding> {
    needed_artifacts(
        plans
            .iter()
            .filter(|plan| providability::is_providable(plan, unprovidable)),
    )
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

/// Collects unique verified image references for providable cases' Kubernetes pods.
fn collect_kind_images(
    plans: &[CasePlan], unprovidable: &BTreeMap<Realization, String>, native: Option<&NativeInputs>,
) -> Result<Vec<String>, String> {
    use std::collections::BTreeSet;

    let mut images = BTreeSet::new();
    for plan in plans {
        if !needs_pod(plan) || !providability::is_providable(plan, unprovidable) {
            continue;
        }
        let inputs = native.ok_or("Native execution inputs were not prepared.")?;
        for &role in plan.image_roles_for(RealizationKind::KubernetesPod) {
            images.insert(inputs.image_reference(role)?.to_string());
        }
    }
    Ok(images.into_iter().collect())
}

/// Starts cluster setup for providable pod cases, even when they have no images to load.
fn start_kind_setup<F>(
    plans: &[CasePlan], unprovidable: &BTreeMap<Realization, String>, images: Result<Vec<String>, String>,
    setup: impl FnOnce(Vec<String>) -> F + Send + 'static,
) -> Option<crate::test::KindReadyReceiver>
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    if !plans
        .iter()
        .any(|plan| needs_pod(plan) && providability::is_providable(plan, unprovidable))
    {
        return None;
    }
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        let result = match images {
            Ok(images) => setup(images).await,
            Err(error) => Err(error),
        };
        let _ = tx.send(Some(result));
    });
    Some(rx)
}

/// Revision of the repository this binary was built from, as recorded by the build script.
fn build_revision() -> String {
    revision(env!("VERGEN_GIT_SHA"), env!("VERGEN_GIT_DIRTY"))
}

#[cfg(test)]
mod tests {

    use clap::Parser as _;
    use panoramic_core::{
        artifact::{ArtifactKind, ArtifactManifest},
        layout::Os,
        plan::Planner,
    };

    use super::*;
    use crate::{
        cli::{Cli, Command},
        collector::EXIT_NO_TESTS_SELECTED,
        config::{
            discover_in_scope,
            tests::{planner, write_case, PARTICIPANT_FIELDS},
        },
        discovery::NO_TEST_DIRECTORIES,
        list_command::listing,
        REGISTRIES,
    };

    async fn run_skipped_selection(requested: Option<&str>, manifest: Option<ArtifactManifest>) {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "windows",
            "name: windows\ntimeout: 1s\nsubject_os: [windows]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        write_case(&cases, "correctness", PARTICIPANT_FIELDS);
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
            run_tests(*cmd, false).with_subscriber(subscriber).await,
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
        assert_eq!(report["schema_version"], 4);
        assert_eq!(report["state"], "complete");
        assert_eq!(report["subject_os"], "macos");
        assert!(report.get("runtime").is_none());
        assert_eq!(report["artifacts"], serde_json::json!([]));
        assert!(diagnostic.contains("No artifacts needed on macos."), "{diagnostic}");
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
            "name: available\ntimeout: 1s\nsubject_os: [macos]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
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
            run_tests(*cmd, false).with_subscriber(subscriber).await,
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
    async fn run_validates_directories_then_manifest() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("missing.toml");
        let logs = dir.path().join("logs");
        for (has_dirs, expected) in [
            (false, NO_TEST_DIRECTORIES.to_string()),
            (
                true,
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
                run_tests(*cmd, false).with_subscriber(subscriber).await,
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
        write_case(dir.path(), "correctness", PARTICIPANT_FIELDS);
        write_case(
            dir.path(),
            "host",
            "name: host\ntimeout: 1s\nsubject_os: [macos]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
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
            let Discovered { mut plans, excluded } =
                discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::MacOs).unwrap();
            assert_eq!(
                excluded.iter().map(CaseConfig::name).collect::<Vec<_>>(),
                ["correctness"]
            );
            let names: Vec<_> = plans
                .iter()
                .map(|plan| plan.case().name())
                .chain(excluded.iter().map(CaseConfig::name))
                .collect();
            let selected = selected.map(|requested| {
                let (selected, unmatched) = resolve_selection(requested, &names);
                assert!(unmatched.is_empty());
                selected
            });
            retain_selected(&mut plans, selected.as_deref(), |plan| plan.case().name());
            let unprovidable = unavailable
                .into_iter()
                .map(|realization| (realization, "unavailable".into()))
                .collect();
            let bindings = run_artifacts(&plans, &unprovidable);
            assert_eq!(
                bindings.iter().map(|binding| binding.role.name).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn artifact_bindings_follow_selection_and_providability() {
        use panoramic_core::layout::RealizationKind;

        use crate::config::tests::plan_with_plain;

        let plain = PARTICIPANT_FIELDS.replace("agent-adp", "plain");
        let unavailable = plain.replace("  adp:", "  windows: {type: agent-adp, os: windows}\n  adp:");
        let mut plans = vec![
            plan_with_plain(&plain, "plain", Os::Linux),
            plan_with_plain(&unavailable, "unavailable", Os::Linux),
            plan_with_plain(PARTICIPANT_FIELDS, "selected", Os::Linux),
            plan_with_plain(PARTICIPANT_FIELDS, "unselected", Os::Linux),
        ];
        let selected = ["plain".into(), "unavailable".into(), "selected".into()];
        retain_selected(&mut plans, Some(&selected), |plan| plan.case().name());
        let unprovidable = BTreeMap::from([(
            Realization {
                os: Os::Windows,
                kind: RealizationKind::WindowsContainer,
            },
            "unavailable".into(),
        )]);
        let bindings = run_artifacts(&plans, &unprovidable);
        assert_eq!(
            bindings.iter().map(|binding| binding.role.name).collect::<Vec<_>>(),
            ["agent-adp-linux-image", "courier-linux-binary", "fixup-image"]
        );
        plans.retain(|plan| plan.case().name() == "unavailable");
        assert!(run_artifacts(&plans, &unprovidable).is_empty());
    }

    #[tokio::test]
    async fn an_empty_selection_verifies_no_artifacts_and_needs_no_execution_inputs() {
        let (verified, native) = prepare_artifacts(
            Arc::new(RunBackends::default()),
            &Console { use_tui: false },
            Os::host(),
            &[],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert!(verified.is_empty());
        assert!(native.is_none());
    }

    fn native_file_run(dir: &Path) -> (cli::RunCommand, Vec<CasePlan>) {
        use panoramic_core::{
            artifact::{ArtifactKind, ArtifactRole},
            case::{CaseLoader, Registries},
            layout::RealizationKind,
            participant::ParticipantType,
        };

        struct LocalFiles;

        impl ParticipantType for LocalFiles {
            fn name(&self) -> &'static str {
                "local-files"
            }

            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                &[
                    (Os::Linux, RealizationKind::HostProcessGroup),
                    (Os::MacOs, RealizationKind::HostProcessGroup),
                    (Os::Windows, RealizationKind::HostProcessGroup),
                ]
            }

            fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
                &[
                    ArtifactRole {
                        name: "native-binary",
                        kind: ArtifactKind::Binary,
                        local_default: "",
                    },
                    ArtifactRole {
                        name: "native-install",
                        kind: ArtifactKind::Install {
                            main_binary: "bin/main",
                        },
                        local_default: "",
                    },
                ]
            }

            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }

        // Synthetic host plans keep providability and verification local. In-memory bindings supply fixture paths
        // independently of the command's --artifacts flag.
        let registries = Registries {
            participant_types: &[&LocalFiles],
            ..REGISTRIES
        };
        let os = Os::host();
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", &format!("[{}]", os.name()))
            .replace("agent-adp", "local-files")
            .replace(
                "subject: true",
                "subject: true, realization: {kind: host-process-group}",
            );
        let case = CaseLoader::new(registries)
            .load_content("cases", &yaml, "native", &dir.join("native/config.yaml"))
            .unwrap()
            .remove(0);
        let bindings = ArtifactManifest::new(
            "fixture",
            BTreeMap::from([
                ("native-binary".into(), dir.join("binary").to_str().unwrap().into()),
                ("native-install".into(), dir.join("install").to_str().unwrap().into()),
            ]),
        )
        .unwrap();
        let plan = Planner::new(registries, bindings)
            .unwrap()
            .compile(case, os)
            .unwrap()
            .into_plan()
            .unwrap();
        let plans = vec![plan];
        let logs = dir.join("logs");
        let Command::Run(cmd) = Cli::try_parse_from([
            "panoramic",
            "run",
            "--subject-os",
            os.name(),
            "--no-tui",
            "-l",
            logs.to_str().unwrap(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected run")
        };
        assert!(cmd.artifacts.is_none());
        (*cmd, plans)
    }

    #[tokio::test]
    async fn native_preparation_without_the_artifacts_flag_fails_verification_before_creating_logs() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let (cmd, plans) = native_file_run(dir.path());
        let console = Console { use_tui: false };
        let backends = Arc::new(RunBackends::default());
        let unprovidable = check_providability(&backends, &console, &plans).await;
        assert!(unprovidable.is_empty());
        let output = std::fs::File::create(dir.path().join("diagnostic.log")).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Mutex::new(output))
            .finish();
        let failure = prepare_artifacts(backends, &console, Os::host(), &plans, &unprovidable)
            .with_subscriber(subscriber)
            .await
            .err()
            .expect("missing native files must fail preparation without --artifacts");
        assert_eq!(failure.message, "2 of 2 artifact(s) failed verification.");
        assert_eq!(failure.report(&console), ExitCode::from(EXIT_HARNESS_ERROR));
        let diagnostic = std::fs::read_to_string(dir.path().join("diagnostic.log")).unwrap();
        assert_eq!(diagnostic.matches("FAILED:").count(), 2, "{diagnostic}");
        for role in ["native-binary", "native-install"] {
            assert!(diagnostic.contains(&format!("{role} =")), "{diagnostic}");
        }
        assert!(!dir.path().join("logs").exists());
        assert!(!cmd.log_dir().join(RUN_REPORT_FILE_NAME).exists());
    }

    #[tokio::test]
    async fn native_verification_without_the_artifacts_flag_records_artifact_identities_in_run_json() {
        use panoramic_core::artifact::test_util::write_executable;

        let dir = tempfile::tempdir().unwrap();
        let (cmd, plans) = native_file_run(dir.path());
        write_executable(&dir.path().join("binary"));
        write_executable(&dir.path().join("install/bin/main"));
        let console = Console { use_tui: false };
        let backends = Arc::new(RunBackends::default());
        let unprovidable = check_providability(&backends, &console, &plans).await;
        assert!(unprovidable.is_empty());
        let (verified, native) = prepare_artifacts(backends, &console, Os::host(), &plans, &unprovidable)
            .await
            .unwrap();
        assert!(native.is_some());
        assert!(!dir.path().join("logs").exists());
        let prepared = PreparedRun {
            subject_os: Os::host(),
            plans,
            unprovidable,
            excluded: vec![],
            verified,
            native,
        };
        // The fixture type is registered only for planning, so runtime preparation fails after the bindings are reported.
        assert_eq!(
            report_run(prepared, &cmd, &console).await,
            ExitCode::from(EXIT_HARNESS_ERROR)
        );
        let run_dir = std::fs::read_dir(dir.path().join("logs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join(RUN_REPORT_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(
            report["artifacts"],
            serde_json::json!([
                {
                    "role": "native-binary",
                    "reference": dir.path().join("binary"),
                    "identity": "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d",
                    "kind": "binary",
                },
                {
                    "role": "native-install",
                    "reference": dir.path().join("install"),
                    "identity": "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d",
                    "kind": "install",
                },
            ])
        );
        assert_eq!(report["state"], "complete");
        assert_eq!(report["tests"][0]["outcome"], "errored");
        assert_eq!(report["tests"][0]["error"]["kind"], "setup");
        assert_eq!(
            report["tests"][0]["error"]["message"],
            if cfg!(unix) {
                "participant 'adp' has unregistered type 'local-files'".to_string()
            } else {
                format!(
                    "panoramic cannot create host-process-group participants natively: 'adp' on {}",
                    Os::host().name()
                )
            }
        );
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
            "name: host\ntimeout: 1s\nsubject_os: [macos]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
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
            run_tests(*cmd, false).with_subscriber(subscriber).await,
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

    struct PodFixture;

    impl panoramic_core::participant::ParticipantType for PodFixture {
        fn name(&self) -> &'static str {
            "pod-fixture"
        }

        fn supports(&self) -> &'static [(Os, panoramic_core::layout::RealizationKind)] {
            &[(Os::Linux, panoramic_core::layout::RealizationKind::KubernetesPod)]
        }

        fn artifacts(&self, _realization: Realization) -> &'static [panoramic_core::artifact::ArtifactRole] {
            &[
                panoramic_participants::AgentAdp::LINUX_IMAGE,
                panoramic_participants::TOOLS_LINUX_IMAGE,
            ]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    #[test]
    fn correctness_ci_environment_accepts_native_plans() {
        use clap::CommandFactory as _;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let mixins: serde_json::Value =
            serde_saphyr::from_str(&std::fs::read_to_string(root.join(".gitlab/correctness-mixins.yml")).unwrap())
                .unwrap();
        let command = Cli::command();
        let run = command.find_subcommand("run").unwrap();
        let mut variables = BTreeMap::new();
        for mixin in mixins.as_object().unwrap().values() {
            if let Some(values) = mixin.get("variables") {
                for (name, value) in values.as_object().unwrap() {
                    let value = value.as_str().unwrap();
                    match name.as_str() {
                        // Resource requests belong to GitLab's Kubernetes executor.
                        "KUBERNETES_CPU_REQUEST" | "KUBERNETES_MEMORY_REQUEST" | "KUBERNETES_MEMORY_LIMIT" => {}
                        "RUST_LOG" => {
                            saluki_common::logging::parse_filter_directives(value).unwrap();
                        }
                        _ => assert!(
                            run.get_arguments()
                                .any(|arg| arg.get_env() == Some(std::ffi::OsStr::new(name))),
                            "correctness CI variable {name} must be consumed by the CLI"
                        ),
                    }
                    variables.insert(name.as_str(), value);
                }
            }
        }
        const CHILD: &str = "PANORAMIC_CI_ENV_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Exercise clap's environment parsing without changing concurrent tests' environment.
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            for arg in run.get_arguments() {
                if let Some(env) = arg.get_env() {
                    child.env_remove(env);
                }
            }
            let output = child
                .env_remove("RUST_LOG")
                .envs(&variables)
                .env(CHILD, "1")
                .args([
                    "--exact",
                    "run_command::tests::correctness_ci_environment_accepts_native_plans",
                    "--nocapture",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("artifacts.toml");
        let render = mixins[".test-correctness-definition"]["script"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap())
            .find(|line| line.starts_with("target/release/panoramic artifacts render "))
            .unwrap();
        let Command::Artifacts(cli::ArtifactsCommand {
            command: cli::ArtifactsSubcommand::Render(mut render),
        }) = Cli::try_parse_from(render.split_whitespace()).unwrap().command
        else {
            panic!("expected artifact render");
        };
        render.output = manifest.clone();
        assert_eq!(artifact_commands::render(render), ExitCode::SUCCESS);
        let cases = root.join("test/correctness/cases");
        let Command::Run(cmd) = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            cases.to_str().unwrap(),
            "-t",
            "dsd-plain",
            "--subject-os",
            "linux",
            "--artifacts",
            manifest.to_str().unwrap(),
            "--no-tui",
        ])
        .unwrap()
        .command
        else {
            panic!("expected run")
        };
        let console = Console { use_tui: false };
        let Selection {
            subject_os,
            discovered: Discovered {
                mut plans,
                mut excluded,
            },
        } = prepare_discovery(&cmd.test_dirs, cmd.subject_os, cmd.artifacts.as_deref())
            .unwrap()
            .discover_selection(&console, false)
            .unwrap();
        assert_eq!(plans.len(), 46);
        assert!(excluded.is_empty());
        assert!(cmd.log_dir().starts_with(variables["PANORAMIC_LOG_DIR"]));
        select_cases(&cmd, &console, subject_os, &mut plans, &mut excluded).unwrap();
        assert_eq!(
            plans.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
            ["dsd-plain"]
        );
    }

    fn plans_with_pods(dir: &Path, os: Os, manifest: ArtifactManifest) -> Vec<CasePlan> {
        let registries = panoramic_core::case::Registries {
            participant_types: &[&panoramic_participants::AgentAdp, &PodFixture],
            ..REGISTRIES
        };
        let mut planner = Planner::new(registries, manifest).unwrap();
        panoramic_core::case::CaseLoader::new(registries)
            .discover_cases(&[dir.to_path_buf()])
            .unwrap()
            .into_iter()
            .filter_map(|case| planner.compile(case, os).unwrap().into_plan())
            .collect()
    }

    fn kind_inputs(plans: &[CasePlan], base: &Path) -> NativeInputs {
        use panoramic_core::artifact::ArtifactVerification;

        let verified = verified_artifacts(
            &needed_artifacts(plans)
                .into_iter()
                .filter(|binding| matches!(binding.role.kind, ArtifactKind::Image { .. }))
                .map(|binding| ArtifactVerification {
                    identity: Ok(format!("verified:{}", binding.role.name)),
                    binding,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        NativeInputs::new(Arc::new(RunBackends::default()), &verified, base).unwrap()
    }

    #[test]
    fn plans_report_bound_images_to_kind_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            &PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]"),
        );
        write_case(dir.path(), "docker", PARTICIPANT_FIELDS);
        let pod = PARTICIPANT_FIELDS
            .replace("agent-adp", "pod-fixture")
            .replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        write_case(dir.path(), "kind", &pod);
        for os in [Os::Linux, Os::Windows] {
            let manifest = ArtifactManifest::new(
                "test",
                BTreeMap::from([
                    ("agent-adp-linux-image".into(), "linux:bound".into()),
                    ("agent-adp-windows-image".into(), "windows:bound".into()),
                    ("tools-linux-image".into(), "tools:bound".into()),
                ]),
            )
            .unwrap();
            let plans = plans_with_pods(dir.path(), os, manifest);
            let listing = listing(&plans, &[], os);
            assert!(listing
                .values()
                .all(|row| row.get("images").is_none() && row.get("runtime").is_none()));
            assert_eq!(
                listing["integration"]["roles"][0]["reference"],
                if os == Os::Linux {
                    "linux:bound"
                } else {
                    "windows:bound"
                }
            );
            let inputs = kind_inputs(&plans, dir.path());
            assert_eq!(
                collect_kind_images(&plans, &BTreeMap::new(), Some(&inputs)).unwrap(),
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
        assert_eq!(prepared.plans.len(), 1);
        let message = providability::error(&prepared.plans[0], &prepared.unprovidable)
            .expect("unavailable realization must report a setup error");
        assert_eq!(report_run(prepared, &cmd, &console).await, ExitCode::from(2));
        let run_dir = std::fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
        let tests = report["tests"].as_array().unwrap();
        assert_eq!(tests.len(), 1);
        assert_eq!(report["artifacts"], serde_json::json!([]));
        assert_eq!(report["schema_version"], 4);
        assert!(report.get("runtime").is_none());
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
    fn kind_images_come_only_from_providable_cases_that_need_a_kubernetes_pod() {
        use crate::{config::tests::plan, runner::test_support::native_pod_plan};

        let dir = tempfile::tempdir().unwrap();
        let plans = [plan(PARTICIPANT_FIELDS, Os::Linux), native_pod_plan()];
        let inputs = kind_inputs(&plans, dir.path());
        assert_eq!(
            collect_kind_images(&plans, &BTreeMap::new(), Some(&inputs)).unwrap(),
            ["pod:bound"]
        );
        assert!(collect_kind_images(&plans, &BTreeMap::new(), None)
            .unwrap_err()
            .contains("inputs were not prepared"));
        let empty_inputs = NativeInputs::new(Arc::new(RunBackends::default()), &[], dir.path()).unwrap();
        assert_eq!(
            collect_kind_images(&plans, &BTreeMap::new(), Some(&empty_inputs)).unwrap_err(),
            "native inputs lack verified image role 'pod-image'",
        );

        for kind in [RealizationKind::KubernetesPod, RealizationKind::LinuxContainer] {
            let unprovidable = BTreeMap::from([(Realization { os: Os::Linux, kind }, "unavailable".into())]);
            assert!(collect_kind_images(&plans, &unprovidable, None).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_native_pod_case_starts_kind_even_without_images_to_load() {
        use crate::runner::test_support::native_pod_plan;

        let mut ready = start_kind_setup(
            &[native_pod_plan()],
            &BTreeMap::new(),
            Ok(Vec::new()),
            |images| async move {
                assert!(images.is_empty());
                Ok(())
            },
        )
        .expect("a native pod case needs cluster setup");
        tokio::time::timeout(std::time::Duration::from_secs(5), ready.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*ready.borrow(), Some(Ok(())));
        assert!(start_kind_setup(&[], &BTreeMap::new(), Ok(Vec::new()), |_| async {
            panic!("a run without pod cases must not start a cluster")
        })
        .is_none());
    }

    #[tokio::test]
    async fn unprovidable_pod_cases_do_not_start_kind() {
        // A providability failure must not start a cluster on the rejected Docker context.
        let plan = crate::runner::test_support::native_pod_plan();
        let unprovidable = BTreeMap::from([(
            Realization {
                os: Os::Linux,
                kind: RealizationKind::KubernetesPod,
            },
            "Docker context mismatch".into(),
        )]);
        assert!(start_kind_setup(&[plan], &unprovidable, Ok(Vec::new()), |_| async {
            panic!("unprovidable pods must not start a cluster")
        })
        .is_none());
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
    async fn a_malformed_case_fails_run_before_any_report_even_when_another_case_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "good",
            "name: good\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
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
                "--subject-os",
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
            assert_eq!(run_tests(*cmd, false).await, ExitCode::from(EXIT_HARNESS_ERROR));
            assert!(!logs.exists(), "discovery must fail before creating the log directory");
        }
    }

    #[test]
    fn selection_matches_loaded_case_names() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            "name: integration\ntimeout: 7s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        for name in ["matrix-first", "matrix-second"] {
            write_case(dir.path(), name, PARTICIPANT_FIELDS);
        }
        let cases = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
            .unwrap()
            .plans;
        let names: Vec<_> = cases.iter().map(|plan| plan.case().name()).collect();
        let (selected, unmatched) = resolve_selection("matrix-first, missing, integration", &names);
        assert_eq!(selected, ["matrix-first", "integration"]);
        assert_eq!(unmatched, ["missing"]);
    }

    #[tokio::test]
    async fn selection_rejects_cases_outside_the_discovery_roots() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "windows-only",
            "name: windows-only\ntimeout: 7s\nsubject_os: [windows]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        write_case(
            dir.path(),
            "linux-only",
            "name: linux-only\ntimeout: 7s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
        );
        write_case(&dir.path().join("disabled-cases"), "disabled", PARTICIPANT_FIELDS);
        let cli = Cli::try_parse_from([
            "panoramic",
            "run",
            "-d",
            dir.path().to_str().unwrap(),
            "--subject-os",
            "linux",
            "-t",
            "disabled",
        ])
        .unwrap();
        let Command::Run(cmd) = cli.command else {
            panic!("expected run")
        };
        // OS exclusion must not hide recursive discovery of the nested Linux case.
        let console = Console { use_tui: false };
        let failure = prepare_run(&cmd, &console)
            .await
            .err()
            .expect("a case outside the discovery roots must not match");
        assert!(
            failure.message.contains("No test matches: disabled"),
            "{}",
            failure.message
        );
        assert_eq!(failure.report(&console), ExitCode::from(EXIT_NO_TESTS_SELECTED));
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
