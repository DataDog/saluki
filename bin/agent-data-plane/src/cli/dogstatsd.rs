use serde_json::{self, from_str};
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
        Ok(response) => match response.text().await {
            Ok(body) => {
                println!("response: {}", body);
                if let Err(e) = output_stats(&format_stats(&body).await).await {
                    error!("Failed to output stats: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to read response body: {}", e);
            }
        },
        Err(e) => {
            error!("Request failed: {}", e);
        }
    }
}

async fn format_stats(body: &str) -> String {
    println!("body: {}", body);
    match from_str::<serde_json::Value>(body) {
        Ok(json) => {
            let mut output = String::new();

            output.push_str(&format!(
                "{:<40} | {:<20} | {:<10} | {:<20}\n",
                "Metric", "Tags", "Count", "Last Seen"
            ));
            output.push_str(&format!("{:-<40}-|-{:-<20}-|-{:-<10}-|-{:-<20}\n", "", "", "", ""));

            if let Some(stats_obj) = json.as_object() {
                println!("stats_obj: {:?}", stats_obj);
                for (key, metric) in stats_obj {
                    let (parsed_name, parsed_tags) = if let Some(parts) = key.split_once('{') {
                        let name = parts.0;
                        if let Some(tags) = parts.1.strip_suffix('}') {
                            (name, tags)
                        } else {
                            (name, "")
                        }
                    } else {
                        (key.as_str(), "")
                    };

                    if let Some(metric_obj) = metric.as_object() {
                        let count = metric_obj.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                        let last_seen_timestamp = metric_obj.get("last_seen").and_then(|v| v.as_u64()).unwrap_or(0);
                        let last_seen = chrono::DateTime::from_timestamp(last_seen_timestamp as i64, 0)
                            .unwrap()
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string();

                        output.push_str(&format!(
                            "{:<40} | {:<20} | {:<10} | {:<20}\n",
                            parsed_name, parsed_tags, count, last_seen
                        ));
                    }
                }
            }

            output
        }
        Err(e) => {
            format!("Error parsing JSON response: {}", e)
        }
    }
}

async fn output_stats(body: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(body.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
