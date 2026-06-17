use async_trait::async_trait;
use datadog_protos::stateful::{
    stateful_logs_service_client::StatefulLogsServiceClient, BatchStatus, StatefulBatch as ProtoStatefulBatch,
};
use tokio::sync::{mpsc, Mutex};
use tonic::transport::Channel;

use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use crate::{BatchCompressor, Endpoint, NoopBatchCompressor, ProtoBatchEncoder, StatefulBatch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    CreateStream(String),
    EncodeBatch(String),
    SendBatch(String),
    ReceiveAck(String),
}

/// Transport contract implemented by a tonic/protobuf adapter.
#[async_trait]
pub trait StreamTransport {
    type Stream: Clone + Eq + Send + Sync + 'static;

    async fn create_stream(&self, endpoint: &Endpoint) -> Result<Self::Stream, TransportError>;

    async fn send_batch(&self, batch: StatefulBatch<Self::Stream>) -> Result<(), TransportError>;

    async fn receive_ack(&self, stream: &Self::Stream) -> Result<u64, TransportError>;
}

/// Cloneable identity for one open tonic bidirectional stream.
#[derive(Clone)]
pub struct TonicStream {
    inner: Arc<TonicStreamInner>,
}

struct TonicStreamInner {
    id: u64,
    batches: mpsc::Sender<ProtoStatefulBatch>,
    acks: Mutex<tonic::Streaming<BatchStatus>>,
}

impl TonicStream {
    fn new(id: u64, batches: mpsc::Sender<ProtoStatefulBatch>, acks: tonic::Streaming<BatchStatus>) -> Self {
        Self {
            inner: Arc::new(TonicStreamInner {
                id,
                batches,
                acks: Mutex::new(acks),
            }),
        }
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }
}

impl fmt::Debug for TonicStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TonicStream")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl PartialEq for TonicStream {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl Eq for TonicStream {}

/// Tonic implementation of the stateful logs stream transport.
#[derive(Clone, Debug)]
pub struct TonicStatefulTransport<C = NoopBatchCompressor> {
    encoder: ProtoBatchEncoder<C>,
    stream_buffer_size: usize,
    next_stream_id: Arc<AtomicU64>,
}

impl Default for TonicStatefulTransport<NoopBatchCompressor> {
    fn default() -> Self {
        Self::new(ProtoBatchEncoder::default())
    }
}

impl<C> TonicStatefulTransport<C>
where
    C: BatchCompressor,
{
    pub const DEFAULT_STREAM_BUFFER_SIZE: usize = 10;

    pub fn new(encoder: ProtoBatchEncoder<C>) -> Self {
        Self {
            encoder,
            stream_buffer_size: Self::DEFAULT_STREAM_BUFFER_SIZE,
            next_stream_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn with_stream_buffer_size(mut self, stream_buffer_size: usize) -> Self {
        self.stream_buffer_size = stream_buffer_size.max(1);
        self
    }

    pub fn encoder(&self) -> &ProtoBatchEncoder<C> {
        &self.encoder
    }
}

#[async_trait]
impl<C> StreamTransport for TonicStatefulTransport<C>
where
    C: BatchCompressor,
{
    type Stream = TonicStream;

    async fn create_stream(&self, endpoint: &Endpoint) -> Result<Self::Stream, TransportError> {
        let channel = Channel::from_shared(endpoint.uri().to_string())
            .map_err(|err| TransportError::CreateStream(err.to_string()))?
            .connect()
            .await
            .map_err(|err| TransportError::CreateStream(err.to_string()))?;

        let (batch_tx, mut batch_rx) = mpsc::channel(self.stream_buffer_size);
        let request_stream = async_stream::stream! {
            while let Some(batch) = batch_rx.recv().await {
                yield batch;
            }
        };

        let mut client = StatefulLogsServiceClient::new(channel);
        let acks = client
            .logs_stream(request_stream)
            .await
            .map_err(|err| TransportError::CreateStream(err.to_string()))?
            .into_inner();

        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        Ok(TonicStream::new(stream_id, batch_tx, acks))
    }

    async fn send_batch(&self, batch: StatefulBatch<Self::Stream>) -> Result<(), TransportError> {
        let proto_batch = self
            .encoder
            .encode(&batch)
            .map_err(|err| TransportError::EncodeBatch(format!("{err:?}")))?;
        batch
            .stream
            .inner
            .batches
            .send(proto_batch)
            .await
            .map_err(|err| TransportError::SendBatch(err.to_string()))
    }

    async fn receive_ack(&self, stream: &Self::Stream) -> Result<u64, TransportError> {
        let mut acks = stream.inner.acks.lock().await;
        let Some(status) = acks
            .message()
            .await
            .map_err(|err| TransportError::ReceiveAck(err.to_string()))?
        else {
            return Err(TransportError::ReceiveAck("ack stream closed".to_string()));
        };

        Ok(u64::from(status.batch_id))
    }
}
