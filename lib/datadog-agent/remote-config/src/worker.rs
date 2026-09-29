use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, Supervisable, SupervisorFuture};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::util::retry::ExponentialBackoff;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::metrics::{Metrics, PollOutcome};
use crate::registry::Shared;
use crate::repository::Repository;
use crate::source::{FetchError, RcAgent};
use crate::RcClientConfiguration;

/// How often the worker retries until its first successful poll, because some products may not start correctly without
/// a first state.
const FIRST_POLL_RETRY: Duration = Duration::from_secs(1);

/// Drives polling and delivery for a [`RemoteConfigurationClient`](crate::RemoteConfigurationClient).
///
/// A supervisor can own and restart the worker, or the caller can drive it directly with [`run`](Self::run).
///
/// The worker does not exit on connection failures, on the Agent having remote configuration disabled, on the Agent
/// reporting its configuration expired, or on a panic in a subscriber's decoder. It keeps polling through all of them,
/// so it exits only on a failure in the client itself.
///
/// A restart keeps every subscription and each product's last accepted snapshot, and discards the protocol state, so
/// the restarted worker fetches and decodes everything again. Subscribers may therefore see a snapshot equal to the one
/// they already hold.
///
/// Everything that happens outside a subscriber's view (polls that fail, responses that cannot be applied,
/// configurations the client rejects) is counted in the client's own metrics, since subscribers cannot see it.
pub struct RemoteConfigurationWorker {
    /// The subscriptions and client ID, shared with every client handle and kept across restarts.
    pub(crate) shared: Arc<Shared>,

    /// The Agent connection, kept across restarts and locked by the one running poll loop.
    agent: Arc<Mutex<Box<dyn RcAgent>>>,

    config: RcClientConfiguration,
}

impl RemoteConfigurationWorker {
    pub(crate) fn new(shared: Arc<Shared>, agent: Box<dyn RcAgent>, config: RcClientConfiguration) -> Self {
        Self {
            shared,
            agent: Arc::new(Mutex::new(agent)),
            config,
        }
    }

    /// Runs the worker without a supervisor.
    ///
    /// This polls until the returned future is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error only on a failure in the client itself. Connection failures, remote configuration being
    /// disabled on the Agent, and rejected or panicking decoders are handled without returning.
    pub async fn run(self) -> Result<(), GenericError> {
        poll_loop(self.shared, self.agent, self.config, ShutdownHandle::noop()).await
    }
}

#[async_trait]
impl Supervisable for RemoteConfigurationWorker {
    fn name(&self) -> &str {
        "remote-config"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        Ok(Box::pin(poll_loop(
            Arc::clone(&self.shared),
            Arc::clone(&self.agent),
            self.config.clone(),
            process_shutdown,
        )))
    }
}

/// Polls until `shutdown` resolves, starting from fresh protocol state.
async fn poll_loop(
    shared: Arc<Shared>, agent: Arc<Mutex<Box<dyn RcAgent>>>, config: RcClientConfiguration, shutdown: ShutdownHandle,
) -> Result<(), GenericError> {
    let mut shutdown = pin!(shutdown);
    let mut agent = tokio::select! {
        _ = &mut shutdown => return Ok(()),
        agent = agent.lock() => agent,
    };
    let mut repository = Repository::new();
    let mut schedule = Schedule::new(&config);
    let metrics = Metrics::new();
    // The reference point until the first success, so the gauge measures how long the worker has gone without a
    // successful poll from the moment it started.
    let mut last_success = tokio::time::Instant::now();

    loop {
        // This poll includes every subscription made so far, so a wake-up already pending for one of them is spent.
        tokio::select! {
            biased;
            _ = shared.wake.notified() => {}
            _ = std::future::ready(()) => {}
        }

        let live = shared.live_products();
        repository.prune(&live);
        let request = repository.request(&shared.client_id, &config, &live);
        let result = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            result = tokio::time::timeout(config.request_timeout, agent.get_configs(request)) => {
                result.unwrap_or_else(|_| {
                    Err(FetchError::Rpc(generic_error!(
                        "The Agent did not answer within {:?}.",
                        config.request_timeout
                    )))
                })
            }
        };

        let (outcome, delay) = match result {
            Ok(response) => match repository.apply(response, &live, &shared, &metrics) {
                Ok(outcome) => {
                    repository.last_error = None;
                    (outcome, schedule.succeeded())
                }
                Err(e) => {
                    error!(error = %e, "Discarded an invalid Remote Configuration response.");
                    repository.last_error = Some(e.to_string());
                    (PollOutcome::InvalidResponse, schedule.failed())
                }
            },
            Err(FetchError::Unimplemented(e)) => (PollOutcome::Unimplemented, schedule.unimplemented(&e)),
            Err(FetchError::Rpc(e)) => {
                if schedule.rpc_failing {
                    debug!(error = %e, "Failed to poll the Agent for Remote Configuration.");
                } else {
                    warn!(error = %e, "Failed to poll the Agent for Remote Configuration.");
                }
                schedule.rpc_failing = true;
                repository.last_error = Some(e.to_string());
                (PollOutcome::RpcError, schedule.failed())
            }
        };
        metrics.count_poll(outcome);
        if matches!(outcome, PollOutcome::Ok | PollOutcome::Expired) {
            last_success = tokio::time::Instant::now();
        }
        metrics.set_seconds_since_successful_poll(last_success.elapsed().as_secs());

        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
            _ = shared.wake.notified() => {}
        }
    }
}

/// Decides how long to wait before the next poll.
struct Schedule {
    poll_interval: Duration,
    max_backoff: Duration,
    backoff: ExponentialBackoff,
    succeeded_once: bool,
    failures: u32,

    /// Whether an RPC has failed since the last success, so that only the first such failure warns. Unlike
    /// `failures`, an invalid response does not set it.
    rpc_failing: bool,

    unimplemented: bool,
}

impl Schedule {
    fn new(config: &RcClientConfiguration) -> Self {
        Self {
            poll_interval: config.poll_interval,
            max_backoff: config.max_backoff,
            backoff: ExponentialBackoff::with_jitter(config.poll_interval, config.max_backoff, 2.0),
            succeeded_once: false,
            failures: 0,
            rpc_failing: false,
            unimplemented: false,
        }
    }

    fn succeeded(&mut self) -> Duration {
        if self.unimplemented {
            info!("Remote Configuration is enabled on the Agent; polling resumed.");
        } else if self.failures > 0 {
            info!(
                failures = self.failures,
                "Polling the Agent for Remote Configuration recovered."
            );
        }
        self.succeeded_once = true;
        self.failures = 0;
        self.rpc_failing = false;
        self.unimplemented = false;
        self.poll_interval
    }

    fn failed(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        if self.succeeded_once {
            self.backoff.get_backoff_duration(self.failures)
        } else {
            FIRST_POLL_RETRY
        }
    }

    /// Remote Configuration is disabled on the Agent, which is expected to last, so the worker keeps checking slowly.
    fn unimplemented(&mut self, error: &GenericError) -> Duration {
        if !self.unimplemented {
            info!(error = %error, "Remote Configuration is not enabled on the Agent; checking again periodically.");
        }
        self.unimplemented = true;
        self.max_backoff
    }
}
