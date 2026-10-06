use std::io::{self, BufRead, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use datadog_checks_protocol::{Consumer as FitConsumer, Message, Producer as FitProducer};
use datadog_protos::checks::{
    checks_client::ChecksClient,
    checks_server::{Checks, ChecksServer},
    SendCheckPayloadRequest, SendCheckPayloadResponse,
};
use saluki_fit::{CancellationToken, ConsumerConfig, ProducerConfig};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Channel, Request, Response, Status};

use crate::common::{self, Phase, SharedStats, Transport, Validator, Workload};

#[derive(Deserialize)]
struct Command {
    op: String,
    phase: Option<Phase>,
}

#[derive(Serialize)]
struct Reply<'a> {
    ok: bool,
    address: Option<&'a str>,
    stats: Option<common::Snapshot>,
    error: Option<&'a str>,
}

fn reply(address: Option<&str>, stats: Option<common::Snapshot>, error: Option<&str>) {
    let message = Reply {
        ok: error.is_none(),
        address,
        stats,
        error,
    };
    println!("{}", serde_json::to_string(&message).expect("reply serializes"));
    io::stdout().flush().expect("flush reply");
}

pub fn run(
    role: &str, transport: Transport, workload: Workload, endpoint: String, capacity: usize, delay_us: u64,
) -> Result<(), String> {
    let stats = Arc::new(SharedStats::default());
    let cancellation = CancellationToken::new();
    let mut producer_commands = None;
    let worker;
    let mut grpc_shutdown = None;
    let address;

    if role == "producer" {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let stats_clone = Arc::clone(&stats);
        let endpoint_clone = endpoint.clone();
        worker = thread::Builder::new()
            .name("bench-producer-data".into())
            .spawn(move || producer_loop(transport, workload, endpoint_clone, stats_clone, ready_tx, commands_rx))
            .map_err(|error| error.to_string())?;
        ready_rx
            .recv_timeout(Duration::from_secs(65))
            .map_err(|error| error.to_string())??;
        producer_commands = Some(commands_tx);
        address = None;
    } else if role == "consumer" {
        let stats_clone = Arc::clone(&stats);
        let cancellation_clone = cancellation.clone();
        if transport == Transport::Fit {
            let path = PathBuf::from(&endpoint);
            worker = thread::Builder::new()
                .name("bench-consumer-data".into())
                .spawn(move || fit_consumer(path, capacity, workload, stats_clone, cancellation_clone, delay_us))
                .map_err(|error| error.to_string())?;
            for _ in 0..500 {
                if std::path::Path::new(&endpoint).exists() {
                    break;
                }
                if worker.is_finished() {
                    return Err(format!("FIT consumer stopped during setup: {:?}", worker.join()));
                }
                thread::sleep(Duration::from_millis(10));
            }
            if !std::path::Path::new(&endpoint).exists() {
                return Err("FIT setup listener did not appear".into());
            }
            address = Some(endpoint.clone());
        } else {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
            listener.set_nonblocking(true).map_err(|error| error.to_string())?;
            let bound = listener.local_addr().map_err(|error| error.to_string())?;
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            worker = thread::Builder::new()
                .name("bench-grpc-server".into())
                .spawn(move || grpc_consumer(listener, workload, stats_clone, shutdown_rx, delay_us))
                .map_err(|error| error.to_string())?;
            grpc_shutdown = Some(shutdown_tx);
            address = Some(bound.to_string());
        }
    } else {
        return Err(format!("invalid worker role: {role}"));
    }

    reply(address.as_deref(), None, None);
    for line in io::stdin().lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        let command: Command = serde_json::from_str(&line).map_err(|error| error.to_string())?;
        match command.op.as_str() {
            "snapshot" => reply(None, Some(stats.snapshot()), None),
            "start" if role == "producer" => {
                let phase = command.phase.ok_or("missing phase")?;
                stats.done.store(false, Ordering::Relaxed);
                producer_commands
                    .as_ref()
                    .ok_or("producer channel absent")?
                    .send(phase)
                    .map_err(|error| error.to_string())?;
                reply(None, None, None);
            }
            "stop" => {
                break;
            }
            _ => reply(None, None, Some("invalid command")),
        }
    }
    drop(producer_commands);
    cancellation.cancel().map_err(|error| error.to_string())?;
    if let Some(shutdown_tx) = grpc_shutdown {
        let _ = shutdown_tx.send(());
    }
    worker.join().map_err(|_| "benchmark worker panicked".to_string())??;
    Ok(())
}

enum Sender {
    Fit(FitProducer),
    Grpc(tokio::runtime::Runtime, ChecksClient<Channel>),
}

impl Sender {
    fn connect(transport: Transport, endpoint: &str) -> Result<Self, String> {
        match transport {
            Transport::Fit => FitProducer::connect(ProducerConfig::new(PathBuf::from(endpoint)))
                .map(Self::Fit)
                .map_err(|error| error.to_string()),
            Transport::Grpc => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                let uri = format!("http://{endpoint}");
                let target = Channel::from_shared(uri).map_err(|error| error.to_string())?;
                let deadline = Instant::now() + Duration::from_secs(60);
                let channel = loop {
                    match runtime.block_on(target.clone().connect()) {
                        Ok(channel) => break channel,
                        Err(_error) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => return Err(format!("gRPC setup failed: {error}")),
                    }
                };
                Ok(Self::Grpc(runtime, ChecksClient::new(channel)))
            }
        }
    }

    fn send(&mut self, messages: Vec<Message>) -> Result<usize, String> {
        match self {
            Self::Fit(producer) => {
                let result = producer.send_batch(&messages).map_err(|error| error.to_string())?;
                if let Some(error) = result.notification_error {
                    return Err(format!(
                        "FIT published {} records but wake failed: {error}",
                        result.accepted
                    ));
                }
                if let Some(datadog_checks_protocol::BatchRejection::Encoding(error)) = result.rejection {
                    return Err(format!("FIT encoding failed: {error}"));
                }
                Ok(result.accepted)
            }
            Self::Grpc(runtime, client) => {
                let count = messages.len();
                let request = SendCheckPayloadRequest {
                    data: messages.into_iter().map(common::into_proto).collect(),
                };
                runtime
                    .block_on(client.send_check_payload(request))
                    .map_err(|error| error.to_string())?;
                Ok(count)
            }
        }
    }
}

