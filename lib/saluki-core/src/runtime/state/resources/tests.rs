use std::{
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
    time::Duration,
};

use saluki_error::generic_error;

use super::*;

/// Hands every created instance a process-unique serial.
///
/// Tests assert on serials rather than on a creation count, so they stay correct when the test binary runs them in
/// parallel: a serial identifies one specific instance no matter what else is happening in the process.
static NEXT_SERIAL: AtomicUsize = AtomicUsize::new(0);

fn next_serial() -> usize {
    NEXT_SERIAL.fetch_add(1, Relaxed)
}

#[derive(Debug)]
struct Counter {
    serial: usize,
}

#[derive(Debug)]
struct Widget;

/// A resource that is internally several handles, standing in for something like a set of sockets bound to one address
/// with `SO_REUSEPORT`. The registry knows nothing about the multiplicity.
#[derive(Debug)]
struct Bundle {
    serials: Vec<usize>,
}

const BUNDLE_HANDLES: usize = 4;

#[derive(Clone, Debug)]
struct CounterSpec {
    key: MetaString,
    fail: bool,
    create_delay: Option<Duration>,
    setting: u32,
}

impl CounterSpec {
    fn new(key: &str) -> Self {
        Self {
            key: MetaString::from(key),
            fail: false,
            create_delay: None,
            setting: 0,
        }
    }

    fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    fn with_create_delay(mut self, delay: Duration) -> Self {
        self.create_delay = Some(delay);
        self
    }

    fn with_setting(mut self, setting: u32) -> Self {
        self.setting = setting;
        self
    }
}

#[async_trait]
impl ResourceSpecification for CounterSpec {
    type Resource = Counter;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        self.key.clone()
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        if let Some(delay) = self.create_delay {
            tokio::time::sleep(delay).await;
        }

        if self.fail {
            return Err(generic_error!("creation deliberately failed"));
        }

        Ok(Counter { serial: next_serial() })
    }
}

/// Identical in kind and key to [`CounterSpec`], but naming a different resource type, so that the two collide.
#[derive(Clone, Debug)]
struct WidgetSpec {
    key: MetaString,
}

impl WidgetSpec {
    fn new(key: &str) -> Self {
        Self {
            key: MetaString::from(key),
        }
    }
}

#[async_trait]
impl ResourceSpecification for WidgetSpec {
    type Resource = Widget;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        self.key.clone()
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        Ok(Widget)
    }
}

/// Shares [`CounterSpec`]'s keys but sits under a different kind, so the two must never collide.
#[derive(Clone, Debug)]
struct OtherKindSpec {
    key: MetaString,
}

impl OtherKindSpec {
    fn new(key: &str) -> Self {
        Self {
            key: MetaString::from(key),
        }
    }
}

#[async_trait]
impl ResourceSpecification for OtherKindSpec {
    type Resource = Widget;

    const KIND: ResourceKind = ResourceKind::Test;

    fn key(&self) -> MetaString {
        self.key.clone()
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        Ok(Widget)
    }
}

/// A resource carrying state that accumulates within a single lease, standing in for a listener's cursor over the
/// sockets it has handed out.
#[derive(Debug)]
struct Dispenser {
    serial: usize,
    handed_out: usize,
}

#[derive(Clone, Debug)]
struct DispenserSpec {
    key: MetaString,
}

impl DispenserSpec {
    fn new(key: &str) -> Self {
        Self {
            key: MetaString::from(key),
        }
    }
}

#[async_trait]
impl ResourceSpecification for DispenserSpec {
    type Resource = Dispenser;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        self.key.clone()
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        Ok(Dispenser {
            serial: next_serial(),
            handed_out: 0,
        })
    }

    fn reset(resource: &mut Self::Resource) {
        resource.handed_out = 0;
    }
}

#[derive(Clone, Debug)]
struct BundleSpec {
    key: MetaString,
}

impl BundleSpec {
    fn new(key: &str) -> Self {
        Self {
            key: MetaString::from(key),
        }
    }
}

#[async_trait]
impl ResourceSpecification for BundleSpec {
    type Resource = Bundle;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        self.key.clone()
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        Ok(Bundle {
            serials: (0..BUNDLE_HANDLES).map(|_| next_serial()).collect(),
        })
    }
}

fn owner(name: &str) -> SubsystemIdentifier {
    SubsystemIdentifier::from_segments(["test", name])
}

