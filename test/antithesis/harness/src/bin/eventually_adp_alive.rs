//! Antithesis `eventually_` liveness check: ADP booted and became reachable
//! within a bounded window.
//!
//! `eventually_` commands run in a fault-quiet period, so a node-fault induced
//! kill of ADP does not trip this check but a self-inflicted process exit
//! does. This triggers on ADP's own bugs, rather than antithesis fault
//! injection.
//!
//! We check two signals. First that ADP is reachable on :5100 and second that
//! it created a `DogStatsD` listener socket.

#[cfg(unix)]
mod unix_check {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::os::unix::fs::FileTypeExt;
    use std::path::PathBuf;
    use std::thread::sleep;
    use std::time::Duration;

    use antithesis_sdk::prelude::*;
    use clap::{builder::NonEmptyStringValueParser, Parser};
    use serde_json::json;

    #[derive(Debug, Parser)]
    #[command(name = "eventually_adp_alive")]
    struct Config {
        #[arg(
            long = "adp-api-addr",
            env = "ADP_API_ADDR",
            default_value = "adp:5100",
            value_parser = NonEmptyStringValueParser::new()
        )]
        adp_api_addr: String,
        #[arg(
            long = "dsd-socket",
            env = "DSD_SOCKET",
            default_value = "/var/run/datadog/dsd.socket"
        )]
        dsd_socket: PathBuf,
    }

    pub(super) fn run() -> anyhow::Result<()> {
        antithesis_init();
        let config = Config::try_parse()?;

        let mut api_reachable = false;
        let mut socket_present = false;
        // Check that the adp-api is reachable and the DogStatsD socket exists for
        // about 60 seconds. A 1s connect timeout keeps the poll cadence bounded
        // even when the API host is unresponsive.
        for _ in 0..60 {
            api_reachable = config
                .adp_api_addr
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok());
            socket_present = config.dsd_socket.metadata().is_ok_and(|m| m.file_type().is_socket());
            if api_reachable && socket_present {
                break;
            }
            sleep(Duration::from_secs(1));
        }

        assert_always!(
            api_reachable && socket_present,
            "ADP booted: API reachable and DogStatsD socket present",
            &json!({
                "adp_api_addr": config.adp_api_addr,
                "dsd_socket": config.dsd_socket.display().to_string(),
                "api_reachable": api_reachable,
                "socket_present": socket_present,
            })
        );

        Ok(())
    }
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix_check::run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("eventually_adp_alive checks a Unix domain socket and is only supported on Unix")
}
