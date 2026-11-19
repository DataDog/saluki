//! The run-time half of the host process backend: the state directory of the runner host, and the programs of each
//! participant.
//!
//! For each case, the backend prepares the state directory, which is the same path for every case and every runner
//! host. Preparing refuses a directory that exists, so it never removes the directory of a run that is still using it.
//! Per participant, it makes the participant's own directory under it, places the participant's files there, and fills
//! in the environment and arguments of each program. At start it runs each program in the participant's directory as a
//! process group, with a cleared environment and only a few variables of the runner's. Every process is entered in the
//! ledger as soon as it starts, and the ledger stops it, then removes the directory.
//!
//! A program is observed through what it is: its exit is the wait result of its process, and its output is its pipes.
//! The participant as a whole has exited once every program has.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airlock::container::{Output, OutputStream};
use airlock::host::{ExitWatch, HostCommand, HostExit, HostProcess};
use async_trait::async_trait;
use futures::StreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::time::{timeout, timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use super::backend::{Backend, Created, Exec, Identities, Preparing, Realized, Running, Scope};
use super::container::fill;
use super::handle::HandleError;
use super::ledger::ResourceLedger;
use super::observe::{ExitStatus, ProcessExit, Sink};
use super::participant::Enclosure;
use super::plan::{Launch, PlannedParticipant};
use super::reference::Values;
use super::spec::{Contents, OutputChannel, ProcessSpec, Protocol};
use super::wiring::PlannedAddress;

/// The variables of the runner's environment that a program gets, when the runner has them.
const PASS_THROUGH: [&str; 3] = ["PATH", "HOME", "TMPDIR"];

/// How long [`RealizedHost::exec`] keeps reading the output of a command that has exited, which a background child can
/// keep open.
const OUTPUT_AFTER_EXIT: Duration = Duration::from_millis(500);

/// The mode of each of a participant's own directories.
const OWN_DIR_MODE: u32 = 0o755;
/// The mode of a placed file that the plan gives none.
const DEFAULT_FILE_MODE: u32 = 0o644;

/// The code of a program that a signal ended, as an exit record reports it.
const SIGNALLED_CODE: i64 = 256;

/// Finds the file `path` runs: under `own_dir` when written `./x`, in the first directory of the runner's `PATH` that
/// holds an executable file of that name when it is a bare name, and, otherwise, as written, relative to the
/// directory panoramic runs in.
///
/// # Errors
///
/// Returns an error that names `path`, as written, if there is no such file or it is not executable.
pub(crate) fn locate(path: &str, own_dir: &Path) -> Result<PathBuf, String> {
    let found = if let Some(rest) = path.strip_prefix("./") {
        own_dir.join(rest)
    } else if !path.contains('/') {
        let search = std::env::var_os("PATH").unwrap_or_default();
        let found = std::env::split_paths(&search)
            .map(|directory| directory.join(path))
            .find(|candidate| executable(candidate).is_ok())
            .ok_or_else(|| format!("'{}' is not an executable file in any directory of PATH", path))?;
        // A relative entry of `PATH` is relative to the runner, and the program runs in another directory.
        return std::path::absolute(&found).map_err(|e| format!("'{}' cannot be made an absolute path: {}", path, e));
    } else {
        std::path::absolute(path).map_err(|e| format!("'{}' cannot be made an absolute path: {}", path, e))?
    };
    match executable(&found) {
        Ok(()) => Ok(found),
        Err(why) => Err(format!("'{}' ({}) {}", path, found.display(), why)),
    }
}

/// Whether `path` is a file, with an execute bit where the OS has one.
fn executable(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("cannot be read: {}", e))?;
    if !metadata.is_file() {
        return Err("is not a file".to_string());
    }
    #[cfg(unix)]
    if std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 == 0 {
        return Err("is not executable".to_string());
    }
    Ok(())
}

/// Creates the programs of cases as host processes.
#[derive(Debug)]
pub(crate) struct HostBackend {
    root: PathBuf,
}

impl HostBackend {
    /// A backend whose state directory is `root`. A run uses the planned root, because `{{ self.dir }}` is a path
    /// under it. The test passes a temporary directory instead, which only works because its case uses no `{{ self.dir
    /// }}`.
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Backend for HostBackend {
    async fn prepare(&self, cx: &Preparing<'_>) -> Result<Box<dyn Scope>, GenericError> {
        // Creating the directory is the only step that can tell whether another run owns it.
        match tokio::fs::create_dir(&self.root).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(generic_error!(
                    "The state directory '{}' exists. Remove it if no other panoramic run is using it.",
                    self.root.display()
                ));
            }
            Err(e) => {
                return Err(GenericError::from(e).context(format!(
                    "Failed to create the state directory '{}'.",
                    self.root.display()
                )));
            }
        }
        // Only a directory this run created is this run's to remove.
        cx.ledger.directory(self.root.clone());
        Ok(Box::new(HostScope {
            root: self.root.clone(),
            ledger: cx.ledger.clone(),
        }))
    }
}

