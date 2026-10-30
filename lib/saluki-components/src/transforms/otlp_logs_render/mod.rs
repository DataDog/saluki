//! OTLP logs render transform.
//!
//! Renders each translated OTLP log into the content that the Agent sends for it, the way the Agent's logs agent
//! exporter does at the end of its OTLP logs path. See [`render_content`] for what the content holds.
//!
//! Enrichment of OTLP logs belongs before this transform, since it changes the properties that go into the content.
//! Processing rules belong after it, since the Agent applies them to the content.

use std::mem;

use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::{log::Log, Event},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tracing::error;

use crate::common::otlp::logs::render_content;

/// OTLP logs render transform.
///
/// Replaces the message of each log with its rendered content, and clears its additional properties, which the
/// content now holds. The status, hostname, service, source, and tags stay on the log, for the envelope. Logs whose
/// message is already rendered content, and events other than logs, pass through unchanged.
#[derive(Debug, Default)]
pub struct OtlpLogsRenderConfiguration;

#[async_trait]
impl SynchronousTransformBuilder for OtlpLogsRenderConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(OtlpLogsRender))
    }
}

impl MemoryBounds for OtlpLogsRenderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<OtlpLogsRender>("component struct");
    }
}

struct OtlpLogsRender;

impl SynchronousTransform for OtlpLogsRender {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        let mut failed = false;
        for event in &mut *buffer {
            let Event::Log(log) = event else {
                continue;
            };
            if log.message_is_content() {
                continue;
            }

            match render_content(log) {
                Ok(content) => {
                    let unrendered = mem::replace(log, Log::new(MetaString::empty()));
                    *log = unrendered
                        .with_rendered_content(content)
                        .with_additional_properties(None);
                }
                Err(e) => {
                    error!(error = %e, "Failed to render OTLP log content. Dropping log.");
                    failed = true;
                }
            }
        }

        // Like the Agent's exporter, drop a log that can't be rendered. That only happens for a property that Go can't
        // serialize either, such as a non-finite float. Removing it can reorder the rest of the buffer.
        if failed {
            buffer.remove_if(|event| matches!(event, Event::Log(log) if !log.message_is_content()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_core::components::test_util::TestComponentDriver;
    use saluki_core::data_model::event::{log::Log, metric::Metric, Event};
    use saluki_core::data_model::tags::{Tag, TagSet};
    use serde_json::json;
    use stringtheory::MetaString;

    use super::OtlpLogsRenderConfiguration;
    use crate::common::otlp::logs::render_content;

    async fn run(events: Vec<Event>) -> Vec<Event> {
        let (control, mut output) = TestComponentDriver::synchronous_transform(OtlpLogsRenderConfiguration)
            .await
            .expect("render transform should build");
        control.send_events(events).await;
        control.shutdown().await.expect("render transform should stop cleanly");
        output.collect_events().await
    }

    fn translated_log(message: &str) -> Log {
        let mut properties = HashMap::new();
        properties.insert(MetaString::from_static("app"), json!("test"));
        properties.insert(MetaString::from_static("otel.severity_number"), json!("5"));
        Log::new(message)
            .with_status("debug")
            .with_source(MetaString::from_static("otlp_log_ingestion"))
            .with_hostname(MetaString::from_static("host-a"))
            .with_service(MetaString::from_static("svc"))
            .with_tags(
                [Tag::from_static("otel_source:datadog_agent")]
                    .into_iter()
                    .collect::<TagSet>(),
            )
            .with_additional_properties(properties)
    }

    fn into_log(event: Event) -> Log {
        match event {
            Event::Log(log) => log,
            other => panic!("expected a log, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn renders_the_content_and_keeps_the_envelope_fields() {
        let original = translated_log("hello");
        let expected_content = render_content(&original).expect("log should render");
        assert_eq!(
            expected_content,
            r#"{"app":"test","hostname":"host-a","message":"hello","otel.severity_number":"5","service":"svc","status":"debug"}"#
        );

        let mut events = run(vec![Event::Log(original.clone())]).await;
        assert_eq!(events.len(), 1);
        let log = into_log(events.remove(0));

        assert!(log.message_is_content());
        assert_eq!(log.message(), expected_content);
        assert!(log.additional_properties().is_empty());
        assert_eq!(log.status(), original.status());
        assert_eq!(log.source(), original.source());
        assert_eq!(log.hostname(), original.hostname());
        assert_eq!(log.service(), original.service());
        assert_eq!(log.tags(), original.tags());
    }

    #[tokio::test]
    async fn keeps_the_order_of_the_logs() {
        let originals = ["first", "second", "third"].map(translated_log);
        let events = run(originals.iter().cloned().map(Event::Log).collect()).await;

        let contents = events
            .into_iter()
            .map(|event| into_log(event).message().to_string())
            .collect::<Vec<_>>();
        let expected = originals
            .iter()
            .map(|log| render_content(log).expect("log should render"))
            .collect::<Vec<_>>();
        assert_eq!(contents, expected);
    }

    #[tokio::test]
    async fn passes_rendered_logs_and_other_events_through() {
        let rendered = translated_log("ignored").with_rendered_content(r#"{"message":"already rendered"}"#);
        let metric = Metric::counter("some.counter", 1.0);
        let events = run(vec![Event::Log(rendered.clone()), Event::Metric(metric.clone())]).await;

        assert_eq!(events, vec![Event::Log(rendered), Event::Metric(metric)]);
    }
}