fn producer_loop(
    transport: Transport, workload: Workload, endpoint: String, stats: Arc<SharedStats>,
    ready: mpsc::Sender<Result<(), String>>, commands: mpsc::Receiver<Phase>,
) -> Result<(), String> {
    let mut sender = match Sender::connect(transport, &endpoint) {
        Ok(sender) => sender,
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let _ = ready.send(Ok(()));
    for phase in commands {
        if let Err(error) = run_phase(&mut sender, workload, &phase, &stats) {
            stats.fatal.store(true, Ordering::Relaxed);
            return Err(error);
        }
        stats.done.store(true, Ordering::Release);
    }
    Ok(())
}

fn run_phase(sender: &mut Sender, workload: Workload, phase: &Phase, stats: &SharedStats) -> Result<(), String> {
    let end_ns = phase
        .start_ns
        .checked_add(phase.duration_ns)
        .ok_or("phase time overflow")?;
    let total = phase.scheduled();
    let mut index = 0u64;
    while index < total {
        let now = common::clock_ns();
        if now >= end_ns {
            if phase.rate.is_some() {
                let missed = total - index;
                stats.scheduled.fetch_add(missed, Ordering::Relaxed);
                stats.schedule_missed.fetch_add(missed, Ordering::Relaxed);
            }
            break;
        }
        let size = if phase.rate.is_some() {
            (total - index).min(phase.batch as u64)
        } else {
            phase.batch as u64
        };
        if let Some(rate) = phase.rate {
            let deadline = phase.start_ns + ((u128::from(index) * 1_000_000_000) / u128::from(rate)) as u64;
            if common::deadline_missed(now, deadline) {
                stats.scheduled.fetch_add(size, Ordering::Relaxed);
                stats.schedule_missed.fetch_add(size, Ordering::Relaxed);
                index += size;
                continue;
            }
            common::sleep_until(deadline);
        }
        let first = phase.first_sequence.checked_add(index).ok_or("sequence overflow")?;
        if first.checked_add(size).ok_or("sequence overflow")? >= common::MAX_SEQUENCE {
            return Err("sequence exceeds exact f64 range".into());
        }
        let messages = (0..size)
            .map(|offset| common::fixture(workload, first + offset))
            .collect();
        stats.scheduled.fetch_add(size, Ordering::Relaxed);
        stats.attempted.fetch_add(size, Ordering::Relaxed);
        stats.batches.fetch_add(1, Ordering::Relaxed);
        let accepted = sender.send(messages)? as u64;
        stats.accepted_range(first, accepted);
        stats.rejected.fetch_add(size - accepted, Ordering::Relaxed);
        index += size;
    }
    Ok(())
}

fn fit_consumer(
    path: PathBuf, capacity: usize, workload: Workload, stats: Arc<SharedStats>, cancellation: CancellationToken,
    delay_us: u64,
) -> Result<(), String> {
    let mut config = ConsumerConfig::new(path);
    config.ring_capacity = capacity;
    let mut consumer = FitConsumer::open_with_cancel(config, &cancellation).map_err(|error| error.to_string())?;
    let mut validator = Validator::new(workload);
    loop {
        match consumer.receive_with_cancel(&cancellation) {
            Ok(Some(message)) => {
                if delay_us > 0 {
                    thread::sleep(Duration::from_micros(delay_us));
                }
                validator.record(&message, &stats);
            }
            Ok(None) => return Ok(()),
            Err(error) => {
                stats.fatal.store(true, Ordering::Relaxed);
                return Err(error.to_string());
            }
        }
    }
}

struct GrpcService {
    stats: Arc<SharedStats>,
    validator: Mutex<Validator>,
    delay_us: u64,
}

#[tonic::async_trait]
impl Checks for GrpcService {
    async fn send_check_payload(
        &self, request: Request<SendCheckPayloadRequest>,
    ) -> Result<Response<SendCheckPayloadResponse>, Status> {
        for item in request.into_inner().data {
            if self.delay_us > 0 {
                tokio::time::sleep(Duration::from_micros(self.delay_us)).await;
            }
            let mut validator = self.validator.lock().unwrap();
            if let Some(message) = common::from_proto(item) {
                validator.record(&message, &self.stats);
            } else {
                self.stats.invalid.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Response::new(SendCheckPayloadResponse {}))
    }
}

fn grpc_consumer(
    listener: TcpListener, workload: Workload, stats: Arc<SharedStats>, shutdown: tokio::sync::oneshot::Receiver<()>,
    delay_us: u64,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let service = GrpcService {
        stats,
        validator: Mutex::new(Validator::new(workload)),
        delay_us,
    };
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::from_std(listener).map_err(|error| error.to_string())?;
        tonic::transport::Server::builder()
            .add_service(ChecksServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown.await;
            })
            .await
            .map_err(|error| error.to_string())
    })
}
