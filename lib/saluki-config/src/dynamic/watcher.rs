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
                    if let Some(new_ref) = new_ref {
                        warn!(
                            key = %self.key,
                            expected = %std::any::type_name::<T>(),
                            actual = %get_type_name(new_ref),
                            "FieldUpdateWatcher failed to deserialize new value. Skipping update."
                        );
                    }
                }
                // Ignore other key changes.
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    saluki_antithesis::unreachable!(
                        "config filter update dropped (broadcast Lagged); live filtering may stay stale",
                        { "key": self.key.to_string() }
                    );
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
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::broadcast;

    use super::FieldUpdateWatcher;
    use crate::dynamic::event::{ConfigChangeEvent, ConfigUpdate};
    use crate::ConfigurationLoader;

    fn change_event(key: &str, new_value: serde_json::Value) -> ConfigChangeEvent {
        ConfigChangeEvent {
            key: key.to_string(),
            old_value: None,
            new_value: Some(new_value),
        }
    }

    fn watcher_over(key: &str, capacity: usize) -> (broadcast::Sender<ConfigChangeEvent>, FieldUpdateWatcher) {
        let (tx, rx) = broadcast::channel(capacity);
        (
            tx,
            FieldUpdateWatcher {
                key: key.to_string(),
                rx: Some(rx),
            },
        )
    }

    #[tokio::test]
    async fn changed_returns_partial_update_for_watched_key() {
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(serde_json::json!({ "foobar": { "a": false, "b": "c" } })),
            None,
            true,
        )
        .await;
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
    async fn changed_returns_nested_key_update() {
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(serde_json::json!({ "foobar": { "a": false, "b": "c" } })),
            None,
            true,
        )
        .await;
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
    async fn changed_returns_update_when_parent_object_changes() {
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(serde_json::json!({ "foobar": { "a": false, "b": "c" } })),
            None,
            true,
        )
        .await;
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

    #[tokio::test]
    async fn changed_waits_forever_when_dynamic_configuration_disabled() {
        // With no receiver (dynamic configuration disabled), `changed` must never resolve.
        let mut watcher = FieldUpdateWatcher {
            key: "watched_key".to_string(),
            rx: None,
        };

        let result = tokio::time::timeout(Duration::from_millis(250), watcher.changed::<String>()).await;
        assert!(
            result.is_err(),
            "changed() must stay pending when dynamic config is disabled"
        );
    }

    #[tokio::test]
    async fn changed_waits_forever_after_channel_closes() {
        // Once the broadcast sender is dropped, `recv` yields `Closed`; the watcher then matches "might never fire"
        // semantics by staying pending rather than returning.
        let (tx, mut watcher) = watcher_over("watched_key", 4);
        drop(tx);

        let result = tokio::time::timeout(Duration::from_millis(250), watcher.changed::<String>()).await;
        assert!(result.is_err(), "changed() must stay pending after the channel closes");
    }

    #[tokio::test]
    async fn changed_skips_lagged_errors_and_returns_next_event() {
        // A capacity-1 channel with three unread sends forces the receiver to lag: the first `recv` returns
        // `Lagged`, which the watcher skips before returning the most recent retained event.
        let (tx, mut watcher) = watcher_over("watched_key", 1);
        tx.send(change_event("watched_key", json!("first"))).unwrap();
        tx.send(change_event("watched_key", json!("second"))).unwrap();
        tx.send(change_event("watched_key", json!("third"))).unwrap();

        let (old, new) = tokio::time::timeout(Duration::from_secs(2), watcher.changed::<String>())
            .await
            .expect("changed() should resolve after skipping the lagged error");

        assert_eq!(old, None);
        assert_eq!(new, Some("third".to_string()));
    }

    #[tokio::test]
    async fn changed_skips_events_that_fail_to_deserialize() {
        // A value that can't be deserialized into the requested type is skipped (neither old nor new deserializes),
        // and the watcher keeps waiting until a well-typed value for the same key arrives.
        let (tx, mut watcher) = watcher_over("watched_key", 8);
        tx.send(change_event("watched_key", json!("not-a-number"))).unwrap();
        tx.send(change_event("watched_key", json!(42))).unwrap();

        let (old, new) = tokio::time::timeout(Duration::from_secs(2), watcher.changed::<u32>())
            .await
            .expect("changed() should resolve once a well-typed value arrives");

        assert_eq!(old, None);
        assert_eq!(new, Some(42));
    }
}
