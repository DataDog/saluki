//! Drive one gRPC stream. Delivery recovery belongs to the owning sender worker.

use std::{collections::VecDeque, future::pending};

use foldspace_core::{
    proto::stateful::{stateful_intake_client::StatefulIntakeClient, BatchStatus, StatefulBatch},
    StreamId,
};
use futures::{future::BoxFuture, FutureExt as _};
use tokio::{
    select,
    sync::mpsc::{self, error::TrySendError},
    time::timeout,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::Endpoint,
    Request, Status, Streaming,
};

use super::{CONNECT_TIMEOUT, MAX_INFLIGHT_BATCHES};

const REQUESTED_STATE_BYTES: &str = "5242880";

pub(super) struct Transport {
    pub stream_id: StreamId,
    pub sender: mpsc::Sender<StatefulBatch>,
    pub state: TransportState,
    pub pending: VecDeque<StatefulBatch>,
}

pub(super) enum TransportState {
    Connecting(BoxFuture<'static, Result<Streaming<BatchStatus>, Status>>),
    Open(Box<Streaming<BatchStatus>>),
}

pub(super) struct TransportEvent {
    pub stream_id: StreamId,
    pub kind: TransportEventKind,
}

pub(super) enum TransportEventKind {
    Opened(Box<Streaming<BatchStatus>>),
    Ack(BatchStatus),
    Failed(Status),
}

impl Transport {
    pub fn open(stream_id: StreamId, endpoint: Endpoint, api_key: MetadataValue<Ascii>) -> Self {
        let (sender, receiver) = mpsc::channel(MAX_INFLIGHT_BATCHES + 1);
        let mut request = Request::new(ReceiverStream::new(receiver));
        request.metadata_mut().insert("dd-api-key", api_key);
        request
            .metadata_mut()
            .insert("dd-content-encoding", MetadataValue::from_static("zstd"));
        request.metadata_mut().insert(
            "dd-state-request-bytes",
            MetadataValue::from_static(REQUESTED_STATE_BYTES),
        );
        let opening = async move {
            timeout(CONNECT_TIMEOUT, async move {
                let channel = endpoint
                    .connect()
                    .await
                    .map_err(|_| Status::unavailable("connection failed"))?;
                StatefulIntakeClient::new(channel)
                    .stateful_stream(request)
                    .await
                    .map(|response| response.into_inner())
            })
            .await
            .unwrap_or_else(|_| Err(Status::deadline_exceeded("opening stream timed out")))
        }
        .boxed();
        Self {
            stream_id,
            sender,
            state: TransportState::Connecting(opening),
            pending: VecDeque::new(),
        }
    }

    pub fn send(&mut self, payload: StatefulBatch) -> Result<(), Status> {
        if !self.pending.is_empty() {
            self.pending.push_back(payload);
            return Ok(());
        }
        match self.sender.try_send(payload) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(payload)) => {
                // The core's inflight window bounds this backlog; never wait here and starve ACKs.
                self.pending.push_back(payload);
                Ok(())
            }
            Err(TrySendError::Closed(_)) => Err(Status::unavailable("outbound stream closed")),
        }
    }

    async fn next_event(&mut self) -> TransportEvent {
        let kind = match &mut self.state {
            TransportState::Connecting(opening) => match opening.await {
                Ok(stream) => TransportEventKind::Opened(Box::new(stream)),
                Err(status) => TransportEventKind::Failed(status),
            },
            TransportState::Open(stream) => loop {
                select! {
                    permit = self.sender.reserve(), if !self.pending.is_empty() => {
                        match permit {
                            Ok(permit) => { permit.send(self.pending.pop_front().expect("pending send exists")); },
                            Err(_) => break TransportEventKind::Failed(Status::unavailable("outbound stream closed")),
                        }
                    },
                    message = stream.message() => break match message {
                        Ok(Some(ack)) => TransportEventKind::Ack(ack),
                        Ok(None) => TransportEventKind::Failed(Status::unavailable("stream ended")),
                        Err(status) => TransportEventKind::Failed(status),
                    },
                }
            },
        };
        TransportEvent {
            stream_id: self.stream_id,
            kind,
        }
    }
}

pub(super) async fn next_transport_event(transport: &mut Option<Transport>) -> TransportEvent {
    match transport {
        Some(transport) => transport.next_event().await,
        None => pending().await,
    }
}
