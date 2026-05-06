use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use k8s_openapi::{
    api::core::v1::{
        ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar, HostPathVolumeSource, Namespace,
        Pod, PodSpec, Volume, VolumeMount,
    },
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};
use kube::{
    api::{Api, DeleteParams, LogParams, PostParams},
    Client,
};
use rand::{distr::SampleString as _, rng};
use rand_distr::Alphanumeric;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{io::AsyncWriteExt as _, net::TcpListener, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    correctness::{
        analysis::{AnalysisMode, AnalysisRunner, CollectedData, TracesAnalysisOptions},
        config::{Config, TargetConfig},
        runner::make_error_result,
    },
    reporter::{PhaseTiming, TestResult},
    test::TestContext,
};

const POD_NAME: &str = "correctness-pod";
const FLUSH_WAIT_SECS: u64 = 32;
const POD_POLL_INTERVAL: Duration = Duration::from_secs(1);
const POD_READY_TIMEOUT: Duration = Duration::from_secs(120);
// Allow enough time for millstone to finish sending all metrics plus a buffer.
const MILLSTONE_EXIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Entry point called by the correctness runner when `runtime: kubernetes_in_docker` is set.
pub async fn run_k8s_correctness_test(name: String, config: Config, tctx: TestContext) -> TestResult {
    let started = Instant::now();

    // Wait for the kind cluster to be ready. The runner already waited before acquiring a concurrency
    // slot, so this is a fast-path check — the value should already be Some by the time we get here.
    if let Some(ref rx) = tctx.kind_ready {
        let status: Option<Result<(), String>> = rx.lock().await.borrow().clone();
        match status {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                return make_error_result(
                    name,
                    started,
                    "kind_setup",
                    generic_error!("Kind cluster setup failed: {}", e),
                );
            }
            None => {
                return make_error_result(
                    name,
                    started,
                    "kind_setup",
                    generic_error!("Kind cluster setup did not complete"),
                );
            }
        }
    }

    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            return make_error_result(
                name,
                started,
                "k8s_connect",
                generic_error!("Failed to connect to Kubernetes cluster: {}", e),
            )
        }
    };

    let baseline_ns = format!("airlock-{}-baseline", generate_isolation_id());
    let comparison_ns = format!("airlock-{}-comparison", generate_isolation_id());

    info!(
        "Spawning kind pods for baseline ({}) and comparison ({})...",
        baseline_ns, comparison_ns
    );

    let run_start = Instant::now();
    let (baseline_result, comparison_result) = tokio::join!(
        run_group(
            client.clone(),
            baseline_ns.clone(),
            &config,
            &config.baseline,
            tctx.log_dir().join("baseline")
        ),
        run_group(
            client.clone(),
            comparison_ns.clone(),
            &config,
            &config.comparison,
            tctx.log_dir().join("comparison")
        ),
    );
    let run_duration = run_start.elapsed();

    // Clean up both namespaces regardless of outcome.
    tokio::join!(
        cleanup_namespace(client.clone(), &baseline_ns),
        cleanup_namespace(client.clone(), &comparison_ns),
    );

    let (baseline_data, comparison_data) = match (baseline_result, comparison_result) {
        (Ok(b), Ok(c)) => (b, c),
        (Err(baseline_err), Err(comparison_err)) => {
            let combined = generic_error!(
                "Both groups failed.\n  baseline: {:?}\n  comparison: {:?}",
                baseline_err,
                comparison_err
            );
            return make_error_result(name, started, "collect_data", combined);
        }
        (Err(e), _) | (_, Err(e)) => return make_error_result(name, started, "collect_data", e),
    };

    let analysis_start = Instant::now();
    let traces_options = match config.analysis_mode {
        AnalysisMode::Traces => Some(TracesAnalysisOptions {
            otlp_direct_analysis_mode: config.otlp_direct_analysis_mode,
            additional_span_ignore_fields: config.additional_span_ignore_fields.clone(),
        }),
        _ => None,
    };
    let analysis_runner = AnalysisRunner::new(config.analysis_mode, baseline_data, comparison_data, traces_options);
    let analysis_result = analysis_runner.run_analysis();
    let analysis_duration = analysis_start.elapsed();

    let phase_timings = vec![
        PhaseTiming {
            phase: "run_groups".to_string(),
            duration: run_duration,
        },
        PhaseTiming {
            phase: "analysis".to_string(),
            duration: analysis_duration,
        },
    ];

    match analysis_result {
        Ok(()) => TestResult {
            name,
            passed: true,
            duration: started.elapsed(),
            assertion_results: vec![crate::assertions::AssertionResult {
                name: "telemetry matches".to_string(),
                passed: true,
                message: "No difference detected between baseline and comparison.".to_string(),
                duration: analysis_duration,
            }],
            error: None,
            phase_timings,
            assertion_details: vec![],
        },
        Err((e, details)) => {
            let full_message = format!("{:?}", e);
            let summary = full_message.lines().next().unwrap_or(&full_message).to_string();
            TestResult {
                name,
                passed: false,
                duration: started.elapsed(),
                assertion_results: vec![crate::assertions::AssertionResult {
                    name: "telemetry matches".to_string(),
                    passed: false,
                    message: full_message,
                    duration: analysis_duration,
                }],
                error: Some(summary),
                phase_timings,
                assertion_details: vec![details],
            }
        }
    }
}

