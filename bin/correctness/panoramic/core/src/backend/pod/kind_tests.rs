use std::collections::BTreeMap;

use kube::config::{KubeConfigOptions, Kubeconfig};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::tests::{recipe, IMAGE};
use super::*;
use crate::{
    artifact::{verified_artifacts, ArtifactBinding, BindingSource},
    backend::{runtime, COURIER_LINUX_BINARY},
    deadline::{Budget, Deadline},
    endpoint::{EndpointKind, RunnerHostRoute, Transport},
    handle::{http_get_from_runner_host, FileKind},
    layout::{Realization, RealizationKind},
    ledger::random_id,
    lifecycle::{CaseControl, NativeInputs},
    result::ReclaimResult,
    tasks::TASK_STOP_TIME,
};

async fn kind(args: &[&str]) -> Result<(), GenericError> {
    let output = timeout(
        Duration::from_secs(180),
        Command::new("kind").args(args).kill_on_drop(true).output(),
    )
    .await??;
    if !output.status.success() {
        return Err(generic_error!(
            "kind {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs local kind and Docker; run with make test-panoramic-kind"]
async fn pods_reach_services_read_shared_files_run_the_courier_and_reclaim_the_scope() {
    #[cfg(not(windows))]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(windows)]
    let _ = rustls_cng_crypto::default_provider().install_default();

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap();
    let work = tempfile::Builder::new()
        .prefix("panoramic-kind-")
        .suffix("-validation")
        .tempdir_in(root.join("target"))
        .unwrap();
    let kubeconfig = work.path().join("kubeconfig");
    let cluster = format!("panoramic-{}", random_id());
    eprintln!("Kind test cluster: {cluster}");
    let backends = Arc::new(RunBackends::default());
    let docker = backends.docker.client(Os::Linux).await.unwrap();
    let had_network = docker
        .list_networks(None)
        .await
        .unwrap()
        .iter()
        .any(|network| network.name.as_deref() == Some("kind"));
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| IMAGE.local_default.into());
    let tasks = CaseTasks::new(&CancellationToken::new());
    let ledger = ResourceLedger::default();
    let scope = ScopeLayout {
        run: random_id(),
        index: 0,
    };
    let run = tokio::spawn({
        let (backends, cluster, kubeconfig, image, tasks, ledger, scope) = (
            backends.clone(),
            cluster.clone(),
            kubeconfig.clone(),
            image.clone(),
            tasks.clone(),
            ledger.clone(),
            scope.clone(),
        );
        let root = root.to_path_buf();
        let log_dir = work.path().join("logs");
        async move {
            timeout(Duration::from_secs(300), async {
                kind(&[
                    "create",
                    "cluster",
                    "--name",
                    &cluster,
                    "--kubeconfig",
                    kubeconfig.to_str().unwrap(),
                    "--wait",
                    "120s",
                ])
                .await
                .unwrap();
                kind(&["load", "docker-image", &image, "--name", &cluster])
                    .await
                    .unwrap();
                let config = kube::Config::from_custom_kubeconfig(
                    Kubeconfig::read_from(&kubeconfig).unwrap(),
                    &KubeConfigOptions::default(),
                )
                .await
                .unwrap();
                let client = Client::try_from(config).unwrap();
                let connection = backends.connect_pods(client.clone()).await.unwrap();
                assert!(backends.pods.set(connection).is_ok());
                let bindings = [
                    ArtifactBinding {
                        role: IMAGE,
                        reference: image,
                        source: BindingSource::Manifest,
                    },
                    ArtifactBinding {
                        role: COURIER_LINUX_BINARY,
                        reference: COURIER_LINUX_BINARY.local_default.into(),
                        source: BindingSource::LocalDefault,
                    },
                ];
                let results = backends.verify(&bindings, &root).await;
                let verified =
                    verified_artifacts(&results).unwrap_or_else(|_| panic!("artifact verification: {results:?}"));
                let inputs = NativeInputs::new(backends.clone(), &verified, &root).unwrap();
                let control = CaseControl {
                    observations: Default::default(),
                    record: Default::default(),
                    deadline: Deadline::after(Budget::Case, Duration::from_secs(120)),
                    cancel: CancellationToken::new(),
                    interrupt: CancellationToken::new(),
                    phases: Default::default(),
                    ledger: ledger.clone(),
                    tasks: tasks.clone(),
                    log_dir,
                };
                let prepared = runtime::prepare(
                    RealizationKind::KubernetesPod,
                    &scope,
                    &["server", "client"],
                    &inputs,
                    &control,
                )
                .await
                .unwrap();
                let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let host_port = held.local_addr().unwrap().port();
                drop(held);
                let mut server_recipe = recipe("exec nc -lk -p 8080 -e /serve");
                server_recipe.containers[0].recipe.files.extend([
                    crate::recipe::RecipeFile { path: "/www/index.html".into(), mode: 0o640, contents: b"pod-response\n".to_vec() },
                    crate::recipe::RecipeFile {
                        path: "/serve".into(), mode: 0o755,
                        contents: b"#!/bin/sh\nwhile IFS= read -r line; do [ \"$line\" = \"$(printf '\\r')\" ] && break; done\nprintf 'HTTP/1.1 200 OK\\r\\nContent-Length: 13\\r\\nConnection: close\\r\\n\\r\\n'\ncat /www/index.html\n".to_vec(),
                    },
                ]);
                let realization = Realization {
                    os: Os::Linux,
                    kind: RealizationKind::KubernetesPod,
                };
                let options = BTreeMap::new();
                let server = prepared
                    .participant(ParticipantRequest {
                        name: "server",
                        realization,
                        recipe: Recipe::Pod(server_recipe),
                        specs: &[],
                        runner_host_ports: vec![RunnerHostPort {
                            transport: Transport::Tcp,
                            container_port: 8080,
                            host_port,
                        }],
                        inputs: &inputs,
                        options: &options,
                    })
                    .unwrap();
                let consumer = prepared
                    .participant(ParticipantRequest {
                        name: "client",
                        realization,
                        recipe: Recipe::Pod(recipe("exec sleep 120")),
                        specs: &[],
                        runner_host_ports: vec![],
                        inputs: &inputs,
                        options: &options,
                    })
                    .unwrap();
                for (name, participant) in [("server", &server), ("client", &consumer)] {
                    ledger.participant(name, participant.removal(), Duration::from_secs(1));
                    assert!(ledger
                        .create_participant(name, &scope.id(), &control.cancel, participant.create())
                        .await
                        .unwrap()
                        .unwrap()
                        .is_empty());
                }
                let server_facts = server.start().await.unwrap();
                let consumer_facts = consumer.start().await.unwrap();
                assert_eq!(server_facts.len(), 3);
                assert_eq!(consumer_facts.len(), 3);
                assert_ne!(server_facts, consumer_facts);
                let pods: Api<Pod> = Api::namespaced(client.clone(), &scope.id());
                let pod = pods.get("server").await.unwrap();
                assert_eq!(
                    server_facts
                        .iter()
                        .find(|(fact, _)| *fact == LateFact::PodUid)
                        .unwrap()
                        .1,
                    pod.metadata.uid.unwrap()
                );
                assert_eq!(
                    pod.spec.unwrap().containers[0].image.as_deref(),
                    Some(bindings[0].reference.as_str())
                );

                let exec = |script: &str| ExecCommand {
                    argv: vec!["sh".into(), "-c".into(), script.into()],
                    env: vec![],
                    stdin: None,
                };
                assert_eq!(consumer.exec(exec("printf ready"), Duration::from_secs(10)).await.unwrap().stdout, b"ready");
                let dns_deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let result = consumer.exec(exec("wget -T 2 -q -O - http://server:8080/"), Duration::from_secs(3)).await;
                    if let Ok(output) = &result {
                        if output.exit_code == 0 {
                            assert_eq!(output.stdout, b"pod-response\n");
                            break;
                        }
                    }
                    assert!(Instant::now() < dns_deadline, "Service request never succeeded: {result:?}");
                    sleep(Duration::from_millis(200)).await;
                }
                let limits = AttemptLimits::new(Duration::from_secs(5));
                let address = ResolvedAddress {
                    kind: EndpointKind::Http,
                    url: format!("http://127.0.0.1:{host_port}"),
                    host: Some("127.0.0.1".into()),
                    port: Some(host_port),
                    path: None,
                };
                let response =
                    http_get_from_runner_host(&RunnerHostRoute::Direct(address), server.as_ref(), "/", &limits)
                        .await
                        .unwrap();
                assert_eq!(
                    response,
                    HttpResponse {
                        status: 200,
                        body: b"pod-response\n".to_vec()
                    }
                );
                let inside = ResolvedAddress {
                    kind: EndpointKind::Tcp,
                    url: "tcp://127.0.0.1:8080".into(),
                    host: Some("127.0.0.1".into()),
                    port: Some(8080),
                    path: None,
                };
                assert_eq!(server.probe(&inside, &limits).await.unwrap(), ProbeAnswer::Listening);
                let node = &backends.pods.get().unwrap().node;
                let courier = node
                    .read(&format!("{}/server/courier", scope.state_dir(Os::Linux)))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    courier.as_slice(),
                    inputs.container_binary(COURIER_LINUX_BINARY).unwrap().as_ref()
                );
                assert_eq!(
                    server.read_file("/www/index.html").await.unwrap().unwrap(),
                    b"pod-response\n"
                );
                assert_eq!(
                    server.stat("/www/index.html").await.unwrap().unwrap(),
                    FileStatus {
                        kind: FileKind::File,
                        mode: 0o640,
                        size: 13
                    }
                );
                assert_eq!(server.read_file("/airlock/server/shared/missing").await.unwrap(), None);
                assert_eq!(server.stat("/airlock/server/shared/missing").await.unwrap(), None);
                assert_eq!(
                    server
                        .exec(
                            exec("printf shared > /airlock/server/shared/proof"),
                            Duration::from_secs(5)
                        )
                        .await
                        .unwrap()
                        .exit_code,
                    0
                );
                assert_eq!(
                    consumer
                        .exec(exec("cat /airlock/server/shared/proof"), Duration::from_secs(5))
                        .await
                        .unwrap()
                        .stdout,
                    b"shared"
                );
                let mut command = exec("printf '%s' \"$VALUE\"; printf err >&2; exit 7");
                command.env.push(("VALUE".into(), "a b;$literal".into()));
                assert_eq!(
                    consumer.exec(command, Duration::from_secs(5)).await.unwrap(),
                    ExecOutput {
                        stdout: b"a b;$literal".to_vec(),
                        stderr: b"err".to_vec(),
                        exit_code: 7
                    }
                );
                let mut command = exec("cat");
                command.stdin = Some(vec![]);
                assert!(consumer
                    .exec(command, Duration::from_secs(5))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("stdin"));
                assert!(consumer
                    .exec(exec("sleep 10"), Duration::from_millis(50))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Abandoned"));
                pods.delete(
                    "server",
                    &DeleteParams {
                        grace_period_seconds: Some(0),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                while pods.get_opt("server").await.unwrap().is_some() {
                    sleep(Duration::from_millis(200)).await;
                }
                assert_eq!(
                    server.read_file("/www/index.html").await.unwrap().unwrap(),
                    b"pod-response\n"
                );
                assert_eq!(
                    server.read_file("/airlock/server/shared/proof").await.unwrap().unwrap(),
                    b"shared"
                );
                assert_eq!(tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
                assert!(TcpListener::bind(("127.0.0.1", host_port)).await.is_ok());
            })
            .await
            .expect("kind scenario exceeded five minutes");
        }
    })
    .await;
    let tasks_remaining = tasks.stop_all(Instant::now() + TASK_STOP_TIME).await;
    let (reclaimed, issues) = ledger.reclaim(Instant::now() + Duration::from_secs(30)).await;
    let remaining = if let Some(connection) = backends.pods.get() {
        let namespaces: Api<Namespace> = Api::all(connection.client.clone());
        let namespace = namespaces.get_opt(&scope.id()).await.unwrap();
        let path = connection.node.stat(&scope.state_dir(Os::Linux)).await.unwrap();
        (namespace.is_some(), path.is_some())
    } else {
        (false, false)
    };
    let deleted = kind(&[
        "delete",
        "cluster",
        "--name",
        &cluster,
        "--kubeconfig",
        kubeconfig.to_str().unwrap(),
    ])
    .await;
    if !had_network {
        if let Ok(network) = docker.inspect_network("kind", None).await {
            if network
                .containers
                .as_ref()
                .is_none_or(|containers| containers.is_empty())
            {
                docker.remove_network(network.id.as_ref().unwrap()).await.unwrap();
            }
        }
    }
    assert!(deleted.is_ok(), "cluster deletion: {deleted:?}");
    assert_eq!(tasks_remaining, 0);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(remaining, (false, false));
    assert!(
        reclaimed.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
        "{reclaimed:?}"
    );
    assert!(docker
        .inspect_container(&format!("{cluster}-control-plane"), None)
        .await
        .is_err());
    if let Err(error) = run {
        std::panic::resume_unwind(error.into_panic());
    }
    assert_eq!(reclaimed.iter().filter(|entry| entry.participant.is_some()).count(), 2);
}
