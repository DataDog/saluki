//! Daemon tests for Windows container participants.
//!
//! They need a Windows Docker daemon, a local Windows image with `cmd` and `pwsh` (`PANORAMIC_TEST_WINDOWS_IMAGE`)
//! and a Windows build of the courier (`PANORAMIC_TEST_WINDOWS_COURIER`, a path). Run them serially to limit
//! concurrent changes to the daemon-wide Windows networking service.

use std::{
    collections::HashMap,
    future::Future,
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use airlock::docker::{image_id, Docker};
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListNetworksOptionsBuilder, ListVolumesOptionsBuilder, LogsOptionsBuilder,
};
use futures::StreamExt as _;
use tokio::time::{sleep, timeout, Instant};
use tokio_util::sync::CancellationToken;

use super::{WindowsParticipant, WindowsScope, COURIER_PATH};
use crate::{
    artifact::{ArtifactKind, ArtifactRole},
    backend::{linux_container::daemon_tests::remaining_resources, LateFact, RunBackends, RuntimeParticipant},
    endpoint::{EndpointKind, ResolvedAddress},
    handle::{ExecCommand, ExecOutput, FileKind, HandleError, Primitives},
    layout::Os,
    ledger::{self, ResourceLedger, RECLAIM_TIME},
    observe::{Capture, ExitObservation, ExitStatus, ParticipantExit, ParticipantObservation, ProcessOutput},
    participant::{ExitChannel, OutputChannel, OutputStream, ProcessSpec},
    plan::ScopeLayout,
    probe::{AttemptLimits, ProbeAnswer},
    recipe::{ContainerRecipe, RecipeFile},
    result::ReclaimResult,
    tasks::{CaseTasks, TASK_STOP_TIME},
};

/// Windows containers start slowly, and `pwsh` takes seconds to start in a cold one.
const SCENARIO_TIME: Duration = Duration::from_secs(300);
const WAIT_TIME: Duration = Duration::from_secs(90);
const EXEC_TIME: Duration = Duration::from_secs(60);
const POLL_TIME: Duration = Duration::from_millis(100);

const IMAGE_ROLE: ArtifactRole = ArtifactRole {
    name: "windows-test-image",
    kind: ArtifactKind::Image { os: Os::Windows },
    local_default: "",
};

const MAIN_SCRIPT: &str = r"C:\test\main.ps1";

