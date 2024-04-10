use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client};
use tokio::fs;
use tracing::debug;

use super::HostnameProvider;

/// A hostname provider that queries the Kubernetes API to get the hostname.
///
/// This provider will only work when running in a Kubernetes environment. The hostname returned will be the name of the
/// node where this provider is running.
///
/// Will attempt to read API credentials and connection information from the local kubeconfig first (specified either by
/// `$KUBECONFIG` or the default location `~/.kube/config`), and then from the in-cluster service environment variables
/// (e.g. `KUBERNETES_SERVICE_HOST`).
pub struct KubernetesHostnameProvider;

#[async_trait]
impl HostnameProvider for KubernetesHostnameProvider {
    async fn get_hostname(&self) -> Option<String> {
        let client = match Client::try_default().await {
            Ok(client) => client,
            Err(_) => {
                debug!("Failed to create Kuberenetes API client. Likely not running in Kubernetes.");
                return None;
            }
        };

        match get_self_pod_name().await {
            Some(pod_name) => {
                let namespace = get_namespace().await;
                let pods: Api<Pod> = Api::namespaced(client, &namespace);
                let self_pod = match pods.get(&pod_name).await {
                    Ok(pod) => pod,
                    Err(e) => {
                        debug!(error = %e, "Failed to get self pod.");
                        return None;
                    }
                };
                self_pod.spec.and_then(|spec| spec.node_name)
            }
            None => None,
        }
    }
}

#[cfg(target_os = "linux")]
async fn get_self_pod_name() -> Option<String> {
    use super::util::is_running_in_host_uts_namespace;

    if let Ok(pod_name) = std::env::var("DD_POD_NAME") {
        return Some(pod_name);
    }

    if is_running_in_host_uts_namespace().await {
        debug!("DD_POD_NAME is not set and process is running in host UTS namespace. Self pod name cannot be determined reliably.");
        return None;
    }

    get_os_hostname()
}

#[cfg(not(target_os = "linux"))]
fn get_self_pod_name() -> Option<String> {
    if let Ok(pod_name) = std::env::var("DD_POD_NAME") {
        return Some(pod_name);
    }

    get_os_hostname()
}

fn get_os_hostname() -> Option<String> {
    match hostname::get() {
        Ok(hostname) => Some(hostname.to_string_lossy().to_string()),
        Err(e) => {
            debug!(error = %e, "Failed to query hostname.");
            None
        }
    }
}

async fn get_namespace() -> String {
    const DEFAULT_NAMESPACE: &str = "default";

    let namespace_path = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
    let namespace = fs::read_to_string(namespace_path)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| DEFAULT_NAMESPACE.to_string());

    if namespace.is_empty() {
        DEFAULT_NAMESPACE.to_string()
    } else {
        namespace
    }
}
