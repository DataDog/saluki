use crate::{EncodedPayload, SenderConfig, StreamWorker, WorkerError};

/// Upstream stateful logs endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    uri: String,
}

impl Endpoint {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }
}

/// Endpoint set supplied by the logs Agent configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointSet {
    reliable_endpoints: Vec<Endpoint>,
}

impl EndpointSet {
    pub fn new(reliable_endpoints: Vec<Endpoint>) -> Self {
        Self { reliable_endpoints }
    }

    pub fn reliable_endpoints(&self) -> &[Endpoint] {
        &self.reliable_endpoints
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderState {
    Unavailable,
    Running,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SenderError {
    Unavailable,
    Stopped,
    NoWorkers,
    WorkerBackpressure { worker_index: usize },
}

/// Stateful gRPC logs sender with one worker per logs pipeline.
#[derive(Clone, Debug)]
pub struct GrpcSender<S> {
    endpoint: Option<Endpoint>,
    state: SenderState,
    workers: Vec<StreamWorker<S>>,
    next_worker_index: usize,
}

impl<S> GrpcSender<S>
where
    S: Clone + Eq,
{
    pub fn new(endpoint_set: EndpointSet, pipeline_count: usize, config: SenderConfig) -> Self {
        let Some(endpoint) = endpoint_set.reliable_endpoints().first().cloned() else {
            return Self {
                endpoint: None,
                state: SenderState::Unavailable,
                workers: Vec::new(),
                next_worker_index: 0,
            };
        };

        let worker_count = pipeline_count.max(1);
        let workers = (0..worker_count)
            .map(|_| StreamWorker::new(endpoint.clone(), config.clone()))
            .collect();

        Self {
            endpoint: Some(endpoint),
            state: SenderState::Running,
            workers,
            next_worker_index: 0,
        }
    }

    pub const fn state(&self) -> SenderState {
        self.state
    }

    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    pub fn workers(&self) -> &[StreamWorker<S>] {
        &self.workers
    }

    pub fn workers_mut(&mut self) -> &mut [StreamWorker<S>] {
        &mut self.workers
    }

    pub const fn next_worker_index(&self) -> usize {
        self.next_worker_index
    }

    pub fn route_payload(&mut self, payload: EncodedPayload) -> Result<usize, SenderError> {
        match self.state {
            SenderState::Unavailable => return Err(SenderError::Unavailable),
            SenderState::Stopped => return Err(SenderError::Stopped),
            SenderState::Running => {}
        }

        if self.workers.is_empty() {
            return Err(SenderError::NoWorkers);
        }

        let worker_index = self.next_worker_index % self.workers.len();
        self.next_worker_index = (self.next_worker_index + 1) % self.workers.len();
        self.workers[worker_index]
            .enqueue_payload(payload)
            .map_err(|err| match err {
                WorkerError::InflightFull => SenderError::WorkerBackpressure { worker_index },
                _ => SenderError::WorkerBackpressure { worker_index },
            })?;

        Ok(worker_index)
    }

    pub fn stop(&mut self) {
        self.state = SenderState::Stopped;
        for worker in &mut self.workers {
            worker.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EncodedPayload, StatefulDatum, WorkerState};

    fn payload(label: &'static [u8]) -> EncodedPayload {
        let _ = label;
        EncodedPayload {
            state_changes: vec![],
            wire_datums: vec![StatefulDatum::log()],
        }
    }

    #[test]
    fn sender_without_endpoint_is_unavailable() {
        let sender = GrpcSender::<u64>::new(EndpointSet::default(), 1, SenderConfig::default());
        assert_eq!(sender.state(), SenderState::Unavailable);
        assert!(sender.endpoint().is_none());
        assert!(sender.workers().is_empty());
    }

    #[test]
    fn sender_with_endpoint_creates_one_worker_per_pipeline() {
        let sender = GrpcSender::<u64>::new(
            EndpointSet::new(vec![Endpoint::new("https://intake.example")]),
            3,
            SenderConfig::default(),
        );

        assert_eq!(sender.state(), SenderState::Running);
        assert_eq!(sender.workers().len(), 3);
        assert!(sender
            .workers()
            .iter()
            .all(|worker| worker.state() == WorkerState::Disconnected));
    }

    #[test]
    fn sender_routes_payloads_round_robin() {
        let mut sender = GrpcSender::<u64>::new(
            EndpointSet::new(vec![Endpoint::new("https://intake.example")]),
            2,
            SenderConfig::default(),
        );

        assert_eq!(sender.route_payload(payload(b"one")), Ok(0));
        assert_eq!(sender.route_payload(payload(b"two")), Ok(1));
        assert_eq!(sender.route_payload(payload(b"three")), Ok(0));
        assert_eq!(sender.next_worker_index(), 1);
    }
}
