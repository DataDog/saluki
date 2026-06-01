//! Config ID metric enrichment.
//!
//! Adds the Fleet Automation config ID tag to outbound metrics when `config_id` is configured.

use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::Tag;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::metric::Metric,
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;

const CONFIG_ID_KEY: &str = "config_id";

/// Config ID enrichment configuration.
pub struct ConfigIdEnrichmentConfiguration {
    config_id_tag: Option<Tag>,
}

impl ConfigIdEnrichmentConfiguration {
    /// Creates a new `ConfigIdEnrichmentConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let config_id = config.try_get_typed::<String>(CONFIG_ID_KEY)?;
        let config_id_tag = config_id
            .filter(|config_id| !config_id.trim().is_empty())
            .map(|config_id| Tag::from(MetaString::from(format!("{CONFIG_ID_KEY}:{config_id}"))));

        Ok(Self { config_id_tag })
    }

    /// Returns true when config ID enrichment should be enabled.
    pub fn enabled(&self) -> bool {
        self.config_id_tag.is_some()
    }
}

#[async_trait]
impl SynchronousTransformBuilder for ConfigIdEnrichmentConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(ConfigIdEnrichment {
            config_id_tag: self.config_id_tag.clone(),
        }))
    }
}

impl MemoryBounds for ConfigIdEnrichmentConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<ConfigIdEnrichment>("component struct");
    }
}

pub struct ConfigIdEnrichment {
    config_id_tag: Option<Tag>,
}

impl ConfigIdEnrichment {
    fn enrich_metric(&self, metric: &mut Metric) {
        let Some(config_id_tag) = self.config_id_tag.as_ref() else {
            return;
        };

        metric.context_mut().with_tag_sets_mut(|tags, origin_tags| {
            let _ = tags.remove_tags(CONFIG_ID_KEY);
            let _ = origin_tags.remove_tags(CONFIG_ID_KEY);
            tags.insert_tag(config_id_tag.clone());
        });
    }
}

impl SynchronousTransform for ConfigIdEnrichment {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        for event in event_buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                self.enrich_metric(metric);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::ConfigurationLoader;
    use saluki_context::{
        tags::{Tag, TagSet},
        Context,
    };
    use saluki_core::data_model::event::{eventd::EventD, Event};
    use saluki_core::{components::transforms::SynchronousTransform, data_model::event::metric::Metric};

    use super::*;

    fn config_id_enrichment(config_id_tag: Option<&'static str>) -> ConfigIdEnrichment {
        ConfigIdEnrichment {
            config_id_tag: config_id_tag.map(Tag::from),
        }
    }

    fn metric_with_origin_tags(tags: &[&'static str], origin_tags: &[&'static str]) -> Metric {
        let origin_tag_set: TagSet = origin_tags.iter().map(|s| Tag::from(*s)).collect();
        let context = Context::from_static_parts("test.metric", tags).with_origin_tags(origin_tag_set);
        Metric::gauge(context, 1.0)
    }

    fn tag_names(metric: &Metric) -> Vec<String> {
        let mut names: Vec<_> = metric
            .context()
            .tags()
            .into_iter()
            .map(|tag| tag.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    fn origin_tag_names(metric: &Metric) -> Vec<String> {
        let mut names: Vec<_> = metric
            .context()
            .origin_tags()
            .into_iter()
            .map(|tag| tag.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn missing_config_id_is_disabled() {
        let (config, _) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, false).await;
        let configuration = ConfigIdEnrichmentConfiguration::from_configuration(&config).unwrap();

        assert!(configuration.config_id_tag.is_none());
    }

    #[tokio::test]
    async fn empty_config_id_is_disabled() {
        let (config, _) =
            ConfigurationLoader::for_tests(Some(serde_json::json!({ "config_id": "" })), None, false).await;
        let configuration = ConfigIdEnrichmentConfiguration::from_configuration(&config).unwrap();

        assert!(configuration.config_id_tag.is_none());
    }

    #[tokio::test]
    async fn whitespace_config_id_is_disabled() {
        let (config, _) =
            ConfigurationLoader::for_tests(Some(serde_json::json!({ "config_id": "   " })), None, false).await;
        let configuration = ConfigIdEnrichmentConfiguration::from_configuration(&config).unwrap();

        assert!(configuration.config_id_tag.is_none());
    }

    #[test]
    fn adds_config_id_tag() {
        let enrichment = config_id_enrichment(Some("config_id:test-config01"));
        let mut metric = metric_with_origin_tags(&["env:prod"], &[]);

        enrichment.enrich_metric(&mut metric);

        assert_eq!(tag_names(&metric), vec!["config_id:test-config01", "env:prod"]);
    }

    #[test]
    fn replaces_existing_config_id_tag() {
        let enrichment = config_id_enrichment(Some("config_id:test-config01"));
        let mut metric = metric_with_origin_tags(&["config_id:old", "env:prod"], &[]);

        enrichment.enrich_metric(&mut metric);

        assert_eq!(tag_names(&metric), vec!["config_id:test-config01", "env:prod"]);
    }

    #[test]
    fn replaces_existing_origin_config_id_tag() {
        let enrichment = config_id_enrichment(Some("config_id:test-config01"));
        let mut metric = metric_with_origin_tags(&["env:prod"], &["config_id:old", "service:web"]);

        enrichment.enrich_metric(&mut metric);

        assert_eq!(tag_names(&metric), vec!["config_id:test-config01", "env:prod"]);
        assert_eq!(origin_tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn disabled_enrichment_leaves_metric_unchanged() {
        let enrichment = config_id_enrichment(None);
        let mut metric = metric_with_origin_tags(&["config_id:old", "env:prod"], &["config_id:origin"]);

        enrichment.enrich_metric(&mut metric);

        assert_eq!(tag_names(&metric), vec!["config_id:old", "env:prod"]);
        assert_eq!(origin_tag_names(&metric), vec!["config_id:origin"]);
    }

    #[test]
    fn non_metric_events_are_unchanged() {
        let mut enrichment = config_id_enrichment(Some("config_id:test-config01"));
        let event = Event::EventD(EventD::new("title", "text"));
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(event.clone()).is_none());

        enrichment.transform_buffer(&mut buffer);

        let events: Vec<_> = buffer.into_iter().collect();
        assert_eq!(events, vec![event]);
    }
}
