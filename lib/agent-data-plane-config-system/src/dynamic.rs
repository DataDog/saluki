//! Dynamic typed-config router.
//!
//! Watches for raw-map update signals on [`GenericConfiguration`], re-translates the full Datadog
//! config, atomically swaps the current [`SalukiConfiguration`], and publishes changed slices to
//! their [`DynamicConfig`] handles.

use std::sync::Arc;

use agent_data_plane_config::{SalukiConfiguration, SalukiOnlyConfiguration};
use arc_swap::ArcSwap;
use datadog_agent_config::DatadogConfiguration;
use saluki_component_config::dogstatsd::PrefixFilterConfig;
use saluki_component_config::DynamicConfig;
use saluki_config_tools::GenericConfiguration;
use tokio::sync::watch;
use tracing::warn;

use crate::translate::translate;

/// Typed dynamic config handles exposed to components.
///
/// Each field is a [`DynamicConfig`] handle for one native config slice. The handle is either
/// [`DynamicConfig::Live`] (when the retained [`GenericConfiguration`] has an update subscription)
/// or [`DynamicConfig::Fixed`] (when dynamic updates are disabled).
pub struct DynamicConfigHandles {
    /// Live handle for the DogStatsD prefix-filter config slice.
    pub prefix_filter: DynamicConfig<PrefixFilterConfig>,
}

