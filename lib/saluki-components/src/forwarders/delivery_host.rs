//! Config-driven dynamic delivery host managing live recipient I/O loops.
//!
//! Manages recipient endpoints, API keys, proxies, and failover targets as live resources without restarting the
//! topology:
//! - Adding a recipient spawns a bounded-channel I/O worker loop.
//! - Rotating an API key updates the worker's live credential in place without draining or restarting it.
//! - Changing an endpoint or proxy spawns the replacement worker, swaps the route atomically, and gracefully drains
//!   and retires the previous worker.
//! - Removing a recipient disconnects its route, drains in-flight payloads, and joins its task so resources are
//!   reclaimed.

use std::{collections::BTreeMap, fmt, future::Future, pin::Pin, sync::Arc};

use saluki_error::{generic_error, GenericError};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::debug;

const DEFAULT_RECIPIENT_QUEUE_CAPACITY: usize = 64;

/// Stable logical identifier for a delivery recipient (for example, `"primary"` or `"mrf"`).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientId(pub String);

impl RecipientId {
    /// Creates a new `RecipientId`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RecipientId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Display for RecipientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Desired live specification for one delivery recipient.
#[derive(Clone, Eq, PartialEq)]
pub struct RecipientSpec {
    /// Stable logical recipient ID.
    pub id: RecipientId,

    /// Destination endpoint URI.
    pub endpoint: String,

    /// Current authentication API key (redacted in `Debug` output).
    pub api_key: String,

    /// Optional HTTP proxy URI for this recipient.
    pub proxy: Option<String>,

    /// Optional fallback recipient ID used when this recipient is unavailable or draining.
    pub failover_target: Option<RecipientId>,
}

impl RecipientSpec {
    /// Returns `true` when `other` shares the same transport configuration (`endpoint` and `proxy`), meaning only
    /// credentials or failover metadata changed and the existing I/O worker can be reused in place.
    pub fn same_transport(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint && self.proxy == other.proxy
    }
}

impl fmt::Debug for RecipientSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecipientSpec")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("api_key", &"<redacted>")
            .field("proxy", &self.proxy)
            .field("failover_target", &self.failover_target)
            .finish()
    }
}

/// Summary of actions taken by [`DeliveryHost::reconcile`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconcileReport {
    /// Newly added recipient IDs.
    pub added: Vec<RecipientId>,
    /// Recipients whose credentials or failover metadata updated in place without restarting their worker.
    pub updated_in_place: Vec<RecipientId>,
    /// Recipients whose transport (`endpoint`/`proxy`) changed, triggering replacement and graceful drain.
    pub replaced: Vec<RecipientId>,
    /// Recipients removed and drained.
    pub retired: Vec<RecipientId>,
}

/// Context passed to a recipient sender callback on each payload delivery.
#[derive(Clone)]
pub struct DeliveryContext {
    /// Logical recipient ID.
    pub recipient_id: RecipientId,
    /// Destination endpoint URI.
    pub endpoint: String,
    /// Optional proxy URI.
    pub proxy: Option<String>,
    /// Snapshot of the recipient's live API key at send time.
    pub api_key: String,
}

impl fmt::Debug for DeliveryContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeliveryContext")
            .field("recipient_id", &self.recipient_id)
            .field("endpoint", &self.endpoint)
            .field("proxy", &self.proxy)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Async transport sink used by [`DeliveryHost`] workers to deliver one payload.
pub trait PayloadSink<P>: Send + Sync + 'static
where
    P: Send + 'static,
{
    /// Delivers `payload` using `ctx`.
    fn send(&self, ctx: DeliveryContext, payload: P) -> Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>;
}

struct ActiveRecipient<P> {
    spec: RecipientSpec,
    tx: mpsc::Sender<P>,
    api_key_tx: watch::Sender<String>,
    worker: JoinHandle<()>,
}

/// Config-driven dynamic delivery host managing live recipient I/O loops.
pub struct DeliveryHost<P, S>
where
    P: Send + 'static,
    S: PayloadSink<P>,
{
    sink: Arc<S>,
    queue_capacity: usize,
    recipients: BTreeMap<RecipientId, ActiveRecipient<P>>,
    failover_routes: BTreeMap<RecipientId, RecipientId>,
}

