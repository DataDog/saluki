use std::{future::Future, time::Instant};

use tokio::{
    select,
    sync::mpsc,
    time::{sleep_until, Instant as TokioInstant},
};

use crate::{
    EncodedPayload, Endpoint, SenderConfig, StatefulBatch, StreamTransport, StreamWorker, TransportError, WorkerError,
    WorkerState,
};

/// Error returned by the long-lived stream worker runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunnerError {
    Worker(WorkerError),
}

impl From<WorkerError> for RunnerError {
    fn from(error: WorkerError) -> Self {
        Self::Worker(error)
    }
}

/// Send handle for a running stream worker.
#[derive(Clone, Debug)]
pub struct StreamWorkerRunnerHandle {
    payloads: mpsc::Sender<EncodedPayload>,
}

impl StreamWorkerRunnerHandle {
    pub async fn send(&self, payload: EncodedPayload) -> Result<(), EncodedPayload> {
        self.payloads.send(payload).await.map_err(|err| err.0)
    }
}

/// Drives one [`StreamWorker`] against a concrete transport.
#[derive(Debug)]
pub struct StreamWorkerRunner<T>
where
    T: StreamTransport,
{
    worker: StreamWorker<T::Stream>,
    transport: T,
    payloads: mpsc::Receiver<EncodedPayload>,
    pending_payload: Option<EncodedPayload>,
    input_open: bool,
}

