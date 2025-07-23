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
    info!("DogStatsD stats");
    let response = client.get("https://localhost:5101/dogstatsd/stats").send().await;
    println!("response: {:?}", response);
}
