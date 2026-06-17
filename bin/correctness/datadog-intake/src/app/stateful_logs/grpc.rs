use std::pin::Pin;

use datadog_protos::stateful::{
    batch_status, stateful_logs_service_server::StatefulLogsService, BatchStatus, StatefulBatch as ProtoStatefulBatch,
};
use futures::Stream;
use tonic::{Request, Response, Status};
use tracing::{debug, info};

use super::StatefulLogsState;

/// gRPC implementation for the stateful logs correctness intake.
#[derive(Clone)]
pub struct StatefulLogsGrpcService {
    state: StatefulLogsState,
}

impl StatefulLogsGrpcService {
    /// Creates a new stateful logs gRPC service backed by the given shared state.
    pub const fn new(state: StatefulLogsState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl StatefulLogsService for StatefulLogsGrpcService {
    type LogsStreamStream = Pin<Box<dyn Stream<Item = Result<BatchStatus, Status>> + Send>>;

    async fn logs_stream(
        &self, request: Request<tonic::Streaming<ProtoStatefulBatch>>,
    ) -> Result<Response<Self::LogsStreamStream>, Status> {
        info!("Accepted stateful logs stream.");

        let state = self.state.clone();
        let mut inbound = request.into_inner();
        let outbound = async_stream::try_stream! {
            while let Some(batch) = inbound.message().await? {
                let batch_id = batch.batch_id;
                debug!(batch_id, "Received stateful logs batch.");
                state.decode_batch(&batch).map_err(Status::invalid_argument)?;
                yield BatchStatus {
                    batch_id,
                    status: batch_status::Status::Ok as i32,
                };
            }
        };

        Ok(Response::new(Box::pin(outbound)))
    }
}
