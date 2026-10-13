//! The launcher: the run directory, the IPC certificate, ADP's configuration, and the ADP child process.
//!
//! Before starting ADP, the launcher checks that ADP's ports are free ([`check_adp_ports`]), so that an ADP left over
//! from an earlier run is reported rather than taken for the one just started. ADP runs in its own process group, with
//! its standard output and error written to `adp.console.log` in the run directory (it also writes its own log file,
//! `adp.log`). Environment variables starting with `DD_` are removed from ADP's environment, so that the generated
//! configuration file is the whole configuration. On shutdown the launcher sends `SIGTERM` to the process group, and
//! `SIGKILL` if ADP has not exited within [`SHUTDOWN_GRACE`] or the caller asks again.
//!
//! Since ADP has its own process group, signals the terminal sends to the demo never reach it: the demo stops it on
//! Ctrl-C, `SIGTERM`, and `SIGHUP`. If the demo itself is killed with `SIGKILL` or aborts, nothing stops ADP; the next
//! run's port check then names the port it holds.

use std::{
    fs::{self, OpenOptions},
    future::Future,
    io::Write as _,
    net::{TcpListener, UdpSocket},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use rustls::pki_types::{pem::PemObject as _, CertificateDer, PrivateKeyDer};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Serialize;
use tokio::{process::Command, sync::watch};

use crate::{
    adp_api::AdpApi,
    clock::unix_millis,
    settings::{DemoPorts, RunSettings},
};

pub mod config;

/// How long ADP has to exit after `SIGTERM` before it is killed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How long the launcher waits for ADP to report ready.
pub const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Creates the run directory, makes sure it has a usable IPC certificate, and writes ADP's configuration.
///
/// A certificate file that is missing, or that does not hold a certificate and a private key (such as one cut short by
/// a crash), is generated anew. One that is readable by others is made readable only by its owner.
///
/// # Errors
///
/// Returns an error if a file cannot be read or written.
pub fn prepare_run_dir(settings: &RunSettings) -> Result<(), GenericError> {
    fs::create_dir_all(&settings.run_dir)
        .with_error_context(|| format!("Failed to create the run directory '{}'.", settings.run_dir.display()))?;

    let cert_path = settings.ipc_cert_path();
    match fs::read(&cert_path) {
        Ok(pem) if holds_certificate_and_key(&pem) => restrict_to_owner(&cert_path)?,
        Ok(_) => {
            tracing::warn!(
                path = %cert_path.display(),
                "The IPC certificate file does not hold a certificate and a private key; generating a new one."
            );
            write_ipc_certificate(&cert_path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => write_ipc_certificate(&cert_path)?,
        Err(error) => {
            return Err(generic_error!(
                "Failed to read the IPC certificate '{}': {}",
                cert_path.display(),
                error
            ))
        }
    }

    let config = config::render(&config::adp_config(settings)).error_context("Failed to write ADP's configuration.")?;
    let config_path = settings.adp_config_path();
    fs::write(&config_path, config)
        .with_error_context(|| format!("Failed to write ADP's configuration to '{}'.", config_path.display()))
}

/// Returns `true` if a PEM file holds a certificate and a private key, as the IPC certificate file must.
fn holds_certificate_and_key(pem: &[u8]) -> bool {
    CertificateDer::from_pem_slice(pem).is_ok() && PrivateKeyDer::from_pem_slice(pem).is_ok()
}

/// Writes a self-signed certificate for `localhost` and its private key, as one PEM file, readable only by the owner.
///
/// ADP presents the certificate on its privileged API and requires clients to present the same one; the demo does.
/// The file is written under a temporary name, created readable only by the owner, flushed, and then renamed into
/// place, so the key is never readable by others and a crash never leaves a partial file under the real name.
fn write_ipc_certificate(path: &Path) -> Result<(), GenericError> {
    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(["localhost".to_string()])
        .map_err(|error| generic_error!("Failed to generate the IPC certificate: {}", error))?;
    let pem = format!("{}{}", cert.pem(), signing_key.serialize_pem());

    let temporary = path.with_extension("pem.tmp");
    // A temporary file left by an earlier, interrupted run may have any mode; never write the key into it.
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(generic_error!(
                "Failed to remove the leftover temporary file '{}': {}",
                temporary.display(),
                error
            ))
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(pem.as_bytes())?;
            file.sync_all()
        })
        .with_error_context(|| format!("Failed to write the IPC certificate '{}'.", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_error_context(|| format!("Failed to move the IPC certificate into place at '{}'.", path.display()))
}

/// Makes a file readable and writable only by its owner, if others can read or write it.
fn restrict_to_owner(path: &Path) -> Result<(), GenericError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(path)
            .with_error_context(|| format!("Failed to read the permissions of '{}'.", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            tracing::warn!(path = %path.display(), "The IPC certificate was readable by others; restricting it.");
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_error_context(|| {
                format!(
                    "Failed to make the IPC certificate '{}' readable only by its owner.",
                    path.display()
                )
            })?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Checks that the ports ADP listens on are free on the loopback interface: its unprivileged and privileged APIs
/// (TCP) and DogStatsD (UDP).
///
/// The demo recognizes ADP only by its answers on these ports. If another process already holds one, ADP fails to
/// start, and the process holding it, often an ADP left over from an earlier run, would be taken for the one just
/// started. Each port is bound and released at once.
///
/// # Errors
///
/// Returns an error naming the first port that is not free, how to find what holds it, and the option that moves it.
pub fn check_adp_ports(ports: &DemoPorts) -> Result<(), GenericError> {
    let tcp = [
        (ports.api, "ADP's unprivileged API", "--api-port"),
        (ports.secure_api, "ADP's privileged API", "--secure-api-port"),
    ];
    for (port, what, option) in tcp {
        TcpListener::bind(DemoPorts::loopback(port)).map_err(|error| port_taken(port, "TCP", what, option, error))?;
    }
    UdpSocket::bind(DemoPorts::loopback(ports.dogstatsd))
        .map_err(|error| port_taken(ports.dogstatsd, "UDP", "ADP's DogStatsD", "--dogstatsd-port", error))?;
    Ok(())
}

fn port_taken(port: u16, protocol: &str, what: &str, option: &str, error: std::io::Error) -> GenericError {
    generic_error!(
        "127.0.0.1:{} ({} port of {}) is not free: {}. Another process holds it, often an ADP left over from an \
         earlier demo run; find it with `lsof -nP -i{}:{}` and stop it, or choose another port with {}.",
        port,
        protocol,
        what,
        error,
        protocol,
        port,
        option
    )
}

/// Returns the command line that runs ADP with the generated configuration, for display.
pub fn adp_command_line(adp_bin: &Path, settings: &RunSettings) -> String {
    format!(
        "{} --config {} run",
        adp_bin.display(),
        settings.adp_config_path().display()
    )
}

/// Where ADP is in its life, as the UI shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The demo did not launch ADP (`--no-launch`).
    External,

    /// ADP was started and has not reported ready yet.
    Starting,

    /// ADP reported ready.
    Ready,

    /// ADP exited.
    Exited,
}

/// The launcher's view of ADP, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct LaunchView {
    /// Where ADP is in its life.
    pub phase: Phase,

    /// ADP's process ID, when the demo launched it.
    pub pid: Option<u32>,

    /// When ADP was started.
    pub started_ms: Option<u64>,

    /// When ADP reported ready.
    pub ready_ms: Option<u64>,

    /// How ADP exited.
    pub exit: Option<String>,

    /// The command that runs ADP with the demo's configuration.
    pub command: String,

    /// ADP's log file.
    pub log_path: String,

    /// The file ADP's standard output and error go to.
    pub console_log_path: String,
}

/// The launcher's record of ADP.
///
/// Cheap to clone: clones share the record.
#[derive(Clone)]
pub struct Launch {
    view: Arc<Mutex<LaunchView>>,
}

impl Launch {
    /// Creates the record for a run in which ADP is in the given phase.
    pub fn new(phase: Phase, adp_bin: &Path, settings: &RunSettings) -> Self {
        Self {
            view: Arc::new(Mutex::new(LaunchView {
                phase,
                pid: None,
                started_ms: None,
                ready_ms: None,
                exit: None,
                command: adp_command_line(adp_bin, settings),
                log_path: settings.adp_log_path().display().to_string(),
                console_log_path: settings.adp_console_log_path().display().to_string(),
            })),
        }
    }

    /// Returns the launcher's view of ADP.
    pub fn view(&self) -> LaunchView {
        self.view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn update<F>(&self, f: F)
    where
        F: FnOnce(&mut LaunchView),
    {
        f(&mut self.view.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    }
}

/// A running ADP child process.
pub struct AdpProcess {
    pid: u32,
    exited: watch::Receiver<Option<String>>,
    stopping: Arc<AtomicBool>,
}

impl AdpProcess {
    /// Starts ADP with the run's configuration, in its own process group.
    ///
    /// # Errors
    ///
    /// Returns an error if the console log cannot be created or the process cannot be started.
    pub fn spawn(adp_bin: &Path, settings: &RunSettings, launch: &Launch) -> Result<Self, GenericError> {
        let console_path = settings.adp_console_log_path();
        let console = fs::File::create(&console_path)
            .with_error_context(|| format!("Failed to create ADP's console log '{}'.", console_path.display()))?;
        let console_err = console
            .try_clone()
            .error_context("Failed to share ADP's console log between standard output and error.")?;

        let mut command = Command::new(adp_bin);
        command
            .arg("--config")
            .arg(settings.adp_config_path())
            .arg("run")
            .current_dir(&settings.run_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(console))
            .stderr(Stdio::from(console_err))
            .kill_on_drop(true);
        let inherited = std::env::vars_os()
            .filter_map(|(name, _)| name.to_str().filter(|name| name.starts_with("DD_")).map(str::to_string))
            .collect::<Vec<_>>();
        for name in &inherited {
            command.env_remove(name);
        }
        if !inherited.is_empty() {
            tracing::info!(
                removed = %inherited.join(", "),
                "Removed DD_* variables from ADP's environment; the generated configuration file is the whole \
                 configuration."
            );
        }
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .with_error_context(|| format!("Failed to start ADP from '{}'.", adp_bin.display()))?;
        let pid = child
            .id()
            .ok_or_else(|| generic_error!("ADP exited before its process ID could be read."))?;
        launch.update(|view| {
            view.pid = Some(pid);
            view.started_ms = Some(unix_millis());
        });

        let (exit_tx, exited) = watch::channel(None);
        let monitor_launch = launch.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let monitor_stopping = Arc::clone(&stopping);
        tokio::spawn(async move {
            let exit = match child.wait().await {
                Ok(status) => describe_exit(status),
                Err(error) => format!("ADP's exit status could not be read: {}", error),
            };
            if monitor_stopping.load(Ordering::Relaxed) {
                tracing::info!(%exit, "ADP stopped.");
            } else {
                tracing::warn!(%exit, "ADP exited.");
            }
            monitor_launch.update(|view| {
                view.phase = Phase::Exited;
                view.exit = Some(exit.clone());
            });
            let _ = exit_tx.send(Some(exit));
        });

        Ok(Self { pid, exited, stopping })
    }

    /// Waits until ADP's unprivileged API reports ready, ADP exits, or [`READY_TIMEOUT`] passes.
    ///
    /// # Errors
    ///
    /// Returns an error if ADP exits or does not become ready in time.
    pub async fn wait_ready(&self, api: &AdpApi, launch: &Launch, console_log: &Path) -> Result<(), GenericError> {
        let started = Instant::now();
        let mut exited = self.exited.clone();
        loop {
            if let Some(exit) = exited.borrow().clone() {
                return Err(generic_error!(
                    "ADP {} before it became ready. Its output is in '{}'.",
                    exit,
                    console_log.display()
                ));
            }
            if api.ready().await {
                launch.update(|view| {
                    view.phase = Phase::Ready;
                    view.ready_ms = Some(unix_millis());
                });
                return Ok(());
            }
            if started.elapsed() > READY_TIMEOUT {
                return Err(generic_error!(
                    "ADP did not report ready within {} seconds. Its output is in '{}'.",
                    READY_TIMEOUT.as_secs(),
                    console_log.display()
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                _ = exited.changed() => {}
            }
        }
    }

    /// Waits until ADP exits on its own.
    pub async fn exited(&self) -> String {
        let mut exited = self.exited.clone();
        loop {
            if let Some(exit) = exited.borrow().clone() {
                return exit;
            }
            if exited.changed().await.is_err() {
                return "ADP's monitor stopped.".to_string();
            }
        }
    }

    /// Stops ADP: `SIGTERM` to its process group, then `SIGKILL` if it has not exited within [`SHUTDOWN_GRACE`], or
    /// as soon as `kill_now` completes (such as on a second Ctrl-C).
    pub async fn shutdown<F>(self, kill_now: F)
    where
        F: Future,
    {
        if self.exited.borrow().is_some() {
            return;
        }
        self.stopping.store(true, Ordering::Relaxed);
        signal_group(self.pid, "TERM").await;
        let mut exited = self.exited.clone();
        let wait = async {
            while exited.borrow().is_none() {
                if exited.changed().await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = wait => return,
            _ = tokio::time::sleep(SHUTDOWN_GRACE) => tracing::warn!(
                "ADP did not exit within {} seconds of SIGTERM; killing its process group.",
                SHUTDOWN_GRACE.as_secs()
            ),
            _ = kill_now => tracing::warn!("Killing ADP's process group now."),
        }
        signal_group(self.pid, "KILL").await;
    }
}

/// Sends a signal to a process group with `kill(1)`, which needs no unsafe code.
async fn signal_group(pgid: u32, signal: &str) {
    let result = Command::new("kill")
        .arg("-s")
        .arg(signal)
        .arg("--")
        .arg(format!("-{}", pgid))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, signal, pgid, "Signalling ADP's process group failed."),
        Err(error) => tracing::warn!(%error, signal, pgid, "Failed to run kill(1) to signal ADP's process group."),
    }
}

fn describe_exit(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return format!("was stopped by signal {}", signal);
        }
    }
    match status.code() {
        Some(0) => "exited normally".to_string(),
        Some(code) => format!("exited with status {}", code),
        None => "exited".to_string(),
    }
}

/// Returns the default run directory: a new directory under `target/programmable-metrics-demo`, named for what runs
/// in it (`run`, or `check` for a scripted check) and the time.
pub fn default_run_dir(kind: &str) -> PathBuf {
    PathBuf::from("target")
        .join("programmable-metrics-demo")
        .join(format!("{}-{}", kind, unix_millis() / 1000))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::settings::DemoPorts;

    fn settings(run_dir: PathBuf) -> RunSettings {
        RunSettings {
            ports: DemoPorts::default(),
            run_dir,
            poll_interval: Duration::from_secs(1),
            aggregate_window_secs: 1,
            counter_expiry_secs: 0,
            traffic_rate: 20.0,
            traffic_paused: false,
            contexts: 0,
            lanes: Default::default(),
        }
    }

    #[tokio::test]
    async fn the_run_directory_gets_a_certificate_the_ipc_client_accepts_and_the_configuration() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path().join("run"));
        prepare_run_dir(&settings).unwrap();

        let pem = fs::read_to_string(settings.ipc_cert_path()).unwrap();
        assert!(pem.contains("-----BEGIN CERTIFICATE-----"));
        assert!(pem.contains("-----BEGIN PRIVATE KEY-----"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(settings.ipc_cert_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // ADP's own IPC client reads it.
        datadog_agent_commons::ipc::tls::build_ipc_client_ipc_tls_config(settings.ipc_cert_path())
            .await
            .unwrap();

        let config = fs::read_to_string(settings.adp_config_path()).unwrap();
        assert_eq!(config, config::render(&config::adp_config(&settings)).unwrap());

        // A second run in the same directory keeps the certificate.
        prepare_run_dir(&settings).unwrap();
        assert_eq!(fs::read_to_string(settings.ipc_cert_path()).unwrap(), pem);
    }

    #[tokio::test]
    async fn a_cut_short_certificate_is_generated_anew_and_one_readable_by_others_is_restricted() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path().join("run"));
        prepare_run_dir(&settings).unwrap();
        let pem = fs::read_to_string(settings.ipc_cert_path()).unwrap();

        // Cut short, as by a crash while writing: neither the key nor, here, the certificate can be read.
        fs::write(settings.ipc_cert_path(), &pem[..pem.len() / 3]).unwrap();
        // A temporary file left by an interrupted run, readable by everyone, is not reused.
        fs::write(settings.ipc_cert_path().with_extension("pem.tmp"), "stale").unwrap();
        prepare_run_dir(&settings).unwrap();
        let regenerated = fs::read_to_string(settings.ipc_cert_path()).unwrap();
        assert_ne!(regenerated, pem);
        assert!(holds_certificate_and_key(regenerated.as_bytes()));
        datadog_agent_commons::ipc::tls::build_ipc_client_ipc_tls_config(settings.ipc_cert_path())
            .await
            .unwrap();
        assert!(!settings.ipc_cert_path().with_extension("pem.tmp").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&settings.ipc_cert_path()), 0o600);
            fs::set_permissions(settings.ipc_cert_path(), fs::Permissions::from_mode(0o644)).unwrap();
            prepare_run_dir(&settings).unwrap();
            assert_eq!(mode(&settings.ipc_cert_path()), 0o600);
            // The certificate itself was kept.
            assert_eq!(fs::read_to_string(settings.ipc_cert_path()).unwrap(), regenerated);
        }
    }

    #[test]
    fn ports_held_by_another_process_are_named_before_adp_starts() {
        let free = || TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ports = DemoPorts {
            api: free(),
            secure_api: free(),
            dogstatsd: UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(),
            ..DemoPorts::default()
        };
        check_adp_ports(&ports).unwrap();

        let held_tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = DemoPorts {
            secure_api: held_tcp.local_addr().unwrap().port(),
            ..ports.clone()
        };
        let error = check_adp_ports(&taken).unwrap_err().to_string();
        assert!(error.contains(&format!(
            "127.0.0.1:{} (TCP port of ADP's privileged API)",
            taken.secure_api
        )));
        assert!(
            error.contains(&format!("lsof -nP -iTCP:{}", taken.secure_api)),
            "{}",
            error
        );
        assert!(error.contains("--secure-api-port"), "{}", error);

        let held_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let taken = DemoPorts {
            dogstatsd: held_udp.local_addr().unwrap().port(),
            ..ports
        };
        let error = check_adp_ports(&taken).unwrap_err().to_string();
        assert!(error.contains("UDP port of ADP's DogStatsD"), "{}", error);
        assert!(error.contains("--dogstatsd-port"), "{}", error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_that_exits_is_reported_and_shutdown_stops_a_running_group() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path().to_path_buf());
        let launch = Launch::new(Phase::Starting, Path::new("/bin/sh"), &settings);

        // A stand-in for ADP that ignores its arguments and exits with status 3.
        let script = dir.path().join("exit-3.sh");
        fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
        make_executable(&script);
        let process = AdpProcess::spawn(&script, &settings, &launch).unwrap();
        assert_eq!(process.exited().await, "exited with status 3");
        assert_eq!(launch.view().phase, Phase::Exited);

        // A stand-in that runs until signalled, with a child of its own in the same process group.
        let script = dir.path().join("sleep.sh");
        fs::write(&script, "#!/bin/sh\nsleep 300 &\nwait\n").unwrap();
        make_executable(&script);
        let launch = Launch::new(Phase::Starting, &script, &settings);
        let process = AdpProcess::spawn(&script, &settings, &launch).unwrap();
        let pid = launch.view().pid.unwrap();
        let started = Instant::now();
        process.shutdown(std::future::pending::<()>()).await;
        assert!(started.elapsed() < SHUTDOWN_GRACE);
        assert_eq!(launch.view().phase, Phase::Exited);
        assert!(launch.view().exit.unwrap().contains("signal 15"));
        // The whole group is gone, including the stand-in's own child (which may take a moment to be reaped).
        wait_until_group_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_request_kills_a_group_that_ignores_sigterm_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path().to_path_buf());

        // A stand-in that ignores SIGTERM, as does its child, which inherits the ignored disposition.
        let script = dir.path().join("stubborn.sh");
        // The script marks itself ready only after installing its trap, so the test never signals it too early.
        let ready = dir.path().join("trap-installed");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap '' TERM\nsleep 300 &\ntouch '{}'\nwait\n",
                ready.display()
            ),
        )
        .unwrap();
        make_executable(&script);
        let launch = Launch::new(Phase::Starting, &script, &settings);
        let process = AdpProcess::spawn(&script, &settings, &launch).unwrap();
        let pid = launch.view().pid.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready.exists(), "the stand-in never installed its trap");

        let started = Instant::now();
        process.shutdown(tokio::time::sleep(Duration::from_millis(300))).await;
        assert!(started.elapsed() < SHUTDOWN_GRACE);
        wait_until_group_gone(pid).await;
        // The monitor records the exit just after reaping the process.
        let deadline = Instant::now() + Duration::from_secs(2);
        while launch.view().exit.is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let exit = launch.view().exit.unwrap_or_default();
        assert!(exit.contains("signal 9"), "{}", exit);
    }

    /// Waits up to five seconds for every process of a group to be gone, and fails if one is left.
    #[cfg(unix)]
    async fn wait_until_group_gone(pgid: u32) {
        let group_alive = || {
            std::process::Command::new("kill")
                .args(["-s", "0", "--", &format!("-{}", pgid)])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_alive() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!group_alive());
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
