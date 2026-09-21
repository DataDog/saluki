use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_data_plane_config::{domains::dogstatsd::Listeners, SalukiConfiguration};
use agent_data_plane_config_system::LoadedConfiguration;
use argh::{FromArgValue, FromArgs};
use comfy_table::{presets::ASCII_FULL_CONDENSED, Cell, ContentArrangement, Row, Table};
use prost_types::{value::Kind, Struct};
use saluki_app::util::wait_for_shutdown_signal;
use saluki_components::sources::DEFAULT_REPLAY_LOOPS;
#[cfg(target_os = "linux")]
use saluki_components::sources::REPLAY_CREDENTIALS_GID;
use saluki_components::sources::{TimestampResolution, TrafficCaptureReader};
use saluki_config::DurationString;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
#[cfg(target_os = "windows")]
use saluki_io::net::ListenAddress;
#[cfg(target_os = "linux")]
use saluki_io::net::{unix::uds_sendmsg_with_creds, ProcessCredentials};
use serde::Deserialize;
#[cfg(target_os = "windows")]
use tokio::io::AsyncWriteExt;
#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::ClientOptions;
#[cfg(unix)]
use tokio::net::UnixDatagram;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::cli::utils::{get_api_client, DataPlaneAPIClient};

mod top;
use self::top::{handle_dogstatsd_dump_contexts, handle_dogstatsd_top, DumpContextsCommand, TopCommand};

/// DogStatsD-specific debugging commands.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "dogstatsd")]
pub struct DogstatsdCommand {
    #[argh(subcommand)]
    subcommand: DogstatsdSubcommand,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum DogstatsdSubcommand {
    Stats(StatsCommand),
    Capture(CaptureCommand),
    Replay(ReplayCommand),
    Top(TopCommand),
    DumpContexts(DumpContextsCommand),
}

/// Prints basic statistics about the metrics received by the data plane.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "stats")]
struct StatsCommand {
    /// amount of time to collect statistics for, in seconds
    #[argh(option, short = 'd', long = "duration-secs")]
    collection_duration_secs: u64,

    /// analysis mode ('summary' or 'cardinality')
    #[argh(option, short = 'm', long = "mode", default = "AnalysisMode::Summary")]
    analysis_mode: AnalysisMode,

    /// sort direction ('asc' or 'desc')
    #[argh(option, short = 's', long = "sort-dir")]
    sort_direction: Option<SortDirection>,

    /// filter to apply to metric names (any metrics which don't match the filter will be excluded)
    #[argh(option, short = 'f', long = "filter")]
    filter: Option<String>,

    /// maximum number of metrics to display (applied after filtering)
    #[argh(option, short = 'l', long = "limit")]
    limit: Option<usize>,
}

const fn default_capture_duration() -> DurationString {
    DurationString::new(Duration::from_secs(60))
}

/// Starts a DogStatsD traffic capture.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "capture")]
struct CaptureCommand {
    /// how long the traffic capture should run for, using Go-style duration syntax such as `10s` or `1m0s`
    #[argh(option, short = 'd', long = "duration", default = "default_capture_duration()")]
    capture_duration: DurationString,

    /// directory path to write the capture into
    #[argh(option, short = 'p', long = "path")]
    capture_path: Option<String>,

    /// whether to zstd-compress the capture file
    #[argh(option, short = 'z', long = "compressed", default = "true")]
    compressed: bool,
}

const fn default_replay_loops() -> u32 {
    DEFAULT_REPLAY_LOOPS
}

/// Replays DogStatsD traffic from a capture file.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "replay")]
struct ReplayCommand {
    /// path to the `.dog` or `.dog.zstd` capture file to replay
    #[argh(option, short = 'f', long = "file")]
    replay_file_path: PathBuf,

    /// number of times to replay the capture file; 0 replays forever until interrupted
    #[argh(option, short = 'l', long = "loops", default = "default_replay_loops()")]
    loops: u32,
}

#[derive(Clone, Copy, Debug, Default)]
enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

impl FromArgValue for SortDirection {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        let value_lc = value.to_lowercase();
        match value_lc.as_str() {
            "asc" => Ok(Self::Ascending),
            "desc" => Ok(Self::Descending),
            other => Err(format!("invalid sort direction '{}': expected 'asc' or 'desc'", other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
enum AnalysisMode {
    #[default]
    Summary,
    Cardinality,
}

impl FromArgValue for AnalysisMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        let value_lc = value.to_lowercase();
        match value_lc.as_str() {
            "summary" => Ok(Self::Summary),
            "cardinality" => Ok(Self::Cardinality),
            other => Err(format!(
                "invalid analysis mode '{}': expected 'summary' or 'cardinality'",
                other
            )),
        }
    }
}

#[derive(Deserialize)]
struct MetricSummary<'a> {
    name: &'a str,
    tags: Vec<&'a str>,
    count: u64,
    last_seen: u64,
}

#[derive(Deserialize)]
struct StatsResponse<'a> {
    #[serde(borrow)]
    stats: Vec<MetricSummary<'a>>,
}

/// Entrypoint for the `dogstatsd` commands.
pub async fn handle_dogstatsd_command(local_config: LoadedConfiguration, cmd: DogstatsdCommand) {
    let cancellation = CancellationToken::new();
    let signal_task = matches!(&cmd.subcommand, DogstatsdSubcommand::Replay(_)).then(|| {
        tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                wait_for_shutdown_signal().await;
                cancellation.cancel();
            }
        })
    });
    let mut output = DirectDogstatsdCommandOutput {
        stdout: std::io::stdout(),
    };
    let result = run_dogstatsd_command(local_config.local(), cmd, &mut output, &cancellation).await;
    if let Some(signal_task) = signal_task {
        signal_task.abort();
    }

    if let Err(error) = result {
        error!("{:#}", error);
        std::process::exit(1);
    }
}