/// Defines what every test script shares: signalling the test through the participant's `shared` directory.
const PRELUDE: &str = r#"$ErrorActionPreference = 'Stop'
$shared = "C:\airlock\$($env:PANORAMIC_PARTICIPANT)\shared"
function Signal($name) { [IO.File]::WriteAllText("$shared\$name", $name) }
function Wait-ForFile($name) { while (-not (Test-Path "$shared\$name")) { Start-Sleep -Milliseconds 100 } }
function Append($path, [byte[]]$bytes) {
    $stream = [IO.File]::Open($path, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
    try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
}
"#;

const IDLE: &str = "Signal 'ready'\nWait-ForFile 'never'";

const SPECS: &[ProcessSpec] = &[
    ProcessSpec {
        name: "main",
        exit: ExitChannel::Container,
        output: OutputChannel::Container,
    },
    ProcessSpec {
        name: "svc",
        exit: ExitChannel::Record { file: "svc" },
        output: OutputChannel::File {
            file: "output",
            stream: OutputStream::Merged,
        },
    },
];

const RECORD_SPECS: &[ProcessSpec] = &[
    ProcessSpec {
        name: "main",
        exit: ExitChannel::Container,
        output: OutputChannel::Container,
    },
    ProcessSpec {
        name: "svc",
        exit: ExitChannel::Record { file: "svc" },
        output: OutputChannel::File {
            file: "svc.log",
            stream: OutputStream::Merged,
        },
    },
    ProcessSpec {
        name: "wide",
        exit: ExitChannel::Record { file: "wide" },
        output: OutputChannel::File {
            file: "wide.log",
            stream: OutputStream::Merged,
        },
    },
];

fn windows_image() -> String {
    std::env::var("PANORAMIC_TEST_WINDOWS_IMAGE").expect(
        "PANORAMIC_TEST_WINDOWS_IMAGE must name a local Windows image with cmd and pwsh; these tests never pull",
    )
}

fn courier_bytes() -> Vec<u8> {
    let path = std::env::var("PANORAMIC_TEST_WINDOWS_COURIER")
        .expect("PANORAMIC_TEST_WINDOWS_COURIER must be the path of a Windows panoramic-courier build");
    std::fs::read(&path).unwrap_or_else(|error| panic!("Cannot read the courier '{path}': {error}"))
}

/// Reports daemon isolation and OS version to diagnose archive API failures.
async fn print_daemon_info(docker: &Docker) {
    let info = docker.info().await.expect("the Docker daemon should answer info");
    eprintln!(
        "Windows Docker daemon: isolation={:?} os_version={:?}",
        info.isolation, info.os_version
    );
}

struct TestScope {
    scope: WindowsScope,
    ledger: ResourceLedger,
    id: String,
    docker: Docker,
}

impl TestScope {
    fn participant(&self, name: &str, recipe: &ContainerRecipe, specs: &'static [ProcessSpec]) -> WindowsParticipant {
        let participant = self
            .scope
            .container(name, &windows_image(), recipe, &[], specs)
            .unwrap_or_else(|error| panic!("participant '{name}' should build: {error:#}"));
        self.ledger
            .participant(name, participant.removal(), Duration::from_secs(1));
        participant
    }
}

async fn in_scope<F, S>(participants: &[&str], scenario: S)
where
    F: Future<Output = ()> + Send + 'static,
    S: FnOnce(TestScope, CaseTasks) -> F + Send + 'static,
{
    let backends = Arc::new(RunBackends::default());
    let scope = Arc::new(ScopeLayout {
        run: ledger::random_id(),
        index: 0,
    });
    let participants: Vec<_> = participants.iter().map(|name| (*name).to_owned()).collect();
    eprintln!("Daemon test scope: {}", scope.id());
    let ledger = ResourceLedger::default();
    ledger.container_scope(backends.clone(), (*scope).clone(), Os::Windows);
    let tasks = CaseTasks::new(&CancellationToken::new());
    let outcome = tokio::spawn({
        let tasks = tasks.clone();
        let backends = backends.clone();
        let scope = scope.clone();
        let ledger = ledger.clone();
        async move {
            timeout(SCENARIO_TIME, async {
                let docker = backends
                    .docker
                    .client(Os::Windows)
                    .await
                    .expect("a Windows Docker daemon is unavailable; check DOCKER_HOST");
                print_daemon_info(&docker).await;
                let image = windows_image();
                assert!(
                    image_id(&docker, &image)
                        .await
                        .expect("image inspection failed")
                        .is_some(),
                    "PANORAMIC_TEST_WINDOWS_IMAGE '{image}' is not local; load it first, these tests never pull"
                );
                let participants: Vec<_> = participants.iter().map(String::as_str).collect();
                let prepared = backends
                    .prepare_windows_scope(&scope, &participants, courier_bytes().into())
                    .await
                    .unwrap();
                scenario(
                    TestScope {
                        scope: prepared,
                        ledger,
                        id: scope.id(),
                        docker,
                    },
                    tasks,
                )
                .await;
            })
            .await
            .expect("the daemon scenario should finish within its time limit");
        }
    })
    .await;

    let mut problems = Vec::new();
    if outcome.is_err() {
        print_container_logs(&backends, &scope.id()).await;
    }
    if tasks.stop_all(Instant::now() + TASK_STOP_TIME).await != 0 {
        problems.push("observation tasks did not stop before reclaim".into());
    }
    let (reclaimed, issues) = ledger.reclaim(Instant::now() + RECLAIM_TIME).await;
    for entry in &reclaimed {
        if entry.result != ReclaimResult::Reclaimed {
            problems.push(format!("unexpected reclaim: {entry:?}"));
        }
    }
    problems.extend(issues.iter().map(|issue| issue.message.clone()));
    let resources = timeout(RECLAIM_TIME, async {
        let docker = backends
            .docker
            .client(Os::Windows)
            .await
            .map_err(|error| error.to_string())?;
        remaining_resources(&docker, &scope.id())
            .await
            .map_err(|error| format!("{error:#}"))
    })
    .await;
    match resources {
        Ok(Ok(remaining)) => problems.extend(remaining),
        Ok(Err(error)) => problems.push(format!("cleanup verification failed: {error}")),
        Err(_) => problems.push("cleanup verification exceeded its time limit".into()),
    }
    let problems = problems.join("\n");
    if let Err(error) = outcome {
        if !problems.is_empty() {
            eprintln!("Cleanup problems for {}:\n{problems}", scope.id());
        }
        std::panic::resume_unwind(error.into_panic());
    }
    assert!(problems.is_empty(), "Cleanup problems for {}:\n{problems}", scope.id());
}

/// Prints the console output of the scope's containers, which says why a script failed.
async fn print_container_logs(backends: &RunBackends, id: &str) {
    let _ = timeout(Duration::from_secs(30), async {
        let Ok(docker) = backends.docker.client(Os::Windows).await else {
            return;
        };
        let filters = HashMap::from([("label", vec![format!("airlock-isolation-group={id}")])]);
        let Ok(containers) = docker
            .list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
        else {
            return;
        };
        for name in containers
            .into_iter()
            .flat_map(|container| container.names.unwrap_or_default())
        {
            eprintln!("Console output of {name}:");
            let options = LogsOptionsBuilder::default()
                .stdout(true)
                .stderr(true)
                .tail("100")
                .build();
            let mut logs = docker.logs(name.trim_start_matches('/'), Some(options));
            while let Some(chunk) = logs.next().await {
                match chunk {
                    Ok(chunk) => eprint!("{chunk}"),
                    Err(error) => {
                        eprintln!("(logs failed: {error})");
                        break;
                    }
                }
            }
        }
    })
    .await;
}

/// Builds a recipe that runs `script` with `pwsh` after the shared prelude.
fn recipe(script: &str) -> ContainerRecipe {
    ContainerRecipe {
        image: IMAGE_ROLE,
        entrypoint: Some(
            ["pwsh", "-NoProfile", "-NonInteractive", "-File", MAIN_SCRIPT]
                .map(String::from)
                .to_vec(),
        ),
        command: None,
        env: Default::default(),
        files: vec![RecipeFile {
            path: MAIN_SCRIPT.into(),
            mode: 0o644,
            contents: format!("{PRELUDE}{script}\n").into_bytes(),
        }],
        mounts: vec![],
        output_line_bytes: None,
    }
}

fn with_file(mut recipe: ContainerRecipe, path: &str, contents: impl Into<Vec<u8>>) -> ContainerRecipe {
    recipe.files.push(RecipeFile {
        path: path.into(),
        mode: 0o644,
        contents: contents.into(),
    });
    recipe
}

/// Writes bytes as a PowerShell `byte[]` expression, so scripts can produce exact binary content.
fn ps_bytes(bytes: &[u8]) -> String {
    let bytes: Vec<_> = bytes.iter().map(u8::to_string).collect();
    format!("[byte[]]({})", bytes.join(","))
}

async fn start_participant(
    scope: &TestScope, name: &str, recipe: &ContainerRecipe, specs: &'static [ProcessSpec],
) -> Arc<WindowsParticipant> {
    let participant = Arc::new(scope.participant(name, recipe, specs));
    participant.create().await.unwrap();
    participant.start().await.unwrap();
    participant
}

fn command(argv: &[&str]) -> ExecCommand {
    ExecCommand {
        argv: argv.iter().map(|arg| (*arg).into()).collect(),
        env: vec![],
        stdin: None,
    }
}

/// A `cmd` command line. `/S` makes `cmd` strip exactly the outer quotes Docker adds.
fn cmd(script: &str) -> ExecCommand {
    command(&["cmd", "/S", "/C", script])
}

fn pwsh_file(path: &str, args: &[&str]) -> ExecCommand {
    let mut argv = vec!["pwsh", "-NoProfile", "-NonInteractive", "-File", path];
    argv.extend(args);
    command(&argv)
}

/// Polls until `check` finds a value, reporting the last error if it never does.
async fn wait_until<T, F, Fut>(what: &str, limit: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>, String>>,
{
    let mut last = String::from("nothing observed");
    let found = timeout(limit, async {
        loop {
            match check().await {
                Ok(Some(value)) => return value,
                Ok(None) => {}
                Err(error) => last = error,
            }
            sleep(POLL_TIME).await;
        }
    })
    .await;
    // `last` is only read after the polling future is dropped.
    found.unwrap_or_else(|_| panic!("{what} did not happen within {limit:?}; last observation: {last}"))
}

/// Waits until a file holds exactly `expected`, tolerating reads that race the writer.
async fn wait_for_file(primitives: &dyn Primitives, path: &str, expected: &[u8]) {
    wait_until(
        &format!("'{path}' holding {} exact bytes", expected.len()),
        WAIT_TIME,
        || async {
            match primitives.read_file(path).await {
                Ok(Some(bytes)) if bytes == expected => Ok(Some(())),
                Ok(Some(bytes)) => Err(format!(
                    "{} bytes, starting {:?}",
                    bytes.len(),
                    &bytes[..bytes.len().min(32)]
                )),
                Ok(None) => Err("missing".into()),
                Err(error) => Err(error.to_string()),
            }
        },
    )
    .await;
}

async fn wait_for_signal(primitives: &dyn Primitives, participant: &str, name: &str) {
    wait_for_file(
        primitives,
        &format!(r"C:\airlock\{participant}\shared\{name}"),
        name.as_bytes(),
    )
    .await;
}

/// Releases a script waiting in `Wait-ForFile`.
async fn release(primitives: &dyn Primitives, participant: &str, name: &str) {
    let output = primitives
        .exec(
            cmd(&format!(r"type nul > C:\airlock\{participant}\shared\{name}")),
            EXEC_TIME,
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0, "{output:?}");
}

async fn wait_for_exit(participant: &WindowsParticipant) {
    timeout(WAIT_TIME, participant.driver.wait_exit())
        .await
        .expect("the participant should stop")
        .unwrap();
}

async fn observe(
    participant: &Arc<WindowsParticipant>, tasks: &CaseTasks, dir: &std::path::Path,
) -> ParticipantObservation {
    let dir = dir.join(&participant.name);
    std::fs::create_dir_all(&dir).unwrap();
    participant.clone().observe(tasks, &dir).await
}

async fn wait_for_capture(output: &ProcessOutput) {
    timeout(WAIT_TIME, async {
        while output.current() == Capture::Following {
            sleep(POLL_TIME).await;
        }
    })
    .await
    .expect("output capture must settle after exit");
    assert_eq!(output.current(), Capture::Complete);
}

async fn wait_for_host_file(path: &std::path::Path, expected: &[u8]) {
    wait_until(
        &format!("'{}' holding the expected bytes", path.display()),
        WAIT_TIME,
        || async {
            let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
            if bytes == expected {
                Ok(Some(()))
            } else {
                Err(format!("{} bytes, expected {}", bytes.len(), expected.len()))
            }
        },
    )
    .await;
}

fn address(kind: EndpointKind, host: &str, port: u16) -> ResolvedAddress {
    ResolvedAddress {
        kind,
        url: format!(
            "{}://{host}:{port}",
            if kind == EndpointKind::Udp { "udp" } else { "tcp" }
        ),
        host: Some(host.into()),
        port: Some(port),
        path: None,
    }
}

fn pipe_address(name: &str) -> ResolvedAddress {
    let path = format!(r"\\.\pipe\{name}");
    ResolvedAddress {
        kind: EndpointKind::NamedPipe,
        url: format!("npipe://{name}"),
        host: None,
        port: None,
        path: Some(path),
    }
}

fn limits() -> AttemptLimits {
    AttemptLimits::new(Duration::from_secs(30))
}

const SERVE_TCP: &str = r"
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, 9000)
$listener.Start()
Signal 'ready'
while ($true) { $listener.AcceptTcpClient().Dispose() }";

const RESOLVE_SCRIPT: &str = r"
$addresses = [System.Net.Dns]::GetHostAddresses($args[0]) | Where-Object { $_.AddressFamily -eq 'InterNetwork' }
[Console]::Out.Write(($addresses | ForEach-Object { $_.ToString() }) -join ',')";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn a_windows_scope_is_one_nat_network_without_a_volume_and_participants_reach_each_other_by_name() {
    in_scope(&["srv", "cli"], |scope, _tasks| async move {
        let filters = HashMap::from([("label", vec![format!("airlock-isolation-group={}", scope.id)])]);
        let networks = scope
            .docker
            .list_networks(Some(ListNetworksOptionsBuilder::default().filters(&filters).build()))
            .await
            .unwrap();
        assert_eq!(networks.len(), 1, "{networks:?}");
        assert_eq!(networks[0].driver.as_deref(), Some("nat"));
        let volumes = scope
            .docker
            .list_volumes(Some(ListVolumesOptionsBuilder::default().filters(&filters).build()))
            .await
            .unwrap()
            .volumes
            .unwrap_or_default();
        assert!(volumes.is_empty(), "{volumes:?}");
        let containers = scope
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .unwrap();
        assert!(
            containers.is_empty(),
            "preparing a scope created containers: {containers:?}"
        );

        let srv = scope.participant(
            "srv",
            &with_file(recipe(SERVE_TCP), r"C:\test\resolve.ps1", RESOLVE_SCRIPT),
            &[],
        );
        let cli = scope.participant(
            "cli",
            &with_file(recipe(IDLE), r"C:\test\resolve.ps1", RESOLVE_SCRIPT),
            &[],
        );
        let mut ips = Vec::new();
        for participant in [&srv, &cli] {
            let facts = participant.create().await.unwrap();
            let id = &facts.iter().find(|(fact, _)| *fact == LateFact::ContainerId).unwrap().1;
            assert_eq!(id.len(), 64);
            assert!(id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            let facts = participant.start().await.unwrap();
            let ip = &facts.iter().find(|(fact, _)| *fact == LateFact::Ip).unwrap().1;
            ip.parse::<Ipv4Addr>().unwrap();
            ips.push(ip.clone());
        }
        let (srv_primitives, cli_primitives): (&dyn Primitives, &dyn Primitives) = (&srv, &cli);
        wait_for_signal(srv_primitives, "srv", "ready").await;
        wait_for_signal(cli_primitives, "cli", "ready").await;

        let resolved = cli_primitives
            .exec(pwsh_file(r"C:\test\resolve.ps1", &["srv"]), EXEC_TIME)
            .await
            .unwrap();
        assert_eq!(resolved.exit_code, 0, "{resolved:?}");
        assert!(
            String::from_utf8_lossy(&resolved.stdout)
                .split(',')
                .any(|ip| ip == ips[0]),
            "'srv' should resolve to {} from 'cli': {resolved:?}",
            ips[0]
        );
        let resolved = srv_primitives
            .exec(pwsh_file(r"C:\test\resolve.ps1", &["cli"]), EXEC_TIME)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&resolved.stdout)
                .split(',')
                .any(|ip| ip == ips[1]),
            "'cli' should resolve to {} from 'srv': {resolved:?}",
            ips[1]
        );
        let answer = cli_primitives
            .probe(&address(EndpointKind::Tcp, "srv", 9000), &limits())
            .await
            .unwrap();
        assert_eq!(answer, ProbeAnswer::Listening);
    })
    .await;
}

