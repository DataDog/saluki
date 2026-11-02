use std::{ffi::OsString, io::Write};

use panoramic_courier::{execute, Invocation, Report};
use tokio::io::AsyncRead;

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

#[cfg(test)]
mod tests {
    use std::{future::Future, time::Duration};

    use panoramic_courier::{Command, Target};
    use tokio::{io::AsyncReadExt, net::TcpListener};

    use super::*;

    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("socket operation must finish within five seconds")
    }

    fn invocation(command: Command) -> Invocation {
        Invocation {
            command,
            timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn a_full_run_writes_one_report_line_and_exits_with_its_code() {
        within(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let to = Target::parse(&format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
            let mut stderr = Vec::new();
            let args = invocation(Command::Send { to })
                .args()
                .into_iter()
                .map(OsString::from)
                .collect();
            let code = run(args, &b"payload"[..], &mut stderr).await;
            assert_eq!(code, 0);
            assert_eq!(stderr.iter().filter(|byte| **byte == b'\n').count(), 1);
            assert_eq!(stderr.last(), Some(&b'\n'));
            assert_eq!(
                Report::from_exec(i64::from(code), &stderr).unwrap(),
                Report::Sent { bytes: 7 }
            );
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, b"payload");
        })
        .await;
    }

    #[tokio::test]
    async fn a_run_with_bad_args_reports_usage_and_exits_2() {
        let mut stderr = Vec::new();
        let (reader, _writer) = tokio::io::duplex(64);
        let code = within(run(vec!["invalid".into()], reader, &mut stderr)).await;
        assert_eq!(code, 2);
        assert_eq!(stderr.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(
            Report::from_exec(i64::from(code), &stderr).unwrap(),
            Report::Usage {
                reason: "unknown command \"invalid\"; use send or probe".into()
            }
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn non_utf8_arguments_report_usage_and_exit_2() {
        // Decode OS arguments explicitly so malformed text produces a usage report rather than an args() panic.
        #[cfg(unix)]
        let invalid = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![0xff])
        };
        #[cfg(windows)]
        let invalid = {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[0xd800])
        };
        let (reader, _writer) = tokio::io::duplex(64);
        let mut stderr = Vec::new();
        let code = within(run(vec![invalid], reader, &mut stderr)).await;
        assert_eq!(code, 2);
        assert_eq!(stderr.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(
            Report::from_exec(i64::from(code), &stderr).unwrap(),
            Report::Usage {
                reason: "arguments must be UTF-8".into()
            }
        );
    }
}
