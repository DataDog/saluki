//! A watcher for a specific configuration key.

use std::future::pending as pending_forever;

use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
use tracing::warn;

use crate::dynamic::ConfigChangeEvent;

/// A watcher for a specific configuration key.
///
/// It filters [`ConfigChangeEvent`]s down to the
/// requested key.
///
/// If dynamic configuration is disabled, [`changed`](Self::changed) will wait indefinitely and never yield.
pub struct FieldUpdateWatcher {
    /// The configuration key to watch for updates.
    pub(crate) key: String,
    /// Receiver of global configuration change events (None when dynamic is disabled).
    pub(crate) rx: Option<broadcast::Receiver<ConfigChangeEvent>>,
}

impl FieldUpdateWatcher {
    /// Waits until the watched key changes and returns a typed (old, new) tuple.
    pub async fn changed<T>(&mut self) -> (Option<T>, Option<T>)
    where
        T: DeserializeOwned,
    {
        if self.rx.is_none() {
            pending_forever::<()>().await;
            unreachable!();
        }

        let rx = self.rx.as_mut().unwrap();
        loop {
            match rx.recv().await {
                Ok(event) if event.key == self.key => {
                    let old_ref = event.old_value.as_ref();
                    let new_ref = event.new_value.as_ref();

                    let old_t = old_ref.and_then(|ov| serde_json::from_value::<T>(ov.clone()).ok());
                    let new_t = new_ref.and_then(|nv| serde_json::from_value::<T>(nv.clone()).ok());

                    if new_t.is_some() || old_t.is_some() {
                        return (old_t, new_t);
                    }

                    // If a new value was present but failed to deserialize, warn so we don't silently hide updates.
                    if new_ref.is_some() {
                        warn!(
                            key = %self.key,
                            expected = %std::any::type_name::<T>(),
                            actual = %get_type_name(new_ref.as_ref().unwrap()),
                            "FieldUpdateWatcher failed to deserialize new value. Skipping update."
                        );
                    }
                }
                // Ignore other key changes.
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    warn!(
                        "FieldUpdateWatcher dropped events for key: {}. Continuing to wait for the next event.",
                        self.key
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Keep pending forever to match "might never fire" semantics.
                    pending_forever::<()>().await;
                    unreachable!();
                }
            }
        }
    }
}

fn get_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
