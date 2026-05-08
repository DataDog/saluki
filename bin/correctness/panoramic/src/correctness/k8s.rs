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
    api::{Api, AttachParams, DeleteParams, LogParams, PostParams},
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
const FLUSH_WAIT: Duration = Duration::from_secs(30);
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
        let status: Option<Result<(), String>> = rx.borrow().clone();
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

    let run_id = generate_isolation_id();
    let baseline_ns = format!("airlock-{}-baseline", run_id);
    let comparison_ns = format!("airlock-{}-comparison", run_id);
    let millstone_ns = format!("airlock-{}-millstone", run_id);

    // Socket directories are created on the kind node via DirectoryOrCreate HostPath volumes —
    // no manual setup required. Both agent pods and the shared millstone pod mount these paths,
    // giving millstone access to both agent sockets without any cross-pod coordination.
    //
    // Note: these directories are not deleted after the test. Kind clusters are ephemeral in CI
    // and the directories are small, so this is acceptable for now.
    let baseline_socket_dir = format!("/tmp/saluki-correctness/{}/baseline", run_id);
    let comparison_socket_dir = format!("/tmp/saluki-correctness/{}/comparison", run_id);

    let millstone_cfg = config.millstone_config();
    let millstone_template = match std::fs::read_to_string(&millstone_cfg.config_path).with_error_context(|| {
        format!(
            "Failed to read millstone config: {}",
            millstone_cfg.config_path.display()
        )
    }) {
        Ok(t) => t,
        Err(e) => return make_error_result(name, started, "read_millstone_config", e),
    };
    let millstone_binary = millstone_cfg
        .binary_path
        .unwrap_or_else(|| "/usr/local/bin/millstone".to_string());

    info!(
        "Spawning kind pods: baseline ({}), comparison ({}), millstone ({})...",
        baseline_ns, comparison_ns, millstone_ns
    );

    let run_start = Instant::now();

    // Phase 1: Start all three pods in parallel. Agent pods use HostPath airlocks. The shared
    // millstone pod mounts both airlock directories and waits for both configs + both sockets.
    let (baseline_prep, comparison_prep, millstone_prep) = tokio::join!(
        prepare_agent_group(
            client.clone(),
            baseline_ns.clone(),
            &config,
            &config.baseline,
            &baseline_socket_dir,
            tctx.log_dir().join("baseline"),
        ),
        prepare_agent_group(
            client.clone(),
            comparison_ns.clone(),
            &config,
            &config.comparison,
            &comparison_socket_dir,
            tctx.log_dir().join("comparison"),
        ),
        prepare_millstone_group(
            client.clone(),
            millstone_ns.clone(),
            &millstone_cfg.image,
            &millstone_binary,
            &baseline_socket_dir,
            &comparison_socket_dir,
            tctx.log_dir().join("millstone"),
        ),
    );

    // Clean up all three namespaces regardless of outcome.
    let cleanup = |err: GenericError| {
        let client = client.clone();
        let (bns, cns, mns) = (baseline_ns.clone(), comparison_ns.clone(), millstone_ns.clone());
        async move {
            tokio::join!(
                cleanup_namespace(client.clone(), &bns),
                cleanup_namespace(client.clone(), &cns),
                cleanup_namespace(client.clone(), &mns),
            );
            err
        }
    };

    if let Err(e) = baseline_prep {
        return make_error_result(name, started, "agent_pod_start", cleanup(e).await);
    }
    if let Err(e) = comparison_prep {
        return make_error_result(name, started, "agent_pod_start", cleanup(e).await);
    }
    if let Err(e) = millstone_prep {
        return make_error_result(name, started, "millstone_pod_start", cleanup(e).await);
    }

    // Phase 2: All pods are Running. Gather origin data from the millstone pod and write both
    // configs. Both runs use the same container ID and pod UID so both agents resolve identical
    // enrichment — even though the origin actually belongs to the millstone pod, not the agents.
    let millstone_pod_api: Api<Pod> = Api::namespaced(client.clone(), &millstone_ns);

    let origin_data = match gather_pod_origin_data(&millstone_pod_api, POD_NAME).await {
        Ok(d) => d,
        Err(e) => return make_error_result(name, started, "gather_origin_data", cleanup(e).await),
    };

    debug!(
        "Millstone pod origin data: pod_uid={}, container_id={}",
        origin_data.pod_uid, origin_data.millstone_container_id
    );

    let config_content = substitute_origin_placeholders(&millstone_template, &origin_data);
    let baseline_config =
        make_millstone_config_for_target(&config_content, "unixgram:///baseline-airlock/metrics.sock");
    let comparison_config =
        make_millstone_config_for_target(&config_content, "unixgram:///comparison-airlock/metrics.sock");

    let write_result = tokio::try_join!(
        exec_write_file(
            client.clone(),
            &millstone_ns,
            POD_NAME,
            "millstone",
            "/etc/millstone/baseline.toml",
            &baseline_config,
        ),
        exec_write_file(
            client.clone(),
            &millstone_ns,
            POD_NAME,
            "millstone",
            "/etc/millstone/comparison.toml",
            &comparison_config,
        ),
    );
    if let Err(e) = write_result {
        return make_error_result(name, started, "write_millstone_configs", cleanup(e).await);
    }

    // Phase 3: Both agent pods are ready; start port-forwards for data collection.
    let baseline_pf_cancel = CancellationToken::new();
    let comparison_pf_cancel = CancellationToken::new();
    let (baseline_port, comparison_port) = match tokio::try_join!(
        start_port_forward(
            client.clone(),
            baseline_ns.clone(),
            POD_NAME.to_string(),
            2049,
            baseline_pf_cancel.clone()
        ),
        start_port_forward(
            client.clone(),
            comparison_ns.clone(),
            POD_NAME.to_string(),
            2049,
            comparison_pf_cancel.clone()
        ),
    ) {
        Ok(ports) => ports,
        Err(e) => return make_error_result(name, started, "port_forward", cleanup(e).await),
    };

    // Phase 4: Wait for millstone to finish both parallel runs, then flush.
    if let Err(e) = wait_for_millstone_exit(&millstone_pod_api, POD_NAME, MILLSTONE_EXIT_TIMEOUT).await {
        baseline_pf_cancel.cancel();
        comparison_pf_cancel.cancel();
        return make_error_result(name, started, "millstone_exit", cleanup(e).await);
    }

    debug!("Millstone completed. Waiting {:?} for flush...", FLUSH_WAIT);
    sleep(FLUSH_WAIT).await;

    // Phase 5: Collect data from both agent pods in parallel.
    let (baseline_result, comparison_result) = tokio::join!(
        CollectedData::for_port(baseline_port),
        CollectedData::for_port(comparison_port),
    );

    baseline_pf_cancel.cancel();
    comparison_pf_cancel.cancel();

    let run_duration = run_start.elapsed();

    // Clean up all three namespaces now that data is collected.
    tokio::join!(
        cleanup_namespace(client.clone(), &baseline_ns),
        cleanup_namespace(client.clone(), &comparison_ns),
        cleanup_namespace(client.clone(), &millstone_ns),
    );

    let (baseline_data, comparison_data) = match (baseline_result, comparison_result) {
        (Ok(b), Ok(c)) => (b, c),
        (Err(baseline_err), Err(comparison_err)) => {
            return make_error_result(
                name,
                started,
                "collect_data",
                generic_error!(
                    "Both groups failed to collect data.\n  baseline: {:?}\n  comparison: {:?}",
                    baseline_err,
                    comparison_err
                ),
            );
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

const AGENT_CONTAINER_NAMES: &[&str] = &["datadog-intake", "target"];
const MILLSTONE_CONTAINER_NAMES: &[&str] = &["millstone"];

/// Creates the namespace and agent pod (datadog-intake + target agent) for one test group,
/// waits for the pod to reach Running, and starts background log streaming.
///
/// The airlock volume is a HostPath directory on the kind node so the shared millstone pod can
/// mount both groups' socket directories simultaneously.
async fn prepare_agent_group(
    client: Client, namespace: String, config: &Config, target_config: &crate::correctness::config::TargetConfig,
    socket_dir: &str, log_dir: PathBuf,
) -> Result<(), GenericError> {
    let intake_cfg = config.datadog_intake_config();

    create_namespace(client.clone(), &namespace)
        .await
        .with_error_context(|| format!("Failed to create namespace '{}'", namespace))?;

    let (agent_volumes, agent_volume_mounts) =
        build_agent_config_volumes(client.clone(), &namespace, config, target_config)
            .await
            .error_context("Failed to create agent config ConfigMaps")?;

    let pod = build_agent_pod(AgentPodConfig {
        namespace: &namespace,
        intake_image: &intake_cfg.image,
        intake_binary: &intake_cfg
            .binary_path
            .unwrap_or_else(|| "/usr/local/bin/datadog-intake".to_string()),
        target_image: &target_config.image,
        target_env_strs: &target_config.additional_env_vars,
        agent_extra_volumes: agent_volumes,
        agent_extra_mounts: agent_volume_mounts,
        socket_dir,
    });

    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    pod_api
        .create(&PostParams::default(), &pod)
        .await
        .map_err(|e| generic_error!("Failed to create agent pod in namespace '{}': {}", namespace, e))?;

    wait_for_pod_running(&pod_api, POD_NAME, POD_READY_TIMEOUT)
        .await
        .with_error_context(|| format!("Agent pod in namespace '{}' failed to reach Running phase", namespace))?;

    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        warn!("Failed to create log directory '{}': {}", log_dir.display(), e);
    } else {
        for &container in AGENT_CONTAINER_NAMES {
            tokio::spawn(stream_container_logs(
                pod_api.clone(),
                POD_NAME,
                container.to_string(),
                log_dir.join(format!("{}.log", container)),
            ));
        }
    }

    debug!("Agent pod running in namespace '{}'.", namespace);
    Ok(())
}

/// Creates the namespace and shared millstone pod, waits for it to reach Running, and starts
/// background log streaming.
///
/// The millstone pod mounts both agent socket directories and waits for both config files and
/// both sockets before launching two parallel millstone processes — one targeting each agent.
async fn prepare_millstone_group(
    client: Client, namespace: String, millstone_image: &str, millstone_binary: &str, baseline_socket_dir: &str,
    comparison_socket_dir: &str, log_dir: PathBuf,
) -> Result<(), GenericError> {
    create_namespace(client.clone(), &namespace)
        .await
        .with_error_context(|| format!("Failed to create millstone namespace '{}'", namespace))?;

    let pod = build_millstone_pod(MillstonePodConfig {
        namespace: &namespace,
        millstone_image,
        millstone_binary,
        baseline_socket_dir,
        comparison_socket_dir,
    });

    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    pod_api
        .create(&PostParams::default(), &pod)
        .await
        .map_err(|e| generic_error!("Failed to create millstone pod in namespace '{}': {}", namespace, e))?;

    wait_for_pod_running(&pod_api, POD_NAME, POD_READY_TIMEOUT)
        .await
        .with_error_context(|| {
            format!(
                "Millstone pod in namespace '{}' failed to reach Running phase",
                namespace
            )
        })?;

    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        warn!("Failed to create log directory '{}': {}", log_dir.display(), e);
    } else {
        for &container in MILLSTONE_CONTAINER_NAMES {
            tokio::spawn(stream_container_logs(
                pod_api.clone(),
                POD_NAME,
                container.to_string(),
                log_dir.join(format!("{}.log", container)),
            ));
        }
    }

    debug!("Millstone pod running in namespace '{}'.", namespace);
    Ok(())
}

