//! Feral `DogStatsD` load generator. A producer thread writes sampled lines into
//! a bounded channel; a consumer thread ships them over the socket. Antithesis
//! runs many of these in parallel to drive concurrency and push context limits.

#[cfg(unix)]
mod unix_driver {
    use std::os::unix::net::UnixDatagram;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::sync_channel;
    use std::thread::{self, sleep};
    use std::time::{Duration, Instant};

    use antithesis_sdk::prelude::*;
    use antithesis_sdk::random::{random_choice, AntithesisRng};
    use clap::Parser;
    use harness::payload::dogstatsd;
    use rand::{rand_core::UnwrapErr, RngExt};
    use serde_json::json;

    #[derive(Debug, Parser)]
    #[command(name = "parallel_driver_send_dogstatsd")]
    struct Config {
        #[arg(
            long = "dogstatsd-socket",
            env = "DSD_SOCKET",
            default_value = "/var/run/datadog/dsd.socket"
        )]
        dogstatsd_socket: PathBuf,
    }

    /// Per-batch composition: 50% clean, 25% feral, 25% mixed.
    #[derive(Clone, Copy)]
    enum Batch {
        Clean,
        Feral,
        Mixed,
    }

    pub(super) fn run() -> anyhow::Result<()> {
        antithesis_init();

        let config = Config::try_parse()?;

        // Socket unavailable (ADP booting, or a fault). No-op exit, not a failure.
        let Some(socket) = connect_with_retry(&config.dogstatsd_socket) else {
            return Ok(());
        };

        let batch = match random_choice(&[Batch::Clean, Batch::Clean, Batch::Feral, Batch::Mixed]) {
            Some(Batch::Feral) => Batch::Feral,
            Some(Batch::Mixed) => Batch::Mixed,
            _ => Batch::Clean,
        };
        let count = {
            let mut rng = UnwrapErr(AntithesisRng);
            rng.random_range(0..=10_000u64)
        };

        let (tx, rx) = sync_channel::<Vec<u8>>(2024);

        let producer = thread::spawn(move || {
            let mut rng = UnwrapErr(AntithesisRng);
            let mut multi_value = false;
            for _ in 0..count {
                let vibe = match batch {
                    Batch::Clean => dogstatsd::Vibe::Clean,
                    Batch::Feral => dogstatsd::Vibe::Feral,
                    Batch::Mixed => dogstatsd::sample_vibe(),
                };
                let mut line = Vec::new();
                if dogstatsd::send(&mut rng, &mut line, vibe) {
                    multi_value = true;
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
            multi_value
        });

        let consumer = thread::spawn(move || {
            let mut attempted = 0usize;
            while let Ok(line) = rx.recv() {
                if socket.send(&line).is_ok() {
                    attempted += 1;
                }
            }
            attempted
        });

        let multi_value = producer.join().expect("producer thread panicked");
        let attempted = consumer.join().expect("consumer thread panicked");

        assert_reachable!(
            "workload ran a dogstatsd batch",
            &json!({ "attempted": attempted, "dogstatsd_socket": config.dogstatsd_socket.display().to_string() })
        );
        assert_sometimes!(
            attempted > 0,
            "workload sent a dogstatsd line",
            &json!({ "attempted": attempted })
        );
        assert_sometimes!(
            attempted > 0 && multi_value,
            "workload emitted a multi-value metric",
            &json!({ "attempted": attempted, "multi_value": multi_value })
        );
        assert_sometimes!(
            attempted > 0 && matches!(batch, Batch::Clean),
            "workload ran a fully clean batch",
            &json!({ "attempted": attempted })
        );
        assert_sometimes!(
            attempted > 0 && matches!(batch, Batch::Feral),
            "workload ran a fully feral batch",
            &json!({ "attempted": attempted })
        );
        assert_sometimes!(
            attempted > 0 && matches!(batch, Batch::Mixed),
            "workload ran a mixed batch",
            &json!({ "attempted": attempted })
        );

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
    anyhow::bail!("parallel_driver_send_dogstatsd requires Unix domain sockets and is only supported on Unix")
}
