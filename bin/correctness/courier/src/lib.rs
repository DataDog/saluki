//! Sends to and probes endpoints in-process or through the courier executable.
//!
//! Callers and the executable share execution, argument handling and outcome decoding.

#![deny(missing_docs)]

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

mod args;
mod net;
mod probe;
mod report;
mod send;
mod target;

pub use args::{Command, Invocation, UsageError};
pub use report::{Report, ReportError};
pub use target::Target;

/// Executes a send or probe within one deadline shared by stdin and network I/O.
///
/// Stdin left open at the deadline is a local failure; a network deadline produces a timeout report.
pub async fn execute(invocation: &Invocation, stdin: impl AsyncRead + Unpin) -> Report {
    execute_with(invocation, stdin, async |command, payload| match command {
        Command::Send { to } => send::send(to, payload).await,
        Command::Probe { to } => probe::probe(to).await,
    })
    .await
}

async fn execute_with(
    invocation: &Invocation, mut stdin: impl AsyncRead + Unpin, run: impl AsyncFnOnce(&Command, &[u8]) -> Report,
) -> Report {
    let limit_ms = invocation.timeout_ms();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(limit_ms);
    let mut payload = Vec::new();
    if let Command::Send { to } = &invocation.command {
        match tokio::time::timeout_at(deadline, stdin.read_to_end(&mut payload)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                return Report::Failed {
                    reason: format!("{to}: reading stdin: {error}"),
                };
            }
            Err(_) => {
                return Report::Failed {
                    reason: format!("{to}: stdin was still open after the {limit_ms} ms limit"),
                };
            }
        }
    }
    tokio::time::timeout_at(deadline, run(&invocation.command, &payload))
        .await
        .unwrap_or(Report::TimedOut { limit_ms })
}

#[cfg(test)]
mod tests;
