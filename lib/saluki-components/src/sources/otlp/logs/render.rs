//! OTLP logs rendering.
//!
//! Renders each translated OTLP log into the content of its log message, as the last step of OTLP logs translation.
//! See [`ContentRenderer`] for what the content holds.
//!
//! Enrichment of OTLP logs belongs before rendering, since it changes the properties that go into the content.
//! Processing rules belong after it, since they apply to the content.

use std::mem;
use std::time::{Duration, Instant};

use metrics::Counter;
use saluki_core::data_model::event::log::Log;
use tracing::error;

use crate::common::otlp::logs::ContentRenderer;

/// The shortest time between two logged render failures.
const RENDER_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// OTLP logs renderer.
///
/// Replaces the message of each log with its rendered content, and clears its additional properties, which the
/// content now holds. The status, hostname, service, source, and tags stay on the log, for the envelope.
pub struct OtlpLogsRenderer {
    renderer: ContentRenderer,
    events_dropped: Counter,
    failures_since_logged: u64,
    last_failure_logged: Option<Instant>,
}

impl OtlpLogsRenderer {
    /// Creates a new `OtlpLogsRenderer`, counting the logs it drops in `events_dropped`.
    pub fn new(events_dropped: Counter) -> Self {
        Self {
            renderer: ContentRenderer::default(),
            events_dropped,
            failures_since_logged: 0,
            last_failure_logged: None,
        }
    }

    /// Renders a log's content into its message.
    ///
    /// If the log can't be rendered, it's counted as dropped and `None` is returned. That only happens for a property
    /// that can't be serialized.
    pub fn render(&mut self, log: Log) -> Option<Log> {
        match self.renderer.render(&log) {
            Ok(content) => Some(log.with_message(content).with_additional_properties(None)),
            Err(error) => {
                self.events_dropped.increment(1);
                self.failures_since_logged += 1;

                let now = Instant::now();
                let log_due = self
                    .last_failure_logged
                    .is_none_or(|last| now.duration_since(last) >= RENDER_FAILURE_LOG_INTERVAL);
                if log_due {
                    self.last_failure_logged = Some(now);
                    let failures = mem::take(&mut self.failures_since_logged);
                    error!(%error, failures, "Failed to render OTLP log content. Dropping logs.");
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use metrics::Counter;
    use saluki_core::data_model::event::log::Log;
    use saluki_core::data_model::tags::{Tag, TagSet};
    use serde_json::json;
    use stringtheory::MetaString;

    use super::OtlpLogsRenderer;
    use crate::common::otlp::logs::ContentRenderer;

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

    #[test]
    fn renders_the_content_and_keeps_the_envelope_fields() {
        let original = translated_log("hello");
        let expected_content = ContentRenderer::default().render(&original).expect("log should render");
        assert_eq!(
            expected_content,
            r#"{"app":"test","hostname":"host-a","message":"hello","otel.severity_number":"5","service":"svc","status":"debug"}"#
        );

        let log = OtlpLogsRenderer::new(Counter::noop())
            .render(original.clone())
            .expect("log should render");

        assert_eq!(log.message(), &*expected_content);
        assert!(log.additional_properties().is_empty());
        assert_eq!(log.status(), original.status());
        assert_eq!(log.source(), original.source());
        assert_eq!(log.hostname(), original.hostname());
        assert_eq!(log.service(), original.service());
        assert_eq!(log.tags(), original.tags());
    }
}
