//! The Docker resources one case shares: a network, an optional volume, and the containers created in it.
//!
//! A [`CaseScope`] computes every name it uses from its ID without talking to Docker, so a caller can record the
//! scope for reclaim before [`CaseScope::prepare`] creates anything.
//!
//! Every resource carries the label `airlock-case=<id>`, never the isolation group label of
//! [`Driver`][crate::driver::Driver], so reclaiming one never removes the other's containers. Names start with
//! `airlock-case-`, which keeps them apart from isolation groups, named `airlock-<id>`, unless an isolation group ID
//! starts with `case-`.

use std::collections::HashMap;

use bollard::{
    models::{Ipam, NetworkCreateRequest, VolumeCreateRequest},
    Docker,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::time::{timeout_at, Instant};
use tracing::debug;

use crate::{
    driver::with_transient_error_retry,
    removal::{bounded, is_not_found, Group, RequestError},
};

const CREATED_BY_LABEL: (&str, &str) = ("created_by", "airlock");
const CASE_LABEL_KEY: &str = "airlock-case";

/// What a case scope looks like on one container OS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScopePlatform {
    /// Docker network driver of the case network.
    pub network_driver: &'static str,

    /// Where every container of the case mounts the case volume, or `None` for a scope with no volume.
    pub volume_path: Option<&'static str>,
}

impl ScopePlatform {
    /// Linux containers: a bridge network, and a volume at `/airlock` in every container.
    pub const LINUX: Self = Self {
        network_driver: "bridge",
        volume_path: Some("/airlock"),
    };
}

/// The network and volume of one case, and the label and names of everything created in it.
#[derive(Clone, Debug)]
pub struct CaseScope {
    pub(crate) docker: Docker,
    pub(crate) id: String,
    pub(crate) platform: ScopePlatform,
}

impl CaseScope {
    /// Creates a scope with the given ID. Nothing is created in Docker until [`Self::prepare`].
    ///
    /// The ID must be unique among the scopes that exist on the Docker daemon, and valid in Docker resource names.
    pub fn new(docker: Docker, id: impl Into<String>, platform: ScopePlatform) -> Self {
        Self {
            docker,
            id: id.into(),
            platform,
        }
    }

    /// Returns the name of the case network, which is also the name of the case volume.
    pub fn resource_name(&self) -> String {
        format!("airlock-case-{}", self.id)
    }

    /// Returns the name of the container for `participant`.
    pub fn container_name(&self, participant: &str) -> String {
        format!("airlock-case-{}-{}", self.id, participant)
    }

    /// Returns the labels every resource of the scope carries.
    pub(crate) fn labels(&self) -> HashMap<String, String> {
        let (created_by_key, created_by_value) = CREATED_BY_LABEL;
        HashMap::from([
            (created_by_key.to_string(), created_by_value.to_string()),
            (CASE_LABEL_KEY.to_string(), self.id.clone()),
        ])
    }

    /// The label every resource of the scope carries, as a Docker filter: `airlock-case=<id>`.
    pub(crate) fn case_label(&self) -> String {
        format!("{}={}", CASE_LABEL_KEY, self.id)
    }

    /// Creates the case network, then the case volume if the platform has one.
    ///
    /// # Errors
    ///
    /// Returns an error if a network of that name belongs to another case, if a volume of that name exists, if Docker
    /// refuses to create a resource, or if `deadline` passes first.
    pub async fn prepare(&self, deadline: Instant) -> Result<(), GenericError> {
        let name = self.resource_name();

        let create_network = with_transient_error_retry("network creation", &self.id, || async {
            // A create that Docker reported as failed can still have made the network (the collision the retry is
            // for), so every attempt looks for it first. The name holds the case ID, so a network of that name with
            // this scope's label was made by an earlier attempt.
            match self.docker.inspect_network(&name, None).await {
                Ok(network)
                    if network.labels.as_ref().and_then(|labels| labels.get(CASE_LABEL_KEY)) == Some(&self.id) =>
                {
                    return Ok(());
                }
                Ok(_) => {
                    return Err(generic_error!(
                        "Network '{}' already exists and belongs to another case.",
                        name
                    ))
                }
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(generic_error!("Failed to look up network '{}': {}", name, e)),
            }
            let request = NetworkCreateRequest {
                name: name.clone(),
                driver: Some(self.platform.network_driver.to_string()),
                ipam: Some(Ipam::default()),
                enable_ipv6: Some(false),
                labels: Some(self.labels()),
                ..Default::default()
            };
            self.docker
                .create_network(request)
                .await
                .map(|_| ())
                .with_error_context(|| format!("Failed to create network '{}'.", name))
        });
        // The deadline bounds the waits between attempts, too.
        timeout_at(deadline, create_network).await.unwrap_or_else(|_| {
            Err(generic_error!(
                "Failed to create network '{}': {}",
                name,
                RequestError::DeadlineExceeded
            ))
        })?;
        debug!("Created network '{}'.", name);

        if self.platform.volume_path.is_some() {
            // Docker answers the create of an existing volume with that volume, contents and all. The volume create is
            // not retried, so no earlier attempt of this scope made one.
            match bounded(Some(deadline), self.docker.inspect_volume(&name)).await {
                Ok(_) => return Err(generic_error!("Volume '{}' already exists.", name)),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(generic_error!("Failed to look up volume '{}': {}", name, e)),
            }
            let request = VolumeCreateRequest {
                name: Some(name.clone()),
                driver: Some("local".to_string()),
                labels: Some(self.labels()),
                ..Default::default()
            };
            bounded(Some(deadline), self.docker.create_volume(request))
                .await
                .map_err(|e| generic_error!("Failed to create volume '{}': {}", name, e))?;
            debug!("Created volume '{}'.", name);
        }

        Ok(())
    }

    /// Removes every container of the scope with its anonymous volumes, then the case volume, then the case network.
    ///
    /// Running containers are killed. Every removal is attempted, each bounded by `deadline`, and a resource that is
    /// already gone counts as removed. A volume or network of the scope's name without the scope's label belongs to
    /// someone else, so it is not removed.
    ///
    /// # Errors
    ///
    /// Returns an error listing every resource that could not be removed by the deadline, and every volume or network
    /// that was not removed because it lacks the scope's label.
    pub async fn reclaim(&self, deadline: Instant) -> Result<(), GenericError> {
        let name = self.resource_name();
        let created_by_label = format!("{}={}", CREATED_BY_LABEL.0, CREATED_BY_LABEL.1);
        let case_label = self.case_label();
        let group = Group {
            labels: &[created_by_label.as_str(), case_label.as_str()],
            volume: self.platform.volume_path.map(|_| name.as_str()),
            network: &name,
            owner_label: Some((CASE_LABEL_KEY, &self.id)),
            remove_anonymous_volumes: true,
        };
        let failures = group.remove(&self.docker, Some(deadline)).await;

        if failures.is_empty() {
            Ok(())
        } else {
            Err(generic_error!(
                "Failed to remove case scope '{}': {}",
                self.id,
                failures.join("; ")
            ))
        }
    }
}
