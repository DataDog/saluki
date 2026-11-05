use std::{collections::BTreeMap, time::SystemTime};

use k8s_openapi::api::core::v1::PodSchedulingGate;
use kube::config::{KubeConfigOptions, Kubeconfig};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::tests::{recipe, IMAGE};
use super::*;
use crate::{
    artifact::{verified_artifacts, ArtifactBinding, ArtifactRole, BindingSource, VerifiedArtifact},
    backend::{runtime, COURIER_LINUX_BINARY},
    case::{fixtures, CaseLoader, Registries},
    deadline::{Budget, Deadline},
    endpoint::{EndpointKind, RunnerHostRoute, Transport},
    handle::{http_get_from_runner_host, FileKind},
    layout::{Realization, RealizationKind},
    ledger::random_id,
    lifecycle::{self, CaseControl, NativeInputs},
    observe::{output::host_output_file, Capture, ExitObservation, ExitStatus, ParticipantExit},
    participant::{ExitChannel, OutputChannel, OutputStream, ParticipantType, RenderedConfig},
    plan::Planner,
    recipe::{ContainerRecipe, PodContainer, RecipeFile},
    record::StepStatus,
    result::{ErrorKind, ReclaimResult, TestOutcome},
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
    let control = Arc::new(CaseControl {
        observations: Default::default(),
        record: Default::default(),
        deadline: Deadline::after(Budget::Case, Duration::from_secs(120)),
        cancel: CancellationToken::new(),
        interrupt: CancellationToken::new(),
        phases: Default::default(),
        ledger: ResourceLedger::default(),
        tasks: CaseTasks::new(&CancellationToken::new()),
        log_dir: work.path().join("logs"),
    });
    let scopes: Vec<_> = (0..6)
        .map(|_| ScopeLayout {
            run: random_id(),
            index: 0,
        })
        .collect();
    let run = tokio::spawn({
        let (backends, cluster, kubeconfig, control, scopes) = (
            backends.clone(),
            cluster.clone(),
            kubeconfig.clone(),
            control.clone(),
            scopes.clone(),
        );
        async move {
            timeout(Duration::from_secs(600), async {
                let (inputs, verified) = prepare_cluster(backends.clone(), &cluster, &kubeconfig, root).await;
                let scope = &scopes[0];
                let prepared = backends
                    .prepare_pod_scope(
                        scope,
                        &["server", "client", "observed", "cancelled"],
                        inputs.container_binary(COURIER_LINUX_BINARY).unwrap().clone(),
                        &control.ledger,
                        &control.tasks,
                    )
                    .await
                    .unwrap();
                let host_port = reaches_services_and_reads_shared_files(&prepared, &inputs, &control, scope).await;
                cancels_a_scheduling_gated_start(&prepared, &inputs, &control).await;
                observes_each_container_and_reads_final_node_files(&prepared, &inputs, &control, scope).await;
                runs_native_cases(&inputs, &verified, root, &control, &scopes[1..]).await;
                assert_eq!(control.tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
                assert!(TcpListener::bind(("127.0.0.1", host_port)).await.is_ok());
            })
            .await
            .expect("kind scenarios exceeded ten minutes");
        }
    })
    .await;
    let reclaim = tokio::spawn({
        let (backends, control) = (backends.clone(), control.clone());
        async move { reclaims_scopes(&backends, &control, &scopes).await }
    })
    .await;
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
    assert!(docker
        .inspect_container(&format!("{cluster}-control-plane"), None)
        .await
        .is_err());
    if let Err(error) = run {
        std::panic::resume_unwind(error.into_panic());
    }
    match reclaim {
        Ok(participants) => assert_eq!(participants, 10),
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

async fn prepare_cluster(
    backends: Arc<RunBackends>, cluster: &str, kubeconfig: &Path, root: &Path,
) -> (NativeInputs, Vec<VerifiedArtifact>) {
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| IMAGE.local_default.into());
    kind(&[
        "create",
        "cluster",
        "--name",
        cluster,
        "--kubeconfig",
        kubeconfig.to_str().unwrap(),
        "--wait",
        "120s",
    ])
    .await
    .unwrap();
    kind(&["load", "docker-image", &image, "--name", cluster])
        .await
        .unwrap();
    let config = kube::Config::from_custom_kubeconfig(
        Kubeconfig::read_from(kubeconfig).unwrap(),
        &KubeConfigOptions::default(),
    )
    .await
    .unwrap();
    let client = Client::try_from(config).unwrap();
    let connection = backends.connect_pods(client).await.unwrap();
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
    let results = backends.verify(&bindings, root).await;
    let verified = verified_artifacts(&results).unwrap_or_else(|_| panic!("artifact verification: {results:?}"));
    let inputs = NativeInputs::new(backends, &verified, root).unwrap();
    (inputs, verified)
}

async fn reclaims_scopes(backends: &RunBackends, control: &CaseControl, scopes: &[ScopeLayout]) -> usize {
    let tasks_remaining = control.tasks.stop_all(Instant::now() + TASK_STOP_TIME).await;
    let reclaim_time = Duration::from_secs(30 * scopes.len() as u64);
    let (reclaimed, issues) = control.ledger.reclaim(Instant::now() + reclaim_time).await;
    let mut remaining = Vec::new();
    if let Some(connection) = backends.pods.get() {
        timeout(Duration::from_secs(30), async {
            let namespaces: Api<Namespace> = Api::all(connection.client.clone());
            for scope in scopes {
                let namespace = namespaces.get_opt(&scope.id()).await.unwrap();
                let path = connection.node.stat(&scope.state_dir(Os::Linux)).await.unwrap();
                if namespace.is_some() || path.is_some() {
                    remaining.push((scope.id(), namespace.is_some(), path.is_some()));
                }
            }
        })
        .await
        .expect("checking reclaimed resources must finish");
    }
    assert_eq!(tasks_remaining, 0);
    assert!(issues.is_empty(), "{issues:?}");
    assert!(remaining.is_empty(), "resources survived reclaim: {remaining:?}");
    assert!(
        reclaimed.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
        "{reclaimed:?}"
    );
    reclaimed.iter().filter(|entry| entry.participant.is_some()).count()
}

async fn reaches_services_and_reads_shared_files(
    prepared: &dyn ScopeRuntime, inputs: &NativeInputs, control: &CaseControl, scope: &ScopeLayout,
) -> u16 {
    let ledger = &control.ledger;
    let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host_port = held.local_addr().unwrap().port();
    drop(held);
    let mut server_recipe =
        recipe("set -eu; cat /etc/pod-identity > /airlock/server/shared/started; exec nc -lk -p 8080 -e /serve");
    for (source, destination) in [
        ("/proc", "/node/proc"),
        ("/etc/hostname", "/node/hostname"),
        (CRI_SOCKET_PATH, "/node/socket"),
    ] {
        server_recipe.containers[0]
            .recipe
            .mounts
            .push(crate::recipe::HostMount {
                host_path: source.into(),
                container_path: destination.into(),
                read_only: true,
            });
    }
    server_recipe
        .late_files
        .insert("/etc/pod-identity".into(), "identity".into());
    server_recipe.containers[0].recipe.files.extend([
        RecipeFile {
            path: "/etc/pod-identity".into(),
            mode: 0o640,
            contents: b"uid={{ self.late.pod_uid }}\nid={{ self.late.container_id }}\nip={{ self.late.ip }}\n".to_vec(),
        },
        RecipeFile {
            path: "/etc/literal".into(),
            mode: 0o644,
            contents: b"{{ self.late.pod_uid }}".to_vec(),
        },
    ]);
    server_recipe.containers[0].recipe.files.extend([
        RecipeFile {
            path: "/www/index.html".into(),
            mode: 0o640,
            contents: b"pod-response\n".to_vec(),
        },
        RecipeFile {
            path: "/serve".into(),
            mode: 0o755,
            contents: b"#!/bin/sh\n\
                        while IFS= read -r line; do [ \"$line\" = \"$(printf '\\r')\" ] && break; done\n\
                        printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 13\\r\\nConnection: close\\r\\n\\r\\n'\n\
                        cat /www/index.html\n"
                .to_vec(),
        },
    ]);
    let realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::KubernetesPod,
    };
    let options = BTreeMap::new();
    let missing_path = format!("{}/missing-mount", scope.state_dir(Os::Linux));
    let mut missing_recipe = recipe("true");
    missing_recipe.containers[0]
        .recipe
        .mounts
        .push(crate::recipe::HostMount {
            host_path: missing_path.clone(),
            container_path: "/missing".into(),
            read_only: true,
        });
    let missing = prepared
        .participant(ParticipantRequest {
            name: "server",
            realization,
            recipe: Recipe::Pod(missing_recipe),
            specs: &[],
            runner_host_ports: vec![],
            inputs,
            options: &options,
        })
        .unwrap();
    assert_eq!(
        missing.create().await.unwrap_err().to_string(),
        format!("pod host mount source '{missing_path}' is missing on the node")
    );
    assert!(inputs
        .backends
        .pods
        .get()
        .unwrap()
        .node
        .stat(&missing_path)
        .await
        .unwrap()
        .is_none());
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
            inputs,
            options: &options,
        })
        .unwrap();
    let mut consumer_recipe =
        recipe("set -eu; cat /etc/upstream-identity > /airlock/client/shared/started; exec sleep 120");
    consumer_recipe
        .late_files
        .insert("/etc/upstream-identity".into(), "identity".into());
    consumer_recipe.containers[0].recipe.files.push(RecipeFile {
        path: "/etc/upstream-identity".into(),
        mode: 0o600,
        contents: b"uid={{ server.late.pod_uid }}\nid={{ server.late.container_id }}\nip={{ server.late.ip }}\n"
            .to_vec(),
    });
    let consumer = prepared
        .participant(ParticipantRequest {
            name: "client",
            realization,
            recipe: Recipe::Pod(consumer_recipe),
            specs: &[],
            runner_host_ports: vec![],
            inputs,
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
    let server_facts = timeout(Duration::from_secs(60), server.start())
        .await
        .expect("server identities must not wait for late files")
        .unwrap();
    let consumer_facts = timeout(Duration::from_secs(60), consumer.start())
        .await
        .expect("client identities must not wait for late files")
        .unwrap();
    assert_eq!(server_facts.len(), 3);
    assert_eq!(consumer_facts.len(), 3);
    assert_ne!(server_facts, consumer_facts);
    let connection = inputs.backends.pods.get().unwrap();
    let pods: Api<Pod> = Api::namespaced(connection.client.clone(), &scope.id());
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
        pod.spec.as_ref().unwrap().containers[0].image.as_deref(),
        Some(inputs.image_reference(IMAGE).unwrap())
    );
    let facts: runtime::LateFacts = [
        ("server".into(), server_facts.into_iter().collect()),
        ("client".into(), consumer_facts.into_iter().collect()),
    ]
    .into();
    let host_paths: BTreeMap<_, _> = pod
        .spec
        .as_ref()
        .unwrap()
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .filter(|volume| volume.name.starts_with("host-"))
        .map(|volume| {
            let source = volume.host_path.as_ref().unwrap();
            (source.path.as_str(), source.type_.as_deref().unwrap())
        })
        .collect();
    assert_eq!(
        host_paths,
        BTreeMap::from([
            ("/proc", "Directory"),
            ("/etc/hostname", "File"),
            (CRI_SOCKET_PATH, "Socket")
        ])
    );
    let status = pod.status.unwrap();
    assert_eq!(facts["server"][&LateFact::Ip], status.pod_ip.unwrap());
    assert_eq!(
        facts["server"][&LateFact::ContainerId],
        status.container_statuses.unwrap()[0]
            .container_id
            .as_deref()
            .unwrap()
            .strip_prefix("containerd://")
            .unwrap(),
    );
    for (participant, path, name) in [
        (&server, "/etc/pod-identity", "server"),
        (&consumer, "/etc/upstream-identity", "client"),
    ] {
        assert_eq!(participant.read_file(path).await.unwrap(), Some(Vec::new()));
        assert_eq!(
            participant
                .read_file(&format!("/airlock/{name}/files-ready"))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            participant
                .read_file(&format!("/airlock/{name}/shared/started"))
                .await
                .unwrap(),
            None
        );
    }
    server.write_late_files(&facts).await.unwrap();
    consumer.write_late_files(&facts).await.unwrap();
    let expected_identity = format!(
        "uid={}\nid={}\nip={}\n",
        facts["server"][&LateFact::PodUid],
        facts["server"][&LateFact::ContainerId],
        facts["server"][&LateFact::Ip],
    );
    for (participant, path, name, mode) in [
        (&server, "/etc/pod-identity", "server", 0o640),
        (&consumer, "/etc/upstream-identity", "client", 0o600),
    ] {
        assert_eq!(
            participant.read_file(path).await.unwrap().unwrap(),
            expected_identity.as_bytes()
        );
        assert_eq!(participant.stat(path).await.unwrap().unwrap().mode, mode);
        timeout(Duration::from_secs(10), async {
            loop {
                let started = participant
                    .read_file(&format!("/airlock/{name}/shared/started"))
                    .await
                    .unwrap();
                if let Some(started) = started.filter(|bytes| !bytes.is_empty()) {
                    assert_eq!(started, expected_identity.as_bytes());
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("commands must read substituted files through their existing mounts");
    }
    assert_eq!(
        server.read_file("/etc/literal").await.unwrap().unwrap(),
        b"{{ self.late.pod_uid }}"
    );

    let exec = |script: &str| ExecCommand {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        env: vec![],
        stdin: None,
    };
    assert_eq!(
        consumer
            .exec(exec("printf ready"), Duration::from_secs(10))
            .await
            .unwrap()
            .stdout,
        b"ready"
    );
    let dns_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = consumer
            .exec(exec("wget -T 2 -q -O - http://server:8080/"), Duration::from_secs(3))
            .await;
        if let Ok(output) = &result {
            if output.exit_code == 0 {
                assert_eq!(output.stdout, b"pod-response\n");
                break;
            }
        }
        assert!(
            Instant::now() < dns_deadline,
            "Service request never succeeded: {result:?}"
        );
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
    let response = http_get_from_runner_host(&RunnerHostRoute::Direct(address), server.as_ref(), "/", &limits)
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
    let node = &connection.node;
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
    timeout(Duration::from_secs(10), async {
        while pods.get_opt("server").await.unwrap().is_some() {
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("server pod deletion must finish");
    assert_eq!(
        server.read_file("/www/index.html").await.unwrap().unwrap(),
        b"pod-response\n"
    );
    assert_eq!(
        server.read_file("/airlock/server/shared/proof").await.unwrap().unwrap(),
        b"shared"
    );
    host_port
}

async fn cancels_a_scheduling_gated_start(prepared: &PodScope, inputs: &NativeInputs, control: &CaseControl) {
    let recipe = recipe("exec sleep 120");
    let mut manifests = Manifests::build(ManifestInput {
        namespace: &prepared.namespace,
        root: &prepared.root,
        node: &prepared.node.container,
        name: "cancelled",
        recipe: &recipe,
        images: &[inputs.image_reference(IMAGE).unwrap().into()],
        options: &BTreeMap::new(),
        routes: &[],
    })
    .unwrap();
    let spec = manifests.pod.spec.as_mut().unwrap();
    // Scheduling gates hold only pods not already bound to a node.
    spec.node_name = None;
    spec.scheduling_gates = Some(vec![PodSchedulingGate {
        name: "panoramic.test/hold-start".into(),
    }]);
    let participant = PodParticipant {
        client: prepared.client.clone(),
        node: prepared.node.clone(),
        namespace: prepared.namespace.clone(),
        name: "cancelled".into(),
        root: prepared.root.clone(),
        primary: recipe.containers[0].name.clone(),
        manifests,
        routes: vec![],
        tasks: prepared.tasks.clone(),
        specs: &[],
    };
    control
        .ledger
        .participant("cancelled", participant.removal(), Duration::from_secs(1));
    control
        .ledger
        .create_participant("cancelled", &prepared.namespace, &control.cancel, participant.create())
        .await
        .unwrap()
        .unwrap();
    let pods: Api<Pod> = Api::namespaced(prepared.client.clone(), &prepared.namespace);
    let cancel = control.cancel.child_token();
    let start = cancel.run_until_cancelled(participant.start());
    tokio::pin!(start);
    timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                biased;
                result = &mut start => panic!("scheduling-gated pod must remain in start: {result:?}"),
                pod = pods.get("cancelled") => {
                    let pod = pod.unwrap();
                    if let Some(status) = pod.status {
                        if status.conditions.as_ref().is_some_and(|conditions| {
                            conditions.iter().any(|condition| {
                                condition.type_ == "PodScheduled"
                                    && condition.status == "False"
                                    && condition.reason.as_deref() == Some("SchedulingGated")
                            })
                        }) {
                            assert_eq!(status.phase.as_deref(), Some("Pending"));
                            break;
                        }
                    }
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the scheduler must report the held gate while start is pending");
    cancel.cancel();
    assert!(timeout(Duration::from_secs(1), start)
        .await
        .expect("cancellation must interrupt pod startup")
        .is_none());
    assert!(pods.get_opt("cancelled").await.unwrap().is_some());
    let services: Api<Service> = Api::namespaced(prepared.client.clone(), &prepared.namespace);
    assert!(services.get_opt("cancelled").await.unwrap().is_some());
    assert_eq!(
        participant.read_file("/airlock/cancelled/files-ready").await.unwrap(),
        None
    );
}

async fn observes_each_container_and_reads_final_node_files(
    prepared: &dyn ScopeRuntime, inputs: &NativeInputs, control: &CaseControl, scope: &ScopeLayout,
) {
    const SPECS: &[ProcessSpec] = &[
        ProcessSpec {
            name: "main",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        },
        ProcessSpec {
            name: "worker",
            exit: ExitChannel::Record { file: "worker-exit" },
            output: OutputChannel::File {
                file: "worker-output",
                stream: OutputStream::Merged,
            },
        },
    ];
    let mut recipe = recipe("printf 'main-out\n'; printf 'main-err\n' >&2; exit 7");
    recipe.containers.push(PodContainer {
        name: "helper".into(),
        recipe: ContainerRecipe::new(IMAGE)
            .with_entrypoint(["sh", "-eu", "-c"])
            .with_command(["printf 'first\n' > \"$PANORAMIC_LOG_DIR/worker-output\"; \
             printf '11 0 1.25' > \"$PANORAMIC_EXIT_DIR/worker-exit\"; \
             until [ -f /airlock/observed/shared/finish ]; do sleep 0.1; done; \
             printf 'final\\000\\377' >> \"$PANORAMIC_LOG_DIR/worker-output\"; \
             printf 'helper-out\n'; exit 9"]),
        ports: vec![],
    });
    let participant = prepared
        .participant(ParticipantRequest {
            name: "observed",
            realization: Realization {
                os: Os::Linux,
                kind: RealizationKind::KubernetesPod,
            },
            recipe: Recipe::Pod(recipe),
            specs: SPECS,
            runner_host_ports: vec![],
            inputs,
            options: &BTreeMap::new(),
        })
        .unwrap();
    control
        .ledger
        .participant("observed", participant.removal(), Duration::from_secs(1));
    control
        .ledger
        .create_participant("observed", &scope.id(), &control.cancel, participant.create())
        .await
        .unwrap()
        .unwrap();
    let facts = participant.start().await.unwrap();
    let output_dir = control.log_dir.join("observed");
    std::fs::create_dir_all(&output_dir).unwrap();
    let observation = participant.clone().observe(&control.tasks, &output_dir).await;
    assert_eq!(
        participant
            .read_file("/airlock/observed/log/worker-output")
            .await
            .unwrap(),
        None
    );
    participant
        .write_late_files(&[("observed".into(), facts.into_iter().collect())].into())
        .await
        .unwrap();
    let main_exit = timeout(Duration::from_secs(30), observation.process("main").unwrap().settled())
        .await
        .expect("the primary exit must not wait for the helper");
    assert!(
        matches!(
            main_exit,
            ExitObservation::Exited(ExitStatus {
                code: Some(7),
                signal: None,
                ..
            })
        ),
        "{main_exit:?}"
    );
    assert_eq!(observation.exit(), ParticipantExit::Running);
    timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read(&observation.outputs["worker"].paths[0]).ok().as_deref() == Some(b"first\n") {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("node-backed output must arrive while the helper runs");
    // An unterminated record is accepted only after the whole pod ends.
    assert_eq!(
        observation.process("worker").unwrap().refresh().await,
        ExitObservation::Running
    );
    let connection = inputs.backends.pods.get().unwrap();
    connection
        .node
        .write(
            &format!("{}/observed/shared/finish", scope.state_dir(Os::Linux)),
            0o644,
            vec![],
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(30), observation.stopped()).await.unwrap();
    assert_eq!(observation.exit(), ParticipantExit::Ended);
    let expected_record = ExitObservation::Exited(ExitStatus {
        code: Some(11),
        signal: None,
        reason: None,
        recorded_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1250)),
    });
    assert_eq!(
        timeout(
            Duration::from_secs(10),
            observation.process("worker").unwrap().settled()
        )
        .await
        .unwrap(),
        expected_record
    );
    timeout(Duration::from_secs(10), observation.finish_output())
        .await
        .unwrap();
    assert_eq!(observation.capture(), Capture::Complete);
    for output in observation.outputs.values() {
        assert_eq!(output.current(), Capture::Complete);
    }
    assert_eq!(
        observation.outputs["main"].paths,
        [output_dir.join("containers/main.log")]
    );
    let main = std::fs::read_to_string(&observation.outputs["main"].paths[0]).unwrap();
    assert!(main.contains("main-out\n") && main.contains("main-err\n"), "{main:?}");
    assert!(!main.contains("helper-out"), "{main:?}");
    assert_eq!(
        std::fs::read(output_dir.join("containers/helper.log")).unwrap(),
        b"helper-out\n"
    );
    let expected_bytes = b"first\nfinal\0\xff";
    let worker_path = output_dir.join(host_output_file("worker", SPECS[1].output).unwrap());
    assert_eq!(observation.outputs["worker"].paths, std::slice::from_ref(&worker_path));
    assert_eq!(std::fs::read(&worker_path).unwrap(), expected_bytes);
    let reports = observation.snapshot(SPECS);
    assert_eq!(
        reports[0].output.merged.as_ref(),
        Some(&observation.outputs["main"].paths[0])
    );
    assert_eq!(reports[0].output.stdout, None);
    assert_eq!(reports[0].output.stderr, None);
    assert_eq!(reports[1].output.merged.as_ref(), Some(&worker_path));
    let (lines, _) = observation.lines_since(0).unwrap();
    for (process, text) in [("main", "main-out"), ("main", "main-err"), ("worker", "first")] {
        assert!(
            lines
                .iter()
                .any(|line| line.process == Some(process) && line.text == text && line.stream == OutputStream::Merged),
            "{lines:?}"
        );
    }
    let pods: Api<Pod> = Api::namespaced(connection.client.clone(), &scope.id());
    let pod = pods.get("observed").await.unwrap();
    let statuses = pod.status.unwrap().container_statuses.unwrap();
    for (name, code) in [("main", 7), ("helper", 9)] {
        assert_eq!(
            statuses
                .iter()
                .find(|status| status.name == name)
                .unwrap()
                .state
                .as_ref()
                .unwrap()
                .terminated
                .as_ref()
                .unwrap()
                .exit_code,
            code
        );
    }

    let replay_dir = control.log_dir.join("observed-after-exit");
    std::fs::create_dir_all(&replay_dir).unwrap();
    let replay = participant.clone().observe(&control.tasks, &replay_dir).await;
    timeout(Duration::from_secs(10), replay.stopped()).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(10), replay.process("worker").unwrap().settled())
            .await
            .unwrap(),
        expected_record
    );
    timeout(Duration::from_secs(10), replay.finish_output()).await.unwrap();
    assert_eq!(replay.capture(), Capture::Complete);
    assert_eq!(replay.outputs["worker"].current(), Capture::Complete);
    assert_eq!(
        std::fs::read(&replay.outputs["worker"].paths[0]).unwrap(),
        expected_bytes
    );
    pods.delete(
        "observed",
        &DeleteParams {
            grace_period_seconds: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(10), async {
        while pods.get_opt("observed").await.unwrap().is_some() {
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        participant
            .read_file("/airlock/observed/log/worker-output")
            .await
            .unwrap()
            .unwrap(),
        expected_bytes
    );
    assert_eq!(
        participant
            .read_file("/airlock/observed/exit/worker-exit")
            .await
            .unwrap()
            .unwrap(),
        b"11 0 1.25"
    );
}

struct NativePod;

impl ParticipantType for NativePod {
    fn name(&self) -> &'static str {
        "native-pod"
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::KubernetesPod)]
    }

    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[IMAGE]
    }

    fn stop_grace(&self, _: Realization) -> Duration {
        Duration::from_secs(1)
    }

    fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        let mut recipe = recipe(if config.files.contains_key("identity") {
            "set -eu; test -s /etc/identity; \
             cat /etc/identity > \"/airlock/$PANORAMIC_PARTICIPANT/shared/started\"; cat /etc/identity"
        } else {
            "exec sleep 120"
        });
        if let Some(contents) = config.files.get("identity") {
            recipe.containers[0].recipe.files.push(RecipeFile {
                path: "/etc/identity".into(),
                mode: 0o600,
                contents: contents.clone(),
            });
            recipe.late_files.insert("/etc/identity".into(), "identity".into());
        }
        if config.env.contains_key("FAIL_SETUP") {
            recipe.containers[0].recipe.files.push(RecipeFile {
                path: "/etc/unavailable".into(),
                mode: 0o600,
                contents: b"{{ absent.late.pod_uid }}".to_vec(),
            });
            recipe.late_files.insert("/etc/unavailable".into(), "identity".into());
        }
        Ok(Recipe::Pod(recipe))
    }

    fn config_files(&self) -> &'static [&'static str] {
        &["identity"]
    }

    fn processes(&self) -> &'static [&'static str] {
        &["main"]
    }

    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        &[ProcessSpec {
            name: "main",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }]
    }
}