pub(crate) trait DogstatsdCommandOutput: Send {
    /// Writes a progress update for the command.
    fn write_status(&mut self, message: &str) -> std::io::Result<()>;

    /// Returns the writer for the command's report output.
    fn report_writer(&mut self) -> &mut (dyn Write + Send);
}

struct DirectDogstatsdCommandOutput {
    stdout: std::io::Stdout,
}

impl DogstatsdCommandOutput for DirectDogstatsdCommandOutput {
    fn write_status(&mut self, message: &str) -> std::io::Result<()> {
        info!("{message}");
        Ok(())
    }

    fn report_writer(&mut self) -> &mut (dyn Write + Send) {
        &mut self.stdout
    }
}

pub(crate) async fn run_dogstatsd_command(
    config: &SalukiConfiguration, cmd: DogstatsdCommand, output: &mut dyn DogstatsdCommandOutput,
    cancellation: &CancellationToken,
) -> Result<(), GenericError> {
    if cancellation.is_cancelled() {
        return Ok(());
    }

    match cmd.subcommand {
        DogstatsdSubcommand::Stats(command) => {
            let mut api_client = get_api_client(config).await?;
            run_cancellable_command(cancellation, async {
                handle_dogstatsd_stats(&mut api_client, command, output)
                    .await
                    .error_context("Failed to run stats subcommand")
            })
            .await
        }
        DogstatsdSubcommand::Capture(command) => {
            let mut api_client = get_api_client(config).await?;
            run_cancellable_command(cancellation, async {
                handle_dogstatsd_capture(&mut api_client, command, output)
                    .await
                    .error_context("Failed to start DogStatsD capture")
            })
            .await
        }
        DogstatsdSubcommand::Replay(command) => {
            let mut api_client = get_api_client(config).await?;
            handle_dogstatsd_replay(
                &mut api_client,
                &config.domains.dogstatsd.listeners,
                command,
                output,
                cancellation,
            )
            .await
            .error_context("Failed to replay DogStatsD traffic")
        }
        DogstatsdSubcommand::Top(command) => {
            let command = command.validate();
            if command.is_offline() {
                handle_dogstatsd_top(None, command, output.report_writer(), cancellation).await
            } else {
                let mut api_client = get_api_client(config).await?;
                handle_dogstatsd_top(Some(&mut api_client), command, output.report_writer(), cancellation).await
            }
        }
        DogstatsdSubcommand::DumpContexts(_) => {
            let mut api_client = get_api_client(config).await?;
            run_cancellable_command(cancellation, async {
                handle_dogstatsd_dump_contexts(&mut api_client, output.report_writer()).await
            })
            .await
        }
    }
}

async fn run_cancellable_command(
    cancellation: &CancellationToken, command: impl Future<Output = Result<(), GenericError>>,
) -> Result<(), GenericError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = command => result,
    }
}

async fn handle_dogstatsd_stats(
    api_client: &mut DataPlaneAPIClient, cmd: StatsCommand, output: &mut dyn DogstatsdCommandOutput,
) -> Result<(), GenericError> {
    // Trigger a statistics collection and wait for it to complete.
    report_status(
        output,
        format!(
            "Triggered statistics collection over the next {} seconds. Waiting for completion...",
            cmd.collection_duration_secs
        ),
    )?;

    let response_body = api_client.dogstatsd_stats(cmd.collection_duration_secs).await?;
    let mut response = serde_json::from_str::<StatsResponse>(&response_body)
        .error_context("Failed to deserialize collected statistics response.")?;

    report_status(output, format!("Collected {} metric(s).", response.stats.len()))?;

    // Filter out any non-matching metrics if a filter was given.
    if let Some(filter) = cmd.filter.as_deref() {
        response.stats.retain(|metric| metric.name.contains(filter));
        report_status(
            output,
            format!("{} metric(s) remain after filtering.", response.stats.len()),
        )?;
    }

    if let Some(limit) = cmd.limit {
        report_status(
            output,
            format!("Output will be limited to the top {} metric(s).", limit),
        )?;
    }

    match cmd.analysis_mode {
        AnalysisMode::Summary => handle_stats_summary_analysis(&cmd, response, output.report_writer())?,
        AnalysisMode::Cardinality => handle_stats_cardinality_analysis(&cmd, response, output.report_writer())?,
    }

    Ok(())
}

async fn handle_dogstatsd_capture(
    api_client: &mut DataPlaneAPIClient, cmd: CaptureCommand, output: &mut dyn DogstatsdCommandOutput,
) -> Result<(), GenericError> {
    report_status(output, "Starting a DogStatsD traffic capture session...".to_string())?;

    let capture_duration = cmd.capture_duration.to_string();
    let capture_path = api_client
        .dogstatsd_capture(&capture_duration, cmd.capture_path.as_deref(), cmd.compressed)
        .await?;

    report_status(
        output,
        format!("Capture started. Data will be written to '{capture_path}'."),
    )?;

    Ok(())
}

