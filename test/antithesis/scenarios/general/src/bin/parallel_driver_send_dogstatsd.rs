//! `DogStatsD` load generator. Fetches a context working set from the intake and
//! drives packed metric lines to the dogstatsd socket via the shared
//! `harness::driver` engine. Antithesis runs many of these in parallel to drive
//! concurrency and push context limits.

#[cfg(unix)]
mod unix_driver {
    use std::path::PathBuf;

    use antithesis_sdk::prelude::*;
    use antithesis_sdk::random::AntithesisRng;
    use clap::Parser;
    use harness::config::DriverConfig;
    use harness::driver;
    use serde_json::json;

    #[derive(Debug, Parser)]
    #[command(name = "parallel_driver_send_dogstatsd")]
    struct Config {
        #[arg(
            long = "dogstatsd-socket",
            env = "ADP_DOGSTATSD_SOCKET",
            default_value = "/var/run/datadog/dsd.socket"
        )]
        dogstatsd_socket: PathBuf,
        /// Directory holding this timeline's `driver.yaml`, read to cap payloads
        /// to the SUT's sampled receive buffer.
        #[arg(long = "config-dir", env = "CONFIG_DIR", default_value = "/agent-config")]
        config_dir: PathBuf,
        /// Intake address the driver samples its context working set from.
        #[arg(long = "intake-addr", env = "INTAKE_ADDR", default_value = "intake:2049")]
        intake_addr: String,
    }

    pub(super) fn run() -> anyhow::Result<()> {
        antithesis_init();

        let config = Config::try_parse()?;

        // Socket unavailable (ADP booting, or a fault). No-op exit, not a failure.
        let Some(socket) = driver::connect_with_retry(&config.dogstatsd_socket) else {
            return Ok(());
        };

        let driver_config = DriverConfig::read(&config.config_dir)?;
        let stats = driver::run(
            AntithesisRng,
            &config.intake_addr,
            driver_config.payload_byte_limit,
            driver_config.datagram_count,
            vec![socket],
        )?;
        let sent = stats.sent[0];
        let max_packed = stats.max_packed[0];

        assert_reachable!(
            "workload ran a dogstatsd load",
            &json!({
                "sent": sent,
                "timed_out": stats.timed_out,
                "dogstatsd_socket": config.dogstatsd_socket.display().to_string()
            })
        );
        assert_sometimes!(sent > 0, "workload sent a dogstatsd line", &json!({ "sent": sent }));
        assert_sometimes!(
            max_packed > 0,
            "workload emitted a multi-value metric",
            &json!({ "sent": sent, "max_packed_values": max_packed })
        );

        Ok(())
    }
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix_driver::run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("parallel_driver_send_dogstatsd requires Unix domain sockets and is only supported on Unix")
}
