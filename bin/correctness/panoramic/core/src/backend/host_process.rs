//! Runs host participants in per-case directories, with one process group per declared process.
//!
//! The ledger owns child reapers; case tasks own output followers and the participant-end observation.

use std::{
    collections::BTreeSet,
    io,
    os::unix::{
        fs::{FileTypeExt as _, PermissionsExt as _},
        process::ExitStatusExt as _,
    },
    path::{Path, PathBuf},
    process::{ExitStatus as OsExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use airlock::exec::OutputChunk;
use async_trait::async_trait;
use futures::{future::join_all, stream, Stream};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    process::{ChildStderr, ChildStdout, Command},
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout, timeout_at, Instant},
};

use super::{courier, LateFact, ParticipantRequest, RuntimeParticipant, ScopeRuntime};
use crate::{
    endpoint::ResolvedAddress,
    handle::{ExecCommand, ExecOutput, FileKind, FileStatus, HandleError, Primitives},
    layout::Os,
    ledger::{ParticipantRemoval, ResourceLedger},
    observe::{
        follow_output, Capture, ExitObservation, ExitStatus, ParticipantExit, ParticipantObservation,
        ProcessObservation, ProcessOutput,
    },
    participant::ProcessSpec,
    plan::ScopeLayout,
    probe::{AttemptLimits, ProbeAnswer},
    recipe::{HostRecipe, Recipe},
    tasks::CaseTasks,
};

/// The case state directory and the participants sharing it.
pub(crate) struct HostScope {
    directory: PathBuf,
    participants: BTreeSet<String>,
}

/// Creates and registers the planned case directory, then creates each participant's shared directory.
pub(crate) fn prepare(
    scope: &ScopeLayout, participants: &[&str], ledger: &ResourceLedger,
) -> Result<HostScope, String> {
    let directory = PathBuf::from(scope.state_dir(Os::host()));
    create_state_directory(&directory, |path| std::fs::create_dir(path))?;
    // Register synchronously after creation so cancellation cannot leave the directory unowned.
    ledger.host_scope(directory.clone());
    for name in participants {
        let shared = directory.join(name).join("shared");
        std::fs::create_dir_all(&shared)
            .map_err(|error| format!("cannot create shared directory '{}': {error}", shared.display()))?;
    }
    Ok(HostScope {
        directory,
        participants: participants.iter().map(|name| (*name).into()).collect(),
    })
}

fn create_state_directory(directory: &Path, mut create: impl FnMut(&Path) -> io::Result<()>) -> Result<(), String> {
    let run = directory.parent().expect("state directories have a run directory");
    loop {
        std::fs::create_dir_all(run)
            .map_err(|error| format!("cannot create run directory '{}': {error}", run.display()))?;
        match create(directory) {
            Ok(()) => return Ok(()),
            // Reclaiming the last case can remove the run directory between these calls.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot create state directory '{}': {error}",
                    directory.display()
                ))
            }
        }
    }
}

impl ScopeRuntime for HostScope {
    fn participant(&self, request: ParticipantRequest<'_>) -> Result<Arc<dyn RuntimeParticipant>, String> {
        let ParticipantRequest {
            name,
            recipe,
            specs,
            inputs,
            ..
        } = request;
        if !self.participants.contains(name) {
            return Err(format!("participant '{name}' is outside the prepared host scope"));
        }
        let Recipe::HostProcessGroup(recipe) = recipe else {
            return Err(format!("participant '{name}' needs a host-process-group recipe"));
        };
        let programs = recipe
            .processes
            .iter()
            .map(|process| {
                inputs
                    .program(process.program)
                    .map_err(|error| format!("participant '{name}' process '{}': {error}", process.name))
            })
            .collect::<Result<_, _>>()?;
        Ok(Arc::new(HostParticipant {
            name: name.into(),
            directory: self.directory.join(name),
            recipe,
            programs,
            specs,
            group: Arc::new(Mutex::new(Processes {
                children: Vec::new(),
                armed: true,
            })),
        }))
    }
}

/// Runs recipe programs with shared files, environment and ledger-owned process groups.
struct HostParticipant {
    name: String,
    directory: PathBuf,
    recipe: HostRecipe,
    programs: Vec<PathBuf>,
    specs: &'static [ProcessSpec],
    group: Arc<Mutex<Processes>>,
}

