use std::sync::{Arc, Mutex};

use serde::Deserialize;
use stele::ServiceCheck;

#[derive(Clone)]
pub struct ServiceChecksState {
    checks: Arc<Mutex<Vec<ServiceCheck>>>,
}

/// A single item from the `/api/v1/check_run` JSON array payload.
#[derive(Deserialize)]
pub struct CheckRunItem {
    #[serde(rename = "check")]
    pub name: String,
    pub status: u8,
    #[serde(rename = "host_name", default)]
    pub hostname: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub timestamp: Option<u64>,
}

impl ServiceChecksState {
    pub fn new() -> Self {
        Self {
            checks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn dump_checks(&self) -> Vec<ServiceCheck> {
        self.checks.lock().unwrap().clone()
    }

    pub fn merge_check_run_payload(&self, items: Vec<CheckRunItem>) {
        let new_checks: Vec<ServiceCheck> = items
            .into_iter()
            .map(|item| {
                ServiceCheck::from_check_run(
                    item.name,
                    item.status,
                    item.hostname,
                    item.message,
                    item.tags,
                    item.timestamp,
                )
            })
            .collect();

        self.checks.lock().unwrap().extend(new_checks);
    }
}
