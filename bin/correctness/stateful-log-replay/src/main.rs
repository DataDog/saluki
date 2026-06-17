//! Replays file-backed log lines over the foldspace stateful gRPC sender.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{env, fs, path::PathBuf, time::Duration};

use foldspace::{
    BatchStrategy, DefaultBatchEncoder, Endpoint, LogRecord, NoopBatchCompressor, ProtoBatchEncoder, SenderConfig,
    StatefulLogTranslator, StatefulMessage, StreamWorkerRunner, TonicStatefulTransport,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::AsyncReadExt as _,
    net::TcpListener,
    signal::unix::{signal, SignalKind},
};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:9130";
const DEFAULT_ENDPOINT: &str = "http://datadog-intake:2050";
const DEFAULT_LOGS_FILE: &str = "/etc/stateful-logs/input.log";
const BATCH_CAPACITY: usize = 7;
const CHANNEL_CAPACITY: usize = 8;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .init();

    match run().await {
        Ok(()) => info!("stateful-log-replay stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), GenericError> {
    let config = Config::from_args()?;

    wait_for_trigger(&config.listen_addr).await?;
    replay_logs(&config.logs_file, &config.endpoint).await?;
    wait_for_shutdown_signal().await?;

    Ok(())
}

async fn wait_for_trigger(listen_addr: &str) -> Result<(), GenericError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_error_context(|| format!("Failed to bind trigger listener at {listen_addr}."))?;
    info!("Waiting for replay trigger on {listen_addr}.");

    let (mut stream, peer_addr) = listener
        .accept()
        .await
        .error_context("Failed to accept replay trigger connection.")?;
    let mut trigger = Vec::new();
    stream
        .read_to_end(&mut trigger)
        .await
        .error_context("Failed to read replay trigger payload.")?;
    info!(peer = %peer_addr, bytes = trigger.len(), "Received replay trigger.");

    Ok(())
}

async fn replay_logs(logs_file: &PathBuf, endpoint: &str) -> Result<(), GenericError> {
    let lines = read_log_lines(logs_file)?;
    info!(line_count = lines.len(), file = %logs_file.display(), endpoint, "Replaying stateful logs.");

    let transport = TonicStatefulTransport::new(ProtoBatchEncoder::new(NoopBatchCompressor));
    let sender_config = SenderConfig {
        stream_lifetime: Duration::from_secs(60),
        ..SenderConfig::default()
    };
    let (runner, handle) = StreamWorkerRunner::with_channel(
        Endpoint::new(endpoint.to_string()),
        sender_config,
        transport,
        CHANNEL_CAPACITY,
    );

    let mut translator = StatefulLogTranslator::new();
    let mut batcher = BatchStrategy::new(DefaultBatchEncoder, BATCH_CAPACITY);

    for (index, line) in lines.iter().enumerate() {
        let mut record = LogRecord::new(line.as_bytes().to_vec(), 1_700_000_000_000 + index as i64);
        record.status = Some("info".to_string());
        record.service = Some("checkout".to_string());
        record.source = Some("foldspace-correctness".to_string());
        record.hostname = Some("stateful-log-replay".to_string());
        record.tags = vec!["env:correctness".to_string(), "team:logs".to_string()];
        record.uuid = Some(format!("stateful-correctness-{index}"));

        for message in translator.translate(&record) {
            push_or_flush(&mut batcher, &handle, message).await?;
        }
    }

    if let Some(payload) = batcher.flush() {
        handle
            .send(payload)
            .await
            .map_err(|_| generic_error!("Failed to enqueue final stateful logs payload."))?;
    }
    drop(handle);

    runner
        .run()
        .await
        .map_err(|err| generic_error!("Stateful logs stream worker failed: {:?}", err))?;
    info!("Finished replaying stateful logs.");

    Ok(())
}

fn read_log_lines(logs_file: &PathBuf) -> Result<Vec<String>, GenericError> {
    let lines = fs::read_to_string(logs_file)
        .with_error_context(|| format!("Failed to read logs file at {}.", logs_file.display()))?
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if lines.is_empty() {
        return Err(generic_error!(
            "Logs file at {} did not contain any non-empty lines.",
            logs_file.display()
        ));
    }

    Ok(lines)
}

async fn push_or_flush(
    batcher: &mut BatchStrategy<DefaultBatchEncoder>, handle: &foldspace::StreamWorkerRunnerHandle,
    message: StatefulMessage,
) -> Result<(), GenericError> {
    if let Err(message) = batcher.push(message) {
        let payload = batcher
            .flush()
            .ok_or_else(|| generic_error!("Stateful logs batcher rejected a message but had no payload to flush."))?;
        handle
            .send(payload)
            .await
            .map_err(|_| generic_error!("Failed to enqueue stateful logs payload."))?;
        batcher
            .push(message)
            .map_err(|_| generic_error!("Stateful logs message did not fit in an empty batch."))?;
    }

    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<(), GenericError> {
    let mut sigint_handler = signal(SignalKind::interrupt()).error_context("Failed to set up SIGINT handler.")?;
    let mut sigterm_handler = signal(SignalKind::terminate()).error_context("Failed to set up SIGTERM handler.")?;

    tokio::select! {
        _ = sigint_handler.recv() => {
            info!("Received SIGINT, shutting down...");
        }
        _ = sigterm_handler.recv() => {
            info!("Received SIGTERM, shutting down...");
        }
    }

    Ok(())
}

#[derive(Debug)]
struct Config {
    listen_addr: String,
    endpoint: String,
    logs_file: PathBuf,
}

impl Config {
    fn from_args() -> Result<Self, GenericError> {
        let mut config = Self {
            listen_addr: env::var("STATEFUL_LOG_REPLAY_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string()),
            endpoint: env::var("STATEFUL_LOG_REPLAY_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            logs_file: env::var("STATEFUL_LOG_REPLAY_LOGS_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_LOGS_FILE)),
        };

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--listen" => {
                    config.listen_addr = args
                        .next()
                        .ok_or_else(|| generic_error!("--listen requires an address value."))?;
                }
                "--endpoint" => {
                    config.endpoint = args
                        .next()
                        .ok_or_else(|| generic_error!("--endpoint requires a URL value."))?;
                }
                "--logs-file" => {
                    config.logs_file = PathBuf::from(
                        args.next()
                            .ok_or_else(|| generic_error!("--logs-file requires a path value."))?,
                    );
                }
                "--help" | "-h" => {
                    return Err(generic_error!(
                        "usage: stateful-log-replay [--listen ADDR] [--endpoint URL] [--logs-file PATH]"
                    ));
                }
                unknown => {
                    return Err(generic_error!("unknown argument: {unknown}"));
                }
            }
        }

        Ok(config)
    }
}
