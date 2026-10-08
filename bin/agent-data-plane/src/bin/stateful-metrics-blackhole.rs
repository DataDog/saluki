//! Ack-only stateful metrics intake for load tests.
//!
//! Acknowledges every `StatefulStream` batch as it arrives, without decompressing or decoding it, so
//! the intake adds as little CPU and memory as possible next to ADP. It doesn't validate payloads;
//! use the Foldspace reference intake for that.
//!
//! `LISTEN_ADDR` sets the listen address (default `127.0.0.1:9201`).

use std::{env, net::SocketAddr};

use foldspace_core::proto::stateful::{
    batch_status,
    stateful_intake_server::{StatefulIntake, StatefulIntakeServer},
    BatchStatus, StatefulBatch, StatelessRequest, StatelessResponse,
};
use futures::{stream::BoxStream, StreamExt as _};
use tonic::{transport::Server, Request, Response, Status, Streaming};

struct AckIntake;

#[tonic::async_trait]
impl StatefulIntake for AckIntake {
    type StatefulStreamStream = BoxStream<'static, Result<BatchStatus, Status>>;

    async fn stateful_stream(
        &self, request: Request<Streaming<StatefulBatch>>,
    ) -> Result<Response<Self::StatefulStreamStream>, Status> {
        let acks = request.into_inner().map(|batch| {
            batch.map(|batch| BatchStatus {
                batch_id: batch.batch_id,
                status: batch_status::Status::Ok.into(),
            })
        });
        Ok(Response::new(acks.boxed()))
    }

    async fn stateless(&self, _: Request<StatelessRequest>) -> Result<Response<StatelessResponse>, Status> {
        Err(Status::unimplemented("stateless delivery is not supported"))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9201".to_string())
        .parse()?;
    Server::builder()
        .add_service(StatefulIntakeServer::new(AckIntake).max_decoding_message_size(usize::MAX))
        .serve(addr)
        .await?;
    Ok(())
}
