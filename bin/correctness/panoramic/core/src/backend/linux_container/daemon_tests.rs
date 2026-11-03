use std::{
    collections::HashMap,
    future::Future,
    net::Ipv4Addr,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use airlock::{
    docker::{image_id, Docker},
    files::FileKind,
};
use bollard::{
    errors::Error,
    models::PortBinding,
    query_parameters::{ListContainersOptionsBuilder, ListNetworksOptionsBuilder, ListVolumesOptionsBuilder},
};
use tokio::time::{sleep, timeout, Instant};

use super::*;
use crate::{
    backend::COURIER_LINUX_BINARY,
    endpoint::{EndpointKind, EndpointOperation, RunnerHostRoute},
    handle::{probe_from_runner_host, send_from_runner_host},
    ledger,
    probe::ProbeAnswer,
    recipe::HostMount,
};

/// Test and fix-up image with BusyBox's `sh`, `syslogd`, `sleep`, `grep`, `cat`, `true`, `mkdir`, `chmod` and `nc`.
fn test_image() -> String {
    std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into())
}

pub(crate) fn host_ports(count: usize) -> Vec<u16> {
    // Stay above panoramic's runner-host pool (20000–29999) and below default ephemeral ranges
    // (Linux 32768+, macOS/Windows 49152+). The clock separates nextest's per-test processes.
    assert!(count <= 2700);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seed = nanos ^ u128::from(std::process::id());
    let base = 30000 + (seed % (2701 - count) as u128) as u16;
    (0..count).map(|offset| base + offset as u16).collect()
}

pub(crate) fn courier_bytes() -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap();
    let path = root.join(COURIER_LINUX_BINARY.local_default);
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "Cannot read {}: {error}; build it with make build-courier-linux",
            path.display()
        )
    })
}

