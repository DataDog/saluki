//! The `courier` command line. The contract is in the `panoramic_courier` crate documentation.

use std::{
    io::{self, Read as _},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Parser, Subcommand};
use panoramic_courier::{Address, Error, Fault};
use tokio::io::AsyncWriteExt as _;

const EXIT_SOFTWARE: u8 = 1;
const EXIT_HARNESS: u8 = 2;

/// Probes, sends, HTTP requests and file follows, run from where the software runs.
#[derive(Debug, Parser)]
#[command(name = "courier")]
struct Cli {
    #[arg(
        long,
        value_parser = parse_duration,
        help = "How long the operation may take: a whole number with a unit of ms, s or m, such as 500ms, 10s or 2m"
    )]
    deadline: Duration,

    #[command(subcommand)]
    operation: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Checks that something accepts a connection at the address.
    Probe { address: Address },

    /// Writes standard input to the address as given: one connection, or one datagram.
    Send {
        address: Address,

        /// Standard input, read before the runtime starts.
        #[arg(skip)]
        payload: Vec<u8>,
    },

    /// Makes one plain-HTTP GET request, then writes the status code on one line and the body.
    Http {
        address: Address,

        /// The request path.
        #[arg(long, default_value = "/")]
        path: String,
    },

    /// Writes a file's contents as they are written, from the start, until the deadline.
    Follow { path: PathBuf },
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    // Read the payload first, so the operation on the runtime does no blocking I/O.
    if let Operation::Send { payload, .. } = &mut cli.operation {
        if let Err(source) = io::stdin().read_to_end(payload) {
            return failure(Error::Input { source });
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("courier: failed to start the runtime: {e}");
            return ExitCode::from(EXIT_HARNESS);
        }
    };
    let result = runtime.block_on(cli.run());
    // Dropping the runtime waits for blocking tasks, and a name lookup that hangs would hold the courier past its
    // deadline.
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failure(e),
    }
}

/// Reports `error` on standard error, and returns the exit code for who it blames.
fn failure(error: Error) -> ExitCode {
    eprintln!("courier: {error}");
    ExitCode::from(match error.fault() {
        Fault::Software => EXIT_SOFTWARE,
        Fault::Harness => EXIT_HARNESS,
    })
}

impl Cli {
    async fn run(self) -> Result<(), Error> {
        let limit = self.deadline;
        match self.operation {
            Operation::Probe { address } => panoramic_courier::probe(&address, limit).await,
            Operation::Send { address, payload } => panoramic_courier::send(&address, &payload, limit).await,
            Operation::Http { address, path } => {
                let response = panoramic_courier::http_get(&address, &path, limit).await?;
                let mut stdout = tokio::io::stdout();
                let status = format!("{}\n", response.status);
                let written = async {
                    stdout.write_all(status.as_bytes()).await?;
                    stdout.write_all(&response.body).await?;
                    stdout.flush().await
                };
                written.await.map_err(|source| Error::Output { source })
            }
            Operation::Follow { path } => panoramic_courier::follow(&path, limit, &mut tokio::io::stdout()).await,
        }
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("'{s}' does not start with a whole number"))?;
    let duration = match unit {
        "ms" => Duration::from_millis(number),
        "s" => Duration::from_secs(number),
        "m" => Duration::from_secs(number.saturating_mul(60)),
        _ => return Err(format!("'{s}' needs a unit of ms, s or m")),
    };
    if duration.is_zero() {
        return Err("the deadline must be greater than zero".to_string());
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_deadlines() {
        for text in ["", "10", "s", "1.5s", "1h", "-1s", "0ms"] {
            assert!(parse_duration(text).is_err(), "{text}");
        }
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
    }
}
