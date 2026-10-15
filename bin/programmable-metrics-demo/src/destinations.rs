//! The demo's destinations: one recording intake per endpoint ADP's configuration names, and the live changes the demo
//! makes to that configuration.
//!
//! At boot the destinations are the primary endpoint, teams B and C (additional endpoints), and the multi-region
//! failover endpoint, each on its port. **Add destination** ([`Destinations::add`]) starts a recording intake on a port
//! the system picks, with an API key of its own and a label for the UI (`Destination D`, then `E`, and so on), and adds
//! it to `additional_endpoints` in the run's `datadog.yaml`, which the launcher rewrites atomically. ADP re-reads the
//! file (`experimental.standalone_config_reload`) and declares the endpoint live, as a recipient named by its URL: the
//! demo maps the URL to the label for display, and ADP knows only the URL. Under the default program, a new additional
//! endpoint receives everything (native dual shipping); under a published program it receives nothing until the program
//! routes to it.
//!
//! **Remove** ([`Destinations::remove`]) takes an additional endpoint out of the file. ADP drains its recipient and
//! retires it; the intake keeps answering until ADP no longer lists the recipient as preparing, live, draining, or
//! exiting (or [`REMOVAL_TIMEOUT`] passes, past the drain deadline), so that the drain can deliver, and then stops. A
//! published program that still routes to it runs degraded: its `emit` to the endpoint becomes a counted drop.
//!
//! The native settings the demo changes ([`Destinations::set_native`]) are written the same way: the failover signal
//! (`multi_region_failover.failover_metrics`) and the metric filterlist (`metric_filterlist`), a native policy setting
//! that the default program applies and a published program does not.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Serialize;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::{
    clock::unix_millis,
    control::ControlSurface,
    intake::{self, IntakeRecorder},
    launcher::{
        self,
        config::{endpoint, LiveSettings, RELOAD_INTERVAL},
    },
    settings::{DemoPorts, Recipient, RunSettings},
};

/// The first letter of the labels of added destinations: teams B and C are configured at boot.
const FIRST_LETTER: u8 = b'D';

/// How many added destinations can be configured at once.
pub const MAX_ADDED: usize = 8;

/// How long a removed destination's intake keeps answering at most, waiting for ADP to retire its recipient: the drain
/// deadline (30 s), with room for ADP to read the file and report the retirement.
pub const REMOVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a removal checks whether ADP retired the recipient.
const REMOVAL_POLL: Duration = Duration::from_millis(500);

/// How many ports a new destination tries before giving up on one no destination of the run had.
const PORT_ATTEMPTS: usize = 16;

/// How long a stopping intake may take to close its connections before its task is aborted.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The recipient states in which ADP still delivers to, or drains toward, an endpoint.
const STILL_DELIVERING: [&str; 4] = ["preparing", "active", "draining", "exiting"];

/// One destination: its intake, the task serving it, and whether it is being removed.
struct Destination {
    recorder: IntakeRecorder,
    removing_since_ms: Option<u64>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Destination {
    /// Stops the intake's server: gracefully, then aborted after [`STOP_GRACE`].
    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

struct State {
    destinations: Vec<Destination>,
    live: LiveSettings,
    next_letter: u8,
    /// Every port a destination listened on in this run, so that a new one never takes a removed one's URL, which a
    /// program written for the removed one would then deliver to.
    used_ports: BTreeSet<u16>,
}

/// One destination, as the UI and API show it.
#[derive(Clone, Debug, Serialize)]
pub struct DestinationView {
    /// The intake's name.
    pub name: String,

    /// Its label.
    pub label: String,

    /// The endpoint's URL, which is also the name of its recipient in ADP's directory.
    pub url: String,

    /// The intake's port.
    pub port: u16,

    /// The API key ADP is configured to send to it.
    pub api_key: String,
}

impl From<&Recipient> for DestinationView {
    fn from(recipient: &Recipient) -> Self {
        Self {
            name: recipient.name.clone(),
            label: recipient.label.clone(),
            url: recipient.url(),
            port: recipient.port,
            api_key: recipient.api_key.clone(),
        }
    }
}

/// The native settings the demo writes, as the UI shows them.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct NativeView {
    /// `metric_filterlist`.
    pub metric_filterlist: Vec<String>,

    /// `multi_region_failover.failover_metrics`.
    pub failover_metrics: bool,
}

/// A change of the native settings the demo writes: each member given replaces the current one.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct NativeChange {
    /// The new `metric_filterlist`; an empty list removes the key.
    pub metric_filterlist: Option<Vec<String>>,