#[tokio::test]
async fn acquire_creates_and_leases() {
    let registry = ResourceRegistry::new();
    let lease = registry
        .acquire(&owner("dsd_in"), CounterSpec::new("counter://a"))
        .await
        .expect("should acquire");

    assert_eq!(lease.key().as_ref(), "counter://a");

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].key, "counter://a");
    assert_eq!(snapshot[0].kind, ResourceKind::Socket);
    assert_eq!(snapshot[0].state, "leased");
    assert_eq!(snapshot[0].owner.as_deref(), Some("test.dsd_in"));
    assert_eq!(
        snapshot[0].acquisition_process_id,
        Some(ProcessId::current().as_usize())
    );
    assert_eq!(snapshot[0].acquisitions, 1);
}

#[tokio::test]
async fn second_acquire_while_leased_is_refused() {
    let registry = ResourceRegistry::new();
    let _held = registry
        .acquire(&owner("first"), CounterSpec::new("counter://b"))
        .await
        .expect("should acquire");

    let err = registry
        .acquire(&owner("second"), CounterSpec::new("counter://b"))
        .await
        .expect_err("should be refused while leased");

    match err {
        AcquireError::AlreadyLeased { kind, key, owner, .. } => {
            assert_eq!(kind, ResourceKind::Socket);
            assert_eq!(key.as_ref(), "counter://b");
            assert_eq!(owner.as_ref(), "test.first");
        }
        other => panic!("expected AlreadyLeased, got {other:?}"),
    }
}

#[tokio::test]
async fn reacquire_returns_the_same_resource() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://c");

    let first = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let original = first.serial;
    drop(first);

    // The resource is back in the registry, so this must hand out the very same one rather than creating a new one.
    // This is the property the whole registry exists for.
    let second = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(second.serial, original);

    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].acquisitions, 2);
    assert_eq!(snapshot[0].owner.as_deref(), Some("test.second"));
}

#[tokio::test]
async fn returning_a_resource_marks_it_idle() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://d");

    let lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    drop(lease);

    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].state, "idle");
    assert!(snapshot[0].owner.is_none());
    assert!(snapshot[0].acquisition_process_id.is_none());
}

#[tokio::test]
async fn a_resource_of_many_handles_is_leased_as_one() {
    let registry = ResourceRegistry::new();
    let spec = BundleSpec::new("bundle://a");

    let first = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    assert_eq!(first.serials.len(), BUNDLE_HANDLES);
    let original = first.serials.clone();

    // Multiplicity lives inside the resource, so the registry still reports exactly one entry and refuses a second
    // acquirer on the strength of a single lease.
    assert_eq!(registry.snapshot().len(), 1);
    assert!(registry.acquire(&owner("second"), spec.clone()).await.is_err());

    drop(first);

    let second = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(second.serials, original);
}

#[tokio::test]
async fn the_same_key_under_two_kinds_does_not_collide() {
    let registry = ResourceRegistry::new();

    // Identical key strings, different kinds. A kind namespaces its own keys, so neither of these can block the other
    // and a specification author only has to keep keys unique within their own kind.
    let socket = registry
        .acquire(&owner("first"), CounterSpec::new("shared-key"))
        .await
        .expect("socket-kind resource should acquire");
    let other = registry
        .acquire(&owner("second"), OtherKindSpec::new("shared-key"))
        .await
        .expect("test-kind resource should acquire alongside it");

    assert_eq!(socket.kind(), ResourceKind::Socket);
    assert_eq!(other.kind(), ResourceKind::Test);
    assert_eq!(socket.key(), other.key());

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].kind, ResourceKind::Socket);
    assert_eq!(snapshot[1].kind, ResourceKind::Test);
}

#[tokio::test]
async fn creation_failure_leaves_no_entry_behind() {
    let registry = ResourceRegistry::new();

    let err = registry
        .acquire(&owner("first"), CounterSpec::new("counter://e").failing())
        .await
        .expect_err("creation should fail");
    assert!(matches!(err, AcquireError::CreationFailed { .. }));

    // A failed creation must not leave the key claimed, otherwise a transient failure would block it forever.
    assert!(registry.snapshot().is_empty());
    registry
        .acquire(&owner("second"), CounterSpec::new("counter://e"))
        .await
        .expect("retry should succeed");
}

