use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use metrics::Level;
use saluki_api::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{post, Router},
    APIHandler, DynamicRoute, EndpointType, StatusCode,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::{
    observability::metrics::FilterHandle,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
};
use saluki_error::generic_error;
use serde::Deserialize;
use tokio::{
    pin, select,
    sync::{mpsc, Mutex},
    time::sleep,
};
use tracing::info;

const MAXIMUM_OVERRIDE_LENGTH_SECS: u64 = 60 * 60;

#[derive(Deserialize)]
struct OverrideQueryParams {
    time_secs: u64,
}

/// State used for the metrics API handler.
#[derive(Clone)]
pub struct MetricsHandlerState {
    override_tx: mpsc::Sender<Option<(Duration, Level)>>,
}

/// An API handler for updating metric filtering directives at runtime.
///
/// This handler exposes two main routes -- `/metrics/override` and `/metrics/reset` -- which allow for
/// overriding the default metric filtering directives (configured at startup) at runtime, and then resetting them once
/// the override is no longer needed.
///
/// As this has the potential for incredibly high metrics cardinality at runtime, the override is set with a specific
/// duration in which it will apply. Once an override has been active for the configured duration, it will automatically
/// be reset unless the override is refreshed before the duration elapses.
///
/// The maximum duration for an override is 60 minutes.
pub struct MetricsAPIHandler {
    state: MetricsHandlerState,
}

impl MetricsAPIHandler {
    async fn override_handler(
        State(state): State<MetricsHandlerState>, params: Query<OverrideQueryParams>, body: String,
    ) -> impl IntoResponse {
        // Make sure the override length is within the acceptable range.
        if params.time_secs > MAXIMUM_OVERRIDE_LENGTH_SECS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "override time cannot be greater than {} seconds",
                    MAXIMUM_OVERRIDE_LENGTH_SECS
                ),
            );
        }

        // Parse the override duration and create a new filter from the body.
        let duration = Duration::from_secs(params.time_secs);
        let new_level = match Level::try_from(body.as_str()) {
            Ok(level) => level,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("failed to parse override filter: {}", e),
                )
            }
        };

        // Instruct the override processor to apply the new metric filtering directives for the given duration.
        let _ = state.override_tx.send(Some((duration, new_level))).await;

        (StatusCode::OK, "acknowledged".to_string())
    }

    async fn reset_handler(State(state): State<MetricsHandlerState>) {
        // Instruct the override processor to immediately reset back to the original metric filtering directives.
        let _ = state.override_tx.send(None).await;
    }
}

impl APIHandler for MetricsAPIHandler {
    type State = MetricsHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/metrics/override", post(Self::override_handler))
            .route("/metrics/reset", post(Self::reset_handler))
    }
}

/// A worker that processes dynamic metric filter override requests.
///
/// When running, the worker asserts a set of routes (based on [`MetricsAPIHandler`]) that allow triggering
/// an override of the current metrics filter directives as well as clearing (resetting) the active override.
pub struct MetricsOverrideWorker {
    handler: MetricsAPIHandler,
    state: Arc<Mutex<MetricsOverrideWorkerState>>,
}

struct MetricsOverrideWorkerState {
    filter_handle: FilterHandle,
    override_rx: mpsc::Receiver<Option<(Duration, Level)>>,
}

impl MetricsOverrideWorker {
    /// Creates a new `MetricsOverrideWorker` driving the given filter handle.
    pub(super) fn new(filter_handle: FilterHandle) -> Self {
        let (override_tx, override_rx) = mpsc::channel(1);
        let handler = MetricsAPIHandler {
            state: MetricsHandlerState { override_tx },
        };
        Self {
            handler,
            state: Arc::new(Mutex::new(MetricsOverrideWorkerState {
                filter_handle,
                override_rx,
            })),
        }
    }
}

#[async_trait]
impl Supervisable for MetricsOverrideWorker {
    fn name(&self) -> &str {
        "dynamic-metrics-override-processor"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let mut state = self.state.clone().lock_owned().await;
        let metrics_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(metrics_route, "metrics-api");

            process_override_requests(&mut state, process_shutdown).await;

            Ok(())
        }))
    }
}

