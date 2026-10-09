//! Anomaly event listener.
//!
//! The isolated Agent Anomaly Detection process publishes anomaly events on a FIT
//! broadcast ring that any number of subscribers share. ADP is one of them: this module
//! subscribes, decodes each event, and logs it. It deliberately joins no topology and
//! forwards nothing.
//!
//! The endpoint belongs to the publisher, not to ADP, which changes the failure model
//! compared to metric forwarding: a missing publisher is normal, because AAD may start
//! later or restart, so a failed or ended subscription is retried instead of failing
//! startup. One exception is documented rather than hidden: FIT has no liveness check, so
//! a publisher that stops without removing its endpoint (a crash) leaves this listener
//! parked on a silent ring until ADP stops. A publisher that exits normally removes its
//! Unix socket, which the watcher below notices, so ADP rejoins the session that follows.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use datadog_checks_protocol::EventSubscriber;
use saluki_fit::{CancellationToken, SetupEndpoint, SubscriberConfig};
use tracing::{debug, info, warn};

/// How long to wait before retrying a subscription that failed or ended.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Per-handshake connect deadline.
///
/// The publisher may not exist yet, so this stays short: it is the retry loop, not the
/// handshake, that waits for AAD to appear.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the watcher checks that the publisher still owns its endpoint.
const ENDPOINT_WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// A running anomaly event listener.
pub struct AnomalyEventListener {
    stopping: Arc<AtomicBool>,
    /// The live session token, so `stop` can cancel whichever attempt is running.
    ///
    /// FIT cancellation tokens are one-shot, so each attempt gets its own token and
    /// publishes it here while it is live.
    live_session: Arc<Mutex<Option<CancellationToken>>>,
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl AnomalyEventListener {
    /// Starts the listener on a dedicated thread.
    pub fn start(endpoint: SetupEndpoint) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let live_session = Arc::new(Mutex::new(None));
        let (stop, stopped) = mpsc::channel();
        let context = Context {
            label: endpoint_label(&endpoint),
            endpoint,
            stopping: stopping.clone(),
            live_session: live_session.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("anomaly-events".to_owned())
            .spawn(move || run(context, stopped))
            .expect("spawning the anomaly event listener thread cannot fail");
        Self {
            stopping,
            live_session,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    /// Stops the listener and waits for its thread to finish.
    ///
    /// Cancelling the live session token is what unblocks a parked receive, and the
    /// channel signal is what ends a retry wait early, so shutdown waits out neither.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let live = self.live_session.lock().unwrap().take();
        if let Some(session) = live {
            let _ = session.cancel();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AnomalyEventListener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Shared state for the listener thread.
struct Context {
    label: String,
    endpoint: SetupEndpoint,
    stopping: Arc<AtomicBool>,
    live_session: Arc<Mutex<Option<CancellationToken>>>,
}

impl Context {
    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn clear_session(&self) {
        *self.live_session.lock().unwrap() = None;
    }
}

/// Why a subscription's drain loop returned.
enum DrainOutcome {
    /// The subscription was cancelled, either by shutdown or by the watcher.
    Cancelled,
    /// The session broke on its own, for example a malformed record.
    SessionEnded,
}

/// Subscribes and drains in a loop until told to stop.
fn run(context: Context, stopped: Receiver<()>) {
    loop {
        let session = CancellationToken::new();
        *context.live_session.lock().unwrap() = Some(session.clone());
        let watch = PublisherWatch::start(&context.label, &context.endpoint, session.clone());

        let mut config = SubscriberConfig::for_endpoint(context.endpoint.clone());
        config.setup_timeout = CONNECT_TIMEOUT;
        match EventSubscriber::subscribe_with_cancel(config, &session) {
            Ok(mut subscriber) => {
                info!(
                    session = subscriber.session_id(),
                    slot = subscriber.slot_id(),
                    endpoint = %context.label,
                    "Subscribed to anomaly events."
                );
                let outcome = drain(&mut subscriber, &session, &context.label);
                // A failed leave is expected when the publisher is already gone.
                if let Err(error) = subscriber.unsubscribe() {
                    debug!(endpoint = %context.label, %error, "Unsubscribing from anomaly events failed.");
                }
                context.clear_session();
                drop(watch);
                if context.stopping() {
                    return;
                }
                if let DrainOutcome::Cancelled = outcome {
                    debug!(endpoint = %context.label, "Anomaly event publisher went away; rejoining.");
                }
            }
            Err(error) => {
                context.clear_session();
                drop(watch);
                if context.stopping() {
                    return;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    debug!(endpoint = %context.label, "Anomaly event subscription was cancelled; rejoining.");
                } else {
                    debug!(endpoint = %context.label, %error, "No anomaly event publisher yet; retrying.");
                }
            }
        }

        match stopped.recv_timeout(RETRY_INTERVAL) {
            // Stopped, or every sender is gone.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Logs events until the session ends.
fn drain(subscriber: &mut EventSubscriber, session: &CancellationToken, label: &str) -> DrainOutcome {
    loop {
        match subscriber.receive_with_cancel(session) {
            Ok(Some(event)) => warn!(
                endpoint = %label,
                title = %event.title,
                description = %event.description,
                timestamp = event.timestamp,
                "Anomaly event received."
            ),
            // The session token was cancelled: shutdown, or the publisher left.
            Ok(None) => return DrainOutcome::Cancelled,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return DrainOutcome::Cancelled,
            Err(error) => {
                warn!(endpoint = %label, %error, "Anomaly event subscription ended; resubscribing.");
                return DrainOutcome::SessionEnded;
            }
        }
    }
}

/// Notices that the publisher removed its endpoint, which is how a normal exit looks.
///
/// FIT has no liveness signal of its own, so without this a subscriber stays parked on
/// the ring of a process that is gone. Only Unix endpoints can be watched this way; a TCP
/// endpoint leaves no path to check.
struct PublisherWatch {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PublisherWatch {
    fn start(label: &str, endpoint: &SetupEndpoint, session: CancellationToken) -> Option<Self> {
        let SetupEndpoint::Unix(path) = endpoint else {
            debug!(
                endpoint = %label,
                "Anomaly events use a TCP endpoint, which cannot be watched for a publisher restart."
            );
            return None;
        };
        let path = path.clone();
        let label = label.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("anomaly-events-watch".to_owned())
            .spawn(move || watch_endpoint(path, label, session, worker_stop))
            .expect("spawning the anomaly event endpoint watcher cannot fail");
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for PublisherWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The watcher sleeps in short intervals, so joining it is quick.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn watch_endpoint(path: PathBuf, label: String, session: CancellationToken, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(ENDPOINT_WATCH_INTERVAL);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if !path.exists() {
            debug!(
                endpoint = %label,
                "The anomaly event publisher removed its endpoint; rejoining the next session."
            );
            let _ = session.cancel();
            return;
        }
    }
}

/// Human-readable endpoint for logs.
fn endpoint_label(endpoint: &SetupEndpoint) -> String {
    match endpoint {
        SetupEndpoint::Unix(path) => path.display().to_string(),
        SetupEndpoint::Tcp(address) => address.to_string(),
    }
}