impl HostParticipant {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.current_dir(&self.directory).env_clear().envs(&self.recipe.env);
        for key in ["PATH", "HOME", "TMPDIR"] {
            if !self.recipe.env.contains_key(key) {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        }
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

#[async_trait]
impl RuntimeParticipant for HostParticipant {
    fn removal(&self) -> ParticipantRemoval {
        ParticipantRemoval::ProcessGroup(ProcessGroup {
            state: self.group.clone(),
        })
    }

    async fn create(&self) -> Result<Vec<(LateFact, String)>, GenericError> {
        let declared: BTreeSet<_> = self.specs.iter().map(|spec| spec.name).collect();
        let mut names = BTreeSet::new();
        for process in &self.recipe.processes {
            if !names.insert(process.name.as_str()) {
                return Err(generic_error!(
                    "participant '{}' recipe repeats process '{}'",
                    self.name,
                    process.name
                ));
            }
        }
        let missing = declared
            .difference(&names)
            .map(|name| format!("missing process '{name}'"));
        let extra = names
            .difference(&declared)
            .map(|name| format!("extra process '{name}'"));
        let differences = missing.chain(extra).collect::<Vec<_>>();
        if !differences.is_empty() {
            return Err(generic_error!(
                "participant '{}' recipe has {}",
                self.name,
                differences.join(", ")
            ));
        }
        for (process, program) in self.recipe.processes.iter().zip(&self.programs) {
            let metadata = std::fs::metadata(program).with_error_context(|| {
                format!(
                    "participant '{}' process '{}' cannot use program '{}'",
                    self.name,
                    process.name,
                    program.display()
                )
            })?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return Err(generic_error!(
                    "participant '{}' process '{}' program '{}' must be a regular executable file",
                    self.name,
                    process.name,
                    program.display()
                ));
            }
        }
        for file in &self.recipe.files {
            if file.path.contains(['\\', '\0'])
                || file
                    .path
                    .split('/')
                    .any(|component| matches!(component, "" | "." | ".."))
                || Path::new(&file.path).is_absolute()
            {
                return Err(generic_error!(
                    "participant '{}' recipe file '{}' must be a relative path with plain components",
                    self.name,
                    file.path
                ));
            }
        }
        for file in &self.recipe.files {
            let path = self.directory.join(&file.path);
            let placed = (|| -> io::Result<()> {
                std::fs::create_dir_all(path.parent().expect("relative recipe files have a parent"))?;
                std::fs::write(&path, &file.contents)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(file.mode))
            })();
            placed
                .with_error_context(|| format!("participant '{}' cannot place file '{}'", self.name, path.display()))?;
        }
        Ok(Vec::new())
    }

    async fn start(&self) -> Result<Vec<(LateFact, String)>, GenericError> {
        let mut group = self.group.lock().unwrap();
        if !group.armed || !group.children.is_empty() {
            return Err(generic_error!(
                "participant '{}' has already started or been reclaimed",
                self.name
            ));
        }
        for (process, program) in self.recipe.processes.iter().zip(&self.programs) {
            let mut child = self
                .command(program)
                .args(&process.args)
                .envs(&process.env)
                .spawn()
                .with_error_context(|| {
                    format!(
                        "participant '{}' failed to start process '{}' from '{}'",
                        self.name,
                        process.name,
                        program.display()
                    )
                })?;
            let pgid = child.id().expect("a newly spawned process has a PID") as i32;
            let pipes = Some((
                child.stdout.take().expect("stdout is piped"),
                child.stderr.take().expect("stderr is piped"),
            ));
            let name = self
                .specs
                .iter()
                .find(|spec| spec.name == process.name)
                .expect("create validated process names")
                .name;
            let (tx, exit) = watch::channel(ExitObservation::Running);
            let reaper = tokio::spawn(async move {
                tx.send_replace(observe_wait(name, child.wait().await));
            });
            group.children.push(ChildProcess {
                name,
                pgid,
                exit,
                reaper,
                pipes,
            });
        }
        Ok(Vec::new())
    }

