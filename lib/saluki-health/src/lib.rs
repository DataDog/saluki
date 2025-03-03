use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use metrics::{gauge, histogram, Gauge, Histogram};
use saluki_error::{generic_error, GenericError};
use serde::Serialize;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::mpsc::{self, error::TrySendError},
    task::JoinHandle,
};
use tokio_util::time::{delay_queue::Key, DelayQueue};
use tracing::{debug, info, trace};

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
    component_health: HashMap<MetaString, ComponentHealth>,
    responses_tx: mpsc::Sender<LivenessResponse>,
    responses_rx: Option<mpsc::Receiver<LivenessResponse>>,
    waiting_new_components: Option<mpsc::Sender<usize>>,
}

impl Inner {
    fn new() -> Self {
        let (responses_tx, responses_rx) = mpsc::channel(16);

        Self {
            registered_components: HashSet::new(),
            component_state: Vec::new(),
            component_health: HashMap::new(),
            responses_tx,
            responses_rx: Some(responses_rx),
            waiting_new_components: None,
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
        if inner.registered_components.contains(&name) {
            return None;
        }

        inner.registered_components.insert(name.clone());

        // Create and store the component.
        let component_id = inner.component_state.len();

        let (state, handle) = ComponentState::new(name, inner.responses_tx.clone());
        inner.component_state.push(state);

        // Figure out if we need to notify the health runner about the new component.
        //
        // We do this because a component could be registered after the health runner has already started, and we need
        // to be able to wake up the runner to schedule a liveness probe as soon as possible. If the runner hasn't
        // started, we don't have to do anything, since it will schedule a liveness probe immediately at startup for all
        // components that are registered.
        if let Some(sender) = inner.waiting_new_components.as_mut() {
            // We're in a synchronous call here, but if the runner is, well... running, it should be responding very
            // quickly, so we do a semi-gross thing and do a fallible synchronous send in a loop until it succeeds.
            //
            // Practically, this should always work the very first time unless there's some utter flood of components
            // being registered at the very same moment... in which case, we're probably in a bad place anyway.
            loop {
                if sender.is_closed() || sender.try_send(component_id).is_ok() {
                    break;
                }
            }
        }

        Some(handle)
    }

    /// Gets an API handler for reporting the health of all components.
    ///
    /// This handler can be used to register routes on an [`APIBuilder`][saluki_api::APIBuilder] to expose the health of
    /// all registered components. See [`HealthAPIHandler`] for more information about routes and responses.
    pub fn api_handler(&self) -> HealthAPIHandler {
        HealthAPIHandler::from_state(Arc::clone(&self.inner))
    }

    /// Spawns the health registry runner, which manages the scheduling and collection of liveness probes.
    ///
    /// ## Errors
    ///
    /// If the health registry has already been spawned, an error will be returned.
    pub async fn spawn(self) -> Result<JoinHandle<()>, GenericError> {
        let mut inner = self.inner.lock().unwrap();

        // Make sure the runner hasn't already been spawned.
        if inner.waiting_new_components.is_some() {
            return Err(generic_error!("health registry already spawned"));
        }

        let responses_rx = match inner.responses_rx.take() {
            Some(rx) => rx,
            None => return Err(generic_error!("health registry already spawned")),
        };

        // Install the channel for sending new components to the runner, and prime the runner to fire off liveness
        // probes for all components registered at this point.
        let (new_components_tx, new_components_rx) = mpsc::channel(1);
        inner.waiting_new_components = Some(new_components_tx);

        let mut pending_probes = DelayQueue::new();
        for id in 0..inner.component_state.len() {
            pending_probes.insert(id, Duration::from_nanos(1));
        }

        drop(inner);

        // Create the runner which we'll then spawn.
        let runner = Runner {
            inner: self.inner,
            pending_probes,
            pending_timeouts: DelayQueue::new(),
            responses_rx,
            new_components_rx,
        };

        Ok(tokio::spawn(runner.run()))
    }
}

struct Runner {
    inner: Arc<Mutex<Inner>>,
    pending_probes: DelayQueue<usize>,
    pending_timeouts: DelayQueue<usize>,
    responses_rx: mpsc::Receiver<LivenessResponse>,
    new_components_rx: mpsc::Receiver<usize>,
}

impl Runner {
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
        self.pending_probes.insert(component_id, DEFAULT_PROBE_BACKOFF_DUR);
    }

    fn handle_component_timeout(&mut self, component_id: usize) {
        // Update the component's health to show as not alive.
        self.process_component_health_update(component_id, HealthUpdate::Unknown);

        // Schedule the next probe for this component.
        self.pending_probes.insert(component_id, DEFAULT_PROBE_BACKOFF_DUR);
    }

    fn process_component_health_update(&mut self, component_id: usize, update: HealthUpdate) {
        // Update the component's health state based on the given update.
        let mut inner = self.inner.lock().unwrap();
        {
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

        // Update the shared health map with our component's new health status.
        let (component_name, component_ready, component_live) = {
            let component_state = &inner.component_state[component_id];
            (
                component_state.name.clone(),
                component_state.is_ready(),
                component_state.is_live(),
            )
        };

        let component_health = inner.component_health.entry(component_name).or_default();
        component_health.ready = component_ready;
        component_health.live = component_live;
    }

    async fn run(mut self) {
        info!("Health checker running.");

        // Do an initial component health update for each component to ensure the shared health map is populated.
        let components_len = self.inner.lock().unwrap().component_state.len();
        for component_id in 0..components_len {
            self.process_component_health_update(component_id, HealthUpdate::Unknown);
        }

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

                // A new component has been registered.
                Some(component_id) = self.new_components_rx.recv() => {
                    self.pending_probes.insert(component_id, Duration::from_nanos(1));
                },
            }
        }
    }
}