    /// The new `multi_region_failover.failover_metrics`.
    pub failover_metrics: Option<bool>,
}

/// Why a destination could not be removed.
#[derive(Debug, Eq, PartialEq)]
pub enum RemoveError {
    /// There is no destination of that name.
    NotFound,

    /// It is not an additional endpoint: the primary and failover endpoints stay.
    NotRemovable,

    /// It is being removed already.
    Removing,

    /// The configuration file could not be written.
    Write(String),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "There is no destination of that name."),
            Self::NotRemovable => write!(
                f,
                "Only additional endpoints can be removed: the primary and the failover endpoints stay."
            ),
            Self::Removing => write!(
                f,
                "The destination is being removed already: ADP is draining its recipient."
            ),
            Self::Write(error) => write!(f, "{}", error),
        }
    }
}

/// The demo's destinations and the live part of ADP's configuration.
///
/// Cheap to clone: clones share the record.
#[derive(Clone)]
pub struct Destinations {
    settings: Arc<RunSettings>,
    control: ControlSurface,
    state: Arc<Mutex<State>>,
}

impl Destinations {
    /// Starts the intakes of the destinations ADP's configuration names at boot, on their ports. The configuration file
    /// itself is written by [`launcher::prepare_run_dir`] with the same settings.
    ///
    /// # Errors
    ///
    /// Returns an error naming the port if an intake cannot listen on it.
    pub fn start(settings: Arc<RunSettings>, control: ControlSurface) -> Result<Self, GenericError> {
        let mut destinations = Vec::new();
        for recipient in settings.ports.recipients() {
            let listener =
                std::net::TcpListener::bind(DemoPorts::loopback(recipient.port)).with_error_context(|| {
                    format!(
                        "Failed to listen on 127.0.0.1:{} for the {} intake. Is another demo running? Choose another \
                     port with the command's port options.",
                        recipient.port, recipient.label
                    )
                })?;
            destinations.push(serve(recipient, listener)?);
        }
        let live = LiveSettings::at_boot(&settings.ports);
        let used_ports = destinations
            .iter()
            .map(|destination| destination.recorder.recipient().port)
            .collect();
        Ok(Self {
            settings,
            control,
            state: Arc::new(Mutex::new(State {
                destinations,
                live,
                next_letter: FIRST_LETTER,
                used_ports,
            })),
        })
    }

    /// Returns every intake, those being removed included, in the order the UI shows them.
    pub fn intakes(&self) -> Vec<IntakeRecorder> {
        self.lock()
            .destinations
            .iter()
            .map(|destination| destination.recorder.clone())
            .collect()
    }

    /// Returns every intake with when its removal started, if it is being removed.
    pub fn intakes_with_removal(&self) -> Vec<(IntakeRecorder, Option<u64>)> {
        self.lock()
            .destinations
            .iter()
            .map(|destination| (destination.recorder.clone(), destination.removing_since_ms))
            .collect()
    }

    /// Returns the intake named `name`, if there is one.
    pub fn find(&self, name: &str) -> Option<IntakeRecorder> {
        self.lock()
            .destinations
            .iter()
            .find(|destination| destination.recorder.recipient().name == name)
            .map(|destination| destination.recorder.clone())
    }