/// Starts the dynamic config router if the retained map supports update subscriptions.
///
/// Returns the handles and (when live) spawns a background task that re-translates on each raw-map
/// update. When dynamic updates are disabled, returns fixed handles seeded from `initial`.
pub(crate) fn start_dynamic_router(
    config: &GenericConfiguration, current: &Arc<ArcSwap<SalukiConfiguration>>, saluki_only: &SalukiOnlyConfiguration,
) -> DynamicConfigHandles {
    let initial_pf = current.load().components.dogstatsd.prefix_filter.clone();

    let events_rx = config.subscribe_for_updates();
    if events_rx.is_none() {
        return DynamicConfigHandles {
            prefix_filter: DynamicConfig::fixed(initial_pf),
        };
    }
    let mut events_rx = events_rx.unwrap();

    let (pf_tx, mut pf_rx) = watch::channel(initial_pf.clone());
    // Mark the initial value as seen so `changed()` only wakes on actual updates.
    pf_rx.borrow_and_update();

    let current = Arc::clone(current);
    let config = config.clone();
    let saluki_only = saluki_only.clone();

    tokio::spawn(async move {
        loop {
            let event = events_rx.recv().await;
            match event {
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Missed signals; the retained map reflects the latest state.
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }

            let datadog: DatadogConfiguration = match config.as_typed() {
                Ok(d) => d,
                Err(e) => {
                    warn!(error = %e, "Dynamic config translation failed to parse; keeping last-good config.");
                    continue;
                }
            };

            let new_saluki = match translate(&saluki_only, &datadog) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Dynamic config translation failed; keeping last-good config.");
                    continue;
                }
            };

            let new_pf = new_saluki.components.dogstatsd.prefix_filter.clone();

            current.store(Arc::new(new_saluki));

            pf_tx.send_if_modified(|current_pf| {
                if *current_pf != new_pf {
                    *current_pf = new_pf;
                    true
                } else {
                    false
                }
            });
        }
    });

    DynamicConfigHandles {
        prefix_filter: DynamicConfig::live(initial_pf, pf_rx),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_config_tools::{dynamic::ConfigUpdate, ConfigurationLoader};
    use serde_json::json;
    use tokio::time::timeout;

    use super::*;
    use crate::system::ConfigurationSystem;

    async fn system_with_dynamic(
        initial: serde_json::Value,
    ) -> (Arc<ConfigurationSystem>, tokio::sync::mpsc::Sender<ConfigUpdate>) {
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(initial), None, true).await;
        let sender = sender.expect("dynamic sender");
        sender.send(ConfigUpdate::Snapshot(json!({}))).await.unwrap();
        cfg.ready().await;

        let system = Arc::new(ConfigurationSystem::load(cfg).start().await.expect("start succeeds"));
        (system, sender)
    }

    #[tokio::test]
    async fn dynamic_update_wakes_prefix_handle() {
        let (system, sender) = system_with_dynamic(json!({})).await;
        let mut handle = system.dynamic_handles().prefix_filter.clone();
        assert!(handle.is_live());

        sender
            .send(ConfigUpdate::Partial {
                key: "metric_filterlist".to_string(),
                value: json!(["test.prefix"]),
            })
            .await
            .unwrap();

        timeout(Duration::from_secs(2), handle.changed())
            .await
            .expect("handle should wake on metric_filterlist update");

        let snap = handle.current();
        assert_eq!(snap.metric_filterlist, vec!["test.prefix"]);
    }

    #[tokio::test]
    async fn repeated_same_slice_does_not_wake_handle() {
        let (system, sender) = system_with_dynamic(json!({})).await;
        let mut handle = system.dynamic_handles().prefix_filter.clone();

        // Set metric_filterlist to ["a"] to establish a baseline.
        sender
            .send(ConfigUpdate::Partial {
                key: "metric_filterlist".to_string(),
                value: json!(["a"]),
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(2), handle.changed())
            .await
            .expect("first update should wake");
        assert_eq!(handle.current().metric_filterlist, vec!["a"]);

        // Send the same value again; the handle should not wake.
        sender
            .send(ConfigUpdate::Partial {
                key: "metric_filterlist".to_string(),
                value: json!(["a"]),
            })
            .await
            .unwrap();

        // Give the router time to process before asserting.
        tokio::task::yield_now().await;
        let result = timeout(Duration::from_millis(200), handle.changed()).await;
        assert!(result.is_err(), "handle should not wake when slice is unchanged");
    }

    #[tokio::test]
    async fn unrelated_key_update_does_not_wake_handle() {
        let (system, sender) = system_with_dynamic(json!({})).await;
        let mut handle = system.dynamic_handles().prefix_filter.clone();

        sender
            .send(ConfigUpdate::Partial {
                key: "log_level".to_string(),
                value: json!("debug"),
            })
            .await
            .unwrap();

        let result = timeout(Duration::from_millis(200), handle.changed()).await;
        assert!(result.is_err(), "handle should not wake on unrelated key");
    }

    #[tokio::test]
    async fn malformed_update_keeps_last_good_config() {
        let (system, sender) = system_with_dynamic(json!({ "metric_filterlist": ["original"] })).await;
        let mut handle = system.dynamic_handles().prefix_filter.clone();

        let original = handle.current();
        assert_eq!(original.metric_filterlist, vec!["original"]);

        // Send a value that will cause DatadogConfiguration parse to accept but translation to
        // produce a different model; verify the original is retained if we send something that
        // changes a different field, then confirm we can still get updates after recovery.
        sender
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_tag_cardinality".to_string(),
                value: json!("bogus_not_a_real_cardinality"),
            })
            .await
            .unwrap();

        // The malformed cardinality causes a translation error; the last-good config is retained.
        let result = timeout(Duration::from_millis(200), handle.changed()).await;
        assert!(result.is_err(), "handle should not wake on malformed update");

        assert_eq!(handle.current().metric_filterlist, vec!["original"]);
        assert_eq!(
            system.saluki().components.dogstatsd.prefix_filter.metric_filterlist,
            vec!["original"]
        );
    }

    #[tokio::test]
    async fn no_dynamic_returns_fixed_handles() {
        let cfg = ConfigurationLoader::for_tests(Some(json!({})), None, false).await.0;

        let system = ConfigurationSystem::load(cfg).start().await.expect("start succeeds");

        assert!(!system.dynamic_handles().prefix_filter.is_live());
    }

    #[tokio::test]
    async fn saluki_reflects_dynamic_update() {
        let (system, sender) = system_with_dynamic(json!({})).await;

        sender
            .send(ConfigUpdate::Partial {
                key: "metric_filterlist".to_string(),
                value: json!(["updated"]),
            })
            .await
            .unwrap();

        let mut handle = system.dynamic_handles().prefix_filter.clone();
        timeout(Duration::from_secs(2), handle.changed())
            .await
            .expect("handle should wake");

        assert_eq!(
            system.saluki().components.dogstatsd.prefix_filter.metric_filterlist,
            vec!["updated"]
        );
    }
}
