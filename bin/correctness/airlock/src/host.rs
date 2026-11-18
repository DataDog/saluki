//! Host programs as process groups, with streamed output and an exit that reports its signal.
//!
//! This is the host process primitive of the participant backends. [`unix`][crate::unix] serves the legacy runner and
//! keeps its own shape: it pushes lines into a sink and loses the signal of an exit.
//!
//! A [`HostProcess`] leads a process group, so it and its descendants can be signalled together. Its output is an
//! [`OutputStream`] of what the program writes to its standard output and error, and its exit is a watch that holds
//! how the program exited once it has. The pipes stay read until they close, even when the stream is dropped, so a
//! program is never blocked or signalled by its observer going away. Stopping sends SIGTERM to the group, then SIGKILL
//! after a grace. Dropping a process sends SIGKILL to its group.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::io::{AsyncRead, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio_util::io::ReaderStream;

use crate::container::{Output, OutputStream};

/// How long [`HostProcess::stop`] waits for the exit after SIGKILL.
#[cfg(unix)]
const KILL_WAIT: Duration = Duration::from_secs(2);

/// How many chunks of output wait for whoever reads them, before the program's pipes fill.
const OUTPUT_BUFFER: usize = 64;

/// A program to run.
#[derive(Clone, Debug)]
pub struct HostCommand {
    /// Names the process in messages.
    pub name: String,
    /// The file to execute.
    pub program: PathBuf,
    /// The arguments, not including the program itself.
    pub args: Vec<String>,
    /// The program's whole environment: nothing is inherited.
    pub env: BTreeMap<String, String>,
    /// The working directory.
    pub dir: PathBuf,
    /// Written to the program's standard input, which is then closed. Without it, the program has no standard input.
    pub stdin: Option<Bytes>,
}

/// How a program exited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostExit {
    /// The exit code, or `None` if a signal ended the program.
    pub code: Option<i32>,
    /// The signal that ended the program, where the OS reports one.
    pub signal: Option<i32>,
}

/// Holds `None` while the program runs, then how it exited, or why that cannot be told.
pub type ExitWatch = watch::Receiver<Option<Result<HostExit, String>>>;

/// A running program and its process group.
///
/// # Limits
///
/// - A descendant can leave the group, for example with `setsid`. Signals no longer reach it.
/// - `Drop` does not run if the owning process is killed with SIGKILL, so a program may outlive it.
/// - The OS may reuse the ID of an empty group before it is signalled, which would signal an unrelated group.
#[derive(Debug)]
pub struct HostProcess {
    name: String,
    #[cfg(unix)]
    exit: ExitWatch,
    /// The group to kill on drop, or `None` once it was sent SIGKILL.
    #[cfg(unix)]
    group: Option<i32>,
}

