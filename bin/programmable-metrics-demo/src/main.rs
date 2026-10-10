//! A demo of programmable metric processing: boots ADP on the default program, serves an editor to change that program
//! live through a stub of the artifact channel, and shows what reaches each endpoint.
//!
//! `programmable-metrics-demo run --adp-bin <path>` (or `make demo-programmable-metrics`) does the following, in one
//! process:
//!
//! 1. **Launcher**: creates a run directory, generates a self-signed IPC certificate, writes an ADP configuration
//!    (standalone mode, UDP DogStatsD, a primary endpoint, two additional endpoints routed by metric prefix, and a
//!    multi-region failover endpoint, all pointing at the demo), starts ADP, and waits for it to report ready.
//! 2. **Recording intakes**: one loopback HTTP listener per endpoint, decoding and recording every metric payload.
//! 3. **Traffic generator**: DogStatsD over UDP at a steady rate, including a canary counter for conservation checks.
//! 4. **Telemetry scraper**: reads ADP's `/metrics` every second and keeps rates of the series the UI shows.
//! 5. **Artifact channel stub**: the server ADP polls for policy artifacts, with a durable store in the run directory.
//! 6. **Control surface client**: ADP's privileged programmable-metrics routes, over mTLS with the IPC certificate.
//! 7. **Compile and publish API** and the **UI**: an editor with diagnostics, and the telemetry viewer.
//!
//! Open the URL it prints. Press Ctrl-C to stop the demo and ADP, and Ctrl-C again to kill ADP at once instead of
//! waiting for it to exit. `SIGTERM` and `SIGHUP` (closing the terminal) stop both too, at any point of the run. ADP
//! runs in its own process group, so only the demo can stop it: if the demo is killed with `SIGKILL` or aborts, ADP
//! keeps running, and the next run reports the ports it holds.
//!
//! `programmable-metrics-demo check --adp-bin <path>` (or `make demo-programmable-metrics-check`) runs the same
//! components headless on other ports, drives the demo's scenarios S0 to S6 through the demo server's API, checks
//! what each endpoint receives, and stops everything: see [`scenario`].
//!
//! The demo is a development tool: it listens on loopback only, writes everything to the run directory, and is not
//! part of any release. ADP implements its side of the artifact channel and control surface from stage 5 of the
//! programmable metrics implementation; with an earlier ADP the demo still runs, and says that the control surface is
//! not exposed.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use argh::FromArgs;
use saluki_common::logging::filter_from_env;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{net::TcpListener, task::JoinSet};
use tracing::{error, info, warn};
use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

mod adp_api;
mod app;
mod canary;
mod checks;
mod clock;
mod compile;
mod control;
mod export;
mod intake;
mod launcher;
mod preview;
mod scenario;
mod settings;
mod stub;
mod telemetry;
mod traffic;
mod ui;

use crate::{
    adp_api::AdpApi,
    app::{App, Components},
    control::ControlSurface,
    intake::IntakeRecorder,
    launcher::{AdpProcess, Launch, Phase},
    settings::{DemoPorts, RunSettings},
    stub::{store::ArtifactStore, StubChannel},
    telemetry::TelemetryScraper,
    traffic::{TrafficChange, TrafficGenerator},
};

/// A demo of programmable metric processing in ADP.
#[derive(FromArgs)]
struct Cli {
    #[argh(subcommand)]
    command: CliCommand,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum CliCommand {
    Run(RunCommand),
    Check(CheckCommand),
}

/// Run the demo's scenarios S0 to S6 headless against ADP, and check what each endpoint receives.
///
/// Launches ADP and the demo on the default ports moved up by --port-offset, drives the scenarios through the demo
/// server's API (compile, publish, revoke), checks every step's acknowledgment, the families each intake receives, and
/// canary conservation across every install, then stops everything. Exits 1 if any check fails; the report, with
/// timings, is written to check-report.json in the run directory.
#[derive(FromArgs)]
#[argh(subcommand, name = "check")]
struct CheckCommand {
    /// path to the agent-data-plane binary
    #[argh(option)]
    adp_bin: PathBuf,

