use serde::{Deserialize, Serialize};

/// A simplified decoded stateful log representation.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct StatefulLog {
    message: String,
    status: Option<String>,
    service: Option<String>,
    tags: Option<String>,
    timestamp: i64,
    uuid: Option<String>,
}

impl StatefulLog {
    /// Builds a decoded stateful log from its constituent fields.
    pub fn from_parts(
        message: String, status: Option<String>, service: Option<String>, tags: Option<String>, timestamp: i64,
        uuid: Option<String>,
    ) -> Self {
        Self {
            message,
            status,
            service,
            tags,
            timestamp,
            uuid,
        }
    }

    /// Returns the log message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the log status.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Returns the log service.
    pub fn service(&self) -> Option<&str> {
        self.service.as_deref()
    }

    /// Returns the flattened log tag string.
    pub fn tags(&self) -> Option<&str> {
        self.tags.as_deref()
    }

    /// Returns the log timestamp in milliseconds.
    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// Returns the log UUID.
    pub fn uuid(&self) -> Option<&str> {
        self.uuid.as_deref()
    }
}