async fn handle_dogstatsd_replay(
    api_client: &mut DataPlaneAPIClient, listeners: &Listeners, cmd: ReplayCommand,
    output: &mut dyn DogstatsdCommandOutput, cancel: &CancellationToken,
) -> Result<(), GenericError> {
    let target = dogstatsd_replay_target(listeners)?;

    report_status(
        output,
        format!("Preparing DogStatsD replay from '{}'.", cmd.replay_file_path.display()),
    )?;

    #[cfg(not(target_os = "linux"))]
    tracing::warn!(
        "DogStatsD replay cannot preserve captured PID-based origin tags on this platform. Replayed metrics may still \
         receive origin tags from client-supplied metadata and the current live workload state."
    );

    let Some(mut reader) = load_replay_capture(&cmd.replay_file_path, cancel).await? else {
        return Ok(());
    };
    let state = reader.read_state()?;
    let Some(session_id) =
        start_replay_session(cancel, api_client.dogstatsd_replay_start_session(state.as_ref())).await?
    else {
        return Ok(());
    };
    let state_status = if state.is_some() {
        "Loaded captured DogStatsD tagger state into ADP."
    } else {
        "Capture file contains no DogStatsD tagger state. Replayed packets will not receive captured tags."
    };
    let (replay_result, finish_result) = run_replay_with_session(
        &session_id,
        cancel,
        async {
            report_status(output, state_status.to_string())?;
            run_dogstatsd_replay(&mut reader, target, cmd.loops, cancel).await
        },
        |session_id| api_client.dogstatsd_replay_finish_session(session_id),
    )
    .await;
    match (replay_result, finish_result) {
        (Ok(()), Ok(())) => {
            if cancel.is_cancelled() {
                report_status(output, "DogStatsD replay interrupted.".to_string())?;
            } else {
                report_status(output, "DogStatsD replay completed.".to_string())?;
            }
            Ok(())
        }
        (Err(replay_error), Ok(())) => Err(replay_error),
        (Ok(()), Err(finish_error)) => Err(finish_error),
        (Err(replay_error), Err(finish_error)) => Err(generic_error!(
            "{} Additionally, failed to finish DogStatsD replay session: {}.",
            replay_error,
            finish_error
        )),
    }
}

async fn start_replay_session(
    cancellation: &CancellationToken, start_session: impl Future<Output = Result<String, GenericError>>,
) -> Result<Option<String>, GenericError> {
    if cancellation.is_cancelled() {
        return Ok(None);
    }

    // Once sent, a start request can create a server-side session even if the client is cancelled. Wait for its result
    // so the caller can finish any session that was created.
    start_session.await.map(Some)
}

async fn run_replay_with_session<'a, Replay, Finish, FinishFuture>(
    session_id: &'a str, cancellation: &CancellationToken, replay: Replay, finish: Finish,
) -> (Result<(), GenericError>, Result<(), GenericError>)
where
    Replay: Future<Output = Result<(), GenericError>>,
    Finish: FnOnce(&'a str) -> FinishFuture,
    FinishFuture: Future<Output = Result<(), GenericError>>,
{
    let replay_result = if cancellation.is_cancelled() {
        Ok(())
    } else {
        replay.await
    };
    let finish_result = finish(session_id).await;

    (replay_result, finish_result)
}

#[cfg(any(unix, test))]
fn dogstatsd_socket_path(listeners: &Listeners) -> Result<PathBuf, GenericError> {
    match listeners.socket.as_deref() {
        Some(path) => Ok(PathBuf::from(path)),
        None => Err(generic_error!(
            "DogStatsD replay requires `dogstatsd_socket` to be configured."
        )),
    }
}

#[derive(Debug)]
enum ReplayTarget {
    #[cfg(unix)]
    UnixDatagram(PathBuf),
    #[cfg(target_os = "windows")]
    NamedPipe(String),
}

fn dogstatsd_replay_target(listeners: &Listeners) -> Result<ReplayTarget, GenericError> {
    #[cfg(unix)]
    {
        Ok(ReplayTarget::UnixDatagram(dogstatsd_socket_path(listeners)?))
    }

    #[cfg(target_os = "windows")]
    {
        let Some(pipe_name) = listeners.pipe_name.clone() else {
            return Err(generic_error!(
                "DogStatsD replay requires `dogstatsd_pipe_name` to be configured."
            ));
        };
        let pipe_path = ListenAddress::named_pipe(pipe_name, String::new())
            .as_windows_named_pipe_path()
            .expect("named pipe address should produce a named pipe path");
        Ok(ReplayTarget::NamedPipe(pipe_path))
    }
}

async fn load_replay_capture(
    replay_file_path: &Path, cancellation: &CancellationToken,
) -> Result<Option<TrafficCaptureReader>, GenericError> {
    let replay_file_path = replay_file_path.to_path_buf();
    run_cancellable_replay_load(cancellation, move || {
        let file = open_replay_capture_file(&replay_file_path)?;
        TrafficCaptureReader::from_file(file)
    })
    .await
}

fn open_replay_capture_file(path: &Path) -> Result<std::fs::File, GenericError> {
    open_regular_file(
        path,
        || {
            format!(
                "DogStatsD replay requires a regular capture file; failed to open '{}'.",
                path.display()
            )
        },
        || {
            format!(
                "DogStatsD replay requires a regular capture file; failed to inspect '{}'.",
                path.display()
            )
        },
        || {
            generic_error!(
                "DogStatsD replay requires a regular capture file; '{}' is not a regular file.",
                path.display()
            )
        },
    )
}

pub(super) fn open_regular_file(
    path: &Path, open_error_context: impl FnOnce() -> String, inspect_error_context: impl FnOnce() -> String,
    not_regular_file_error: impl FnOnce() -> GenericError,
) -> Result<std::fs::File, GenericError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);

    let file = options.open(path).with_error_context(open_error_context)?;
    if file.metadata().with_error_context(inspect_error_context)?.is_file() {
        Ok(file)
    } else {
        Err(not_regular_file_error())
    }
}

async fn run_cancellable_replay_load<T>(
    cancellation: &CancellationToken, load: impl FnOnce() -> Result<T, GenericError> + Send + 'static,
) -> Result<Option<T>, GenericError>
where
    T: Send + 'static,
{
    if cancellation.is_cancelled() {
        return Ok(None);
    }

    run_cancellable_blocking(
        cancellation,
        tokio::task::spawn_blocking(load),
        "DogStatsD replay capture loading",
    )
    .await
}