/// The participants of one run.
struct HostScope {
    root: PathBuf,
    ledger: ResourceLedger,
}

/// A program, filled in and found, ready to start.
struct Program {
    name: String,
    path: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

impl HostScope {
    /// Makes the directory of participant `name`, places its files, and fills in its programs.
    async fn place(
        &self, name: &str, planned: &PlannedParticipant, values: &Values,
    ) -> Result<CreatedHost, GenericError> {
        if planned.realization.enclosure != Enclosure::HostProcessGroup {
            return Err(generic_error!(
                "Participant '{}' is a {}, and this backend creates {}s.",
                name,
                planned.realization.enclosure,
                Enclosure::HostProcessGroup
            ));
        }
        let Launch::Programs(planned_programs) = &planned.launch else {
            return Err(generic_error!(
                "Participant '{}' runs an image, and host processes run programs.",
                name
            ));
        };
        let dir = self.root.join(name).join("shared");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_error_context(|| format!("Failed to create '{}'.", dir.display()))?;
        set_mode(&dir, OWN_DIR_MODE).await?;

        for file in &planned.files {
            // A host destination is `./…`, and planning has expanded each directory into its files.
            let relative = file.destination.strip_prefix("./").unwrap_or(&file.destination);
            let path = dir.join(relative);
            let contents = match &file.contents {
                Contents::Text(text) => text.clone().into_bytes(),
                Contents::Bytes(bytes) => bytes.clone(),
                Contents::Template(template) => fill(values, name, template)?.into_bytes(),
            };
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_error_context(|| format!("Failed to create '{}'.", parent.display()))?;
            }
            tokio::fs::write(&path, contents)
                .await
                .with_error_context(|| format!("Failed to write '{}'.", path.display()))?;
            set_mode(&path, file.mode.unwrap_or(DEFAULT_FILE_MODE)).await?;
        }

        let mut env: BTreeMap<String, String> = PASS_THROUGH
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_string(), value)))
            .collect();
        for (key, value) in &planned.env {
            env.insert(key.clone(), fill(values, name, value)?);
        }
        // The files are placed, so a program at `./x` is found.
        let mut programs = Vec::new();
        for program in planned_programs {
            let path = locate(program.path.used(), &dir)
                .map_err(|e| generic_error!("Participant '{}' program '{}': {}", name, program.name, e))?;
            let args = program
                .args
                .iter()
                .map(|arg| fill(values, name, arg))
                .collect::<Result<_, _>>()?;
            let mut env = env.clone();
            for (key, value) in &program.env {
                env.insert(key.clone(), fill(values, name, value)?);
            }
            programs.push(Program {
                name: program.name.clone(),
                path,
                args,
                env,
            });
        }
        let ports = planned
            .endpoints
            .iter()
            .filter(|(_, endpoint)| matches!(endpoint.protocol, Protocol::Tcp | Protocol::Http | Protocol::Grpc))
            .filter_map(|(endpoint_name, endpoint)| match endpoint.address {
                PlannedAddress::Port(port) => Some((endpoint_name.clone(), port)),
                PlannedAddress::Path(_) | PlannedAddress::Pipe(_) => None,
            })
            .collect();
        Ok(CreatedHost {
            ledger: self.ledger.clone(),
            dir,
            env,
            programs,
            ports,
        })
    }
}

/// Sets the mode of `path`, on the OSes that have one.
async fn set_mode(path: &Path, mode: u32) -> Result<(), GenericError> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .await
        .with_error_context(|| format!("Failed to set the mode of '{}'.", path.display()))?;
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[async_trait]
impl Scope for HostScope {
    async fn create(
        &self, name: &str, planned: &PlannedParticipant, values: &Values, cancel: &CancellationToken, deadline: Instant,
    ) -> Result<Box<dyn Created>, GenericError> {
        let placed = timeout_at(deadline, cancel.run_until_cancelled(self.place(name, planned, values)))
            .await
            .map_err(|_| generic_error!("The deadline passed while creating participant '{}'.", name))?
            .ok_or_else(|| generic_error!("Cancelled while creating participant '{}'.", name))??;
        Ok(Box::new(placed))
    }
}

