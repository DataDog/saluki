use std::{ffi::OsString, io::Write, time::Duration};

use panoramic_courier::{Command, Invocation, Report};
use tokio::io::{AsyncRead, AsyncReadExt};

mod net;
mod probe;
mod send;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = std::env::args_os().skip(1).collect();
    let mut stderr = std::io::stderr();
    // Runtime shutdown would wait for a blocking stdin read even after the command's deadline.
    std::process::exit(run(args, tokio::io::stdin(), &mut stderr).await);
}

async fn run(args: Vec<OsString>, stdin: impl AsyncRead + Unpin, stderr: &mut impl Write) -> i32 {
    let invocation = args
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "arguments must be UTF-8".to_owned())
        .and_then(|args| Invocation::parse(args).map_err(|error| error.to_string()));
    let report = match invocation {
        Ok(invocation) => execute(&invocation, stdin).await,
        Err(reason) => Report::Usage { reason },
    };
    let mut line = serde_json::to_vec(&report).expect("reports contain only JSON-serializable values");
    line.push(b'\n');
    match stderr.write_all(&line) {
        Ok(()) => report.exit_code(),
        Err(error) => Report::Failed {
            reason: format!("writing stderr: {error}"),
        }
        .exit_code(),
    }
}

async fn execute(invocation: &Invocation, mut stdin: impl AsyncRead + Unpin) -> Report {
    let limit_ms = invocation.timeout_ms();
    let command = async {
        match &invocation.command {
            Command::Send { to } => {
                let mut payload = Vec::new();
                if let Err(error) = stdin.read_to_end(&mut payload).await {
                    return Report::Failed {
                        reason: format!("{to}: reading stdin: {error}"),
                    };
                }
                send::send(to, &payload).await
            }
            Command::Probe { to } => probe::probe(to).await,
        }
    };
    tokio::time::timeout(Duration::from_millis(limit_ms), command)
        .await
        .unwrap_or(Report::TimedOut { limit_ms })
}

#[cfg(test)]
mod tests;
