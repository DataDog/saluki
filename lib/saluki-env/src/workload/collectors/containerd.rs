use std::{collections::HashMap, sync::Arc};

use async_stream::stream;
use async_trait::async_trait;
use containerd_client::services::v1::Namespace;
use futures::{stream::select_all, Stream, StreamExt as _};
use oci_spec::runtime::Spec;
use prost_types::Any;
use saluki_config::GenericConfiguration;
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::workload::{
    entity::EntityId,
    helpers::containerd::{
        events::{ContainerdEvent, ContainerdTopic},
        ContainerdClient,
    },
    metadata::{MetadataAction, MetadataOperation, TagCardinality},
    tags::{
        EnvironmentVariableTagFilter, TagsBuilder, TagsExtractor, AUTODISCOVERY_TAGS_LABEL_KEY, STANDARD_ENV_VAR_TAGS,
    },
};

use super::MetadataCollector;

static CONTAINERD_WATCH_EVENTS: &[ContainerdTopic] = &[
    ContainerdTopic::ContainerCreated,
    ContainerdTopic::ContainerUpdated,
    ContainerdTopic::ContainerDeleted,
    ContainerdTopic::ImageCreated,
    ContainerdTopic::ImageUpdated,
    ContainerdTopic::ImageDeleted,
    ContainerdTopic::TaskStarted,
    ContainerdTopic::TaskOutOfMemory,
    ContainerdTopic::TaskExited,
    ContainerdTopic::TaskDeleted,
    ContainerdTopic::TaskPaused,
    ContainerdTopic::TaskResumed,
];

/// A workload provider that uses the containerd API to provide workload information.
pub struct ContainerdMetadataCollector {
    client: ContainerdClient,
    watched_namespaces: Vec<Namespace>,
    env_var_filter: Arc<EnvironmentVariableTagFilter>,
    container_labels_extractor: Arc<TagsExtractor>,
    container_env_vars_extractor: Arc<TagsExtractor>,
}

impl ContainerdMetadataCollector {
    /// Creates a new `ContainerdWorkloadProvider` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the collector fails to connect to the containerd API, an error will be returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, String> {
        let client = ContainerdClient::from_configuration(config)
            .await
            .map_err(|e| e.to_string())?;

        let watched_namespaces = client.get_namespaces().await.map_err(|e| e.to_string())?;

        let container_label_tag_map = config.get_typed_or_default("container_labels_as_tags");
        let container_labels_extractor = Arc::new(TagsExtractor::from_patterns(container_label_tag_map)?);

        let container_env_var_tag_map = config.get_typed_or_default("container_env_as_tags");
        let container_env_vars_extractor = Arc::new(TagsExtractor::from_patterns(container_env_var_tag_map)?);

        let env_var_filter = Arc::new(EnvironmentVariableTagFilter::from_config(config));

        Ok(Self {
            client,
            watched_namespaces,
            env_var_filter,
            container_labels_extractor,
            container_env_vars_extractor,
        })
    }
}

#[async_trait]
impl MetadataCollector for ContainerdMetadataCollector {
    fn name(&self) -> &'static str {
        "containerd"
    }

    async fn watch(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Create a watcher for each namespace, and then join all of their watch streams, which then we'll just funnel
        // back to the operations channel.
        let watchers = self.watched_namespaces.iter().map(|ns| {
            NamespaceWatcher::new(
                self.client.clone(),
                ns.clone(),
                Arc::clone(&self.env_var_filter),
                Arc::clone(&self.container_labels_extractor),
                Arc::clone(&self.container_env_vars_extractor),
            )
            .watch()
        });

        let mut operations_stream = select_all(watchers);

        while let Some(operation) = operations_stream.next().await {
            operations_tx.send(operation).await?;
        }

        Ok(())
    }
}

struct NamespaceWatcher {
    namespace: Namespace,
    client: ContainerdClient,
    env_var_filter: Arc<EnvironmentVariableTagFilter>,
    container_labels_extractor: Arc<TagsExtractor>,
    container_env_vars_extractor: Arc<TagsExtractor>,
}

