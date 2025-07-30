use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use metrics::{gauge, histogram, Gauge, Histogram};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{
        mpsc::{self, error::TrySendError},
        Notify,
    },
    task::JoinHandle,
};
use tokio_util::time::{delay_queue::Key, DelayQueue};
use tracing::{debug, info, trace};

use self::api::HealthAPIHandler;

mod api;

const DEFAULT_PROBE_TIMEOUT_DUR: Duration = Duration::from_secs(5);
const DEFAULT_PROBE_BACKOFF_DUR: Duration = Duration::from_secs(1);

/// A handle for updating the health of a component.
pub struct Health {
    shared: Arc<SharedComponentState>,
    request_rx: mpsc::Receiver<LivenessRequest>,
    response_tx: mpsc::Sender<LivenessResponse>,
}

impl Health {
    /// Marks the component as ready.
    pub fn mark_ready(&mut self) {
        self.update_readiness(true);
    }

    /// Marks the component as not ready.
    pub fn mark_not_ready(&mut self) {
        self.update_readiness(false);
    }

    fn update_readiness(&self, ready: bool) {
        self.shared.ready.store(ready, Relaxed);
        self.shared.telemetry.update_readiness(ready);
    }

    /// Waits for a liveness probe to be sent to the component, and then responds to it.
    ///
    /// This should generally be polled as part of a `select!` block to ensure it is checked alongside other
    /// asynchronous operations.
    pub async fn live(&mut self) {
        // Simply wait for the health registry to send us a liveness probe, and if we receive one, we respond back to it
        // immediately.
        if let Some(request) = self.request_rx.recv().await {
            let response = request.into_response();
            let _ = self.response_tx.send(response).await;
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum HealthState {
    Live,
    Unknown,
    Dead,
}

struct Telemetry {
    component_ready: Gauge,
    component_live: Gauge,
    component_liveness_latency_secs: Histogram,
}

impl Telemetry {
    fn new(name: &str) -> Self {
        let component_id: Arc<str> = Arc::from(name);

        Self {
            component_ready: gauge!("health.component.ready", "component_id" => Arc::clone(&component_id)),
            component_live: gauge!("health.component.alive", "component_id" => Arc::clone(&component_id)),
            component_liveness_latency_secs: histogram!("health.component.liveness_latency_secs", "component_id" => component_id),
        }
    }

    fn update_readiness(&self, ready: bool) {
        self.component_ready.set(if ready { 1.0 } else { 0.0 });
    }

    fn update_liveness(&self, state: HealthState, response_latency: Duration) {
        let live = match state {
            HealthState::Live => 1.0,
            HealthState::Unknown => 0.0,
            HealthState::Dead => -1.0,
        };

        self.component_live.set(live);
        self.component_liveness_latency_secs
            .record(response_latency.as_secs_f64());
    }
}

struct SharedComponentState {
    ready: AtomicBool,
    telemetry: Telemetry,
}

struct ComponentState {
    name: MetaString,
    health: HealthState,
    shared: Arc<SharedComponentState>,
    request_tx: mpsc::Sender<LivenessRequest>,
    last_response: Instant,
    last_response_latency: Duration,
}

impl ComponentState {
    fn new(name: MetaString, response_tx: mpsc::Sender<LivenessResponse>) -> (Self, Health) {
        let shared = Arc::new(SharedComponentState {
            ready: AtomicBool::new(false),
            telemetry: Telemetry::new(&name),
        });
        let (request_tx, request_rx) = mpsc::channel(1);

        let state = Self {
            name,
            health: HealthState::Unknown,
            shared: Arc::clone(&shared),
            request_tx,
            last_response: Instant::now(),
            last_response_latency: Duration::from_secs(0),
        };

        let handle = Health {
            shared,
            request_rx,
            response_tx,
        };

        (state, handle)
    }

    fn is_ready(&self) -> bool {
        // We consider a component ready if it's marked as ready (duh) and it's not dead.
        //
        // Being "dead" is a special case as it means the component is very likely not even running at all, not just
        // responding slowly or deadlocked. In these cases, it can't possibly be ready since it's not even running.
        self.shared.ready.load(Relaxed) && self.health != HealthState::Dead
    }

    fn is_live(&self) -> bool {
        self.health == HealthState::Live
    }

    fn mark_live(&mut self, response_sent: Instant, response_latency: Duration) {
        self.health = HealthState::Live;
        self.last_response = response_sent;
        self.last_response_latency = response_latency;
        self.shared.telemetry.update_liveness(self.health, response_latency);
    }

    fn mark_not_live(&mut self) {
        self.health = HealthState::Unknown;

        // We use the default timeout as the latency for when the component is not considered alive.
        self.shared
            .telemetry
            .update_liveness(self.health, DEFAULT_PROBE_TIMEOUT_DUR);
    }

    fn mark_dead(&mut self) {
        self.health = HealthState::Dead;

        // We use the default timeout as the latency for when the component is not considered alive.
        self.shared
            .telemetry
            .update_liveness(self.health, DEFAULT_PROBE_TIMEOUT_DUR);
    }
}

struct LivenessRequest {
    component_id: usize,
    timeout_key: Key,
    request_sent: Instant,
}

impl LivenessRequest {
    fn new(component_id: usize, timeout_key: Key) -> Self {
        Self {
            component_id,
            timeout_key,
            request_sent: Instant::now(),
        }
    }

    fn into_response(self) -> LivenessResponse {
        LivenessResponse {
            request: self,
            response_sent: Instant::now(),
        }
    }
}

struct LivenessResponse {
    request: LivenessRequest,
    response_sent: Instant,
}

enum HealthUpdate {
    Alive {
        last_response: Instant,
        last_response_latency: Duration,
    },
    Unknown,
    Dead,
}

impl HealthUpdate {
    fn as_str(&self) -> &'static str {
        match self {
            HealthUpdate::Alive { .. } => "alive",
            HealthUpdate::Unknown => "unknown",
            HealthUpdate::Dead => "dead",
        }
    }
}

struct Inner {
    registered_components: HashSet<MetaString>,
    component_state: Vec<ComponentState>,
    responses_tx: mpsc::Sender<LivenessResponse>,
    responses_rx: Option<mpsc::Receiver<LivenessResponse>>,
    pending_components: Vec<usize>,
    pending_components_notify: Arc<Notify>,
}

impl Inner {
    fn new() -> Self {
        let (responses_tx, responses_rx) = mpsc::channel(16);

        Self {
            registered_components: HashSet::new(),
            component_state: Vec::new(),
            responses_tx,
            responses_rx: Some(responses_rx),
            pending_components: Vec::new(),
            pending_components_notify: Arc::new(Notify::new()),
        }
    }
}

/// A registry of components and their health.
///
/// `HealthRegistry` is responsible for tracking the health of all registered components, by storing both their
/// readiness, which indicates whether or not they are initialized and generally ready to process data, as well as
/// probing their liveness, which indicates if they're currently responding, or able to respond, to requests.
///
/// # Telemetry
///
/// The health registry emits some internal telemetry about the status of registered components. In particular, three
/// metrics are emitted:
///
/// - `health.component_ready`: whether or not a component is ready (`gauge`, `0` for not ready, `1` for ready)
/// - `health.component_alive`: whether or not a component is alive (`gauge`, `0` for not alive/unknown, `1` for alive, `-1` for dead)
/// - `health.component_liveness_latency_secs`: the response latency of the component for liveness probes (`histogram`,
///   in seconds)
///
/// All metrics have a `component_id` tag that corresponds to the name of the component that was given when registering it.
#[derive(Clone)]
pub struct HealthRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl HealthRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new())),
        }
    }

    /// Registers a component with the registry.
    ///
    /// A handle is returned that must be used by the component to set its readiness as well as respond to liveness
    /// probes. See [`Health::mark_ready`], [`Health::mark_not_ready`], and [`Health::live`] for more information.
    pub fn register_component<S: Into<MetaString>>(&self, name: S) -> Option<Health> {
        let mut inner = self.inner.lock().unwrap();

        // Make sure we don't already have this component registered.
        let name = name.into();
        if !inner.registered_components.insert(name.clone()) {
            return None;
        }

        // Add the component state.
        let (state, handle) = ComponentState::new(name.clone(), inner.responses_tx.clone());
        let component_id = inner.component_state.len();
        inner.component_state.push(state);

        debug!(component_id, "Registered component '{}'.", name);

        // Mark ourselves as having a pending component that needs to be scheduled.
        inner.pending_components.push(component_id);
        inner.pending_components_notify.notify_one();

        Some(handle)
    }

    /// Gets an API handler for reporting the health of all components.
    ///
    /// This handler can be used to register routes on an [`APIBuilder`][saluki_api::APIBuilder] to expose the health of
    /// all registered components. See [`HealthAPIHandler`] for more information about routes and responses.
    pub fn api_handler(&self) -> HealthAPIHandler {
        HealthAPIHandler::from_state(Arc::clone(&self.inner))
    }

    /// Returns `true` if all components are ready.
    pub fn all_ready(&self) -> bool {
        let inner = self.inner.lock().unwrap();

        for component in &inner.component_state {
            if !component.is_ready() {
                return false;
            }
        }

        true
    }

    /// Spawns the health registry runner, which manages the scheduling and collection of liveness probes.
    ///
    /// # Errors
    ///
    /// If the health registry has already been spawned, an error will be returned.
    pub async fn spawn(self) -> Result<JoinHandle<()>, GenericError> {
        // Make sure the runner hasn't already been spawned.
        let (responses_rx, pending_components_notify) = {
            let mut inner = self.inner.lock().unwrap();
            let responses_rx = match inner.responses_rx.take() {
                Some(rx) => rx,
                None => return Err(generic_error!("health registry already spawned")),
            };

            let pending_components_notify = Arc::clone(&inner.pending_components_notify);
            (responses_rx, pending_components_notify)
        };

        // Create and spawn the runner.
        let runner = Runner {
            inner: self.inner,
            pending_probes: DelayQueue::new(),
            pending_timeouts: DelayQueue::new(),
            responses_rx,
            pending_components_notify,
        };

        Ok(tokio::spawn(runner.run()))
    }
}

