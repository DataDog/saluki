use serde_json::{self, from_str};
use tokio::io::{self, AsyncWriteExt};
use tracing::error;

use crate::config::DogstatsdConfig;

pub async fn handle_dogstatsd_subcommand(config: DogstatsdConfig) {
    match config {
        DogstatsdConfig::Stats => {
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .unwrap();
            handle_dogstatsd_stats(client).await;
        }
    }
}

async fn handle_dogstatsd_stats(client: reqwest::Client) {
    let response = client.get("https://localhost:5101/dogstatsd/stats").send().await;

    match response {
        Ok(response) => match response.text().await {
            Ok(body) => {
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
    match from_str::<serde_json::Value>(body) {
        Ok(json) => {
            let mut output = String::new();

            output.push_str(&format!(
                "{:<40} | {:<20} | {:<10} | {:<20}\n",
                "Metric", "Tags", "Count", "Last Seen"
            ));
            output.push_str(&format!("{:-<40}-|-{:-<20}-|-{:-<10}-|-{:-<20}\n", "", "", "", ""));

            if let Some(metrics_received) = json.get("metrics_received") {
                if let Some(metrics_obj) = metrics_received.as_object() {
                    for (_key, metric) in metrics_obj {
                        if let Some(metric_obj) = metric.as_object() {
                            let name = metric_obj.get("name").and_then(|v| v.as_str()).unwrap_or("N/A");
                            let tags = metric_obj.get("tags").and_then(|v| v.as_str()).unwrap_or("N/A");
                            let count = metric_obj.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                            let last_seen = metric_obj.get("last_seen").and_then(|v| v.as_str()).unwrap_or("N/A");

                            output.push_str(&format!(
                                "{:<40} | {:<20} | {:<10} | {:<20}\n",
                                name, tags, count, last_seen
                            ));
                        }
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