const FILE_SCRIPT: &str = r#"
[IO.File]::WriteAllText("$env:PANORAMIC_LOG_DIR\started.txt", 'started')
Signal 'ready'
Wait-ForFile 'finish'
[IO.File]::WriteAllBytes("$env:PANORAMIC_LOG_DIR\late.bin", LATE)"#;

const LATE: &[u8] = b"late\r\n\x00\xff";
const DATA_PATH: &str = r"C:\test\data.bin";

fn data() -> Vec<u8> {
    let mut data: Vec<_> = (0..=255).collect();
    data.extend_from_slice(b"\r\n\x00");
    data
}

async fn check_files(primitives: &dyn Primitives, courier: &[u8], state: &str) {
    let data = data();
    let status = primitives
        .stat(COURIER_PATH)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{state}: no courier"));
    assert_eq!(
        (status.kind, status.size),
        (FileKind::File, courier.len() as u64),
        "{state}"
    );
    assert_eq!(
        primitives.read_file(COURIER_PATH).await.unwrap().as_deref(),
        Some(courier),
        "{state}"
    );
    let status = primitives
        .stat(DATA_PATH)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{state}: no data file"));
    assert_eq!(
        (status.kind, status.size),
        (FileKind::File, data.len() as u64),
        "{state}"
    );
    assert_eq!(
        primitives.read_file(DATA_PATH).await.unwrap().as_deref(),
        Some(data.as_slice()),
        "{state}"
    );
    for directory in [
        r"C:\test",
        r"C:\airlock\p\exit",
        r"C:\airlock\p\log",
        r"C:\airlock\p\shared",
    ] {
        let status = primitives
            .stat(directory)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{state}: no {directory}"));
        assert_eq!(status.kind, FileKind::Directory, "{state}: {directory}");
    }
    for missing in [
        r"C:\test\missing.txt",
        r"C:\test\missing\deeper.txt",
        r"C:\airlock\p\exit\missing",
    ] {
        assert_eq!(primitives.read_file(missing).await.unwrap(), None, "{state}: {missing}");
        assert_eq!(primitives.stat(missing).await.unwrap(), None, "{state}: {missing}");
    }
    let error = primitives.read_file(r"C:\airlock\p\exit").await.unwrap_err();
    let HandleError::Harness { message } = error else {
        panic!("{state}: reading a directory should be a harness error")
    };
    assert!(
        message.contains("participant 'p'") && message.contains(r"C:\airlock\p\exit"),
        "{state}: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn files_and_directories_are_read_and_stated_when_created_running_and_stopped() {
    in_scope(&["p"], |scope, _tasks| async move {
        let courier = courier_bytes();
        let script = FILE_SCRIPT.replace("LATE", &ps_bytes(LATE));
        let participant = scope.participant("p", &with_file(recipe(&script), DATA_PATH, data()), &[]);
        let primitives: &dyn Primitives = &participant;

        participant.create().await.unwrap();
        check_files(primitives, &courier, "created").await;
        assert_eq!(
            primitives.read_file(r"C:\airlock\p\log\started.txt").await.unwrap(),
            None
        );

        participant.start().await.unwrap();
        wait_for_signal(primitives, "p", "ready").await;
        check_files(primitives, &courier, "running").await;
        wait_for_file(primitives, r"C:\airlock\p\log\started.txt", b"started").await;
        assert_eq!(primitives.read_file(r"C:\airlock\p\log\late.bin").await.unwrap(), None);

        release(primitives, "p", "finish").await;
        wait_for_exit(&participant).await;
        check_files(primitives, &courier, "stopped").await;
        assert_eq!(
            primitives
                .read_file(r"C:\airlock\p\log\started.txt")
                .await
                .unwrap()
                .as_deref(),
            Some(b"started".as_slice())
        );
        assert_eq!(
            primitives
                .read_file(r"C:\airlock\p\log\late.bin")
                .await
                .unwrap()
                .as_deref(),
            Some(LATE)
        );
    })
    .await;
}

const BINARY_SCRIPT: &str = r"
$bytes = [byte[]](0..255)
$out = [Console]::OpenStandardOutput()
$out.Write($bytes, 0, $bytes.Length)
$out.Flush()
$err = [Console]::OpenStandardError()
$err.Write(ERR, 0, ERR_LEN)
$err.Flush()
exit 7";

const STDIN_SCRIPT: &str = r"
param([int]$Length)
$inputStream = [Console]::OpenStandardInput()
$bytes = [byte[]]::new($Length)
$offset = 0
while ($offset -lt $Length) {
    $read = $inputStream.Read($bytes, $offset, $Length - $offset)
    if ($read -eq 0) { throw 'stdin ended before the declared length' }
    $offset += $read
}
$outputStream = [Console]::OpenStandardOutput()
$prefix = [System.Text.Encoding]::ASCII.GetBytes('got:')
$outputStream.Write($prefix, 0, $prefix.Length)
$outputStream.Write($bytes, 0, $bytes.Length)
$outputStream.Flush()
exit 5";

const BINARY_ERR: &[u8] = b"\xff\r\n\x00e";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn exec_returns_separate_streams_environment_and_exit_codes_as_data() {
    in_scope(&["p"], |scope, _tasks| async move {
        let binary = BINARY_SCRIPT
            .replace("ERR_LEN", &BINARY_ERR.len().to_string())
            .replace("ERR", &ps_bytes(BINARY_ERR));
        let participant = start_participant(
            &scope,
            "p",
            &with_file(recipe(IDLE), r"C:\test\binary.ps1", binary),
            &[],
        )
        .await;
        let primitives: &dyn Primitives = participant.as_ref();
        wait_for_signal(primitives, "p", "ready").await;

        let mut streams = cmd("echo %KEY%& 1>&2 echo err& exit /b 3");
        streams.env.push(("KEY".into(), "value".into()));
        assert_eq!(
            primitives.exec(streams, EXEC_TIME).await.unwrap(),
            ExecOutput {
                stdout: b"value\r\n".to_vec(),
                stderr: b"err\r\n".to_vec(),
                exit_code: 3,
            }
        );
        assert_eq!(
            primitives.exec(cmd("exit /b 0"), EXEC_TIME).await.unwrap(),
            ExecOutput {
                stdout: vec![],
                stderr: vec![],
                exit_code: 0,
            }
        );
        let environment = primitives
            .exec(
                cmd("echo %PANORAMIC_PARTICIPANT%& echo %PANORAMIC_EXIT_DIR%& echo %PANORAMIC_LOG_DIR%"),
                EXEC_TIME,
            )
            .await
            .unwrap();
        assert_eq!(
            environment.stdout, b"p\r\nC:\\airlock\\p\\exit\r\nC:\\airlock\\p\\log\r\n",
            "{environment:?}"
        );
        let expected: Vec<u8> = (0..=255).collect();
        let binary = primitives
            .exec(pwsh_file(r"C:\test\binary.ps1", &[]), EXEC_TIME)
            .await
            .unwrap();
        assert!(
            binary.stdout == expected,
            "binary stdout differs: {} bytes",
            binary.stdout.len()
        );
        assert_eq!(binary.stderr, BINARY_ERR);
        assert_eq!(binary.exit_code, 7);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn exec_stdin_delivers_length_bounded_payloads_and_empty_input() {
    in_scope(&["p"], |scope, _tasks| async move {
        let participant = start_participant(
            &scope,
            "p",
            &with_file(recipe(IDLE), r"C:\test\stdin.ps1", STDIN_SCRIPT),
            &[],
        )
        .await;
        let primitives: &dyn Primitives = participant.as_ref();
        wait_for_signal(primitives, "p", "ready").await;
        for (stdin, expected) in [
            (b"payload\r\nsecond".as_slice(), b"got:payload\r\nsecond".as_slice()),
            (b"".as_slice(), b"got:".as_slice()),
            (b"\0\xff\r\n".as_slice(), b"got:\0\xff\r\n".as_slice()),
        ] {
            let mut exec = pwsh_file(r"C:\test\stdin.ps1", &[&stdin.len().to_string()]);
            exec.stdin = Some(stdin.to_vec());
            let output = primitives.exec(exec, EXEC_TIME).await.unwrap_or_else(|error| {
                panic!("stdin {stdin:?} did not reach its declared length over the engine connection: {error}")
            });
            assert_eq!(
                output,
                ExecOutput {
                    stdout: expected.to_vec(),
                    stderr: vec![],
                    exit_code: 5,
                }
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn exec_in_a_stopped_windows_container_is_a_harness_error() {
    in_scope(&["p"], |scope, _tasks| async move {
        let participant = start_participant(&scope, "p", &recipe("exit 0"), &[]).await;
        wait_for_exit(&participant).await;
        let error = participant.exec(cmd("exit /b 0"), EXEC_TIME).await.unwrap_err();
        let HandleError::Harness { message } = error else {
            panic!("expected a harness error")
        };
        assert!(message.contains("participant 'p'"), "{message}");
    })
    .await;
}

const COURIER_SERVER: &str = r#"
$log = $env:PANORAMIC_LOG_DIR
$tcp = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, 9000)
$tcp.Start()
$udp = [System.Net.Sockets.UdpClient]::new(9001)
$pipe = [System.IO.Pipes.NamedPipeServerStream]::new('panoramic-test', [System.IO.Pipes.PipeDirection]::InOut, 1, [System.IO.Pipes.PipeTransmissionMode]::Byte, [System.IO.Pipes.PipeOptions]::Asynchronous)
$tcpAccept = $tcp.AcceptTcpClientAsync()
$udpReceive = $udp.ReceiveAsync()
$pipeConnect = $pipe.WaitForConnectionAsync()
Signal 'ready'
while ($true) {
    $pending = [System.Threading.Tasks.Task[]]@($tcpAccept, $udpReceive, $pipeConnect)
    [void][System.Threading.Tasks.Task]::WaitAny($pending)
    if ($tcpAccept.IsCompleted) {
        $client = $tcpAccept.Result
        $buffer = [System.IO.MemoryStream]::new()
        $client.GetStream().CopyTo($buffer)
        $client.Dispose()
        if ($buffer.Length -gt 0) { [IO.File]::WriteAllBytes("$log\tcp", $buffer.ToArray()) }
        $tcpAccept = $tcp.AcceptTcpClientAsync()
    }
    if ($udpReceive.IsCompleted) {
        [IO.File]::WriteAllBytes("$log\udp", $udpReceive.Result.Buffer)
        $udpReceive = $udp.ReceiveAsync()
    }
    if ($pipeConnect.IsCompleted) {
        $buffer = [System.IO.MemoryStream]::new()
        $pipe.CopyTo($buffer)
        if ($buffer.Length -gt 0) { [IO.File]::WriteAllBytes("$log\pipe", $buffer.ToArray()) }
        $pipe.Disconnect()
        $pipeConnect = $pipe.WaitForConnectionAsync()
    }
}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn the_courier_probes_and_sends_over_tcp_udp_and_a_named_pipe_inside_its_container() {
    in_scope(&["srv"], |scope, _tasks| async move {
        let participant = start_participant(&scope, "srv", &recipe(COURIER_SERVER), &[]).await;
        let primitives: &dyn Primitives = participant.as_ref();
        wait_for_signal(primitives, "srv", "ready").await;

        let tcp = address(EndpointKind::Tcp, "127.0.0.1", 9000);
        let udp = address(EndpointKind::Udp, "127.0.0.1", 9001);
        let pipe = pipe_address("panoramic-test");
        assert_eq!(primitives.probe(&tcp, &limits()).await.unwrap(), ProbeAnswer::Listening);
        assert_eq!(
            primitives.probe(&udp, &limits()).await.unwrap(),
            ProbeAnswer::Addressable
        );
        assert_eq!(
            primitives.probe(&pipe, &limits()).await.unwrap(),
            ProbeAnswer::Listening
        );
        for closed in [
            address(EndpointKind::Tcp, "127.0.0.1", 9999),
            pipe_address("panoramic-missing"),
        ] {
            let answer = primitives.probe(&closed, &limits()).await.unwrap();
            assert!(
                matches!(answer, ProbeAnswer::NotListening { .. }),
                "{closed:?}: {answer:?}"
            );
        }

        for (name, address) in [("tcp", &tcp), ("udp", &udp), ("pipe", &pipe)] {
            let payload = format!("{name} payload\r\n\0\u{e9}").into_bytes();
            primitives.send(address, &payload, &limits()).await.unwrap();
            wait_for_file(primitives, &format!(r"C:\airlock\srv\log\{name}"), &payload).await;
        }
        let error = primitives
            .send(&address(EndpointKind::Tcp, "127.0.0.1", 9999), b"payload", &limits())
            .await
            .unwrap_err();
        assert!(matches!(error, HandleError::Product { .. }), "{error:?}");
    })
    .await;
}

const BODY_LEN: usize = 1024 * 1024 + 12_345;

const HTTP_SERVER: &str = r#"
$length = BODY_LEN
$pattern = [byte[]](0..255)
$body = [byte[]]::new($length)
for ($offset = 0; $offset -lt $length; $offset += 256) {
    [Array]::Copy($pattern, 0, $body, $offset, [Math]::Min(256, $length - $offset))
}
$missing = [Text.Encoding]::ASCII.GetBytes('missing')
function Respond($stream, $status, [byte[]]$payload) {
    $header = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 $status`r`nContent-Type: application/octet-stream`r`nContent-Length: $($payload.Length)`r`nConnection: close`r`n`r`n")
    $stream.Write($header, 0, $header.Length)
    $stream.Write($payload, 0, $payload.Length)
    $stream.Flush()
}
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, 8080)
$listener.Start()
Signal 'ready'
while ($true) {
    $client = $listener.AcceptTcpClient()
    try {
        $stream = $client.GetStream()
        $request = ''
        while (-not $request.EndsWith("`r`n`r`n")) {
            $next = $stream.ReadByte()
            if ($next -lt 0) { break }
            $request += [char]$next
        }
        if ($request.StartsWith('GET /big ')) { Respond $stream '200 OK' $body }
        elseif ($request.StartsWith('GET ')) { Respond $stream '404 Not Found' $missing }
    } catch { } finally { $client.Dispose() }
}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn the_courier_fetches_an_http_body_larger_than_one_mebibyte_from_a_container_server() {
    in_scope(&["srv"], |scope, _tasks| async move {
        let script = HTTP_SERVER.replace("BODY_LEN", &BODY_LEN.to_string());
        let participant = start_participant(&scope, "srv", &recipe(&script), &[]).await;
        let primitives: &dyn Primitives = participant.as_ref();
        wait_for_signal(primitives, "srv", "ready").await;

        let server = address(EndpointKind::Http, "127.0.0.1", 8080);
        let response = primitives.http(&server, "/big", false, &limits()).await.unwrap();
        let expected: Vec<u8> = (0..BODY_LEN).map(|index| (index % 256) as u8).collect();
        assert_eq!(response.status, 200);
        assert!(
            response.body == expected,
            "the body differs: {} bytes, expected {}",
            response.body.len(),
            expected.len()
        );
        let response = primitives.http(&server, "/big", true, &limits()).await.unwrap();
        assert_eq!((response.status, response.body.len()), (200, 0));
        let response = primitives.http(&server, "/other", false, &limits()).await.unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (404, b"missing".as_slice())
        );
        let error = primitives
            .http(
                &address(EndpointKind::Http, "127.0.0.1", 8081),
                "/big",
                false,
                &limits(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, HandleError::Product { .. }), "{error:?}");
    })
    .await;
}

const RECORD_SCRIPT: &str = r#"
[IO.File]::WriteAllText("$env:PANORAMIC_EXIT_DIR\svc", "3221225786 0 1700000000.1234567`r`n")
[IO.File]::WriteAllText("$env:PANORAMIC_EXIT_DIR\wide", "4294967295 0 1700000000.6000000`r`n7 0 1700000001.0000000`r`n")
Signal 'ready'
Wait-ForFile 'finish'"#;

fn exited(code: i32, recorded_at: Option<std::time::SystemTime>) -> ExitObservation {
    ExitObservation::Exited(ExitStatus {
        code: Some(code),
        signal: None,
        reason: None,
        recorded_at,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn crlf_exit_records_with_32_bit_codes_are_read_while_the_container_runs() {
    in_scope(&["p"], |scope, tasks| async move {
        let participant = start_participant(&scope, "p", &recipe(RECORD_SCRIPT), RECORD_SPECS).await;
        let primitives: &dyn Primitives = participant.as_ref();
        wait_for_signal(primitives, "p", "ready").await;
        let dir = tempfile::tempdir().unwrap();
        let observation = observe(&participant, &tasks, dir.path()).await;

        let svc = timeout(WAIT_TIME, observation.process("svc").unwrap().settled())
            .await
            .unwrap();
        assert_eq!(
            svc,
            exited(
                0xC000_013A_u32 as i32,
                Some(UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700))
            )
        );
        let wide = timeout(WAIT_TIME, observation.process("wide").unwrap().settled())
            .await
            .unwrap();
        assert_eq!(
            wide,
            exited(-1, Some(UNIX_EPOCH + Duration::new(1_700_000_000, 600_000_000)))
        );
        assert_eq!(observation.exit(), ParticipantExit::Running);
        assert_eq!(observation.process("main").unwrap().current(), ExitObservation::Running);

        release(primitives, "p", "finish").await;
        let main = timeout(WAIT_TIME, observation.process("main").unwrap().settled())
            .await
            .unwrap();
        assert_eq!(main, exited(0, None));
        assert_eq!(observation.process("svc").unwrap().current(), svc);
    })
    .await;
}

const FIRST: &[u8] = b"first\r\n\xff\x00\x80 line\r\n";
const SECOND: &[u8] = b"second\n\x01\x02\r";
const THIRD: &[u8] = b"last without newline\xfe";

const FOLLOW_SCRIPT: &str = r#"
$out = "$env:PANORAMIC_LOG_DIR\output"
Append $out FIRST
Signal 'ready'
Wait-ForFile 'continue'
Append $out SECOND
Wait-ForFile 'finish'
Append $out THIRD"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn a_followed_output_file_keeps_its_exact_bytes_while_the_container_runs_and_after_it_stops() {
    in_scope(&["p"], |scope, tasks| async move {
        let script = FOLLOW_SCRIPT
            .replace("FIRST", &ps_bytes(FIRST))
            .replace("SECOND", &ps_bytes(SECOND))
            .replace("THIRD", &ps_bytes(THIRD));
        let participant = start_participant(&scope, "p", &recipe(&script), SPECS).await;
        let primitives: &dyn Primitives = participant.as_ref();
        let dir = tempfile::tempdir().unwrap();
        let observation = observe(&participant, &tasks, dir.path()).await;
        let output = &observation.outputs["svc"];
        let host_path = output.paths[0].clone();

        wait_for_host_file(&host_path, FIRST).await;
        release(primitives, "p", "continue").await;
        wait_for_host_file(&host_path, &[FIRST, SECOND].concat()).await;
        assert_eq!(output.current(), Capture::Following);
        assert_eq!(observation.exit(), ParticipantExit::Running);

        release(primitives, "p", "finish").await;
        wait_for_capture(output).await;
        assert_eq!(std::fs::read(&host_path).unwrap(), [FIRST, SECOND, THIRD].concat());
        wait_for_capture(&observation.outputs["main"]).await;
        assert_eq!(observation.process("main").unwrap().settled().await, exited(0, None));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn a_container_that_stops_without_an_exit_record_leaves_its_process_undetermined() {
    in_scope(&["p"], |scope, tasks| async move {
        let participant = start_participant(&scope, "p", &recipe("exit 0"), SPECS).await;
        let dir = tempfile::tempdir().unwrap();
        let observation = observe(&participant, &tasks, dir.path()).await;
        let main = timeout(WAIT_TIME, observation.process("main").unwrap().settled())
            .await
            .unwrap();
        assert_eq!(main, exited(0, None));
        let svc = timeout(WAIT_TIME, observation.process("svc").unwrap().settled())
            .await
            .unwrap();
        assert_eq!(
            svc,
            ExitObservation::Undetermined("participant 'p' stopped without an exit record for process 'svc'".into())
        );
    })
    .await;
}

const RETAIN_SCRIPT: &str = r#"
[Console]::Out.Write("out`n")
[Console]::Error.Write('err')
Append "$env:PANORAMIC_LOG_DIR\output" FILE_BYTES
Start-Sleep -Milliseconds $env:DELAY_MS"#;

const RETAINED: &[u8] = b"kept\r\n\xff\x00 tail";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn output_written_before_the_container_stops_is_retained_however_early_it_is_observed() {
    in_scope(&["fast", "slow", "late"], |scope, tasks| async move {
        let script = RETAIN_SCRIPT.replace("FILE_BYTES", &ps_bytes(RETAINED));
        let dir = tempfile::tempdir().unwrap();
        let mut observations = Vec::new();
        for (name, delay, observe_after_stop) in [("fast", "0", false), ("slow", "3000", false), ("late", "0", true)] {
            let mut recipe = recipe(&script);
            recipe.env.insert("DELAY_MS".into(), delay.into());
            let participant = start_participant(&scope, name, &recipe, SPECS).await;
            if observe_after_stop {
                wait_for_exit(&participant).await;
            }
            observations.push((name, observe(&participant, &tasks, dir.path()).await));
        }
        for (name, observation) in &observations {
            wait_for_capture(&observation.outputs["main"]).await;
            wait_for_capture(&observation.outputs["svc"]).await;
            assert_eq!(
                std::fs::read(observation.stdout_path.as_ref().unwrap()).unwrap(),
                b"out\n",
                "{name}"
            );
            assert_eq!(
                std::fs::read(observation.stderr_path.as_ref().unwrap()).unwrap(),
                b"err",
                "{name}"
            );
            assert_eq!(
                std::fs::read(&observation.outputs["svc"].paths[0]).unwrap(),
                RETAINED,
                "{name}"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Windows Docker daemon; run in the Windows integration job"]
async fn cancelling_a_case_reclaims_its_containers_and_network_within_the_reclaim_budget() {
    in_scope(&["a", "b"], |scope, _tasks| async move {
        let cancel = CancellationToken::new();
        let tasks = CaseTasks::new(&cancel);
        let dir = tempfile::tempdir().unwrap();
        let mut observations = Vec::new();
        for name in ["a", "b"] {
            let participant = start_participant(&scope, name, &recipe(IDLE), SPECS).await;
            wait_for_signal(participant.as_ref(), name, "ready").await;
            observations.push(observe(&participant, &tasks, dir.path()).await);
        }

        cancel.cancel();
        let started = Instant::now();
        assert_eq!(tasks.stop_all(started + TASK_STOP_TIME).await, 0);
        let (reclaimed, issues) = scope.ledger.reclaim(started + RECLAIM_TIME).await;
        assert!(started.elapsed() < RECLAIM_TIME, "reclaim took {:?}", started.elapsed());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            reclaimed
                .iter()
                .map(|entry| entry.participant.as_deref())
                .collect::<Vec<_>>(),
            [Some("b"), Some("a"), None]
        );
        assert!(
            reclaimed.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
            "{reclaimed:?}"
        );
        assert_eq!(
            reclaimed.last().unwrap().resource,
            format!("isolation_group:{}", scope.id)
        );
        assert_eq!(
            remaining_resources(&scope.docker, &scope.id).await.unwrap(),
            Vec::<String>::new()
        );
        drop(observations);
    })
    .await;
}