const CONTAINER_NAMES: &[&str] = &["datadog-intake", "target", "millstone"];

/// Runs one test group (baseline or comparison) as a multi-container pod.
///
/// All three containers (datadog-intake, target agent, millstone) share the same pod and a single
/// emptyDir volume at `/airlock`. Because they share the pod network namespace, containers
/// communicate over `localhost` rather than container-name DNS.
async fn run_group(
    client: Client, namespace: String, config: &Config, target_config: &crate::correctness::config::TargetConfig,
    log_dir: PathBuf,
) -> Result<CollectedData, GenericError> {
    let millstone_cfg = config.millstone_config();
    let intake_cfg = config.datadog_intake_config();

    // 1. Create the namespace.
    create_namespace(client.clone(), &namespace)
        .await
        .with_error_context(|| format!("Failed to create namespace '{}'", namespace))?;

    // 2. Create ConfigMaps for config files that need to be injected.
    //    - One ConfigMap per unique target-container directory derived from `files:` entries.
    //    - One ConfigMap for the millstone config.
    //    (datadog-intake hardcodes 0.0.0.0:2049 and ignores any config file.)
    let millstone_config_content = std::fs::read_to_string(&millstone_cfg.config_path).with_error_context(|| {
        format!(
            "Failed to read millstone config: {}",
            millstone_cfg.config_path.display()
        )
    })?;

    create_config_map(
        client.clone(),
        &namespace,
        "millstone-config",
        [("config.toml".to_string(), millstone_config_content)].into(),
    )
    .await
    .error_context("Failed to create millstone ConfigMap")?;

    // Parse target files and group by container directory so we create one ConfigMap per mount point.
    let (agent_volumes, agent_volume_mounts) =
        build_agent_config_volumes(client.clone(), &namespace, config, target_config)
            .await
            .error_context("Failed to create agent config ConfigMaps")?;

    // 3. Build and create the pod.
    let pod = build_pod(PodConfig {
        namespace: &namespace,
        intake_image: &intake_cfg.image,
        intake_binary: &intake_cfg
            .binary_path
            .unwrap_or_else(|| "/usr/local/bin/datadog-intake".to_string()),
        target_image: &target_config.image,
        target_env_strs: &target_config.additional_env_vars,
        agent_extra_volumes: agent_volumes,
        agent_extra_mounts: agent_volume_mounts,
        millstone_image: &millstone_cfg.image,
        millstone_binary: &millstone_cfg
            .binary_path
            .unwrap_or_else(|| "/usr/local/bin/millstone".to_string()),
    });

    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    pod_api
        .create(&PostParams::default(), &pod)
        .await
        .map_err(|e| generic_error!("Failed to create pod in namespace '{}': {}", namespace, e))?;

    debug!("Pod created in namespace '{}'. Waiting for Running phase...", namespace);

    // 4. Wait for the pod to reach Running (or Succeeded) phase.
    wait_for_pod_running(&pod_api, POD_NAME, POD_READY_TIMEOUT)
        .await
        .with_error_context(|| format!("Pod in namespace '{}' failed to reach Running phase", namespace))?;

    // 5. Stream container logs to the log directory (best-effort; errors are logged and ignored).
    {
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            warn!("Failed to create log directory '{}': {}", log_dir.display(), e);
        } else {
            for &container in CONTAINER_NAMES {
                let log_path = log_dir.join(format!("{}.log", container));
                tokio::spawn(stream_container_logs(
                    pod_api.clone(),
                    POD_NAME,
                    container.to_string(),
                    log_path,
                ));
            }
        }
    }

    // 6. Start a local port-forward to datadog-intake's port 2049.
    let pf_cancel = CancellationToken::new();
    let local_port = start_port_forward(
        client.clone(),
        namespace.clone(),
        POD_NAME.to_string(),
        2049,
        pf_cancel.clone(),
    )
    .await
    .error_context("Failed to start port-forward to datadog-intake")?;

    debug!(
        "Port-forward established: localhost:{} -> pod/{} port 2049",
        local_port, POD_NAME
    );

    // 7. Wait for the millstone container to exit successfully.
    wait_for_millstone_exit(&pod_api, POD_NAME, MILLSTONE_EXIT_TIMEOUT)
        .await
        .with_error_context(|| format!("Millstone container in namespace '{}' did not exit cleanly", namespace))?;

    debug!(
        "Millstone completed in namespace '{}'. Waiting {}s for flush...",
        namespace, FLUSH_WAIT_SECS
    );

    // 8. Wait for the aggregation flush interval.
    sleep(Duration::from_secs(FLUSH_WAIT_SECS)).await;

    // 9. Collect telemetry from datadog-intake via the forwarded port.
    let data = CollectedData::for_port(local_port).await.with_error_context(|| {
        format!(
            "Failed to collect telemetry from datadog-intake in namespace '{}'",
            namespace
        )
    })?;

    // Port-forward is no longer needed once data is collected.
    pf_cancel.cancel();

    info!("Data collected from namespace '{}'.", namespace);

    Ok(data)
}

