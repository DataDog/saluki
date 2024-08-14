use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use serde::Serialize;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::timeout,
};
use tokio_util::time::DelayQueue;

use self::api::HealthAPIHandler;

mod api;

const DEFAULT_PROBE_TIMEOUT_DUR: Duration = Duration::from_secs(5);
const DEFAULT_PROBE_BACKOFF_DUR: Duration = Duration::from_secs(1);

#[derive(Default, Serialize)]
struct ComponentHealth {
    ready: bool,
    live: bool,
}

/// A handle for updating the health of a component.
pub struct Health {
    ready: Arc<AtomicBool>,
    liveness_rx: mpsc::Receiver<(Instant, oneshot::Sender<(Instant, Instant)>)>,
}

impl Health {
    /// Marks the component as ready.
    pub fn mark_ready(&self) {
        self.ready.store(true, Relaxed);
    }

    /// Marks the component as not ready.
    pub fn mark_not_ready(&self) {
        self.ready.store(false, Relaxed);
    }

    /// Waits for a liveness probe to be sent to the component, and then responds to it.
    ///
    /// This should generally be polled as part of a `select!` block to ensure it is checked alongside other
    /// asynchronous operations.
    pub async fn live(&mut self) {
        // Simply wait for the health registry to send us a liveness probe, and if we receive one, we respond back to it
        // immediately.
        if let Some((send_time, tx)) = self.liveness_rx.recv().await {
            let receive_time = Instant::now();
            let _ = tx.send((send_time, receive_time));
        }
    }
}

#[derive(Eq, PartialEq)]
enum HealthState {
    Live,
    Unknown,
    Dead,
}

struct ComponentState {
    name: MetaString,
    health: HealthState,
    ready: Arc<AtomicBool>,
    liveness_tx: mpsc::Sender<(Instant, oneshot::Sender<(Instant, Instant)>)>,
    last_response: Instant,
    last_response_latency: Duration,
}

impl ComponentState {
    fn is_ready(&self) -> bool {
        // We consider a component ready if it's marked as ready (duh) and it's not dead.
        //
        // Being "dead" is a special case as it means the component is very likely not even running at all, not just
        // responding slowly or deadlocked. In these cases, it can't possibly be ready since it's not even running.
        self.ready.load(Relaxed) && self.health != HealthState::Dead
    }

    fn is_live(&self) -> bool {
        self.health == HealthState::Live
    }

    fn update_state(&mut self, result: LivenessResult) {
        match result {
            // If we timed out waiting for a response to a liveness probe, then the component could still be live but
            // just responding slowly, so we mark it as unknown. This still manifests as a failing liveness probe, but
            // it lets us differentiate between the component being dead -- like the task panicked and is never coming
            // back -- versus being deadlocked or otherwise unresponsive.
            LivenessResult::TimedOut { .. } => {
                self.health = HealthState::Unknown;
            }

            // The component is live.
            LivenessResult::Success {
                response_sent,
                response_latency,
                ..
            } => {
                self.health = HealthState::Live;
                self.last_response = response_sent;
                self.last_response_latency = response_latency;
            }

            // The component is dead.
            //
            // It's likely that the component's task/thread panicked and is never coming back.
            LivenessResult::Dead { .. } => {
                self.health = HealthState::Dead;
            }
        }
    }
}

enum LivenessResult {
    TimedOut {
        id: usize,
    },
    Success {
        id: usize,
        response_sent: Instant,
        response_latency: Duration,
    },
    Dead {
        id: usize,
    },
}

impl LivenessResult {
    fn id(&self) -> usize {
        match self {
            LivenessResult::TimedOut { id, .. } => *id,
            LivenessResult::Success { id, .. } => *id,
            LivenessResult::Dead { id, .. } => *id,
        }
    }
}

/// A registry of components and their health.
///
/// `HealthRegistry` is responsible for tracking the health of all registered components, by storing both their
/// readiness, which indicates whether or not they are initialized and generally ready to process data, as well as
/// probing their liveness, which indicates if they're currently responding, or able to respond, to requests.
pub struct HealthRegistry {
    registered_components: HashSet<MetaString>,
    components: Vec<ComponentState>,
    pending_requests: JoinSet<LivenessResult>,
    probe_scheduler: DelayQueue<usize>,
    timeout_dur: Duration,
    probe_backoff_dur: Duration,
    health_view: Arc<Mutex<HashMap<MetaString, ComponentHealth>>>,
}

