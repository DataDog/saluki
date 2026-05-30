//! Generates a randomized, valid `datadog.yaml` for the Agent Data Plane (ADP)
//! per Antithesis replay branch.
//!
//! After `setup_complete`, this generates a `datadog.yaml` (see [`config`])
//! whose `DogStatsD` options are drawn with Antithesis SDK randomness, writes it
//! to the shared `agent-config` volume, and touches a `ready` sentinel. ADP's
//! boot wrapper (`deploy/adp/entrypoint.sh`) copies the yaml into place and
//! execs the agent.
//!
//! The draw runs *after* the snapshot boundary so each Antithesis replay branch
//! gets a different config and ADP boots fresh with it. Deployment-specific
//! fields come from the environment (see [`Cli`]).

mod config;

use std::fs;
use std::path::PathBuf;

use antithesis_sdk::prelude::*;
use antithesis_sdk::random::AntithesisRng;
use anyhow::Context as _;
use clap::Parser;
use config::DatadogConfig;
use rand::rand_core::UnwrapErr;
use serde_json::json;

/// Deployment inputs, sourced from the environment (or flags).
#[derive(Debug, Parser)]
#[command(name = "datadog-yaml-config-gen")]
struct Cli {
    /// Directory to write `datadog.yaml` and the `ready` sentinel into.
    #[arg(long, env = "CONFIG_DIR", default_value = "/agent-config")]
    config_dir: PathBuf,
    /// Agent hostname written into the config. (`DD_HOSTNAME`, not the ambient
    /// `HOSTNAME`, so a container's own hostname does not leak in.)
    #[arg(long, env = "DD_HOSTNAME", default_value = "antithesis-adp")]
    hostname: String,
    /// Agent API key written into the config.
    #[arg(long, env = "API_KEY", default_value = "antithesis-test-api-key")]
    api_key: String,
    /// Metrics intake base URL.
    #[arg(long, env = "DD_URL", default_value = "http://intake:2049")]
    dd_url: String,
    /// `DogStatsD` unix datagram socket path.
    #[arg(long, env = "DOGSTATSD_SOCKET", default_value = "/var/run/datadog/dsd.socket")]
    dogstatsd_socket: PathBuf,
}

fn main() -> anyhow::Result<()> {
    antithesis_init();
    let cli = Cli::parse();

    // Snapshot boundary. The draw below must happen AFTER this so each replay
    // branch generates an independent config.
    lifecycle::setup_complete(&json!({ "component": "datadog-yaml-config-gen" }));

    fs::create_dir_all(&cli.config_dir)
        .with_context(|| format!("create agent config dir {}", cli.config_dir.display()))?;

    let mut rng = UnwrapErr(AntithesisRng);
    let config = DatadogConfig::gen(
        &mut rng,
        &cli.hostname,
        &cli.api_key,
        &cli.dd_url,
        &cli.dogstatsd_socket,
    );

    let yaml_path = cli.config_dir.join("datadog.yaml");
    fs::write(&yaml_path, config.to_yaml()?.as_bytes())
        .with_context(|| format!("write agent config {}", yaml_path.display()))?;

    let details = serde_json::to_value(&config).unwrap_or_else(|_| json!({}));
    assert_reachable!("datadog_yaml_config_gen.datadog_yaml_generated", &details);

    let ready_path = cli.config_dir.join("ready");
    fs::write(&ready_path, b"ready\n").with_context(|| format!("write sentinel {}", ready_path.display()))?;
    Ok(())
}