impl<P, S> DeliveryHost<P, S>
where
    P: Send + 'static,
    S: PayloadSink<P>,
{
    /// Creates a new `DeliveryHost` backed by `sink`.
    pub fn new(sink: Arc<S>) -> Self {
        Self::with_queue_capacity(sink, DEFAULT_RECIPIENT_QUEUE_CAPACITY)
    }

    /// Creates a new `DeliveryHost` with a custom per-recipient bounded channel capacity.
    pub fn with_queue_capacity(sink: Arc<S>, queue_capacity: usize) -> Self {
        Self {
            sink,
            queue_capacity: queue_capacity.max(1),
            recipients: BTreeMap::new(),
            failover_routes: BTreeMap::new(),
        }
    }

    /// Returns the number of currently active recipient workers.
    pub fn active_recipient_count(&self) -> usize {
        self.recipients.len()
    }

    /// Returns the IDs of all active recipients.
    pub fn active_recipient_ids(&self) -> Vec<RecipientId> {
        self.recipients.keys().cloned().collect()
    }

    /// Reconciles active recipients against `desired`, spawning new workers, updating credentials in place, and
    /// gracefully draining/joining replaced or retired workers.
    pub async fn reconcile(&mut self, desired: Vec<RecipientSpec>) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let mut desired_by_id: BTreeMap<RecipientId, RecipientSpec> = BTreeMap::new();
        for spec in desired {
            desired_by_id.insert(spec.id.clone(), spec);
        }

        // Collect retired workers so we can drain and join them after swapping active routes.
        let mut draining_workers: Vec<JoinHandle<()>> = Vec::new();

        // 1. Retire recipients absent from `desired`.
        let to_retire: Vec<RecipientId> = self
            .recipients
            .keys()
            .filter(|id| !desired_by_id.contains_key(*id))
            .cloned()
            .collect();
        for id in to_retire {
            if let Some(old) = self.recipients.remove(&id) {
                drop(old.tx);
                draining_workers.push(old.worker);
                report.retired.push(id);
            }
        }

        // 2. Add new recipients, update credentials in place, or replace changed transports.
        for (id, spec) in desired_by_id {
            match self.recipients.get_mut(&id) {
                None => {
                    let active = self.spawn_recipient(spec);
                    self.recipients.insert(id.clone(), active);
                    report.added.push(id);
                }
                Some(existing) if existing.spec.same_transport(&spec) => {
                    if existing.spec.api_key != spec.api_key {
                        let _ = existing.api_key_tx.send(spec.api_key.clone());
                    }
                    if existing.spec != spec {
                        existing.spec = spec;
                        report.updated_in_place.push(id);
                    }
                }
                Some(_) => {
                    let replacement = self.spawn_recipient(spec);
                    if let Some(old) = self.recipients.insert(id.clone(), replacement) {
                        drop(old.tx);
                        draining_workers.push(old.worker);
                    }
                    report.replaced.push(id);
                }
            }
        }

        // Rebuild failover lookup table from active recipient specs.
        self.failover_routes.clear();
        for (id, active) in &self.recipients {
            if let Some(fallback) = &active.spec.failover_target {
                self.failover_routes.insert(id.clone(), fallback.clone());
            }
        }

        // Gracefully drain and join all retired or replaced workers so resources are reclaimed.
        for worker in draining_workers {
            let _ = worker.await;
        }

        report
    }

    /// Registers a static fallback mapping `from -> to` used if `from` is absent or retired.
    pub fn set_failover_route(&mut self, from: impl Into<RecipientId>, to: impl Into<RecipientId>) {
        self.failover_routes.insert(from.into(), to.into());
    }

    /// Delivers `payload` to `target`, falling back to a configured failover target if `target` is absent.
    pub async fn deliver(&self, target: &RecipientId, payload: P) -> Result<RecipientId, GenericError> {
        let resolved = if self.recipients.contains_key(target) {
            target
        } else if let Some(fallback) = self.failover_routes.get(target) {
            if self.recipients.contains_key(fallback) {
                fallback
            } else {
                return Err(generic_error!(
                    "recipient `{target}` and failover target `{fallback}` are both unavailable"
                ));
            }
        } else {
            return Err(generic_error!("unknown delivery recipient `{target}`"));
        };

        let active = &self.recipients[resolved];
        active
            .tx
            .send(payload)
            .await
            .map_err(|_| generic_error!("delivery recipient `{resolved}` closed before accepting payload"))?;

        Ok(resolved.clone())
    }

    /// Gracefully drains every active recipient and joins all worker tasks.
    pub async fn shutdown(mut self) {
        let workers: Vec<JoinHandle<()>> = self
            .recipients
            .drain_filter_all()
            .into_iter()
            .map(|active| {
                drop(active.tx);
                active.worker
            })
            .collect();

        for worker in workers {
            let _ = worker.await;
        }
    }

    fn spawn_recipient(&self, spec: RecipientSpec) -> ActiveRecipient<P> {
        let (tx, mut rx) = mpsc::channel::<P>(self.queue_capacity);
        let (api_key_tx, api_key_rx) = watch::channel(spec.api_key.clone());
        let sink = Arc::clone(&self.sink);
        let id = spec.id.clone();
        let endpoint = spec.endpoint.clone();
        let proxy = spec.proxy.clone();

        let worker = tokio::spawn(async move {
            debug!(recipient = %id, endpoint = %endpoint, "Delivery recipient worker started.");
            while let Some(payload) = rx.recv().await {
                let api_key = api_key_rx.borrow().clone();
                let ctx = DeliveryContext {
                    recipient_id: id.clone(),
                    endpoint: endpoint.clone(),
                    proxy: proxy.clone(),
                    api_key,
                };
                if let Err(e) = sink.send(ctx, payload).await {
                    debug!(recipient = %id, error = %e, "Delivery sink failed for payload.");
                }
            }
            debug!(recipient = %id, endpoint = %endpoint, "Delivery recipient worker drained and stopped.");
        });

        ActiveRecipient {
            spec,
            tx,
            api_key_tx,
            worker,
        }
    }
}