struct Runner {
    inner: Arc<Mutex<Inner>>,
    pending_probes: DelayQueue<usize>,
    pending_timeouts: DelayQueue<usize>,
    responses_rx: mpsc::Receiver<LivenessResponse>,
    pending_components_notify: Arc<Notify>,
}

impl Runner {
    fn drain_pending_components(&mut self) -> Vec<usize> {
        // Drain all pending components.
        let mut inner = self.inner.lock().unwrap();
        inner.pending_components.drain(..).collect()
    }

    fn send_component_probe_request(&mut self, component_id: usize) -> Option<HealthUpdate> {
        let mut inner = self.inner.lock().unwrap();
        let component_state = &mut inner.component_state[component_id];

        // Check if our component is already dead, in which case we don't need to send a liveness probe.
        if component_state.request_tx.is_closed() {
            debug!(component_name = %component_state.name, "Component is dead, skipping liveness probe.");
            return Some(HealthUpdate::Dead);
        }

        trace!(component_name = %component_state.name, probe_timeout = ?DEFAULT_PROBE_TIMEOUT_DUR, "Sending liveness probe to component.");

        // Our component _isn't_ dead, so try to send a liveness probe to it.
        //
        // We'll register an entry in `pending_timeouts` that automatically marks the component as not live if we don't
        // receive a response to the liveness probe within the timeout duration.
        let timeout_key = self.pending_timeouts.insert(component_id, DEFAULT_PROBE_TIMEOUT_DUR);

        let request = LivenessRequest::new(component_id, timeout_key);
        if let Err(TrySendError::Closed(request)) = component_state.request_tx.try_send(request) {
            debug!(component_name = %component_state.name, "Component is dead, removing pending timeout.");

            // We failed to send the probe to the component due to the component being dead. We'll drop our pending
            // timeout as we're going to mark this component dead right now.
            //
            // When our send fails due to the channel being full, that's OK: it means it's going to be handled by an
            // existing timeout and will be probed again later.
            self.pending_timeouts.remove(&request.timeout_key);

            return Some(HealthUpdate::Dead);
        }

        None
    }

