use std::collections::BTreeSet;

use futures::future;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::process::Command;
use tracing::{debug, info, warn};

pub const DEFAULT_CLUSTER_NAME: &str = "saluki-correctness";

/// Manages the lifecycle of a kind cluster used for kind-runtime correctness tests.
///
/// On construction, ensures a cluster is running (creating one if needed) and loads
/// all required images into it. On teardown, optionally deletes the cluster.
pub struct KindLifecycle {
    cluster_name: String,
}

impl KindLifecycle {
    /// Ensures a kind cluster with the given name is running, then pulls and loads
    /// all required images into it. Creates the cluster if it does not already exist.
    pub async fn ensure(cluster_name: String, images: Vec<String>) -> Result<Self, GenericError> {
        check_kind_installed().await?;

        if cluster_exists(&cluster_name).await? {
            info!("Reusing existing kind cluster '{}'.", cluster_name);
        } else {
            info!("Creating kind cluster '{}'...", cluster_name);
            create_cluster(&cluster_name).await?;
        }

        let unique_images: Vec<_> = images.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        if !unique_images.is_empty() {
            load_images(&cluster_name, &unique_images).await?;
        }

        Ok(Self { cluster_name })
    }

    /// Deletes the kind cluster.
    pub async fn teardown(self) {
        info!("Deleting kind cluster '{}'...", self.cluster_name);
        if let Err(e) = delete_cluster(&self.cluster_name).await {
            warn!("Failed to delete kind cluster '{}': {}", self.cluster_name, e);
        }
    }
}

async fn check_kind_installed() -> Result<(), GenericError> {
    Command::new("kind")
        .arg("version")
        .output()
        .await
        .map(|_| ())
        .map_err(|_| {
            generic_error!(
                "'kind' not found in PATH. Install it from https://kind.sigs.k8s.io/docs/user/quick-start/#installation"
            )
        })
}

async fn cluster_exists(name: &str) -> Result<bool, GenericError> {
    let output = Command::new("kind")
        .args(["get", "clusters"])
        .output()
        .await
        .error_context("Failed to list kind clusters")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|l| l.trim() == name))
}

async fn create_cluster(name: &str) -> Result<(), GenericError> {
    let status = Command::new("kind")
        .args(["create", "cluster", "--name", name, "--wait", "120s"])
        .status()
        .await
        .error_context("Failed to spawn kind create cluster")?;
    if !status.success() {
        return Err(generic_error!("'kind create cluster' exited with status: {}", status));
    }
    Ok(())
}

async fn delete_cluster(name: &str) -> Result<(), GenericError> {
    let status = Command::new("kind")
        .args(["delete", "cluster", "--name", name])
        .status()
        .await
        .error_context("Failed to spawn kind delete cluster")?;
    if !status.success() {
        return Err(generic_error!("'kind delete cluster' exited with status: {}", status));
    }
    Ok(())
}

async fn load_image(cluster_name: &str, image: &str) -> Result<(), GenericError> {
    let output = Command::new("kind")
        .args(["load", "docker-image", image, "--name", cluster_name])
        .output()
        .await
        .error_context("Failed to spawn kind load docker-image")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(generic_error!(
            "'kind load docker-image {}' exited with non-zero status\nstdout: {}\nstderr: {}",
            image,
            stdout.trim(),
            stderr.trim(),
        ));
    }
    Ok(())
}

/// Returns true if the image is already present in the local Docker daemon.
async fn image_present_locally(image: &str) -> Result<bool, GenericError> {
    let status = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()
        .await
        .error_context("Failed to spawn docker image inspect")?;
    Ok(status.status.success())
}

async fn pull_image(image: &str) -> Result<(), GenericError> {
    let status = Command::new("docker")
        .args(["pull", image])
        .status()
        .await
        .error_context("Failed to spawn docker pull")?;
    if !status.success() {
        return Err(generic_error!("'docker pull {}' failed", image));
    }
    Ok(())
}

async fn ensure_image_present(image: &str) -> Result<(), GenericError> {
    if image_present_locally(image).await? {
        debug!("Image '{}' already present in local Docker daemon.", image);
        return Ok(());
    }
    info!("Image '{}' not found locally, pulling...", image);
    pull_image(image)
        .await
        .with_error_context(|| format!("Image '{}' is not available locally and could not be pulled", image))
}

async fn load_images(cluster_name: &str, images: &[String]) -> Result<(), GenericError> {
    // Ensure all images are present in the local Docker daemon before loading into kind.
    // Runs in parallel; any pull failure is fatal — kind load requires the image to be present.
    let ensure_futs: Vec<_> = images.iter().map(|img| ensure_image_present(img.as_str())).collect();
    let ensure_results = future::join_all(ensure_futs).await;
    for (img, result) in images.iter().zip(ensure_results) {
        result.with_error_context(|| format!("Failed to ensure image '{}' is available", img))?;
    }

    info!(
        "Loading {} image(s) into kind cluster '{}' in parallel...",
        images.len(),
        cluster_name
    );

    let load_futs: Vec<_> = images
        .iter()
        .map(|img| load_image(cluster_name, img.as_str()))
        .collect();
    let load_results = future::join_all(load_futs).await;
    for (img, result) in images.iter().zip(load_results) {
        result.with_error_context(|| format!("Failed to load image '{}' into kind cluster '{}'", img, cluster_name))?;
    }

    Ok(())
}
