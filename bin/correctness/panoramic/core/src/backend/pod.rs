//! Creates pods in one case namespace, with shared storage on a single kind node.
//! Files remain readable through the node after a pod exits. Host requests use TCP port-forwards.

mod manifests;
pub(crate) mod node;
mod observe;

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use k8s_openapi::{
    api::core::v1::{Namespace, Pod, Service},
    apimachinery::pkg::apis::meta::v1::{ObjectMeta, Status},
};
use kube::{
    api::{AttachParams, DeleteParams, PostParams},
    Api, Client,
};
use manifests::{ManifestInput, Manifests, COURIER_PATH};
use node::Node;
use saluki_error::{generic_error, GenericError};
use tokio::{
    io::AsyncReadExt as _,
    net::TcpListener,
    time::{sleep, timeout, Instant},
};

use super::{courier, LateFact, ParticipantRequest, RunBackends, RuntimeParticipant, ScopeRuntime};
use crate::{
    artifact::ArtifactKind,
    endpoint::{ResolvedAddress, RunnerHostPort},
    handle::{ExecCommand, ExecOutput, FileStatus, HandleError, HttpResponse, Primitives},
    layout::Os,
    ledger::{ParticipantRemoval, ResourceLedger},
    observe::ParticipantObservation,
    participant::ProcessSpec,
    plan::ScopeLayout,
    probe::{AttemptLimits, ProbeAnswer},
    recipe::Recipe,
    tasks::CaseTasks,
};

pub(super) const HOST_PID: &str = "host_pid";
pub(super) const CRI_SOCKET: &str = "cri_socket";
const CRI_SOCKET_PATH: &str = "/run/containerd/containerd.sock";

pub(crate) struct PodScope {
    client: Client,
    node: Node,
    namespace: String,
    root: String,
    tasks: CaseTasks,
}