    /// run directory (default: a new target/programmable-metrics-demo/check-(unix seconds))
    #[argh(option)]
    run_dir: Option<PathBuf>,

    /// added to every default port, so that the check can run beside a demo on the default ports (default 10000:
    /// the UI on 27080, ADP's DogStatsD on 27125)
    #[argh(option, default = "10000")]
    port_offset: u16,

    /// rate of DogStatsD datagrams, per second (default 20)
    #[argh(option, default = "traffic::DEFAULT_RATE")]
    traffic_rate: f64,
}

/// Boot ADP on the default program and serve the editor and telemetry viewer.
#[derive(FromArgs)]
#[argh(subcommand, name = "run")]
struct RunCommand {
    /// path to the agent-data-plane binary (required unless --no-launch)
    #[argh(option)]
    adp_bin: Option<PathBuf>,

    /// run directory for the certificate, configuration, logs, and artifact store (default: a new
    /// target/programmable-metrics-demo/run-(unix seconds)); reuse one to keep the published artifact
    #[argh(option)]
    run_dir: Option<PathBuf>,

    /// do not start ADP: write the certificate and configuration and serve the demo for an ADP started separately
    #[argh(switch)]
    no_launch: bool,

    /// port of the demo UI (default 17080)
    #[argh(option, default = "DemoPorts::default().ui")]
    ui_port: u16,

    /// port of the artifact channel stub (default 17100)
    #[argh(option, default = "DemoPorts::default().stub")]
    stub_port: u16,

    /// port of the primary endpoint's intake (default 17101)
    #[argh(option, default = "DemoPorts::default().primary")]
    primary_port: u16,

    /// port of team B's endpoint's intake (default 17102)
    #[argh(option, default = "DemoPorts::default().team_b")]
    team_b_port: u16,

    /// port of team C's endpoint's intake (default 17103)
    #[argh(option, default = "DemoPorts::default().team_c")]
    team_c_port: u16,

    /// port of the multi-region failover endpoint's intake (default 17104)
    #[argh(option, default = "DemoPorts::default().mrf")]
    mrf_port: u16,

    /// port of ADP's unprivileged API (default 17110)
    #[argh(option, default = "DemoPorts::default().api")]
    api_port: u16,

    /// port of ADP's privileged API (default 17111)
    #[argh(option, default = "DemoPorts::default().secure_api")]
    secure_api_port: u16,

    /// port of ADP's DogStatsD UDP listener (default 17125)
    #[argh(option, default = "DemoPorts::default().dogstatsd")]
    dogstatsd_port: u16,

    /// how often ADP polls the artifact channel stub, in milliseconds (default 1000)
    #[argh(option, default = "1000")]
    poll_interval_ms: u64,

    /// ADP's aggregation window, in seconds (default 1, so that changes show within seconds)
    #[argh(option, default = "1")]
    aggregate_window_secs: u64,

    /// how long ADP keeps sending zeros for an idle counter, in seconds (default 0, so that a counter that is
    /// paused or routed away leaves the intake columns within seconds; ADP's own default is 300)
    #[argh(option, default = "0")]
    counter_expiry_secs: u64,

    /// rate of DogStatsD datagrams, per second (default 20)
    #[argh(option, default = "traffic::DEFAULT_RATE")]
    traffic_rate: f64,

    /// start with the traffic generator paused
    #[argh(switch)]
    traffic_paused: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .compact()
        .with_ansi(true)
        .with_target(false)
        .finish()
        .with(filter_from_env(Targets::new().with_default(LevelFilter::INFO)))
        .init();

    let cli: Cli = argh::from_env();
    let result = match cli.command {
        CliCommand::Run(command) => run(command).await,
        CliCommand::Check(command) => check(command).await,
    };
    if let Err(e) = result {
        error!("{:#}", e);
        std::process::exit(1);
    }
}

async fn bind(port: u16, what: &str) -> Result<TcpListener, GenericError> {
    TcpListener::bind(DemoPorts::loopback(port))
        .await
        .with_error_context(|| {
            format!(
                "Failed to listen on 127.0.0.1:{} for the {}. Is another demo running? Choose another port with the \
             command's port options.",
                port, what
            )
        })
}

fn serve(tasks: &mut JoinSet<()>, listener: TcpListener, router: axum::Router, what: &'static str) {
    tasks.spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            error!(%error, "The {} stopped.", what);
        }
    });
}