    async fn observe(self: Arc<Self>, tasks: &CaseTasks, output_dir: &Path) -> ParticipantObservation {
        let (tx, rx) = watch::channel(ParticipantExit::Running);
        let mut observation = ParticipantObservation::host(rx);
        let mut processes = Vec::new();
        for child in &mut self.group.lock().unwrap().children {
            let process = ProcessObservation::Wait(child.exit.clone());
            processes.push((child.name, process.clone()));
            observation.processes.insert(child.name, process);
            let stdout = output_dir.join(format!("{}.stdout.log", child.name));
            let stderr = output_dir.join(format!("{}.stderr.log", child.name));
            let (capture_tx, capture) = watch::channel(Capture::Following);
            observation.outputs.insert(
                child.name,
                ProcessOutput {
                    capture,
                    paths: vec![stdout.clone(), stderr.clone()],
                },
            );
            let (out, err) = child.pipes.take().expect("output is followed once per started process");
            tasks.spawn(follow_output(
                stream::select(pipe_stream(out, true), pipe_stream(err, false)),
                stdout,
                stderr,
                capture_tx,
                observation.lines.clone(),
                self.name.clone().into(),
                Some(child.name),
            ));
        }
        tasks.spawn(async move {
            let exits = join_all(
                processes
                    .into_iter()
                    .map(|(name, process)| async move { (name, process.settled().await) }),
            )
            .await;
            let errors = exits
                .into_iter()
                .filter_map(|(name, exit)| match exit {
                    ExitObservation::Undetermined(reason) => Some(format!("process '{name}': {reason}")),
                    _ => None,
                })
                .collect::<Vec<_>>();
            tx.send_replace(if errors.is_empty() {
                ParticipantExit::Ended
            } else {
                ParticipantExit::Undetermined {
                    reason: errors.join("; "),
                }
            });
        });
        observation
    }
}

fn observe_wait(name: &str, result: io::Result<OsExitStatus>) -> ExitObservation {
    match result {
        Ok(status) if status.code().is_some() || status.signal().is_some() => ExitObservation::Exited(ExitStatus {
            code: status.code(),
            signal: status.signal(),
            reason: None,
            recorded_at: None,
        }),
        Ok(_) => ExitObservation::Undetermined(format!("waiting for process '{name}' returned no code or signal")),
        Err(error) => ExitObservation::Undetermined(format!("waiting for process '{name}' failed: {error}")),
    }
}

fn pipe_stream<R>(reader: R, stdout: bool) -> impl Stream<Item = Result<OutputChunk, GenericError>> + Send
where
    R: AsyncRead + Unpin + Send,
{
    stream::try_unfold(reader, move |mut reader| async move {
        let mut bytes = vec![0; 8192];
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(None);
        }
        bytes.truncate(count);
        let chunk = if stdout {
            OutputChunk::Stdout(bytes.into())
        } else {
            OutputChunk::Stderr(bytes.into())
        };
        Ok(Some((chunk, reader)))
    })
}

struct ChildProcess {
    name: &'static str,
    pgid: i32,
    exit: watch::Receiver<ExitObservation>,
    reaper: JoinHandle<()>,
    pipes: Option<(ChildStdout, ChildStderr)>,
}

struct Processes {
    children: Vec<ChildProcess>,
    armed: bool,
}

impl Processes {
    fn abort(&mut self) {
        if self.armed {
            for child in self.children.iter().rev() {
                let _ = signal_group(child.pgid, libc::SIGKILL);
                child.reaper.abort();
            }
            self.armed = false;
        }
    }
}

