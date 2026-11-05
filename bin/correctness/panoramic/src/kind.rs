use std::collections::BTreeSet;

use futures::future;
use panoramic_core::events::TestEvent;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::utils::strip_ansi_codes;

pub const DEFAULT_CLUSTER_NAME: &str = "saluki-correctness";

/// Manages a kind cluster for cases with Kubernetes pod participants.
///
/// On construction, ensures a cluster is running (creating one if needed) and loads
/// all required images into it. On teardown, optionally deletes the cluster.
pub struct KindLifecycle {
    cluster_name: String,
}

impl KindLifecycle {
    /// Creates or reuses the named kind cluster, then loads the verified local image references.
    pub async fn ensure(
        cluster_name: String, images: Vec<String>, event_tx: mpsc::UnboundedSender<TestEvent>,
    ) -> Result<Self, GenericError> {
        check_kind_installed().await?;

        let status_line = |msg: &str| {
            let _ = event_tx.send(TestEvent::StatusLine {
                message: msg.to_string(),
            });
        };

        if cluster_exists(&cluster_name).await? {
            status_line(&format!("Reusing existing kind cluster '{}'.", cluster_name));
        } else {
            status_line(&format!("Creating kind cluster '{}'...", cluster_name));
            create_cluster(&cluster_name).await?;
        }

        let unique_images: Vec<_> = images.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        if !unique_images.is_empty() {
            load_images(&cluster_name, &unique_images, &event_tx).await?;
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
    let output = Command::new("kind").arg("version").output().await.map_err(|_| {
        generic_error!(
            "'kind' not found in PATH. Install it from https://kind.sigs.k8s.io/docs/user/quick-start/#installation"
        )
    })?;
    if !output.status.success() {
        return Err(generic_error!(
            "'kind version' exited with status {}; is kind installed correctly?",
            output.status
        ));
    }
    Ok(())
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
    let output = Command::new("kind")
        .args(["create", "cluster", "--name", name, "--wait", "120s"])
        .output()
        .await
        .error_context("Failed to spawn kind create cluster")?;

    // kind writes progress to stderr. Log each line through tracing so it doesn't
    // interleave with other output, and strip ANSI codes since kind uses color.
    let combined = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
    for line in String::from_utf8_lossy(&combined).lines() {
        let clean = strip_ansi_codes(line.as_bytes());
        let clean = String::from_utf8_lossy(&clean);
        let clean = clean.trim();
        if !clean.is_empty() {
            debug!("[kind] {}", clean);
        }
    }

    if !output.status.success() {
        return Err(generic_error!(
            "'kind create cluster' exited with status: {}",
            output.status
        ));
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

async fn load_images(
    cluster_name: &str, images: &[String], event_tx: &mpsc::UnboundedSender<TestEvent>,
) -> Result<(), GenericError> {
    let _ = event_tx.send(TestEvent::StatusLine {
        message: format!("Loading images into kind cluster '{}'...", cluster_name),
    });

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
