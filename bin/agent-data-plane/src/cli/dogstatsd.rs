use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(target_os = "linux", test))]
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use argh::{FromArgValue, FromArgs};
use comfy_table::{presets::ASCII_FULL_CONDENSED, Cell, ContentArrangement, Row, Table};
use saluki_component_config::DogStatsDConfig;
#[cfg(any(target_os = "linux", test))]
use saluki_components::sources::TimestampResolution;
#[cfg(target_os = "linux")]
use saluki_components::sources::TrafficCaptureReader;
use saluki_components::sources::DEFAULT_REPLAY_LOOPS;
#[cfg(target_os = "linux")]
use saluki_components::sources::REPLAY_CREDENTIALS_GID;
#[cfg(any(target_os = "linux", test))]
use saluki_error::generic_error;
use saluki_error::{ErrorContext as _, GenericError};
#[cfg(target_os = "linux")]
use saluki_io::net::{unix::uds_sendmsg_with_creds, ProcessCredentials};
use serde::Deserialize;
use tokio::io::{self, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tokio::net::UnixDatagram;
#[cfg(target_os = "linux")]
use tokio_util::sync::CancellationToken;
#[cfg(target_os = "linux")]
use tracing::debug;
use tracing::{error, info};

use crate::{cli::utils::DataPlaneAPIClient, config::DataPlaneConfiguration};

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

fn default_capture_duration() -> String {
    "60s".to_string()
}

/// Starts a DogStatsD traffic capture.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "capture")]
struct CaptureCommand {
    /// how long the traffic capture should run for, using Go-style duration syntax such as `10s` or `1m0s`
    #[argh(option, short = 'd', long = "duration", default = "default_capture_duration()")]
    capture_duration: String,

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
pub async fn handle_dogstatsd_command(
    dp_config: &DataPlaneConfiguration, dogstatsd_config: &DogStatsDConfig, cmd: DogstatsdCommand,
) {
    #[cfg(not(target_os = "linux"))]
    let _ = dogstatsd_config;

    let mut api_client = match DataPlaneAPIClient::from_data_plane_config(dp_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create data plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    match cmd.subcommand {
        DogstatsdSubcommand::Stats(config) => {
            if let Err(e) = handle_dogstatsd_stats(&mut api_client, config).await {
                error!("Failed to run stats subcommand: {:#}", e);
                std::process::exit(1);
            }
        }
        DogstatsdSubcommand::Capture(config) => {
            if let Err(e) = handle_dogstatsd_capture(&mut api_client, config).await {
                error!("Failed to start DogStatsD capture: {:#}", e);
                std::process::exit(1);
            }
        }
        DogstatsdSubcommand::Replay(config) => {
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = handle_dogstatsd_replay(&mut api_client, dogstatsd_config, config).await {
                    error!("Failed to replay DogStatsD traffic: {:#}", e);
                    std::process::exit(1);
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                let ReplayCommand {
                    replay_file_path,
                    loops,
                } = config;
                let _ = (replay_file_path, loops);
                error!("DogStatsD replay is only supported on Linux.");
                std::process::exit(1);
            }
        }
    }
}

async fn handle_dogstatsd_stats(api_client: &mut DataPlaneAPIClient, cmd: StatsCommand) -> Result<(), GenericError> {
    // Trigger a statistics collection and wait for it to complete.
    info!(
        "Triggered statistics collection over the next {} seconds. Waiting for completion...",
        cmd.collection_duration_secs
    );

    let response_body = api_client.dogstatsd_stats(cmd.collection_duration_secs).await?;
    let mut response = serde_json::from_str::<StatsResponse>(&response_body)
        .error_context("Failed to deserialize collected statistics response.")?;

    info!("Collected {} metric(s).", response.stats.len());

    // Filter out any non-matching metrics if a filter was given.
    if let Some(filter) = cmd.filter.as_deref() {
        response.stats.retain(|metric| metric.name.contains(filter));
        info!("{} metric(s) remain after filtering.", response.stats.len());
    }

    if let Some(limit) = cmd.limit {
        info!("Output will be limited to the top {} metric(s).", limit);
    }

    match cmd.analysis_mode {
        AnalysisMode::Summary => handle_stats_summary_analysis(&cmd, response).await?,
        AnalysisMode::Cardinality => handle_stats_cardinality_analysis(&cmd, response).await?,
    }

    Ok(())
}

async fn handle_dogstatsd_capture(
    api_client: &mut DataPlaneAPIClient, cmd: CaptureCommand,
) -> Result<(), GenericError> {
    info!("Starting a DogStatsD traffic capture session...");

    let capture_path = api_client
        .dogstatsd_capture(&cmd.capture_duration, cmd.capture_path.as_deref(), cmd.compressed)
        .await?;

    info!("Capture started. Data will be written to '{capture_path}'.");

    Ok(())
}

#[cfg(target_os = "linux")]
async fn handle_dogstatsd_replay(
    api_client: &mut DataPlaneAPIClient, config: &DogStatsDConfig, cmd: ReplayCommand,
) -> Result<(), GenericError> {
    let socket_path = dogstatsd_socket_path(config)?;

    info!("Preparing DogStatsD replay from '{}'.", cmd.replay_file_path.display());

    let reader = TrafficCaptureReader::from_path(&cmd.replay_file_path)?;
    let state = reader.read_state()?;
    let session_id = api_client.dogstatsd_replay_start_session(state.as_ref()).await?;
    if state.is_some() {
        info!("Loaded captured DogStatsD tagger state into ADP.");
    } else {
        info!("Capture file contains no DogStatsD tagger state. Replayed packets will not receive captured tags.");
    }
    drop(reader);

    let cancel = CancellationToken::new();
    let cancel_on_signal = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }
    });

    let replay_result = run_dogstatsd_replay(&cmd.replay_file_path, &socket_path, cmd.loops, &cancel).await;
    cancel_on_signal.abort();

    let finish_result = api_client.dogstatsd_replay_finish_session(&session_id).await;
    match (replay_result, finish_result) {
        (Ok(()), Ok(())) => {
            if cancel.is_cancelled() {
                info!("DogStatsD replay interrupted.");
            } else {
                info!("DogStatsD replay completed.");
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

#[cfg(any(target_os = "linux", test))]
fn dogstatsd_socket_path(config: &DogStatsDConfig) -> Result<PathBuf, GenericError> {
    match config.socket_path.as_deref() {
        Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Err(generic_error!(
            "DogStatsD replay requires `dogstatsd_socket` to be configured."
        )),
    }
}

#[cfg(target_os = "linux")]
async fn run_dogstatsd_replay(
    replay_file_path: &Path, socket_path: &Path, loops: u32, cancel: &CancellationToken,
) -> Result<(), GenericError> {
    let socket = UnixDatagram::unbound().map_err(|e| generic_error!("Failed to open UDS client for replay: {}", e))?;
    socket
        .connect(socket_path)
        .map_err(|e| generic_error!("Failed to connect replay client to '{}': {}", socket_path.display(), e))?;

    let mut iteration: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if loops != 0 && iteration >= loops {
            return Ok(());
        }
        iteration = iteration.saturating_add(1);

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            r = replay_one_iteration(replay_file_path, &socket, cancel) => {
                r?;
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn replay_one_iteration(
    replay_file_path: &Path, socket: &UnixDatagram, cancel: &CancellationToken,
) -> Result<(), GenericError> {
    let mut reader = TrafficCaptureReader::from_path(replay_file_path)?;
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
            // Wait until the target deadline or cancellation, prioritizing cancellation.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(target_deadline)) => {}
            }
        }

        let credentials = ProcessCredentials {
            pid: std::process::id() as i32,
            uid: msg.pid as u32,
            gid: REPLAY_CREDENTIALS_GID,
        };

        uds_sendmsg_with_creds(socket, &msg.payload, &credentials)
            .await
            .map_err(|err| generic_error!("Replay packet send failed (captured_pid={}): {}", msg.pid, err))?;
        packets_sent += 1;
    }
}

#[cfg(any(target_os = "linux", test))]
fn compute_target_offset(timestamp: i64, first_timestamp: i64, resolution: TimestampResolution) -> Duration {
    let delta = timestamp.saturating_sub(first_timestamp).max(0) as u64;
    match resolution {
        TimestampResolution::Seconds => Duration::from_secs(delta),
        TimestampResolution::Nanoseconds => Duration::from_nanos(delta),
    }
}

async fn handle_stats_summary_analysis(cmd: &StatsCommand, mut response: StatsResponse<'_>) -> io::Result<()> {
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

    output_lines(table.lines()).await
}

async fn handle_stats_cardinality_analysis<'a>(cmd: &StatsCommand, response: StatsResponse<'a>) -> io::Result<()> {
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

    output_lines(table.lines()).await
}

fn get_stylized_table() -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_preset(ASCII_FULL_CONDENSED);

    table
}

async fn output_lines<I>(lines: I) -> io::Result<()>
where
    I: IntoIterator<Item = String>,
{
    let mut stdout = io::stdout();
    for line in lines {
        stdout.write_all(line.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
    }
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_component_config::DogStatsDConfig;

    use super::{
        compute_target_offset, default_capture_duration, default_replay_loops, dogstatsd_socket_path,
        TimestampResolution,
    };

    #[test]
    fn dogstatsd_capture_default_duration_matches_go() {
        assert_eq!(default_capture_duration(), "60s");
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

    #[test]
    fn dogstatsd_socket_path_requires_configured_socket() {
        let config = DogStatsDConfig::default();
        let err = dogstatsd_socket_path(&config).expect_err("empty socket should fail");
        assert!(err.to_string().contains("dogstatsd_socket"));
    }

    #[test]
    fn dogstatsd_socket_path_reads_configured_socket() {
        let config = DogStatsDConfig {
            socket_path: Some("/tmp/dsd.sock".to_string()),
            ..DogStatsDConfig::default()
        };
        let path = dogstatsd_socket_path(&config).expect("socket should be configured");
        assert_eq!(path, std::path::PathBuf::from("/tmp/dsd.sock"));
    }
}
