//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{borrow::Borrow, collections::BTreeMap, fmt::Write as _, path::Path};
use std::{io::IsTerminal, process::ExitCode};

use clap::Parser as _;
use panoramic_core::artifact::ArtifactBinding;
use panoramic_core::case::{CaseLoader, Registries};
use panoramic_core::events::create_event_channel;
use panoramic_core::interrupts::Interrupts;
use panoramic_core::layout::Realization;
use panoramic_core::machine_output::{revision, RunReport, TestReport, RUN_REPORT_FILE_NAME};
use panoramic_core::plan::needed_artifacts;
use saluki_common::logging::{filter_from_env, parse_filter_directives};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::{runner::Runner, test::RunnerSettings};

mod kind;
use self::kind::KindLifecycle;

mod actions;
mod artifact_commands;
mod assertions;
mod cli;
mod collector;
use self::collector::{
    consume_run, run_teardown, RunCollector, RunDisplay, RunEnd, EXIT_HARNESS_ERROR, EXIT_NO_TESTS_SELECTED,
};

mod correctness;
use self::cli::{Cli, Command, LogLevel};

mod config;
mod dynamic_vars;
mod integration;
mod mounts;
use self::config::{
    build_runners, discover_in_scope, load_planner,
    runner_artifacts::{BoundArtifacts, RunnerArtifacts},
    runtime_for_os,
    typed_case::{needs_pod, realizations, LoadedCase, LoadedPlan, TypedConfigLoader},
    BuiltCase, Discovered, NO_TEST_DIRECTORIES,
};

mod image_override;
mod intake;

#[cfg(test)]
mod case_loader_tests;
#[cfg(test)]
mod crate_dependencies_tests;
#[cfg(test)]
mod source_boundary_tests;

/// Participant types, steps and typed-schema artifact roles available to loading and compilation.
pub(crate) const REGISTRIES: Registries<'static> = Registries {
    participant_types: panoramic_participants::REGISTERED_TYPES,
    steps: panoramic_steps::REGISTERED_STEPS,
    typed_case_roles: &[panoramic_participants::TOOLS_LINUX_IMAGE],
};

/// Loads case files with the participant types and steps this binary registers.
pub(crate) const CASE_LOADER: CaseLoader<'static, TypedConfigLoader> =
    CaseLoader::new(REGISTRIES, TypedConfigLoader, test::TestSuite::Integration.name());

mod text_output;
use self::text_output::{OutputFormat, Reporter};

mod providability;
mod runner;
mod test;
mod test_env;
mod traffic;
mod tui;
mod unix_runner;
mod utils;

#[cfg(not(windows))]
fn default_crypto_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

#[cfg(windows)]
fn default_crypto_provider() -> rustls::crypto::CryptoProvider {
    rustls_cng_crypto::default_provider()
}

#[tokio::main]
async fn main() -> ExitCode {
    // Install the rustls crypto provider once at startup. reqwest is built without selecting a provider, so the
    // process-wide provider must be installed before any Rustls client configuration is built.
    let _ = default_crypto_provider().install_default();

    let cli = Cli::parse();

    // See if we should use TUI mode.
    //
    // This influences how we configure things since some output gets redirected/rendered differently in TUI mode.
    let (use_tui, is_test_run) = match &cli.command {
        Command::Run(cmd) => (
            !cmd.no_tui && matches!(cmd.output, OutputFormat::Text) && std::io::stdout().is_terminal(),
            true,
        ),
        Command::List(_) | Command::Artifacts(_) => (false, false),
    };

    if !use_tui {
        initialize_logging(cli.log_level);
        if is_test_run {
            info!("Panoramic starting...");
        }
    }

    let result = match cli.command {
        Command::Run(cmd) => run_tests(cmd, use_tui).await,
        Command::List(cmd) => list_tests(cmd).await,
        Command::Artifacts(cmd) => match cmd.command {
            cli::ArtifactsSubcommand::Render(cmd) => artifact_commands::render(cmd),
            cli::ArtifactsSubcommand::Verify(cmd) => artifact_commands::verify(cmd, &mut std::io::stdout()).await,
            cli::ArtifactsSubcommand::Pull(cmd) => artifact_commands::pull(cmd, &mut std::io::stdout()).await,
        },
    };

    if !use_tui {
        match result {
            ExitCode::SUCCESS => {
                if is_test_run {
                    info!("Panoramic stopped.")
                }
            }
            _ => error!("Panoramic stopped with errors."),
        }
    }

    result
}