/// A participant whose files are placed and whose programs are found, and which has not started.
struct CreatedHost {
    ledger: ResourceLedger,
    /// The participant's own directory, which its programs run in.
    dir: PathBuf,
    /// The environment of every program: the runner's few variables, and the participant's.
    env: BTreeMap<String, String>,
    /// In start order.
    programs: Vec<Program>,
    /// The port of each endpoint a request from the runner reaches.
    ports: BTreeMap<String, u16>,
}

#[async_trait]
impl Created for CreatedHost {
    fn identities(&self) -> Identities {
        Identities::default()
    }

    async fn start(
        self: Box<Self>, _values: &Values, cancel: &CancellationToken, _deadline: Instant,
    ) -> Result<Arc<dyn Running>, GenericError> {
        let Self {
            ledger,
            dir,
            env,
            programs,
            ports,
        } = *self;
        let mut outputs = BTreeMap::new();
        let mut exits = Vec::new();
        for program in programs {
            if cancel.is_cancelled() {
                return Err(generic_error!("Cancelled while starting program '{}'.", program.name));
            }
            let command = HostCommand {
                name: program.name.clone(),
                program: program.path,
                args: program.args,
                env: program.env,
                dir: dir.clone(),
                stdin: None,
            };
            let (process, output, exit) = HostProcess::spawn(command)?;
            // At once, so a failure of the next program leaves this one to reclaim.
            ledger.host_process(process);
            outputs.insert(program.name.clone(), output);
            exits.push((program.name, exit));
        }
        Ok(Arc::new(RealizedHost {
            dir,
            env,
            exits,
            outputs: Mutex::new(outputs),
            ports,
        }))
    }
}

/// A participant's started programs.
struct RealizedHost {
    dir: PathBuf,
    env: BTreeMap<String, String>,
    /// The exit of each program, in start order.
    exits: Vec<(String, ExitWatch)>,
    /// The output of each program that is not captured yet.
    outputs: Mutex<BTreeMap<String, OutputStream>>,
    ports: BTreeMap<String, u16>,
}

impl fmt::Debug for RealizedHost {
    /// Omits the output streams, which aren't `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealizedHost")
            .field("dir", &self.dir)
            .field("exits", &self.exits)
            .finish_non_exhaustive()
    }
}

impl RealizedHost {
    /// The status of an exit.
    fn status(exit: HostExit) -> Result<ExitStatus, String> {
        match (exit.code, exit.signal) {
            (Some(code), _) => Ok(ExitStatus {
                code: code.into(),
                signal: None,
            }),
            (None, Some(signal)) => Ok(ExitStatus {
                code: SIGNALLED_CODE,
                signal: Some(signal.unsigned_abs()),
            }),
            (None, None) => Err("the OS reported neither an exit code nor a signal".to_string()),
        }
    }

    /// The code of the first program, once every program has exited.
    fn first_code(&self) -> Result<i64, GenericError> {
        let Some((name, exit)) = self.exits.first() else {
            return Err(generic_error!("The participant has no programs."));
        };
        let exit = exit.borrow().clone();
        match exit {
            Some(Ok(exit)) => Self::status(exit)
                .map(|status| status.code)
                .map_err(|why| generic_error!("The exit of '{}' cannot be determined: {}", name, why)),
            Some(Err(why)) => Err(generic_error!("The exit of '{}' cannot be determined: {}", name, why)),
            None => Err(generic_error!("Program '{}' has not exited.", name)),
        }
    }

    /// The file `path` names in the participant: `./x` under its directory, or an absolute path.
    fn file(&self, path: &str) -> Result<PathBuf, HandleError> {
        match path.strip_prefix("./") {
            Some(rest) => Ok(self.dir.join(rest)),
            None if Path::new(path).is_absolute() => Ok(PathBuf::from(path)),
            None => Err(HandleError::Harness(format!(
                "'{}' is not a path of a host participant: write './…', under its directory, or an absolute path",
                path
            ))),
        }
    }
}