pub(super) async fn run_cancellable_blocking<T>(
    cancellation: &CancellationToken, mut task: tokio::task::JoinHandle<Result<T, GenericError>>, task_name: &str,
) -> Result<Option<T>, GenericError>
where
    T: Send + 'static,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            let _ = task.await;
            Ok(None)
        }
        result = &mut task => match result {
            Ok(result) => result.map(Some),
            Err(error) => Err(generic_error!("{task_name} task failed: {error}")),
        },
    }
}

enum ReplaySender {
    #[cfg(unix)]
    UnixDatagram(UnixDatagram),
    #[cfg(target_os = "windows")]
    NamedPipe(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl ReplaySender {
    async fn connect(target: ReplayTarget) -> Result<Self, GenericError> {
        match target {
            #[cfg(unix)]
            ReplayTarget::UnixDatagram(socket_path) => {
                let socket = UnixDatagram::unbound()
                    .map_err(|e| generic_error!("Failed to open UDS client for replay: {}", e))?;
                socket.connect(&socket_path).map_err(|e| {
                    generic_error!("Failed to connect replay client to '{}': {}", socket_path.display(), e)
                })?;
                Ok(Self::UnixDatagram(socket))
            }
            #[cfg(target_os = "windows")]
            ReplayTarget::NamedPipe(pipe_path) => {
                let mut options = ClientOptions::new();
                options.read(false).write(true);
                let pipe = options.open(&pipe_path).map_err(|e| {
                    generic_error!("Failed to connect replay client to named pipe '{}': {}", pipe_path, e)
                })?;
                Ok(Self::NamedPipe(pipe))
            }
        }
    }

    async fn send(&mut self, payload: &[u8], captured_pid: i32) -> Result<(), GenericError> {
        match self {
            #[cfg(unix)]
            Self::UnixDatagram(socket) => {
                #[cfg(target_os = "linux")]
                {
                    let credentials = ProcessCredentials {
                        pid: std::process::id() as i32,
                        uid: captured_pid as u32,
                        gid: REPLAY_CREDENTIALS_GID,
                    };
                    uds_sendmsg_with_creds(socket, payload, &credentials)
                        .await
                        .map_err(|err| {
                            generic_error!("Replay packet send failed (captured_pid={}): {}", captured_pid, err)
                        })?;
                }

                #[cfg(not(target_os = "linux"))]
                socket.send(payload).await.map_err(|err| {
                    generic_error!("Replay packet send failed (captured_pid={}): {}", captured_pid, err)
                })?;
            }
            #[cfg(target_os = "windows")]
            Self::NamedPipe(pipe) => {
                pipe.write_all(payload).await.map_err(|err| {
                    generic_error!("Replay packet send failed (captured_pid={}): {}", captured_pid, err)
                })?;
                pipe.write_all(b"\n").await.map_err(|err| {
                    generic_error!("Replay packet send failed (captured_pid={}): {}", captured_pid, err)
                })?;
                pipe.flush().await.map_err(|err| {
                    generic_error!(
                        "Failed to flush replay named pipe (captured_pid={}): {}",
                        captured_pid,
                        err
                    )
                })?;
            }
        }

        Ok(())
    }
}

async fn run_dogstatsd_replay(
    reader: &mut TrafficCaptureReader, target: ReplayTarget, loops: u32, cancel: &CancellationToken,
) -> Result<(), GenericError> {
    let mut sender = ReplaySender::connect(target).await?;
    let mut iteration: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if loops != 0 && iteration >= loops {
            return Ok(());
        }
        iteration = iteration.saturating_add(1);
        reader.rewind();

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            r = replay_one_iteration(reader, &mut sender, cancel) => {
                r?;
            }
        }
    }
}

async fn replay_one_iteration(
    reader: &mut TrafficCaptureReader, sender: &mut ReplaySender, cancel: &CancellationToken,
) -> Result<(), GenericError> {
    let resolution = reader.timestamp_resolution();

    let start = Instant::now();
    let mut first_timestamp: Option<i64> = None;
    let mut packets_sent: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let msg = match reader.read_next()? {
            Some(msg) => msg,
            None => {
                debug!(packets_sent, "Replay iteration completed.");
                return Ok(());
            }
        };

        let first = *first_timestamp.get_or_insert(msg.timestamp);
        let target_offset = compute_target_offset(msg.timestamp, first, resolution);
        let target_deadline = start + target_offset;
        let now = Instant::now();
        if target_deadline > now {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(target_deadline)) => {}
            }
        }

        sender.send(&msg.payload, msg.pid).await?;
        packets_sent += 1;
    }
}

fn compute_target_offset(timestamp: i64, first_timestamp: i64, resolution: TimestampResolution) -> Duration {
    let delta = timestamp.saturating_sub(first_timestamp).max(0) as u64;
    match resolution {
        TimestampResolution::Seconds => Duration::from_secs(delta),
        TimestampResolution::Nanoseconds => Duration::from_nanos(delta),
    }
}