/// Replaces the `target:` field value in a millstone config YAML with the given socket path.
///
/// The convention for kind tests is that the on-disk millstone.yaml uses
/// `target: "unixgram:///airlock/metrics.sock"` as a placeholder. The harness generates two
/// configs from the same template — one per agent — by substituting the target path here.
fn make_millstone_config_for_target(template: &str, target: &str) -> String {
    let mut result = String::with_capacity(template.len());
    for line in template.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("target:") {
            let indent = &line[..line.len() - trimmed.len()];
            result.push_str(&format!("{}target: \"{}\"\n", indent, target));
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Pod construction helpers
// ---------------------------------------------------------------------------

struct AgentPodConfig<'a> {
    namespace: &'a str,
    intake_image: &'a str,
    intake_binary: &'a str,
    target_image: &'a str,
    target_env_strs: &'a [String],
    agent_extra_volumes: Vec<Volume>,
    agent_extra_mounts: Vec<VolumeMount>,
    /// HostPath directory on the kind node where the agent creates its DSD socket.
    /// Used by the shared millstone pod to reach this agent's socket.
    socket_dir: &'a str,
}

/// Builds the agent pod spec (datadog-intake + target agent, no millstone).
///
/// The airlock volume is a HostPath directory rather than an EmptyDir so the shared millstone pod
/// can mount both agents' socket directories simultaneously.
fn build_agent_pod(cfg: AgentPodConfig<'_>) -> Pod {
    let AgentPodConfig {
        namespace,
        intake_image,
        intake_binary,
        target_image,
        target_env_strs,
        agent_extra_volumes,
        agent_extra_mounts,
        socket_dir,
    } = cfg;

    let mut volumes = vec![
        // HostPath so the shared millstone pod can see this agent's socket directory.
        // DirectoryOrCreate means the kubelet creates the path on the node if absent.
        Volume {
            name: "airlock".to_string(),
            host_path: Some(HostPathVolumeSource {
                path: socket_dir.to_string(),
                type_: Some("DirectoryOrCreate".to_string()),
            }),
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
        // The containerd socket from the kind node is mounted so the agent can use the containerd
        // gRPC API to resolve sender PIDs to container IDs for UDS origin detection. Without it,
        // cgroup-based PID resolution fails: the container's private cgroup namespace makes
        // /proc/<pid>/cgroup paths useless for identifying containers. Both ADP (cri_socket_path)
        // and the core agent skip containerd auto-detection when running inside Docker, so the
        // socket path must also be configured explicitly in datadog.yaml.
        Volume {
            name: "containerd-socket".to_string(),
            host_path: Some(HostPathVolumeSource {
                path: "/run/containerd/containerd.sock".to_string(),
                type_: Some("Socket".to_string()),
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
        VolumeMount {
            name: "containerd-socket".to_string(),
            mount_path: "/var/run/containerd/containerd.sock".to_string(),
            read_only: Some(true),
            ..Default::default()
        },
    ];
    target_mounts.extend(agent_extra_mounts);

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
            ],
            ..Default::default()
        }),
        ..Default::default()
    }
}

struct MillstonePodConfig<'a> {
    namespace: &'a str,
    millstone_image: &'a str,
    millstone_binary: &'a str,
    /// HostPath directory for the baseline agent's socket.
    baseline_socket_dir: &'a str,
    /// HostPath directory for the comparison agent's socket.
    comparison_socket_dir: &'a str,
}

