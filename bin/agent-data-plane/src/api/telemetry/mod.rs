use std::{
    collections::HashMap, pin::Pin, result::Result, sync::Arc, task::{ready, Context, Poll}, time::{Instant, SystemTime, UNIX_EPOCH}
};

use futures::Stream;
use saluki_core::state::reflector::Reflector;
use tokio_util::sync::ReusableBoxFuture;
use tonic::{async_trait, Request, Response, Status};
use tonic_web::CorsGrpcWeb;

mod proto;
use crate::state::metrics::AggregatedMetricsProcessor;

use self::proto::generated::datadog::adp::v1::telemetry::{
    telemetry_service_server::{TelemetryService, TelemetryServiceServer},
    *,
};

struct TelemetryState {
    process_info: ProcessInformation,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
}

impl TelemetryState {
    fn get_metrics_ingested(&self) -> f64 {
        self.internal_metrics.state()
            .get_aggregated_with_tags("adp.component_events_received_total", &["component_id:dsd_in"])
    }
}

pub fn build_telemetry_service(internal_metrics: Reflector<AggregatedMetricsProcessor>) -> CorsGrpcWeb<TelemetryServiceServer<TelemetryServerImpl>> {
    let state = Arc::new(TelemetryState {
        process_info: ProcessInformation::new(),
        internal_metrics,
    });

    // TODO: Write a middleware layer to enter/exit the allocation group for a service.
    build_telemetry_server(state)
}

fn build_telemetry_server(state: Arc<TelemetryState>) -> CorsGrpcWeb<TelemetryServiceServer<TelemetryServerImpl>> {
    let svc = TelemetryServiceServer::new(TelemetryServerImpl { state });

    tonic_web::enable(svc)
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
            metrics_ingested: self.state.get_metrics_ingested() as u64,
            logs_ingested: 0,
            traces_ingested: 0,
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
        Ok(Response::new(LatestMetricsBroadcastStream::from_reflector(self.state.internal_metrics.clone())))
    }
}

fn unix_timestamp_utc() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(n) => n.as_secs(),
        Err(_) => 0,
    }
}

pub struct LatestMetricsBroadcastStream {
    inner: ReusableBoxFuture<'static, (MetricSnapshot, Reflector<AggregatedMetricsProcessor>)>,
}

impl LatestMetricsBroadcastStream {
    fn from_reflector(internal_metrics: Reflector<AggregatedMetricsProcessor>) -> Self {
        Self {
            inner: ReusableBoxFuture::new(make_latest_metrics_future(internal_metrics)),
        }
    }
}

impl Stream for LatestMetricsBroadcastStream {
    type Item = Result<GetLatestMetricsResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (snapshot, internal_metrics) = ready!(self.inner.poll(cx));

        self.inner.set(make_latest_metrics_future(internal_metrics));

        Poll::Ready(Some(Ok(GetLatestMetricsResponse { snapshot: Some(snapshot) })))
    }
}

async fn make_latest_metrics_future(internal_metrics: Reflector<AggregatedMetricsProcessor>) -> (MetricSnapshot, Reflector<AggregatedMetricsProcessor>) {
    internal_metrics.wait_for_update().await;

    let mut snapshot = MetricSnapshot {
        unix_timestamp_utc: unix_timestamp_utc(),
        metrics: HashMap::new(),
    };

    internal_metrics.state().visit_metrics(|context, value| {
        snapshot.metrics.insert(context.name().as_ref().to_owned(), value.value());
    });

    (snapshot, internal_metrics)
}
