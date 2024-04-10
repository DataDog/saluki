use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

pub fn initialize_logging() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .try_init()
}
