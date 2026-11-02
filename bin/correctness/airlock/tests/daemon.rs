use std::{
    collections::HashMap,
    future::Future,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use airlock::{
    docker::{connect, image_id, Docker},
    driver::{Driver, DriverConfig, ExitStatus, Protocol},
    exec::{ExecOutput, ExecRequest, OutputChunk},
    files::ContainerFile,
    group::IsolationGroup,
};
use bollard::{
    errors::Error,
    models::{ContainerCreateBody, HostConfig, PortBinding},
    query_parameters::{
        CreateContainerOptionsBuilder, ListContainersOptionsBuilder, ListNetworksOptionsBuilder,
        ListVolumesOptionsBuilder, RemoveContainerOptionsBuilder,
    },
};
use futures::{future::try_join_all, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::AsyncReadExt as _,
    net::TcpStream,
    time::{sleep, timeout},
};

fn test_image() -> String {
    std::env::var("AIRLOCK_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".to_string())
}

async fn daemon() -> Docker {
    let docker = connect().expect("failed to connect to Docker; check DOCKER_HOST");
    docker
        .ping()
        .await
        .expect("Docker daemon is unavailable; check DOCKER_HOST");
    let image = test_image();
    assert!(
        image_id(&docker, &image)
            .await
            .expect("image inspection failed; check DOCKER_HOST")
            .is_some(),
        "AIRLOCK_TEST_IMAGE '{image}' is not on the daemon; load it first, these tests never pull"
    );
    docker
}

fn unique_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let mut id = format!("t{}x{nanos:x}", std::process::id());
    id.truncate(63);
    id
}

fn host_ports(count: usize) -> Vec<u16> {
    // Stay above panoramic's runner-host pool (20000–29999) and below default ephemeral ranges
    // (Linux 32768+, macOS/Windows 49152+). The clock separates nextest's per-test processes.
    assert!(count <= 2700);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seed = nanos ^ u128::from(std::process::id());
    let base = 30000 + (seed % (2701 - count) as u128) as u16;
    (0..count).map(|offset| base + offset as u16).collect()
}

async fn read_from_host(port: u16) -> String {
    timeout(Duration::from_secs(20), async {
        loop {
            let attempt = timeout(Duration::from_secs(2), async {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
                let mut reply = String::new();
                stream.read_to_string(&mut reply).await?;
                Ok::<_, std::io::Error>(reply)
            })
            .await;
            // A VM port forwarder may accept and close before the container is listening.
            match attempt {
                Ok(Ok(reply)) if !reply.is_empty() => return reply,
                _ => sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no reply from 127.0.0.1:{port} within 20 seconds"))
}

async fn run_on_host_network(docker: &Docker, id: &str, image: &str, script: String) -> Result<i64, GenericError> {
    let name = format!("airlock-{id}-host");
    docker
        .create_container(
            Some(CreateContainerOptionsBuilder::default().name(&name).build()),
            ContainerCreateBody {
                image: Some(image.to_string()),
                entrypoint: Some(vec!["sh".to_string(), "-c".to_string(), script]),
                host_config: Some(HostConfig {
                    network_mode: Some("host".to_string()),
                    ..Default::default()
                }),
                labels: Some(HashMap::from([
                    ("created_by".to_string(), "airlock".to_string()),
                    ("airlock-isolation-group".to_string(), id.to_string()),
                ])),
                ..Default::default()
            },
        )
        .await?;
    docker.start_container(&name, None).await?;
    match docker.wait_container(&name, None).next().await {
        Some(Ok(response)) => Ok(response.status_code),
        Some(Err(Error::DockerContainerWaitError { code, .. })) => Ok(code),
        Some(Err(error)) => Err(error.into()),
        None => Err(generic_error!("Wait stream ended without an exit code for '{name}'")),
    }
}

async fn in_group(id: String, scenario: impl Future<Output = ()> + Send + 'static) {
    let outcome = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(60), scenario)
            .await
            .expect("the daemon scenario should finish within a minute");
    })
    .await;
    let mut problems = Vec::new();
    if let Err(error) = Driver::clean_related_resources(id.clone()).await {
        problems.push(format!("group cleanup failed: {error:#}"));
    }
    let resources = match connect() {
        Ok(docker) => group_resources(&docker, &id).await,
        Err(error) => Err(error),
    };
    match resources {
        Ok(resources) => {
            for (kind, remaining) in [
                ("containers", resources.containers),
                ("volumes", resources.volumes),
                ("networks", resources.networks),
            ] {
                if !remaining.is_empty() {
                    problems.push(format!("remaining {kind}: {remaining:?}"));
                }
            }
        }
        Err(error) => problems.push(format!("cleanup verification failed: {error:#}")),
    }
    let problems = problems.join("\n");
    if let Err(error) = outcome {
        if !problems.is_empty() {
            eprintln!("Cleanup problems for {id}:\n{problems}");
        }
        std::panic::resume_unwind(error.into_panic());
    }
    assert!(problems.is_empty(), "Cleanup problems for {id}:\n{problems}");
}

#[derive(Debug)]
struct GroupResources {
    containers: Vec<String>,
    volumes: Vec<String>,
    networks: Vec<String>,
}

async fn group_resources(docker: &Docker, id: &str) -> Result<GroupResources, GenericError> {
    let filters: HashMap<_, _> = [("label", vec![format!("airlock-isolation-group={id}")])]
        .into_iter()
        .collect();
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .error_context("Failed to list containers")?
        .into_iter()
        .flat_map(|container| container.names.unwrap_or_default())
        .map(|name| name.trim_start_matches('/').to_string())
        .collect();
    let volumes = docker
        .list_volumes(Some(ListVolumesOptionsBuilder::default().filters(&filters).build()))
        .await
        .error_context("Failed to list volumes")?
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|volume| volume.name)
        .collect();
    let networks = docker
        .list_networks(Some(ListNetworksOptionsBuilder::default().filters(&filters).build()))
        .await
        .error_context("Failed to list networks")?
        .into_iter()
        .map(|network| network.name.ok_or_else(|| generic_error!("Network has no name")))
        .collect::<Result<_, _>>()?;
    Ok(GroupResources {
        containers,
        volumes,
        networks,
    })
}

