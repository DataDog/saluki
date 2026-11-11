//! Removal of a group of Docker resources: its labeled containers, then its volume, then its network.

use std::collections::HashMap;

use bollard::{
    errors::Error,
    query_parameters::{ListContainersOptionsBuilder, RemoveContainerOptionsBuilder, RemoveVolumeOptions},
    Docker,
};
use tracing::debug;

/// The resources of one group, found by label and by name.
pub(crate) struct Group<'a> {
    /// Label filters, in `key=value` form, that every container of the group matches.
    pub(crate) labels: &'a [&'a str],
    pub(crate) volume: &'a str,
    pub(crate) network: &'a str,
}

impl Group<'_> {
    /// Removes the group's containers, then its volume, then its network, and returns one line per failure.
    ///
    /// Force-removes containers, killing any that are running. Attempts every removal despite failures and treats
    /// missing resources as removed.
    pub(crate) async fn remove(&self, docker: &Docker) -> Vec<String> {
        let mut failures = Vec::new();

        let list_filters: HashMap<&str, Vec<&str>> = [("label", self.labels.to_vec())].into_iter().collect();
        let list_options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&list_filters)
            .build();
        match docker.list_containers(Some(list_options)).await {
            Ok(containers) => {
                for container_id in containers.into_iter().filter_map(|container| container.id) {
                    let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();
                    match docker.remove_container(&container_id, Some(remove_options)).await {
                        Err(e) if !is_not_found(&e) => failures.push(format!("container '{}': {}", container_id, e)),
                        _ => debug!("Removed container '{}'.", container_id),
                    }
                }
            }
            Err(e) => failures.push(format!("container list: {}", e)),
        }

        match docker.remove_volume(self.volume, None::<RemoveVolumeOptions>).await {
            Err(e) if !is_not_found(&e) => failures.push(format!("volume '{}': {}", self.volume, e)),
            _ => debug!("Removed shared volume '{}'.", self.volume),
        }

        match docker.remove_network(self.network).await {
            Err(e) if !is_not_found(&e) => failures.push(format!("network '{}': {}", self.network, e)),
            _ => debug!("Removed shared network '{}'.", self.network),
        }

        failures
    }
}

/// Returns whether Docker reported a missing resource.
fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::DockerResponseServerError { status_code: 404, .. })
}
