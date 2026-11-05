//! Observes pod container exits, merged logs, and node-backed process records and files.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use airlock::exec::OutputChunk;
use futures::{io::AsyncRead, stream, AsyncReadExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::Pod;
use kube::{api::LogParams, Api};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{fs::File, io::AsyncWriteExt as _, sync::watch, time::sleep};

use super::{node::Node, PodParticipant};
use crate::{
    handle::Primitives as _,
    observe::{
        exit_record::{watch_record, RecordRefresh},
        output::{host_output_file, FileOutput, LineBuffer, FILE_OUTPUT_START},
        Capture, ExitObservation, ExitStatus, ParticipantExit, ParticipantObservation, ProcessObservation,
        ProcessOutput,
    },
    participant::{ExitChannel, OutputChannel, OutputStream},
    probe::POLL_INTERVAL,
    tasks::CaseTasks,
};

impl PodParticipant {
    pub(super) async fn observation(self: Arc<Self>, tasks: &CaseTasks, output_dir: &Path) -> ParticipantObservation {
        let paths: BTreeMap<_, _> = self
            .manifests
            .pod
            .spec
            .as_ref()
            .expect("a pod manifest has a spec")
            .containers
            .iter()
            .map(|container| {
                (
                    container.name.clone(),
                    output_dir.join("containers").join(format!("{}.log", container.name)),
                )
            })
            .collect();
        let primary_path = paths[&self.primary].clone();
        let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
        let (capture_tx, capture_rx) = watch::channel(Capture::Following);
        let mut observation = ParticipantObservation::merged(exit_rx.clone(), capture_rx, primary_path);
        let output_owner = self.specs.iter().find(|spec| spec.output == OutputChannel::Container);
        let mut containers = BTreeMap::new();
        let mut exits = Vec::new();
        let mut captures = Vec::new();
        for (container, path) in paths {
            let (tx, status) = watch::channel(ExitObservation::Running);
            let process = ProcessObservation::ContainerStatus(status);
            exits.push((container.clone(), tx));
            let (tx, capture) = watch::channel(Capture::Following);
            captures.push((container.clone(), capture.clone()));
            let lines = LineBuffer::new(
                observation.lines.clone(),
                self.name.clone().into(),
                output_owner.filter(|_| container == self.primary).map(|spec| spec.name),
                OutputStream::Merged,
            );
            let participant = self.clone();
            let exit = process.clone();
            let host_path = path.clone();
            let name = container.clone();
            tasks.spawn(async move {
                let pods: Api<Pod> = Api::namespaced(participant.client.clone(), &participant.namespace);
                capture_logs(
                    |follow| {
                        let pods = pods.clone();
                        let pod = participant.name.clone();
                        let params = LogParams {
                            container: Some(name.clone()),
                            follow,
                            ..Default::default()
                        };
                        async move { pods.log_stream(&pod, &params).await.map_err(Into::into) }
                    },
                    host_path,
                    lines,
                    exit,
                    tx,
                )
                .await;
            });
            containers.insert(
                container,
                (
                    process,
                    ProcessOutput {
                        capture,
                        paths: vec![path],
                    },
                ),
            );
        }
        let participant = self.clone();
        tasks.spawn(async move {
            let pods: Api<Pod> = Api::namespaced(participant.client.clone(), &participant.namespace);
            watch_containers(
                || async {
                    pods.get(&participant.name).await.with_error_context(|| {
                        format!("cannot observe pod '{}/{}'", participant.namespace, participant.name)
                    })
                },
                exits,
                exit_tx,
            )
            .await;
        });
        tasks.spawn(watch_captures(captures, capture_tx));
        let (primary_exit, primary_output) = containers.remove(&self.primary).expect("the primary container exists");
        for spec in self.specs {
            let process = match spec.exit {
                ExitChannel::Wait => unreachable!("host process exit channels are rejected during preparation"),
                ExitChannel::Container => primary_exit.clone(),
                ExitChannel::Record { file } => {
                    let path = format!("/airlock/{}/exit/{file}", self.name);
                    let participant = self.clone();
                    let exit = exit_rx.clone();
                    let (tx, status) = watch::channel(ExitObservation::Running);
                    let (requests, requests_rx) = watch::channel(0);
                    let (served_tx, served) = watch::channel(0);
                    tasks.spawn(async move {
                        watch_record(
                            || async { participant.read_file(&path).await.map_err(Into::into) },
                            exit,
                            tx,
                            &participant.name,
                            spec.name,
                            &path,
                            (requests_rx, served_tx),
                        )
                        .await;
                    });
                    ProcessObservation::Record {
                        status,
                        refresh: Some(Arc::new(RecordRefresh { requests, served })),
                    }
                }
            };
            observation.processes.insert(spec.name, process);
            let output = match spec.output {
                OutputChannel::Pipes => unreachable!("host process output channels are rejected during preparation"),
                OutputChannel::Container => ProcessOutput {
                    capture: primary_output.capture.clone(),
                    paths: primary_output.paths.clone(),
                },
                OutputChannel::File { file, stream } => {
                    let source_path = format!("/airlock/{}/log/{file}", self.name);
                    let host_path = output_dir.join(host_output_file(spec.name, spec.output).expect("file output"));
                    let output = FileOutput {
                        participant: self.name.clone().into(),
                        process: spec.name,
                        stream,
                        source_path: source_path.clone(),
                        host_path: host_path.clone(),
                        lines: observation.lines.clone(),
                    };
                    let participant = self.clone();
                    let exit = exit_rx.clone();
                    let (tx, capture) = watch::channel(Capture::Following);
                    tasks.spawn(async move {
                        output
                            .follow(
                                async {
                                    let path = participant.node_path(&source_path)?;
                                    Ok(file_chunks(participant.node.clone(), path))
                                },
                                || async { participant.read_file(&source_path).await.map_err(Into::into) },
                                exit,
                                tx,
                            )
                            .await;
                    });
                    ProcessOutput {
                        capture,
                        paths: vec![host_path],
                    }
                }
            };
            observation.outputs.insert(spec.name, output);
        }
        observation
    }
}

fn container_status(pod: &Pod, container: &str) -> ExitObservation {
    let status = pod.status.as_ref();
    let terminated = status
        .and_then(|status| status.container_statuses.as_ref())
        .and_then(|statuses| statuses.iter().find(|status| status.name == container))
        .and_then(|status| status.state.as_ref())
        .and_then(|state| state.terminated.as_ref());
    if let Some(terminated) = terminated {
        let reason = [terminated.reason.as_deref(), terminated.message.as_deref()]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(": ");
        if terminated.reason.as_deref() == Some("ContainerStatusUnknown") {
            return ExitObservation::Undetermined(format!("container '{container}': {reason}"));
        }
        if terminated.signal.is_some_and(|signal| signal < 0) {
            return ExitObservation::Undetermined(format!("container '{container}' has a negative termination signal"));
        }
        let signal = terminated.signal.filter(|signal| *signal > 0);
        return ExitObservation::Exited(ExitStatus {
            code: signal.is_none().then_some(terminated.exit_code),
            signal,
            reason: (!reason.is_empty()).then_some(reason),
            recorded_at: terminated.finished_at.as_ref().map(|time| time.0.into()),
        });
    }
    if status.is_some_and(|status| matches!(status.phase.as_deref(), Some("Succeeded" | "Failed" | "Unknown"))) {
        let status = status.expect("unsettled container in a stopped or unknown pod");
        let reason = [status.reason.as_deref(), status.message.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(": ");
        return ExitObservation::Undetermined(format!(
            "pod status {} has no termination status for container '{container}': {reason}",
            status.phase.as_deref().unwrap_or_default()
        ));
    }
    ExitObservation::Running
}

fn participant_exit(exits: &[ExitObservation]) -> ParticipantExit {
    let errors = exits
        .iter()
        .filter_map(|exit| match exit {
            ExitObservation::Undetermined(reason) => Some(reason.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        return ParticipantExit::Undetermined {
            reason: errors.join("; "),
        };
    }
    if exits.contains(&ExitObservation::Running) {
        return ParticipantExit::Running;
    }
    match exits {
        [ExitObservation::Exited(ExitStatus {
            code: Some(code),
            reason,
            ..
        })] => ParticipantExit::Exited {
            code: i64::from(*code),
            error: reason.clone(),
        },
        _ => ParticipantExit::Ended,
    }
}

async fn watch_containers<F>(
    mut read: impl FnMut() -> F, containers: Vec<(String, watch::Sender<ExitObservation>)>,
    tx: watch::Sender<ParticipantExit>,
) where
    F: Future<Output = Result<Pod, GenericError>>,
{
    let mut consecutive_errors = 0;
    loop {
        let pod = read().await;
        match &pod {
            Ok(_) => consecutive_errors = 0,
            Err(error) => {
                consecutive_errors += 1;
                let not_found = matches!(error.downcast_ref::<kube::Error>(),
                    Some(kube::Error::Api(status)) if status.is_not_found());
                if !not_found && consecutive_errors < 3 {
                    sleep(POLL_INTERVAL).await;
                    continue;
                }
            }
        }
        let mut exits = Vec::with_capacity(containers.len());
        for (name, status) in &containers {
            if *status.borrow() == ExitObservation::Running {
                let exit = match &pod {
                    Ok(pod) => container_status(pod, name),
                    Err(error) => ExitObservation::Undetermined(format!("container '{name}': {error:#}")),
                };
                if exit != ExitObservation::Running {
                    status.send_replace(exit);
                }
            }
            exits.push(status.borrow().clone());
        }
        let exit = participant_exit(&exits);
        if exit != ParticipantExit::Running {
            tx.send_replace(exit);
            return;
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn watch_captures(captures: Vec<(String, watch::Receiver<Capture>)>, tx: watch::Sender<Capture>) {
    let mut pending: stream::FuturesUnordered<_> = captures
        .into_iter()
        .map(|(name, mut capture)| async move {
            loop {
                let state = capture.borrow_and_update().clone();
                match state {
                    Capture::Complete => return Ok(()),
                    Capture::Failed { reason } => return Err(format!("container '{name}': {reason}")),
                    Capture::Following => {}
                }
                if capture.changed().await.is_err() {
                    return Err(format!("container '{name}' output capture ended before completion"));
                }
            }
        })
        .collect();
    while let Some(result) = pending.next().await {
        if let Err(reason) = result {
            tx.send_replace(Capture::Failed { reason });
            return;
        }
    }
    tx.send_replace(Capture::Complete);
}

async fn capture_logs<R, F>(
    mut open: impl FnMut(bool) -> F, path: PathBuf, mut lines: LineBuffer, exit: ProcessObservation,
    tx: watch::Sender<Capture>,
) where
    R: AsyncRead + Unpin,
    F: Future<Output = Result<R, GenericError>>,
{
    let result: Result<(), GenericError> = async {
        tokio::fs::create_dir_all(path.parent().expect("container logs have a parent directory")).await?;
        let mut file = File::create(&path).await?;
        let mut bytes = [0; 8192];
        let mut followed = 0usize;
        let ended = 'follow: loop {
            let mut reader = match open(true).await {
                Ok(reader) => reader,
                Err(error) => break Err(error),
            };
            loop {
                let count = tokio::select! {
                    biased;
                    status = exit.settled() => break 'follow Ok(status),
                    count = reader.read(&mut bytes) => match count {
                        Ok(count) => count,
                        Err(error) => break 'follow Err(error.into()),
                    },
                };
                if count == 0 {
                    if followed == 0 {
                        // A newly running container can return an empty stream before its log file exists.
                        tokio::select! {
                            biased;
                            status = exit.settled() => break 'follow Ok(status),
                            _ = sleep(POLL_INTERVAL) => continue 'follow,
                        }
                    }
                    break 'follow Err(generic_error!("log stream ended while its container was running"));
                }
                file.write_all(&bytes[..count]).await?;
                file.flush().await?;
                lines.push(&bytes[..count], SystemTime::now());
                followed += count;
            }
        };
        let status = match ended {
            Ok(status) => status,
            // Log streaming can fail before the next status poll observes termination.
            Err(error) => tokio::time::timeout(Duration::from_secs(5), exit.settled())
                .await
                .map_err(|_| error)?,
        };
        // Re-read after termination to recover trailing bytes without duplicating the followed prefix.
        let mut reader = open(false).await?;
        let mut skip = followed;
        loop {
            let count = reader.read(&mut bytes).await?;
            if count == 0 {
                break;
            }
            let skipped = skip.min(count);
            skip -= skipped;
            file.write_all(&bytes[skipped..count]).await?;
            file.flush().await?;
            lines.push(&bytes[skipped..count], SystemTime::now());
        }
        if skip != 0 {
            return Err(generic_error!(
                "final log is shorter than the {followed} bytes already followed"
            ));
        }
        if let ExitObservation::Undetermined(reason) = status {
            return Err(generic_error!(
                "container exit is undetermined ({reason}); output may be incomplete"
            ));
        }
        Ok(())
    }
    .await;
    lines.finish();
    tx.send_replace(match result {
        Ok(()) => Capture::Complete,
        Err(error) => Capture::Failed {
            reason: format!("cannot capture log '{}': {error:#}", path.display()),
        },
    });
}

fn file_chunks(node: Node, path: String) -> impl futures::Stream<Item = Result<OutputChunk, GenericError>> {
    let chunks = stream::try_unfold((node, path, 0usize), |(node, path, mut offset)| async move {
        loop {
            let output = node
                .shell(
                    "if [ ! -e \"$1\" ] && [ ! -L \"$1\" ]; then exit 44; fi; [ -f \"$1\" ]; \
                 [ \"$(stat -c %s -- \"$1\")\" -ge \"$2\" ] || exit 45; \
                 dd if=\"$1\" bs=65536 skip=\"$2\" count=1 iflag=skip_bytes status=none",
                    &[&path, &offset.to_string()],
                    None,
                )
                .await?;
            match output.exit_code {
                0 if !output.stdout.is_empty() => {
                    offset += output.stdout.len();
                    return Ok(Some((OutputChunk::Stdout(output.stdout.into()), (node, path, offset))));
                }
                0 | 44 if offset == 0 || output.exit_code == 0 => {}
                code => {
                    return Err(generic_error!(
                        "cannot follow node file '{path}' at byte {offset}: exit {code}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ))
                }
            }
            sleep(POLL_INTERVAL).await;
        }
    });
    stream::once(std::future::ready(Ok(OutputChunk::Stdout(FILE_OUTPUT_START.into())))).chain(chunks)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io, task::Poll};

    use futures::TryStreamExt as _;
    use serde_json::{json, Value};
    use tokio::time::{timeout, Instant};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{observe::output::OutputLines, participant::ProcessSpec, tasks::TASK_STOP_TIME};

    fn pod(status: Value) -> Pod {
        serde_json::from_value(json!({"status": status})).unwrap()
    }

    fn state(name: &str, state: Value) -> Value {
        json!({"name": name, "image": "fixture", "imageID": "fixture", "ready": false,
            "restartCount": 0, "state": state})
    }

    fn exited(code: i32) -> ExitObservation {
        ExitObservation::Exited(ExitStatus {
            code: Some(code),
            signal: None,
            reason: None,
            recorded_at: None,
        })
    }

    #[test]
    fn absent_pending_and_running_container_statuses_do_not_invent_exits() {
        for pod in [
            Pod::default(),
            pod(json!({"phase": "Pending"})),
            pod(json!({"phase": "Running", "containerStatuses": []})),
            pod(json!({"containerStatuses": [state("main", json!({"waiting": {"reason": "ContainerCreating"}}))]})),
            pod(json!({"containerStatuses": [state("main", json!({"running": {}}))]})),
        ] {
            assert_eq!(container_status(&pod, "main"), ExitObservation::Running);
        }
        let mut restarted = state("main", json!({"running": {}}));
        restarted["lastState"] = json!({"terminated": {"exitCode": 7}});
        assert_eq!(
            container_status(&pod(json!({"containerStatuses": [restarted]})), "main"),
            ExitObservation::Running
        );
    }

    #[tokio::test]
    async fn container_status_preserves_signal_reason_time_and_container_source() {
        let pod = pod(json!({"containerStatuses": [state("main", json!({"terminated": {
            "exitCode": 137, "signal": 9, "reason": "OOMKilled", "message": "memory limit",
            "finishedAt": "1970-01-01T00:00:01Z"
        }}))]}));
        let status = container_status(&pod, "main");
        assert_eq!(
            status,
            ExitObservation::Exited(ExitStatus {
                code: None,
                signal: Some(9),
                reason: Some("OOMKilled: memory limit".into()),
                recorded_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            })
        );
        let (_, rx) = watch::channel(status.clone());
        let process = ProcessObservation::ContainerStatus(rx);
        assert_eq!(process.current(), status);
        assert_eq!(process.refresh().await, status);
        assert_eq!(process.settled().await, status);
        assert_eq!(
            serde_json::to_value(process.snapshot()).unwrap(),
            json!({
                "state": "exited", "source": "container", "signal": 9, "reason": "OOMKilled: memory limit"
            })
        );
        for signal in [None, Some(0)] {
            let mut pod = pod_with_exit(7, signal);
            pod.status.as_mut().unwrap().phase = Some("Unknown".into());
            assert_eq!(container_status(&pod, "main"), exited(7));
        }
    }

    fn pod_with_exit(code: i32, signal: Option<i32>) -> Pod {
        pod(json!({"containerStatuses": [state("main", json!({"terminated": {"exitCode": code, "signal": signal}}))]}))
    }

    #[test]
    fn terminal_missing_and_unknown_statuses_are_observation_errors() {
        for (status, expected) in [
            (
                json!({"phase": "Failed", "reason": "Evicted", "message": "node pressure"}),
                "Evicted: node pressure",
            ),
            (
                json!({"phase": "Succeeded", "containerStatuses": []}),
                "no termination status",
            ),
            (json!({"phase": "Unknown", "reason": "NodeLost"}), "NodeLost"),
            (
                json!({"phase": "Failed", "containerStatuses": [state("main", json!({"waiting": {}}))]}),
                "no termination status",
            ),
            (
                json!({"containerStatuses": [state("main", json!({"terminated": {
                    "exitCode": 137, "reason": "ContainerStatusUnknown"
                }}))]}),
                "ContainerStatusUnknown",
            ),
            (
                json!({"containerStatuses": [state("main", json!({"terminated": {
                    "exitCode": 1, "signal": -1
                }}))]}),
                "negative termination signal",
            ),
        ] {
            let ExitObservation::Undetermined(reason) = container_status(&pod(status), "main") else {
                panic!("expected an undetermined exit");
            };
            assert!(reason.contains(expected), "{reason}");
            assert!(reason.contains("main"), "{reason}");
        }
    }

    #[test]
    fn participant_waits_for_every_container_and_only_reports_observed_codes() {
        assert_eq!(
            participant_exit(&[exited(7)]),
            ParticipantExit::Exited { code: 7, error: None }
        );
        assert_eq!(
            participant_exit(&[exited(7), ExitObservation::Running]),
            ParticipantExit::Running
        );
        assert_eq!(participant_exit(&[exited(7), exited(0)]), ParticipantExit::Ended);
        let signal = container_status(&pod_with_exit(137, Some(9)), "main");
        assert_eq!(participant_exit(&[signal]), ParticipantExit::Ended);
        assert_eq!(
            participant_exit(&[exited(7), ExitObservation::Undetermined("missing".into())]),
            ParticipantExit::Undetermined {
                reason: "missing".into()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn polling_retains_first_termination_until_siblings_end() {
        let mut pods = VecDeque::from([
            pod(json!({"containerStatuses": [
                state("main", json!({"terminated": {"exitCode": 7}})), state("helper", json!({"running": {}}))
            ]})),
            pod(json!({"containerStatuses": [
                state("main", json!({"running": {}})), state("helper", json!({"terminated": {"exitCode": 0}}))
            ]})),
        ]);
        let (main_tx, main) = watch::channel(ExitObservation::Running);
        let (helper_tx, helper) = watch::channel(ExitObservation::Running);
        let (tx, exit) = watch::channel(ParticipantExit::Running);
        watch_containers(
            || std::future::ready(Ok(pods.pop_front().unwrap())),
            vec![("main".into(), main_tx), ("helper".into(), helper_tx)],
            tx,
        )
        .await;
        assert!(pods.is_empty());
        assert_eq!(*main.borrow(), exited(7));
        assert_eq!(*helper.borrow(), exited(0));
        assert_eq!(*exit.borrow(), ParticipantExit::Ended);
    }

    #[tokio::test(start_paused = true)]
    async fn status_request_errors_retry_transient_failures_and_preserve_known_exits() {
        // A failed poll must not settle running containers; NotFound must survive the read's error context.
        let not_found = Err::<Pod, _>(kube::Error::Api(
            serde_json::from_value(json!({
                "status": "Failure", "reason": "NotFound", "message": "pod was deleted", "code": 404
            }))
            .unwrap(),
        ))
        .with_error_context(|| "cannot observe pod 'ns/p'")
        .unwrap_err();
        let terminated = || {
            Ok(pod(json!({"containerStatuses": [
                state("helper", json!({"terminated": {"exitCode": 0}}))
            ]})))
        };
        let failure = || Err(generic_error!("status request failed"));
        for (name, responses, expected_error) in [
            (
                "one transient failure",
                vec![failure(), Ok(Pod::default()), terminated()],
                None,
            ),
            (
                "a success resets consecutive failures",
                vec![
                    failure(),
                    failure(),
                    Ok(Pod::default()),
                    failure(),
                    failure(),
                    terminated(),
                ],
                None,
            ),
            (
                "three consecutive failures",
                vec![failure(), failure(), failure()],
                Some("status request failed"),
            ),
            ("NotFound", vec![Err(not_found)], Some("cannot observe pod 'ns/p':")),
        ] {
            let polls = responses.len();
            let mut responses = VecDeque::from(responses);
            let (main_tx, main) = watch::channel(exited(7));
            let (helper_tx, helper) = watch::channel(ExitObservation::Running);
            let (tx, exit) = watch::channel(ParticipantExit::Running);
            let started = Instant::now();
            watch_containers(
                || {
                    assert_eq!(*exit.borrow(), ParticipantExit::Running, "{name}");
                    assert_eq!(*helper.borrow(), ExitObservation::Running, "{name}");
                    std::future::ready(responses.pop_front().expect("no further status requests"))
                },
                vec![("main".into(), main_tx), ("helper".into(), helper_tx)],
                tx,
            )
            .await;
            assert!(responses.is_empty(), "{name}");
            assert_eq!(started.elapsed(), POLL_INTERVAL * (polls - 1) as u32, "{name}");
            assert_eq!(*main.borrow(), exited(7), "{name}");
            match expected_error {
                Some(expected) => {
                    let ExitObservation::Undetermined(reason) = &*helper.borrow() else {
                        panic!("{name}: expected an undetermined exit");
                    };
                    assert!(reason.contains(expected), "{name}: {reason}");
                    assert_eq!(
                        *exit.borrow(),
                        ParticipantExit::Undetermined { reason: reason.clone() },
                        "{name}"
                    );
                    assert!(reason.contains("helper"), "{name}: {reason}");
                }
                None => {
                    assert_eq!(*helper.borrow(), exited(0), "{name}");
                    assert_eq!(*exit.borrow(), ParticipantExit::Ended, "{name}");
                }
            }
        }
        let (_, rx) = watch::channel(ExitObservation::Running);
        assert!(matches!(
            ProcessObservation::ContainerStatus(rx).settled().await,
            ExitObservation::Undetermined(_)
        ));
    }

    struct Logs {
        dir: tempfile::TempDir,
        lines: OutputLines,
        exit_tx: watch::Sender<ExitObservation>,
        exit: ProcessObservation,
        capture_tx: watch::Sender<Capture>,
        capture: watch::Receiver<Capture>,
    }

    impl Logs {
        fn new(limit: usize) -> Self {
            let (exit_tx, status) = watch::channel(ExitObservation::Running);
            let (capture_tx, capture) = watch::channel(Capture::Following);
            Self {
                dir: tempfile::tempdir().unwrap(),
                lines: OutputLines::new(limit),
                exit_tx,
                exit: ProcessObservation::ContainerStatus(status),
                capture_tx,
                capture,
            }
        }

        async fn follow(&self, followed: &[u8], final_bytes: &[u8]) {
            capture_logs(
                |follow| {
                    let mut bytes = Some(if follow {
                        followed.to_vec()
                    } else {
                        final_bytes.to_vec()
                    });
                    let exit = self.exit_tx.clone();
                    let reader = stream::poll_fn(move |_| {
                        let bytes = bytes.take();
                        if follow && bytes.is_none() {
                            exit.send_replace(exited(0));
                        }
                        Poll::Ready(bytes.map(Ok::<_, io::Error>))
                    })
                    .into_async_read();
                    std::future::ready(Ok(reader))
                },
                self.dir.path().join("containers/main.log"),
                LineBuffer::new(self.lines.clone(), "p".into(), Some("main"), OutputStream::Merged),
                self.exit.clone(),
                self.capture_tx.clone(),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn merged_logs_keep_tags_and_final_bytes_without_overwriting_process_files() {
        let logs = Logs::new(1024);
        let other = logs.dir.path().join("main.log");
        tokio::fs::write(&other, b"process output").await.unwrap();
        logs.follow(b"one\ntw", b"one\ntwo\nlast").await;
        assert_eq!(*logs.capture.borrow(), Capture::Complete);
        let path = logs.dir.path().join("containers/main.log");
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"one\ntwo\nlast");
        assert_eq!(tokio::fs::read(other).await.unwrap(), b"process output");
        let lines = logs.lines.since(0).unwrap().0;
        assert_eq!(
            lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>(),
            ["one", "two", "last"]
        );
        assert!(lines.iter().all(|line| &*line.participant == "p"
            && line.process == Some("main")
            && line.stream == OutputStream::Merged));
        let (_, exit) = watch::channel(ParticipantExit::Ended);
        let mut observation = ParticipantObservation::merged(exit, logs.capture.clone(), path.clone());
        observation.processes.insert("main", logs.exit.clone());
        observation.outputs.insert(
            "main",
            ProcessOutput {
                capture: logs.capture.clone(),
                paths: vec![path.clone()],
            },
        );
        let report = observation.snapshot(&[ProcessSpec {
            name: "main",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }]);
        assert_eq!(report[0].output.merged.as_ref(), Some(&path));
        assert_eq!(report[0].output.stdout, None);
        assert_eq!(report[0].output.stderr, None);
    }

    #[tokio::test]
    async fn final_logs_detect_truncation_and_line_overflow_still_writes_every_byte() {
        let logs = Logs::new(1024);
        logs.follow(b"followed\n", b"short").await;
        assert!(matches!(&*logs.capture.borrow(), Capture::Failed { reason } if reason.contains("shorter than")));
        let logs = Logs::new(4);
        logs.follow(b"one\n", b"one\ntwo\nlast").await;
        assert_eq!(*logs.capture.borrow(), Capture::Complete);
        assert_eq!(
            tokio::fs::read(logs.dir.path().join("containers/main.log"))
                .await
                .unwrap(),
            b"one\ntwo\nlast"
        );
        let (prefix, cursor) = logs.lines.since(0).unwrap();
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].text, "one");
        assert!(logs.lines.since(cursor).unwrap_err().contains("4-byte"));
    }

    #[tokio::test(start_paused = true)]
    async fn log_open_read_and_directory_errors_fail_capture() {
        for failure in ["open", "read", "directory"] {
            let logs = Logs::new(1024);
            if failure == "directory" {
                tokio::fs::write(logs.dir.path().join("containers"), b"not a directory")
                    .await
                    .unwrap();
            }
            capture_logs(
                |_| {
                    let reader = stream::once(std::future::ready(Err::<Vec<u8>, _>(io::Error::other("read failure"))))
                        .into_async_read();
                    std::future::ready(if failure == "open" {
                        Err(generic_error!("open failure"))
                    } else {
                        Ok(reader)
                    })
                },
                logs.dir.path().join("containers/main.log"),
                LineBuffer::new(logs.lines.clone(), "p".into(), None, OutputStream::Merged),
                logs.exit.clone(),
                logs.capture_tx.clone(),
            )
            .await;
            assert!(
                matches!(&*logs.capture.borrow(), Capture::Failed { reason } if reason.contains("main.log")),
                "{failure}"
            );
        }
    }

    #[tokio::test]
    async fn a_final_read_recovers_stream_failures_only_after_confirmed_termination() {
        for failure in ["open", "read", "final-read", "unknown-exit"] {
            let logs = Logs::new(1024);
            capture_logs(
                |follow| {
                    let mut chunks = VecDeque::from([Ok(b"first\n".to_vec())]);
                    if follow || failure == "final-read" {
                        chunks.push_back(Err(io::Error::other("stream failure")));
                    } else {
                        chunks.push_back(Ok(b"last\n".to_vec()));
                    }
                    let exit = logs.exit_tx.clone();
                    let reader = stream::poll_fn(move |_| {
                        let chunk = chunks.pop_front();
                        if follow && matches!(&chunk, Some(Err(_))) {
                            exit.send_replace(if failure == "unknown-exit" {
                                ExitObservation::Undetermined("node lost".into())
                            } else {
                                exited(0)
                            });
                        }
                        Poll::Ready(chunk)
                    })
                    .into_async_read();
                    std::future::ready(if follow && failure == "open" {
                        logs.exit_tx.send_replace(exited(0));
                        Err(generic_error!("open failure"))
                    } else {
                        Ok(reader)
                    })
                },
                logs.dir.path().join("containers/main.log"),
                LineBuffer::new(logs.lines.clone(), "p".into(), None, OutputStream::Merged),
                logs.exit.clone(),
                logs.capture_tx.clone(),
            )
            .await;
            match failure {
                "open" | "read" => assert_eq!(*logs.capture.borrow(), Capture::Complete),
                "final-read" => assert!(
                    matches!(&*logs.capture.borrow(), Capture::Failed { reason } if reason.contains("stream failure"))
                ),
                _ => assert!(
                    matches!(&*logs.capture.borrow(), Capture::Failed { reason } if reason.contains("node lost"))
                ),
            }
            let expected: &[u8] = if failure == "final-read" {
                b"first\n"
            } else {
                b"first\nlast\n"
            };
            assert_eq!(
                tokio::fs::read(logs.dir.path().join("containers/main.log"))
                    .await
                    .unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn empty_initial_streams_retry_until_output_or_container_termination() {
        // A container with no output can return an empty stream; capture must wait until it terminates.
        for output in [b"".as_slice(), b"ready\n".as_slice()] {
            let logs = Logs::new(1024);
            let mut attempts = 0;
            capture_logs(
                |follow| {
                    let bytes = if follow {
                        attempts += 1;
                        assert_eq!(*logs.capture.borrow(), Capture::Following);
                        if attempts < 3 {
                            Vec::new()
                        } else {
                            output.to_vec()
                        }
                    } else {
                        output.to_vec()
                    };
                    let mut chunk = Some(bytes);
                    let exit = logs.exit_tx.clone();
                    let terminate = follow && attempts == 3;
                    if terminate && output.is_empty() {
                        exit.send_replace(exited(0));
                    }
                    let reader = stream::poll_fn(move |_| {
                        let bytes = chunk.take();
                        if terminate && bytes.is_none() {
                            exit.send_replace(exited(0));
                        }
                        Poll::Ready(bytes.map(Ok::<_, io::Error>))
                    })
                    .into_async_read();
                    std::future::ready(Ok(reader))
                },
                logs.dir.path().join("containers/main.log"),
                LineBuffer::new(logs.lines.clone(), "p".into(), None, OutputStream::Merged),
                logs.exit.clone(),
                logs.capture_tx.clone(),
            )
            .await;
            assert_eq!(attempts, 3);
            assert_eq!(*logs.capture.borrow(), Capture::Complete);
            assert_eq!(
                tokio::fs::read(logs.dir.path().join("containers/main.log"))
                    .await
                    .unwrap(),
                output
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_log_eof_after_output_is_not_complete_without_container_termination() {
        let logs = Logs::new(1024);
        capture_logs(
            |_| std::future::ready(Ok(futures::io::Cursor::new(b"partial\n".to_vec()))),
            logs.dir.path().join("containers/main.log"),
            LineBuffer::new(logs.lines.clone(), "p".into(), None, OutputStream::Merged),
            logs.exit.clone(),
            logs.capture_tx.clone(),
        )
        .await;
        assert!(
            matches!(&*logs.capture.borrow(), Capture::Failed { reason } if reason.contains("while its container was running"))
        );
    }

    #[tokio::test]
    async fn aggregate_capture_includes_unowned_containers_and_detects_closed_watchers() {
        for final_capture in [
            Some(Capture::Complete),
            Some(Capture::Failed {
                reason: "lost logs".into(),
            }),
            None,
        ] {
            let (_, primary) = watch::channel(Capture::Complete);
            let (helper_tx, helper) = watch::channel(Capture::Following);
            let (tx, mut capture) = watch::channel(Capture::Following);
            let tasks = CaseTasks::new(&CancellationToken::new());
            tasks.spawn(watch_captures(
                vec![("main".into(), primary), ("helper".into(), helper)],
                tx,
            ));
            tokio::task::yield_now().await;
            assert_eq!(*capture.borrow(), Capture::Following);
            if let Some(state) = &final_capture {
                helper_tx.send_replace(state.clone());
            }
            drop(helper_tx);
            timeout(
                Duration::from_secs(5),
                capture.wait_for(|state| *state != Capture::Following),
            )
            .await
            .unwrap()
            .unwrap();
            match final_capture {
                Some(Capture::Complete) => assert_eq!(*capture.borrow(), Capture::Complete),
                _ => assert!(matches!(&*capture.borrow(), Capture::Failed { reason } if reason.contains("helper"))),
            }
            assert_eq!(tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
        }
    }
}
