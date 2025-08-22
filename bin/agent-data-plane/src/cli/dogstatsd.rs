use std::collections::{HashMap, HashSet};

use comfy_table::{presets::UTF8_FULL, Cell, ContentArrangement, Row, Table};
use saluki_api::StatusCode;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use tokio::io::{self, AsyncWriteExt};
use tracing::error;

use crate::config::{AnalysisMode, DogstatsdConfig, DogstatsdStatsConfig, SortDirection};

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

pub async fn handle_dogstatsd_subcommand(config: DogstatsdConfig) {
    match config {
        DogstatsdConfig::Stats(config) => {
            if let Err(e) = handle_dogstatsd_stats(config).await {
                error!("Failed to run stats subcommand: {}", e);
            }
        }
    }
}

async fn handle_dogstatsd_stats(config: DogstatsdStatsConfig) -> Result<(), GenericError> {
    // Trigger a statistics collection and wait for it to complete.
    let client = get_http_client();
    let response = client
        .get("https://localhost:5101/dogstatsd/stats")
        .query(&[("collection_duration_secs", config.collection_duration_secs)])
        .send()
        .await
        .error_context("Failed to send statistics collection request.")?;

    // Parse the response and extract the deserialized statistics.
    let status = response.status();
    let response_body = response.text().await.error_context("Failed to read response body.")?;

    if status != StatusCode::OK {
        output(&response_body).await.unwrap();
        return Err(generic_error!(
            "Non-success response ({}) after statistics collection. Raw response payload has been logged to stdout.",
            status
        ));
    }

    let mut response = serde_json::from_str::<StatsResponse>(&response_body)
        .error_context("Failed to deserialize collected statistics response.")?;

    // Filter out any non-matching metrics if a filter was given.
    if let Some(filter) = config.filter.as_deref() {
        response.stats.retain(|metric| metric.name.contains(filter));
    }

    let analysis_output = match config.analysis_mode {
        AnalysisMode::Summary => handle_stats_summary_analysis(&config, response),
        AnalysisMode::Cardinality => handle_stats_cardinality_analysis(&config, response),
    };

    output(&analysis_output)
        .await
        .error_context("Failed to output analysis results to stdout.")?;
    Ok(())
}

fn handle_stats_summary_analysis(config: &DogstatsdStatsConfig, mut response: StatsResponse<'_>) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Metric", "Tags", "Count", "Last Seen"]);

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
    for metric_summary in response.stats {
        let tags = metric_summary.tags.join(",");
        let last_seen = chrono::DateTime::from_timestamp(metric_summary.last_seen as i64, 0)
            .unwrap_or_default()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        table.add_row(Row::from(vec![
            Cell::new(metric_summary.name),
            Cell::new(tags),
            Cell::new(metric_summary.count.to_string()),
            Cell::new(last_seen),
        ]));
    }

    table.to_string()
}

fn handle_stats_cardinality_analysis<'a>(config: &DogstatsdStatsConfig, response: StatsResponse<'a>) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Metric", "Unique Contexts", "Highest Cardinality Tags (top 5)"]);

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
    // We default to descending order for the unique contexts.
    let sort_direction = config.sort_direction.unwrap_or(SortDirection::Descending);
    let sort_descending = matches!(sort_direction, SortDirection::Descending);
    flattened_cardinality_map.sort_by(|a, b| if sort_descending { b.1.cmp(&a.1) } else { a.1.cmp(&b.1) });

    // Add each metric summary to the table.
    for (metric_name, unique_contexts, tag_cardinalities) in flattened_cardinality_map {
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

        table.add_row(Row::from(vec![
            Cell::new(metric_name),
            Cell::new(unique_contexts.to_string()),
            Cell::new(highest_cardinality_tags),
        ]));
    }

    table.to_string()
}

async fn output(body: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(body.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

fn get_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        // We need this because our privileged API uses a self-signed certificate.
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}