impl RunBackends {
    pub(crate) async fn prepare_pod_scope(
        &self, scope: &ScopeLayout, participants: &[&str], courier: Bytes, ledger: &ResourceLedger, tasks: &CaseTasks,
    ) -> Result<PodScope, GenericError> {
        let connection = self.pod_connection().await?;
        let namespace = scope.id();
        let root = scope.state_dir(Os::Linux);
        ledger.pod_directory(
            connection.node.clone(),
            root.clone(),
            connection.client.clone(),
            namespace.clone(),
        );
        ledger.namespace(connection.client.clone(), namespace.clone());
        let create = async {
            let namespaces: Api<Namespace> = Api::all(connection.client.clone());
            namespaces
                .create(
                    &PostParams::default(),
                    &Namespace {
                        metadata: ObjectMeta {
                            name: Some(namespace.clone()),
                            labels: Some([("created-by".into(), "panoramic-pods".into())].into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .await?;
            for name in participants {
                connection.node.checked_shell(
                    "mkdir -p -- \"$1/exit\" \"$1/log\" \"$1/shared\"; chmod 777 \"$1\" \"$1/exit\" \"$1/log\" \"$1/shared\"",
                    &[&format!("{root}/{name}")], None,
                ).await?;
                connection
                    .node
                    .write(&format!("{root}/{name}/courier"), 0o755, courier.to_vec())
                    .await?;
            }
            Ok::<(), GenericError>(())
        };
        ledger
            .create(&namespace, &tasks.stop, create)
            .await
            .ok_or_else(|| generic_error!("Cancelled while preparing pod scope '{namespace}'"))??;
        Ok(PodScope {
            client: connection.client.clone(),
            node: connection.node.clone(),
            namespace,
            root,
            tasks: tasks.clone(),
        })
    }
}

impl ScopeRuntime for PodScope {
    fn participant(&self, request: ParticipantRequest<'_>) -> Result<Arc<dyn RuntimeParticipant>, String> {
        let Recipe::Pod(recipe) = request.recipe else {
            return Err(format!("Participant '{}' needs a pod recipe", request.name));
        };
        let images = recipe
            .containers
            .iter()
            .map(|container| {
                let role = container.recipe.image;
                if role.kind != (ArtifactKind::Image { os: Os::Linux }) {
                    return Err(format!(
                        "Pod '{}' image role '{}' is not a Linux image",
                        request.name, role.name
                    ));
                }
                request.inputs.image_reference(role).map(str::to_string)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let manifests = Manifests::build(ManifestInput {
            namespace: &self.namespace,
            root: &self.root,
            node: &self.node.container,
            name: request.name,
            recipe: &recipe,
            images: &images,
            options: request.options,
            routes: &request.runner_host_ports,
        })
        .map_err(|error| format!("Participant '{}': {error:#}", request.name))?;
        Ok(Arc::new(PodParticipant {
            client: self.client.clone(),
            node: self.node.clone(),
            namespace: self.namespace.clone(),
            name: request.name.into(),
            root: self.root.clone(),
            primary: recipe.containers[0].name.clone(),
            manifests,
            routes: request.runner_host_ports,
            tasks: self.tasks.clone(),
            specs: request.specs,
        }))
    }
}

struct PodParticipant {
    client: Client,
    node: Node,
    namespace: String,
    name: String,
    root: String,
    primary: String,
    manifests: Manifests,
    routes: Vec<RunnerHostPort>,
    tasks: CaseTasks,
    specs: &'static [ProcessSpec],
}

pub(crate) struct PodRemoval {
    client: Client,
    namespace: String,
    name: String,
}

impl PodRemoval {
    pub(crate) async fn stop(&self, grace: Duration, deadline: Instant) -> Result<(), GenericError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let params = DeleteParams {
            grace_period_seconds: Some(deletion_grace(
                grace,
                deadline.saturating_duration_since(Instant::now()),
            )),
            ..Default::default()
        };
        let service_params = DeleteParams::default();
        let (pod, service) = tokio::join!(
            pods.delete(&self.name, &params),
            services.delete(&self.name, &service_params)
        );
        for result in [pod.map(|_| ()), service.map(|_| ())] {
            match result {
                Ok(()) => {}
                Err(kube::Error::Api(error)) if error.is_not_found() => {}
                Err(error) => return Err(error.into()),
            }
        }
        while pods.get_opt(&self.name).await?.is_some() {
            sleep(Duration::from_millis(200)).await;
        }
        Ok(())
    }
}

#[async_trait]
impl RuntimeParticipant for PodParticipant {
    fn removal(&self) -> ParticipantRemoval {
        ParticipantRemoval::Pod(PodRemoval {
            client: self.client.clone(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        })
    }

    async fn create(&self) -> Result<Vec<(LateFact, String)>, GenericError> {
        let pod = self.manifests.checked_pod(&self.node).await?;
        for file in &self.manifests.files {
            self.node
                .write(&file.node_path, file.mode, file.contents.clone())
                .await?;
        }
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        services.create(&PostParams::default(), &self.manifests.service).await?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        pods.create(&PostParams::default(), &pod).await?;
        Ok(Vec::new())
    }

    async fn start(&self) -> Result<Vec<(LateFact, String)>, GenericError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut last_status = "no status received".to_string();
        let facts = timeout(Duration::from_secs(120), async {
            loop {
                let pod = pods.get(&self.name).await?;
                last_status = startup_status(&pod)?;
                if let Some(facts) = started_facts(&pod, &self.primary)? {
                    return Ok::<_, GenericError>(facts);
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .map_err(|_| {
            generic_error!(
                "Pod '{}/{}' did not start within 120s; last status: {last_status}",
                self.namespace,
                self.name
            )
        })??;
        for route in &self.routes {
            let listener = TcpListener::bind(("127.0.0.1", route.host_port)).await?;
            let pods = pods.clone();
            let name = self.name.clone();
            let port = route.container_port;
            let connections = self.tasks.clone();
            self.tasks.spawn(async move {
                loop {
                    let (mut socket, _) = match listener.accept().await {
                        Ok(connection) => connection,
                        Err(error) => {
                            tracing::warn!(participant = %name, port, %error, "Port-forward listener failed.");
                            break;
                        }
                    };
                    let pods = pods.clone();
                    let name = name.clone();
                    connections.spawn(async move {
                        let result: Result<(), GenericError> = async {
                            let mut forwarder = pods.portforward(&name, &[port]).await?;
                            let mut stream = forwarder
                                .take_stream(port)
                                .ok_or_else(|| generic_error!("Port-forward has no stream for {port}"))?;
                            tokio::io::copy_bidirectional(&mut socket, &mut stream).await?;
                            Ok(())
                        }
                        .await;
                        if let Err(error) = result {
                            let message = "Port-forward connection failed.";
                            tracing::warn!(participant = %name, port, error = %format!("{error:#}"), message);
                        }
                    });
                }
            });
        }
        Ok(facts)
    }

    async fn write_late_files(&self, facts: &super::runtime::LateFacts) -> Result<(), GenericError> {
        let rendered = self.manifests.render_late_files(&self.name, facts)?;
        for (path, contents) in rendered {
            // A single-file hostPath mount pins its starting inode, so write in place instead of renaming over it.
            self.node
                .checked_shell("cat > \"$1\"", &[path], Some(contents.into_bytes()))
                .await?;
        }
        self.node
            .checked_shell(
                ": > \"$1\"",
                &[&format!("{}/{}/files-ready", self.root, self.name)],
                None,
            )
            .await?;
        Ok(())
    }

    async fn observe(self: Arc<Self>, tasks: &CaseTasks, output_dir: &Path) -> ParticipantObservation {
        self.observation(tasks, output_dir).await
    }
}

fn deletion_grace(grace: Duration, remaining: Duration) -> u32 {
    if remaining.is_zero() {
        return 0;
    }
    let grace = grace.min(remaining);
    grace
        .as_secs()
        .saturating_add(u64::from(grace.subsec_nanos() != 0))
        .max(1)
        .try_into()
        .unwrap_or(u32::MAX)
}

fn startup_status(pod: &Pod) -> Result<String, GenericError> {
    let Some(status) = &pod.status else {
        return Ok("phase unknown".into());
    };
    let mut parts = vec![format!("phase {}", status.phase.as_deref().unwrap_or("unknown"))];
    for container in status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten())
    {
        if let Some(waiting) = container.state.as_ref().and_then(|state| state.waiting.as_ref()) {
            let reason = waiting.reason.as_deref().unwrap_or("unknown reason");
            let diagnostic = format!(
                "container '{}': {reason}: {}",
                container.name,
                waiting.message.as_deref().unwrap_or("no message")
            );
            if matches!(
                reason,
                "ErrImageNeverPull" | "CreateContainerConfigError" | "CreateContainerError" | "InvalidImageName"
            ) {
                return Err(generic_error!("Pod cannot start: {diagnostic}"));
            }
            parts.push(diagnostic);
        }
    }
    Ok(parts.join("; "))
}

fn started_facts(pod: &Pod, primary: &str) -> Result<Option<Vec<(LateFact, String)>>, GenericError> {
    let Some(status) = &pod.status else {
        return Ok(None);
    };
    if !matches!(status.phase.as_deref(), Some("Running" | "Succeeded" | "Failed")) {
        return Ok(None);
    }
    let Some(statuses) = status.container_statuses.as_ref() else {
        return Ok(None);
    };
    if statuses.len() != pod.spec.as_ref().map_or(0, |spec| spec.containers.len())
        || statuses.iter().any(|status| status.container_id.is_none())
    {
        return Ok(None);
    }
    let Some(container_id) = statuses
        .iter()
        .find(|status| status.name == primary)
        .and_then(|status| status.container_id.as_deref())
    else {
        return Ok(None);
    };
    let container_id = container_id
        .strip_prefix("containerd://")
        .filter(|id| !id.is_empty())
        .ok_or_else(|| generic_error!("Pod container has an invalid containerd ID '{container_id}'"))?;
    let Some(ip) = status.pod_ip.as_ref() else {
        return Ok(None);
    };
    let uid = pod
        .metadata
        .uid
        .as_ref()
        .ok_or_else(|| generic_error!("Started pod has no UID"))?;
    Ok(Some(vec![
        (LateFact::ContainerId, container_id.into()),
        (LateFact::Ip, ip.clone()),
        (LateFact::PodUid, uid.clone()),
    ]))
}

impl PodParticipant {
    fn node_path(&self, path: &str) -> Result<String, HandleError> {
        manifests::check_path(path).map_err(|error| HandleError::Harness {
            message: error.to_string(),
        })?;
        if let Some(path) = self.manifests.readable_files.get(path) {
            return Ok(path.clone());
        }
        path.strip_prefix("/airlock/")
            .map(|relative| format!("{}/{relative}", self.root))
            .ok_or_else(|| HandleError::Harness {
                message: format!("Pod file '{path}' is outside shared storage and declared recipe files"),
            })
    }

    async fn exec_inner(&self, command: ExecCommand) -> Result<ExecOutput, GenericError> {
        if command.stdin.is_some() {
            return Err(generic_error!(
                "Pod exec does not support stdin; place files through node storage"
            ));
        }
        if command.argv.is_empty()
            || command
                .env
                .iter()
                .any(|(key, _)| key.is_empty() || key.contains(['=', '\0']))
        {
            return Err(generic_error!(
                "Pod exec requires a command and nonempty environment keys without '=' or NUL"
            ));
        }
        let mut argv = Vec::new();
        if !command.env.is_empty() {
            argv.extend(["env".to_string(), "--".to_string()]);
            argv.extend(command.env.into_iter().map(|(key, value)| format!("{key}={value}")));
        }
        argv.extend(command.argv);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut process = pods
            .exec(
                &self.name,
                argv,
                &AttachParams::default()
                    .container(&self.primary)
                    .stdout(true)
                    .stderr(true)
                    .stdin(false),
            )
            .await?;
        let status = process
            .take_status()
            .ok_or_else(|| generic_error!("Pod exec has no status channel"))?;
        let mut stdout = process
            .stdout()
            .ok_or_else(|| generic_error!("Pod exec has no stdout"))?;
        let mut stderr = process
            .stderr()
            .ok_or_else(|| generic_error!("Pod exec has no stderr"))?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
        process.join().await?;
        let status = status
            .await
            .ok_or_else(|| generic_error!("Pod exec ended without a status"))?;
        exec_result(status, out, err)
    }
}

fn exec_result(status: Status, stdout: Vec<u8>, mut stderr: Vec<u8>) -> Result<ExecOutput, GenericError> {
    let exit_code = if status.status.as_deref() == Some("Success") {
        0
    } else if status.reason.as_deref() == Some("NonZeroExitCode") {
        status
            .details
            .as_ref()
            .and_then(|details| details.causes.as_ref())
            .and_then(|causes| causes.iter().find(|cause| cause.reason.as_deref() == Some("ExitCode")))
            .and_then(|cause| cause.message.as_deref())
            .and_then(|code| code.parse::<i64>().ok())
            .ok_or_else(|| generic_error!("Pod exec status lacks its nonzero exit code"))?
    } else if status.status.as_deref() == Some("Failure")
        && status
            .message
            .as_deref()
            .is_some_and(|message| message.contains("executable file not found"))
    {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(status.message.as_ref().unwrap().as_bytes());
        127
    } else {
        return Err(generic_error!(
            "Pod exec failed: {}",
            status.message.unwrap_or_default()
        ));
    };
    Ok(ExecOutput {
        stdout,
        stderr,
        exit_code,
    })
}

#[async_trait]
impl Primitives for PodParticipant {
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError> {
        timeout(limit, self.exec_inner(command))
            .await
            .map_err(|_| HandleError::Harness {
                message: format!("Abandoned pod exec after {limit:?}; the command may still be running"),
            })?
            .map_err(|error| HandleError::Harness {
                message: format!("Pod exec failed: {error:#}"),
            })
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        self.node
            .read(&self.node_path(path)?)
            .await
            .map_err(|error| HandleError::Harness {
                message: format!("Pod file read failed: {error:#}"),
            })
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStatus>, HandleError> {
        self.node
            .stat(&self.node_path(path)?)
            .await
            .map_err(|error| HandleError::Harness {
                message: format!("Pod file stat failed: {error:#}"),
            })
    }

    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        courier::probe(|command| self.exec_inner(command), COURIER_PATH, address, limits).await
    }

    async fn send(
        &self, _address: &ResolvedAddress, _payload: &[u8], _limits: &AttemptLimits,
    ) -> Result<String, HandleError> {
        Err(HandleError::Harness {
            message: "Pod payload delivery requires a sending participant".into(),
        })
    }

    async fn http(
        &self, address: &ResolvedAddress, path: &str, headers_only: bool, limits: &AttemptLimits,
    ) -> Result<HttpResponse, HandleError> {
        courier::http(
            |command| self.exec_inner(command),
            COURIER_PATH,
            address,
            path,
            headers_only,
            limits,
        )
        .await
    }
}

#[cfg(test)]
mod kind_tests;
#[cfg(test)]
mod tests;
