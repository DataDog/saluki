//! Misbehaving driver. Hammers one `DogStatsD` distribution context with one
//! value per `DDSketch` bucket, sweeping past `bin_limit` so the sketch collapses.
//! Distributions feed the agent `DDSketch`. Histograms do not.

#[cfg(unix)]
mod unix_driver {
    use std::os::unix::net::UnixDatagram;
    use std::path::{Path, PathBuf};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use antithesis_sdk::prelude::*;
    use clap::Parser;
    use serde_json::json;

    /// Agent `DDSketch` gamma. One step up the sweep is one bucket up.
    const GAMMA: f64 = 1.015_625;

    /// Buckets to fill. `bin_limit` is 4096, so this overshoots and forces a collapse.
    const STEPS: i32 = 4200;

    #[derive(Debug, Parser)]
    #[command(name = "parallel_driver_sketchburst")]
    struct Config {
        #[arg(
            long = "dogstatsd-socket",
            env = "DSD_SOCKET",
            default_value = "/var/run/datadog/dsd.socket"
        )]
        dogstatsd_socket: PathBuf,
    }

    pub(super) fn run() -> anyhow::Result<()> {
        antithesis_init();
        let config = Config::try_parse()?;

        // Socket unavailable (ADP booting, or a fault). No-op exit, not a failure.
        let Some(socket) = connect_with_retry(&config.dogstatsd_socket) else {
            return Ok(());
        };

        let mut sent = 0usize;
        let mut line = Vec::new();
        let mut ryu = ryu::Buffer::new();
        for i in 0..STEPS {
            let value = GAMMA.powi(i);
            line.clear();
            line.extend_from_slice(b"sketchburst:");
            line.extend_from_slice(ryu.format(value).as_bytes());
            line.extend_from_slice(b"|d\n");
            if socket.send(&line).is_ok() {
                sent += 1;
            }
        }

        assert_reachable!("sketchburst swept the sketch bins", &json!({ "sent": sent }));

        Ok(())
    }

    /// Wait for ADP to bind the socket, intentionally naive.
    fn connect_with_retry(path: &Path) -> Option<UnixDatagram> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(socket) = UnixDatagram::unbound() {
                if socket.connect(path).is_ok() {
                    return Some(socket);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix_driver::run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("parallel_driver_sketchburst requires Unix domain sockets and is only supported on Unix")
}