async fn in_scope<F, S>(participants: &[&str], courier: Vec<u8>, scenario: S)
where
    F: Future<Output = ()> + Send + 'static,
    S: FnOnce(LinuxScope) -> F + Send + 'static,
{
    let backends = Arc::new(RunBackends::default());
    let scope = Arc::new(ScopeLayout {
        run: ledger::random_id(),
        index: 0,
    });
    let participants: Vec<_> = participants.iter().map(|name| (*name).to_owned()).collect();
    eprintln!("Daemon test scope: {}", scope.id());
    let outcome = tokio::spawn({
        let backends = backends.clone();
        let scope = scope.clone();
        async move {
            timeout(Duration::from_secs(60), async {
                let docker = backends
                    .docker
                    .client(Os::Linux)
                    .await
                    .expect("Docker daemon is unavailable; check DOCKER_HOST");
                let image = test_image();
                assert!(
                    image_id(&docker, &image)
                        .await
                        .expect("image inspection failed")
                        .is_some(),
                    "PANORAMIC_TEST_IMAGE '{image}' is not local; load it first, these tests never pull"
                );
                let participants: Vec<_> = participants.iter().map(String::as_str).collect();
                let prepared = backends
                    .prepare_linux_scope(&scope, &participants, &image, courier.into())
                    .await
                    .unwrap();
                scenario(prepared).await;
            })
            .await
            .expect("the daemon scenario should finish within a minute");
        }
    })
    .await;

    let mut problems = Vec::new();
    match timeout(Duration::from_secs(30), backends.remove_linux_scope(&scope)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => problems.push(format!("scope cleanup failed: {error:#}")),
        Err(_) => problems.push("scope cleanup exceeded 30 seconds".into()),
    }
    let resources = timeout(Duration::from_secs(30), async {
        let docker = backends
            .docker
            .client(Os::Linux)
            .await
            .map_err(|error| generic_error!(error))?;
        remaining_resources(&docker, &scope.id()).await
    })
    .await;
    match resources {
        Ok(Ok(remaining)) => problems.extend(remaining),
        Ok(Err(error)) => problems.push(format!("cleanup verification failed: {error:#}")),
        Err(_) => problems.push("cleanup verification exceeded 30 seconds".into()),
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

pub(crate) async fn remaining_resources(docker: &Docker, id: &str) -> Result<Vec<String>, GenericError> {
    let filters = HashMap::from([("label", vec![format!("airlock-isolation-group={id}")])]);
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await?
        .into_iter()
        .flat_map(|container| container.names.unwrap_or_default())
        .collect::<Vec<_>>();
    let volumes = docker
        .list_volumes(Some(ListVolumesOptionsBuilder::default().filters(&filters).build()))
        .await?
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|volume| volume.name)
        .collect::<Vec<_>>();
    let networks = docker
        .list_networks(Some(ListNetworksOptionsBuilder::default().filters(&filters).build()))
        .await?
        .into_iter()
        .map(|network| network.name.unwrap_or_else(|| "unnamed network".into()))
        .collect::<Vec<_>>();
    Ok(
        [("containers", containers), ("volumes", volumes), ("networks", networks)]
            .into_iter()
            .filter(|(_, names)| !names.is_empty())
            .map(|(kind, names)| format!("remaining {kind}: {names:?}"))
            .collect(),
    )
}

fn recipe(script: &str) -> ContainerRecipe {
    ContainerRecipe {
        image: super::super::FIXUP_IMAGE,
        entrypoint: Some(vec!["sh".into(), "-c".into()]),
        command: Some(vec![script.into()]),
        env: Default::default(),
        files: vec![],
        mounts: vec![],
    }
}

async fn start_participant(scope: &LinuxScope, name: &str, recipe: &ContainerRecipe) -> LinuxParticipant {
    let participant = scope.participant(name, &test_image(), recipe, &[]).unwrap();
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

async fn wait_for_file(primitives: &dyn Primitives, path: &str, expected: &[u8]) -> Vec<u8> {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(bytes) = primitives.read_file(path).await.unwrap() {
                if bytes.windows(expected.len()).any(|window| window == expected) {
                    return bytes;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("'{path}' did not contain the expected bytes within 10 seconds"))
}

fn socket_address(kind: EndpointKind, path: &str) -> ResolvedAddress {
    let scheme = match kind {
        EndpointKind::UnixDatagram => "unixgram",
        EndpointKind::UnixStream => "unix",
        _ => panic!("expected a Unix socket kind"),
    };
    ResolvedAddress {
        kind,
        url: format!("{scheme}://{path}"),
        path: Some(path.into()),
        host: None,
        port: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_participant_is_created_from_its_recipe_and_shares_the_host_pid_namespace() {
    let courier = courier_bytes();
    let courier_size = courier.len() as u64;
    in_scope(&["p"], courier.clone(), move |scope| async move {
        let mut recipe =
            recipe("echo \"$GREETING\" > /airlock/p/shared/env; echo $$ > /airlock/p/shared/pid; exec sleep 60");
        recipe.env.insert("GREETING".into(), "hello".into());
        recipe.files.push(RecipeFile {
            path: "/etc/panoramic-test/config".into(),
            mode: 0o600,
            contents: b"placed".to_vec(),
        });
        let participant = scope.participant("p", &test_image(), &recipe, &[]).unwrap();
        let id = participant.create().await.unwrap();
        assert_eq!(id.len(), 64);
        assert!(id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        let primitives: &dyn Primitives = &participant;
        assert_eq!(
            primitives.stat(COURIER_PATH).await.unwrap(),
            Some(FileStat {
                kind: FileKind::File,
                mode: 0o755,
                size: courier_size
            })
        );
        assert_eq!(
            primitives.read_file(COURIER_PATH).await.unwrap().as_deref(),
            Some(courier.as_slice())
        );
        assert_eq!(
            primitives.stat("/etc/panoramic-test/config").await.unwrap(),
            Some(FileStat {
                kind: FileKind::File,
                mode: 0o600,
                size: 6
            })
        );
        assert_eq!(
            primitives
                .read_file("/etc/panoramic-test/config")
                .await
                .unwrap()
                .as_deref(),
            Some(b"placed".as_slice())
        );
        participant
            .start()
            .await
            .unwrap()
            .expect("a running participant has an IP")
            .parse::<Ipv4Addr>()
            .unwrap();
        assert_eq!(
            wait_for_file(primitives, "/airlock/p/shared/env", b"hello\n").await,
            b"hello\n"
        );
        let pid = wait_for_file(primitives, "/airlock/p/shared/pid", b"\n").await;
        assert!(String::from_utf8(pid).unwrap().trim().parse::<u32>().unwrap() > 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn recipe_host_mounts_are_bound_read_only_or_read_write() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let mut recipe = recipe("exec sleep 60");
        recipe.mounts = vec![
            HostMount {
                host_path: "/etc".into(),
                container_path: "/host/etc".into(),
                read_only: true,
            },
            HostMount {
                host_path: "/tmp".into(),
                container_path: "/host/tmp".into(),
                read_only: false,
            },
        ];
        let participant = start_participant(&scope, "p", &recipe).await;
        let primitives: &dyn Primitives = &participant;
        let output = primitives
            .exec(
                command(&["grep", "-E", " /host/(etc|tmp) ", "/proc/mounts"]),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, 0);
        let mounts = String::from_utf8(output.stdout).unwrap();
        for (path, expected) in [("/host/etc", "ro"), ("/host/tmp", "rw")] {
            let fields = mounts
                .lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
                .find(|fields| fields.get(1) == Some(&path))
                .unwrap_or_else(|| panic!("missing mount {path}: {mounts}"));
            assert_eq!(fields[3].split(',').next(), Some(expected));
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn exec_passes_env_and_stdin_and_returns_both_streams_and_a_nonzero_exit_as_data() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = start_participant(&scope, "p", &recipe("exec sleep 60")).await;
        let primitives: &dyn Primitives = &participant;
        let mut command = command(&["sh", "-c", "cat; printf %s \"$KEY\" >&2; exit 3"]);
        command.env.push(("KEY".into(), "value".into()));
        command.stdin = Some(b"payload".to_vec());
        assert_eq!(
            primitives.exec(command, Duration::from_secs(10)).await.unwrap(),
            ExecOutput {
                stdout: b"payload".to_vec(),
                stderr: b"value".to_vec(),
                exit_code: 3,
            }
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn an_exec_that_outlasts_its_limit_is_abandoned_as_a_harness_error() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = start_participant(&scope, "p", &recipe("exec sleep 60")).await;
        let primitives: &dyn Primitives = &participant;
        let started = Instant::now();
        let error = primitives
            .exec(command(&["sleep", "30"]), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        let HandleError::Harness { message } = error else {
            panic!("expected a harness error")
        };
        assert!(
            message.contains("Abandoned") && message.contains("participant 'p'"),
            "{message}"
        );
        assert!(!message.contains("sleep") && !message.contains("30"), "{message}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn an_exec_that_closes_its_output_still_returns_its_exit_code() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = start_participant(&scope, "p", &recipe("exec sleep 60")).await;
        let primitives: &dyn Primitives = &participant;
        // Docker keeps the attached output stream open until the exec exits.
        let output = primitives
            .exec(
                command(&["sh", "-c", "exec >&- 2>&-; sleep 1; exit 4"]),
                Duration::from_secs(20),
            )
            .await
            .unwrap();
        assert_eq!(
            output,
            ExecOutput {
                stdout: vec![],
                stderr: vec![],
                exit_code: 4
            }
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn files_are_read_and_stated_after_the_participant_exits_and_exec_then_fails() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = start_participant(&scope, "p", &recipe("echo 3 > /airlock/p/exit/done; exit 3")).await;
        let primitives: &dyn Primitives = &participant;
        timeout(Duration::from_secs(10), async {
            loop {
                match primitives.exec(command(&["true"]), Duration::from_secs(1)).await {
                    Err(HandleError::Harness { message }) => {
                        assert!(message.contains("participant 'p'"), "{message}");
                        break;
                    }
                    Ok(_) => sleep(Duration::from_millis(50)).await,
                    Err(error) => panic!("expected an exec failure, got {error}"),
                }
            }
        })
        .await
        .expect("the participant should exit within 10 seconds");
        assert_eq!(
            primitives.read_file("/airlock/p/exit/done").await.unwrap().as_deref(),
            Some(b"3\n".as_slice())
        );
        let stat = primitives.stat("/airlock/p/exit/done").await.unwrap().unwrap();
        assert_eq!(stat.kind, FileKind::File);
        assert_eq!(stat.size, 2);
        assert_eq!(primitives.read_file("/missing").await.unwrap(), None);
        assert_eq!(primitives.stat("/missing").await.unwrap(), None);
        let error = primitives.read_file("/airlock/p/exit").await.unwrap_err();
        let HandleError::Harness { message } = error else {
            panic!("expected a harness error")
        };
        assert!(
            message.contains("participant 'p'") && message.contains("/airlock/p/exit"),
            "{message}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn the_courier_probes_and_sends_to_a_unix_datagram_socket_inside_its_participant() {
    in_scope(&["srv"], courier_bytes(), |scope| async move {
        let participant =
            start_participant(&scope, "srv", &recipe("exec syslogd -n -O /airlock/srv/log/messages")).await;
        let primitives: &dyn Primitives = &participant;
        let limits = AttemptLimits::new(Duration::from_secs(10));
        let address = socket_address(EndpointKind::UnixDatagram, "/dev/log");
        timeout(Duration::from_secs(10), async {
            loop {
                if primitives.probe(&address, &limits).await.unwrap() == ProbeAnswer::Listening {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("syslogd should listen within 10 seconds");
        primitives
            .send(&address, b"<14>courier-delivery-check", &limits)
            .await
            .unwrap();
        wait_for_file(primitives, "/airlock/srv/log/messages", b"courier-delivery-check").await;
        assert!(matches!(
            primitives
                .probe(
                    &socket_address(EndpointKind::UnixStream, "/airlock/srv/shared/missing.sock"),
                    &limits
                )
                .await
                .unwrap(),
            ProbeAnswer::NotListening { .. }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_courier_that_cannot_run_is_a_harness_error_saying_it_never_ran() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = start_participant(&scope, "p", &recipe("exec sleep 60")).await;
        let primitives: &dyn Primitives = &participant;
        // Removing execute permission makes the courier fail Docker's launch checks.
        let chmod = primitives
            .exec(command(&["chmod", "0644", COURIER_PATH]), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(chmod.exit_code, 0);
        let error = primitives
            .probe(
                &socket_address(EndpointKind::UnixStream, "/missing.sock"),
                &AttemptLimits::new(Duration::from_secs(10)),
            )
            .await
            .unwrap_err();
        let HandleError::Harness { message } = error else {
            panic!("expected a harness error")
        };
        assert!(
            message.contains("The courier never ran (exit 126)")
                || message.contains("The courier never ran (exit 127)"),
            "{message}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_container_reference_removes_its_participant_after_the_participant_is_dropped() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = scope
            .participant("p", &test_image(), &recipe("exec sleep 60"), &[])
            .unwrap();
        let reference = participant.container_ref();
        let id = participant.create().await.unwrap();
        participant.start().await.unwrap();
        drop(participant);
        let docker = airlock::docker::connect().unwrap();
        assert!(docker.inspect_container(&id, None).await.is_ok());
        reference
            .stop_and_remove(Duration::from_secs(1), Instant::now() + Duration::from_secs(15))
            .await
            .unwrap();
        assert!(matches!(
            docker.inspect_container(&id, None).await,
            Err(Error::DockerResponseServerError { status_code: 404, .. })
        ));
        reference
            .stop_and_remove(Duration::from_secs(1), Instant::now() + Duration::from_secs(15))
            .await
            .unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_participant_outside_the_prepared_scope_is_rejected_before_docker() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let error = scope
            .participant("other", &test_image(), &recipe("exec sleep 60"), &[])
            .err()
            .unwrap();
        assert!(error.to_string().contains("other"), "{error:#}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn the_runner_host_receives_an_http_response_through_a_published_port() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    in_scope(&["srv"], courier_bytes(), |scope| async move {
        let ports = host_ports(2);
        let ports = [
            RunnerHostPort {
                transport: Transport::Tcp,
                container_port: 9000,
                host_port: ports[0],
            },
            RunnerHostPort {
                transport: Transport::Udp,
                container_port: 9001,
                host_port: ports[1],
            },
        ];
        let recipe = recipe(
            r#"nc -lk -s 0.0.0.0 -p 9000 -e sh -c '
while IFS= read -r line; do
    printf "%s\n" "$line" >> /airlock/srv/log/http
    [ "$line" = "$(printf "\r")" ] && break
done
printf "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
' &
while ! grep -q ':2328 ' /proc/net/tcp; do sleep 0.05; done
echo ready > /airlock/srv/shared/ready
wait"#,
        );
        let participant = scope.participant("srv", &test_image(), &recipe, &ports).unwrap();
        assert_eq!(participant.ports, ports);
        let id = participant.create().await.unwrap();
        participant.start().await.unwrap();
        wait_for_file(&participant, "/airlock/srv/shared/ready", b"ready").await;
        let docker = airlock::docker::connect().unwrap();
        let config = docker.inspect_container(&id, None).await.unwrap().host_config.unwrap();
        assert_ne!(config.publish_all_ports, Some(true));
        assert_eq!(
            config.port_bindings,
            Some(
                ports
                    .iter()
                    .map(|port| (
                        format!("{}/{}", port.container_port, port.transport.name()),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".into()),
                            host_port: Some(port.host_port.to_string())
                        }]),
                    ))
                    .collect()
            )
        );
        timeout(Duration::from_secs(10), async {
            loop {
                let response = timeout(Duration::from_secs(1), async {
                    let mut stream = TcpStream::connect(("127.0.0.1", ports[0].host_port)).await?;
                    stream
                        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .await?;
                    let mut response = Vec::new();
                    stream.read_to_end(&mut response).await?;
                    Ok::<_, std::io::Error>(response)
                })
                .await;
                if let Ok(Ok(response)) = response {
                    assert_eq!(
                        response,
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                    );
                    break;
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("the published port should return the HTTP server's response");
        wait_for_file(&participant, "/airlock/srv/log/http", b"GET / HTTP/1.1").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_participant_whose_runner_host_port_is_held_fails_to_start_naming_its_ports() {
    in_scope(&["first", "second"], courier_bytes(), |scope| async move {
        let ports = host_ports(2);
        let ports = [
            RunnerHostPort { transport: Transport::Tcp, container_port: 9000, host_port: ports[0] },
            RunnerHostPort { transport: Transport::Udp, container_port: 9001, host_port: ports[1] },
        ];
        let recipe = recipe("exec sleep 60");
        let first = scope.participant("first", &test_image(), &recipe, &ports).unwrap();
        let second = scope.participant("second", &test_image(), &recipe, &ports).unwrap();
        first.create().await.unwrap();
        second.create().await.unwrap();
        first.start().await.unwrap();
        let error = second.start().await.unwrap_err();
        assert_eq!(error.to_string(), format!(
            "Failed to start participant 'second', which publishes runner-host ports 127.0.0.1:{}/tcp, 127.0.0.1:{}/udp; run one panoramic invocation per runner host and Docker daemon at a time", ports[0].host_port, ports[1].host_port));
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn the_courier_route_to_a_socket_of_an_exited_participant_is_a_harness_error() {
    in_scope(&["p"], courier_bytes(), |scope| async move {
        let participant = start_participant(&scope, "p", &recipe("exit 0")).await;
        timeout(Duration::from_secs(10), participant.driver.wait_exit())
            .await
            .unwrap()
            .unwrap();
        let route = RunnerHostRoute::Courier(socket_address(EndpointKind::UnixStream, "/run/server.sock"));
        let limits = AttemptLimits::new(Duration::from_secs(10));
        for error in [
            probe_from_runner_host(&route, &participant, &limits).await.unwrap_err(),
            send_from_runner_host(&route, &participant, b"payload", &limits)
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(error, HandleError::Harness { message } if message.contains("exec failed")));
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_probe_of_an_unlistened_published_port_is_not_listening() {
    use crate::{
        case::fixtures::{minimal_procedure, LOADER, PARTICIPANT_FIELDS, REGISTRIES},
        plan::Planner,
    };

    in_scope(&["subject"], courier_bytes(), |scope| async move {
        let yaml = PARTICIPANT_FIELDS
            .replace("subject: true", "subject: true, config: {env: {PORT: '9000'}, endpoints: {extra: {port: 9000}}}")
            .replace(
                minimal_procedure!(),
                "procedure: [{step: reach, endpoint: subject.api}, {step: reach, endpoint: subject.extra, operation: send}]",
            );
        let case = LOADER
            .load_content(&yaml, "probe", Path::new("cases/probe/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(REGISTRIES, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        let port = RunnerHostPort {
            transport: Transport::Tcp,
            container_port: 9000,
            host_port: host_ports(1)[0],
        };
        let participant = scope
            .participant("subject", &test_image(), &recipe("exec sleep 60"), &[port])
            .unwrap();
        participant.create().await.unwrap();
        participant.start().await.unwrap();
        let mut route = plan
            .runner_host_route("subject", "api", EndpointOperation::Probe)
            .unwrap()
            .clone();
        // Daemon tests use separate host ports because nextest runs each test in its own process.
        if let RunnerHostRoute::Direct(address) = &mut route {
            address.port = Some(port.host_port);
            address.url = format!("http://127.0.0.1:{}", port.host_port);
        }
        let answer = probe_from_runner_host(&route, &participant, &AttemptLimits::new(Duration::from_secs(10)))
            .await
            .unwrap();
        assert!(matches!(answer, ProbeAnswer::NotListening { .. }), "{answer:?}");
        let route = plan.runner_host_route("subject", "extra", EndpointOperation::Send).unwrap();
        let error = send_from_runner_host(route, &participant, b"payload", &AttemptLimits::new(Duration::from_secs(10))).await.unwrap_err();
        assert!(matches!(error, HandleError::Product { .. }), "{error:?}");
        assert!(plan.runner_host_ports("subject").is_empty());
    })
    .await;
}
