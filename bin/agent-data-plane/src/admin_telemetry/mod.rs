use std::{
    pin::Pin,
    result::Result,
    sync::Arc,
    task::{ready, Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt as _;
use memory_accounting::allocator::AllocationGroupRegistry;
use papaya::HashMap;
use pin_project_lite::pin_project;
use saluki_core::{observability::metrics::MetricsStream, task::spawn_traced};
use saluki_event::metric::{Metric, MetricValues, ScalarPoints};
use stringtheory::MetaString;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, Stream};
use tonic::{async_trait, Request, Response, Status};
use tonic_web::CorsGrpcWeb;
use tracing::trace;

mod proto;
use self::proto::generated::datadog::adp::v1::telemetry::{
    telemetry_service_server::{TelemetryService, TelemetryServiceServer},
    *,
};

struct TelemetryState {
    process_info: ProcessInformation,
    aggregated_metrics: HashMap<MetaString, f64>,
    updates_tx: broadcast::Sender<MetricSnapshot>,
}

impl TelemetryState {
    fn update_aggregated_metric(&self, metric: &ProcessedMetric<'_>) {
        self.aggregated_metrics.pin().update_or_insert_with(
            metric.name.clone(),
            |value| *value + metric.value,
            || metric.value,
        );
    }
}

pub fn initialize_telemetry_service() -> CorsGrpcWeb<TelemetryServiceServer<TelemetryServerImpl>> {
    let (updates_tx, _) = broadcast::channel(4);
    let state = Arc::new(TelemetryState {
        process_info: ProcessInformation::new(),
        aggregated_metrics: HashMap::new(),
        updates_tx,
    });

    // Spawn our telemetry collector.
    let allocation_token =
        AllocationGroupRegistry::global().register_allocation_group("internal.admin_telemetry_collector");
    let _enter = allocation_token.enter();

    spawn_traced(run_telemetry_collector(state.clone()));

    // Spawn our telemetry server.
    //
    // TODO: Write a middleware layer to enter/exit the allocation group for a service.
    //let allocation_token =
    //    AllocationGroupRegistry::global().register_allocation_group("internal.admin_telemetry_server");
    //let _enter = allocation_token.enter();

    build_telemetry_server(state)
}

async fn run_telemetry_collector(state: Arc<TelemetryState>) {
    let mut metrics_stream = MetricsStream::register();

    while let Some(metrics) = metrics_stream.next().await {
        let now = unix_timestamp_utc();

        // Build a new snapshot that we'll fill with the latest metrics.
        let mut snapshot = MetricSnapshot {
            unix_timestamp_utc: now,
            metrics: std::collections::HashMap::new(),
        };

        // Process each incoming metric, and if it's one we should be tracking, add it to the snapshot and
        // update it in our aggregated metrics.
        for metric in metrics.iter() {
            if let Some(processed_metric) = metric.try_as_metric().and_then(process_metric) {
                state.update_aggregated_metric(&processed_metric);
                snapshot
                    .metrics
                    .insert(processed_metric.name.clone().into_owned(), processed_metric.value);
            }
        }

        // Send the snapshot to all connected clients.
        let connected_clients = state.updates_tx.receiver_count();
        if connected_clients > 0 {
            trace!("Sending snapshot to {} client(s).", connected_clients);

            if state.updates_tx.send(snapshot).is_err() {
                trace!("No connected clients present when sending incremental update. Likely disconnected mid-update.");
            }
        } else {
            trace!("No connected clients present. Skipping incremental update.");
        }
    }
}

fn build_telemetry_server(state: Arc<TelemetryState>) -> CorsGrpcWeb<TelemetryServiceServer<TelemetryServerImpl>> {
    let svc = TelemetryServiceServer::new(TelemetryServerImpl { state });

    tonic_web::enable(svc)
}

struct ProcessedMetric<'a> {
    name: &'a MetaString,
    value: f64,
}

fn process_metric(metric: &Metric) -> Option<ProcessedMetric<'_>> {
    // TODO: In the future, this is where we would do remapper-esque logic to both filter out metrics we don't want and
    // remap their full context (name + tags) to a single user-friendly name for the response payload.
    let value = match metric.values() {
        MetricValues::Counter(points) => get_aggregated_counter_value(points),
        MetricValues::Gauge(points) => get_aggregated_gauge_value(points),
        _ => None,
    }?;

    Some(ProcessedMetric {
        name: metric.context().name(),
        value,
    })
}

fn get_aggregated_counter_value(points: &ScalarPoints) -> Option<f64> {
    // Aggregate all points that don't have a timestamp.
    let mut total = None;
    for (ts, point) in points {
        if ts.is_none() {
            let total = total.get_or_insert(0.0);
            *total += point;
        }
    }

    total
}

fn get_aggregated_gauge_value(points: &ScalarPoints) -> Option<f64> {
    // Take the latest point that doesn't have a timestamp.
    let mut latest = None;
    for (ts, point) in points {
        if ts.is_none() {
            latest = Some(point);
        }
    }

    latest
}

#[derive(Clone)]
struct ProcessInformation {
    process_id: u32,
    started: Instant,
}

impl ProcessInformation {
    fn new() -> Self {
        Self {
            process_id: std::process::id(),
            started: Instant::now(),
        }
    }

    fn process_id(&self) -> u32 {
        self.process_id
    }

    fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn rss_bytes(&self) -> usize {
        let mut querier = process_memory::Querier::default();
        querier.resident_set_size().unwrap_or(0)
    }
}

pub struct TelemetryServerImpl {
    state: Arc<TelemetryState>,
}

#[async_trait]
impl TelemetryService for TelemetryServerImpl {
    async fn get_process_information(
        &self, _: Request<GetProcessInformationRequest>,
    ) -> Result<Response<GetProcessInformationResponse>, Status> {
        Ok(Response::new(GetProcessInformationResponse {
            pid: self.state.process_info.process_id(),
            uptime_secs: self.state.process_info.uptime_secs(),
            rss_bytes: self.state.process_info.rss_bytes() as u64,
        }))
    }

    async fn get_aggregated_metrics(
        &self, _: Request<GetAggregatedMetricsRequest>,
    ) -> Result<Response<GetAggregatedMetricsResponse>, Status> {
        // Implement the logic for get_aggregated_metrics here
        unimplemented!()
    }

    type GetLatestMetricsStream = LatestMetricsBroadcastStream;

    async fn get_latest_metrics(
        &self, _: Request<GetLatestMetricsRequest>,
    ) -> Result<Response<Self::GetLatestMetricsStream>, Status> {
        Ok(Response::new(LatestMetricsBroadcastStream {
            inner: BroadcastStream::new(self.state.updates_tx.subscribe()),
        }))
    }
}

fn unix_timestamp_utc() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(n) => n.as_secs(),
        Err(_) => 0,
    }
}

pin_project! {
    pub struct LatestMetricsBroadcastStream {
        #[pin]
        inner: BroadcastStream<MetricSnapshot>,
    }
}

impl Stream for LatestMetricsBroadcastStream {
    type Item = Result<GetLatestMetricsResponse, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            // Loop until we get a real result, as we don't care about knowing when we've lagged.
            match ready!(this.inner.as_mut().poll_next(cx)) {
                Some(Ok(snapshot)) => {
                    return Poll::Ready(Some(Ok(GetLatestMetricsResponse {
                        snapshot: Some(snapshot),
                    })));
                }
                Some(Err(_)) => {
                    trace!("Metrics snapshot receiver lagged. Metrics may be missing for client. Continuing...");
                    continue;
                }
                None => return Poll::Ready(None),
            }
        }
    }
}
