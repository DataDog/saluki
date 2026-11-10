//! OTLP logs render transform.
//!
//! Renders each translated OTLP log into the content of its log message, as the last step of the OTLP logs path. See
//! [`ContentRenderer`] for what the content holds.
//!
//! Enrichment of OTLP logs belongs before this transform, since it changes the properties that go into the content.
//! Processing rules belong after it, since they apply to the content.

use std::mem;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use metrics::Counter;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::{log::Log, Event},
    observability::ComponentMetricsExt as _,
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tracing::error;

use crate::common::otlp::logs::{
    ContentRenderer, CONTENT_BUFFER_RETENTION_LIMIT_BYTES, CONTENT_FIELDS_RETENTION_LIMIT_BYTES,
};

/// The shortest time between two logged render failures.
const RENDER_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// OTLP logs render transform.
///
/// Replaces the message of each log with its rendered content, and clears its additional properties, which the
/// content now holds. The status, hostname, service, source, and tags stay on the log, for the envelope. Events other
/// than logs pass through unchanged.
#[derive(Debug, Default)]
pub struct OtlpLogsRenderConfiguration;

#[async_trait]
impl SynchronousTransformBuilder for OtlpLogsRenderConfiguration {
    async fn build(&self, context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let events_dropped = metrics_builder.register_counter_with_tags(
            "component_events_dropped_total",
            ["intentional:false", "drop_reason:render_failure"],
        );
        Ok(Box::new(OtlpLogsRender {
            renderer: ContentRenderer::default(),
            events_dropped,
            last_failure_logged: None,
        }))
    }
}

impl MemoryBounds for OtlpLogsRenderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<OtlpLogsRender>("component struct")
            .with_fixed_amount("render buffer", CONTENT_BUFFER_RETENTION_LIMIT_BYTES)
            .with_fixed_amount("render field list", CONTENT_FIELDS_RETENTION_LIMIT_BYTES);
    }
}

struct OtlpLogsRender {
    renderer: ContentRenderer,
    events_dropped: Counter,
    last_failure_logged: Option<Instant>,
}

impl SynchronousTransform for OtlpLogsRender {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        // Render each log, and drop the logs that can't be rendered. That only happens for a property that can't be
        // serialized, such as a non-finite float. Removing them can reorder the rest of the buffer.
        let renderer = &mut self.renderer;
        let mut failures = 0;
        let mut first_failure = None;
        buffer.remove_if(|event| {
            let Event::Log(log) = event else {
                return false;
            };
            match renderer.render(log) {
                Ok(content) => {
                    let unrendered = mem::replace(log, Log::new(MetaString::empty()));
                    *log = unrendered.with_message(content).with_additional_properties(None);
                    false
                }
                Err(e) => {
                    failures += 1;
                    first_failure.get_or_insert(e);
                    true
                }
            }
        });

        if let Some(error) = first_failure {
            self.events_dropped.increment(failures);

            let now = Instant::now();
            let log_due = self
                .last_failure_logged
                .is_none_or(|last| now.duration_since(last) >= RENDER_FAILURE_LOG_INTERVAL);
            if log_due {
                self.last_failure_logged = Some(now);
                error!(%error, failures, "Failed to render OTLP log content. Dropping logs.");
            }
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
    use crate::common::otlp::logs::ContentRenderer;

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
        let expected_content = ContentRenderer::default().render(&original).expect("log should render");
        assert_eq!(
            expected_content,
            r#"{"app":"test","hostname":"host-a","message":"hello","otel.severity_number":"5","service":"svc","status":"debug"}"#
        );

        let mut events = run(vec![Event::Log(original.clone())]).await;
        assert_eq!(events.len(), 1);
        let log = into_log(events.remove(0));

        assert_eq!(log.message(), &*expected_content);
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
            .map(|log| {
                ContentRenderer::default()
                    .render(log)
                    .expect("log should render")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(contents, expected);
    }

    #[tokio::test]
    async fn passes_other_events_through() {
        let metric = Metric::counter("some.counter", 1.0);
        let events = run(vec![Event::Metric(metric.clone())]).await;

        assert_eq!(events, vec![Event::Metric(metric)]);
    }
}
