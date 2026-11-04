//! Reaches endpoints and follows files in-process or through the courier executable.
//!
//! Callers and the executable share execution, argument handling and outcome decoding.

#![deny(missing_docs)]

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

mod args;
mod follow;
mod http;
mod net;
mod probe;
mod report;
mod send;
mod target;

pub use args::{Command, Invocation, UsageError};
pub use report::{Report, ReportError};
pub use target::Target;

/// Prefix emitted before a file follower starts copying bytes.
pub const FOLLOW_STARTED: &str = "panoramic-file-output\n";

/// Executes a command, streaming HTTP bodies and followed files to stdout.
///
/// Stdin left open at the deadline is a local failure; a network deadline produces a timeout report.
pub async fn execute(
    invocation: &Invocation, stdin: impl AsyncRead + Unpin, mut stdout: impl AsyncWrite + Unpin,
) -> Report {
    if let Command::Follow { path } = &invocation.command {
        if invocation.timeout.is_some() {
            return Report::Usage {
                reason: "follow does not accept --timeout-ms".into(),
            };
        }
        return follow::follow(path, &mut stdout).await;
    }
    execute_with(invocation, stdin, async |command, payload| match command {
        Command::Send { to } => send::send(to, payload).await,
        Command::Probe { to } => probe::probe(to).await,
        Command::Http { url, headers_only } => http::get(url, *headers_only, &mut stdout).await,
        Command::Follow { .. } => unreachable!(),
    })
    .await
}

async fn execute_with(
    invocation: &Invocation, mut stdin: impl AsyncRead + Unpin, run: impl AsyncFnOnce(&Command, &[u8]) -> Report,
) -> Report {
    let Some(limit_ms) = invocation.timeout_ms() else {
        return Report::Usage {
            reason: "missing --timeout-ms".into(),
        };
    };
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
