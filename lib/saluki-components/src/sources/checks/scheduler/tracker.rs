use std::collections::HashMap;
use std::sync::RwLock;
use tracing::{error, info};

use super::*;

pub struct RunningCheckTracker {
    // FIXME if getting the actual check isn't needed, not to store the Arc
    running: RwLock<HashMap<String, Arc<dyn Check + Send + Sync>>>,
}

impl RunningCheckTracker {
    pub fn new() -> Self {
        Self {
            running: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_check(&self, check: Arc<dyn Check + Send + Sync>) -> bool {
        let id = check.id();

        let mut running = self.running.write().unwrap();
        if running.contains_key(id) {
            info!(check.id = id, "Check already present.");
            return false;
        }
        running.insert(id.to_string(), check.clone());
        true
    }

    pub fn remove_check(&self, check: Arc<dyn Check + Send + Sync>) {
        let id = check.id();

        let mut running = self.running.write().unwrap();
        let maybe_check = running.remove(id);
        if maybe_check.is_none() {
            error!(check.id = id, "Check is not tracked.")
        }
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.read().unwrap().contains_key(id)
    }

    pub fn running(&self) -> usize {
        self.running.read().unwrap().len()
    }
}