fn handle_stats_summary_analysis(
    cmd: &StatsCommand, mut response: StatsResponse<'_>, output: &mut (dyn Write + Send),
) -> std::io::Result<()> {
    let mut table = get_stylized_table();
    table.set_header(vec!["Metric", "Tags", "Count", "Last Seen"]);

    // Handle sorting first.
    //
    // We default to ascending order for the summary analysis.
    let sort_direction = cmd.sort_direction.unwrap_or(SortDirection::Ascending);
    let sort_ascending = matches!(sort_direction, SortDirection::Ascending);
    response.stats.sort_by(|a, b| {
        if sort_ascending {
            a.name.cmp(b.name)
        } else {
            b.name.cmp(a.name)
        }
    });

    // Add each metric summary to the table.
    for metric_summary in response.stats.into_iter().take(cmd.limit.unwrap_or(usize::MAX)) {
        let tags = metric_summary.tags.join(",");
        let last_seen = chrono::DateTime::from_timestamp(metric_summary.last_seen as i64, 0)
            .unwrap_or_default()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S");

        table.add_row(Row::from([
            Cell::new(metric_summary.name),
            Cell::new(tags),
            Cell::new(metric_summary.count),
            Cell::new(last_seen),
        ]));
    }

    output_lines(output, table.lines())
}

fn handle_stats_cardinality_analysis<'a>(
    cmd: &StatsCommand, response: StatsResponse<'a>, output: &mut (dyn Write + Send),
) -> std::io::Result<()> {
    let mut table = get_stylized_table();
    table.set_header(["Metric", "Unique Contexts", "Highest Cardinality Tags (top 5)"]);

    // Build and populate our cardinality map.
    //
    // We have a high-level map that is keyed by metric name, and holds both the unique contexts seen for a given metric _and_
    // a map of unique values seen for each tag, to let us calculate the cardinality of each tag.
    let mut cardinality_stats: HashMap<&'a str, (u64, HashMap<&'a str, HashSet<&'a str>>)> = HashMap::new();

    for metric_summary in &response.stats {
        // We know every metric summary we get is a unique context, so we always increment the unique context count.
        let (unique_contexts, tag_values) = cardinality_stats.entry(metric_summary.name).or_default();
        *unique_contexts += 1;

        // For each tag, split it apart into key and value and update the tag cardinality map.
        for tag in &metric_summary.tags {
            let (key, value) = tag.split_once(':').unwrap_or((tag, ""));
            tag_values.entry(key).or_default().insert(value);
        }
    }

    let mut flattened_cardinality_map = cardinality_stats
        .into_iter()
        .map(|(name, (unique_contexts, tag_values))| {
            // Flatten the tag cardinality map, and then sort it, in descending order, based on the number of unique values.
            let mut tag_cardinalities = Vec::new();
            for (tag_key, values) in tag_values {
                tag_cardinalities.push((tag_key, values.len() as u64));
            }

            tag_cardinalities.sort_by_key(|a| Reverse(a.1));

            (name, unique_contexts, tag_cardinalities)
        })
        .collect::<Vec<_>>();

    // Handle sorting now that we've built our cardinality map.
    //
    // We default to descending order for the unique contexts. We do a subsort on the metric name just to keep things
    // stable when multiple metrics have the same number of unique contexts.
    let sort_direction = cmd.sort_direction.unwrap_or(SortDirection::Descending);
    let sort_descending = matches!(sort_direction, SortDirection::Descending);

    flattened_cardinality_map.sort_by(|a, b| {
        if sort_descending {
            b.1.cmp(&a.1).then_with(|| b.0.cmp(a.0))
        } else {
            a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0))
        }
    });

    // Add each metric summary to the table.
    for (metric_name, unique_contexts, tag_cardinalities) in flattened_cardinality_map
        .into_iter()
        .take(cmd.limit.unwrap_or(usize::MAX))
    {
        let highest_cardinality_tags = if tag_cardinalities.is_empty() {
            "[no tags]".to_string()
        } else {
            tag_cardinalities
                .into_iter()
                .take(5)
                .map(|(tag_key, cardinality)| format!("{} ({})", tag_key, cardinality))
                .collect::<Vec<_>>()
                .join(", ")
        };

        table.add_row(Row::from([
            Cell::new(metric_name),
            Cell::new(unique_contexts),
            Cell::new(highest_cardinality_tags),
        ]));
    }

    output_lines(output, table.lines())
}

fn get_stylized_table() -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_style(ASCII_FULL_CONDENSED);

    table
}

fn report_status(output: &mut dyn DogstatsdCommandOutput, message: String) -> std::io::Result<()> {
    output.write_status(&message)
}

fn output_lines<I>(output: &mut (dyn Write + Send), lines: I) -> std::io::Result<()>
where
    I: IntoIterator<Item = String>,
{
    for line in lines {
        writeln!(output, "{line}")?;
    }
    output.flush()
}

pub(crate) struct RemoteDogstatsdCommandDescriptor {
    pub(crate) name: &'static str,
    pub(crate) helper: &'static str,
    pub(crate) parameters: &'static [RemoteDogstatsdParameterDescriptor],
}

