use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    time::Duration,
};

use saluki_error::generic_error;
use tokio::time::timeout;

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

    async fn create(&self, _subleases: Subleases) -> Result<Self::Resource, GenericError> {
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

    async fn create(&self, _subleases: Subleases) -> Result<Self::Resource, GenericError> {
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

    async fn create(&self, _subleases: Subleases) -> Result<Self::Resource, GenericError> {
        Ok(Widget)
    }
}

/// A resource that subdivides itself, standing in for a connectionless listener lending out its bound socket.
///
/// Carries state accumulating within a single lease (the hand-out cursor), and hands out items that hold a sublease
/// for as long as they live.
#[derive(Debug)]
struct Dispenser {
    serial: usize,
    handed_out: usize,
    subleases: Subleases,
}

impl Dispenser {
    /// Hands out an item, subleased for as long as the returned value lives.
    fn dispense(&mut self) -> Sublease {
        self.handed_out += 1;
        self.subleases.issue().expect("resource is still registered")
    }
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

    async fn create(&self, subleases: Subleases) -> Result<Self::Resource, GenericError> {
        Ok(Dispenser {
            serial: next_serial(),
            handed_out: 0,
            subleases,
        })
    }

    fn reset(resource: &mut Self::Resource) {
        resource.handed_out = 0;
    }
}

/// A resource whose reset panics while its switch is on, standing in for an implementor-supplied reset with a bug
/// in it.
#[derive(Debug)]
struct Fragile {
    serial: usize,
    mutated: u32,
    panic_on_reset: Arc<AtomicBool>,
}

#[derive(Clone)]
struct FragileSpec {
    key: MetaString,
    panic_on_reset: Arc<AtomicBool>,
}

impl FragileSpec {
    fn new(key: &str) -> Self {
        Self {
            key: MetaString::from(key),
            panic_on_reset: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// Hand-written so the switch stays out of the rendered specification: the registry compares those renderings across
/// acquisitions, and a switch that the test flips partway through would read as a changed specification.
impl fmt::Debug for FragileSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FragileSpec").field("key", &self.key).finish()
    }
}

#[async_trait]
impl ResourceSpecification for FragileSpec {
    type Resource = Fragile;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        self.key.clone()
    }

    async fn create(&self, _subleases: Subleases) -> Result<Self::Resource, GenericError> {
        Ok(Fragile {
            serial: next_serial(),
            mutated: 0,
            panic_on_reset: Arc::clone(&self.panic_on_reset),
        })
    }

    fn reset(resource: &mut Self::Resource) {
        assert!(!resource.panic_on_reset.load(Relaxed), "reset is broken");

        resource.mutated = 0;
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

    async fn create(&self, _subleases: Subleases) -> Result<Self::Resource, GenericError> {
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
async fn per_lease_state_is_reset_before_the_next_holder_gets_it() {
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
async fn a_panicking_reset_leaves_the_resource_in_the_registry() {
    let registry = ResourceRegistry::new();
    let spec = FragileSpec::new("fragile://a");

    // Creation doesn't reset, so the first acquisition gets through.
    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let serial = lease.serial;
    lease.mutated = 7;
    drop(lease);

    // The second acquisition does reset, and this one panics. The acquisition dies with it.
    let acquisition = {
        let registry = registry.clone();
        let spec = spec.clone();
        tokio::spawn(async move { registry.acquire(&owner("second"), spec).await.map(|_| ()) })
    };
    let error = acquisition.await.expect_err("acquisition should panic");
    assert!(error.is_panic());

    // It takes nothing with it, though. Answering at all means the registry lock survived the unwind rather than
    // being poisoned by it, and the resource is idle rather than gone: it was already owned by its lease when the
    // reset ran, so unwinding returned it here instead of dropping it and releasing the underlying resource.
    let statuses = registry.snapshot();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, "idle");
    assert_eq!(statuses[0].owner, None);

    // And it really is the same resource, not a replacement built in its place.
    spec.panic_on_reset.store(false, Relaxed);

    let lease = registry.acquire(&owner("third"), spec).await.expect("should reacquire");
    assert_eq!(lease.serial, serial);
    assert_eq!(lease.mutated, 0);
}

/// How long to wait before concluding an acquisition is genuinely blocked.
const BLOCKED: Duration = Duration::from_millis(100);

/// Upper bound on an acquisition that should complete promptly.
const PROMPTLY: Duration = Duration::from_secs(5);

#[tokio::test]
async fn an_outstanding_sublease_holds_the_lease_open() {
    // The property subleases exist for. Releasing the head lease isn't enough on its own: something the holder lent
    // out is still in use, and handing the resource to the next acquirer now would let both use it at once.
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://sublease_holds");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let serial = lease.serial;
    let sublease = lease.dispense();
    drop(lease);

    // The head lease is gone, so the entry is idle -- but not available.
    assert!(
        timeout(BLOCKED, registry.acquire(&owner("second"), spec.clone()))
            .await
            .is_err(),
        "acquisition should block while a sublease is outstanding"
    );

    drop(sublease);

    let reacquired = timeout(PROMPTLY, registry.acquire(&owner("second"), spec))
        .await
        .expect("acquisition should complete once the sublease is returned")
        .expect("should reacquire");
    assert_eq!(reacquired.serial, serial);
}

#[tokio::test]
async fn per_lease_state_is_reset_only_once_subleases_are_returned() {
    // `reset` runs at hand-over rather than on return, so it sees a resource whose previous holder is genuinely
    // finished -- including with whatever it lent out.
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://reset_after_subleases");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let sublease = lease.dispense();
    assert_eq!(lease.handed_out, 1);
    drop(lease);

    drop(sublease);

    let reacquired = timeout(PROMPTLY, registry.acquire(&owner("second"), spec))
        .await
        .expect("acquisition should complete")
        .expect("should reacquire");
    assert_eq!(reacquired.handed_out, 0);
}

#[tokio::test]
async fn a_resource_that_never_subdivides_is_handed_over_without_waiting() {
    // The common case must not pay for the mechanism: with nothing subleased, the wait resolves on its first look.
    let registry = ResourceRegistry::new();
    let spec = CounterSpec::new("counter://no_subleases");

    drop(
        registry
            .acquire(&owner("first"), spec.clone())
            .await
            .expect("should acquire"),
    );

    timeout(BLOCKED, registry.acquire(&owner("second"), spec))
        .await
        .expect("acquisition should not wait when nothing is subleased")
        .expect("should reacquire");
}

#[tokio::test]
async fn a_cancelled_acquisition_releases_its_claim_on_the_resource() {
    // An acquisition blocked on a sublease is a cancellation point: a supervisor aborting a component mid-build drops
    // it. The claim must not outlive it, or the resource is unreachable for the life of the process.
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://cancelled_claim");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let serial = lease.serial;
    let sublease = lease.dispense();
    drop(lease);

    // Blocks on the outstanding sublease, then gets dropped when the timeout elapses.
    assert!(
        timeout(BLOCKED, registry.acquire(&owner("second"), spec.clone()))
            .await
            .is_err(),
        "acquisition should block while a sublease is outstanding"
    );

    drop(sublease);

    // The abandoned claim would otherwise still be recorded, and this would fail with `AlreadyLeased`.
    let reacquired = timeout(PROMPTLY, registry.acquire(&owner("third"), spec))
        .await
        .expect("acquisition should complete once the sublease is returned")
        .expect("should acquire after the cancelled attempt released its claim");
    assert_eq!(reacquired.serial, serial);
}

#[tokio::test]
async fn discarding_with_an_outstanding_sublease_holds_the_key() {
    // Discarding drops the resource, but that alone doesn't release a subdivided one: its subresources are what hold
    // the underlying resource open. Building the replacement now would stand it up alongside the resource being
    // discarded -- for a connectionless listener, a second socket bound to the same address, splitting traffic with
    // the first.
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://discard_holds");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let discarded = lease.serial;
    let sublease = lease.dispense();
    lease.discard();

    assert!(
        timeout(BLOCKED, registry.acquire(&owner("second"), spec.clone()))
            .await
            .is_err(),
        "acquisition should block while a sublease on the discarded resource is outstanding"
    );

    drop(sublease);

    // Only now is the old resource gone in full, so only now is a replacement safe to build -- and it is a
    // replacement, not the resource that was discarded.
    let rebuilt = timeout(PROMPTLY, registry.acquire(&owner("second"), spec))
        .await
        .expect("acquisition should complete once the sublease is returned")
        .expect("should acquire");
    assert_ne!(rebuilt.serial, discarded);
}

#[tokio::test]
async fn discarding_with_every_sublease_returned_frees_the_key_at_once() {
    // Holding the key is only for subleases that are still out. With none outstanding, nothing is keeping the
    // underlying resource open, so there is nothing to wait for and the discard is final immediately.
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://discard_settled");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let discarded = lease.serial;
    let sublease = lease.dispense();
    drop(sublease);
    lease.discard();

    assert!(
        registry.snapshot().is_empty(),
        "a discarded resource with nothing outstanding should leave no entry behind"
    );

    let rebuilt = timeout(PROMPTLY, registry.acquire(&owner("second"), spec))
        .await
        .expect("acquisition should not wait when nothing is subleased")
        .expect("should acquire");
    assert_ne!(rebuilt.serial, discarded);
}

#[tokio::test]
async fn a_cancelled_acquisition_releases_its_claim_on_a_discarded_resource() {
    // The same cancellation point as for an idle resource: an acquisition waiting on the discarded resource's
    // subleases can be dropped, and its claim must not outlive it, or the key could never be rebuilt.
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://cancelled_discard_claim");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let discarded = lease.serial;
    let sublease = lease.dispense();
    lease.discard();

    // Blocks on the outstanding sublease, then gets dropped when the timeout elapses.
    assert!(
        timeout(BLOCKED, registry.acquire(&owner("second"), spec.clone()))
            .await
            .is_err(),
        "acquisition should block while a sublease on the discarded resource is outstanding"
    );

    drop(sublease);

    // The abandoned claim would otherwise still be recorded, and this would fail with `AlreadyLeased`.
    let rebuilt = timeout(PROMPTLY, registry.acquire(&owner("third"), spec))
        .await
        .expect("acquisition should complete once the sublease is returned")
        .expect("should acquire after the cancelled attempt released its claim");
    assert_ne!(rebuilt.serial, discarded);
}

#[tokio::test]
async fn a_discarded_resource_holds_its_key_without_holding_its_type() {
    // What survives a discard is the key, not the resource, so there is nothing left to type-check against: refusing
    // the acquisition below would be refusing it on the basis of a resource that no longer exists. The wait still
    // applies, because that is about the subresources, which are still very much real.
    let registry = ResourceRegistry::new();

    let mut lease = registry
        .acquire(&owner("first"), DispenserSpec::new("dispenser://discard_retype"))
        .await
        .expect("should acquire");
    let sublease = lease.dispense();
    lease.discard();

    assert!(
        timeout(
            BLOCKED,
            registry.acquire(&owner("second"), WidgetSpec::new("dispenser://discard_retype"))
        )
        .await
        .is_err(),
        "acquisition should block while a sublease on the discarded resource is outstanding"
    );

    drop(sublease);

    timeout(
        PROMPTLY,
        registry.acquire(&owner("second"), WidgetSpec::new("dispenser://discard_retype")),
    )
    .await
    .expect("acquisition should complete once the sublease is returned")
    .expect("a discarded resource should not hold its key to the type it used to be");
}

#[tokio::test]
async fn snapshot_reports_outstanding_subleases() {
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://snapshot_subleases");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let sublease = lease.dispense();
    drop(lease);

    // The head lease is back, so this isn't `leased` -- but it isn't available either, and the snapshot says why.
    let statuses = registry.snapshot();
    let status = statuses
        .iter()
        .find(|status| status.key == "dispenser://snapshot_subleases")
        .expect("resource should be registered");
    assert_eq!(status.state, "subleased");
    assert_eq!(status.outstanding_subleases, 1);

    drop(sublease);

    let statuses = registry.snapshot();
    let status = statuses
        .iter()
        .find(|status| status.key == "dispenser://snapshot_subleases")
        .expect("resource should be registered");
    assert_eq!(status.state, "idle");
    assert_eq!(status.outstanding_subleases, 0);
}

#[tokio::test]
async fn snapshot_reports_a_discarded_resource_holding_its_key() {
    let registry = ResourceRegistry::new();
    let spec = DispenserSpec::new("dispenser://snapshot_discarded");

    let mut lease = registry
        .acquire(&owner("first"), spec.clone())
        .await
        .expect("should acquire");
    let sublease = lease.dispense();
    lease.discard();

    // The resource itself is gone, so this isn't `subleased`: the key alone is reserved, and only until the sublease
    // that is keeping the underlying resource alive comes back.
    let statuses = registry.snapshot();
    let status = statuses
        .iter()
        .find(|status| status.key == "dispenser://snapshot_discarded")
        .expect("a discarded resource should still hold its key");
    assert_eq!(status.state, "discarded");
    assert_eq!(status.outstanding_subleases, 1);
    assert_eq!(status.owner, None);

    drop(sublease);
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