// ---------------------------------------------------------------------------
// Pod construction helpers
// ---------------------------------------------------------------------------

struct PodConfig<'a> {
    namespace: &'a str,
    intake_image: &'a str,
    intake_binary: &'a str,
    target_image: &'a str,
    target_env_strs: &'a [String],
    agent_extra_volumes: Vec<Volume>,
    agent_extra_mounts: Vec<VolumeMount>,
    millstone_image: &'a str,
    millstone_binary: &'a str,
}

fn build_pod(cfg: PodConfig<'_>) -> Pod {
    let PodConfig {
        namespace,
        intake_image,
        intake_binary,
        target_image,
        target_env_strs,
        agent_extra_volumes,
        agent_extra_mounts,
        millstone_image,
        millstone_binary,
    } = cfg;
    let mut volumes = vec![
        Volume {
            name: "airlock".to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: "proc".to_string(),
            host_path: Some(HostPathVolumeSource {
                path: "/proc".to_string(),
                type_: None,
            }),
            ..Default::default()
        },
        Volume {
            name: "cgroup".to_string(),
            host_path: Some(HostPathVolumeSource {
                path: "/sys/fs/cgroup".to_string(),
                type_: None,
            }),
            ..Default::default()
        },
        Volume {
            name: "millstone-config".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: Some("millstone-config".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    volumes.extend(agent_extra_volumes);

    let target_env: Vec<EnvVar> = target_env_strs
        .iter()
        .filter_map(|s| match s.split_once('=') {
            Some((k, v)) => Some(EnvVar {
                name: k.to_string(),
                value: Some(v.to_string()),
                ..Default::default()
            }),
            None => {
                warn!("Ignoring malformed env var (no '=' found): {:?}", s);
                None
            }
        })
        .collect();

    let mut target_mounts = vec![
        VolumeMount {
            name: "airlock".to_string(),
            mount_path: "/airlock".to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "proc".to_string(),
            mount_path: "/host/proc".to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "cgroup".to_string(),
            mount_path: "/host/sys/fs/cgroup".to_string(),
            read_only: Some(true),
            ..Default::default()
        },
    ];
    target_mounts.extend(agent_extra_mounts);

    // Millstone waits for the DSD socket before sending, ensuring the agent is ready.
    let millstone_wait_cmd = format!(
        "until [ -S /airlock/metrics.sock ]; do sleep 1; done; exec {} /etc/millstone/config.toml",
        millstone_binary
    );

    Pod {
        metadata: ObjectMeta {
            name: Some(POD_NAME.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            host_pid: Some(true),
            restart_policy: Some("Never".to_string()),
            volumes: Some(volumes),
            containers: vec![
                Container {
                    name: "datadog-intake".to_string(),
                    image: Some(intake_image.to_string()),
                    command: Some(vec![intake_binary.to_string()]),
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    volume_mounts: Some(vec![VolumeMount {
                        name: "airlock".to_string(),
                        mount_path: "/airlock".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                Container {
                    name: "target".to_string(),
                    image: Some(target_image.to_string()),
                    env: Some(target_env),
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    volume_mounts: Some(target_mounts),
                    ..Default::default()
                },
                Container {
                    name: "millstone".to_string(),
                    image: Some(millstone_image.to_string()),
                    command: Some(vec!["/bin/sh".to_string()]),
                    args: Some(vec!["-c".to_string(), millstone_wait_cmd]),
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    volume_mounts: Some(vec![
                        VolumeMount {
                            name: "airlock".to_string(),
                            mount_path: "/airlock".to_string(),
                            ..Default::default()
                        },
                        VolumeMount {
                            name: "millstone-config".to_string(),
                            mount_path: "/etc/millstone".to_string(),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Reads target `files:` entries, creates one ConfigMap per unique container directory,
/// and returns the corresponding Volume and VolumeMount definitions.
async fn build_agent_config_volumes(
    client: Client, namespace: &str, config: &Config, target_config: &TargetConfig,
) -> Result<(Vec<Volume>, Vec<VolumeMount>), GenericError> {
    use std::path::Path;

    // Group files by their container-side directory.
    let mut dir_map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for file_entry in &target_config.files {
        let (host_rel, container_path) = file_entry.split_once(':').ok_or_else(|| {
            generic_error!(
                "Invalid file entry '{}' (expected 'host_path:container_path')",
                file_entry
            )
        })?;

        let host_abs = config.get_canonicalized_config_path(host_rel);
        let content = std::fs::read_to_string(&host_abs)
            .with_error_context(|| format!("Failed to read config file: {}", host_abs.display()))?;

        let container_path = Path::new(container_path);
        let dir = container_path
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/")
            .to_string();
        let filename = container_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config")
            .to_string();

        dir_map.entry(dir).or_default().push((filename, content));
    }

    let mut volumes = Vec::new();
    let mut mounts = Vec::new();

    for (i, (dir, files)) in dir_map.iter().enumerate() {
        let cm_name = format!("agent-config-{}", i);

        let data: BTreeMap<String, String> = files.iter().cloned().collect();
        create_config_map(client.clone(), namespace, &cm_name, data)
            .await
            .with_error_context(|| format!("Failed to create agent ConfigMap '{}'", cm_name))?;

        volumes.push(Volume {
            name: cm_name.clone(),
            config_map: Some(ConfigMapVolumeSource {
                name: Some(cm_name.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });

        // Mount each file individually using subPath so we only overlay the specific file
        // rather than replacing the entire directory. This keeps the rest of the directory
        // writable, which the agent needs to create auth_token and other runtime files.
        for (filename, _) in files {
            mounts.push(VolumeMount {
                name: cm_name.clone(),
                mount_path: format!("{}/{}", dir, filename),
                sub_path: Some(filename.clone()),
                ..Default::default()
            });
        }
    }

    Ok((volumes, mounts))
}

// ---------------------------------------------------------------------------
// Kubernetes API helpers
// ---------------------------------------------------------------------------

async fn create_namespace(client: Client, name: &str) -> Result<(), GenericError> {
    let ns_api: Api<Namespace> = Api::all(client);
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some([("created-by".to_string(), "panoramic-kind".to_string())].into()),
            ..Default::default()
        },
        ..Default::default()
    };
    ns_api
        .create(&PostParams::default(), &ns)
        .await
        .map_err(|e| generic_error!("Failed to create namespace '{}': {}", name, e))?;
    Ok(())
}

async fn create_config_map(
    client: Client, namespace: &str, name: &str, data: BTreeMap<String, String>,
) -> Result<(), GenericError> {
    let cm_api: Api<ConfigMap> = Api::namespaced(client, namespace);
    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };
    cm_api
        .create(&PostParams::default(), &cm)
        .await
        .map_err(|e| generic_error!("Failed to create ConfigMap '{}': {}", name, e))?;
    Ok(())
}

async fn cleanup_namespace(client: Client, name: &str) {
    let ns_api: Api<Namespace> = Api::all(client);
    if let Err(e) = ns_api.delete(name, &DeleteParams::default()).await {
        warn!("Failed to delete namespace '{}': {}", name, e);
    } else {
        debug!("Deleted namespace '{}'.", name);
    }
}

// ---------------------------------------------------------------------------
// Pod status polling
// ---------------------------------------------------------------------------

async fn wait_for_pod_running(pods: &Api<Pod>, pod_name: &str, timeout: Duration) -> Result<(), GenericError> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(generic_error!(
                "Timed out waiting for pod '{}' to reach Running phase",
                pod_name
            ));
        }

        let pod = pods
            .get(pod_name)
            .await
            .map_err(|e| generic_error!("Failed to get pod '{}': {}", pod_name, e))?;

        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());

        match phase {
            Some("Running") | Some("Succeeded") => return Ok(()),
            Some("Failed") => {
                return Err(generic_error!("Pod '{}' entered Failed phase", pod_name));
            }
            _ => {
                sleep(POD_POLL_INTERVAL).await;
            }
        }
    }
}

async fn wait_for_millstone_exit(pods: &Api<Pod>, pod_name: &str, timeout: Duration) -> Result<(), GenericError> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(generic_error!(
                "Timed out waiting for millstone container in pod '{}' to exit",
                pod_name
            ));
        }

        let pod = pods
            .get(pod_name)
            .await
            .map_err(|e| generic_error!("Failed to get pod '{}': {}", pod_name, e))?;

        let millstone_status = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|statuses| statuses.iter().find(|s| s.name == "millstone"));

        if let Some(status) = millstone_status {
            if let Some(state) = &status.state {
                if let Some(terminated) = &state.terminated {
                    if terminated.exit_code != 0 {
                        return Err(generic_error!(
                            "Millstone exited with non-zero exit code: {}",
                            terminated.exit_code
                        ));
                    }
                    return Ok(());
                }
            }
        }

        // Also handle the pod itself succeeding (all containers exited 0).
        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
        if phase == Some("Succeeded") {
            return Ok(());
        }
        if phase == Some("Failed") {
            return Err(generic_error!(
                "Pod '{}' failed before millstone could exit cleanly",
                pod_name
            ));
        }

        sleep(POD_POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Container log streaming
// ---------------------------------------------------------------------------

/// Streams logs from a single container to a file. Runs as a background task; errors are logged
/// and the task exits cleanly when the pod is deleted.
async fn stream_container_logs(pods: Api<Pod>, pod_name: &'static str, container: String, path: PathBuf) {
    let params = LogParams {
        container: Some(container.clone()),
        follow: true,
        ..Default::default()
    };

    let stream = match pods.log_stream(pod_name, &params).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to start log stream for container '{}': {}", container, e);
            return;
        }
    };

    let mut file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to create log file '{}': {}", path.display(), e);
            return;
        }
    };

    use tokio::io::AsyncReadExt as _;
    use tokio_util::compat::FuturesAsyncReadCompatExt as _;
    let mut reader = stream.compat();
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let stripped = strip_ansi_codes(&buf[..n]);
                if let Err(e) = file.write_all(&stripped).await {
                    warn!("Failed to write log chunk for container '{}': {}", container, e);
                    break;
                }
            }
            Err(e) => {
                debug!("Log stream for container '{}' ended: {}", container, e);
                break;
            }
        }
    }
    let _ = file.flush().await;
}

/// Removes ANSI escape sequences (`ESC[...letter`) from a byte slice.
fn strip_ansi_codes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        // ESC [ ... <letter> — skip the entire sequence.
        if input[i] == 0x1b && input.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < input.len() && !input[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1; // skip the terminating letter
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Port-forward
// ---------------------------------------------------------------------------

/// Binds a local TCP listener and forwards each accepted connection to the given pod port.
/// Returns the local ephemeral port number. The forward runs until `cancel` is triggered.
async fn start_port_forward(
    client: Client, namespace: String, pod_name: String, pod_port: u16, cancel: CancellationToken,
) -> Result<u16, GenericError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .error_context("Failed to bind local TCP listener for port-forward")?;
    let local_port = listener
        .local_addr()
        .error_context("Failed to get local address")?
        .port();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => {
                    let Ok((mut conn, _)) = result else { break };
                    let client = client.clone();
                    let namespace = namespace.clone();
                    let pod_name = pod_name.clone();
                    tokio::spawn(async move {
                        let pods: Api<Pod> = Api::namespaced(client, &namespace);
                        let Ok(mut pf) = pods.portforward(&pod_name, &[pod_port]).await else {
                            return;
                        };
                        let Some(mut stream) = pf.take_stream(pod_port) else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut conn, &mut stream).await;
                    });
                }
            }
        }
    });

    Ok(local_port)
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn generate_isolation_id() -> String {
    Alphanumeric.sample_string(&mut rng(), 8).to_lowercase()
}
