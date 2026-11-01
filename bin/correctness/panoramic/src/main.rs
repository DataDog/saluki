//! Test runner for running integration tests.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{borrow::Borrow, collections::BTreeMap, fmt::Write as _};
use std::{io::IsTerminal, process::ExitCode};

use clap::Parser as _;
use panoramic_core::case::{CaseLoader, Registries};
use panoramic_core::events::create_event_channel;
use panoramic_core::interrupts::Interrupts;
use panoramic_core::machine_output::{revision, RunReport};
use saluki_common::logging::{filter_from_env, parse_filter_directives};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::runner::Runner;

mod kind;
use self::kind::KindLifecycle;

mod actions;
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
    build_runners, discover_in_scope, runtime_for_os,
    typed_case::{needs_pod, LoadedPlan, TypedConfigLoader},
    BuiltCase,
};

mod image_override;
mod intake;

#[cfg(test)]
mod case_loader_tests;
#[cfg(test)]
mod crate_dependencies_tests;
#[cfg(test)]
mod source_boundary_tests;

/// Participant types and steps available to loading and compilation.
pub(crate) const REGISTRIES: Registries<'static> = Registries {
    participant_types: panoramic_participants::REGISTERED_TYPES,
    steps: panoramic_steps::REGISTERED_STEPS,
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
        Command::List(_) => (false, false),
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

