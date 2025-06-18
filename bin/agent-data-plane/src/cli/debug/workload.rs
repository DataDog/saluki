use std::sync::LazyLock;

use colored::{ColoredString, Colorize};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tracing::error;

use crate::{cli::utils::APIClient, config::WorkloadConfig};

#[derive(Deserialize)]
struct TagsWithCardinality<'a> {
    #[serde(borrow)]
    low_cardinality: Vec<&'a str>,
    #[serde(borrow)]
    orchestrator_cardinality: Vec<&'a str>,
    #[serde(borrow)]
    high_cardinality: Vec<&'a str>,
}

impl TagsWithCardinality<'_> {
    fn is_empty(&self) -> bool {
        self.low_cardinality.is_empty() && self.orchestrator_cardinality.is_empty() && self.high_cardinality.is_empty()
    }
}

#[derive(Deserialize)]
struct EntityTagsEntry<'a> {
    entity_id: &'a str,
    alias: Option<&'a str>,
    tags: TagsWithCardinality<'a>,
}

#[derive(Deserialize)]
struct ExternalDataEntry<'a> {
    pod_uid: &'a str,
    container_name: &'a str,
    init_container: bool,
    container_id: &'a str,
}

/// Entrypoint for all `workload` subcommands.
pub async fn handle_workload_command(config: WorkloadConfig) {
    match config {
        WorkloadConfig::Tags { json } => {
            if let Err(e) = dump_tags(json).await {
                error!("Failed to dump workload tags: {}", e);
                std::process::exit(1);
            }
        }
        WorkloadConfig::ExternalData { json } => {
            if let Err(e) = dump_external_data(json).await {
                error!("Failed to dump workload external data: {}", e);
                std::process::exit(1);
            }
        }
    }
}

/// Dumps all tags from the workload provider.
pub async fn dump_tags(json_output: bool) -> Result<(), GenericError> {
    static ENTITY: LazyLock<ColoredString> = LazyLock::new(|| "Entity".bold());
    static ALIAS: LazyLock<ColoredString> = LazyLock::new(|| "Alias".bold());
    static TAGS: LazyLock<ColoredString> = LazyLock::new(|| "Tags".bold());
    static LOW_CARD: LazyLock<ColoredString> = LazyLock::new(|| "Low".bold());
    static ORCH_CARD: LazyLock<ColoredString> = LazyLock::new(|| "Orchestrator".bold());
    static HIGH_CARD: LazyLock<ColoredString> = LazyLock::new(|| "High".bold());

    let api_client = APIClient::new();
    let raw_entity_tags = api_client.workload_tags().await?;

    let entity_tags_entries = serde_json::from_str::<Vec<EntityTagsEntry>>(&raw_entity_tags)
        .error_context("Failed to decode workload tags dump response as valid JSON.")?;

    // If JSON output has been requested, print it now.
    //
    // We do this here, after deserializing, to ensure we're returning valid JSON to the caller.
    if json_output {
        println!("{}", raw_entity_tags);
        return Ok(());
    }

    for entity_tags_entry in entity_tags_entries {
        println!("{}: {}", *ENTITY, entity_tags_entry.entity_id.italic().green());
        if let Some(alias) = entity_tags_entry.alias {
            println!("  {}: {}", *ALIAS, alias.italic().green());
        }

        if entity_tags_entry.tags.is_empty() {
            println!("  {}: None", *TAGS);
        } else {
            println!("  {}:", *TAGS);
            print_tags(&*LOW_CARD, entity_tags_entry.tags.low_cardinality);
            print_tags(&*ORCH_CARD, entity_tags_entry.tags.orchestrator_cardinality);
            print_tags(&*HIGH_CARD, entity_tags_entry.tags.high_cardinality);
        }

        println!();
    }

    Ok(())
}

fn print_tags<T: std::fmt::Display>(tag_set_name: &T, tags: Vec<&str>) {
    static OPEN_BRACKET: LazyLock<ColoredString> = LazyLock::new(|| "[".bold());
    static CLOSE_BRACKET: LazyLock<ColoredString> = LazyLock::new(|| "]".bold());

    if !tags.is_empty() {
        let joined = tags
            .iter()
            .map(|tag| match tag.split_once(':') {
                Some((key, value)) => format!("{}:{}", key, value.cyan()).italic().to_string(),
                None => tag.italic().to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("    {}: {}{}{}", tag_set_name, *OPEN_BRACKET, joined, *CLOSE_BRACKET);
    }
}

/// Dumps all External Data entries from the workload provider.
pub async fn dump_external_data(json_output: bool) -> Result<(), GenericError> {
    static CONTAINER_ID: LazyLock<ColoredString> = LazyLock::new(|| "Container ID".bold());
    static POD_UID: LazyLock<ColoredString> = LazyLock::new(|| "Pod UID".bold());
    static CONTAINER_NAME: LazyLock<ColoredString> = LazyLock::new(|| "Container Name".bold());
    static INIT_CONTAINER: LazyLock<ColoredString> = LazyLock::new(|| "Init Container".bold());

    let api_client = APIClient::new();
    let raw_external_data = api_client.workload_external_data().await?;

    let external_data_entries = serde_json::from_str::<Vec<ExternalDataEntry>>(&raw_external_data)
        .error_context("Failed to decode workload external data dump response as valid JSON.")?;

    // If JSON output has been requested, print it now.
    //
    // We do this here, after deserializing, to ensure we're returning valid JSON to the caller.
    if json_output {
        println!("{}", raw_external_data);
        return Ok(());
    }

    for external_data_entry in external_data_entries {
        println!(
            "{}: {}",
            *CONTAINER_ID,
            external_data_entry.container_id.italic().green()
        );
        println!("  {}: {}", *POD_UID, external_data_entry.pod_uid.italic().green());
        println!("  {}: {}", *CONTAINER_NAME, external_data_entry.container_name.italic());
        println!("  {}: {}", *INIT_CONTAINER, external_data_entry.init_container);

        println!();
    }

    Ok(())
}
