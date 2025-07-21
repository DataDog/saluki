use tracing::info;

use crate::config::DogstatsdConfig;

pub async fn handle_dogstatsd(config: DogstatsdConfig) {
    match config {
        DogstatsdConfig::Stats => {
            handle_dogstatsd_stats().await;
        }
    }
}

async fn handle_dogstatsd_stats() {
    info!("DogStatsD stats");
    // TODO: call API endpoint for dogstatsd-stats
}
