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

impl ResourceSpecification for CounterSpec {
    fn key(&self) -> MetaString {
        self.key.clone()
    }
}

#[async_trait]
impl Resource for Counter {
    type Specification = CounterSpec;

    const KIND: &'static str = "counter";

    async fn create(spec: &Self::Specification) -> Result<Self, GenericError> {
        if let Some(delay) = spec.create_delay {
            tokio::time::sleep(delay).await;
        }

        if spec.fail {
            return Err(generic_error!("creation deliberately failed"));
        }

        Ok(Counter { serial: next_serial() })
    }
}

#[async_trait]
impl Resource for Widget {
    type Specification = CounterSpec;

    const KIND: &'static str = "widget";

    async fn create(_spec: &Self::Specification) -> Result<Self, GenericError> {
        Ok(Widget)
    }
}

#[async_trait]
impl Resource for Bundle {
    type Specification = CounterSpec;

    const KIND: &'static str = "bundle";

    async fn create(_spec: &Self::Specification) -> Result<Self, GenericError> {
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
        .acquire::<Counter>(&owner("dsd_in"), CounterSpec::new("counter://a"))
        .await
        .expect("should acquire");

    assert_eq!(lease.key().as_ref(), "counter://a");

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].key, "counter://a");
    assert_eq!(snapshot[0].kind, "counter");
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
        .acquire::<Counter>(&owner("first"), CounterSpec::new("counter://b"))
        .await
        .expect("should acquire");

    let err = registry
        .acquire::<Counter>(&owner("second"), CounterSpec::new("counter://b"))
        .await
        .expect_err("should be refused while leased");

    match err {
        AcquireError::AlreadyLeased { kind, key, owner, .. } => {
            assert_eq!(kind, "counter");
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
        .acquire::<Counter>(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let original = first.serial;
    drop(first);

    // The resource is back in the registry, so this must hand out the very same one rather than creating a new one.
    // This is the property the whole registry exists for.
    let second = registry
        .acquire::<Counter>(&owner("second"), spec)
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
        .acquire::<Counter>(&owner("first"), spec.clone())
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
    let spec = CounterSpec::new("bundle://a");

    let first = registry
        .acquire::<Bundle>(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    assert_eq!(first.serials.len(), BUNDLE_HANDLES);
    let original = first.serials.clone();

    // Multiplicity lives inside the resource, so the registry still reports exactly one entry and refuses a second
    // acquirer on the strength of a single lease.
    assert_eq!(registry.snapshot().len(), 1);
    assert!(registry
        .acquire::<Bundle>(&owner("second"), spec.clone())
        .await
        .is_err());

    drop(first);

    let second = registry
        .acquire::<Bundle>(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(second.serials, original);
}

#[tokio::test]
async fn creation_failure_leaves_no_entry_behind() {
    let registry = ResourceRegistry::new();

    let err = registry
        .acquire::<Counter>(&owner("first"), CounterSpec::new("counter://e").failing())
        .await
        .expect_err("creation should fail");
    assert!(matches!(err, AcquireError::CreationFailed { .. }));

    // A failed creation must not leave the key claimed, otherwise a transient failure would block it forever.
    assert!(registry.snapshot().is_empty());
    registry
        .acquire::<Counter>(&owner("second"), CounterSpec::new("counter://e"))
        .await
        .expect("retry should succeed");
}

#[tokio::test]
async fn same_key_with_a_different_type_is_refused() {
    let registry = ResourceRegistry::new();
    let _held = registry
        .acquire::<Counter>(&owner("first"), CounterSpec::new("counter://g"))
        .await
        .expect("should acquire");

    let err = registry
        .acquire::<Widget>(&owner("second"), CounterSpec::new("counter://g"))
        .await
        .expect_err("a different type on the same key should be refused");

    match err {
        AcquireError::KindMismatch {
            key,
            existing_kind,
            requested_kind,
        } => {
            assert_eq!(key.as_ref(), "counter://g");
            assert_eq!(existing_kind, "counter");
            assert_eq!(requested_kind, "widget");
        }
        other => panic!("expected KindMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn kind_mismatch_is_detected_even_when_idle() {
    let registry = ResourceRegistry::new();
    drop(
        registry
            .acquire::<Counter>(&owner("first"), CounterSpec::new("counter://h"))
            .await
            .expect("should acquire"),
    );

    let err = registry
        .acquire::<Widget>(&owner("second"), CounterSpec::new("counter://h"))
        .await
        .expect_err("a different type on the same key should be refused");
    assert!(matches!(err, AcquireError::KindMismatch { .. }));
}

#[tokio::test]
async fn concurrent_acquire_during_creation_does_not_double_create() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://i").with_create_delay(Duration::from_millis(100));

    let first = tokio::spawn({
        let registry = registry.clone();
        let spec = spec.clone();
        async move { registry.acquire::<Counter>(&owner("first"), spec).await }
    });

    // Give the first acquire time to stake its claim on the key, but not to finish creating.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let second = registry.acquire::<Counter>(&owner("second"), spec).await;
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
    let timed_out = tokio::time::timeout(
        Duration::from_millis(50),
        registry.acquire::<Counter>(&owner("first"), spec),
    )
    .await;
    assert!(timed_out.is_err(), "the acquisition should still have been creating");

    // The claim must not outlive the acquisition that staked it, or the key would be blocked for the life of the
    // process and no later caller could ever bind it.
    assert!(registry.snapshot().is_empty());

    let lease = registry
        .acquire::<Counter>(&owner("second"), CounterSpec::new("counter://r"))
        .await
        .expect("should acquire after the cancelled attempt");
    assert_eq!(lease.key().as_ref(), "counter://r");
}

#[tokio::test]
async fn discard_recreates_on_the_next_acquire() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://j");

    let lease = registry
        .acquire::<Counter>(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let original = lease.serial;
    lease.discard();

    // A discarded resource is gone entirely, so the next acquire builds a fresh one.
    assert!(registry.snapshot().is_empty());

    let rebuilt = registry
        .acquire::<Counter>(&owner("second"), spec)
        .await
        .expect("should acquire");
    assert_ne!(rebuilt.serial, original);
}

#[tokio::test]
async fn release_returns_the_resource_immediately() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://l");

    let lease = registry
        .acquire::<Counter>(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    lease.release();

    assert_eq!(registry.snapshot()[0].state, "idle");
    registry
        .acquire::<Counter>(&owner("second"), spec)
        .await
        .expect("should acquire after release");
}

#[tokio::test]
async fn differing_settings_reuse_the_existing_resource() {
    let registry = ResourceRegistry::new();

    let first = registry
        .acquire::<Counter>(&owner("first"), CounterSpec::new("counter://m").with_setting(1))
        .await
        .expect("should acquire");
    let original = first.serial;
    drop(first);

    // The key identifies the resource, so a setting that isn't part of the key can't cause a rebuild.
    let second = registry
        .acquire::<Counter>(&owner("second"), CounterSpec::new("counter://m").with_setting(99))
        .await
        .expect("should acquire");
    assert_eq!(second.serial, original);
}

#[tokio::test]
async fn mutations_through_a_lease_survive_the_round_trip() {
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://n");

    let mut lease = registry
        .acquire::<Counter>(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    lease.serial = 4242;
    drop(lease);

    let reacquired = registry
        .acquire::<Counter>(&owner("second"), spec)
        .await
        .expect("should reacquire");
    assert_eq!(reacquired.serial, 4242);
}

#[tokio::test]
async fn snapshot_is_ordered_by_key() {
    let registry = ResourceRegistry::new();
    for key in ["counter://q_c", "counter://q_a", "counter://q_b"] {
        drop(
            registry
                .acquire::<Counter>(&owner("first"), CounterSpec::new(key))
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