impl HealthRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            registered_components: HashSet::new(),
            components: Vec::new(),
            pending_requests: JoinSet::new(),
            probe_scheduler: DelayQueue::new(),
            timeout_dur: DEFAULT_PROBE_TIMEOUT_DUR,
            probe_backoff_dur: DEFAULT_PROBE_BACKOFF_DUR,
            health_view: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registers a component with the registry.
    ///
    /// A handle is returned that must be used by the component to set its readiness as well as respond to liveness
    /// probes. See [`Health::mark_ready`], [`Health::mark_not_ready`], and [`Health::live`] for more information.
    pub fn register_component<S: Into<MetaString>>(&mut self, name: S) -> Option<Health> {
        let name = name.into();

        // Make sure we don't already have this component registered.
        if self.registered_components.contains(&name) {
            return None;
        }

        self.registered_components.insert(name.clone());

        // Create and store the component.
        let id = self.components.len();

        let ready = Arc::new(AtomicBool::new(false));
        let (liveness_tx, liveness_rx) = mpsc::channel(1);

        self.components.push(ComponentState {
            name,
            health: HealthState::Unknown,
            ready: Arc::clone(&ready),
            liveness_tx,
            last_response: Instant::now(),
            last_response_latency: Duration::from_secs(0),
        });

        // Immediately schedule a liveness probe for this component.
        self.probe_scheduler.insert(id, Duration::from_nanos(1));

        Some(Health { ready, liveness_rx })
    }

    /// Gets an API handler for reporting the health of all components.
    ///
    /// This handler can be used to register routes on an [`APIBuilder`][saluki_api::APIBuilder] to expose the health of
    /// all registered components. See [`HealthAPIHandler`] for more information about routes and responses.
    pub fn api_handler(&self) -> HealthAPIHandler {
        HealthAPIHandler::from_state(Arc::clone(&self.health_view))
    }

    async fn spawn_liveness_probe(&mut self, id: usize) {
        let (tx, rx) = oneshot::channel();
        match self.components[id].liveness_tx.send((Instant::now(), tx)).await {
            Ok(()) => {
                // We were able to send the liveness probe request to the component, so spawn our task for waiting for
                // the response.
                let _ = self.pending_requests.spawn(check_liveness(id, self.timeout_dur, rx));
            }

            // If we can't send the liveness probe, then the component is dead or the handle is gone... which are
            // conceptually identical.
            Err(_) => {
                self.components[id].health = HealthState::Dead;
            }
        }
    }

    fn update_component_state(&mut self, id: usize, result: LivenessResult) {
        // Update the component state itself.
        let component = &mut self.components[id];
        component.update_state(result);

        // Update the health view.
        let mut health_view = self.health_view.lock().unwrap();
        let component_health = match health_view.get_mut(&component.name) {
            Some(health) => health,
            None => health_view.entry(component.name.clone()).or_default(),
        };

        component_health.ready = component.is_ready();
        component_health.live = component.is_live();
    }

    /// Runs the health registry, probing components for liveness and maintaining a complete view of the health of all
    /// registered components.
    pub async fn run(mut self) {
        select! {
            // A component has been scheduled to have a liveness probe sent to it.
            Some(entry) = self.probe_scheduler.next() => {
                let component_id = entry.into_inner();
                self.spawn_liveness_probe(component_id).await;
            },

            // A liveness probe has completed.
            Some(probe_result) = self.pending_requests.join_next() => match probe_result {
                Ok(result) => {
                    let component_id = result.id();

                    self.update_component_state(component_id, result);
                    self.probe_scheduler.insert(component_id, self.probe_backoff_dur);
                },

                // In this case, we mean very specifically that the future we spawn to check liveness should, itself,
                // never panic: it does nothing that should lead to a panic. Tokio, likewise, won't indicate that the
                // task panicked due to its own internals: only the future itself panicking when polled or dropped will
                // cause us to hit this path. Thus, we should never hit this path.
                Err(_) => unreachable!("probe task should never panic"),
            },
        }
    }
}

async fn check_liveness(id: usize, timeout_dur: Duration, rx: oneshot::Receiver<(Instant, Instant)>) -> LivenessResult {
    match timeout(timeout_dur, rx).await {
        Ok(Ok((request_sent, response_sent))) => {
            // We're just being a little safer than normal here since `Instant::duration_since` has some scary verbiage
            // about not panicking _now_ but how it _might_ panic in the future... too spooky for me to depend on.
            let response_latency = response_sent.checked_duration_since(request_sent).unwrap_or_default();

            LivenessResult::Success {
                id,
                response_sent,
                response_latency,
            }
        }

        // We got an error when trying to receive, which means the component is dead or the handle is gone... which are
        // conceptually identical.
        Ok(Err(_)) => LivenessResult::Dead { id },

        // We timed out waiting for a response.
        Err(_) => LivenessResult::TimedOut { id },
    }
}
