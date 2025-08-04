use reqwest::StatusCode;
use serde_json::from_str;
use tokio::io::{self, AsyncWriteExt};
use tracing::error;

use crate::config::DogstatsdConfig;

pub async fn handle_dogstatsd_subcommand(config: DogstatsdConfig) {
    match config {
        DogstatsdConfig::Stats(config) => {
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .unwrap();
            handle_dogstatsd_stats(client, config.collection_duration_secs).await;
        }
    }
}

async fn handle_dogstatsd_stats(client: reqwest::Client, collection_duration_secs: u64) {
    let response = client
        .get("https://localhost:5101/dogstatsd/stats")
        .query(&[("collection_duration_secs", collection_duration_secs)])
        .send()
        .await;

    match response {
        Ok(response) => {
            let status = response.status();

            match response.text().await {
                Ok(body) => {
                    if status == StatusCode::OK {
                        if let Err(e) = output(&format_stats(&body).await).await {
                            error!("Failed to output stats: {}", e);
                        }
                    } else {
                        output(&body).await.unwrap();
                    }
                }
                Err(e) => {
                    error!("Failed to read response body: {}", e);
                }
            }
        }
        Err(e) => {
            error!("Request failed: {}", e);
        }
    }
}

async fn format_stats(body: &str) -> String {
    match from_str::<serde_json::Value>(body) {
        Ok(json) => {
            let mut output = String::new();

            output.push_str(&format!(
                "{:<40} | {:<20} | {:<10} | {:<20}\n",
                "Metric", "Tags", "Count", "Last Seen"
            ));
            output.push_str(&format!("{:-<40}-|-{:-<20}-|-{:-<10}-|-{:-<20}\n", "", "", "", ""));

            if let Some(collected_stats) = json.as_object() {
                if let Some(stats) = collected_stats.get("stats") {
                    for stat in stats.as_array().unwrap() {
                        let stat_obj = stat.as_object().unwrap();
                        let name = stat_obj.get("name").unwrap().as_str().unwrap();
                        let tags = stat_obj.get("tags").unwrap().as_array().unwrap();
                        let wrapped_tags = wrap_tags(tags, 20);
                        let count = stat_obj.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                        let last_seen_timestamp = stat_obj.get("last_seen").and_then(|v| v.as_u64()).unwrap_or(0);
                        let last_seen = chrono::DateTime::from_timestamp(last_seen_timestamp as i64, 0)
                            .unwrap()
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string();

                        output.push_str(&format_table_row(name, &wrapped_tags, count, &last_seen));
                    }
                } else {
                    error!("Error parsing collected stats.");
                }
            }
            output
        }
        Err(e) => {
            format!("Error parsing JSON response: {}", e)
        }
    }
}

fn wrap_tags(tags: &[serde_json::Value], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current_line = String::new();

    for (i, tag) in tags.iter().enumerate() {
        let tag = tag.as_str().unwrap();
        let last_tag = i == tags.len() - 1;
        println!(
            "last_tag: {}, current_line len: {}, tag len: {}",
            last_tag,
            current_line.len(),
            tag.len()
        );
        if current_line.is_empty() {
            current_line = tag.to_string();
            if !last_tag {
                current_line.push(',');
            }
        } else if (!last_tag && current_line.len() + 1 + tag.len() <= width)
            || (last_tag && current_line.len() + tag.len() <= width)
        {
            current_line.push_str(tag);
            if !last_tag {
                current_line.push(',');
            }
        } else {
            // Current line is full, start a new one
            lines.push(format!("{:<width$}", current_line));
            current_line = tag.to_string();
            if !last_tag {
                current_line.push(',');
            }
        }
    }

    // Add the last line if it's not empty
    if !current_line.is_empty() {
        lines.push(format!("{:<width$}", current_line));
    }

    lines
}

/// Formats a table row with support for multi-line content.
/// If tags span multiple lines, the other columns are repeated for each line.
fn format_table_row(name: &str, tags: &[String], count: u64, last_seen: &str) -> String {
    let mut output = String::new();

    for (i, tag_line) in tags.iter().enumerate() {
        if i == 0 {
            // First line: include all columns
            output.push_str(&format!(
                "{:<40} | {:<20} | {:<10} | {:<20}\n",
                name, tag_line, count, last_seen
            ));
        } else {
            // Subsequent lines: only show tags column, others are empty
            output.push_str(&format!("{:<40} | {:<20} | {:<10} | {:<20}\n", "", tag_line, "", ""));
        }
    }

    output
}

async fn output(body: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(body.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
