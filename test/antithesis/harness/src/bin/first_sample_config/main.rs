//! Antithesis `first_` command: sample this timeline's `datadog.yaml` and release ADP.
//!
//! Runs once per execution path after `setup_complete`, so the sample (see
//! [`harness::config`], Antithesis SDK randomness) is a post-snapshot, per-timeline
//! decision Antithesis branches. Writes the config to the shared `agent-config`
//! volume then a `ready` sentinel the blocked ADP entrypoint waits on; running
//! upstream of ADP's boot is what makes each timeline boot under its own config.
//! Deployment fields come from the environment (see [`Cli`]).

use std::fs;
use std::path::PathBuf;

use antithesis_sdk::prelude::*;
use antithesis_sdk::random::AntithesisRng;
use anyhow::Context as _;
use clap::Parser;
use harness::config::{ConfigProfile, DatadogConfig};
use rand::rand_core::UnwrapErr;
use serde_json::json;

/// Deployment inputs, sourced from the environment (or flags).
#[derive(Debug, Parser)]
#[command(name = "first_sample_config")]
struct Cli {
    /// Directory to write `datadog.yaml` and the `ready` sentinel into (shared
    /// `agent-config` volume; the ADP container reads it).
    #[arg(long, env = "CONFIG_DIR", default_value = "/agent-config")]
    config_dir: PathBuf,
    /// Which `datadog.yaml` variation to sample. The differential scenario sets
    /// `CONFIG_PROFILE=differential` so both targets share a config they can both
    /// honor; ADP-only scenarios keep the default full feral surface.
    #[arg(long, env = "CONFIG_PROFILE", value_enum, default_value_t = ConfigProfile::General)]
    profile: ConfigProfile,
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

    fs::create_dir_all(&cli.config_dir)
        .with_context(|| format!("create agent config dir {}", cli.config_dir.display()))?;

    let mut rng = UnwrapErr(AntithesisRng);
    let config = DatadogConfig::sample(
        &mut rng,
        &cli.hostname,
        &cli.api_key,
        &cli.dd_url,
        &cli.dogstatsd_socket,
        cli.profile,
    );

    let yaml_path = cli.config_dir.join("datadog.yaml");
    fs::write(&yaml_path, config.to_yaml()?.as_bytes())
        .with_context(|| format!("write agent config {}", yaml_path.display()))?;

    // Per-timeline anchor: counting these in triage tells us how many distinct
    // configs the run sampled.
    let details = serde_json::to_value(&config).unwrap_or_else(|e| json!({ "serialize_error": e.to_string() }));
    assert_reachable!("first_sample_config.config_sampled", &details);

    // Release ADP: it blocks on this sentinel, then boots under the config above.
    let ready_path = cli.config_dir.join("ready");
    fs::write(&ready_path, b"ready\n").with_context(|| format!("write sentinel {}", ready_path.display()))?;
    Ok(())
}
