use datadog_protos::events::EventsPayload;
use serde::{Deserialize, Serialize};

/// A simplified event representation.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Event {
    title: String,
    text: String,
    alert_type: String,
    aggregation_key: String,
    hostname: String,
    priority: String,
    source_type_name: String,
    tags: Vec<String>,
    /// Unix timestamp in seconds from the `d:` field, or 0 if not present.
    ///
    /// NOTE: Do not compare this field directly without first normalizing pipeline-generated
    /// fill-in values — see `EventsAnalyzer` for details.
    pub timestamp: i64,
}

impl Event {
    /// Returns the title of the event.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the text of the event.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the alert type of the event.
    pub fn alert_type(&self) -> &str {
        &self.alert_type
    }

    /// Returns the aggregation key of the event.
    pub fn aggregation_key(&self) -> &str {
        &self.aggregation_key
    }

    /// Returns the hostname of the event.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Returns the priority of the event.
    pub fn priority(&self) -> &str {
        &self.priority
    }

    /// Returns the source type name of the event.
    pub fn source_type_name(&self) -> &str {
        &self.source_type_name
    }

    /// Returns the tags of the event.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Returns the timestamp of the event (Unix seconds, 0 if not set).
    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// Converts an `EventsPayload` into a list of `Event`s.
    pub fn from_events_payload(payload: EventsPayload) -> Vec<Self> {
        payload
            .events
            .into_iter()
            .map(|e| {
                let mut tags: Vec<String> = e.tags().iter().map(|t| t.to_string()).collect();
                tags.sort_unstable();

                Event {
                    title: e.title().to_string(),
                    text: e.text().to_string(),
                    alert_type: e.alert_type().to_string(),
                    aggregation_key: e.aggregation_key().to_string(),
                    hostname: e.host().to_string(),
                    priority: e.priority().to_string(),
                    source_type_name: e.source_type_name().to_string(),
                    tags,
                    timestamp: e.ts(),
                }
            })
            .collect()
    }

    /// Builds an `Event` from the fields extracted from the stock agent's `/intake/` JSON format.
    #[allow(clippy::too_many_arguments)]
    pub fn from_intake_event(
        title: String, text: String, alert_type: String, aggregation_key: String, hostname: String, priority: String,
        source_type_name: String, mut tags: Vec<String>, timestamp: i64,
    ) -> Self {
        tags.sort_unstable();
        Event {
            title,
            text,
            alert_type,
            aggregation_key,
            hostname,
            priority,
            source_type_name,
            tags,
            timestamp,
        }
    }
}
