use std::{collections::BTreeMap, future::Future, os::unix::fs::symlink};

use tokio::net::{TcpListener, UdpSocket};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    artifact::{ArtifactKind, ArtifactRole, VerifiedArtifact},
    backend::RunBackends,
    endpoint::EndpointKind,
    layout::{Realization, RealizationKind},
    ledger::random_id,
    lifecycle::NativeInputs,
    participant::{ExitChannel, OutputChannel, OutputStream},
    recipe::{HostProcess, RecipeFile},
    result::ReclaimResult,
};

const SHELL: ArtifactRole = ArtifactRole {
    name: "host-test-shell",
    kind: ArtifactKind::Binary,
    local_default: "/bin/sh",
};
const SPECS: &[ProcessSpec] = &[
    ProcessSpec {
        name: "first",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    },
    ProcessSpec {
        name: "second",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    },
];
const BOUND: Duration = Duration::from_secs(10);

fn inputs(role: ArtifactRole, path: &Path) -> NativeInputs {
    NativeInputs::new(
        Arc::new(RunBackends::default()),
        &[VerifiedArtifact {
            role: role.name,
            kind: role.kind,
            reference: path.to_string_lossy().into(),
            identity: "verified".into(),
        }],
        Path::new("."),
    )
    .unwrap()
}

struct Fixture {
    scope: HostScope,
    layout: ScopeLayout,
    ledger: Option<ResourceLedger>,
    tasks: CaseTasks,
    logs: tempfile::TempDir,
    inputs: NativeInputs,
}

impl Fixture {
    fn new() -> Self {
        let layout = ScopeLayout {
            run: random_id(),
            index: 0,
        };
        let ledger = ResourceLedger::default();
        let scope = prepare(&layout, &["subject"], &ledger).unwrap();
        Self {
            scope,
            layout,
            ledger: Some(ledger),
            tasks: CaseTasks::new(&CancellationToken::new()),
            logs: tempfile::tempdir().unwrap(),
            inputs: inputs(SHELL, Path::new("/bin/sh")),
        }
    }

    fn process(&self, name: &str, script: &str) -> HostProcess {
        HostProcess::new(
            name,
            SHELL,
            vec![
                "-c".into(),
                script.into(),
                format!("panoramic-host-{}-{name}", self.layout.run),
            ],
        )
    }

    fn participant(
        &self, recipe: HostRecipe, specs: &'static [ProcessSpec], grace: Duration,
    ) -> Arc<dyn RuntimeParticipant> {
        let participant = self
            .scope
            .participant(ParticipantRequest {
                name: "subject",
                realization: Realization {
                    os: Os::host(),
                    kind: RealizationKind::HostProcessGroup,
                },
                recipe: Recipe::HostProcessGroup(recipe),
                specs,
                runner_host_ports: vec![],
                inputs: &self.inputs,
                options: &BTreeMap::new(),
            })
            .unwrap();
        self.ledger
            .as_ref()
            .unwrap()
            .participant("subject", participant.removal(), grace);
        participant
    }

    async fn reclaim(&mut self) {
        assert_eq!(self.tasks.stop_all(Instant::now() + BOUND).await, 0);
        let (entries, issues) = self.ledger.take().unwrap().reclaim(Instant::now() + BOUND).await;
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            entries.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
            "{entries:?}"
        );
        assert!(!self.scope.directory.exists());
        assert!(!self.scope.directory.parent().unwrap().exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.ledger.take();
        let _ = std::fs::remove_dir_all(&self.scope.directory);
        let _ = std::fs::remove_dir(self.scope.directory.parent().unwrap());
    }
}

