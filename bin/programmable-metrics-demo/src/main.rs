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
    let CliCommand::Run(command) = cli.command;
    if let Err(e) = run(command).await {
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
                return Err(generic_error!(
                    "The ADP binary '{}' does not exist. Build it with `make build-adp`, or pass --no-launch.",
                    path.display()
                ));
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

    let run_dir = command.run_dir.clone().unwrap_or_else(launcher::default_run_dir);
    let run_dir = std::path::absolute(&run_dir).unwrap_or(run_dir);
    let settings = Arc::new(RunSettings {
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
        run_dir,
        poll_interval: Duration::from_millis(command.poll_interval_ms.max(1)),
        aggregate_window_secs: command.aggregate_window_secs.max(1),
        counter_expiry_secs: command.counter_expiry_secs,
        traffic_rate: command.traffic_rate,
        traffic_paused: command.traffic_paused,
    });
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
    let start_traffic_now = command.no_launch && !settings.traffic_paused;
    let traffic = TrafficGenerator::new(dogstatsd, settings.traffic_rate, start_traffic_now);
    tasks.spawn(traffic.clone().run());

    let launch = Launch::new(
        if command.no_launch {
            Phase::External
        } else {
            Phase::Starting
        },
        &adp_bin,
        &settings,
    );
    let app = App::new(Components {
        settings: Arc::clone(&settings),
        intakes,
        traffic: traffic.clone(),
        telemetry,
        channel,
        control,
        launch: launch.clone(),
        api: api.clone(),
    });
    let ui_listener = bind(settings.ports.ui, "demo UI").await?;
    serve(&mut tasks, ui_listener, app::router(app), "demo UI");
    let ui_url = DemoPorts::http_url(settings.ports.ui);

    let adp = if command.no_launch {
        info!(
            "Not launching ADP. Start it with: {}",
            launcher::adp_command_line(&adp_bin, &settings)
        );
        None
    } else {
        launcher::check_adp_ports(&settings.ports)?;
        let process = AdpProcess::spawn(&adp_bin, &settings, &launch)?;
        info!(log = %settings.adp_log_path().display(), "Started ADP; waiting for it to report ready...");
        let console_log = settings.adp_console_log_path();
        let ready = tokio::select! {
            ready = process.wait_ready(&api, &launch, &console_log) => Some(ready),
            signal = signals.next() => {
                info!("Received {} while waiting for ADP to report ready.", signal);
                None
            }
        };
        match ready {
            Some(Ok(())) => {
                info!("ADP is ready.");
                if !settings.traffic_paused {
                    traffic.apply(TrafficChange {
                        running: Some(true),
                        rate: None,
                    });
                }
            }
            Some(Err(error)) => warn!("{:#}", error),
            None => {
                stop(Some(process), &mut signals, tasks).await;
                return Ok(());
            }
        }
        Some(process)
    };

    info!("The demo is running: open {} (press Ctrl-C to stop).", ui_url);

    match &adp {
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

    stop(adp, &mut signals, tasks).await;
    Ok(())
}

/// Stops ADP, if the demo launched it, and then the demo's own tasks. Another stop signal meanwhile kills ADP at once.
async fn stop(adp: Option<AdpProcess>, signals: &mut ShutdownSignals, mut tasks: JoinSet<()>) {
    info!("Stopping...");
    if let Some(process) = adp {
        info!("Stopping ADP (press Ctrl-C again to kill it at once)...");
        process.shutdown(signals.next()).await;
    }
    tasks.shutdown().await;
    info!("Stopped.");
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
