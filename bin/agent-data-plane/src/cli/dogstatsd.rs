use std::collections::{HashMap, HashSet};

use comfy_table::{presets::ASCII_FULL_CONDENSED, Cell, ContentArrangement, Row, Table};
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tokio::io::{self, AsyncWriteExt};
use tracing::{error, info};

use crate::{
    cli::utils::ControlPlaneAPIClient,
    config::{AnalysisMode, DogstatsdConfig, DogstatsdStatsConfig, SortDirection},
};

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

/// Entrypoint for all `dogstatsd` subcommands.
pub async fn handle_dogstatsd_subcommand(bootstrap_config: &GenericConfiguration, config: DogstatsdConfig) {
    let api_client = match ControlPlaneAPIClient::from_config(bootstrap_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create control plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    match config {
        DogstatsdConfig::Stats(config) => {
            if let Err(e) = handle_dogstatsd_stats(api_client, config).await {
                error!("Failed to run stats subcommand: {:#}", e);
                std::process::exit(1);
            }
        }
    }
}

async fn handle_dogstatsd_stats(
    api_client: ControlPlaneAPIClient, config: DogstatsdStatsConfig,
) -> Result<(), GenericError> {
    // Trigger a statistics collection and wait for it to complete.
    info!(
        "Triggered statistics collection over the next {} seconds. Waiting for completion...",
        config.collection_duration_secs
    );

    let response_body = api_client.dogstatsd_stats(config.collection_duration_secs).await?;
    let mut response = serde_json::from_str::<StatsResponse>(&response_body)
        .error_context("Failed to deserialize collected statistics response.")?;

    info!("Collected {} metric(s).", response.stats.len());

    // Filter out any non-matching metrics if a filter was given.
    if let Some(filter) = config.filter.as_deref() {
        response.stats.retain(|metric| metric.name.contains(filter));
        info!("{} metric(s) remain after filtering.", response.stats.len());
    }

    if let Some(limit) = config.limit {
        info!("Output will be limited to the top {} metric(s).", limit);
    }

    match config.analysis_mode {
        AnalysisMode::Summary => handle_stats_summary_analysis(&config, response).await?,
        AnalysisMode::Cardinality => handle_stats_cardinality_analysis(&config, response).await?,
    }

    Ok(())
}

async fn handle_stats_summary_analysis(
    config: &DogstatsdStatsConfig, mut response: StatsResponse<'_>,
) -> io::Result<()> {
    let mut table = get_stylized_table();
    table.set_header(vec!["Metric", "Tags", "Count", "Last Seen"]);

    // Handle sorting first.
    //
    // We default to ascending order for the summary analysis.
    let sort_direction = config.sort_direction.unwrap_or(SortDirection::Ascending);
    let sort_ascending = matches!(sort_direction, SortDirection::Ascending);
    response.stats.sort_by(|a, b| {
        if sort_ascending {
            a.name.cmp(b.name)
        } else {
            b.name.cmp(a.name)
        }
    });

    // Add each metric summary to the table.
    for metric_summary in response.stats.into_iter().take(config.limit.unwrap_or(usize::MAX)) {
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

async fn handle_stats_cardinality_analysis<'a>(
    config: &DogstatsdStatsConfig, response: StatsResponse<'a>,
) -> io::Result<()> {
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
    let sort_direction = config.sort_direction.unwrap_or(SortDirection::Descending);
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
        .take(config.limit.unwrap_or(usize::MAX))
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