async fn until(mut ready: impl FnMut() -> bool) {
    timeout(BOUND, async {
        while !ready() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the expected state must arrive within the bound");
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    timeout(BOUND, future)
        .await
        .expect("operation must finish within its bound")
}

#[tokio::test]
async fn preparation_preserves_planned_paths_and_never_registers_an_existing_directory() {
    let mut fixture = Fixture::new();
    assert_eq!(
        fixture.scope.directory,
        PathBuf::from(fixture.layout.state_dir(Os::host()))
    );
    assert!(fixture.scope.directory.join("subject/shared").is_dir());
    assert_eq!(
        fixture.ledger.as_ref().unwrap().labels(),
        [format!("directory:{}", fixture.scope.directory.display())]
    );
    let other = ResourceLedger::default();
    let error = prepare(&fixture.layout, &["subject"], &other).err().unwrap();
    assert!(
        error.contains(&fixture.scope.directory.display().to_string()),
        "{error}"
    );
    assert!(other.labels().is_empty());
    assert!(other.reclaim(Instant::now() + BOUND).await.0.is_empty());
    assert!(fixture.scope.directory.join("subject/shared").is_dir());
    fixture.reclaim().await;
}

#[test]
fn preparation_retries_when_reclaim_removes_the_run_directory_before_case_creation() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("run/0000");
    let mut attempts = 0;
    create_state_directory(&directory, |path| {
        attempts += 1;
        assert!(attempts <= 2, "the retry must restore the parent directory");
        if attempts == 1 {
            std::fs::remove_dir(path.parent().unwrap()).unwrap();
        }
        std::fs::create_dir(path)
    })
    .unwrap();
    assert_eq!(attempts, 2);
    assert!(directory.is_dir());
    let mut attempts = 0;
    let error = create_state_directory(&directory, |path| {
        attempts += 1;
        assert_eq!(attempts, 1, "an existing case directory must not be retried");
        std::fs::create_dir(path)
    })
    .unwrap_err();
    assert_eq!(attempts, 1);
    assert!(error.contains(&directory.display().to_string()), "{error}");
}

#[tokio::test]
async fn reclaim_keeps_a_run_directory_until_its_last_scope_is_removed() {
    let mut first = Fixture::new();
    let layout = ScopeLayout {
        run: first.layout.run.clone(),
        index: 1,
    };
    let second = ResourceLedger::default();
    let scope = prepare(&layout, &["other"], &second).unwrap();
    first.tasks.stop_all(Instant::now() + BOUND).await;
    let (entries, _) = first.ledger.take().unwrap().reclaim(Instant::now() + BOUND).await;
    assert_eq!(entries[0].result, ReclaimResult::Reclaimed);
    assert!(!first.scope.directory.exists());
    assert!(scope.directory.join("other/shared").is_dir());
    let (entries, _) = second.reclaim(Instant::now() + BOUND).await;
    assert_eq!(entries[0].result, ReclaimResult::Reclaimed);
    assert!(!scope.directory.parent().unwrap().exists());
}

#[tokio::test]
async fn creation_places_relative_files_with_modes_before_any_process_runs() {
    let mut fixture = Fixture::new();
    let mut recipe = HostRecipe::new(vec![fixture.process("first", "cat nested/settings")]);
    recipe.files.push(RecipeFile {
        path: "nested/settings".into(),
        mode: 0o640,
        contents: b"configured".to_vec(),
    });
    let participant = fixture.participant(recipe, &SPECS[..1], Duration::ZERO);
    assert!(participant.create().await.unwrap().is_empty());
    let dir = fixture.scope.directory.join("subject");
    assert_eq!(std::fs::read(dir.join("nested/settings")).unwrap(), b"configured");
    assert_eq!(
        std::fs::metadata(dir.join("nested/settings"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    bounded(observation.stopped()).await;
    bounded(observation.finish_output()).await;
    assert_eq!(
        std::fs::read(fixture.logs.path().join("first.stdout.log")).unwrap(),
        b"configured"
    );
    fixture.reclaim().await;
}

#[tokio::test]
async fn creation_rejects_paths_that_are_not_plain_relative_files() {
    for path in [
        "",
        "/outside",
        ".",
        "..",
        "../outside",
        "nested/../outside",
        "nested/./file",
        "nested\\file",
        "nested//file",
    ] {
        let mut fixture = Fixture::new();
        let mut recipe = HostRecipe::new(vec![fixture.process("first", "echo started > started")]);
        recipe.files.push(RecipeFile {
            path: path.into(),
            mode: 0o600,
            contents: vec![],
        });
        let participant = fixture.participant(recipe, &SPECS[..1], Duration::ZERO);
        let error = participant.create().await.unwrap_err().to_string();
        assert!(error.contains(&format!("recipe file '{path}'")), "{error}");
        assert!(!fixture.scope.directory.join("subject/started").exists());
        fixture.reclaim().await;
    }
}

#[tokio::test]
async fn creation_names_missing_or_non_executable_programs_without_starting_anything() {
    for kind in ["missing", "file", "directory"] {
        let mut fixture = Fixture::new();
        let path = fixture.logs.path().join(kind);
        match kind {
            "file" => {
                std::fs::write(&path, b"exit 0").unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            "directory" => std::fs::create_dir(&path).unwrap(),
            _ => {}
        }
        fixture.inputs = inputs(SHELL, &path);
        let participant = fixture.participant(
            HostRecipe::new(vec![fixture.process("first", "echo started > started")]),
            &SPECS[..1],
            Duration::ZERO,
        );
        let error = format!("{:#}", participant.create().await.unwrap_err());
        assert!(
            error.contains("process 'first'") && error.contains(&path.display().to_string()),
            "{error}"
        );
        assert!(!fixture.scope.directory.join("subject/started").exists());
        fixture.reclaim().await;
    }
}

#[tokio::test]
async fn creation_names_missing_extra_and_duplicate_recipe_processes() {
    for (names, expected) in [
        (vec!["first"], "missing process 'second'"),
        (vec!["first", "second", "third"], "extra process 'third'"),
        (vec!["first", "first"], "repeats process 'first'"),
    ] {
        let mut fixture = Fixture::new();
        let recipe = HostRecipe::new(
            names
                .into_iter()
                .map(|name| fixture.process(name, "echo started > started"))
                .collect(),
        );
        let participant = fixture.participant(recipe, SPECS, Duration::ZERO);
        let error = participant.create().await.unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
        assert!(!fixture.scope.directory.join("subject/started").exists());
        fixture.reclaim().await;
    }
}

#[tokio::test]
async fn an_install_role_runs_its_main_binary_and_records_paths_relative_to_the_input_base() {
    let mut fixture = Fixture::new();
    let role = ArtifactRole {
        name: "host-test-install",
        kind: ArtifactKind::Install {
            main_binary: "bin/shell",
        },
        local_default: "",
    };
    std::fs::create_dir(fixture.logs.path().join("bin")).unwrap();
    symlink("/bin/sh", fixture.logs.path().join("bin/shell")).unwrap();
    fixture.inputs = NativeInputs::new(
        Arc::default(),
        &[VerifiedArtifact {
            role: role.name,
            kind: role.kind,
            reference: ".".into(),
            identity: "verified".into(),
        }],
        fixture.logs.path(),
    )
    .unwrap();
    let mut process = fixture.process("first", "echo installed");
    process.program = role;
    let participant = fixture.participant(HostRecipe::new(vec![process]), &SPECS[..1], Duration::ZERO);
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    bounded(observation.stopped()).await;
    bounded(observation.finish_output()).await;
    assert_eq!(
        std::fs::read(&observation.outputs["first"].paths[0]).unwrap(),
        b"installed\n"
    );
    fixture.reclaim().await;
}

#[tokio::test]
async fn processes_get_a_clean_shared_environment_with_per_process_overrides_and_a_working_directory() {
    let mut fixture = Fixture::new();
    let mut second = fixture.process("second", "env; pwd");
    second.env.insert("CHOICE".into(), "second".into());
    let mut recipe = HostRecipe::new(vec![fixture.process("first", "env; pwd"), second]);
    recipe.env = BTreeMap::from([
        ("SHARED".into(), "both".into()),
        ("CHOICE".into(), "participant".into()),
        ("PATH".into(), "/bin:/usr/bin".into()),
    ]);
    let participant = fixture.participant(recipe, SPECS, Duration::ZERO);
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    bounded(observation.stopped()).await;
    bounded(observation.finish_output()).await;
    for (name, choice) in [("first", "participant"), ("second", "second")] {
        let text = std::fs::read_to_string(&observation.outputs[name].paths[0]).unwrap();
        assert!(text.lines().any(|line| line == "SHARED=both"), "{text}");
        assert!(text.lines().any(|line| line == format!("CHOICE={choice}")), "{text}");
        assert!(!text.contains("CARGO_"), "{text}");
        for line in text.lines().filter(|line| line.contains('=')) {
            let key = line.split_once('=').unwrap().0;
            assert!(
                ["PATH", "HOME", "TMPDIR", "SHARED", "CHOICE", "PWD", "OLDPWD", "SHLVL", "_"].contains(&key),
                "inherited {line}"
            );
        }
        let cwd = Path::new(text.lines().last().unwrap());
        assert_eq!(
            std::fs::canonicalize(cwd).unwrap(),
            std::fs::canonicalize(fixture.scope.directory.join("subject")).unwrap()
        );
    }
    fixture.reclaim().await;
}

#[tokio::test]
async fn pipes_are_separate_and_tagged_while_the_group_waits_for_every_process_to_end() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(
        HostRecipe::new(vec![
            fixture.process("first", "echo out; echo err >&2; kill -TERM $$"),
            fixture.process(
                "second",
                "echo waiting; while [ ! -f release ]; do sleep 0.01; done; exit 3",
            ),
        ]),
        SPECS,
        Duration::ZERO,
    );
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    let first = bounded(observation.process("first").unwrap().settled()).await;
    assert!(matches!(
        first,
        ExitObservation::Exited(ExitStatus {
            code: None,
            signal: Some(15),
            ..
        })
    ));
    until(|| observation.outputs["first"].current() == Capture::Complete).await;
    assert_eq!(observation.exit(), ParticipantExit::Running);
    assert_eq!(std::fs::read(&observation.outputs["first"].paths[0]).unwrap(), b"out\n");
    assert_eq!(std::fs::read(&observation.outputs["first"].paths[1]).unwrap(), b"err\n");
    let (lines, _) = observation.lines_since(0).unwrap();
    assert!(lines
        .iter()
        .any(|line| line.process == Some("first") && line.stream == OutputStream::Stdout && line.text == "out"));
    assert!(lines
        .iter()
        .any(|line| line.process == Some("first") && line.stream == OutputStream::Stderr && line.text == "err"));
    std::fs::write(fixture.scope.directory.join("subject/release"), "").unwrap();
    assert_eq!(
        bounded(observation.stopped()).await,
        "ended: process 'first' was killed by signal 15, process 'second' exited with code 3"
    );
    bounded(observation.finish_output()).await;
    let report = serde_json::to_value(observation.snapshot(SPECS)).unwrap();
    assert_eq!(report[0]["exit"]["source"], "process");
    assert_eq!(report[0]["exit"]["signal"], 15);
    assert!(report[0]["exit"].get("code").is_none());
    assert_eq!(report[1]["exit"]["code"], 3);
    assert_eq!(
        report[0]["output"]["stdout"],
        fixture.logs.path().join("first.stdout.log").to_str().unwrap()
    );
    assert!(!fixture.logs.path().join("container.stdout.log").exists());
    fixture.reclaim().await;
}

#[test]
fn waiting_errors_are_undetermined_and_never_become_exit_codes() {
    assert_eq!(
        observe_wait("first", Err(io::Error::other("lost wait"))),
        ExitObservation::Undetermined("waiting for process 'first' failed: lost wait".into())
    );
    assert!(matches!(
        observe_wait("first", Ok(OsExitStatus::from_raw(3 << 8))),
        ExitObservation::Exited(ExitStatus {
            code: Some(3),
            signal: None,
            ..
        })
    ));
    assert!(matches!(
        observe_wait("first", Ok(OsExitStatus::from_raw(libc::SIGKILL))),
        ExitObservation::Exited(ExitStatus {
            code: None,
            signal: Some(9),
            ..
        })
    ));
}

#[tokio::test]
async fn exec_uses_the_participant_environment_and_stdin_and_kills_a_timed_out_group() {
    let mut fixture = Fixture::new();
    let mut recipe = HostRecipe::new(vec![fixture.process("first", "exit 0")]);
    recipe.env.insert("SHARED".into(), "value".into());
    let participant = fixture.participant(recipe, &SPECS[..1], Duration::ZERO);
    participant.create().await.unwrap();
    let command = |script: &str| ExecCommand {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            script.into(),
            format!("panoramic-host-{}-exec", fixture.layout.run),
        ],
        env: vec![("CHOICE".into(), "override".into())],
        stdin: Some(b"input".to_vec()),
    };
    let result = participant
        .exec(
            command("cat; printf '%s:%s' \"$SHARED\" \"$CHOICE\"; printf err >&2; exit 7"),
            BOUND,
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        ExecOutput {
            stdout: b"inputvalue:override".to_vec(),
            stderr: b"err".to_vec(),
            exit_code: 7
        }
    );
    let mut early_exit = command("exit 3");
    early_exit.stdin = Some(vec![b'x'; 1024 * 1024]);
    let result = participant.exec(early_exit, BOUND).await.unwrap();
    assert_eq!(
        result,
        ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 3
        }
    );
    let mut background = command("sleep 60 >/dev/null 2>&1 & echo $$");
    background.stdin = None;
    let result = participant.exec(background, BOUND).await.unwrap();
    let pgid: i32 = String::from_utf8(result.stdout).unwrap().trim().parse().unwrap();
    let _cleanup = ExecGroup(pgid);
    assert_eq!(result.exit_code, 0);
    until(|| !group_exists(pgid)).await;
    let mut signalled = command("kill -TERM $$");
    signalled.stdin = None;
    assert_eq!(participant.exec(signalled, BOUND).await.unwrap().exit_code, 143);
    let pidfile = fixture.scope.directory.join("subject/exec-pid");
    let error = participant
        .exec(
            command("echo $$ > exec-pid; trap '' TERM; while :; do sleep 1; done"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, HandleError::Harness { ref message } if message.contains("killed its process group")),
        "{error}"
    );
    let pid: i32 = std::fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    until(|| !group_exists(pid)).await;
    fixture.reclaim().await;
}

#[tokio::test]
async fn exec_command_pipes_supplied_input_without_caller_setup() {
    let directory = tempfile::tempdir().unwrap();
    let participant = HostParticipant {
        name: "subject".into(),
        directory: directory.path().into(),
        recipe: HostRecipe::new(vec![]),
        programs: vec![],
        specs: &[],
        group: Arc::new(Mutex::new(Processes {
            children: vec![],
            armed: true,
        })),
    };
    for input in [b"".as_slice(), b"input\0\xff".as_slice()] {
        let output = participant
            .exec_command(participant.command("/bin/cat"), Some(input.to_vec()), BOUND)
            .await
            .unwrap();
        assert_eq!(
            output,
            ExecOutput {
                stdout: input.to_vec(),
                stderr: vec![],
                exit_code: 0
            }
        );
    }
}

#[tokio::test]
async fn exec_process_runs_the_artifact_bound_program_with_replacement_arguments() {
    let mut fixture = Fixture::new();
    fixture.inputs = inputs(SHELL, Path::new("/bin/echo"));
    let recipe = HostRecipe::new(vec![fixture.process("first", "declared arguments")]);
    let participant = fixture.participant(recipe, &SPECS[..1], Duration::ZERO);
    participant.create().await.unwrap();
    let result = participant
        .exec_process("first", vec!["replacement".into(), "a b".into()], BOUND)
        .await
        .unwrap();
    assert_eq!(
        result,
        ExecOutput {
            stdout: b"replacement a b\n".to_vec(),
            stderr: vec![],
            exit_code: 0,
        }
    );
    fixture.reclaim().await;
}

#[tokio::test]
async fn exec_process_replaces_arguments_in_the_participant_directory_with_its_process_environment() {
    let mut fixture = Fixture::new();
    let mut process = fixture.process("first", "exit 99");
    process.env.insert("CHOICE".into(), "process".into());
    process.env.insert("OWN".into(), "own".into());
    let mut recipe = HostRecipe::new(vec![fixture.process("second", "exit 88"), process]);
    recipe.env.insert("SHARED".into(), "group".into());
    recipe.env.insert("CHOICE".into(), "group".into());
    let participant = fixture.participant(recipe, SPECS, Duration::ZERO);
    participant.create().await.unwrap();
    let result = participant
        .exec_process(
            "first",
            vec![
                "-c".into(),
                "printf '%s:%s:%s' \"$SHARED\" \"$CHOICE\" \"$OWN\"; printf working > marker; printf error >&2; exit 7"
                    .into(),
            ],
            BOUND,
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        ExecOutput {
            stdout: b"group:process:own".to_vec(),
            stderr: b"error".to_vec(),
            exit_code: 7
        }
    );
    assert_eq!(
        std::fs::read(fixture.scope.directory.join("subject/marker")).unwrap(),
        b"working"
    );
    let error = participant.exec_process("unknown", vec![], BOUND).await.unwrap_err();
    assert_eq!(
        error,
        HandleError::Harness {
            message: "participant 'subject' has no declared process 'unknown'".into()
        }
    );
    fixture.reclaim().await;
}

#[tokio::test]
async fn exec_process_kills_its_process_group_at_its_limit() {
    let mut fixture = Fixture::new();
    let recipe = HostRecipe::new(vec![fixture.process("first", "exit 0")]);
    let participant = fixture.participant(recipe, &SPECS[..1], Duration::ZERO);
    participant.create().await.unwrap();
    let error = participant
        .exec_process(
            "first",
            vec![
                "-c".into(),
                "echo $$ > exec-pid; trap '' TERM; while :; do sleep 1; done".into(),
                format!("panoramic-host-{}-exec-process", fixture.layout.run),
            ],
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, HandleError::Harness { ref message } if message.contains("killed its process group")),
        "{error}"
    );
    let pid: i32 = std::fs::read_to_string(fixture.scope.directory.join("subject/exec-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _cleanup = ExecGroup(pid);
    until(|| !group_exists(pid)).await;
    fixture.reclaim().await;
}

#[tokio::test]
async fn files_report_absence_content_modes_and_kinds_including_links_and_sockets() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(HostRecipe::new(vec![]), &[], Duration::ZERO);
    participant.create().await.unwrap();
    let dir = fixture.scope.directory.join("subject");
    std::fs::write(dir.join("file"), b"contents").unwrap();
    std::fs::set_permissions(dir.join("file"), std::fs::Permissions::from_mode(0o640)).unwrap();
    symlink("file", dir.join("link")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
    assert_eq!(participant.read_file("missing").await.unwrap(), None);
    assert_eq!(participant.stat("missing").await.unwrap(), None);
    assert_eq!(participant.read_file("file").await.unwrap(), Some(b"contents".to_vec()));
    assert!(participant.read_file("shared").await.is_err());
    assert_eq!(
        participant.stat("file").await.unwrap(),
        Some(FileStatus {
            kind: FileKind::File,
            mode: 0o640,
            size: 8
        })
    );
    assert_eq!(
        participant.stat("shared").await.unwrap().unwrap().kind,
        FileKind::Directory
    );
    assert_eq!(
        participant.stat("link").await.unwrap().unwrap().kind,
        FileKind::Symlink { target: "file".into() }
    );
    assert_eq!(
        participant.stat("socket").await.unwrap().unwrap().kind,
        FileKind::Socket
    );
    fixture.reclaim().await;
}

#[tokio::test]
async fn probe_and_send_reach_loopback_without_a_courier_process() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(HostRecipe::new(vec![]), &[], Duration::ZERO);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let address = ResolvedAddress {
        kind: EndpointKind::Tcp,
        host: Some("127.0.0.1".into()),
        port: Some(port),
        path: None,
        url: format!("tcp://127.0.0.1:{port}"),
    };
    let limits = AttemptLimits {
        request: Duration::from_secs(2),
        attempt: Duration::from_secs(3),
    };
    assert_eq!(
        participant.probe(&address, &limits).await.unwrap(),
        ProbeAnswer::Listening
    );
    let _ = listener.accept().await.unwrap();
    participant.send(&address, b"stream", &limits).await.unwrap();
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    bounded(stream.read_to_end(&mut bytes)).await.unwrap();
    assert_eq!(bytes, b"stream");
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let address = ResolvedAddress {
        kind: EndpointKind::Udp,
        port: Some(port),
        url: format!("udp://127.0.0.1:{port}"),
        ..address
    };
    participant.send(&address, b"packet", &limits).await.unwrap();
    let mut bytes = [0; 32];
    let count = bounded(socket.recv(&mut bytes)).await.unwrap();
    assert_eq!(&bytes[..count], b"packet");
    fixture.reclaim().await;
}

#[tokio::test]
async fn reclaim_reaps_after_case_tasks_stop_and_kills_resistant_descendants_of_exited_leaders() {
    let fixture = Fixture::new();
    let participant = fixture.participant(HostRecipe::new(vec![
        fixture.process("first", "echo $$ > leader; sh -c 'trap \"\" TERM; echo $$ > descendant; while :; do sleep 1; done' \"$0-child\" & while [ ! -f descendant ]; do sleep 0.01; done; exit 0"),
        fixture.process("second", "echo $$ > second; trap 'echo term > terminated; exit 0' TERM; echo ready; while :; do :; done"),
    ]), SPECS, Duration::from_millis(200));
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    bounded(observation.process("first").unwrap().settled()).await;
    let dir = fixture.scope.directory.join("subject");
    until(|| {
        observation
            .lines_since(0)
            .unwrap()
            .0
            .iter()
            .any(|line| line.text == "ready")
    })
    .await;
    let leader: i32 = std::fs::read_to_string(dir.join("leader"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let second = observation.process("second").unwrap().clone();
    assert!(group_exists(leader), "the descendant must outlive its group leader");
    assert_eq!(fixture.tasks.stop_all(Instant::now() + BOUND).await, 0);
    let (entries, issues) = fixture.ledger.as_ref().unwrap().reclaim(Instant::now() + BOUND).await;
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(entries[0].resource, "participant:subject");
    assert!(
        entries.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
        "{entries:?}"
    );
    assert!(matches!(
        second.current(),
        ExitObservation::Exited(ExitStatus { code: Some(0), .. })
    ));
    until(|| !group_exists(leader)).await;
    assert!(!fixture.scope.directory.exists());
}

#[tokio::test]
async fn dropping_the_ledger_kills_registered_groups_while_the_participant_is_still_alive() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(
        HostRecipe::new(vec![
            fixture.process("first", "echo $$ > pid; while :; do sleep 1; done")
        ]),
        &SPECS[..1],
        Duration::ZERO,
    );
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let pidfile = fixture.scope.directory.join("subject/pid");
    until(|| std::fs::read_to_string(&pidfile).is_ok_and(|pid| !pid.trim().is_empty())).await;
    let pid = std::fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    drop(fixture.ledger.take());
    until(|| !group_exists(pid)).await;
    assert!(participant.read_file("pid").await.unwrap().is_some());
}

#[tokio::test]
async fn a_spawn_failure_leaves_earlier_processes_registered_for_reclaim() {
    let mut fixture = Fixture::new();
    let invalid = fixture.logs.path().join("invalid");
    std::fs::write(&invalid, b"#!/panoramic-no-such-interpreter\n").unwrap();
    std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o755)).unwrap();
    let role = ArtifactRole {
        name: "host-test-invalid",
        ..SHELL
    };
    let verified = [
        VerifiedArtifact {
            role: SHELL.name,
            kind: SHELL.kind,
            reference: "/bin/sh".into(),
            identity: "verified".into(),
        },
        VerifiedArtifact {
            role: role.name,
            kind: role.kind,
            reference: invalid.to_string_lossy().into(),
            identity: "verified".into(),
        },
    ];
    fixture.inputs = NativeInputs::new(Arc::default(), &verified, Path::new(".")).unwrap();
    let mut second = fixture.process("second", "");
    second.program = role;
    let participant = fixture.participant(
        HostRecipe::new(vec![
            fixture.process("first", "echo $$ > pid; while :; do sleep 1; done"),
            second,
        ]),
        SPECS,
        Duration::ZERO,
    );
    participant.create().await.unwrap();
    let error = format!("{:#}", participant.start().await.unwrap_err());
    assert!(error.contains("failed to start process 'second'"), "{error}");
    assert_eq!(fixture.ledger.as_ref().unwrap().labels().len(), 2);
    let pidfile = fixture.scope.directory.join("subject/pid");
    until(|| std::fs::read_to_string(&pidfile).is_ok_and(|pid| !pid.trim().is_empty())).await;
    let pid: i32 = std::fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    let _cleanup = ExecGroup(pid);
    assert!(group_exists(pid));
    fixture.reclaim().await;
    until(|| !group_exists(pid)).await;
}

#[tokio::test]
async fn reclaim_signals_every_group_before_waiting_for_any_group() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(
        HostRecipe::new(vec![
            fixture.process(
                "first",
                "trap 'echo done > shared/first; exit 0' TERM; echo ready; while :; do :; done",
            ),
            fixture.process(
                "second",
                "trap 'while [ ! -f shared/first ]; do :; done; exit 0' TERM; echo ready; while :; do :; done",
            ),
        ]),
        SPECS,
        Duration::from_secs(2),
    );
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    until(|| {
        observation
            .lines_since(0)
            .unwrap()
            .0
            .iter()
            .filter(|line| line.text == "ready")
            .count()
            == 2
    })
    .await;
    fixture.reclaim().await;
    for name in ["first", "second"] {
        assert!(matches!(
            observation.process(name).unwrap().current(),
            ExitObservation::Exited(ExitStatus {
                code: Some(0),
                signal: None,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn unconfirmed_reaping_is_bounded_and_keeps_the_host_scope() {
    let mut fixture = Fixture::new();
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "while :; do sleep 1; done",
            &format!("panoramic-host-{}-unconfirmed", fixture.layout.run),
        ])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pgid = child.id().unwrap() as i32;
    let exit = watch::channel(ExitObservation::Undetermined("wait failed".into())).1;
    let group = ProcessGroup {
        state: Arc::new(Mutex::new(Processes {
            children: vec![ChildProcess {
                name: "first",
                pgid,
                exit,
                pipes: None,
                reaper: tokio::spawn(async move {
                    let _ = child.wait().await;
                }),
            }],
            armed: true,
        })),
    };
    fixture
        .ledger
        .as_ref()
        .unwrap()
        .participant("subject", ParticipantRemoval::ProcessGroup(group), Duration::ZERO);
    let (entries, issues) = bounded(
        fixture
            .ledger
            .take()
            .unwrap()
            .reclaim(Instant::now() + Duration::from_millis(200)),
    )
    .await;
    assert_eq!(entries[0].result, ReclaimResult::Unsettled);
    assert_eq!(entries[1].result, ReclaimResult::Leaked);
    assert!(
        issues.iter().any(|issue| issue
            .message
            .contains("kept because participant:subject was not reclaimed")),
        "{issues:?}"
    );
    assert!(!issues.is_empty());
    assert!(fixture.scope.directory.is_dir());
    until(|| !group_exists(pgid)).await;
}

#[tokio::test]
async fn output_held_by_a_descendant_stays_following_until_case_tasks_are_stopped() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(
        HostRecipe::new(vec![
            fixture.process("first", "sh -c 'while :; do sleep 1; done' \"$0-child\" & exit 0")
        ]),
        &SPECS[..1],
        Duration::ZERO,
    );
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let observation = participant.observe(&fixture.tasks, fixture.logs.path()).await;
    bounded(observation.stopped()).await;
    assert_eq!(observation.outputs["first"].current(), Capture::Following);
    assert!(timeout(Duration::from_millis(50), observation.finish_output())
        .await
        .is_err());
    fixture.reclaim().await;
    assert!(matches!(observation.outputs["first"].current(), Capture::Failed { .. }));
}

#[tokio::test]
async fn a_signal_failure_stays_pending_until_its_process_group_disappears() {
    let mut fixture = Fixture::new();
    let participant = fixture.participant(
        HostRecipe::new(vec![
            fixture.process("first", "echo $$ > pid; while :; do sleep 1; done")
        ]),
        &SPECS[..1],
        Duration::ZERO,
    );
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    let pidfile = fixture.scope.directory.join("subject/pid");
    until(|| std::fs::read_to_string(&pidfile).is_ok_and(|pid| !pid.trim().is_empty())).await;
    let pid: i32 = std::fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    let _cleanup = ExecGroup(pid);
    let mut errors = vec![(pid, "signal permission denied".into())];
    assert!(timeout(Duration::from_millis(30), finish_signals(&mut errors))
        .await
        .is_err());
    assert_eq!(errors.len(), 1);
    assert!(group_exists(pid));
    signal_group(pid, libc::SIGKILL).unwrap();
    timeout(BOUND, finish_signals(&mut errors)).await.unwrap();
    assert!(errors.is_empty());
    assert!(!group_exists(pid));
    fixture.reclaim().await;
}