async fn run(command: RunCommand) -> Result<(), GenericError> {
    // Installed before anything else, so that every stop signal from here on goes through the shutdown below.
    let mut signals = ShutdownSignals::install()?;
    saluki_tls::initialize_default_crypto_provider()?;

    let adp_bin = match (&command.adp_bin, command.no_launch) {
        (Some(path), _) => {
            if !command.no_launch && !path.is_file() {
                return Err(missing_adp_bin(path));
            }
            std::path::absolute(path).unwrap_or_else(|_| path.clone())
        }
        (None, true) => PathBuf::from("agent-data-plane"),
        (None, false) => {
            return Err(generic_error!(
                "Pass --adp-bin <path to agent-data-plane>, or --no-launch to run the demo for an ADP you start \
                 yourself."
            ))
        }
    };

    let run_dir = command
        .run_dir
        .clone()
        .unwrap_or_else(|| launcher::default_run_dir("run"));
    let settings = RunSettings {
        ports: DemoPorts {
            ui: command.ui_port,
            stub: command.stub_port,
            primary: command.primary_port,
            team_b: command.team_b_port,
            team_c: command.team_c_port,
            mrf: command.mrf_port,
            api: command.api_port,
            secure_api: command.secure_api_port,
            dogstatsd: command.dogstatsd_port,
        },
        run_dir: std::path::absolute(&run_dir).unwrap_or(run_dir),
        poll_interval: Duration::from_millis(command.poll_interval_ms.max(1)),
        aggregate_window_secs: command.aggregate_window_secs.max(1),
        counter_expiry_secs: command.counter_expiry_secs,
        traffic_rate: command.traffic_rate,
        traffic_paused: command.traffic_paused,
    };
    let launch = if command.no_launch {
        None
    } else {
        Some(adp_bin.as_path())
    };
    let Some(demo) = Demo::start(settings, &adp_bin, launch, &mut signals).await? else {
        return Ok(());
    };
    if let Err(error) = &demo.ready {
        warn!("{:#}", error);
    }
    info!(
        "The demo is running: open {} (press Ctrl-C to stop).",
        DemoPorts::http_url(demo.settings.ports.ui)
    );

    match &demo.adp {
        Some(process) => {
            tokio::select! {
                signal = signals.next() => info!("Received {}.", signal),
                exit = process.exited() => {
                    warn!("ADP {}. The demo keeps running so you can inspect it; press Ctrl-C to stop.", exit);
                    info!("Received {}.", signals.next().await);
                }
            }
        }
        None => info!("Received {}.", signals.next().await),
    }

    demo.stop(&mut signals).await;
    Ok(())
}