trait DrainAll<K, V> {
    fn drain_filter_all(&mut self) -> Vec<V>;
}

impl<K: Ord, V> DrainAll<K, V> for BTreeMap<K, V> {
    fn drain_filter_all(&mut self) -> Vec<V> {
        std::mem::take(self).into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedDelivery {
        recipient: String,
        endpoint: String,
        proxy: Option<String>,
        api_key: String,
        payload: String,
    }

    #[derive(Default)]
    struct RecordingIntake {
        deliveries: Mutex<Vec<RecordedDelivery>>,
    }

    impl RecordingIntake {
        fn snapshot(&self) -> Vec<RecordedDelivery> {
            self.deliveries.lock().expect("lock").clone()
        }
    }

    impl PayloadSink<String> for RecordingIntake {
        fn send(
            &self, ctx: DeliveryContext, payload: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>> {
            let record = RecordedDelivery {
                recipient: ctx.recipient_id.to_string(),
                endpoint: ctx.endpoint,
                proxy: ctx.proxy,
                api_key: ctx.api_key,
                payload,
            };
            self.deliveries.lock().expect("lock").push(record);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn live_add_credential_rotation_transport_replacement_and_retirement() {
        let intake = Arc::new(RecordingIntake::default());
        let mut host = DeliveryHost::new(Arc::clone(&intake));

        // 1. Add primary recipient.
        let r1 = host
            .reconcile(vec![RecipientSpec {
                id: RecipientId::from("primary"),
                endpoint: "https://intake-a.example".to_string(),
                api_key: "key-v1".to_string(),
                proxy: None,
                failover_target: None,
            }])
            .await;
        assert_eq!(r1.added, vec![RecipientId::from("primary")]);
        assert_eq!(host.active_recipient_count(), 1);

        host.deliver(&RecipientId::from("primary"), "p1".to_string())
            .await
            .unwrap();

        // Wait briefly for worker delivery before rotating credential so order is deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;

        // 2. Rotate API key in place (no worker restart or drain).
        let r2 = host
            .reconcile(vec![RecipientSpec {
                id: RecipientId::from("primary"),
                endpoint: "https://intake-a.example".to_string(),
                api_key: "key-v2".to_string(),
                proxy: None,
                failover_target: None,
            }])
            .await;
        assert_eq!(r2.updated_in_place, vec![RecipientId::from("primary")]);
        assert!(r2.replaced.is_empty());

        host.deliver(&RecipientId::from("primary"), "p2".to_string())
            .await
            .unwrap();

        // 3. Replace endpoint + proxy: old worker drains `p2` cleanly before retiring, new worker handles `p3`.
        let r3 = host
            .reconcile(vec![RecipientSpec {
                id: RecipientId::from("primary"),
                endpoint: "https://intake-b.example".to_string(),
                api_key: "key-v2".to_string(),
                proxy: Some("http://proxy.internal:3128".to_string()),
                failover_target: None,
            }])
            .await;
        assert_eq!(r3.replaced, vec![RecipientId::from("primary")]);

        host.deliver(&RecipientId::from("primary"), "p3".to_string())
            .await
            .unwrap();

        // 4. Retire recipient and verify full reclamation.
        let r4 = host.reconcile(vec![]).await;
        assert_eq!(r4.retired, vec![RecipientId::from("primary")]);
        assert_eq!(host.active_recipient_count(), 0);
        host.shutdown().await;

        let delivered = intake.snapshot();
        assert_eq!(delivered.len(), 3);
        assert_eq!(delivered[0].payload, "p1");
        assert_eq!(delivered[0].api_key, "key-v1");
        assert_eq!(delivered[0].endpoint, "https://intake-a.example");

        assert_eq!(delivered[1].payload, "p2");
        assert_eq!(delivered[1].api_key, "key-v2");
        assert_eq!(delivered[1].endpoint, "https://intake-a.example");

        assert_eq!(delivered[2].payload, "p3");
        assert_eq!(delivered[2].endpoint, "https://intake-b.example");
        assert_eq!(delivered[2].proxy.as_deref(), Some("http://proxy.internal:3128"));
    }

    #[tokio::test]
    async fn failover_routes_to_backup_when_primary_retired() {
        let intake = Arc::new(RecordingIntake::default());
        let mut host = DeliveryHost::new(Arc::clone(&intake));

        host.reconcile(vec![
            RecipientSpec {
                id: RecipientId::from("primary"),
                endpoint: "https://us1.example".to_string(),
                api_key: "k1".to_string(),
                proxy: None,
                failover_target: Some(RecipientId::from("mrf")),
            },
            RecipientSpec {
                id: RecipientId::from("mrf"),
                endpoint: "https://eu1.example".to_string(),
                api_key: "k2".to_string(),
                proxy: None,
                failover_target: None,
            },
        ])
        .await;

        // Retire primary and keep failover route pointing `primary -> mrf`.
        host.reconcile(vec![RecipientSpec {
            id: RecipientId::from("mrf"),
            endpoint: "https://eu1.example".to_string(),
            api_key: "k2".to_string(),
            proxy: None,
            failover_target: None,
        }])
        .await;
        host.set_failover_route("primary", "mrf");

        let delivered_to = host
            .deliver(&RecipientId::from("primary"), "failover-payload".to_string())
            .await
            .unwrap();
        assert_eq!(delivered_to, RecipientId::from("mrf"));

        host.shutdown().await;

        let delivered = intake.snapshot();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].recipient, "mrf");
        assert_eq!(delivered[0].payload, "failover-payload");
    }

    #[test]
    fn debug_redacts_api_keys() {
        let spec = RecipientSpec {
            id: RecipientId::from("primary"),
            endpoint: "https://intake.example".to_string(),
            api_key: "super-secret-api-key".to_string(),
            proxy: None,
            failover_target: None,
        };
        let rendered = format!("{spec:?}");
        assert!(!rendered.contains("super-secret-api-key"));
        assert!(rendered.contains("<redacted>"));
    }
}