async fn runs_native_cases(
    inputs: &NativeInputs, verified: &[VerifiedArtifact], root: &Path, shared: &CaseControl, scopes: &[ScopeLayout],
) {
    let directory = shared.log_dir.join("native");
    std::fs::create_dir_all(&directory).unwrap();
    let own = "shared={{ self.facts.shared_dir }}\n\
               uid={{ self.late.pod_uid }}\n\
               id={{ self.late.container_id }}\n\
               ip={{ self.late.ip }}\n";
    std::fs::write(directory.join("own.txt"), own).unwrap();
    std::fs::write(
        directory.join("peer.txt"),
        format!(
            "{own}peer_uid={{{{ first.late.pod_uid }}}}\n\
             peer_id={{{{ first.late.container_id }}}}\n\
             peer_ip={{{{ first.late.ip }}}}\n",
        ),
    )
    .unwrap();
    let registries = Registries {
        participant_types: &[&NativePod],
        steps: fixtures::STEPS,
    };
    let scenarios = [
        (
            "late-files",
            r"  first:
    type: native-pod
    subject: true
    realization: {kind: kubernetes-pod}
    config: {files: {identity: {from: own.txt, template: true}}}
  second:
    type: native-pod
    config: {files: {identity: {from: peer.txt, template: true}}}",
            "[{step: probe, on: first, process: main}, {step: probe, on: second, process: main}]",
        ),
        (
            "missing-image",
            r"  missing-image:
    type: native-pod
    subject: true
    realization: {kind: kubernetes-pod}",
            "[{step: noop}]",
        ),
        (
            "setup",
            r"  setup:
    type: native-pod
    subject: true
    realization: {kind: kubernetes-pod}
    config: {env: {FAIL_SETUP: 'true'}, files: {identity: {from: own.txt, template: true}}}",
            "[{step: noop}]",
        ),
        (
            "failed",
            "  failed: {type: native-pod, subject: true, realization: {kind: kubernetes-pod}}",
            "[{step: probe, on: failed, path: /airlock/failed/shared/missing}]",
        ),
        (
            "timed",
            "  timed: {type: native-pod, subject: true, realization: {kind: kubernetes-pod}}",
            "[{step: marker, on: timed, timeout: 100ms}]",
        ),
    ];
    assert_eq!(scopes.len(), scenarios.len());
    for (scope, (name, participants, procedure)) in scopes.iter().zip(scenarios) {
        let case = CaseLoader::new(registries)
            .load_content(
                "cases",
                &format!("timeout: 90s\nsubject_os: [linux]\nparticipants:\n{participants}\nprocedure: {procedure}\n"),
                "case",
                &directory.join("config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, &scope.run)
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(plan.scope(), scope);
        if name == "late-files" {
            assert_eq!(
                plan.start_order(),
                &[vec!["first".to_string()], vec!["second".to_string()]]
            );
        }
        let control = CaseControl {
            observations: Default::default(),
            record: Default::default(),
            deadline: Deadline::after(Budget::Case, Duration::from_secs(90)),
            cancel: CancellationToken::new(),
            interrupt: CancellationToken::new(),
            phases: Default::default(),
            ledger: shared.ledger.clone(),
            tasks: shared.tasks.clone(),
            log_dir: directory.join(name),
        };
        let mut artifacts = verified.to_vec();
        let missing_image = format!("panoramic-kind-missing:{}", random_id());
        if name == "missing-image" {
            // A missing local image must fail setup before the startup deadline.
            artifacts
                .iter_mut()
                .find(|artifact| artifact.role == IMAGE.name)
                .unwrap()
                .reference = missing_image.clone();
        }
        let inputs = NativeInputs::new(inputs.backends.clone(), &artifacts, root).unwrap();
        let connection = inputs.backends.pods.get().unwrap();
        let pods: Api<Pod> = Api::namespaced(connection.client.clone(), &scope.id());
        let limit = Duration::from_secs(if name == "missing-image" { 30 } else { 90 });
        let result = timeout(
            limit,
            lifecycle::run(plan.participants(), &[&NativePod], &inputs, &control),
        )
        .await
        .unwrap_or_else(|_| panic!("native scenario '{name}' did not finish within {limit:?}"));
        match name {
            "late-files" => {
                assert_eq!(result.outcome, TestOutcome::Passed, "{result:?}");
                let first = pods.get("first").await.unwrap();
                let first = started_facts(&first, "main")
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .collect::<BTreeMap<_, _>>();
                for name in ["first", "second"] {
                    let pod = pods.get(name).await.unwrap();
                    let facts = started_facts(&pod, "main")
                        .unwrap()
                        .unwrap()
                        .into_iter()
                        .collect::<BTreeMap<_, _>>();
                    let mut expected = format!(
                        "shared=/airlock/{name}/shared\nuid={}\nid={}\nip={}\n",
                        facts[&LateFact::PodUid],
                        facts[&LateFact::ContainerId],
                        facts[&LateFact::Ip],
                    );
                    if name == "second" {
                        expected.push_str(&format!(
                            "peer_uid={}\npeer_id={}\npeer_ip={}\n",
                            first[&LateFact::PodUid],
                            first[&LateFact::ContainerId],
                            first[&LateFact::Ip]
                        ));
                    }
                    let path = format!("{}/{name}/shared/started", scope.state_dir(Os::Linux));
                    assert_eq!(connection.node.read(&path).await.unwrap().unwrap(), expected.as_bytes());
                    let reports = control.observations.snapshot();
                    let process = &reports.iter().find(|report| report.name == name).unwrap().processes[0];
                    assert!(
                        matches!(
                            process.exit,
                            crate::result::ProcessExitReport::Exited { code: Some(0), .. }
                        ),
                        "{process:?}"
                    );
                    let path = process.output.merged.as_ref().expect("pod logs are merged");
                    assert_eq!(std::fs::read(path).unwrap(), expected.as_bytes());
                }
            }
            "missing-image" | "setup" => {
                assert_eq!(result.outcome, TestOutcome::Errored, "{result:?}");
                let error = result.error.unwrap();
                assert_eq!(error.kind, ErrorKind::Setup);
                if name == "missing-image" {
                    assert!(!control.cancel.is_cancelled());
                    assert!(error.message.contains("ErrImageNeverPull"), "{error:?}");
                    assert!(error.message.contains("main"), "{error:?}");
                    assert!(error.message.contains(&missing_image), "{error:?}");
                } else {
                    assert!(error.message.contains("needs unavailable reference"), "{error:?}");
                    assert!(error.message.contains("/etc/unavailable"), "{error:?}");
                }
                assert!(result.assertion_results.is_empty());
                let path = format!("{}/{name}/files-ready", scope.state_dir(Os::Linux));
                assert_eq!(connection.node.read(&path).await.unwrap(), None);
            }
            "failed" => {
                assert_eq!(result.outcome, TestOutcome::Failed, "{result:?}");
                assert_eq!(result.assertion_results[0].status, StepStatus::Failed);
                assert!(result.assertion_results[0].message.contains("is missing"), "{result:?}");
            }
            "timed" => {
                assert_eq!(result.outcome, TestOutcome::Errored, "{result:?}");
                assert_eq!(result.assertion_results[0].status, StepStatus::Errored);
                assert!(
                    result.assertion_results[0].message.contains("step budget"),
                    "{result:?}"
                );
            }
            _ => unreachable!(),
        }
        let services: Api<Service> = Api::namespaced(connection.client.clone(), &scope.id());
        for participant in plan.participants().file().participants.keys() {
            assert!(pods.get_opt(participant).await.unwrap().is_some());
            assert!(services.get_opt(participant).await.unwrap().is_some());
        }
        assert!(connection
            .node
            .stat(&scope.state_dir(Os::Linux))
            .await
            .unwrap()
            .is_some());
    }
}
