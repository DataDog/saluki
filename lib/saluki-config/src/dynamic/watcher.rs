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

#[cfg(test)]
mod tests {
    use crate::dynamic::event::ConfigUpdate;
    use crate::ConfigurationLoader;

    #[tokio::test]
    async fn test_basic_field_update_watcher() {
        let (cfg, sender) = ConfigurationLoader::for_tests(true).await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("watched_key");

        sender
            .send(ConfigUpdate::Partial {
                key: "watched_key".to_string(),
                value: serde_json::json!("hello"),
            })
            .await
            .unwrap();

        let (old, new) = tokio::time::timeout(std::time::Duration::from_secs(2), watcher.changed::<String>())
            .await
            .expect("timed out waiting for watched_key update");

        assert_eq!(old, None);
        assert_eq!(new, Some("hello".to_string()));
    }

    #[tokio::test]
    async fn test_field_update_watcher_nested_key() {
        let (cfg, sender) = ConfigurationLoader::for_tests(true).await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("foobar.a");

        // Update nested value via dotted path
        sender
            .send(ConfigUpdate::Partial {
                key: "foobar.a".to_string(),
                value: serde_json::json!(true),
            })
            .await
            .unwrap();

        let (old, new) = tokio::time::timeout(std::time::Duration::from_secs(2), watcher.changed::<bool>())
            .await
            .expect("timed out waiting for foobar.a update");

        assert_eq!(old, Some(false));
        assert_eq!(new, Some(true));
        assert!(cfg.get_typed::<bool>("foobar.a").unwrap());

        // Existing nested key not updated is still present
        assert_eq!(cfg.get_typed::<String>("foobar.b").unwrap(), "c");
    }

    #[tokio::test]
    async fn test_field_update_watcher_parent_update() {
        let (cfg, sender) = ConfigurationLoader::for_tests(true).await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("foobar.a");

        // Update parent object directly
        sender
            .send(ConfigUpdate::Partial {
                key: "foobar".to_string(),
                value: serde_json::json!({ "a": true }),
            })
            .await
            .unwrap();

        let (old, new) = tokio::time::timeout(std::time::Duration::from_secs(2), watcher.changed::<bool>())
            .await
            .expect("timed out waiting for foobar.a update");

        assert_eq!(old, Some(false));
        assert_eq!(new, Some(true));
        assert!(cfg.get_typed::<bool>("foobar.a").unwrap());

        // Existing nested key not updated is still present
        assert_eq!(cfg.get_typed::<String>("foobar.b").unwrap(), "c");
    }
}