    /// Returns the labels of the destinations, by URL: what the editor writes beside a recipient named by its URL.
    pub fn labels(&self) -> Vec<(String, String)> {
        self.lock()
            .destinations
            .iter()
            .map(|destination| {
                let recipient = destination.recorder.recipient();
                (recipient.url(), recipient.label.clone())
            })
            .collect()
    }

    /// Returns the native settings the demo wrote last.
    pub fn native(&self) -> NativeView {
        let state = self.lock();
        NativeView {
            metric_filterlist: state.live.metric_filterlist.clone(),
            failover_metrics: state.live.failover_metrics,
        }
    }

    /// Adds a destination: starts its intake on a port the system picks, and adds it to `additional_endpoints` in
    /// ADP's configuration file.
    ///
    /// # Errors
    ///
    /// Returns an error if [`MAX_ADDED`] added destinations are configured already, the labels are used up, the intake
    /// cannot listen, or the file cannot be written; nothing is added then.
    pub fn add(&self) -> Result<Recipient, GenericError> {
        let mut state = self.lock();
        let added = state
            .destinations
            .iter()
            .filter(|destination| destination.recorder.recipient().added && destination.removing_since_ms.is_none())
            .count();
        if added >= MAX_ADDED {
            return Err(generic_error!(
                "{} added destinations are configured already: remove one first.",
                MAX_ADDED
            ));
        }
        if state.next_letter > b'Z' {
            return Err(generic_error!(
                "Every label from Destination D to Destination Z was used in this run: restart the demo to add more."
            ));
        }
        // A port the system picks, and one no destination of this run had: a removed destination's URL is not reused.
        let (listener, port) = (0..PORT_ATTEMPTS)
            .find_map(|_| {
                let listener = std::net::TcpListener::bind(DemoPorts::loopback(0)).ok()?;
                let port = listener.local_addr().ok()?.port();
                (!state.used_ports.contains(&port)).then_some((listener, port))
            })
            .ok_or_else(|| {
                generic_error!("Failed to listen on a fresh loopback port for the new destination's intake.")
            })?;
        let recipient = Recipient::added(char::from(state.next_letter), port);
        // The intake listens before the file names it, and the file names nothing the demo does not record.
        let destination = serve(recipient.clone(), listener)?;
        let mut live = state.live.clone();
        live.additional_endpoints.push(endpoint(&recipient));
        if let Err(error) = launcher::write_config(&self.settings, &live) {
            if let Some(task) = &destination.task {
                task.abort();
            }
            return Err(error);
        }
        state.used_ports.insert(port);
        state.live = live;
        state.next_letter += 1;
        state.destinations.push(destination);
        tracing::info!(
            destination = %recipient.label,
            url = %recipient.url(),
            "Added a destination to ADP's configuration."
        );
        Ok(recipient)
    }

    /// Removes the destination named `name` from ADP's configuration file. Its intake keeps answering while ADP drains
    /// its recipient, and stops once ADP retired it, or after [`REMOVAL_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Returns an error if there is no such destination, it is not an additional endpoint, it is being removed already,
    /// or the file cannot be written; nothing changes then.
    pub fn remove(&self, name: &str) -> Result<Recipient, RemoveError> {
        let recipient = {
            let mut state = self.lock();
            let Some(index) = state
                .destinations
                .iter()
                .position(|destination| destination.recorder.recipient().name == name)
            else {
                return Err(RemoveError::NotFound);
            };
            let recipient = state.destinations[index].recorder.recipient().clone();
            if !recipient.removable() {
                return Err(RemoveError::NotRemovable);
            }
            if state.destinations[index].removing_since_ms.is_some() {
                return Err(RemoveError::Removing);
            }
            let mut live = state.live.clone();
            let url = recipient.url();
            live.additional_endpoints.retain(|(endpoint, _)| *endpoint != url);
            launcher::write_config(&self.settings, &live)
                .map_err(|error| RemoveError::Write(format!("{:#}", error)))?;
            state.live = live;
            state.destinations[index].removing_since_ms = Some(unix_millis());
            recipient
        };
        let written_ms = unix_millis();
        tracing::info!(
            destination = %recipient.label,
            url = %recipient.url(),
            "Removed a destination from ADP's configuration; its intake answers until ADP retires its recipient."
        );
        let destinations = self.clone();
        let removed = recipient.clone();
        tokio::spawn(async move { destinations.finish_removal(removed, written_ms).await });
        Ok(recipient)
    }