    fn schedule_probe_for_component(&mut self, component_id: usize, duration: Duration) {
        self.pending_probes.insert(component_id, duration);
    }

    fn handle_component_probe_response(&mut self, response: LivenessResponse) {
        let component_id = response.request.component_id;
        let timeout_key = response.request.timeout_key;
        let request_sent = response.request.request_sent;
        let response_sent = response.response_sent;
        let response_latency = response_sent.checked_duration_since(request_sent).unwrap_or_default();

        // Clear any pending timeouts for this component and schedule the next probe.
        if self.pending_timeouts.try_remove(&timeout_key).is_none() {
            let mut inner = self.inner.lock().unwrap();
            let component_state = &mut inner.component_state[component_id];

            debug!(component_name = %component_state.name, "Received probe response for component that already timed out.");
        }

        // Update the component's health to show as alive.
        let update = HealthUpdate::Alive {
            last_response: response_sent,
            last_response_latency: response_latency,
        };
        self.process_component_health_update(component_id, update);

        // Schedule the next probe for this component.
        self.schedule_probe_for_component(component_id, DEFAULT_PROBE_BACKOFF_DUR);
    }

    fn handle_component_timeout(&mut self, component_id: usize) {
        // Update the component's health to show as not alive.
        self.process_component_health_update(component_id, HealthUpdate::Unknown);

        // Schedule the next probe for this component.
        self.schedule_probe_for_component(component_id, DEFAULT_PROBE_BACKOFF_DUR);
    }