pub(crate) struct RemoteDogstatsdParameterDescriptor {
    pub(crate) name: &'static str,
    pub(crate) short_name: &'static str,
    pub(crate) helper: &'static str,
    pub(crate) argument_type: RemoteArgumentType,
    pub(crate) required: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteArgumentType {
    String,
    Bool,
    Uint,
}

pub(crate) const REMOTE_DOGSTATSD_COMMANDS: &[RemoteDogstatsdCommandDescriptor] = &[
    RemoteDogstatsdCommandDescriptor {
        name: "stats",
        helper: "Print basic statistics about metrics received by the data plane.",
        parameters: &[
            RemoteDogstatsdParameterDescriptor {
                name: "duration-secs",
                short_name: "d",
                helper: "Amount of time to collect statistics for, in seconds.",
                argument_type: RemoteArgumentType::Uint,
                required: true,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "mode",
                short_name: "m",
                helper: "Analysis mode: summary or cardinality.",
                argument_type: RemoteArgumentType::String,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "sort-dir",
                short_name: "s",
                helper: "Sort direction: asc or desc.",
                argument_type: RemoteArgumentType::String,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "filter",
                short_name: "f",
                helper: "Exclude metrics whose names do not contain this value.",
                argument_type: RemoteArgumentType::String,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "limit",
                short_name: "l",
                helper: "Maximum number of metrics to display.",
                argument_type: RemoteArgumentType::Uint,
                required: false,
            },
        ],
    },
    RemoteDogstatsdCommandDescriptor {
        name: "capture",
        helper: "Start a DogStatsD traffic capture.",
        parameters: &[
            RemoteDogstatsdParameterDescriptor {
                name: "duration",
                short_name: "d",
                helper: "Capture duration in Go duration syntax.",
                argument_type: RemoteArgumentType::String,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "path",
                short_name: "p",
                helper: "Directory in which to write the capture.",
                argument_type: RemoteArgumentType::String,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "compressed",
                short_name: "z",
                helper: "Whether to zstd-compress the capture file.",
                argument_type: RemoteArgumentType::Bool,
                required: false,
            },
        ],
    },
    RemoteDogstatsdCommandDescriptor {
        name: "replay",
        helper: "Replay DogStatsD traffic from a capture file.",
        parameters: &[
            RemoteDogstatsdParameterDescriptor {
                name: "file",
                short_name: "f",
                helper: "Path to the .dog or .dog.zstd capture file to replay.",
                argument_type: RemoteArgumentType::String,
                required: true,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "loops",
                short_name: "l",
                helper: "Number of replay iterations; 0 repeats until cancelled.",
                argument_type: RemoteArgumentType::Uint,
                required: false,
            },
        ],
    },
    RemoteDogstatsdCommandDescriptor {
        name: "top",
        helper: "Display DogStatsD contexts with the highest cardinality.",
        parameters: &[
            RemoteDogstatsdParameterDescriptor {
                name: "path",
                short_name: "p",
                helper: "Read a context dump artifact instead of requesting one.",
                argument_type: RemoteArgumentType::String,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "num-metrics",
                short_name: "m",
                helper: "Maximum number of metrics to display.",
                argument_type: RemoteArgumentType::Uint,
                required: false,
            },
            RemoteDogstatsdParameterDescriptor {
                name: "num-tags",
                short_name: "t",
                helper: "Maximum number of tags to display per metric.",
                argument_type: RemoteArgumentType::Uint,
                required: false,
            },
        ],
    },
    RemoteDogstatsdCommandDescriptor {
        name: "dump-contexts",
        helper: "Write currently tracked DogStatsD contexts as JSON.",
        parameters: &[],
    },
];

/// Parses a typed remote-command request into the same command representation used by the local CLI.
pub(crate) fn parse_remote_dogstatsd_command(
    command_path: &[String], arguments: &Struct,
) -> Result<DogstatsdCommand, GenericError> {
    let [command] = command_path else {
        return Err(generic_error!("expected exactly one DogStatsD command path segment"));
    };

    let mut argv = vec![command.clone()];
    for (name, value) in &arguments.fields {
        let expected_type = remote_argument_type(command, name)
            .ok_or_else(|| generic_error!("unexpected argument `{name}` for DogStatsD command `{command}`"))?;
        argv.push(format!("--{name}"));
        argv.push(remote_argument_value(name, value.kind.as_ref(), expected_type)?);
    }

    let argv_refs = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let command = DogstatsdCommand::from_args(&["agent-data-plane", "dogstatsd"], &argv_refs)
        .map_err(|error| generic_error!("invalid arguments for DogStatsD command `{command}`: {}", error.output))?;
    Ok(command)
}

fn remote_argument_type(command: &str, name: &str) -> Option<RemoteArgumentType> {
    REMOTE_DOGSTATSD_COMMANDS
        .iter()
        .find(|descriptor| descriptor.name == command)
        .and_then(|descriptor| descriptor.parameters.iter().find(|parameter| parameter.name == name))
        .map(|parameter| parameter.argument_type)
}

fn remote_argument_value(
    name: &str, kind: Option<&Kind>, expected_type: RemoteArgumentType,
) -> Result<String, GenericError> {
    match (expected_type, kind) {
        (RemoteArgumentType::String, Some(Kind::StringValue(value))) => Ok(value.clone()),
        (RemoteArgumentType::Bool, Some(Kind::BoolValue(value))) => Ok(value.to_string()),
        (RemoteArgumentType::Uint, Some(Kind::NumberValue(value)))
            if value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64 =>
        {
            Ok(format!("{value:.0}"))
        }
        (RemoteArgumentType::String, _) => Err(generic_error!("argument `{name}` must be a string")),
        (RemoteArgumentType::Bool, _) => Err(generic_error!("argument `{name}` must be a boolean")),
        (RemoteArgumentType::Uint, _) => Err(generic_error!("argument `{name}` must be an unsigned integer")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::Listeners;
    use prost_types::Value;
    use tokio_util::sync::CancellationToken;

    use super::{
        compute_target_offset, default_capture_duration, default_replay_loops, dogstatsd_replay_target,
        dogstatsd_socket_path, parse_remote_dogstatsd_command, DogstatsdSubcommand, GenericError, ReplayTarget,
        TimestampResolution,
    };

    #[test]
    fn remote_command_descriptors_are_the_canonical_remote_command_inventory() {
        let commands = super::REMOTE_DOGSTATSD_COMMANDS;

        assert_eq!(
            commands.iter().map(|command| command.name).collect::<Vec<_>>(),
            ["stats", "capture", "replay", "top", "dump-contexts"]
        );
        assert_eq!(commands[0].parameters[0].name, "duration-secs");
        assert!(commands[0].parameters[0].required);
        assert_eq!(commands[1].parameters[2].argument_type, super::RemoteArgumentType::Bool);
    }

    #[test]
    fn remote_command_parser_validates_argument_types_from_the_descriptor() {
        let arguments = prost_types::Struct {
            fields: [(
                "duration-secs".to_string(),
                Value {
                    kind: Some(prost_types::value::Kind::StringValue("60".to_string())),
                },
            )]
            .into(),
        };

        let error = parse_remote_dogstatsd_command(&["stats".to_string()], &arguments)
            .expect_err("stats duration must be an unsigned integer");

        assert!(error.to_string().contains("unsigned integer"));
    }

    #[test]
    fn remote_command_parser_requires_the_stats_duration() {
        let error = parse_remote_dogstatsd_command(&["stats".to_string()], &prost_types::Struct::default())
            .expect_err("stats duration is required");

        assert!(error.to_string().contains("duration-secs"));
    }

    #[test]
    fn remote_command_parser_preserves_capture_defaults() {
        let command = parse_remote_dogstatsd_command(&["capture".to_string()], &prost_types::Struct::default())
            .expect("capture accepts no optional flags");
        let DogstatsdSubcommand::Capture(capture) = command.subcommand else {
            panic!("expected capture command");
        };

        assert_eq!(capture.capture_duration.as_duration(), Duration::from_secs(60));
        assert!(capture.compressed);
        assert_eq!(capture.capture_path, None);
    }

    #[test]
    fn remote_command_parser_defers_top_file_validation_to_execution() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let command =
            parse_remote_dogstatsd_command(&["top".to_string()], &remote_file_argument("path", directory.path()))
                .expect("remote top parsing should not inspect the file path");

        assert!(matches!(command.subcommand, DogstatsdSubcommand::Top(_)));
    }

    #[test]
    fn remote_command_parser_defers_replay_directory_validation_to_execution() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let command =
            parse_remote_dogstatsd_command(&["replay".to_string()], &remote_file_argument("file", directory.path()))
                .expect("remote replay parsing should not inspect the file path");

        let DogstatsdSubcommand::Replay(command) = command.subcommand else {
            panic!("expected replay command");
        };
        let error = super::open_replay_capture_file(&command.replay_file_path)
            .expect_err("replay should reject a directory during execution");

        assert!(
            error.to_string().contains("regular file") || error.to_string().contains("failed to open"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_command_parser_defers_top_fifo_validation_to_execution() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let fifo = directory.path().join("context-dump.fifo");
        create_fifo(&fifo);

        let command = parse_remote_dogstatsd_command(&["top".to_string()], &remote_file_argument("path", &fifo))
            .expect("remote top parsing should not inspect the file path");

        assert!(matches!(command.subcommand, DogstatsdSubcommand::Top(_)));
    }

    #[cfg(unix)]
    #[test]
    fn remote_command_parser_defers_replay_file_validation_to_execution() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let fifo = directory.path().join("capture.fifo");
        create_fifo(&fifo);

        let command = parse_remote_dogstatsd_command(&["replay".to_string()], &remote_file_argument("file", &fifo))
            .expect("remote replay parsing should not inspect the file path");

        assert!(matches!(command.subcommand, DogstatsdSubcommand::Replay(_)));
    }

    #[cfg(unix)]
    #[test]
    fn replay_file_open_rejects_a_fifo_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let fifo = directory.path().join("capture.fifo");
        create_fifo(&fifo);

        let error = super::open_replay_capture_file(&fifo).expect_err("replay should reject a FIFO");

        assert!(error.to_string().contains("regular file"), "{error:#}");
    }

    #[test]
    fn replay_file_reader_uses_the_validated_descriptor_after_the_path_changes() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("capture.dog");
        std::fs::write(&path, [0xD4, 0x74, 0xD0, 0x60, 0xF3, 0xFF, 0x00, 0x00]).expect("capture should be written");
        let file = super::open_replay_capture_file(&path).expect("capture should open");
        let replacement = directory.path().join("replacement.dog");
        std::fs::write(&replacement, b"not a capture file").expect("replacement capture should be written");
        std::fs::rename(&replacement, &path).expect("capture path should be replaced");

        let reader = super::TrafficCaptureReader::from_file(file).expect("reader should use the opened capture");

        assert_eq!(reader.version(), 3);
    }