#[async_trait]
impl Realized for RealizedHost {
    async fn exec(
        &self, exec: Exec, stdout: &mut (dyn AsyncWrite + Send + Unpin), stderr: &mut (dyn AsyncWrite + Send + Unpin),
        deadline: Instant,
    ) -> Result<i64, HandleError> {
        let harness = |e: String| HandleError::Harness(e);
        if self.has_exited().await {
            return Err(HandleError::Software(
                "the participant has exited, so it cannot run a command".to_string(),
            ));
        }
        let Some((program, args)) = exec.argv.split_first() else {
            return Err(harness("a command needs a program".to_string()));
        };
        let path = locate(program, &self.dir).map_err(harness)?;
        let mut env = self.env.clone();
        env.extend(exec.env);
        let command = HostCommand {
            name: program.clone(),
            program: path,
            args: args.to_vec(),
            env,
            dir: self.dir.clone(),
            stdin: exec.stdin,
        };
        let (process, mut output, mut exit) = HostProcess::spawn(command).map_err(|e| harness(format!("{:#}", e)))?;

        let copy = async {
            while let Some(chunk) = output.next().await {
                let written = match chunk.map_err(|e| format!("{:#}", e))? {
                    Output::Stdout(bytes) => stdout.write_all(&bytes).await,
                    Output::Stderr(bytes) => stderr.write_all(&bytes).await,
                };
                written.map_err(|e| format!("The output of '{}' cannot be passed on: {}", program, e))?;
            }
            Ok::<_, String>(())
        };
        let waited = async {
            match exit.wait_for(Option::is_some).await.map(|exited| exited.clone()) {
                Ok(Some(exited)) => exited,
                Ok(None) => Err(format!("'{}' has not exited", program)),
                Err(e) => Err(format!("The exit of '{}' is not reported: {}", program, e)),
            }
        };
        tokio::pin!(copy, waited);
        let ran = async {
            tokio::select! {
                copied = &mut copy => {
                    copied?;
                    waited.await
                }
                exited = &mut waited => {
                    // A background child can hold the pipes after the command has exited, so read for a short time.
                    if let Ok(copied) = timeout(OUTPUT_AFTER_EXIT, &mut copy).await {
                        copied?;
                    }
                    exited
                }
            }
        };
        let exited = match timeout_at(deadline, ran).await {
            Ok(exited) => exited.map_err(harness)?,
            Err(_) => {
                // Dropping the process kills its group.
                drop(process);
                return Err(harness(format!("'{}' did not exit within the deadline", program)));
            }
        };
        // Kill what the command left behind in its group.
        let _ = process.stop(Duration::ZERO).await;
        match (exited.code, exited.signal) {
            (Some(code), _) => Ok(code.into()),
            (None, Some(signal)) => Ok(128 + i64::from(signal)),
            (None, None) => Err(harness(format!("'{}' ended with neither a code nor a signal", program))),
        }
    }

    async fn has_exited(&self) -> bool {
        self.exits.iter().all(|(_, exit)| exit.borrow().is_some())
    }

    async fn read_file(&self, path: &str, deadline: Instant) -> Result<Option<Vec<u8>>, HandleError> {
        let file = self.file(path)?;
        match timeout_at(deadline, tokio::fs::read(&file)).await {
            Err(_) => Err(HandleError::Harness(format!(
                "Reading '{}' did not finish within the deadline.",
                file.display()
            ))),
            Ok(Ok(bytes)) => Ok(Some(bytes)),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Ok(Err(e)) => Err(HandleError::Harness(format!(
                "Failed to read '{}': {}",
                file.display(),
                e
            ))),
        }
    }

    fn reported_exit(&self, process: &str) -> Option<ProcessExit> {
        let (_, exit) = self.exits.iter().find(|(name, _)| name == process)?;
        let exit = exit.borrow().clone();
        Some(match exit {
            None => ProcessExit::Running,
            Some(Ok(exit)) => match Self::status(exit) {
                Ok(status) => ProcessExit::Exited(status),
                Err(why) => ProcessExit::Undetermined(why),
            },
            Some(Err(why)) => ProcessExit::Undetermined(why),
        })
    }

    fn host_port(&self, endpoint: &str) -> Option<u16> {
        self.ports.get(endpoint).copied()
    }
}

#[async_trait]
impl Running for RealizedHost {
    fn identities(&self) -> Identities {
        Identities::default()
    }

    async fn wait(&self, deadline: Instant) -> Result<i64, GenericError> {
        for (name, exit) in &self.exits {
            let mut exit = exit.clone();
            timeout_at(deadline, exit.wait_for(Option::is_some))
                .await
                .map_err(|_| generic_error!("Program '{}' had not exited when the deadline passed.", name))?
                .map_err(|e| generic_error!("The exit of '{}' is not reported: {}", name, e))?;
        }
        self.first_code()
    }

    async fn exit_now(&self, _deadline: Instant) -> Result<Option<i64>, GenericError> {
        if self.has_exited().await {
            self.first_code().map(Some)
        } else {
            Ok(None)
        }
    }