impl NamespaceWatcher {
    fn new(
        client: ContainerdClient, namespace: Namespace, env_var_filter: Arc<EnvironmentVariableTagFilter>,
        container_labels_extractor: Arc<TagsExtractor>, container_env_vars_extractor: Arc<TagsExtractor>,
    ) -> Self {
        Self {
            namespace,
            client,
            env_var_filter,
            container_labels_extractor,
            container_env_vars_extractor,
        }
    }

    async fn handle_container_event(&self, id: String) -> Option<MetadataOperation> {
        let container = match self.client.get_container(&self.namespace, id.clone()).await {
            Ok(container) => Some(container),
            Err(e) => {
                error!(namespace = self.namespace.name, container_id = id, error = %e, "Failed to get container.");
                None
            }
        }?;

        let mut tags_builder = TagsBuilder::default();

        // Add basic container tags.
        tags_builder.add_high_cardinality("container_id", id);

        // Handle container image tags, if the image ID is present.
        if container.image.is_empty() {
            debug!(
                namespace = self.namespace.name,
                container_id = container.id,
                "Container image ID is empty, skipping image tags."
            );
        } else {
            self.generate_container_image_tags(&mut tags_builder, container.image)
                .await;
        }

        // Handle container label tags.
        self.generate_container_label_tags(&mut tags_builder, container.labels);

        // Handle container environment variable tags.
        match container.spec {
            Some(raw_spec) => match parse_container_spec(raw_spec) {
                Some(container_spec) => {
                    if let Some(env) = container_spec.process().as_ref().and_then(|p| p.env().as_ref()) {
                        self.generate_container_env_var_tags(&mut tags_builder, env);
                    }
                }
                None => debug!(
                    namespace = self.namespace.name,
                    container_id = container.id,
                    "Failed to parse container specification."
                ),
            },
            None => debug!(
                namespace = self.namespace.name,
                container_id = container.id,
                "Container specification is empty, skipping environment variable tags."
            ),
        }

        let mut actions = Vec::with_capacity(2);
        if !tags_builder.low_cardinality.is_empty() {
            actions.push(MetadataAction::AddTags {
                cardinality: TagCardinality::Low,
                tags: tags_builder.low_cardinality,
            });
        }

        if !tags_builder.high_cardinality.is_empty() {
            actions.push(MetadataAction::AddTags {
                cardinality: TagCardinality::High,
                tags: tags_builder.high_cardinality,
            });
        }

        if !actions.is_empty() {
            Some(MetadataOperation {
                entity_id: EntityId::Container(container.id),
                actions: actions.into(),
            })
        } else {
            None
        }
    }

    async fn generate_container_image_tags(&self, _tags_builder: &mut TagsBuilder, _image: String) {
        // TODO: Actually generate image tags.
    }

    fn generate_container_label_tags(&self, builder: &mut TagsBuilder, labels: HashMap<String, String>) {
        // TODO: Docker labels, non-Kubernetes orchestrator labels.

        // Handle extracting tags from labels based on the configured extractor.
        for (key, value) in &labels {
            self.container_labels_extractor.extract(key, value, builder);
        }

        // If there's autodiscovery-based tags specified, parse and add those as high-cardinality tags.
        //
        // The label value should be a JSON array of tags in the standard Datadog `key:value` format.
        if let Some(autodiscovery_label_value) = labels.get(AUTODISCOVERY_TAGS_LABEL_KEY) {
            match serde_json::from_str::<Vec<String>>(autodiscovery_label_value) {
                Ok(tags) => {
                    for tag in tags {
                        match tag.split_once(':') {
                            Some((key, value)) => builder.add_high_cardinality(key, value),
                            None => debug!(tag, "Autodiscovery tag not in the expected `key:value` format."),
                        }
                    }
                }
                Err(e) => debug!(error = %e, "Failed to decode autodiscovery tags."),
            };
        }
    }