#[tokio::test]
async fn same_key_with_a_different_resource_type_is_refused() {
    let registry = ResourceRegistry::new();
    let _held = registry
        .acquire(&owner("first"), CounterSpec::new("counter://g"))
        .await
        .expect("should acquire");

    let err = registry
        .acquire(&owner("second"), WidgetSpec::new("counter://g"))
        .await
        .expect_err("a different type on the same key should be refused");

    match err {
        AcquireError::TypeMismatch {
            kind,
            key,
            existing_type,
            requested_type,
        } => {
            assert_eq!(kind, ResourceKind::Socket);
            assert_eq!(key.as_ref(), "counter://g");
            assert!(existing_type.ends_with("Counter"), "got {existing_type}");
            assert!(requested_type.ends_with("Widget"), "got {requested_type}");
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn type_mismatch_is_detected_even_when_idle() {
    let registry = ResourceRegistry::new();
    drop(
        registry
            .acquire(&owner("first"), CounterSpec::new("counter://h"))
            .await
            .expect("should acquire"),
    );

    let err = registry
        .acquire(&owner("second"), WidgetSpec::new("counter://h"))
        .await
        .expect_err("a different type on the same key should be refused");
    assert!(matches!(err, AcquireError::TypeMismatch { .. }));
}

#[tokio::test]
async fn concurrent_acquire_during_creation_does_not_double_create() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://i").with_create_delay(Duration::from_millis(100));

    let first = tokio::spawn({
        let registry = registry.clone();
        let spec = spec.clone();
        async move { registry.acquire(&owner("first"), spec).await }
    });

    // Give the first acquire time to stake its claim on the key, but not to finish creating.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let second = registry.acquire(&owner("second"), spec).await;
    assert!(
        matches!(second, Err(AcquireError::AlreadyLeased { .. })),
        "an in-flight creation should be visible to a concurrent acquirer"
    );

    let first = first.await.expect("task should not panic").expect("should acquire");
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].acquisitions, 1);
    drop(first);
}

#[tokio::test]
async fn cancelling_an_acquisition_releases_its_claim() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://r").with_create_delay(Duration::from_secs(30));

    // Drop the acquisition while creation is still pending, the way a supervisor aborts a worker that is still
    // initializing when shutdown arrives.
    let timed_out = tokio::time::timeout(Duration::from_millis(50), registry.acquire(&owner("first"), spec)).await;
    assert!(timed_out.is_err(), "the acquisition should still have been creating");

    // The claim must not outlive the acquisition that staked it, or the key would be blocked for the life of the
    // process and no later caller could ever bind it.
    assert!(registry.snapshot().is_empty());

    let lease = registry
        .acquire(&owner("second"), CounterSpec::new("counter://r"))
        .await
        .expect("should acquire after the cancelled attempt");
    assert_eq!(lease.key().as_ref(), "counter://r");
}

#[tokio::test]
async fn discard_recreates_on_the_next_acquire() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://j");

    let lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let original = lease.serial;
    lease.discard();

    // A discarded resource is gone entirely, so the next acquire builds a fresh one.
    assert!(registry.snapshot().is_empty());

    let rebuilt = registry.acquire(&owner("second"), spec).await.expect("should acquire");
    assert_ne!(rebuilt.serial, original);
}

#[tokio::test]
async fn release_returns_the_resource_immediately() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://l");

    let lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    lease.release();

    assert_eq!(registry.snapshot()[0].state, "idle");
    registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should acquire after release");
}

#[tokio::test]
async fn differing_settings_reuse_the_existing_resource() {
    let registry = ResourceRegistry::new();

    let first = registry
        .acquire(&owner("first"), CounterSpec::new("counter://m").with_setting(1))
        .await
        .expect("should acquire");
    let original = first.serial;
    drop(first);

    // The key identifies the resource, so a setting that isn't part of the key can't cause a rebuild.
    let second = registry
        .acquire(&owner("second"), CounterSpec::new("counter://m").with_setting(99))
        .await
        .expect("should acquire");
    assert_eq!(second.serial, original);
}

#[tokio::test]
async fn mutations_through_a_lease_survive_the_round_trip() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://n");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    lease.serial = 4242;
    drop(lease);

    let reacquired = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(reacquired.serial, 4242);
}

#[tokio::test]
async fn per_lease_state_is_reset_when_the_resource_returns() {
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://a");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let serial = lease.serial;
    lease.handed_out = 3;
    drop(lease);

    let lease = registry
        .acquire(&owner("second"), spec)
        .await
        .expect("should reacquire");

    // The resource itself survives -- that is the whole point of the registry ...
    assert_eq!(lease.serial, serial);

    // ... but state that only made sense to the previous holder starts clean, so a re-acquired listener hands out its
    // sockets again rather than looking exhausted.
    assert_eq!(lease.handed_out, 0);
}

#[tokio::test]
async fn snapshot_is_ordered_by_key() {
    let registry = ResourceRegistry::new();
    for key in ["counter://q_c", "counter://q_a", "counter://q_b"] {
        drop(
            registry
                .acquire(&owner("first"), CounterSpec::new(key))
                .await
                .expect("should acquire"),
        );
    }

    let keys = registry
        .snapshot()
        .into_iter()
        .map(|status| status.key)
        .collect::<Vec<_>>();
    assert_eq!(keys, vec!["counter://q_a", "counter://q_b", "counter://q_c"]);
}
