//! Tests against a real Docker daemon. They are ignored by default; run them by hand with `make test-container-daemon`.

use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bollard::{
    models::{NetworkCreateRequest, VolumeCreateRequest},
    query_parameters::{ListContainersOptionsBuilder, RemoveVolumeOptions},
};
use bytes::Bytes;
use futures::{FutureExt as _, StreamExt as _};
use tokio::{io::AsyncReadExt as _, net::TcpStream, time::Instant};

use crate::{
    container::{ContainerSpec, Output},
    files::Archive,
    image::{ensure_image, DAEMON_TEST_IMAGE},
    removal::is_not_found,
    scope::{CaseScope, ScopePlatform},
};

const BUDGET: Duration = Duration::from_secs(60);

fn deadline() -> Instant {
    Instant::now() + BUDGET
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

/// Returns a scope ID that no other test run uses.
fn unique_id(test: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{test}-{nanos:x}")
}

/// Runs `body` in a prepared scope with an ID no other test run uses, then checks that the scope is reclaimed.
///
/// The scope is reclaimed even when `body` panics, so a failing test leaves nothing behind.
async fn in_scope<F, Fut>(test: &str, body: F)
where
    F: FnOnce(CaseScope) -> Fut,
    Fut: Future<Output = ()>,
{
    let docker = crate::docker::connect().expect("a Docker daemon should be reachable");
    ensure_image(&docker, DAEMON_TEST_IMAGE, deadline())
        .await
        .expect("the test image should be available");
    let scope = CaseScope::new(docker, unique_id(test), ScopePlatform::LINUX);
    let outcome = AssertUnwindSafe(async {
        scope.prepare(deadline()).await.expect("the scope should be prepared");
        body(scope.clone()).await;
    })
    .catch_unwind()
    .await;
    if let Err(panic) = outcome {
        if let Err(e) = scope.reclaim(deadline()).await {
            eprintln!("Failed to reclaim the scope of a failed test: {e:#}");
        }
        std::panic::resume_unwind(panic);
    }
    assert_reclaimed(&scope).await;
}

/// Reclaims `scope` and checks that no container, volume or network of it is left.
async fn assert_reclaimed(scope: &CaseScope) {
    scope.reclaim(deadline()).await.expect("reclaim should succeed");

    let name = scope.resource_name();
    let label = scope.case_label();
    let filters: HashMap<&str, Vec<&str>> = HashMap::from([("label", vec![label.as_str()])]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let containers = scope.docker.list_containers(Some(options)).await.unwrap();
    assert!(containers.is_empty(), "{containers:?}");
    let network = scope.docker.inspect_network(&name, None).await;
    assert!(network.as_ref().is_err_and(is_not_found), "{network:?}");
    let volume = scope.docker.inspect_volume(&name).await;
    assert!(volume.as_ref().is_err_and(is_not_found), "{volume:?}");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_started_container_has_its_files_runs_commands_and_publishes_a_port() {
    in_scope("files", |scope| async move {
        let spec = ContainerSpec::new(DAEMON_TEST_IMAGE)
            .with_command(strings(&[
                "sh",
                "-c",
                "nc -lk -p 8080 -e echo pong & cat /data/greeting && exec sleep 300",
            ]))
            .with_published_port(8080);
        let container = scope.create_container("server", spec, deadline()).await.unwrap();

        let mut archive = Archive::new();
        archive.add_file("data/greeting", b"hello\n", 0o644).unwrap();
        container.place("/", archive, deadline()).await.unwrap();
        container.start(deadline()).await.unwrap();

        // The command printed the file, so it was in place before start.
        let first = tokio::time::timeout(BUDGET, container.logs().next()).await.unwrap();
        assert_eq!(first.unwrap().unwrap(), Output::Stdout(Bytes::from_static(b"hello\n")));
        assert_eq!(
            container
                .read_file("/data/greeting", deadline())
                .await
                .unwrap()
                .unwrap(),
            b"hello\n"
        );
        assert_eq!(container.read_file("/data/missing", deadline()).await.unwrap(), None);

        let echoed = container
            .exec(strings(&["cat"]), Some(Bytes::from_static(b"ping")), deadline())
            .await
            .unwrap();
        assert_eq!((echoed.exit_code, echoed.stdout.as_slice()), (0, b"ping".as_slice()));
        // More input than a pipe holds, to a command that reads none of it.
        let ignored = container
            .exec(strings(&["true"]), Some(Bytes::from(vec![0; 1 << 20])), deadline())
            .await
            .unwrap();
        assert_eq!(ignored.exit_code, 0);
        let failed = container
            .exec(strings(&["sh", "-c", "echo oops >&2; exit 3"]), None, deadline())
            .await
            .unwrap();
        assert_eq!((failed.exit_code, failed.stderr.as_slice()), (3, b"oops\n".as_slice()));

        // This settles that `bollard` reports the host port Docker chose.
        let port = container.published_port(8080, deadline()).await.unwrap();
        assert_ne!(port, 0);
        // Retries until the listener is up.
        let answer = tokio::time::timeout(BUDGET, async {
            loop {
                let mut answer = Vec::new();
                if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await {
                    if stream.read_to_end(&mut answer).await.is_ok() && !answer.is_empty() {
                        return answer;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the listener should answer through the published port");
        assert_eq!(answer, b"pong\n");

        // `sleep` as PID 1 ignores SIGTERM, so stop kills it after the grace.
        container.stop(Duration::from_secs(1), deadline()).await.unwrap();
        assert_eq!(container.wait(deadline()).await.unwrap(), 128 + 9);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn an_exited_container_reports_its_exit_code_and_output() {
    in_scope("exit", |scope| async move {
        let spec = ContainerSpec::new(DAEMON_TEST_IMAGE).with_command(strings(&[
            "sh",
            "-c",
            "echo out; echo err >&2; exit 7",
        ]));
        let container = scope.create_container("job", spec, deadline()).await.unwrap();
        container.start(deadline()).await.unwrap();

        assert_eq!(container.wait(deadline()).await.unwrap(), 7);
        let output: Vec<Output> = container.logs().map(Result::unwrap).collect().await;
        assert!(
            output.contains(&Output::Stdout(Bytes::from_static(b"out\n"))),
            "{output:?}"
        );
        assert!(
            output.contains(&Output::Stderr(Bytes::from_static(b"err\n"))),
            "{output:?}"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn reclaim_removes_running_and_never_started_containers() {
    in_scope("reclaim", |scope| async move {
        let broken = ContainerSpec::new(DAEMON_TEST_IMAGE).with_entrypoint(strings(&["/no/such/program"]));
        let broken = scope.create_container("broken", broken, deadline()).await.unwrap();
        broken
            .start(deadline())
            .await
            .expect_err("a missing program should fail the start");
        broken
            .wait(deadline())
            .await
            .expect_err("a container whose start failed has no exit code");
        let idle = ContainerSpec::new(DAEMON_TEST_IMAGE);
        let idle = scope.create_container("idle", idle, deadline()).await.unwrap();
        idle.wait(deadline())
            .await
            .expect_err("a container that was never started has no exit code");
        let running = ContainerSpec::new(DAEMON_TEST_IMAGE).with_command(strings(&["sleep", "300"]));
        let running = scope.create_container("running", running, deadline()).await.unwrap();
        running.start(deadline()).await.unwrap();

        scope.reclaim(deadline()).await.unwrap();
        // `in_scope` reclaims again, when everything is already gone, which counts as removed.
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_scope_neither_adopts_nor_reclaims_a_network_or_volume_of_another_case() {
    let docker = crate::docker::connect().expect("a Docker daemon should be reachable");
    let scope = CaseScope::new(docker.clone(), unique_id("foreign"), ScopePlatform::LINUX);
    let name = scope.resource_name();
    let foreign_labels = HashMap::from([("airlock-case".to_string(), "another-case".to_string())]);
    let volume = VolumeCreateRequest {
        name: Some(name.clone()),
        labels: Some(foreign_labels.clone()),
        ..Default::default()
    };
    docker.create_volume(volume).await.unwrap();

    let outcome = AssertUnwindSafe(async {
        let error = scope
            .prepare(deadline())
            .await
            .expect_err("an existing volume should fail the prepare");
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
        // The scope's own network goes; the volume stays.
        let error = scope
            .reclaim(deadline())
            .await
            .expect_err("reclaim should report the volume");
        assert!(
            format!("{error:#}").contains(&format!("volume '{name}': skipped")),
            "{error:#}"
        );
        docker
            .inspect_volume(&name)
            .await
            .expect("the volume should be left alone");
        let network = docker.inspect_network(&name, None).await;
        assert!(network.as_ref().is_err_and(is_not_found), "{network:?}");

        let network = NetworkCreateRequest {
            name: name.clone(),
            labels: Some(foreign_labels),
            ..Default::default()
        };
        docker.create_network(network).await.unwrap();
        let error = scope
            .prepare(deadline())
            .await
            .expect_err("another case's network should fail the prepare");
        assert!(format!("{error:#}").contains("another case"), "{error:#}");
        let error = scope
            .reclaim(deadline())
            .await
            .expect_err("reclaim should report the network");
        assert!(
            format!("{error:#}").contains(&format!("network '{name}': skipped")),
            "{error:#}"
        );
        docker
            .inspect_network(&name, None)
            .await
            .expect("the network should be left alone");
    })
    .catch_unwind()
    .await;

    let volume_removed = docker.remove_volume(&name, None::<RemoveVolumeOptions>).await;
    let network_removed = docker.remove_network(&name).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    volume_removed.expect("the test should remove the volume it made");
    network_removed.expect("the test should remove the network it made");
}