    fn generate_container_env_var_tags(&self, builder: &mut TagsBuilder, env_vars: &[String]) {
        // TODO: non-Kubernetes orchestrator environment variables.

        // Parse the key/value pair out of each entry, and also filter out any environment variables we shouldn't be
        // generating tags from.
        let env_vars = env_vars
            .iter()
            .filter_map(|env_var| match env_var.split_once('=') {
                Some((key, value)) => {
                    if self.env_var_filter.is_allowed(key) {
                        Some((key.to_string(), value.to_string()))
                    } else {
                        None
                    }
                }
                None => {
                    debug!("Environment variable not in expected `key=value` format.");
                    None
                }
            })
            .collect::<HashMap<String, String>>();

        // Extract standard environment variable tags.
        for (env_var_name, tag_key) in STANDARD_ENV_VAR_TAGS {
            if let Some(tag_value) = env_vars.get(*env_var_name) {
                builder.add_low_cardinality(*tag_key, tag_value);
            }
        }

        // Handle extracting tags from environment variables based on the configured extractor.
        for (key, value) in &env_vars {
            self.container_env_vars_extractor.extract(key, value, builder);
        }
    }

    async fn process_event(&self, event: ContainerdEvent) -> Option<MetadataOperation> {
        match event {
            ContainerdEvent::ContainerCreated { id } => self.handle_container_event(id).await,
            ContainerdEvent::ContainerUpdated { id } => self.handle_container_event(id).await,
            ContainerdEvent::ContainerDeleted { id } => Some(MetadataOperation::delete(EntityId::Container(id))),
            ContainerdEvent::TaskStarted { id, pid } => {
                let pid_entity_id = EntityId::ContainerPid(pid);
                let container_entity_id = EntityId::Container(id);
                Some(MetadataOperation::link_ancestor(pid_entity_id, container_entity_id))
            }
            ContainerdEvent::TaskDeleted { pid, .. } => Some(MetadataOperation::delete(EntityId::ContainerPid(pid))),
            _ => {
                debug!(namespace = self.namespace.name, event = ?event, "Ignoring unhandled event.");
                None
            }
        }
    }

    async fn build_initial_metadata_operations(&self) -> Option<Vec<MetadataOperation>> {
        let mut operations = Vec::new();

        // Get a list of all containers in the namespace.
        let containers = match self.client.list_containers(&self.namespace).await {
            Ok(containers) => containers,
            Err(e) => {
                error!(namespace = self.namespace.name, error = %e, "Error listing containers.");
                return None;
            }
        };

        for container in containers {
            if let Some(operation) = self.handle_container_event(container.id.clone()).await {
                operations.push(operation);
            }

            let pids = match self
                .client
                .get_pids_for_container(&self.namespace, container.id.clone())
                .await
            {
                Ok(pids) => pids,
                Err(e) => {
                    if let Some(status) = e.as_response_error() {
                        if status.code() == tonic::Code::NotFound {
                            // The container may have been deleted before we could get the PIDs for it, so we'll just
                            // skip it without making a fuss.
                            continue;
                        }
                    }

                    error!(namespace = self.namespace.name, container_id = container.id, error = %e, "Error getting PIDs for container.");
                    continue;
                }
            };

            for pid in pids {
                let pid_entity_id = EntityId::ContainerPid(pid);
                let container_entity_id = EntityId::Container(container.id.clone());
                operations.push(MetadataOperation::link_ancestor(pid_entity_id, container_entity_id));
            }
        }

        Some(operations)
    }

    fn watch(self) -> impl Stream<Item = MetadataOperation> + Unpin {
        // We watch the given namespace for all of the relevant events, and convert those into metadata operations that
        // we pass back to be collected by the parent watcher task, which then forwards them to the metadata aggregator.
        Box::pin(stream! {
            // Do an initial scan of the namespace to get all of the existing containers, their tasks and images, and
            // so on, and generate metadata operations from that as a way to prime the store.
            if let Some(initial_operations) = self.build_initial_metadata_operations().await {
                for operation in initial_operations {
                    yield operation;
                }
            }

            // Now watch for events.
            loop {
                let mut event_stream = match self.client.watch_events(CONTAINERD_WATCH_EVENTS, &self.namespace).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!(namespace = self.namespace.name, error = %e, "Error watching container events.");
                        continue;
                    },
                };

                while let Some(event_result) = event_stream.next().await {
                    let event = match event_result {
                        Ok(event) => event,
                        Err(e) => {
                            error!(namespace = self.namespace.name, error = %e, "Error watching container events.");
                            continue;
                        },
                    };

                    if let Some(operation) = self.process_event(event).await {
                        yield operation;
                    }
                }
            }
        })
    }
}

fn parse_container_spec(raw_spec: Any) -> Option<Spec> {
    serde_json::from_slice(&raw_spec.value).ok()
}