async fn process_override_requests(state: &mut MetricsOverrideWorkerState, process_shutdown: ShutdownHandle) {
    let mut override_active = false;
    let override_timeout = sleep(Duration::MAX);

    pin!(override_timeout, process_shutdown);

    loop {
        select! {
            _ = &mut process_shutdown => break,
            maybe_override = state.override_rx.recv() => match maybe_override {
                Some(Some((duration, new_level))) => {
                    // TODO: Using the `Debug` representation of `Level` is noisy, and we should add a method upstream to
                    // just get the stringified representation of the level instead.
                    info!(level = ?new_level, "Overriding existing metric filtering directive for {} seconds...", duration.as_secs());

                    state.filter_handle.override_filter(new_level);

                    // Mark ourselves as having an active override and update the override timeout.
                    override_active = true;
                    override_timeout.as_mut().reset(tokio::time::Instant::now() + duration);
                },

                Some(None) => {
                    // We've been instructed to immediately reset the filter back to the original one, so simply update
                    // the override timeout to fire as soon as possible.
                    override_timeout.as_mut().reset(tokio::time::Instant::now());
                },

                // Our sender has dropped, so there's no more override requests for us to handle.
                None => break,
            },
            _ = &mut override_timeout => {
                // Our override timeout has fired. If we have an active override, reset it.
                //
                // Otherwise, this just means that we've been running for a while without any overrides, so we can just
                // reset the timeout with a long duration.
                if override_active {
                    override_active = false;

                    state.filter_handle.reset_filter();

                    info!("Restored original metric filtering directive.");
                }

                override_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(MAXIMUM_OVERRIDE_LENGTH_SECS));
            }
        }
    }
}

// This module mirrors the well-tested override/reset worker in `logging/api.rs`. Unlike the logging worker -- whose
// effect is observable through a locally-constructed `tracing` reload handle -- the metrics worker's only observable
// effect is on a `FilterHandle`, which is created by installing the process-global metrics recorder. Because that
// recorder can only be installed once per process, all of the worker's behaviors are exercised in a single test,
// observed end-to-end through the real metrics pipeline.
#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use metrics::{counter, Key};
    use saluki_core::observability::metrics::{delete_counter, initialize_metrics, MetricsStream};

    use super::*;

    // Emits a DEBUG-level counter and immediately evicts it, returning whether the resulting snapshot carried the
    // counter as an upsert -- i.e. whether a DEBUG-level metric currently passes the runtime filter level.
    async fn debug_metric_passes_filter(stream: &mut MetricsStream, name: &'static str) -> bool {
        counter!(level: Level::DEBUG, name).increment(1);
        assert!(
            delete_counter(Key::from_name(name)),
            "the counter should exist before eviction"
        );

        let snapshot = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("a snapshot should be produced after eviction")
            .expect("the metrics stream should not close");

        !snapshot.upserts.is_empty()
    }

    async fn wait_until_filter_passes(stream: &mut MetricsStream, name: &'static str, expected: bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

        loop {
            if debug_metric_passes_filter(stream, name).await == expected {
                return;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "filter never reached `passes={expected}`"
            );

            sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn override_reset_and_expiration_toggle_the_runtime_filter_level() {
        let (filter_handle, _flusher) = initialize_metrics("test".to_string(), Level::INFO)
            .await
            .expect("metrics subsystem should initialize");

        let (override_tx, override_rx) = mpsc::channel(1);
        let mut state = MetricsOverrideWorkerState {
            filter_handle,
            override_rx,
        };
        let processor = tokio::spawn(async move {
            process_override_requests(&mut state, ShutdownHandle::noop()).await;
        });

        let mut stream = MetricsStream::register();
        const PROBE: &str = "override_reset_probe";

        // With the default INFO filter, a DEBUG-level metric is filtered out.
        assert!(!debug_metric_passes_filter(&mut stream, PROBE).await);

        // Overriding to DEBUG lets the DEBUG-level metric through.
        override_tx
            .send(Some((Duration::from_secs(60), Level::DEBUG)))
            .await
            .expect("send override");
        wait_until_filter_passes(&mut stream, PROBE, true).await;

        // Resetting restores the INFO default, filtering the DEBUG-level metric out again.
        override_tx.send(None).await.expect("send reset");
        wait_until_filter_passes(&mut stream, PROBE, false).await;

        // A time-boxed override auto-expires and restores the default filter without an explicit reset.
        override_tx
            .send(Some((Duration::from_millis(250), Level::DEBUG)))
            .await
            .expect("send time-boxed override");
        wait_until_filter_passes(&mut stream, PROBE, true).await;
        wait_until_filter_passes(&mut stream, PROBE, false).await;

        // Dropping the controller side makes the worker exit cleanly.
        drop(override_tx);
        processor.await.expect("override processor should exit cleanly");
    }
}
