use std::time::Duration;

use snafu::Snafu;

use crate::Target;

/// An operation performed in one courier process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Send stdin through one connection or as one datagram.
    Send {
        /// Destination endpoint.
        to: Target,
    },
    /// Connect to an endpoint without sending data.
    Probe {
        /// Endpoint to probe.
        to: Target,
    },
}

/// A command and its whole-process time budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    /// Operation to execute.
    pub command: Command,
    /// Budget for reading stdin, resolving, connecting and writing.
    pub timeout: Duration,
}

impl Invocation {
    /// Returns the wire budget, rounded up to milliseconds and clamped to `1..=u64::MAX`.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout
            .as_nanos()
            .div_ceil(1_000_000)
            .clamp(1, u128::from(u64::MAX)) as u64
    }

    /// Renders arguments after the executable name, rounding the budget up to whole milliseconds.
    pub fn args(&self) -> Vec<String> {
        let (command, to) = match &self.command {
            Command::Send { to } => ("send", to),
            Command::Probe { to } => ("probe", to),
        };
        vec![
            command.into(),
            "--to".into(),
            to.to_string(),
            "--timeout-ms".into(),
            self.timeout_ms().to_string(),
        ]
    }

    /// Parses arguments after the executable name. Both flags are required and may appear in either order.
    ///
    /// # Errors
    ///
    /// Returns a usage error for an unknown command, missing, repeated or unknown flags, or invalid values.
    pub fn parse(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self, UsageError> {
        let mut args = args.into_iter();
        let command = args.next().ok_or_else(|| UsageError {
            reason: "missing command; use send or probe".into(),
        })?;
        if !matches!(command.as_ref(), "send" | "probe") {
            return Err(UsageError {
                reason: format!("unknown command {:?}; use send or probe", command.as_ref()),
            });
        }
        let mut to = None;
        let mut timeout = None;
        while let Some(flag) = args.next() {
            let flag = flag.as_ref();
            let repeated = match flag {
                "--to" => to.is_some(),
                "--timeout-ms" => timeout.is_some(),
                _ => {
                    return Err(UsageError {
                        reason: format!("unknown flag {flag:?}"),
                    })
                }
            };
            if repeated {
                return Err(UsageError {
                    reason: format!("repeated flag {flag}"),
                });
            }
            let value = args.next().ok_or_else(|| UsageError {
                reason: format!("{flag} requires a value"),
            })?;
            let value = value.as_ref();
            if value.starts_with("--") {
                return Err(UsageError {
                    reason: format!("{flag} requires a value, got flag {value:?}"),
                });
            }
            if flag == "--to" {
                to = Some(Target::parse(value)?);
            } else {
                let milliseconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|number| *number > 0 && value.bytes().all(|byte| byte.is_ascii_digit()))
                    .ok_or_else(|| UsageError {
                        reason: format!(
                            "--timeout-ms must be a decimal integer from 1 through {}; got {value:?}",
                            u64::MAX
                        ),
                    })?;
                timeout = Some(Duration::from_millis(milliseconds));
            }
        }
        let to = to.ok_or_else(|| UsageError {
            reason: "missing --to".into(),
        })?;
        let timeout = timeout.ok_or_else(|| UsageError {
            reason: "missing --timeout-ms".into(),
        })?;
        let command = if command.as_ref() == "send" {
            Command::Send { to }
        } else {
            Command::Probe { to }
        };
        Ok(Self { command, timeout })
    }
}

/// Invalid courier arguments.
#[derive(Debug, Snafu)]
#[snafu(display("{reason}"))]
pub struct UsageError {
    pub(crate) reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_invocation_renders_args_that_parse_back_to_itself() {
        for url in [
            "tcp://localhost:8125",
            "tcp://127.0.0.1:65535",
            "tcp://[::1]:8125",
            "udp://localhost:8125",
            "udp://[::1]:8125",
            "unix:///run/a.sock",
            "unixgram:///run/a.sock",
        ] {
            let to = Target::parse(url).unwrap();
            for command in [Command::Send { to: to.clone() }, Command::Probe { to }] {
                let invocation = Invocation {
                    command,
                    timeout: Duration::from_millis(1500),
                };
                let args = invocation.args();
                assert_eq!(Invocation::parse(&args).unwrap(), invocation);
                let reversed_flags = [&args[0], &args[3], &args[4], &args[1], &args[2]];
                assert_eq!(Invocation::parse(reversed_flags).unwrap(), invocation);
            }
        }
    }

    #[test]
    fn malformed_args_are_usage_errors_that_name_the_problem() {
        for (args, problem) in [
            ("", "command"),
            ("http", "http"),
            ("send --timeout-ms 1", "--to"),
            ("probe --to tcp://h:1", "--timeout-ms"),
            ("send --to tcp://h:1 --to tcp://h:2", "--to"),
            ("probe --timeout-ms 1 --timeout-ms 2", "--timeout-ms"),
            ("send --wat 1", "--wat"),
            ("send --to", "--to"),
            ("send --timeout-ms", "--timeout-ms"),
            ("send --to --timeout-ms 1", "--to"),
            ("send --to tcp://h:1 --timeout-ms 0", "got \"0\""),
            ("send --to tcp://h:1 --timeout-ms abc", "got \"abc\""),
            ("send --to tcp://h:1 --timeout-ms +1", "got \"+1\""),
            ("send --to tcp://h:1 --timeout-ms -1", "got \"-1\""),
            (
                "send --to tcp://h:1 --timeout-ms 18446744073709551616",
                "got \"18446744073709551616\"",
            ),
            ("send --to http://h:1 --timeout-ms 1", "http://h:1"),
            ("send --to unix://a.sock --timeout-ms 1", "unix://a.sock"),
            ("send --to unixgram://a.sock --timeout-ms 1", "unixgram://a.sock"),
            ("send --to tcp://h --timeout-ms 1", "tcp://h"),
            ("send --to tcp://h:0 --timeout-ms 1", "tcp://h:0"),
            ("send --to tcp://h:65536 --timeout-ms 1", "tcp://h:65536"),
            ("send --to tcp://:1 --timeout-ms 1", "tcp://:1"),
            ("send --to tcp://h:+1 --timeout-ms 1", "tcp://h:+1"),
            ("send --to tcp://h:1/path --timeout-ms 1", "tcp://h:1/path"),
            ("send --to tcp://[nope]:1 --timeout-ms 1", "tcp://[nope]:1"),
            ("send --to tcp://[::1:1 --timeout-ms 1", "tcp://[::1:1"),
            ("send --to tcp://::1:1 --timeout-ms 1", "tcp://::1:1"),
            ("send --to tcp://h:1 --timeout-ms 1 extra", "extra"),
        ] {
            let error = Invocation::parse(args.split_whitespace()).unwrap_err().to_string();
            assert!(error.contains(problem), "{args:?}: {error:?} must name {problem:?}");
        }
    }

    #[test]
    fn a_sub_millisecond_timeout_renders_as_one_millisecond() {
        for (timeout, expected) in [
            (Duration::ZERO, "1"),
            (Duration::from_nanos(1), "1"),
            (Duration::from_micros(999), "1"),
            (Duration::from_micros(1001), "2"),
            (Duration::MAX, "18446744073709551615"),
        ] {
            let invocation = Invocation {
                command: Command::Probe {
                    to: Target::parse("tcp://h:1").unwrap(),
                },
                timeout,
            };
            assert_eq!(invocation.args()[4], expected);
            assert_eq!(
                Invocation::parse(invocation.args())
                    .unwrap()
                    .timeout
                    .as_millis()
                    .to_string(),
                expected
            );
        }
    }
}
