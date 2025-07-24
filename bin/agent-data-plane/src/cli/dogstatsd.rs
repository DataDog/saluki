use serde_json::{self, from_str};
use tracing::info;

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
                output_formatted_stats(&body).await;
            }
            Err(e) => {
                info!("Failed to read response body: {}", e);
            }
        },
        Err(e) => {
            info!("Request failed: {}", e);
        }
    }
}

async fn output_formatted_stats(body: &str) {
    match from_str::<serde_json::Value>(body) {
        Ok(json) => {
            println!(
                "{:<40} | {:<20} | {:<10} | {:<20}",
                "Metric", "Tags", "Count", "Last Seen"
            );
            println!("{:-<40}-|-{:-<20}-|-{:-<10}-|-{:-<20}", "", "", "", "");

            if let Some(metrics_received) = json.get("metrics_received") {
                if let Some(metrics_obj) = metrics_received.as_object() {
                    for (_key, metric) in metrics_obj {
                        if let Some(metric_obj) = metric.as_object() {
                            let name = metric_obj.get("name").and_then(|v| v.as_str()).unwrap_or("N/A");
                            let tags = metric_obj.get("tags").and_then(|v| v.as_str()).unwrap_or("N/A");
                            let count = metric_obj.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                            let last_seen = metric_obj.get("last_seen").and_then(|v| v.as_str()).unwrap_or("N/A");

                            println!("{:<40} | {:<20} | {:<10} | {:<20}", name, tags, count, last_seen);
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("Error parsing JSON response: {}", e);
            println!("Raw response: {}", body);
        }
    }
}