async fn check(command: CheckCommand) -> Result<(), GenericError> {
    let mut signals = ShutdownSignals::install()?;
    saluki_tls::initialize_default_crypto_provider()?;
    if !command.adp_bin.is_file() {
        return Err(missing_adp_bin(&command.adp_bin));
    }
    let adp_bin = std::path::absolute(&command.adp_bin).unwrap_or_else(|_| command.adp_bin.clone());
    let ports = DemoPorts::offset_from_default(command.port_offset).ok_or_else(|| {
        generic_error!(
            "--port-offset {} moves a port past 65535: the default ports are 17080 to 17125.",
            command.port_offset
        )
    })?;
    let run_dir = command
        .run_dir
        .clone()
        .unwrap_or_else(|| launcher::default_run_dir("check"));
    let settings = RunSettings {
        ports,
        run_dir: std::path::absolute(&run_dir).unwrap_or(run_dir),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: command.traffic_rate,
        traffic_paused: false,
    };
    let report_path = settings.run_dir.join("check-report.json");
    let Some(demo) = Demo::start(settings, &adp_bin, Some(&adp_bin), &mut signals).await? else {
        return Err(generic_error!("Interrupted while waiting for ADP to report ready."));
    };

    let outcome = match &demo.ready {
        Err(error) => Err(generic_error!("{:#}", error)),
        Ok(()) => {
            let process = demo.adp.as_ref().expect("the check launches ADP");
            let scenarios = scenario::run(&demo.settings);
            tokio::select! {
                report = scenarios => Ok(report),
                signal = signals.next() => Err(generic_error!("Interrupted by {}.", signal)),
                exit = process.exited() => Err(generic_error!(
                    "ADP {} during the check; its output is in '{}'.",
                    exit,
                    demo.settings.adp_console_log_path().display()
                )),
            }
        }
    };
    let run_dir = demo.settings.run_dir.clone();
    demo.stop(&mut signals).await;

    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            return Err(generic_error!(
                "The programmable metrics demo check failed: {:#}\nThe run directory, with ADP's logs and the \
                 artifact store, is '{}'.",
                error,
                run_dir.display()
            ))
        }
    };
    match serde_json::to_vec_pretty(&report) {
        Ok(json) => {
            if let Err(error) = std::fs::write(&report_path, json) {
                warn!(path = %report_path.display(), %error, "Failed to write the check report.");
            }
        }
        Err(error) => warn!(%error, "Failed to write the check report."),
    }
    for line in report.summary_lines() {
        info!("{}", line);
    }
    match report.failure() {
        Some(failure) => Err(generic_error!(
            "The programmable metrics demo check failed: {}\nThe report is '{}'.",
            failure,
            report_path.display()
        )),
        None => {
            info!(report = %report_path.display(), "The programmable metrics demo check passed.");
            Ok(())
        }
    }
}

fn missing_adp_bin(path: &std::path::Path) -> GenericError {
    generic_error!(
        "The ADP binary '{}' does not exist. Build it with `make build-adp`, or pass --no-launch.",
        path.display()
    )
}

/// The demo's components, running.
struct Demo {
    settings: Arc<RunSettings>,
    tasks: JoinSet<()>,
    adp: Option<AdpProcess>,
    /// Whether ADP reported ready, when the demo launched it.
    ready: Result<(), GenericError>,
}

