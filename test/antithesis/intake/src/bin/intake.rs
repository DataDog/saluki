//! Binary entry point for the mock Datadog intake.

#![deny(warnings)]

#[cfg(unix)]
mod unix_intake {
    use antithesis_intake::http::build_router;
    use antithesis_sdk::prelude::*;
    use anyhow::{Context, Result};
    use clap::Parser;
    use tokio::{net::TcpListener, sync::mpsc};
    use tracing::{error, info};
    use tracing_subscriber::{filter::LevelFilter, EnvFilter};

    /// Mock Datadog intake configuration, settable via environment variables
    #[derive(Parser)]
    #[command(name = "antithesis-intake")]
    struct Config {
        /// Address the HTTP intake binds and serves on.
        #[arg(long = "listen-addr", env = "LISTEN_ADDR", default_value = "0.0.0.0:2049")]
        listen_addr: String,
    }

    #[tokio::main]
    pub(super) async fn run() {
        tracing_subscriber::fmt()
            .compact()
            .with_env_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .with_ansi(true)
            .with_target(true)
            .init();

        antithesis_init();

        if let Err(e) = serve(Config::parse()).await {
            error!("{:?}", e);
            std::process::exit(1);
        }
        info!("antithesis-intake stopped.");
    }

    /// Build the intake app, bind the listener, and serve until a shutdown signal.
    async fn serve(config: Config) -> Result<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        spawn_signal_handlers(shutdown_tx).context("Failed to configure signal handlers.")?;

        let listener = TcpListener::bind(&config.listen_addr)
            .await
            .context("Failed to bind HTTP intake listener.")?;
        info!("antithesis-intake started: listening on {}.", config.listen_addr);

        axum::serve(listener, build_router())
            .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
            .await
            .map_err(Into::into)
    }

    /// Spawn a task that signals shutdown on SIGINT or SIGTERM.
    fn spawn_signal_handlers(shutdown_tx: mpsc::Sender<()>) -> Result<()> {
        use tokio::{
            select,
            signal::unix::{signal, SignalKind},
        };

        let mut sigint_handler = signal(SignalKind::interrupt()).context("Failed to set up SIGINT handler.")?;
        let mut sigterm_handler = signal(SignalKind::terminate()).context("Failed to set up SIGTERM handler.")?;

        tokio::spawn(async move {
            select! {
                _ = sigint_handler.recv() => info!("Received SIGINT, shutting down..."),
                _ = sigterm_handler.recv() => info!("Received SIGTERM, shutting down..."),
            }

            if let Err(e) = shutdown_tx.send(()).await {
                error!("Failed to send shutdown signal: {:?}", e);
            }
        });

        Ok(())
    }
}

#[cfg(unix)]
fn main() {
    unix_intake::run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("antithesis-intake requires Unix signal handling and is only supported on Unix")
}
