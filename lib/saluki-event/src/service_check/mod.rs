//! Service Check type
mod status;
pub use self::status::*;

/// ServiceCheck
#[derive(Clone, Debug)]
pub struct ServiceCheck {
    name: String,
    status: ServiceCheckStatus,
    timestamp: u64,
    host: String,
    message: String,
    tags: Vec<String>,
}

impl ServiceCheck {
    /// Gets a reference to the service check name
    pub fn name(&self) -> &String {
        &self.name
    }

    /// Gets a reference to the status
    pub fn status(&self) -> &ServiceCheckStatus {
        &self.status
    }

    /// Gets a reference to the timestamp
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Gets a reference to the host name
    pub fn host(&self) -> &String {
        &self.host
    }

    /// Gets a reference to the message
    pub fn message(&self) -> &String {
        &self.message
    }

    /// Gets a reference to the tags
    pub fn tags(&self) -> &Vec<String> {
        &self.tags
    }
}
