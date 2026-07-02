//! Binary entry point for the mock Datadog intake.

#![deny(warnings)]

#[cfg(unix)]
mod unix_intake {
    use std::future::IntoFuture;

    use antithesis_intake::{
        capture::State,
        http::{build_router, state::AppState},
    };
    use antithesis_sdk::prelude::*;
    use anyhow::{Context, Result};
    use clap::Parser;
    use tokio::{net::TcpListener, sync::watch};
    use tracing::{error, info};
    use tracing_subscriber::{filter::LevelFilter, EnvFilter};

    /// Mock Datadog intake configuration, settable via environment variables
    #[derive(Parser)]
    #[command(name = "antithesis-intake")]
    struct Config {
        /// Address the HTTP intake binds and serves on.
        #[arg(long = "listen-addr", env = "LISTEN_ADDR", default_value = "0.0.0.0:2049")]
        listen_addr: String,
        /// Optional Agent-target HTTP bind address.
        #[arg(long = "agent-listen-addr", env = "AGENT_LISTEN_ADDR")]
        agent_listen_addr: Option<String>,
        /// Optional ADP-target HTTP bind address.
        #[arg(long = "adp-listen-addr", env = "ADP_LISTEN_ADDR")]
        adp_listen_addr: Option<String>,
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
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        spawn_signal_handlers(shutdown_tx).context("Failed to configure signal handlers.")?;

        let capture = State::new();

        if let (Some(agent_addr), Some(adp_addr)) = (config.agent_listen_addr.as_ref(), config.adp_listen_addr.as_ref())
        {
            let agent_listener = TcpListener::bind(agent_addr)
                .await
                .context("Failed to bind Agent-target HTTP intake listener.")?;
            let adp_listener = TcpListener::bind(adp_addr)
                .await
                .context("Failed to bind ADP-target HTTP intake listener.")?;
            info!(
                "antithesis-intake started: Agent target on {}, ADP target on {}.",
                agent_addr, adp_addr
            );

            let agent_router = build_router(AppState::agent(&capture));
            let adp_router = build_router(AppState::adp(&capture));

            // Both servers drain in flight requests on the shared shutdown signal.
            let agent_server = axum::serve(agent_listener, agent_router)
                .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));
            let adp_server =
                axum::serve(adp_listener, adp_router).with_graceful_shutdown(wait_for_shutdown(shutdown_rx));

            let (agent_result, adp_result) = tokio::join!(agent_server.into_future(), adp_server.into_future());
            agent_result.context("Agent-target intake server stopped unexpectedly")?;
            adp_result.context("ADP-target intake server stopped unexpectedly")?;
            Ok(())
        } else {
            let listener = TcpListener::bind(&config.listen_addr)
                .await
                .context("Failed to bind HTTP intake listener.")?;
            info!("antithesis-intake started: listening on {}.", config.listen_addr);

            axum::serve(listener, build_router(AppState::adp(&capture)))
                .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
                .await
                .map_err(Into::into)
        }
    }

    /// Resolve once the shutdown signal flips to `true`.
    async fn wait_for_shutdown(mut shutdown_rx: watch::Receiver<bool>) {
        let _ = shutdown_rx.wait_for(|signalled| *signalled).await;
    }

    /// Spawn a task that signals shutdown on SIGINT or SIGTERM.
    fn spawn_signal_handlers(shutdown_tx: watch::Sender<bool>) -> Result<()> {
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

            if let Err(e) = shutdown_tx.send(true) {
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