    #[tokio::test]
    async fn replay_capture_load_skips_file_access_when_cancelled() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let reader = super::load_replay_capture(std::path::Path::new("does-not-exist"), &cancellation)
            .await
            .expect("cancelled replay capture load should not access the file");

        assert!(reader.is_none());
    }

    #[tokio::test]
    async fn cancelled_replay_load_waits_for_its_blocking_task_to_finish() {
        let cancellation = CancellationToken::new();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(1);
        let mut task = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                super::run_cancellable_replay_load(&cancellation, move || {
                    started_tx.send(()).expect("test should wait for the blocking task");
                    finish_rx.recv().expect("test should release the blocking task");
                    Ok::<_, GenericError>(())
                })
                .await
            }
        });

        tokio::task::spawn_blocking(move || started_rx.recv().expect("blocking task should start"))
            .await
            .expect("wait task should not panic");
        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut task)
                .await
                .is_err(),
            "cancellation must wait for the blocking task rather than detach it"
        );
        finish_tx.send(()).expect("blocking task should still be running");

        assert!(task
            .await
            .expect("load task should not panic")
            .expect("load should not fail")
            .is_none());
    }

    #[tokio::test]
    async fn remote_replay_session_started_before_cancellation_is_finished() {
        let cancellation = CancellationToken::new();
        let session_id = super::start_replay_session(&cancellation, {
            let cancellation = cancellation.clone();
            async move {
                cancellation.cancel();
                tokio::task::yield_now().await;
                Ok::<_, GenericError>("replay-session".to_string())
            }
        })
        .await
        .expect("replay session start should succeed")
        .expect("completed replay session start should retain its session ID");
        let session_finished = Arc::new(AtomicBool::new(false));

        let (replay_result, finish_result) = super::run_replay_with_session(
            &session_id,
            &cancellation,
            async { panic!("cancelled replay session should not start replaying") },
            {
                let session_finished = Arc::clone(&session_finished);
                move |_| async move {
                    session_finished.store(true, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;

        replay_result.expect("cancelled replay should not fail");
        finish_result.expect("cancelled replay session should finish");
        assert!(session_finished.load(Ordering::SeqCst));
    }

    #[test]
    fn dogstatsd_capture_default_duration_matches_go() {
        assert_eq!(default_capture_duration().as_duration(), Duration::from_secs(60));
    }

    #[test]
    fn dogstatsd_replay_default_loops_matches_go() {
        assert_eq!(default_replay_loops(), 1);
    }

    #[test]
    fn compute_target_offset_handles_both_resolutions() {
        let seconds = compute_target_offset(105, 100, TimestampResolution::Seconds);
        assert_eq!(seconds, Duration::from_secs(5));

        let nanos = compute_target_offset(1_000_000_000, 0, TimestampResolution::Nanoseconds);
        assert_eq!(nanos, Duration::from_nanos(1_000_000_000));

        let clamped = compute_target_offset(50, 100, TimestampResolution::Nanoseconds);
        assert_eq!(clamped, Duration::ZERO);
    }

    fn remote_file_argument(name: &str, path: &std::path::Path) -> prost_types::Struct {
        prost_types::Struct {
            fields: [(
                name.to_string(),
                Value {
                    kind: Some(prost_types::value::Kind::StringValue(path.display().to_string())),
                },
            )]
            .into(),
        }
    }

    #[cfg(unix)]
    fn create_fifo(path: &std::path::Path) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path should not contain a null byte");
        let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "FIFO should be created: {}", std::io::Error::last_os_error());
    }

    fn listeners_with(socket: Option<&str>, pipe_name: Option<&str>) -> Listeners {
        Listeners {
            socket: socket.map(String::from),
            pipe_name: pipe_name.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn dogstatsd_socket_path_requires_configured_socket() {
        let listeners = listeners_with(None, None);

        let error = dogstatsd_socket_path(&listeners).expect_err("unset socket should fail");

        assert!(error.to_string().contains("dogstatsd_socket"));
    }

    #[test]
    fn dogstatsd_socket_path_reads_configured_socket() {
        let listeners = listeners_with(Some("/tmp/dsd.sock"), None);

        let path = dogstatsd_socket_path(&listeners).expect("socket should be configured");

        assert_eq!(path, std::path::PathBuf::from("/tmp/dsd.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn dogstatsd_replay_target_uses_configured_unix_datagram_socket() {
        let listeners = listeners_with(Some("/tmp/dsd.sock"), None);

        let target = dogstatsd_replay_target(&listeners).expect("socket should be configured");

        assert!(
            matches!(target, ReplayTarget::UnixDatagram(path) if path.as_path() == std::path::Path::new("/tmp/dsd.sock"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn dogstatsd_replay_target_requires_configured_named_pipe() {
        let listeners = listeners_with(None, None);

        let error = dogstatsd_replay_target(&listeners).expect_err("unset pipe name should fail");

        assert!(error.to_string().contains("dogstatsd_pipe_name"));
    }

    #[cfg(windows)]
    #[test]
    fn dogstatsd_replay_target_uses_configured_named_pipe() {
        let listeners = listeners_with(None, Some(r"\\.\pipe\datadog-dogstatsd"));

        let target = dogstatsd_replay_target(&listeners).expect("pipe should be configured");

        assert!(matches!(target, ReplayTarget::NamedPipe(path) if path == r"\\.\pipe\datadog-dogstatsd"));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn non_linux_unix_replay_sender_delivers_the_capture_payload() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let socket_path = directory.path().join("replay.sock");
        let receiver = tokio::net::UnixDatagram::bind(&socket_path).expect("receiver should bind");
        let mut sender = super::ReplaySender::connect(ReplayTarget::UnixDatagram(socket_path))
            .await
            .expect("sender should connect");

        sender
            .send(b"replay.sender:1|c", 1234)
            .await
            .expect("sender should write payload");

        let mut buffer = [0; 128];
        let bytes_read = receiver.recv(&mut buffer).await.expect("receiver should read payload");
        assert_eq!(&buffer[..bytes_read], b"replay.sender:1|c");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_replay_sender_delimits_each_capture_payload_with_a_newline() {
        use tokio::io::AsyncReadExt as _;

        let pipe_path = format!(r"\\.\pipe\saluki-replay-sender-{}", uuid::Uuid::new_v4());
        let mut server = tokio::net::windows::named_pipe::ServerOptions::new()
            .create(&pipe_path)
            .expect("named pipe server should bind");
        let receive = tokio::spawn(async move {
            server.connect().await.expect("named pipe server should accept client");
            let mut buffer = [0; b"replay.sender:1|c\n".len()];
            server
                .read_exact(&mut buffer)
                .await
                .expect("named pipe server should read payload");
            buffer.to_vec()
        });
        let mut sender = super::ReplaySender::connect(ReplayTarget::NamedPipe(pipe_path))
            .await
            .expect("sender should connect");

        sender
            .send(b"replay.sender:1|c", 1234)
            .await
            .expect("sender should write payload");

        assert_eq!(
            receive.await.expect("receiver task should complete"),
            b"replay.sender:1|c\n"
        );
    }
}