impl<T> StreamWorkerRunner<T>
where
    T: StreamTransport + Sync,
    T::Stream: Send + Sync + 'static,
{
    pub fn new(worker: StreamWorker<T::Stream>, transport: T, payloads: mpsc::Receiver<EncodedPayload>) -> Self {
        Self {
            worker,
            transport,
            payloads,
            pending_payload: None,
            input_open: true,
        }
    }

    pub fn with_channel(
        endpoint: Endpoint, config: SenderConfig, transport: T, channel_capacity: usize,
    ) -> (Self, StreamWorkerRunnerHandle) {
        let (payload_tx, payload_rx) = mpsc::channel(channel_capacity);
        let worker = StreamWorker::new(endpoint, config);
        (
            Self::new(worker, transport, payload_rx),
            StreamWorkerRunnerHandle { payloads: payload_tx },
        )
    }

    pub fn worker(&self) -> &StreamWorker<T::Stream> {
        &self.worker
    }

    pub async fn run(mut self) -> Result<(), RunnerError> {
        while self.step().await? {}
        Ok(())
    }

    pub async fn run_until_shutdown<F>(mut self, shutdown: F) -> Result<(), RunnerError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        loop {
            select! {
                biased;
                _ = &mut shutdown => {
                    self.worker.stop();
                    return Ok(());
                }
                should_continue = self.step() => {
                    if !should_continue? {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn step(&mut self) -> Result<bool, RunnerError> {
        self.try_enqueue_pending()?;
        if self.should_stop_after_drain() {
            self.worker.stop();
            return Ok(false);
        }

        match self.worker.state() {
            WorkerState::Disconnected => self.drive_disconnected().await,
            WorkerState::Connecting => self.drive_connecting().await,
            WorkerState::Active => self.drive_active().await,
            WorkerState::Draining => self.drive_draining().await,
            WorkerState::Stopped => Ok(false),
        }
    }

    async fn drive_disconnected(&mut self) -> Result<bool, RunnerError> {
        let now = Instant::now();
        if let Some(deadline) = self.worker.backoff_deadline() {
            if deadline <= now {
                self.worker.backoff_elapsed(now);
                return Ok(true);
            }

            select! {
                maybe_payload = self.payloads.recv(), if self.can_receive_payload() => {
                    self.handle_payload_input(maybe_payload)?;
                }
                _ = sleep_until(TokioInstant::from_std(deadline)) => {
                    self.worker.backoff_elapsed(Instant::now());
                }
            }
            return Ok(true);
        }

        self.worker.start()?;
        Ok(true)
    }

    async fn drive_connecting(&mut self) -> Result<bool, RunnerError> {
        match self.worker.create_stream(&self.transport, Instant::now()).await {
            Ok(snapshot_batch) => {
                if let Some(snapshot_batch) = snapshot_batch {
                    self.send_planned_batch(snapshot_batch).await;
                }
            }
            Err(WorkerError::Transport(_)) => {}
            Err(err) => return Err(err.into()),
        }
        Ok(true)
    }

    async fn drive_active(&mut self) -> Result<bool, RunnerError> {
        self.send_ready_payloads().await?;
        self.try_enqueue_pending()?;

        if self.should_stop_after_drain() {
            self.worker.stop();
            return Ok(false);
        }

        let Some(stream) = self.worker.current_stream().cloned() else {
            return Err(WorkerError::NoCurrentStream.into());
        };
        let Some(deadline) = self.worker.stream_lifetime_deadline() else {
            return Err(WorkerError::InvalidState {
                expected: "active stream lifetime deadline",
                actual: self.worker.state(),
            }
            .into());
        };

        select! {
            maybe_payload = self.payloads.recv(), if self.can_receive_payload() => {
                self.handle_payload_input(maybe_payload)?;
            }
            ack = self.transport.receive_ack(&stream) => {
                self.handle_ack_result(&stream, ack)?;
            }
            _ = sleep_until(TokioInstant::from_std(deadline)) => {
                self.worker.stream_lifetime_elapsed(Instant::now());
            }
        }

        Ok(true)
    }

    async fn drive_draining(&mut self) -> Result<bool, RunnerError> {
        let Some(stream) = self.worker.current_stream().cloned() else {
            return Err(WorkerError::NoCurrentStream.into());
        };
        let Some(deadline) = self.worker.drain_deadline() else {
            return Err(WorkerError::InvalidState {
                expected: "draining deadline",
                actual: self.worker.state(),
            }
            .into());
        };

        select! {
            maybe_payload = self.payloads.recv(), if self.can_receive_payload() => {
                self.handle_payload_input(maybe_payload)?;
            }
            ack = self.transport.receive_ack(&stream) => {
                self.handle_ack_result(&stream, ack)?;
            }
            _ = sleep_until(TokioInstant::from_std(deadline)) => {
                self.worker.drain_elapsed(Instant::now());
            }
        }

        Ok(true)
    }

    async fn send_ready_payloads(&mut self) -> Result<(), RunnerError> {
        while self.worker.may_send_payload() {
            match self.worker.send_next(&self.transport, Instant::now()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(WorkerError::Transport(_)) => break,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    async fn send_planned_batch(&mut self, batch: StatefulBatch<T::Stream>) {
        let stream = batch.stream.clone();
        if let Err(err) = self.transport.send_batch(batch).await {
            self.handle_stream_transport_error(&stream, err);
        }
    }

    fn handle_ack_result(&mut self, stream: &T::Stream, ack: Result<u64, TransportError>) -> Result<(), RunnerError> {
        match ack {
            Ok(batch_id) => match self.worker.handle_ack(stream, batch_id, Instant::now()) {
                Ok(_) => Ok(()),
                Err(WorkerError::AckMismatch(_)) => Ok(()),
                Err(err) => Err(err.into()),
            },
            Err(err) => {
                self.handle_stream_transport_error(stream, err);
                Ok(())
            }
        }
    }

    fn handle_stream_transport_error(&mut self, stream: &T::Stream, _err: TransportError) {
        self.worker.stream_failure(stream, Instant::now());
    }

    fn handle_payload_input(&mut self, maybe_payload: Option<EncodedPayload>) -> Result<(), RunnerError> {
        match maybe_payload {
            Some(payload) => self.accept_payload(payload),
            None => {
                self.input_open = false;
                Ok(())
            }
        }
    }

    fn accept_payload(&mut self, payload: EncodedPayload) -> Result<(), RunnerError> {
        if self.worker.may_accept_payload() {
            self.worker.enqueue_payload(payload)?;
        } else {
            self.pending_payload = Some(payload);
        }
        Ok(())
    }

    fn try_enqueue_pending(&mut self) -> Result<(), RunnerError> {
        let Some(payload) = self.pending_payload.take() else {
            return Ok(());
        };

        if self.worker.may_accept_payload() {
            self.worker.enqueue_payload(payload)?;
        } else {
            self.pending_payload = Some(payload);
        }

        Ok(())
    }

    fn can_receive_payload(&self) -> bool {
        self.input_open && self.pending_payload.is_none()
    }

    fn should_stop_after_drain(&self) -> bool {
        !self.input_open && self.pending_payload.is_none() && self.worker.inflight().total_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
    };

    use datadog_protos::stateful::{
        batch_status,
        stateful_logs_service_server::{StatefulLogsService, StatefulLogsServiceServer},
        BatchStatus, DatumSequence, StatefulBatch as ProtoStatefulBatch,
    };
    use futures::Stream;
    use prost::Message as _;
    use tokio::{net::TcpListener, time::timeout};
    use tonic::{Request, Response, Status};

    use super::*;
    use crate::{NoopBatchCompressor, ProtoBatchEncoder, StatefulDatum, TonicStatefulTransport};

    #[derive(Clone, Default)]
    struct CapturingStatefulLogsService {
        captured: Arc<Mutex<Vec<ProtoStatefulBatch>>>,
    }

    #[tonic::async_trait]
    impl StatefulLogsService for CapturingStatefulLogsService {
        type LogsStreamStream = Pin<Box<dyn Stream<Item = Result<BatchStatus, Status>> + Send>>;

        async fn logs_stream(
            &self, request: Request<tonic::Streaming<ProtoStatefulBatch>>,
        ) -> Result<Response<Self::LogsStreamStream>, Status> {
            let captured = self.captured.clone();
            let mut inbound = request.into_inner();
            let outbound = async_stream::try_stream! {
                while let Some(batch) = inbound.message().await? {
                    captured.lock().unwrap().push(batch.clone());
                    yield BatchStatus {
                        batch_id: batch.batch_id,
                        status: batch_status::Status::Ok as i32,
                    };
                }
            };

            Ok(Response::new(Box::pin(outbound)))
        }
    }

    fn payload(datums: Vec<StatefulDatum>) -> EncodedPayload {
        EncodedPayload {
            state_changes: datums.iter().filter(|datum| datum.is_state_change()).cloned().collect(),
            wire_datums: datums,
        }
    }

    async fn spawn_fake_server(
        service: CapturingStatefulLogsService,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let incoming = async_stream::stream! {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => yield Ok::<_, std::io::Error>(stream),
                    Err(err) => {
                        yield Err(err);
                        break;
                    }
                }
            }
        };

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(StatefulLogsServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        Ok(format!("http://{}", addr))
    }

    async fn wait_for_captured_len(captured: &Arc<Mutex<Vec<ProtoStatefulBatch>>>, expected_len: usize) {
        timeout(std::time::Duration::from_secs(2), async {
            loop {
                if captured.lock().unwrap().len() >= expected_len {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("captured batches should reach expected length");
    }

    #[tokio::test]
    async fn runner_sends_batches_over_real_tonic_stateful_stream() {
        let service = CapturingStatefulLogsService::default();
        let captured = service.captured.clone();
        let endpoint = spawn_fake_server(service).await.unwrap();
        let transport = TonicStatefulTransport::new(ProtoBatchEncoder::new(NoopBatchCompressor));
        let config = SenderConfig {
            stream_lifetime: std::time::Duration::from_secs(60),
            ..SenderConfig::default()
        };
        let (runner, handle) = StreamWorkerRunner::with_channel(Endpoint::new(endpoint), config, transport, 8);

        handle
            .send(payload(vec![StatefulDatum::pattern_define(42), StatefulDatum::log()]))
            .await
            .unwrap();
        drop(handle);

        runner.run().await.unwrap();

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].batch_id, 1);

        let decoded = DatumSequence::decode(captured[0].data.as_slice()).unwrap();
        assert_eq!(decoded.data.len(), 2);
    }

    #[tokio::test]
    async fn runner_sends_snapshot_before_replayed_payload_on_rotated_stream() {
        let service = CapturingStatefulLogsService::default();
        let captured = service.captured.clone();
        let endpoint = spawn_fake_server(service).await.unwrap();
        let transport = TonicStatefulTransport::new(ProtoBatchEncoder::new(NoopBatchCompressor));
        let config = SenderConfig {
            stream_lifetime: std::time::Duration::from_millis(10),
            ..SenderConfig::default()
        };
        let (runner, handle) = StreamWorkerRunner::with_channel(Endpoint::new(endpoint), config.clone(), transport, 8);
        let runner_task = tokio::spawn(runner.run());

        handle
            .send(payload(vec![StatefulDatum::pattern_define(42)]))
            .await
            .unwrap();
        wait_for_captured_len(&captured, 1).await;
        wait_for_captured_len(&captured, 2).await;
        handle.send(payload(vec![StatefulDatum::log()])).await.unwrap();
        drop(handle);

        runner_task.await.unwrap().unwrap();

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].batch_id, config.first_payload_batch_id as u32);
        assert_eq!(captured[1].batch_id, config.snapshot_batch_id as u32);
        assert_eq!(captured[2].batch_id, config.first_payload_batch_id as u32);
    }
}