impl HostProcess {
    /// Starts `command` in a new process group.
    ///
    /// Returns the process, its output, and the watch of its exit.
    ///
    /// # Errors
    ///
    /// Returns an error if the program cannot be started.
    pub fn spawn(command: HostCommand) -> Result<(Self, OutputStream, ExitWatch), GenericError> {
        let HostCommand {
            name,
            program,
            args,
            env,
            dir,
            stdin,
        } = command;
        let mut process = Command::new(&program);
        process
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(dir)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        let mut child = process
            .spawn()
            .with_error_context(|| format!("Failed to start '{}' ({}).", name, program.display()))?;

        #[cfg(unix)]
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| generic_error!("No standard output."))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| generic_error!("No standard error."))?;
        if let (Some(payload), Some(mut input)) = (stdin, child.stdin.take()) {
            // A program that stops reading early is its own business: the write simply ends.
            tokio::spawn(async move {
                let _ = input.write_all(&payload).await;
                let _ = input.shutdown().await;
            });
        }

        let stdout = output_of(stdout, &name, "output", Output::Stdout);
        let stderr = output_of(stderr, &name, "error", Output::Stderr);
        let output = keep_draining(stream::select(stdout, stderr).boxed());

        let (sender, exit) = watch::channel(None);
        let waited = name.clone();
        // The task owns the child, so it is the only thing that reaps it.
        tokio::spawn(async move {
            let exited = match child.wait().await {
                Ok(status) => Ok(HostExit {
                    code: status.code(),
                    signal: signal_of(&status),
                }),
                Err(e) => Err(format!("Failed to wait for '{}': {}", waited, e)),
            };
            sender.send_replace(Some(exited));
        });

        let process = Self {
            name,
            #[cfg(unix)]
            exit: exit.clone(),
            #[cfg(unix)]
            group,
        };
        Ok((process, output, exit))
    }

    /// The name the process was given.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stops the process and its group: SIGTERM, then SIGKILL after `grace`, then up to two seconds for the exit.
    ///
    /// Whatever is left in the group is killed even when the program itself exited. Dropping the future before the
    /// SIGKILL drops the process, which sends SIGKILL.
    ///
    /// # Errors
    ///
    /// Returns an error if the process has not exited after the SIGKILL. A process on a target with no process groups
    /// is not signalled, and stopping it does nothing.
    #[cfg_attr(not(unix), allow(unused_mut, unused_variables))]
    pub async fn stop(mut self, grace: Duration) -> Result<(), GenericError> {
        #[cfg(unix)]
        {
            use tokio::time::timeout;

            if self.exit.borrow().is_none() {
                self.signal(libc::SIGTERM);
                let _ = timeout(grace, self.exit.wait_for(Option::is_some)).await;
            }
            self.signal(libc::SIGKILL);
            self.group = None;
            if timeout(KILL_WAIT, self.exit.wait_for(Option::is_some)).await.is_err() {
                return Err(generic_error!(
                    "'{}' did not exit within {:?} of SIGKILL",
                    self.name,
                    KILL_WAIT
                ));
            }
        }
        Ok(())
    }

    /// Sends `signal` to the group, ignoring failure: the group may already be empty.
    #[cfg(unix)]
    fn signal(&self, signal: libc::c_int) {
        if let Some(group) = self.group {
            // SAFETY: `killpg` takes no pointers and touches no memory of this process.
            unsafe {
                libc::killpg(group, signal);
            }
        }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.group.is_some() {
            self.signal(libc::SIGKILL);
            tracing::warn!(name = %self.name, "A host process was dropped before it was stopped. Killed its group.");
        }
    }
}

/// The chunks `reader` yields, as `tag` makes them, with `name`'s read errors.
fn output_of<R>(reader: R, name: &str, stream: &'static str, tag: fn(Bytes) -> Output) -> OutputStream
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let name = name.to_string();
    ReaderStream::new(reader)
        .map(move |chunk| match chunk {
            Ok(bytes) => Ok(tag(bytes)),
            Err(e) => Err(generic_error!(
                "Failed to read the standard {} of '{}': {}",
                stream,
                name,
                e
            )),
        })
        .boxed()
}

/// Passes on the chunks of `output`, and goes on reading it after the returned stream is dropped.
///
/// The stream owns the pipes only through the task, so whoever stops observing a program does not close its pipes,
/// which would make the program's next write fail while it is still running.
fn keep_draining(mut output: OutputStream) -> OutputStream {
    let (sender, receiver) = mpsc::channel(OUTPUT_BUFFER);
    tokio::spawn(async move {
        let mut observed = true;
        while let Some(chunk) = output.next().await {
            if observed && sender.send(chunk).await.is_err() {
                observed = false;
            }
        }
    });
    stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    })
    .boxed()
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use futures::StreamExt as _;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn text_of(chunk: Output) -> String {
        match chunk {
            Output::Stdout(bytes) | Output::Stderr(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    #[tokio::test]
    async fn stop_signals_the_whole_group_and_kills_what_ignores_sigterm() {
        // The subshell and its `sleep` ignore SIGTERM, so only SIGKILL ends them.
        let script = "trap 'echo got-term' TERM; (trap '' TERM; sleep 60) & echo ready; wait";
        let command = HostCommand {
            name: "test".to_string(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            env: BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap_or_default())]),
            dir: std::env::temp_dir(),
            stdin: None,
        };
        let (process, mut output, _exit) = HostProcess::spawn(command).unwrap();

        let mut text = String::new();
        tokio::time::timeout(WAIT, async {
            while !text.contains("ready") {
                text.push_str(&text_of(
                    output.next().await.expect("output ended before ready").unwrap(),
                ));
            }
        })
        .await
        .expect("the program should say ready");

        tokio::time::timeout(WAIT, process.stop(Duration::from_millis(500)))
            .await
            .expect("stop should return")
            .expect("stop should confirm the exit");

        // The output ends once nothing in the group holds its pipes.
        tokio::time::timeout(WAIT, async {
            while let Some(chunk) = output.next().await {
                text.push_str(&text_of(chunk.unwrap()));
            }
        })
        .await
        .expect("a process still holds the output pipes");
        assert!(text.contains("got-term"), "{text}");
    }
}