/// Builds the shared millstone pod spec.
///
/// The pod mounts both agents' socket directories (as `/baseline-airlock` and
/// `/comparison-airlock`) and an EmptyDir for the two config files the harness writes after
/// the pod is Running. The wait script blocks until both configs and both sockets are present,
/// then launches both millstone processes in parallel and waits for both to exit.
fn build_millstone_pod(cfg: MillstonePodConfig<'_>) -> Pod {
    let MillstonePodConfig {
        namespace,
        millstone_image,
        millstone_binary,
        baseline_socket_dir,
        comparison_socket_dir,
    } = cfg;

    // Capture both child PIDs and propagate a non-zero exit if either run fails. Plain `wait`
    // with no arguments exits with the status of the last reaped job, which would mask a failure
    // from the first child if the second exits cleanly.
    let wait_cmd = format!(
        "until [ -f /etc/millstone/baseline.toml ] && \
               [ -f /etc/millstone/comparison.toml ] && \
               [ -S /baseline-airlock/metrics.sock ] && \
               [ -S /comparison-airlock/metrics.sock ]; do sleep 1; done; \
         exec sh -c '{bin} /etc/millstone/baseline.toml & P1=$!; \
                      {bin} /etc/millstone/comparison.toml & P2=$!; \
                      wait $P1; R1=$?; wait $P2; R2=$?; exit $((R1 | R2))'",
        bin = millstone_binary
    );

    Pod {
        metadata: ObjectMeta {
            name: Some(POD_NAME.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            volumes: Some(vec![
                Volume {
                    name: "baseline-airlock".to_string(),
                    host_path: Some(HostPathVolumeSource {
                        path: baseline_socket_dir.to_string(),
                        type_: Some("DirectoryOrCreate".to_string()),
                    }),
                    ..Default::default()
                },
                Volume {
                    name: "comparison-airlock".to_string(),
                    host_path: Some(HostPathVolumeSource {
                        path: comparison_socket_dir.to_string(),
                        type_: Some("DirectoryOrCreate".to_string()),
                    }),
                    ..Default::default()
                },
                // EmptyDir for the two config files written by the harness via exec.
                Volume {
                    name: "millstone-config".to_string(),
                    empty_dir: Some(EmptyDirVolumeSource::default()),
                    ..Default::default()
                },
            ]),
            containers: vec![Container {
                name: "millstone".to_string(),
                image: Some(millstone_image.to_string()),
                command: Some(vec!["/bin/sh".to_string()]),
                args: Some(vec!["-c".to_string(), wait_cmd]),
                image_pull_policy: Some("IfNotPresent".to_string()),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: "baseline-airlock".to_string(),
                        mount_path: "/baseline-airlock".to_string(),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "comparison-airlock".to_string(),
                        mount_path: "/comparison-airlock".to_string(),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "millstone-config".to_string(),
                        mount_path: "/etc/millstone".to_string(),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }],
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

use crate::utils::strip_ansi_codes;

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
// Pod origin data
// ---------------------------------------------------------------------------

/// Runtime origin data extracted from a running pod, used to substitute placeholders in the
/// millstone config template before writing it into the pod.
struct PodOriginData {
    /// Kubernetes UID of the pod (e.g. `"a1b2c3d4-..."`).
    pod_uid: String,
    /// Container ID of the millstone container, with the `containerd://` scheme stripped
    /// (e.g. `"abc123..."`). This is the ID the agent resolves via the containerd socket.
    millstone_container_id: String,
}

/// Queries the running pod to extract its UID and the millstone container's runtime container ID.
///
/// Must be called after the pod has reached Running phase.
async fn gather_pod_origin_data(pods: &Api<Pod>, pod_name: &str) -> Result<PodOriginData, GenericError> {
    let pod = pods
        .get(pod_name)
        .await
        .map_err(|e| generic_error!("Failed to get pod '{}': {}", pod_name, e))?;

    let pod_uid = pod
        .metadata
        .uid
        .ok_or_else(|| generic_error!("Pod '{}' has no UID", pod_name))?;

    let raw_container_id = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == "millstone"))
        .and_then(|c| c.container_id.as_deref())
        .ok_or_else(|| {
            generic_error!(
                "Could not find container ID for 'millstone' container in pod '{}'",
                pod_name
            )
        })?;

    const CONTAINERD_PREFIX: &str = "containerd://";
    let millstone_container_id = raw_container_id
        .strip_prefix(CONTAINERD_PREFIX)
        .ok_or_else(|| {
            generic_error!(
                "Container ID '{}' for 'millstone' in pod '{}' does not have the expected '{}' prefix",
                raw_container_id,
                pod_name,
                CONTAINERD_PREFIX
            )
        })?
        .to_string();

    Ok(PodOriginData {
        pod_uid,
        millstone_container_id,
    })
}

/// Replaces `{{POD_UID}}` and `{{CONTAINER_ID}}` placeholder tokens in the millstone config
/// template with the real runtime values from the pod.
///
/// Tests that do not use these placeholders are unaffected: the substitution is a no-op.
fn substitute_origin_placeholders(template: &str, origin: &PodOriginData) -> String {
    template
        .replace("{{POD_UID}}", &origin.pod_uid)
        .replace("{{CONTAINER_ID}}", &origin.millstone_container_id)
}

/// Writes `content` to `path` inside `container` by running `cat > path` via the k8s exec API
/// and streaming the content to its stdin.
async fn exec_write_file(
    client: Client, namespace: &str, pod_name: &str, container: &str, path: &str, content: &str,
) -> Result<(), GenericError> {
    use tokio::io::AsyncWriteExt as _;

    let pods: Api<Pod> = Api::namespaced(client, namespace);
    let ap = AttachParams::default()
        .container(container)
        .stdin(true)
        .stdout(false)
        .stderr(false)
        .tty(false);

    let mut proc = pods
        .exec(pod_name, vec!["sh", "-c", &format!("cat > '{}'", path)], &ap)
        .await
        .with_error_context(|| {
            format!(
                "Failed to exec into container '{}' in pod '{}/{}'",
                container, namespace, pod_name
            )
        })?;

    let mut stdin = proc
        .stdin()
        .ok_or_else(|| generic_error!("Exec stdin not available for container '{}'", container))?;

    stdin
        .write_all(content.as_bytes())
        .await
        .error_context("Failed to write config content to exec stdin")?;
    stdin.shutdown().await.error_context("Failed to close exec stdin")?;

    proc.join()
        .await
        .map_err(|e| generic_error!("Exec command failed: {}", e))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn generate_isolation_id() -> String {
    Alphanumeric.sample_string(&mut rng(), 8).to_lowercase()
}