    fn process_component_health_update(&mut self, component_id: usize, update: HealthUpdate) {
        // Update the component's health state based on the given update.
        let mut inner = self.inner.lock().unwrap();
        let component_state = &mut inner.component_state[component_id];
        trace!(component_name = %component_state.name, status = update.as_str(), "Updating component health status.");

        match update {
            HealthUpdate::Alive {
                last_response,
                last_response_latency,
            } => component_state.mark_live(last_response, last_response_latency),
            HealthUpdate::Unknown => component_state.mark_not_live(),
            HealthUpdate::Dead => component_state.mark_dead(),
        }
    }

    async fn run(mut self) {
        info!("Health checker running.");

        loop {
            select! {
                // A component has been scheduled to have a liveness probe sent to it.
                Some(entry) = self.pending_probes.next() => {
                    let component_id = entry.into_inner();
                    if let Some(health_update) = self.send_component_probe_request(component_id) {
                        // If we got a health update for this component, that means we detected that it's dead, so we need
                        // to do an out-of-band update to its health.
                        self.process_component_health_update(component_id, health_update);
                    }
                },

                // A component's outstanding liveness probe has expired.
                Some(entry) = self.pending_timeouts.next() => {
                    let component_id = entry.into_inner();
                    self.handle_component_timeout(component_id);
                },

                // A probe response has been received.
                Some(response) = self.responses_rx.recv() => {
                    self.handle_component_probe_response(response);
                },

                // A component is pending finalization of their registration.
                _ = self.pending_components_notify.notified() => {
                    // Drain all pending components, give them a clean initial state of "unknown", and immediately schedule a probe for them.
                    let pending_component_ids = self.drain_pending_components();
                    for pending_component_id in pending_component_ids {
                        self.process_component_health_update(pending_component_id, HealthUpdate::Unknown);
                        self.schedule_probe_for_component(pending_component_id, Duration::from_nanos(1));
                    }
                },
            }
        }
    }
}
