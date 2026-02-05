use super::Tags;

// integrations-core/datadog_checks_base/datadog_checks/base/types.py
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Status {
    Ok = 0,
    Warning,
    Critical,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ServiceCheck {
    pub name: String,
    pub status: Status,
    pub tags: Tags,
    pub message: String,
}