impl Demo {
    /// Prepares the run directory, starts the demo's components, and, if `launch` names the ADP binary, starts ADP and
    /// waits for it to report ready, and then starts the traffic unless the settings start it paused.
    ///
    /// `adp_bin` is the binary named in the command line shown for an ADP started separately. Returns `None` if a stop
    /// signal arrived while waiting for ADP, which is then stopped with everything else.
    async fn start(
        settings: RunSettings, adp_bin: &std::path::Path, launch: Option<&std::path::Path>,
        signals: &mut ShutdownSignals,
    ) -> Result<Option<Self>, GenericError> {
        let settings = Arc::new(settings);
        launcher::prepare_run_dir(&settings)?;
        info!(run_dir = %settings.run_dir.display(), "Prepared the run directory.");

        let mut tasks = JoinSet::new();

        let channel = StubChannel::new(ArtifactStore::open(&settings.store_dir())?);
        let stub_listener = bind(settings.ports.stub, "artifact channel stub").await?;
        serve(
            &mut tasks,
            stub_listener,
            stub::router(channel.clone()),
            "artifact channel stub",
        );

        let mut intakes = Vec::new();
        for recipient in settings.ports.recipients() {
            let recorder = IntakeRecorder::new(recipient);
            let listener = bind(recipient.port, "recording intake").await?;
            serve(
                &mut tasks,
                listener,
                intake::router(recorder.clone()),
                "recording intake",
            );
            intakes.push(recorder);
        }

        let api = AdpApi::new(settings.ports.api, settings.ports.secure_api, &settings.ipc_cert_path()).await?;

        let telemetry = TelemetryScraper::new();
        tasks.spawn(telemetry.clone().run(api.clone()));
        let control = ControlSurface::new();
        tasks.spawn(control.clone().run(api.clone()));

        let dogstatsd: SocketAddr = DemoPorts::loopback(settings.ports.dogstatsd);
        let start_traffic_now = launch.is_none() && !settings.traffic_paused;
        let traffic = TrafficGenerator::new(dogstatsd, settings.traffic_rate, start_traffic_now);
        tasks.spawn(traffic.clone().run());

        let phase = if launch.is_some() {
            Phase::Starting
        } else {
            Phase::External
        };
        let record = Launch::new(phase, adp_bin, &settings);
        let app = App::new(Components {
            settings: Arc::clone(&settings),
            intakes,
            traffic: traffic.clone(),
            telemetry,
            channel,
            control,
            launch: record.clone(),
            api: api.clone(),
        });
        let ui_listener = bind(settings.ports.ui, "demo UI").await?;
        serve(&mut tasks, ui_listener, app::router(app), "demo UI");

        let Some(adp_bin) = launch else {
            info!(
                "Not launching ADP. Start it with: {}",
                launcher::adp_command_line(adp_bin, &settings)
            );
            return Ok(Some(Self {
                settings,
                tasks,
                adp: None,
                ready: Ok(()),
            }));
        };
        launcher::check_adp_ports(&settings.ports)?;
        let process = AdpProcess::spawn(adp_bin, &settings, &record)?;
        info!(log = %settings.adp_log_path().display(), "Started ADP; waiting for it to report ready...");
        let console_log = settings.adp_console_log_path();
        let ready = tokio::select! {
            ready = process.wait_ready(&api, &record, &console_log) => ready,
            signal = signals.next() => {
                info!("Received {} while waiting for ADP to report ready.", signal);
                let demo = Self { settings, tasks, adp: Some(process), ready: Ok(()) };
                demo.stop(signals).await;
                return Ok(None);
            }
        };
        if ready.is_ok() {
            info!("ADP is ready.");
            if !settings.traffic_paused {
                traffic.apply(TrafficChange {
                    running: Some(true),
                    rate: None,
                });
            }
        }
        Ok(Some(Self {
            settings,
            tasks,
            adp: Some(process),
            ready,
        }))
    }

    /// Stops ADP, if the demo launched it, and then the demo's own tasks. Another stop signal meanwhile kills ADP at
    /// once.
    async fn stop(mut self, signals: &mut ShutdownSignals) {
        info!("Stopping...");
        if let Some(process) = self.adp.take() {
            info!("Stopping ADP (press Ctrl-C again to kill it at once)...");
            process.shutdown(signals.next()).await;
        }
        self.tasks.shutdown().await;
        info!("Stopped.");
    }
}

/// The signals that stop the demo: Ctrl-C (`SIGINT`), `SIGTERM`, and `SIGHUP` on Unix; Ctrl-C elsewhere.
///
/// Installing them replaces their default action, which would end the demo without stopping ADP, so they are installed
/// before ADP is started and stay installed until the demo exits; every one of them, received at any point, stops ADP.
struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    #[cfg(unix)]
    fn install() -> Result<Self, GenericError> {
        use tokio::signal::unix::{signal, SignalKind};
        let install = |kind: SignalKind, name: &str| {
            signal(kind).with_error_context(|| format!("Failed to install a handler for {}.", name))
        };
        Ok(Self {
            interrupt: install(SignalKind::interrupt(), "SIGINT")?,
            terminate: install(SignalKind::terminate(), "SIGTERM")?,
            hangup: install(SignalKind::hangup(), "SIGHUP")?,
        })
    }

    #[cfg(not(unix))]
    fn install() -> Result<Self, GenericError> {
        Ok(Self {})
    }

    /// Waits for the next stop signal, and returns its name.
    #[cfg(unix)]
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "Ctrl-C (SIGINT)",
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.hangup.recv() => "SIGHUP",
        }
    }

    /// Waits for the next stop signal, and returns its name.
    #[cfg(not(unix))]
    async fn next(&mut self) -> &'static str {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}
