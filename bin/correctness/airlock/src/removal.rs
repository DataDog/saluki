//! Removal of a group of Docker resources: its labeled containers, then its volume, then its network.

use std::{collections::HashMap, future::Future, time::Duration};

use bollard::{
    errors::Error,
    query_parameters::{
        InspectNetworkOptions, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder, RemoveVolumeOptions,
    },
    Docker,
};
use futures::future::join_all;
use tokio::time::{timeout_at, Instant};
use tracing::debug;

/// How long the volume and network removals get when the containers have used up the deadline.
const TAIL_TIME: Duration = Duration::from_secs(5);

/// The resources of one group, found by label and by name.
pub(crate) struct Group<'a> {
    /// Label filters, in `key=value` form, that every container of the group matches.
    pub(crate) labels: &'a [&'a str],
    pub(crate) volume: Option<&'a str>,
    pub(crate) network: &'a str,
    /// A label, as key and value, that the volume and network must carry to be removed. Without it, they are removed
    /// by name alone.
    pub(crate) owner_label: Option<(&'a str, &'a str)>,
    /// Whether removing a container also removes its anonymous volumes.
    pub(crate) remove_anonymous_volumes: bool,
}

impl Group<'_> {
    /// Removes the group's containers, then its volume, then its network, and returns one line per failure.
    ///
    /// Force-removes containers, killing any that are running, all at once. Attempts every removal despite failures and
    /// treats missing resources as removed. A volume or network without the owner label is not removed, and is
    /// reported as a failure. With a deadline, every request is bounded by it, and a request still pending at the
    /// deadline is reported as a failure. The volume and network are attempted even when the containers used up the
    /// deadline, for at most `TAIL_TIME` more.
    pub(crate) async fn remove(&self, docker: &Docker, deadline: Option<Instant>) -> Vec<String> {
        let mut failures = Vec::new();

        let filters: HashMap<&str, Vec<&str>> = [("label", self.labels.to_vec())].into_iter().collect();
        let list_options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();
        match bounded(deadline, docker.list_containers(Some(list_options))).await {
            Ok(containers) => {
                let removals = containers
                    .into_iter()
                    .filter_map(|container| container.id)
                    .map(|id| async move {
                        let remove_options = RemoveContainerOptionsBuilder::default()
                            .force(true)
                            .v(self.remove_anonymous_volumes)
                            .build();
                        match bounded(deadline, docker.remove_container(&id, Some(remove_options))).await {
                            Err(e) if !e.is_not_found() => Some(format!("container '{}': {}", id, e)),
                            _ => {
                                debug!("Removed container '{}'.", id);
                                None
                            }
                        }
                    });
                failures.extend(join_all(removals).await.into_iter().flatten());
            }
            Err(e) => failures.push(format!("container list: {}", e)),
        }

        let deadline = deadline.map(|deadline| deadline.max(Instant::now() + TAIL_TIME));

        if let Some(volume) = self.volume {
            let labels = async { docker.inspect_volume(volume).await.map(|volume| Some(volume.labels)) };
            let remove = docker.remove_volume(volume, None::<RemoveVolumeOptions>);
            failures.extend(self.remove_named("volume", volume, deadline, labels, remove).await);
        }

        let labels = async {
            docker
                .inspect_network(self.network, None::<InspectNetworkOptions>)
                .await
                .map(|network| network.labels)
        };
        let remove = docker.remove_network(self.network);
        let failure = self
            .remove_named("network", self.network, deadline, labels, remove)
            .await;
        failures.extend(failure);

        failures
    }

    /// Removes the volume or network `name`, once its `labels` show the owner label if the group has one, and returns
    /// the failure, if any.
    async fn remove_named(
        &self, resource: &str, name: &str, deadline: Option<Instant>,
        labels: impl Future<Output = Result<Option<HashMap<String, String>>, Error>>,
        remove: impl Future<Output = Result<(), Error>>,
    ) -> Option<String> {
        if let Some((key, value)) = self.owner_label {
            match bounded(deadline, labels).await {
                Ok(labels)
                    if labels
                        .as_ref()
                        .and_then(|labels| labels.get(key))
                        .is_some_and(|owner| owner == value) => {}
                Ok(_) => {
                    return Some(format!(
                        "{} '{}': skipped, it lacks the label '{}={}'",
                        resource, name, key, value
                    ))
                }
                Err(e) if e.is_not_found() => return None,
                Err(e) => return Some(format!("{} '{}': {}", resource, name, e)),
            }
        }
        match bounded(deadline, remove).await {
            Err(e) if !e.is_not_found() => Some(format!("{} '{}': {}", resource, name, e)),
            _ => {
                debug!("Removed shared {} '{}'.", resource, name);
                None
            }
        }
    }
}

/// Why a bounded request did not succeed.
pub(crate) enum RequestError {
    Docker(Error),
    DeadlineExceeded,
}

impl RequestError {
    /// Returns whether Docker reported a missing resource.
    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self, Self::Docker(e) if is_not_found(e))
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Docker(e) => e.fmt(f),
            Self::DeadlineExceeded => f.write_str("the deadline passed before Docker answered"),
        }
    }
}

/// Runs a Docker request, giving up at `deadline` if there is one.
pub(crate) async fn bounded<T, F>(deadline: Option<Instant>, request: F) -> Result<T, RequestError>
where
    F: Future<Output = Result<T, Error>>,
{
    let answered = match deadline {
        Some(deadline) => timeout_at(deadline, request)
            .await
            .map_err(|_| RequestError::DeadlineExceeded)?,
        None => request.await,
    };
    answered.map_err(RequestError::Docker)
}

/// Returns whether Docker reported a missing resource.
pub(crate) fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::DockerResponseServerError { status_code: 404, .. })
}