async fn run_tests(cmd: cli::RunCommand, use_tui: bool) -> ExitCode {
    let settings = cmd.runner_settings();
    debug!(
        adp_binary_path = %settings.adp_binary_path.display(),
        core_agent_binary_path = %settings.core_agent_binary_path.display(),
        mounts_dir = %settings.mounts_dir.display(),
        alpine_image = %settings.alpine_image,
        "Resolved runner settings."
    );

    if cmd.test_dirs.is_empty() {
        let msg = "No test directories specified. Use -d <path> to specify one or more directories.";
        if use_tui {
            eprintln!("{}", msg);
        } else {
            error!("{}", msg);
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
    let integration_runtime = runtime_for_os(subject_os);
    let mut built = match discover_in_scope(&cmd.test_dirs, subject_os)
        .and_then(|plans| build_runners(plans, subject_os, &cmd.image_overrides))
    {
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

    if built.is_empty() {
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
            let names: Vec<_> = built.iter().map(|built| built.plan.case().name()).collect();
            let (selected, unmatched) = resolve_selection(requested, &names);
            if !unmatched.is_empty() {
                let msg = format!(
                    "No test matches: {}. Run 'panoramic list' to see the tests in scope for runtime '{}'.",
                    unmatched.join(", "),
                    integration_runtime
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
    built.retain(|built| {
        selected_tests
            .as_ref()
            .is_none_or(|names| names.iter().any(|name| name == built.plan.case().name()))
    });

    if use_tui {
        eprintln!("Checking that this host can provide the selected cases...");
    } else {
        info!("Checking that this host can provide the selected cases...");
    }
    providability::check(&mut built).await;

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
            cmd.parallelism.get(),
            log_dir.clone(),
            built.iter().map(|built| built.plan.case().name().to_string()).collect(),
        ),
        cmd.fail_fast,
    );

    // Create the event channel early so the kind setup task can emit status messages.
    let (tx, rx) = create_event_channel();

    // Spawn kind cluster setup in the background so non-kind tests start immediately.
    // Kind tests will wait on `kind_rx` before doing any work.
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

    // Inject runtime config and build the test registry.
    let mut registry = Runner::new(log_dir.clone(), settings);
    if let Some(ref rx) = kind_rx {
        registry = registry.with_kind_ready(rx.clone());
    }
    for built in built {
        registry.register(built).expect("failure to register test");
    }

    // The consume loop cancels this on the first interrupt.
    let cancel_all = CancellationToken::new();

    // Build run args.
    let args = runner::RunArgs::new(cancel_all.clone(), tx)
        .with_parallelism(cmd.parallelism)
        .with_fail_fast(cmd.fail_fast);

    // Spawn the test runner task (same code path for both modes).
    let runner_handle = tokio::spawn(async move { registry.run_tests(args).await });

    let mut display_error = None;
    let display = if use_tui {
        match tui::Tui::new(&log_dir) {
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
            Some(log_dir.clone()),
        )))
    };

    let consumed = consume_run(rx, runner_handle, &mut collector, display, &mut interrupts, &cancel_all).await;
    let display_error = display_error.or(consumed.display_error);
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
                } else if let Some(source) = run_teardown(lifecycle.teardown(), &mut collector, &mut interrupts).await {
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
        error!("No test directories specified. Use -d <path> to specify one or more directories.");
        return ExitCode::from(2);
    }

    let subject_os = match cmd.subject_os.resolve() {
        Ok(os) => os,
        Err(message) => {
            error!("{message}");
            return ExitCode::from(2);
        }
    };

    if !cmd.json {
        let dirs_str: Vec<_> = cmd.test_dirs.iter().map(|d| d.display().to_string()).collect();
        info!("Discovering test cases from: {}...", dirs_str.join(", "));
    }

    let plans = match discover_in_scope(&cmd.test_dirs, subject_os) {
        Ok(plans) => plans,
        Err(e) => {
            error!("Failed to discover tests: {:#}", e);
            return ExitCode::from(EXIT_HARNESS_ERROR);
        }
    };

    if cmd.json {
        let runners = match build_runners(&plans, subject_os, &[]) {
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
        if plans.is_empty() {
            info!("No test cases found.");
            return ExitCode::SUCCESS;
        }

        info!("Discovered {} test case(s).", plans.len());

        println!();
        println!("Available tests ({}):", plans.len());
        println!();

        for plan in &plans {
            print!("{}", listing_text(plan));
        }
    }
    ExitCode::SUCCESS
}

/// Formats one case's entry in the text listing, with its layout if it has one.
fn listing_text(plan: &LoadedPlan) -> String {
    let case = plan.case();
    let mut text = format!("  {}\n", case.name());
    if let Some(description) = case.description() {
        writeln!(text, "    {description}").unwrap();
    }
    writeln!(text, "    Timeout: {:?}", case.timeout()).unwrap();
    if let Some(layout) = plan.layout() {
        let participants: Vec<_> = layout
            .iter()
            .map(|(name, realization)| format!("{name} on {} as {}", realization.os.name(), realization.kind.name()))
            .collect();
        writeln!(text, "    Layout: {}", participants.join("; ")).unwrap();
    }
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
    use panoramic_core::layout::Os;

    use super::*;
    use crate::config::{
        tests::{build_case_runners, write_case, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
        DEFAULT_LINUX_TARGET_IMAGE, DEFAULT_WINDOWS_TARGET_IMAGE,
    };

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
        let plans = discover_in_scope(&[dir.path().to_path_buf()], Os::Linux).unwrap();
        let built: Vec<_> = build_runners(plans, Os::Linux, &[])
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
        let error = discover_in_scope(&[dir.path().to_path_buf()], Os::Linux)
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
        let cases = discover_in_scope(&[dir.path().to_path_buf()], Os::Linux).unwrap();
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
        let cases = discover_in_scope(&[dir.path().to_path_buf()], Os::Linux).unwrap();
        let names: Vec<_> = cases.iter().map(|plan| plan.case().name()).collect();
        let (selected, unmatched) = resolve_selection("matrix/first, missing, integration", &names);
        assert_eq!(selected, ["matrix/first", "integration"]);
        assert_eq!(unmatched, ["missing"]);
    }

    #[tokio::test]
    async fn selection_rejects_disabled_cases_and_cases_for_another_runtime() {
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
        for name in ["linux-only", "disabled"] {
            let cli = Cli::try_parse_from([
                "panoramic",
                "run",
                "-d",
                dir.path().to_str().unwrap(),
                "--runtime",
                "windows",
                "-t",
                name,
            ])
            .unwrap();
            let Command::Run(cmd) = cli.command else {
                panic!("expected run")
            };
            assert_eq!(
                run_tests(cmd, false).await,
                ExitCode::from(EXIT_NO_TESTS_SELECTED),
                "{name}"
            );
        }
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
            let message = discover_in_scope(&[dir.path().to_path_buf()], os)
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
                };
                assert_eq!(exit, ExitCode::from(2), "{command} under {os:?}");
                assert!(!logs.exists(), "layout errors must precede log directory creation");
            }
        }
    }

    #[test]
    fn listing_text_prints_each_participant_layout_and_nothing_for_typed_cases() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "  adp:",
            "  zebra: {type: agent-adp}\n  alpha: {type: agent-adp, os: linux}\n  adp:",
        );
        let case = crate::CASE_LOADER
            .load_content(&yaml, "participant", std::path::Path::new("config.yaml"))
            .unwrap()
            .remove(0);
        let plan = LoadedPlan::compile(case, Os::MacOs, &REGISTRIES).unwrap().unwrap();
        assert_eq!(
            listing_text(&plan),
            "  participant\n    Timeout: 1s\n    \
             Layout: adp on macos as host-process-group; alpha on linux as linux-container; \
             zebra on macos as host-process-group\n\n"
        );
        for (yaml, expected) in [
            (
                "type: integration\nname: typed\ndescription: A typed case\n\
                 timeout: 7s\nruntimes: [linux]\nprocedure: []\n"
                    .to_string(),
                "  typed\n    A typed case\n    Timeout: 7s\n\n",
            ),
            (
                format!("type: correctness\n{CORRECTNESS_FIELDS}"),
                "  typed\n    Timeout: 1200s\n\n",
            ),
        ] {
            let case = crate::CASE_LOADER
                .load_content(&yaml, "typed", std::path::Path::new("config.yaml"))
                .unwrap()
                .remove(0);
            let plan = LoadedPlan::compile(case, Os::Linux, &REGISTRIES).unwrap().unwrap();
            assert_eq!(listing_text(&plan), expected);
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
        assert_eq!(entry["images"]["container"], DEFAULT_WINDOWS_TARGET_IMAGE);

        let path = write_case(dir.path(), "participant", &PARTICIPANT_FIELDS.replace("1s", "9s"));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        let runners = build_case_runners(cases, Os::Linux, &[]).unwrap();
        let entries = listing(&runners);
        assert_eq!(entries.len(), 1);
        let entry = &entries["participant"];
        assert_eq!(entry["type"], "integration");
        assert_eq!(entry["runtime"], "linux");
        assert_eq!(entry["timeout"], serde_json::json!({"secs": 9, "nanos": 0}));
        assert_eq!(entry["images"]["container"], DEFAULT_LINUX_TARGET_IMAGE);
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