    /// Waits until ADP no longer delivers to `recipient`'s endpoint, or [`REMOVAL_TIMEOUT`] passes, then stops its
    /// intake and forgets it. Only a status ADP sent after it can have applied the file written at `written_ms` (two
    /// reload intervals later) says so: before, a destination added and removed at once may not be listed yet.
    async fn finish_removal(self, recipient: Recipient, written_ms: u64) {
        let started = Instant::now();
        let suffix = format!("/{}", recipient.url());
        let applied_ms = written_ms + 2 * RELOAD_INTERVAL.as_millis() as u64;
        loop {
            tokio::time::sleep(REMOVAL_POLL).await;
            let recent = self
                .control
                .status()
                .is_some_and(|(received_ms, _)| received_ms >= applied_ms);
            let delivering = self.control.recipients().recipients.iter().any(|listed| {
                listed.listed && listed.id.ends_with(&suffix) && STILL_DELIVERING.contains(&listed.state.as_str())
            });
            if recent && !delivering {
                break;
            }
            if started.elapsed() >= REMOVAL_TIMEOUT {
                tracing::warn!(
                    destination = %recipient.label,
                    "ADP still lists the removed destination's recipient after {} s; stopping its intake.",
                    REMOVAL_TIMEOUT.as_secs()
                );
                break;
            }
        }
        let destination = {
            let mut state = self.lock();
            let index = state
                .destinations
                .iter()
                .position(|destination| destination.recorder.recipient().name == recipient.name);
            index.map(|index| state.destinations.remove(index))
        };
        if let Some(destination) = destination {
            destination.stop().await;
            tracing::info!(destination = %recipient.label, "Stopped the removed destination's intake.");
        }
    }

    /// Changes the native settings the demo writes, and writes ADP's configuration file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written; nothing changes then.
    pub fn set_native(&self, change: NativeChange) -> Result<NativeView, GenericError> {
        let mut state = self.lock();
        let mut live = state.live.clone();
        if let Some(filterlist) = change.metric_filterlist {
            live.metric_filterlist = filterlist
                .into_iter()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect();
        }
        if let Some(failover) = change.failover_metrics {
            live.failover_metrics = failover;
        }
        launcher::write_config(&self.settings, &live)?;
        state.live = live;
        Ok(NativeView {
            metric_filterlist: state.live.metric_filterlist.clone(),
            failover_metrics: state.live.failover_metrics,
        })
    }

    /// Stops every intake.
    pub async fn shutdown(&self) {
        let destinations = std::mem::take(&mut self.lock().destinations);
        for destination in destinations {
            destination.stop().await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Serves a recording intake for `recipient` on `listener`, until told to stop.
fn serve(recipient: Recipient, listener: std::net::TcpListener) -> Result<Destination, GenericError> {
    listener
        .set_nonblocking(true)
        .error_context("Failed to make an intake's listener non-blocking.")?;
    let listener =
        tokio::net::TcpListener::from_std(listener).error_context("Failed to hand an intake's listener to tokio.")?;
    let recorder = IntakeRecorder::new(recipient);
    let router = intake::router(recorder.clone());
    let (stop, stopped) = oneshot::channel::<()>();
    let label = recorder.recipient().label.clone();
    let task = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        if let Err(error) = axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
            tracing::error!(%error, intake = %label, "A recording intake stopped.");
        }
    });
    Ok(Destination {
        recorder,
        removing_since_ms: None,
        stop: Some(stop),
        task: Some(task),
    })
}

#[cfg(test)]
mod tests;