async fn start_exec_participant(docker: Docker, id: &str, script: &str) -> Driver {
    let image = test_image();
    let group = IsolationGroup::prepare(docker, id, &["subject"], &image).await.unwrap();
    let driver = group
        .driver(DriverConfig::from_image("subject", image).with_entrypoint(vec![
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ]))
        .unwrap();
    driver.create().await.unwrap();
    driver.start_container().await.unwrap();
    driver
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_group_creates_its_participants_concurrently_on_one_network_and_volume() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let participants = ["alpha", "bravo", "charlie"];
        let image = test_image();
        let group = IsolationGroup::prepare(docker.clone(), &id, &participants, &image)
            .await
            .unwrap();
        let drivers: Vec<_> = participants
            .iter()
            .map(|participant| {
                let script = format!(
                    "for p in alpha bravo charlie; do for d in exit log shared; do \
                 [ \"$(stat -c %a /airlock/$p/$d)\" = 777 ] || exit 1; done; done; \
                 echo ok > /airlock/{participant}/shared/ok"
                );
                group
                    .driver(
                        DriverConfig::from_image(*participant, image.clone()).with_entrypoint(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            script,
                        ]),
                    )
                    .unwrap()
            })
            .collect();

        try_join_all(drivers.iter().map(Driver::create)).await.unwrap();

        let mut resources = group_resources(&docker, &id).await.unwrap();
        let name = format!("airlock-{id}");
        assert_eq!(resources.networks, vec![name.clone()]);
        assert_eq!(resources.volumes, vec![name.clone()]);
        resources.containers.sort();
        assert_eq!(
            resources.containers,
            participants.map(|participant| format!("{name}-{participant}"))
        );

        for driver in &drivers {
            let details = driver.start_container().await.unwrap();
            let status = driver.wait_for_container_exit().await.unwrap();
            assert!(
                matches!(status, ExitStatus::Success),
                "{}: {status}",
                details.container_name()
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_participant_reaches_another_by_its_participant_name() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let names: Vec<String> = ["server", "client"].into_iter().map(String::from).collect();
        let image = test_image();
        let group = IsolationGroup::prepare(docker.clone(), &id, &names, &image)
            .await
            .unwrap();
        let server = group
            .driver(
                DriverConfig::from_image(names[0].clone(), image.clone()).with_entrypoint(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "nc -l -p 9000 -e echo reached".to_string(),
                ]),
            )
            .unwrap();
        let client = group
            .driver(DriverConfig::from_image(names[1].clone(), image).with_entrypoint(vec![
                "sh".to_string(),
                "-c".to_string(),
                r#"[ "$(hostname)" = client ] || exit 2
for i in $(seq 50); do
    out=$(nc -w 2 server 9000 2>/dev/null) && [ "$out" = reached ] && exit 0
    sleep 0.1
done
exit 1"#
                    .to_string(),
            ]))
            .unwrap();

        tokio::try_join!(server.create(), client.create()).unwrap();
        tokio::try_join!(server.start_container(), client.start_container()).unwrap();
        let (server_status, client_status) =
            tokio::try_join!(server.wait_for_container_exit(), client.wait_for_container_exit()).unwrap();
        assert!(matches!(server_status, ExitStatus::Success), "server: {server_status}");
        assert!(matches!(client_status, ExitStatus::Success), "client: {client_status}");

        let name = format!("airlock-{id}");
        let inspected = docker.inspect_container(&format!("{name}-server"), None).await.unwrap();
        assert_eq!(inspected.config.unwrap().hostname.as_deref(), Some("server"));
        let networks = inspected.network_settings.unwrap().networks.unwrap();
        let aliases = networks[&name].aliases.as_ref().unwrap();
        assert!(aliases.iter().any(|alias| alias == "server"), "{aliases:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_participant_publishes_tcp_and_udp_on_given_host_ports() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let image = test_image();
        let group = IsolationGroup::prepare(docker.clone(), &id, &["server"], &image)
            .await
            .unwrap();
        let ports = host_ports(2);
        let (tcp, udp) = (ports[0], ports[1]);
        let server = group
            .driver(
                DriverConfig::from_image("server", image.clone())
                    .with_published_port(Protocol::Tcp, 9000, tcp)
                    .with_published_port(Protocol::Udp, 9001, udp)
                    .with_exposed_port(Protocol::Tcp, 9002)
                    .with_entrypoint(vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        r#"(while true; do nc -l -p 9000 -e echo tcp-reached; done) &
(while true; do nc -l -p 9002 -e echo ephemeral-reached; done) &
out=$(nc -u -l -p 9001 -w 1); [ "$out" = udp-reached ]"#
                            .to_string(),
                    ]),
            )
            .unwrap();

        server.create().await.unwrap();
        let details = server.start_container().await.unwrap();
        assert_eq!(details.try_get_exposed_port(Protocol::Tcp, 9000), Some(tcp));
        assert_eq!(details.try_get_exposed_port(Protocol::Udp, 9001), Some(udp));
        let ephemeral = details
            .try_get_exposed_port(Protocol::Tcp, 9002)
            .expect("an ephemeral host port");
        assert!(!ports.contains(&ephemeral));

        let inspected = docker
            .inspect_container(&format!("airlock-{id}-server"), None)
            .await
            .unwrap();
        let host_config = inspected.host_config.unwrap();
        assert_ne!(host_config.publish_all_ports, Some(true));
        assert_eq!(
            host_config.port_bindings,
            Some(HashMap::from([
                (
                    "9000/tcp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some(tcp.to_string()),
                    }])
                ),
                (
                    "9001/udp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some(udp.to_string()),
                    }])
                ),
                (
                    "9002/tcp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some(String::new()),
                        host_port: Some(String::new()),
                    }])
                ),
            ]))
        );
        assert_eq!(read_from_host(tcp).await, "tcp-reached\n");
        assert_eq!(read_from_host(ephemeral).await, "ephemeral-reached\n");

        // VM-to-host port forwarders need not carry UDP, so check it from a host-network sibling.
        let code = run_on_host_network(
            &docker,
            &id,
            &image,
            format!(
                r#"tcp_status=1
for i in $(seq 50); do
    out=$(nc -w 2 127.0.0.1 {tcp} 2>/dev/null) && [ "$out" = tcp-reached ] && tcp_status=0 && break
    sleep 0.1
done
# BusyBox nc waits for a reply after sending, even with -w 0; timeout bounds each send.
for i in $(seq 50); do
    echo udp-reached | timeout 0.1 nc -u -w 0 127.0.0.1 {udp}
    sleep 0.1
done
exit "$tcp_status""#,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        let status = server.wait_for_container_exit().await.unwrap();
        assert!(matches!(status, ExitStatus::Success), "server: {status}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_port_already_published_fails_the_second_start() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let image = test_image();
        let group = IsolationGroup::prepare(docker, &id, &["first", "second"], &image)
            .await
            .unwrap();
        let port = host_ports(1)[0];
        let [first, second] = ["first", "second"].map(|name| {
            group
                .driver(
                    DriverConfig::from_image(name, image.clone())
                        .with_published_port(Protocol::Tcp, 9000, port)
                        .with_entrypoint(vec!["sleep".to_string(), "30".to_string()]),
                )
                .unwrap()
        });

        // A host-port conflict surfaces at start, not create.
        first.create().await.unwrap();
        second.create().await.unwrap();
        first.start_container().await.unwrap();
        let error = second.start_container().await.expect_err("the port is already held");
        assert!(error.to_string().contains(&format!("127.0.0.1:{port}")), "{error}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn preparing_a_group_whose_identity_is_in_use_fails() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let image = test_image();
        let _group = IsolationGroup::prepare(docker.clone(), &id, &["alpha"], &image)
            .await
            .unwrap();
        let error = IsolationGroup::prepare(docker.clone(), &id, &["alpha"], &image)
            .await
            .err()
            .expect("an identity in use should be rejected")
            .to_string();
        assert_eq!(
            error,
            format!("Isolation group '{id}' already has network 'airlock-{id}'")
        );
        let resources = group_resources(&docker, &id).await.unwrap();
        let name = format!("airlock-{id}");
        assert_eq!(resources.networks, vec![name.clone()]);
        assert_eq!(resources.volumes, vec![name]);
        assert!(resources.containers.is_empty(), "{resources:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_from_an_absent_image_fails_with_no_such_image() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let group = IsolationGroup::prepare(docker.clone(), &id, &["alpha"], &test_image())
            .await
            .unwrap();
        let absent = format!("airlock.invalid/absent:{id}");
        assert_eq!(image_id(&docker, &absent).await.unwrap(), None);
        let driver = group.driver(DriverConfig::from_image("alpha", absent)).unwrap();
        let error = driver.create().await.expect_err("an absent image should fail creation");
        assert!(
            matches!(
                error.downcast_ref::<bollard::errors::Error>(),
                Some(bollard::errors::Error::DockerResponseServerError { status_code: 404, message })
                    if message.contains("No such image")
            ),
            "{error}"
        );
        assert!(group_resources(&docker, &id).await.unwrap().containers.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_placed_before_start_keep_their_mode_and_are_read_after_exit() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let image = test_image();
        let group = IsolationGroup::prepare(docker, &id, &["subject"], &image)
            .await
            .unwrap();
        let long = format!("/opt/airlock-test/{}/file", "a".repeat(210));
        let script = format!(
            r#"set -e
[ "$(stat -c '%a %u:%g' /opt/airlock-test/bin/greet)" = '755 0:0' ]
[ "$(stat -c '%a %u:%g' /airlock/subject/shared/config.yaml)" = '600 0:0' ]
[ "$(stat -c %a /airlock/subject/shared)" = 777 ]
[ "$(stat -c %a /tmp)" = 1777 ]
[ "$(cat /tmp/placed)" = placed ]
[ "$(/opt/airlock-test/bin/greet)" = hello ]
[ "$(cat /airlock/subject/shared/config.yaml)" = 'key: value' ]
[ "$(cat '{long}')" = 'long path' ]
printf '7 0\n' > /airlock/subject/exit/record
printf layer > /tmp/output"#
        );
        let driver = group
            .driver(DriverConfig::from_image("subject", image).with_entrypoint(vec![
                "sh".to_string(),
                "-c".to_string(),
                script,
            ]))
            .unwrap();
        driver.create().await.unwrap();
        let config_path = "/airlock/subject/shared/config.yaml";
        let config = "key: value\n";
        driver
            .place_files(&[
                ContainerFile::new("/opt/airlock-test/bin/greet", "#!/bin/sh\nprintf hello\n").with_mode(0o755),
                ContainerFile::new(config_path, config).with_mode(0o600),
                ContainerFile::new(&long, "long path"),
                ContainerFile::new("/tmp/placed", "placed"),
            ])
            .await
            .unwrap();
        assert_eq!(driver.read_file(config_path).await.unwrap().unwrap(), config.as_bytes());
        assert_eq!(driver.read_file(&long).await.unwrap().unwrap(), b"long path");
        let error = driver
            .place_files(&[ContainerFile::new("/etc", "file")])
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("cannot overwrite directory"), "{error:#}");

        driver.start_container().await.unwrap();
        let status = driver.wait_for_container_exit().await.unwrap();
        assert!(matches!(status, ExitStatus::Success), "{status}");
        for (path, expected) in [
            ("/airlock/subject/exit/record", &b"7 0\n"[..]),
            ("/tmp/output", &b"layer"[..]),
            (config_path, config.as_bytes()),
        ] {
            assert_eq!(driver.read_file(path).await.unwrap().unwrap(), expected);
        }
        for path in ["/airlock/subject/missing", "/no/such/dir/file"] {
            assert_eq!(driver.read_file(path).await.unwrap(), None);
        }
        let error = driver.read_file("/airlock/subject/exit").await.unwrap_err();
        assert!(error.to_string().contains("directory"), "{error}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_is_read_while_its_container_runs_and_reading_a_removed_container_fails() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(docker.clone(), &id, "printf running > /tmp/state; exec sleep 60").await;
        timeout(Duration::from_secs(10), async {
            while driver.read_file("/tmp/state").await.unwrap().as_deref() != Some(b"running") {
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the container should write its state within 10 seconds");
        docker
            .remove_container(
                &format!("airlock-{id}-subject"),
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await
            .unwrap();
        let error = driver.read_file("/tmp/state").await.unwrap_err();
        assert!(error.to_string().contains("no such container"), "{error}");
        assert!(driver
            .place_files(&[ContainerFile::new("/file", "contents")])
            .await
            .is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_feeds_stdin_and_env_and_returns_both_streams_and_a_nonzero_exit_as_data() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(docker, &id, "exec sleep 60").await;
        let stdin = b"payload\nwithout-eol".to_vec();
        let greeting = "hello from exec";
        let output = driver
            .exec(
                ExecRequest::new(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "cat; printf %s \"$GREETING\" >&2; exit 3".to_string(),
                ])
                .with_env_var("GREETING", greeting)
                .with_stdin(stdin.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            output,
            ExecOutput {
                stdout: stdin,
                stderr: greeting.as_bytes().to_vec(),
                exit_code: 3
            }
        );

        let empty = driver
            .exec(ExecRequest::new(vec!["cat".to_string()]).with_stdin(Vec::new()))
            .await
            .unwrap();
        assert_eq!(
            empty,
            ExecOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 0
            }
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_echoes_a_large_stdin_whole() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(docker, &id, "exec sleep 60").await;
        // Input must exceed connection buffering to catch a write-before-read deadlock.
        let stdin: Vec<_> = (0..32 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let output = timeout(
            Duration::from_secs(30),
            driver.exec(ExecRequest::new(vec!["cat".to_string()]).with_stdin(stdin.clone())),
        )
        .await
        .expect("cat should echo 32 MiB within 30 seconds")
        .unwrap();
        assert_eq!(output.stdout, stdin);
        assert!(output.stderr.is_empty());
        assert_eq!(output.exit_code, 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_returns_when_a_command_exits_without_reading_its_stdin() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(docker, &id, "exec sleep 60").await;
        // Input must exceed connection buffering to catch waiting for a blocked writer after output closes.
        let output = timeout(
            Duration::from_secs(30),
            driver.exec(
                ExecRequest::new(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo done; exit 4".to_string(),
                ])
                .with_stdin(vec![b'x'; 32 * 1024 * 1024]),
            ),
        )
        .await
        .expect("exec should finish within 30 seconds even with unread stdin")
        .unwrap();
        assert_eq!(
            output,
            ExecOutput {
                stdout: b"done\n".to_vec(),
                stderr: Vec::new(),
                exit_code: 4
            }
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_program_or_a_killed_command_exits_as_data() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(docker, &id, "exec sleep 60").await;
        let output = driver
            .exec(ExecRequest::new(vec!["/no/such/program".to_string()]))
            .await
            .unwrap();
        assert_eq!(output.exit_code, 127);
        let output = driver
            .exec(ExecRequest::new(vec![
                "sh".to_string(),
                "-c".to_string(),
                "kill -9 $$".to_string(),
            ]))
            .await
            .unwrap();
        assert_eq!(output.exit_code, 137);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_in_a_container_that_is_not_running_is_an_error() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let image = test_image();
        let group = IsolationGroup::prepare(docker, &id, &["subject"], &image)
            .await
            .unwrap();
        let driver = group
            .driver(DriverConfig::from_image("subject", image).with_entrypoint(vec!["true".to_string()]))
            .unwrap();
        driver.create().await.unwrap();
        let name = format!("airlock-{id}-subject");
        let error = driver
            .exec(ExecRequest::new(vec!["true".to_string()]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&name), "{error}");
        assert!(error.to_string().contains("not running"), "{error}");
        let error = driver
            .exec_stream(ExecRequest::new(vec!["true".to_string()]))
            .await
            .err()
            .expect("a container that has never started cannot run exec");
        assert!(error.to_string().contains(&name), "{error}");
        assert!(error.to_string().contains("not running"), "{error}");

        driver.start_container().await.unwrap();
        assert!(matches!(
            driver.wait_for_container_exit().await.unwrap(),
            ExitStatus::Success
        ));
        let error = driver
            .exec(ExecRequest::new(vec!["true".to_string()]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&name), "{error}");
        assert!(error.to_string().contains("not running"), "{error}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_exec_stream_leaves_its_process_running_until_the_container_stops() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(
            docker.clone(),
            &id,
            "trap 'exit 0' TERM; i=0; while true; do i=$((i+1)); echo line$i >> /tmp/lines; sleep 0.1; done",
        )
        .await;
        let mut stream = driver
            .exec_stream(ExecRequest::new(vec![
                "tail".to_string(),
                "-F".to_string(),
                "/tmp/lines".to_string(),
            ]))
            .await
            .unwrap();
        timeout(Duration::from_secs(20), async {
            let mut stdout = Vec::new();
            loop {
                match stream.next().await.expect("tail should keep running").unwrap() {
                    OutputChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                    OutputChunk::Stderr(bytes) => panic!("unexpected stderr from tail: {bytes:?}"),
                }
                if stdout.windows(b"line3\n".len()).any(|window| window == b"line3\n") {
                    break;
                }
            }
        })
        .await
        .expect("tail should stream line3 within 20 seconds");
        let error = stream.exit_code().await.unwrap_err();
        assert!(error.to_string().contains("still running"), "{error}");

        let name = format!("airlock-{id}-subject");
        let exec_ids = docker.inspect_container(&name, None).await.unwrap().exec_ids.unwrap();
        assert_eq!(exec_ids.len(), 1);
        let exec_id = &exec_ids[0];
        drop(stream);
        assert_eq!(docker.inspect_exec(exec_id).await.unwrap().running, Some(true));
        docker.stop_container(&name, None).await.unwrap();
        let inspected = docker.inspect_exec(exec_id).await.unwrap();
        assert_eq!(inspected.running, Some(false));
        assert_eq!(inspected.exit_code, Some(137));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_command_that_ends_reports_its_exit_code() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let driver = start_exec_participant(docker, &id, "exec sleep 60").await;
        let mut stream = driver
            .exec_stream(ExecRequest::new(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo out; echo err >&2; exit 5".to_string(),
            ]))
            .await
            .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                OutputChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                OutputChunk::Stderr(bytes) => stderr.extend_from_slice(&bytes),
            }
        }
        assert_eq!(stdout, b"out\n");
        assert_eq!(stderr, b"err\n");
        assert!(stream.next().await.is_none());
        assert_eq!(stream.exit_code().await.unwrap(), 5);
    })
    .await;
}