impl Drop for Processes {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Owns reclaim and the reapers independently of case-task cancellation.
pub(crate) struct ProcessGroup {
    state: Arc<Mutex<Processes>>,
}

impl ProcessGroup {
    pub(crate) async fn stop(&self, grace: Duration, deadline: Instant) -> Result<(), GenericError> {
        let children = {
            let group = self.state.lock().unwrap();
            if !group.armed {
                return Ok(());
            }
            group
                .children
                .iter()
                .map(|child| (child.pgid, child.exit.clone()))
                .collect::<Vec<_>>()
        };
        let mut errors = Vec::new();
        for (pgid, _) in children.iter().rev() {
            if let Err(error) = signal_group(*pgid, libc::SIGTERM) {
                errors.push((*pgid, format!("cannot stop process group {pgid}: {error}")));
            }
        }
        let groups_gone = timeout_at((Instant::now() + grace).min(deadline), async {
            loop {
                let reaped = children
                    .iter()
                    .all(|(_, exit)| matches!(*exit.borrow(), ExitObservation::Exited(_)));
                if reaped && children.iter().all(|(pgid, _)| !group_exists(*pgid)) {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if groups_gone {
            errors.clear();
        } else {
            // A leader can exit before descendants; retain its group ID through both signals. Reuse after the
            // whole group exits could signal an unrelated group before reclaim finishes.
            for (pgid, _) in children.iter().rev() {
                if let Err(error) = signal_group(*pgid, libc::SIGKILL) {
                    errors.push((*pgid, format!("cannot kill process group {pgid}: {error}")));
                }
            }
        }
        join_all(children.into_iter().map(|(_, mut exit)| async move {
            loop {
                if matches!(*exit.borrow(), ExitObservation::Exited(_)) {
                    return;
                }
                if exit.changed().await.is_err() {
                    return std::future::pending::<()>().await;
                }
            }
        }))
        .await;
        for (_, error) in &errors {
            tracing::warn!(%error, "Waiting for a process group after a signal failed");
        }
        finish_signals(&mut errors).await;
        self.state.lock().unwrap().armed = false;
        Ok(())
    }
}

async fn finish_signals(errors: &mut Vec<(i32, String)>) {
    // A signal failure stops mattering once its group is confirmed absent. Persistent failures remain
    // pending for the ledger's deadline, retaining the scope and the group's drop fallback.
    loop {
        errors.retain(|(pgid, _)| group_exists(*pgid));
        if errors.is_empty() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).abort();
    }
}

fn signal_group(pgid: i32, signal: i32) -> io::Result<()> {
    // Negating a positive child PID selects that child's process group.
    assert!(pgid > 0);
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn group_exists(pgid: i32) -> bool {
    unsafe { libc::kill(-pgid, 0) == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
}

/// Ends the whole command group on return or cancellation, including successful commands' background children.
struct ExecGroup(i32);
impl Drop for ExecGroup {
    fn drop(&mut self) {
        let _ = signal_group(self.0, libc::SIGKILL);
    }
}

#[async_trait]
impl Primitives for HostParticipant {
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError> {
        let failure = |message| HandleError::Harness {
            message: format!("exec in participant '{}': {message}", self.name),
        };
        let (program, args) = command
            .argv
            .split_first()
            .ok_or_else(|| failure("empty command".into()))?;
        let mut builder = self.command(program);
        builder.args(args).envs(command.env);
        if command.stdin.is_some() {
            builder.stdin(Stdio::piped());
        }
        let mut child = builder
            .spawn()
            .map_err(|error| failure(format!("cannot start '{program}': {error}")))?;
        let group = ExecGroup(child.id().expect("a newly spawned command has a PID") as i32);
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let stdin = child.stdin.take();
        let result = timeout(limit, async {
            let (status, (), out, err) = tokio::try_join!(
                child.wait(),
                async {
                    if let Some(mut stdin) = stdin {
                        let written = async {
                            stdin.write_all(command.stdin.as_deref().unwrap_or_default()).await?;
                            stdin.shutdown().await
                        }
                        .await;
                        if let Err(error) = written {
                            if error.kind() != io::ErrorKind::BrokenPipe {
                                return Err(error);
                            }
                        }
                    }
                    Ok::<_, io::Error>(())
                },
                async {
                    let mut out = Vec::new();
                    stdout.read_to_end(&mut out).await?;
                    Ok::<_, io::Error>(out)
                },
                async {
                    let mut err = Vec::new();
                    stderr.read_to_end(&mut err).await?;
                    Ok::<_, io::Error>(err)
                },
            )?;
            let code = status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .ok_or_else(|| io::Error::other("command ended without a code or signal"))?;
            Ok::<_, io::Error>(ExecOutput {
                stdout: out,
                stderr: err,
                exit_code: i64::from(code),
            })
        })
        .await;
        drop(group);
        match result {
            Ok(Ok(output)) => Ok(output),
            failed => {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                match failed {
                    Ok(Err(error)) => Err(failure(error.to_string())),
                    Err(_) => Err(failure(format!("killed its process group after {limit:?}"))),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        match tokio::fs::read(self.directory.join(path)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(HandleError::Harness {
                message: format!("cannot read '{path}' in participant '{}': {error}", self.name),
            }),
        }
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStatus>, HandleError> {
        let path = self.directory.join(path);
        let failure = |error| HandleError::Harness {
            message: format!(
                "cannot stat '{}' in participant '{}': {error}",
                path.display(),
                self.name
            ),
        };
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(failure(error)),
        };
        let ft = metadata.file_type();
        let kind = if ft.is_file() {
            FileKind::File
        } else if ft.is_dir() {
            FileKind::Directory
        } else if ft.is_symlink() {
            FileKind::Symlink {
                target: tokio::fs::read_link(&path)
                    .await
                    .map_err(failure)?
                    .to_string_lossy()
                    .into_owned(),
            }
        } else if ft.is_socket() {
            FileKind::Socket
        } else if ft.is_fifo() {
            FileKind::NamedPipe
        } else {
            FileKind::Other
        };
        Ok(Some(FileStatus {
            kind,
            mode: metadata.permissions().mode() & 0o7777,
            size: metadata.len(),
        }))
    }

    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        courier::probe_in_process(address, limits).await
    }

    async fn send(&self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError> {
        courier::send_in_process(address, payload, limits).await
    }
}

#[cfg(test)]
mod tests;
