//! Feral `DogStatsD` load generator for the differential scenario.
//!
//! Drives one batch of sampled lines, via the shared `harness::driver` engine,
//! to both the Datadog Agent socket and the ADP socket so their handling can be
//! compared.

#[cfg(unix)]
mod unix_driver {
    use std::path::PathBuf;

    use antithesis_sdk::prelude::*;
    use clap::Parser;
    use harness::driver;
    use serde_json::json;

    #[derive(Debug, Parser)]
    #[command(name = "parallel_driver_send_dogstatsd_differential")]
    struct Config {
        #[arg(
            long = "agent-dogstatsd-socket",
            env = "AGENT_DOGSTATSD_SOCKET",
            default_value = "/var/run/datadog-agent/dsd.socket"
        )]
        agent_dogstatsd_socket: PathBuf,
        #[arg(
            long = "adp-dogstatsd-socket",
            env = "ADP_DOGSTATSD_SOCKET",
            default_value = "/var/run/adp/dsd.socket"
        )]
        adp_dogstatsd_socket: PathBuf,
    }

    pub(super) fn run() -> anyhow::Result<()> {
        antithesis_init();

        let config = Config::try_parse()?;

        // Probe both sockets before bailing so each lane's connectivity is asserted independently. An
        // Agent-socket failure must not mask ADP's, and either outage attributes to the right lane.
        let agent = driver::connect_with_retry(&config.agent_dogstatsd_socket);
        let adp = driver::connect_with_retry(&config.adp_dogstatsd_socket);
        assert_sometimes!(
            agent.is_some(),
            "differential workload connected to Datadog Agent dogstatsd socket",
            &json!({ "agent_dogstatsd_socket": config.agent_dogstatsd_socket.display().to_string() })
        );
        assert_sometimes!(
            adp.is_some(),
            "differential workload connected to ADP dogstatsd socket",
            &json!({ "adp_dogstatsd_socket": config.adp_dogstatsd_socket.display().to_string() })
        );
        let (Some(agent_socket), Some(adp_socket)) = (agent, adp) else {
            return Ok(());
        };

        // Agent first, ADP second: `stats.sent` and `stats.max_packed` are indexed in this order.
        let batch = driver::sample();
        let stats = driver::run(batch, vec![agent_socket, adp_socket])?;
        let received = stats.received;
        let agent_sent = stats.sent[0];
        let adp_sent = stats.sent[1];
        let multi_value_both = stats.max_packed[0] > 0 && stats.max_packed[1] > 0;

        assert_reachable!(
            "differential workload ran a dogstatsd batch",
            &json!({
                "received": received,
                "agent_sent": agent_sent,
                "adp_sent": adp_sent,
                "agent_dogstatsd_socket": config.agent_dogstatsd_socket.display().to_string(),
                "adp_dogstatsd_socket": config.adp_dogstatsd_socket.display().to_string(),
            })
        );
        assert_sometimes!(
            agent_sent > 0 && adp_sent > 0,
            "differential workload sent a dogstatsd line to both targets",
            &json!({ "received": received, "agent_sent": agent_sent, "adp_sent": adp_sent })
        );
        assert_sometimes!(
            multi_value_both,
            "differential workload emitted a multi-value metric",
            &json!({ "received": received, "agent_sent": agent_sent, "adp_sent": adp_sent })
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
    anyhow::bail!(
        "parallel_driver_send_dogstatsd_differential requires Unix domain sockets and is only supported on Unix"
    )
}
