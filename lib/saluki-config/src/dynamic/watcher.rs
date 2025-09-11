//! A watcher for a specific configuration key.

use std::future::pending as pending_forever;

use serde::de::DeserializeOwned;
use tokio::sync::broadcast;

use crate::dynamic::ConfigChangeEvent;

/// A watcher for a specific configuration key.
///
/// It filters [`ConfigChangeEvent`]s down to the
/// requested key.
///
/// If dynamic configuration is disabled, [`changed`](Self::changed) will wait indefinitely and never yield.
pub struct FieldUpdateWatcher {
    /// The configuration key to watch for updates.
    pub key: String,
    /// Receiver of global configuration change events (None when dynamic is disabled).
    pub rx: Option<broadcast::Receiver<ConfigChangeEvent>>,
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
                    // Attempt to deserialize old/new; tolerate failures by setting None.
                    let old_t = match event.old_value {
                        Some(ref ov) => serde_json::from_value::<T>(ov.clone()).ok(),
                        None => None,
                    };
                    let new_t = match event.new_value {
                        Some(ref nv) => serde_json::from_value::<T>(nv.clone()).ok(),
                        None => None,
                    };

                    if new_t.is_some() || old_t.is_some() {
                        return (old_t, new_t);
                    }
                }
                // Ignore other key changes.
                Ok(_) => continue,
                Err(_) => {
                    // Keep pending forever to match "might never fire" semantics.
                    pending_forever::<()>().await;
                    unreachable!();
                }
            }
        }
    }
}