    fn check_capture(&self, participant: &str, process: &ProcessSpec) -> Result<(), GenericError> {
        match process.output() {
            OutputChannel::Container => Ok(()),
            OutputChannel::File { .. } => Err(generic_error!(
                "Participant '{}' runs host processes, which have no log directory.",
                participant
            )),
        }
    }

    async fn capture(
        &self, _participant: &str, process: &ProcessSpec, sink: Sink, _limit: Duration, finish: CancellationToken,
    ) {
        let output = self
            .outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(process.name());
        match output {
            Some(output) => sink.capture(output, finish).await,
            None => sink.failed(format!(
                "the output of program '{}' is already captured, or the program does not exist",
                process.name()
            )),
        }
    }

    async fn capture_log(&self, _sink: Sink, _finish: CancellationToken) {
        // Every host process is observed through its own pipes, so the lifecycle never asks for a participant log.
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::core::case::{self, CASE_FILE};
    use crate::core::participant::Os;
    use crate::core::plan::{plan, CasePlan};
    use crate::core::registry::Registry;
    use crate::core::tasks::CaseTasks;

    const CASE_TIME: Duration = Duration::from_secs(30);

    /// A case with one participant, `app`, that runs `main`, which writes its PID to `pid_file` and sleeps, and, with
    /// `broken`, `gone`, a script whose interpreter does not exist, so its start fails after `main` runs. A file with no
    /// `#!` line is no good here: macOS starts it with `sh`, where Linux refuses to.
    fn planned(suite: &Path, pid_file: &Path, broken: bool) -> CasePlan {
        let dir = suite.join("case");
        fs::create_dir_all(&dir).unwrap();
        let mut config = format!(
            "
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    realization: {{enclosure: host-process-group}}
    programs:
      - name: main
        path: sh
        args: [-c, 'echo $$ > {}; sleep 60']
",
            pid_file.display()
        );
        if broken {
            config.push_str(
                "      - {name: gone, path: ./gone}
    files:
      ./gone: {from: gone, mode: \"0755\"}
",
            );
            fs::write(dir.join("gone"), "#!/nonexistent/interpreter\n").unwrap();
        }
        config.push_str("procedure: []\n");
        fs::write(dir.join(CASE_FILE), config).unwrap();
        let registry = Registry::default();
        let case = case::load(&dir, &registry).unwrap();
        plan(&case, Os::Linux, &registry, &BTreeMap::new()).unwrap()
    }

    /// Waits until `pid_file` holds the PID of `main`, and returns it.
    async fn started_pid(pid_file: &Path) -> String {
        for _ in 0..100 {
            if let Some(pid) = fs::read_to_string(pid_file).ok().filter(|pid| pid.ends_with('\n')) {
                return pid.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("main never wrote its PID");
    }

    /// Whether the process `pid` exists, as `kill -0` says.
    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .output()
            .unwrap()
            .status
            .success()
    }

    /// Checks that the process `pid` runs, then reclaims `ledger` and checks that `root` and the process are gone.
    async fn assert_nothing_left(ledger: &ResourceLedger, root: &Path, pid: &str) {
        assert!(alive(pid), "process {pid} is not running before reclaim");
        let (_, issues) = ledger.reclaim(Instant::now() + CASE_TIME).await;
        assert!(issues.is_empty(), "{issues:?}");
        assert!(!root.exists(), "{} is left", root.display());
        for _ in 0..100 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("process {pid} is left");
    }

    #[tokio::test]
    async fn a_failed_start_and_a_cancelled_case_leave_no_process_or_directory() {
        for broken in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let pids = tempfile::tempdir().unwrap();
            let pid_file = pids.path().join("main.pid");
            let plan = planned(temporary.path(), &pid_file, broken);
            let root = temporary.path().join("root");

            let token = CancellationToken::new();
            let tasks = CaseTasks::new(&token);
            let ledger = ResourceLedger::default();
            let deadline = Instant::now() + CASE_TIME;
            let cx = Preparing {
                ledger: &ledger,
                tasks: &tasks,
                cancel: &token,
                deadline,
            };
            let scope = HostBackend::new(root.clone()).prepare(&cx).await.unwrap();
            let values = Values::of_plan(&plan.participants);
            let created = scope
                .create("app", &plan.participants["app"], &values, &token, deadline)
                .await
                .unwrap();
            let started = created.start(&values, &token, deadline).await;

            if broken {
                let error = started.expect_err("the second program cannot start");
                assert!(error.to_string().contains("gone"), "{error:#}");
            } else {
                started.expect("the program should start");
                token.cancel();
            }
            let pid = started_pid(&pid_file).await;
            assert_nothing_left(&ledger, &root, &pid).await;
        }
    }
}
