use super::Tags;

#[derive(Debug, Clone)]
pub struct Event {
    pub title: String,
    pub text: String,
    pub timestamp: u64,
    pub priority: String,
    pub tags: Tags,
    pub alert_type: String,
    pub aggregation_key: String,
    pub source_type_name: String,
    pub event_type: String,
}
