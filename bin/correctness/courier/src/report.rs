use serde::{Deserialize, Serialize};
use snafu::Snafu;

/// A courier outcome, serialized as one JSON line on stderr.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Report {
    /// The local transport accepted all stdin bytes; datagrams have no receipt guarantee.
    Sent {
        /// Number of bytes accepted by the transport.
        bytes: u64,
    },
    /// An HTTP server returned a response, regardless of status.
    Responded {
        /// HTTP status code.
        status: u16,
        /// Body bytes written to stdout; zero for headers-only requests.
        bytes: u64,
    },
    /// A TCP, Unix socket or named pipe connect succeeded.
    Listening,
    /// A UDP socket connected; this does not establish that a listener exists.
    Addressable,
    /// The command line was invalid.
    Usage {
        /// Invalid argument and how to correct it.
        reason: String,
    },
    /// Resolution or connection failed.
    Unreachable {
        /// I/O error and destination URL.
        reason: String,
    },
    /// Writing failed after the destination was resolved or connected.
    SendFailed {
        /// I/O error and destination URL, excluding payload bytes.
        reason: String,
    },
    /// Resolving, connecting or writing exceeded the budget shared with reading stdin.
    TimedOut {
        /// Requested budget in milliseconds.
        limit_ms: u64,
    },
    /// Local I/O failed, or stdin remained open past the command's budget.
    Failed {
        /// Local failure and destination URL, when applicable.
        reason: String,
    },
}

impl Report {
    /// Returns the process exit code for this outcome.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Sent { .. } | Self::Responded { .. } | Self::Listening | Self::Addressable => 0,
            Self::Usage { .. } => 2,
            Self::Unreachable { .. } => 10,
            Self::SendFailed { .. } => 11,
            Self::TimedOut { .. } => 12,
            Self::Failed { .. } => 13,
        }
    }

    /// Reads the last nonempty stderr line and checks that the outcome agrees with the exec exit code.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError::NotStarted`] for Docker launch-failure codes, [`ReportError::NoReport`] for
    /// missing or invalid JSON, or [`ReportError::Mismatch`] when the report contradicts the exit code.
    pub fn from_exec(exit_code: i64, stderr: &[u8]) -> Result<Self, ReportError> {
        if matches!(exit_code, 126 | 127) {
            return Err(ReportError::NotStarted { exit_code });
        }
        let report: Self = stderr
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.iter().all(u8::is_ascii_whitespace))
            .and_then(|line| serde_json::from_slice(line).ok())
            .ok_or(ReportError::NoReport { exit_code })?;
        if i64::from(report.exit_code()) != exit_code {
            return Err(ReportError::Mismatch {
                exit_code,
                outcome: report,
            });
        }
        Ok(report)
    }
}

/// An exec result that cannot be trusted as a courier outcome.
#[derive(Debug, Eq, PartialEq, Snafu)]
pub enum ReportError {
    /// Docker failed to start the executable.
    #[snafu(display(
        "The courier never ran (exit {exit_code}); check that the binary is installed, executable and built \
         for the container architecture"
    ))]
    NotStarted {
        /// Exit code returned by exec.
        exit_code: i64,
    },
    /// The last nonempty stderr line contained no valid report.
    #[snafu(display(
        "Courier exit {exit_code} has no valid report; inspect exec stderr for a crash or interrupted execution"
    ))]
    NoReport {
        /// Exit code returned by exec.
        exit_code: i64,
    },
    /// The report and exit status disagree.
    #[snafu(display(
        "Courier exit {exit_code} disagrees with outcome {outcome:?}; check the courier version and exec transport"
    ))]
    Mismatch {
        /// Exit code returned by exec.
        exit_code: i64,
        /// Report whose expected exit code differs.
        outcome: Report,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reports() -> [Report; 9] {
        [
            Report::Sent { bytes: 42 },
            Report::Responded { status: 503, bytes: 42 },
            Report::Listening,
            Report::Addressable,
            Report::Usage {
                reason: "missing --to".into(),
            },
            Report::Unreachable {
                reason: "tcp://h:1: refused".into(),
            },
            Report::SendFailed {
                reason: "unix:///run/a.sock: broken pipe".into(),
            },
            Report::TimedOut { limit_ms: 300 },
            Report::Failed {
                reason: "stdin: error\nwith a newline".into(),
            },
        ]
    }

    #[test]
    fn every_report_survives_its_stderr_line_and_exit_code() {
        for (report, expected_code) in reports().into_iter().zip([0, 0, 0, 0, 2, 10, 11, 12, 13]) {
            let mut line = serde_json::to_vec(&report).unwrap();
            line.push(b'\n');
            assert_eq!(report.exit_code(), expected_code);
            assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
            assert_eq!(Report::from_exec(i64::from(expected_code), &line).unwrap(), report);
        }
        assert_eq!(
            serde_json::to_string(&Report::Sent { bytes: 42 }).unwrap(),
            r#"{"outcome":"sent","bytes":42}"#
        );
    }

    #[test]
    fn no_report_uses_an_exit_code_docker_or_a_panic_uses() {
        for report in reports() {
            assert!(![101, 126, 127, 137].contains(&report.exit_code()));
        }
    }

    #[test]
    fn a_report_is_read_from_the_last_line_of_stderr() {
        let stderr = b"earlier diagnostic\n{\"outcome\":\"listening\"}\n \r\n";
        assert_eq!(Report::from_exec(0, stderr).unwrap(), Report::Listening);
    }

    #[test]
    fn exit_codes_126_and_127_mean_the_courier_never_ran() {
        for exit_code in [126, 127] {
            let error = Report::from_exec(exit_code, b"{\"outcome\":\"listening\"}\n").unwrap_err();
            assert_eq!(error, ReportError::NotStarted { exit_code });
            assert!(error.to_string().contains("never ran"));
        }
    }

    #[test]
    fn stderr_without_a_report_is_an_error_naming_the_exit_code() {
        for (exit_code, stderr) in [
            (101, &b"thread 'main' panicked at courier\n"[..]),
            (137, &b""[..]),
            (0, &b" \n\r\n"[..]),
            (0, &b"{\"outcome\":\"unknown\"}\n"[..]),
            (0, &b"{\"outcome\":\"listening\"}\ntrailing text\n"[..]),
            (0, &b"\xff\n"[..]),
        ] {
            let error = Report::from_exec(exit_code, stderr).unwrap_err();
            assert_eq!(error, ReportError::NoReport { exit_code });
            assert!(error.to_string().contains(&exit_code.to_string()));
        }
    }

    #[test]
    fn a_report_whose_outcome_disagrees_with_the_exit_code_is_an_error() {
        let error = Report::from_exec(10, b"{\"outcome\":\"listening\"}\n").unwrap_err();
        assert_eq!(
            error,
            ReportError::Mismatch {
                exit_code: 10,
                outcome: Report::Listening
            }
        );
        assert!(error.to_string().contains("10"));
        assert!(error.to_string().contains("Listening"));
    }
}