fn initialize_logging(log_level: LogLevel) {
    let default_filter = parse_filter_directives(&log_level.filter_directives())
        .expect("first-party log filter directives should always be valid");

    tracing_subscriber::registry()
        .with(filter_from_env(default_filter))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .with_target(false)
                .with_thread_ids(false)
                .compact(),
        )
        .init();
}

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

async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    if cmd.test_dirs.is_empty() {
        if use_tui {
            eprintln!("{NO_TEST_DIRECTORIES}");
        } else {
            error!("{NO_TEST_DIRECTORIES}");
        }
        return ExitCode::from(2);
    }

    let subject_os = match cmd.subject_os.resolve() {
        Ok(os) => os,
        Err(message) => {
            if use_tui {
                eprintln!("{message}");
            } else {
                error!("{message}");
            }
            return ExitCode::from(2);
        }
    };
    let mut planner = match load_planner(cmd.artifacts.as_deref()) {
        Ok(planner) => planner,
        Err(e) => {
            if use_tui {
                eprintln!("{e}");
            } else {
                error!("{e}");
            }
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };
    let integration_runtime = runtime_for_os(subject_os);
    let Discovered { plans, mut excluded } = match discover_in_scope(&mut planner, &cmd.test_dirs, subject_os) {
        Ok(discovered) => discovered,
        Err(e) => {
            if use_tui {
                eprintln!("Failed to discover tests: {:#}", e);
            } else {
                error!("Failed to discover tests: {:#}", e);
            }
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };
    let mut settings = cmd.runner_settings();
    let artifacts = if cmd.artifacts.is_some() {
        let bound = BoundArtifacts::new(needed_artifacts(&plans));
        bound.apply_runner_settings(&mut settings);
        RunnerArtifacts::Bound(bound)
    } else {
        RunnerArtifacts::Overrides(&cmd.image_overrides)
    };
    let mut built = match build_runners(plans, subject_os, &artifacts) {
        Ok(built) => built,
        Err(e) => {
            if use_tui {
                eprintln!("Failed to discover tests: {:#}", e);
            } else {
                error!("Failed to discover tests: {:#}", e);
            }
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };

    debug!(
        adp_binary_path = %settings.adp_binary_path.display(),
        core_agent_binary_path = %settings.core_agent_binary_path.display(),
        mounts_dir = %settings.mounts_dir.display(),
        alpine_image = %settings.alpine_image,
        "Resolved runner settings."
    );

    if built.is_empty() && excluded.is_empty() {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        let msg = format!("No test cases found in: {}", dirs_str.join(", "));
        if use_tui {
            eprintln!("{}", msg);
        } else {
            error!("{}", msg);
        }
        return ExitCode::from(2);
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
                if use_tui {
                    eprintln!("{}", msg);
                } else {
                    error!("{}", msg);
                }
                return ExitCode::from(EXIT_NO_TESTS_SELECTED);
            }
            if selected.is_empty() {
                let msg = "No tests selected. Drop -t, or name at least one test.";
                if use_tui {
                    eprintln!("{}", msg);
                } else {
                    error!("{}", msg);
                }
                return ExitCode::from(EXIT_NO_TESTS_SELECTED);
            }
            Some(selected)
        }
        None => None,
    };
    // Register only the selected tests so the runner and `run.json` share one selection.
    retain_selected(&mut built, selected_tests.as_deref(), |built| built.plan.case().name());
    retain_selected(&mut excluded, selected_tests.as_deref(), LoadedCase::name);

    if !excluded.is_empty() {
        let names = excluded.iter().map(LoadedCase::name).collect::<Vec<_>>().join(", ");
        let message = format!(
            "Skipping {} test(s) whose subject_os excludes {}: {names}",
            excluded.len(),
            subject_os.name()
        );
        if use_tui {
            eprintln!("{message}");
        } else {
            info!("{message}");
        }
    }

    let unprovidable = if built.is_empty() {
        BTreeMap::new()
    } else {
        if use_tui {
            eprintln!("Checking that this host can provide the selected cases...");
        } else {
            info!("Checking that this host can provide the selected cases...");
        }
        providability::check(&mut built).await
    };
    if cmd.artifacts.is_some() {
        let bindings = run_artifacts(&built, &unprovidable);
        let base = match std::env::current_dir() {
            Ok(base) => base,
            Err(error) => {
                if use_tui {
                    eprintln!("Cannot read the current directory: {error}");
                } else {
                    error!("Cannot read the current directory: {error}");
                }
                return ExitCode::from(EXIT_HARNESS_ERROR);
            }
        };
        if use_tui {
            eprintln!("Verifying the artifacts the selected cases need…");
        } else {
            info!("Verifying the artifacts the selected cases need…");
        }
        let (report, failed) = artifact_commands::verify_bindings(subject_os, &bindings, &base).await;
        let report = report.trim_end();
        if use_tui {
            eprintln!("{report}");
        } else if failed == 0 {
            info!("{report}");
        } else {
            error!("{report}");
        }
        if failed != 0 {
            let n = bindings.len();
            if use_tui {
                eprintln!("{failed} of {n} artifact(s) failed verification.");
            } else {
                error!("{failed} of {n} artifact(s) failed verification.");
            }
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    }

    // Create log directory.
    let log_dir = match std::path::absolute(cmd.log_dir()) {
        Ok(path) => path,
        Err(e) => {
            error!("Failed to resolve log directory: {}", e);
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        if use_tui {
            eprintln!("Failed to create log directory: {}", e);
        } else {
            error!("Failed to create log directory: {}", e);
        }
        return ExitCode::from(2);
    }

    // Install interrupt handlers before starting background work.
    let mut interrupts = Interrupts::install();
    let mut collector = RunCollector::start(
        RunReport::new(
            build_revision(),
            cmd.test_dirs.clone(),
            integration_runtime.to_string(),
            subject_os,
            cmd.parallelism.get(),
            log_dir.clone(),
            built.iter().map(|built| built.plan.case().name().to_string()).collect(),
            excluded
                .into_iter()
                .map(|case| {
                    TestReport::skipped(
                        case.name().to_string(),
                        case.source().parent().unwrap_or(Path::new("")).to_path_buf(),
                        subject_os,
                    )
                })
                .collect(),
        ),
        cmd.fail_fast,
    );

    let display_error = if !built.is_empty() {
        execute(
            built,
            settings,
            &cmd,
            use_tui,
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
        if use_tui {
            eprintln!("{message}");
        } else {
            warn!("{message}");
        }
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
    built: Vec<BuiltCase>, settings: RunnerSettings, cmd: &cli::RunCommand, use_tui: bool, log_dir: &Path,
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
    let display = if use_tui {
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
        // The TUI has shut down by now, so its notices go to stderr directly.
        let notice = |message: String| {
            if use_tui {
                eprintln!("{}", message);
            } else {
                warn!("{}", message);
            }
        };
        let cluster_name = &cmd.kind_cluster_name;

        if consumed.end == RunEnd::Abandoned {
            notice(format!(
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
                    notice(format!(
                        "Received {}; abandoned kind cluster teardown. A 'kind delete' already started may still \
                         finish. If cluster '{}' is still running, delete it with 'kind delete cluster --name {}'.",
                        source, cluster_name, cluster_name
                    ));
                }
            } else {
                // Failed or unfinished setup can leave a cluster behind.
                notice(format!(
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

/// Collects the unique images of cases that need Kubernetes pods.
fn collect_kind_images(built: &[BuiltCase]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut images = BTreeSet::new();
    for BuiltCase { plan, runner } in built {
        if needs_pod(plan) {
            for (_, image) in runner.images() {
                images.insert(image);
            }
        }
    }
    images.into_iter().collect()
}

async fn list_tests(cmd: cli::ListCommand) -> ExitCode {
    if cmd.test_dirs.is_empty() {
        error!("{NO_TEST_DIRECTORIES}");
        return ExitCode::from(EXIT_HARNESS_ERROR);
    }
    let subject_os = match cmd.subject_os.resolve() {
        Ok(os) => os,
        Err(message) => {
            error!("{message}");
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };
    let mut planner = match load_planner(cmd.artifacts.as_deref()) {
        Ok(planner) => planner,
        Err(message) => {
            error!("{message}");
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };

    if !cmd.json {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        info!("Discovering test cases from: {}...", dirs_str.join(", "));
    }

    let Discovered { plans, excluded } = match discover_in_scope(&mut planner, &cmd.test_dirs, subject_os) {
        Ok(discovered) => discovered,
        Err(e) => {
            error!("Failed to discover tests: {:#}", e);
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };

    if cmd.json {
        let runners = match build_runners(&plans, subject_os, &RunnerArtifacts::Overrides(&[])) {
            Ok(runners) => runners,
            Err(e) => {
                error!("Failed to discover tests: {:#}", e);
                return ExitCode::from(EXIT_HARNESS_ERROR);
            }
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&listing(&runners)).expect("Unable to serialize a map of the tests")
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

/// Lists each case's suite and timeout, with the runtime and images its runner uses.
fn listing<P>(built: &[BuiltCase<P>]) -> BTreeMap<String, serde_json::Value>
where
    P: Borrow<LoadedPlan>,
{
    built
        .iter()
        .map(|BuiltCase { plan, runner }| {
            let case = plan.borrow().case();
            (
                case.name().to_string(),
                serde_json::json!({
                    "type": case.suite(),
                    "runtime": runner.runtime(),
                    "timeout": case.timeout(),
                    "images": runner.images(),
                }),
            )
        })
        .collect()
}

/// Revision of the repository this binary was built from, as recorded by the build script.
fn build_revision() -> String {
    revision(env!("VERGEN_GIT_SHA"), env!("VERGEN_GIT_DIRTY"))
}

#[cfg(test)]
mod tests {
    use panoramic_core::{artifact::ArtifactManifest, layout::Os, plan::Planner};

    use super::*;
    use crate::config::tests::{build_case_runners, planner, write_case, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS};

    async fn run_skipped_selection(requested: Option<&str>) {
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
        let expected = requested.map_or_else(|| vec!["correctness", "windows"], |name| vec![name]);
        assert_eq!(report["totals"]["skipped"], expected.len());
        assert_eq!(report["running"], serde_json::json!([]));
        assert_eq!(report["not_run"], serde_json::json!([]));
        assert_eq!(report["tests"].as_array().unwrap().len(), expected.len());
        for (test, name) in report["tests"].as_array().unwrap().iter().zip(&expected) {
            assert_eq!(test["name"], *name);
            assert_eq!(test["outcome"], "skipped");
            assert_eq!(test["skip_reason"], "subject_os excludes macos");
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
        run_skipped_selection(None).await;
    }

    #[tokio::test]
    async fn naming_a_skipped_case_selects_it_and_the_run_exits_3() {
        run_skipped_selection(Some("windows")).await;
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
        let (report, failed) = artifact_commands::verify_bindings(Os::MacOs, &bindings, dir.path()).await;
        assert_eq!(failed, 2, "{report}");
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
            assert!(diagnostic.contains("No artifacts needed on macos."), "{diagnostic}");
            assert!(!diagnostic.contains("failed verification"), "{diagnostic}");
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
            let listing = listing(&built);
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

    #[test]
    fn listing_json_ignores_artifact_bindings() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "integration",
            "type: integration\nname: integration\ntimeout: 1s\nruntimes: [linux, windows, mac]\nprocedure: []\n",
        );
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let manifest = dir.path().join("artifacts.toml");
        std::fs::write(&manifest, "schema = 1\n[roles]\nagent-adp-linux-image = 'changed:linux'\nagent-adp-windows-image = 'changed:windows'\ntools-linux-image = 'changed:tools'\n").unwrap();
        for os in [Os::Linux, Os::Windows, Os::MacOs] {
            let listings: Vec<_> = [None, Some(manifest.as_path())]
                .into_iter()
                .map(|manifest| {
                    let plans =
                        discover_in_scope(&mut load_planner(manifest).unwrap(), &[dir.path().to_path_buf()], os)
                            .unwrap()
                            .plans;
                    let built = build_runners(plans, os, &RunnerArtifacts::Overrides(&[])).unwrap();
                    serde_json::to_string_pretty(&listing(&built)).unwrap()
                })
                .collect();
            assert_eq!(listings[0], listings[1]);
            assert_eq!(listings[0].contains("base:file"), os == Os::Linux);
            assert!(!listings[0].contains("changed:"));
        }
    }

    #[tokio::test]
    async fn conflicting_runtime_and_subject_os_exit_2_for_run_list_and_list_json_before_discovery_or_logs() {
        use tracing::instrument::WithSubscriber as _;

        let dir = tempfile::tempdir().unwrap();
        let cases = dir.path().join("cases");
        write_case(
            &cases,
            "case",
            "type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, windows]\nprocedure: []\n",
        );
        write_case(&cases, "bad", "type: bogus\n");
        let logs = dir.path().join("logs");
        for (command, extra, use_tui) in [
            ("run", vec!["-t", "not-selected", "-l", logs.to_str().unwrap()], false),
            ("run", vec!["-t", "not-selected", "-l", logs.to_str().unwrap()], true),
            ("list", vec![], false),
            ("list", vec!["--json"], false),
        ] {
            let mut args = vec![
                "panoramic",
                command,
                "-d",
                cases.to_str().unwrap(),
                "--runtime",
                "linux",
                "--subject-os",
                "windows",
            ];
            args.extend(extra);
            let output = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
                .finish();
            let exit = async {
                match Cli::try_parse_from(args).unwrap().command {
                    Command::Run(cmd) => run_tests(cmd, use_tui).await,
                    Command::List(cmd) => list_tests(cmd).await,
                    Command::Artifacts(_) => panic!("expected run or list"),
                }
            }
            .with_subscriber(subscriber)
            .await;
            assert_eq!(exit, ExitCode::from(2));
            assert!(!logs.exists());
            if !use_tui {
                let diagnostic = std::fs::read_to_string(output.path()).unwrap();
                assert!(
                    diagnostic.contains(
                        "--runtime linux selects subject OS 'linux', which conflicts with --subject-os windows"
                    ),
                    "{diagnostic}"
                );
                assert!(!diagnostic.contains("Failed to discover"), "{diagnostic}");
            }
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
        assert_eq!(run_tests(cmd, false).await, ExitCode::from(2));
        let run_dir = std::fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
        let tests = report["tests"].as_array().unwrap();
        assert_eq!(tests.len(), 1);
        let result = &tests[0];
        assert_eq!(result["name"], "unprovidable");
        assert_eq!(result["outcome"], "errored");
        assert_eq!(result["error"]["kind"], "setup");
        assert!(result["error"]["message"].as_str().unwrap().starts_with(&format!(
            "realization {kind} on {} for participant 'adp' cannot be provided on this host: ",
            os.name()
        )));
        assert!(std::path::Path::new(result["log_dir"].as_str().unwrap())
            .join("result.json")
            .is_file());
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
        let built: Vec<_> = build_runners(plans, Os::Linux, &RunnerArtifacts::Overrides(&[]))
            .unwrap()
            .into_iter()
            .map(|BuiltCase { plan, runner }| BuiltCase {
                plan,
                runner: Box::new(DefaultRuntime(runner)),
            })
            .collect();
        let expected: std::collections::BTreeSet<_> = built
            .iter()
            .find(|built| built.plan.case().name() == "pod")
            .unwrap()
            .runner
            .images()
            .into_values()
            .collect();
        let images = collect_kind_images(&built);
        assert_eq!(images, expected.into_iter().collect::<Vec<_>>());
        assert!(images.contains(&"base:pod".to_string()));
        assert!(images.contains(&"comp:pod".to_string()));
        assert!(!images.contains(&"base:file".to_string()));
        assert!(!images.contains(&"comp:file".to_string()));
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

    #[test]
    fn log_level_scopes_the_selected_level_to_first_party_crates() {
        let directives = parse_filter_directives(&LogLevel::Debug.filter_directives())
            .expect("valid directives")
            .to_string();

        // Parsing reorders directives, so check membership rather than the whole string.
        assert!(
            directives.contains("panoramic=debug"),
            "directives were '{}'",
            directives
        );
        assert!(directives.contains("airlock=debug"), "directives were '{}'", directives);
        assert!(directives.contains("off"), "directives were '{}'", directives);
        assert!(!directives.contains("hyper"), "directives were '{}'", directives);
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
    async fn list_shows_a_participant_case_the_integration_runner_cannot_run_but_list_json_and_run_fail() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "participant",
            &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: linux-container}"),
        );
        write_case(
            dir.path(),
            "good",
            "type: integration\nname: good\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
        );
        let cases = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], Os::Linux)
            .unwrap()
            .plans;
        assert_eq!(
            cases.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
            ["good", "participant"]
        );
        for json in [false, true] {
            let mut args = vec![
                "panoramic",
                "list",
                "-d",
                dir.path().to_str().unwrap(),
                "--runtime",
                "linux",
            ];
            if json {
                args.push("--json");
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::List(cmd) = cli.command else {
                panic!("expected list");
            };
            let expected = if json {
                ExitCode::from(EXIT_HARNESS_ERROR)
            } else {
                ExitCode::SUCCESS
            };
            assert_eq!(list_tests(cmd).await, expected, "json: {json}");
        }
        let logs = dir.path().join("logs");
        for selection in [Some("good"), None] {
            let mut args = vec![
                "panoramic",
                "run",
                "-d",
                dir.path().to_str().unwrap(),
                "--runtime",
                "linux",
                "--no-tui",
                "-l",
                logs.to_str().unwrap(),
            ];
            if let Some(name) = selection {
                args.extend(["-t", name]);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Run(cmd) = cli.command else {
                panic!("expected run");
            };
            assert_eq!(run_tests(cmd, false).await, ExitCode::from(EXIT_HARNESS_ERROR));
            assert!(
                !logs.exists(),
                "runner construction must fail before creating the log directory"
            );
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

    #[tokio::test]
    async fn a_case_with_an_unsupported_realization_fails_list_list_json_and_run_with_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let pod = write_case(
            dir.path(),
            "b-pod",
            &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}"),
        );
        let windows = write_case(
            dir.path(),
            "a-windows",
            &PARTICIPANT_FIELDS
                .replace("[linux]", "[linux, windows]")
                .replace("subject: true", "subject: true, realization: {kind: linux-container}"),
        );
        let logs = dir.path().join("logs");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = discover_in_scope(&mut planner(), &[dir.path().to_path_buf()], os)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(
                message,
                format!(
                    "Failed to plan test cases:\n  - case 'a-windows' in {}: on subject OS 'windows', \
                     participant 'adp' of type 'agent-adp' cannot run as linux-container on windows; \
                     it supports linux-container on linux, host-process-group on macos, \
                     windows-container on windows\n  - case 'b-pod' in {}: on subject OS 'linux', \
                     participant 'adp' of type 'agent-adp' cannot run as kubernetes-pod on linux; \
                     it supports linux-container on linux, host-process-group on macos, windows-container on windows",
                    windows.canonicalize().unwrap().display(),
                    pod.canonicalize().unwrap().display()
                )
            );
            for (command, extra) in [
                ("list", vec![]),
                ("list", vec!["--json"]),
                ("run", vec!["--no-tui", "-l", logs.to_str().unwrap()]),
            ] {
                let mut args = vec![
                    "panoramic",
                    command,
                    "-d",
                    dir.path().to_str().unwrap(),
                    "--runtime",
                    runtime_for_os(os),
                ];
                args.extend(extra);
                let cli = Cli::try_parse_from(args).unwrap();
                let exit = match cli.command {
                    Command::Run(cmd) => run_tests(cmd, false).await,
                    Command::List(cmd) => list_tests(cmd).await,
                    Command::Artifacts(_) => panic!("expected run or list"),
                };
                assert_eq!(exit, ExitCode::from(2), "{command} under {os:?}");
                assert!(!logs.exists(), "layout errors must precede log directory creation");
            }
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
            "  participant\n    Timeout: 1s\n    Status: runnable\n    \
             Layout: adp on macos as host-process-group; alpha on linux as linux-container; \
             zebra on macos as host-process-group\n    \
             Artifact roles:\n      \
             adp-host-binary = target/release/agent-data-plane (local default)\n      \
             agent-adp-linux-image = saluki-images/datadog-agent:testing-devel (local default)\n      \
             agent-host-install = /tmp/saluki-dda/datadog-agent (local default)\n      \
             fixup-image = registry.example/alpine:3 (manifest)\n    \
             Create order: adp, alpha, zebra\n    \
             Start order: adp, alpha, zebra\n\n"
        );
        for (yaml, expected) in [
            (
                "type: integration\nname: typed\ndescription: A typed case\n\
                 timeout: 7s\nruntimes: [linux]\nprocedure: []\n"
                    .to_string(),
                "  typed\n    A typed case\n    Timeout: 7s\n    Status: runnable\n    Artifact roles:\n      \
                 agent-adp-linux-image = saluki-images/datadog-agent:testing-devel (local default)\n      \
                 fixup-image = alpine:latest (local default)\n\n",
            ),
            (
                format!("type: correctness\n{CORRECTNESS_FIELDS}"),
                "  typed\n    Timeout: 1200s\n    Status: runnable\n    Artifact roles:\n      \
                 agent-adp-linux-image = saluki-images/datadog-agent:testing-devel (local default)\n      \
                 fixup-image = alpine:latest (local default)\n      \
                 tools-linux-image = saluki-images/correctness-tools:latest (local default)\n\n",
            ),
        ] {
            let case = crate::CASE_LOADER
                .load_content(&yaml, "typed", std::path::Path::new("config.yaml"))
                .unwrap()
                .remove(0);
            let plan = planner().compile(case, Os::Linux).unwrap().into_plan().unwrap();
            assert_eq!(listing_text(&plan), expected);
        }
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
            "type: integration\nname: integration\ndescription: Windows only\n\
             timeout: 7s\nruntimes: [windows]\nprocedure: []\n",
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
        let entries = listing(&runners);
        assert_eq!(entries.len(), 1);
        let entry = &entries["correctness"];
        assert_eq!(entry["type"], "correctness");
        assert_eq!(entry["runtime"], "docker");
        assert_eq!(entry["timeout"], serde_json::json!({"secs": 1200, "nanos": 0}));
        assert_eq!(entry["images"]["baseline"], "base:file");

        let path = write_case(
            dir.path(),
            "integration",
            "type: integration\nname: integration\ntimeout: 7s\nruntimes: [windows]\nprocedure: []\n",
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let runners = build_case_runners(cases, Os::Windows, &[]).unwrap();
        let entries = listing(&runners);
        assert_eq!(entries.len(), 1);
        let entry = &entries["integration"];
        assert_eq!(entry["type"], "integration");
        assert_eq!(entry["runtime"], "windows");
        assert_eq!(entry["timeout"], serde_json::json!({"secs": 7, "nanos": 0}));
        assert_eq!(
            entry["images"]["container"],
            "saluki-images/agent-data-plane:testing-windows"
        );

        let path = write_case(dir.path(), "participant", &PARTICIPANT_FIELDS.replace("1s", "9s"));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let runners = build_case_runners(cases, Os::Linux, &[]).unwrap();
        let entries = listing(&runners);
        assert_eq!(entries.len(), 1);
        let entry = &entries["participant"];
        assert_eq!(entry["type"], "integration");
        assert_eq!(entry["runtime"], "linux");
        assert_eq!(entry["timeout"], serde_json::json!({"secs": 9, "nanos": 0}));
        assert_eq!(
            entry["images"]["container"],
            "saluki-images/datadog-agent:testing-devel"
        );
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
