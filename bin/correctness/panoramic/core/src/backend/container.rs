//! Observes container exits and output through the same Docker and archive APIs on either OS.

use std::{path::Path, sync::Arc};

use airlock::{driver::Driver, exec::ExecRequest};
use tokio::sync::watch;

use crate::{
    handle::{ExecCommand, ExecOutput, FileKind, FileStatus},
    observe::{
        exit_record::{watch_record, RecordRefresh},
        follow_output,
        output::{host_output_file, FileOutput},
        watch_exit, Capture, ExitObservation, OutputPaths, ParticipantExit, ParticipantObservation, ProcessObservation,
        ProcessOutput,
    },
    participant::{ExitChannel, OutputChannel, ProcessSpec},
    tasks::CaseTasks,
};

pub(super) struct ContainerObserver {
    pub(super) driver: Arc<Driver>,
    pub(super) name: String,
    pub(super) specs: &'static [ProcessSpec],
    // Include the container OS's trailing path separator.
    pub(super) exit_dir: String,
    pub(super) log_dir: String,
    pub(super) follower: fn(&str) -> ExecRequest,
    #[cfg(any(test, feature = "test-util"))]
    pub(super) output_line_bytes: Option<usize>,
}

impl ContainerObserver {
    pub(super) async fn observe(self, tasks: &CaseTasks, output_dir: &Path) -> ParticipantObservation {
        let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
        let (capture_tx, capture_rx) = watch::channel(Capture::Following);
        let mut observation = ParticipantObservation::new(
            exit_rx.clone(),
            capture_rx.clone(),
            output_dir.join("container.stdout.log"),
            output_dir.join("container.stderr.log"),
        );
        let Some(OutputPaths::Split { stdout, stderr }) = &observation.output_paths else {
            unreachable!("container output has separate streams");
        };
        #[cfg(any(test, feature = "test-util"))]
        if let Some(limit) = self.output_line_bytes {
            observation.lines = crate::observe::output::OutputLines::new(limit);
        }
        tasks.spawn(watch_exit(self.driver.wait_exit(), exit_tx));
        let mut container_output = None;
        for spec in self.specs {
            let process = match spec.exit {
                ExitChannel::Wait => unreachable!("host process exit channels are rejected during preparation"),
                ExitChannel::Container => ProcessObservation::Container(exit_rx.clone()),
                ExitChannel::Record { file } => {
                    let path = format!("{}{file}", self.exit_dir);
                    let driver = self.driver.clone();
                    let participant = self.name.clone();
                    let name = spec.name;
                    let exit = exit_rx.clone();
                    let (tx, rx) = watch::channel(ExitObservation::Running);
                    let (requests, requests_rx) = watch::channel(0);
                    let (served_tx, served) = watch::channel(0);
                    tasks.spawn(async move {
                        watch_record(
                            || driver.read_file(&path),
                            exit,
                            tx,
                            &participant,
                            name,
                            &path,
                            (requests_rx, served_tx),
                        )
                        .await;
                    });
                    ProcessObservation::Record {
                        status: rx,
                        refresh: Some(Arc::new(RecordRefresh { requests, served })),
                    }
                }
            };
            observation.processes.insert(spec.name, process);
            let output = match spec.output {
                OutputChannel::Pipes => unreachable!("host process output channels are rejected during preparation"),
                OutputChannel::Container => {
                    container_output = Some(spec.name);
                    ProcessOutput {
                        capture: capture_rx.clone(),
                        paths: vec![stdout.clone(), stderr.clone()],
                    }
                }
                OutputChannel::File { file, .. } => {
                    let path = format!("{}{file}", self.log_dir);
                    let filename = host_output_file(spec.name, spec.output).expect("file output has a host file name");
                    let host_path = output_dir.join(filename);
                    let output = FileOutput {
                        participant: self.name.clone().into(),
                        process: spec.name,
                        source_path: path.clone(),
                        host_path: host_path.clone(),
                        lines: observation.lines.clone(),
                    };
                    let driver = self.driver.clone();
                    let follower = self.follower;
                    let exit = exit_rx.clone();
                    let (tx, rx) = watch::channel(Capture::Following);
                    tasks.spawn(async move {
                        output
                            .follow(
                                driver.exec_stream(follower(&path)),
                                || driver.read_file(&path),
                                exit,
                                tx,
                            )
                            .await;
                    });
                    ProcessOutput {
                        capture: rx,
                        paths: vec![host_path],
                    }
                }
            };
            observation.outputs.insert(spec.name, output);
        }
        match self.driver.logs().await {
            Ok(stream) => {
                tasks.spawn(follow_output(
                    stream,
                    stdout.clone(),
                    stderr.clone(),
                    capture_tx,
                    observation.lines.clone(),
                    container_output,
                ));
            }
            Err(error) => {
                capture_tx.send_replace(Capture::Failed {
                    reason: format!("{error:#}"),
                });
            }
        }
        observation
    }
}

impl From<airlock::exec::ExecOutput> for ExecOutput {
    fn from(output: airlock::exec::ExecOutput) -> Self {
        Self {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
        }
    }
}

impl From<airlock::files::FileStat> for FileStatus {
    fn from(status: airlock::files::FileStat) -> Self {
        use airlock::files::FileKind as BackendKind;
        let kind = match status.kind {
            BackendKind::File => FileKind::File,
            BackendKind::Directory => FileKind::Directory,
            BackendKind::Symlink { target } => FileKind::Symlink { target },
            BackendKind::Socket => FileKind::Socket,
            BackendKind::NamedPipe => FileKind::NamedPipe,
            BackendKind::Other => FileKind::Other,
        };
        Self {
            kind,
            mode: status.mode,
            size: status.size,
        }
    }
}

pub(super) fn exec_request(command: ExecCommand) -> ExecRequest {
    let mut request = ExecRequest::new(command.argv);
    for (key, value) in command.env {
        request = request.with_env_var(key, value);
    }
    if let Some(stdin) = command.stdin {
        request = request.with_stdin(stdin);
    }
    request
}
