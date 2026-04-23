use std::collections::{HashMap, HashSet};

use argh::{FromArgValue, FromArgs};
use comfy_table::{presets::ASCII_FULL_CONDENSED, Cell, ContentArrangement, Row, Table};
use saluki_components::sources::{DogStatsDConfiguration, DogStatsDReplayInjector, TrafficCaptureReader};
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tokio::io::{self, AsyncWriteExt};
use tracing::{error, info};

use crate::cli::utils::{DataPlaneAPIClient, DataPlaneSecureClient};

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
    "1m0s".to_string()
}

const fn default_replay_loops() -> usize {
    1
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

/// Replays a DogStatsD traffic capture back into the configured Unix socket.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "replay")]
struct ReplayCommand {
    /// input file created by `dogstatsd capture`
    #[argh(option, short = 'f', long = "file")]
    replay_file_path: String,

    /// print one line per replayed packet
    #[argh(switch, short = 'v', long = "verbose")]
    verbose: bool,

    /// number of replay loops to run, where `0` means replay until interrupted
    #[argh(option, short = 'l', long = "loops", default = "default_replay_loops()")]
    loops: usize,
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
pub async fn handle_dogstatsd_command(bootstrap_config: &GenericConfiguration, cmd: DogstatsdCommand) {
    match cmd.subcommand {
        DogstatsdSubcommand::Stats(config) => {
            let mut api_client = match DataPlaneAPIClient::from_config(bootstrap_config) {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to create data plane API client: {:#}", e);
                    std::process::exit(1);
                }
            };

            if let Err(e) = handle_dogstatsd_stats(&mut api_client, config).await {
                error!("Failed to run stats subcommand: {:#}", e);
                std::process::exit(1);
            }
        }
        DogstatsdSubcommand::Capture(config) => {
            let mut api_client = match DataPlaneSecureClient::from_config(bootstrap_config).await {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to create secure data plane API client: {:#}", e);
                    std::process::exit(1);
                }
            };

            if let Err(e) = handle_dogstatsd_capture(&mut api_client, config).await {
                error!("Failed to start DogStatsD capture: {:#}", e);
                std::process::exit(1);
            }
        }
        DogstatsdSubcommand::Replay(config) => {
            let mut api_client = match DataPlaneSecureClient::from_config(bootstrap_config).await {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to create secure data plane API client: {:#}", e);
                    std::process::exit(1);
                }
            };

            if let Err(e) = handle_dogstatsd_replay(bootstrap_config, &mut api_client, config).await {
                error!("Failed to replay DogStatsD capture: {:#}", e);
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
    api_client: &mut DataPlaneSecureClient, cmd: CaptureCommand,
) -> Result<(), GenericError> {
    println!("Starting a dogstatsd traffic capture session...\n");

    let capture_path = api_client
        .dogstatsd_capture(&cmd.capture_duration, cmd.capture_path.as_deref(), cmd.compressed)
        .await?;

    println!("Capture started, capture file being written to: {capture_path}");

    Ok(())
}

async fn handle_dogstatsd_replay(
    bootstrap_config: &GenericConfiguration, api_client: &mut DataPlaneSecureClient, cmd: ReplayCommand,
) -> Result<(), GenericError> {
    println!("Replaying dogstatsd traffic...\n");

    let dogstatsd_config = DogStatsDConfiguration::from_configuration(bootstrap_config)
        .error_context("Failed to load DogStatsD configuration for replay.")?;
    let socket_path = dogstatsd_config.socket_path().ok_or_else(|| {
        saluki_error::generic_error!("DogStatsD Unix socket disabled: configure `dogstatsd_socket` before replaying.")
    })?;

    let mut reader = TrafficCaptureReader::from_path(&cmd.replay_file_path)?;
    let injector = DogStatsDReplayInjector::from_socket_path(socket_path).await?;

    match reader.read_state() {
        Ok(Some(tagger_state)) => match api_client.dogstatsd_set_tagger_state(tagger_state).await {
            Ok(true) => {}
            Ok(false) => {
                println!("API refused to set replay state, tag enrichment will be unavailable for this capture.");
            }
            Err(e) => {
                println!("Unable to load replay state through the API, tag enrichment will be unavailable for this capture: {e}");
            }
        },
        Ok(None) => {
            println!("Capture file did not contain replay state, tag enrichment will be unavailable for this capture.");
        }
        Err(e) => {
            println!("Unable to load replay state from file, tag enrichment will be unavailable for this capture: {e}");
        }
    }

    let replay_result = tokio::select! {
        result = replay_capture_loops(&injector, &mut reader, cmd.loops, cmd.verbose) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("Replay interrupted.");
            Ok(())
        }
    };

    println!("Clearing agent replay state...");
    let clear_result = api_client
        .dogstatsd_clear_tagger_state()
        .await
        .error_context("Failed to clear DogStatsD replay state after replay.");

    if let Ok(true) = &clear_result {
        println!("The capture state and pid map have been successfully cleared from ADP.");
    }

    match (replay_result, clear_result) {
        (Ok(()), Ok(_)) => {
            println!("Replay done");
            Ok(())
        }
        (Err(replay_error), Ok(_)) => Err(replay_error),
        (Ok(()), Err(clear_error)) => Err(clear_error),
        (Err(replay_error), Err(clear_error)) => Err(saluki_error::generic_error!(
            "Replay failed: {}. Additionally failed to clear replay state: {}.",
            replay_error,
            clear_error
        )),
    }
}

async fn replay_capture_loops(
    injector: &DogStatsDReplayInjector, reader: &mut TrafficCaptureReader, loops: usize, verbose: bool,
) -> Result<(), GenericError> {
    let mut iteration = 0;
    while should_replay_iteration(iteration, loops) {
        injector.replay_once(reader, verbose).await?;
        iteration += 1;
    }

    Ok(())
}

fn should_replay_iteration(iteration: usize, loops: usize) -> bool {
    loops == 0 || iteration < loops
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

            tag_cardinalities.sort_by(|a, b| b.1.cmp(&a.1));

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
    use super::{default_capture_duration, should_replay_iteration};

    #[test]
    fn dogstatsd_capture_default_duration_matches_go() {
        assert_eq!(default_capture_duration(), "1m0s");
    }

    #[test]
    fn replay_loops_zero_means_forever() {
        assert!(should_replay_iteration(0, 0));
        assert!(should_replay_iteration(25, 0));
    }

    #[test]
    fn replay_loops_positive_limits_iterations() {
        assert!(should_replay_iteration(0, 2));
        assert!(should_replay_iteration(1, 2));
        assert!(!should_replay_iteration(2, 2));
    }
}
