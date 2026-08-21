//! External resource management.

use std::{
    any::{Any, TypeId},
    fmt, mem,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use saluki_common::collections::FastHashMap;
use saluki_error::GenericError;
use serde::Serialize;
use snafu::Snafu;
use stringtheory::MetaString;
use tracing::{debug, warn};

use crate::{runtime::process::Id as ProcessId, support::SubsystemIdentifier};

mod api;
pub use self::api::{ResourceRegistryAPIHandler, ResourceRegistryState};

mod worker;
pub use self::worker::ResourceRegistryWorker;

#[cfg(test)]
mod tests;

/// A specification naming one resource.
///
/// Implementations describe *what* to create without creating it, so a specification can be built and compared long
/// before the underlying resource exists.
pub trait ResourceSpecification: Clone + fmt::Debug + Send + Sync + 'static {
    /// Returns the globally unique key for the resource this specification names.
    ///
    /// Keys are unique across all resource kinds, not just one: two specifications that yield the same key are
    /// understood to name the same underlying scarce thing and will conflict with each other. Namespace keys by scheme
    /// (for example, `udp://0.0.0.0:8125`) so that unrelated kinds cannot collide.
    ///
    /// Only the parts of a specification that identify the underlying thing belong in the key. Settings that do not
    /// change *which* resource is named should be left out, so that two specifications differing only in their
    /// settings are correctly recognized as naming the same resource.
    fn key(&self) -> MetaString;
}

/// A resource whose lifetime is owned by a [`ResourceRegistry`].
#[async_trait]
pub trait Resource: Sized + Send + 'static {
    /// Specification used to name and create this resource.
    type Specification: ResourceSpecification;

    /// Human-readable kind, used in accounting output.
    const KIND: &'static str;

    /// Creates the resource named by `spec`.
    ///
    /// A resource that is only meaningful as a set of underlying handles -- several sockets bound to one address with
    /// `SO_REUSEPORT`, say -- holds all of them itself. Creating them together in a single call is what makes them
    /// atomic: if any one fails, this returns an error and nothing is registered.
    ///
    /// # Errors
    ///
    /// If the resource can't be created, an error is returned and nothing is registered.
    async fn create(spec: &Self::Specification) -> Result<Self, GenericError>;
}

/// An error that occurred while acquiring a resource.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum AcquireError {
    /// The resource is already leased by something else in this process.
    #[snafu(display(
        "{} resource '{}' is already leased by '{}' (acquired by process {})",
        kind,
        key,
        owner,
        acquisition_process_id.as_usize()
    ))]
    AlreadyLeased {
        /// Kind of the resource.
        kind: &'static str,

        /// Key of the resource.
        key: MetaString,

        /// Rendered identity of the subsystem holding the resource.
        ///
        /// This is the authoritative answer to who holds the resource. Rendered rather than kept as a
        /// [`SubsystemIdentifier`], which stores enough segments inline to make this error large enough to slow down
        /// every `Result` that carries it.
        owner: MetaString,

        /// Identifier of the process that acquired the resource.
        acquisition_process_id: ProcessId,
    },

    /// The key is registered, but to a different type of resource.
    #[snafu(display(
        "resource '{}' is registered as kind '{}', but was requested as kind '{}'",
        key,
        existing_kind,
        requested_kind
    ))]
    KindMismatch {
        /// Key of the resource.
        key: MetaString,

        /// Kind the resource was registered under.
        existing_kind: &'static str,

        /// Kind the resource was requested as.
        requested_kind: &'static str,
    },

    /// The resource could not be created.
    #[snafu(display("failed to create {} resource '{}': {}", kind, key, source))]
    CreationFailed {
        /// Kind of the resource.
        kind: &'static str,

        /// Key of the resource.
        key: MetaString,

        /// Source of the error.
        source: GenericError,
    },
}

/// Identity of whatever currently holds a resource.
#[derive(Clone, Debug)]
struct LeaseInfo {
    owner: SubsystemIdentifier,
    acquisition_process_id: ProcessId,
}

impl LeaseInfo {
    fn from_owner(owner: &SubsystemIdentifier) -> Self {
        Self {
            owner: owner.clone(),
            acquisition_process_id: ProcessId::current(),
        }
    }
}

/// RAII guard to disarm in-progress leases when resource creation fails to run to completion.
///
/// [`ResourceRegistry::acquire`] marks an entry as [`EntryState::Creating`] before awaiting [`Resource::create`], and
/// that await is a cancellation point: a supervisor aborts a worker that is still initializing when shutdown arrives,
/// so a builder's acquisition can be dropped partway through. Without this guard the claim would outlive the
/// acquisition and block the key for the life of the process.
struct ClaimCreationGuard<'a> {
    registry: &'a ResourceRegistry,
    key: &'a MetaString,
    armed: bool,
}

impl<'a> ClaimCreationGuard<'a> {
    /// Creates a new guard for the given key in the armed state.
    fn from_key(registry: &'a ResourceRegistry, key: &'a MetaString) -> Self {
        Self {
            registry,
            key,
            armed: true,
        }
    }

    /// Disarm and consume the guard.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ClaimCreationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let mut state = self.registry.inner.lock().unwrap();

        // Only take back a claim that is still ours to take back.
        if matches!(
            state.entries.get(self.key).map(|entry| &entry.state),
            Some(EntryState::Creating(_))
        ) {
            debug!(key = %self.key, "Resource creation was cancelled. Releasing the claim on its key.");
            state.entries.remove(self.key);
        }
    }
}

/// Lifecycle state of a registry entry.
enum EntryState {
    /// The resource is held by the registry and can be acquired.
    Idle(Box<dyn Any + Send>),

    /// Creation is in flight. The entry holds nothing yet, but is already spoken for.
    Creating(LeaseInfo),

    /// The resource is lent out.
    Leased(LeaseInfo),
}

impl EntryState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Idle(_) => "idle",
            Self::Creating(_) => "creating",
            Self::Leased(_) => "leased",
        }
    }

    fn holder(&self) -> Option<&LeaseInfo> {
        match self {
            Self::Idle(_) => None,
            Self::Creating(info) | Self::Leased(info) => Some(info),
        }
    }
}

/// One resource registered under a single key.
struct Entry {
    kind: &'static str,
    type_id: TypeId,
    spec_desc: String,
    state: EntryState,
    acquisitions: u64,
}

impl Entry {
    fn new<R: Resource>(spec: &R::Specification, lease_info: LeaseInfo) -> Self {
        Self {
            kind: R::KIND,
            type_id: TypeId::of::<R>(),
            spec_desc: format!("{:?}", spec),
            state: EntryState::Creating(lease_info),
            acquisitions: 0,
        }
    }

    /// Hands out an idle resource, or explains why it can't.
    fn lease<R: Resource>(
        &mut self, registry: &ResourceRegistry, new_spec: &R::Specification, lease_info: LeaseInfo,
    ) -> Result<ResourceLease<R>, AcquireError> {
        if self.type_id != TypeId::of::<R>() {
            return Err(AcquireError::KindMismatch {
                key: new_spec.key(),
                existing_kind: self.kind,
                requested_kind: R::KIND,
            });
        }

        if let Some(holder) = self.state.holder() {
            return Err(AcquireError::AlreadyLeased {
                kind: self.kind,
                key: new_spec.key(),
                owner: MetaString::from(holder.owner.to_string()),
                acquisition_process_id: holder.acquisition_process_id,
            });
        }

        let key = new_spec.key();

        // The key identifies the resource, so a specification differing only in its settings still names this same
        // resource. Hand back what exists rather than rebuilding it, but say so, since the new settings have no effect.
        let new_spec_desc = format!("{:?}", new_spec);
        if self.spec_desc != new_spec_desc {
            warn!(
                %key,
                existing = %self.spec_desc,
                requested = %new_spec_desc,
                "Resource acquired with a different specification than it was created with. Using the existing resource; \
                 the requested specification has no effect."
            );
        }

        // `holder` returned `None` just above, so the entry is idle and holds its value.
        let value = match mem::replace(&mut self.state, EntryState::Leased(lease_info)) {
            EntryState::Idle(value) => value,
            _ => unreachable!("entry without a holder is idle"),
        };

        self.acquisitions += 1;

        Ok(ResourceLease {
            value: Some(*value.downcast::<R>().expect("entry type checked above")),
            registry: registry.clone(),
            key,
        })
    }
}

#[derive(Default)]
struct RegistryState {
    entries: FastHashMap<MetaString, Entry>,
}

impl RegistryState {
    fn snapshot(&self) -> Vec<ResourceStatus> {
        let mut statuses = self
            .entries
            .iter()
            .map(|(key, entry)| ResourceStatus {
                key: key.to_string(),
                kind: entry.kind,
                spec: entry.spec_desc.clone(),
                state: entry.state.as_str(),
                owner: entry.state.holder().map(|info| info.owner.to_string()),
                acquisition_process_id: entry.state.holder().map(|info| info.acquisition_process_id.as_usize()),
                acquisitions: entry.acquisitions,
            })
            .collect::<Vec<_>>();
        statuses.sort_by(|a, b| a.key.cmp(&b.key));

        statuses
    }
}

/// A registry for scarce, externally backed resources.
///
/// In many cases, data planes will have to interact with the outside world by way of exposing network endpoints, or
/// exposing files, and so on... referred to here as "resources." These resources are unique, or are conceptually meant
/// to be unique: there should be no other OS processes trying to take ownership of them, and only one part of the code
/// in the data plane should own them.
///
/// A [`ResourceRegistry`] owns those resources on behalf of the entire data plane and lends them out. A child process
/// never owns a resource, but instead holds a [`ResourceLease`]. When the lease drops -- including when the child
/// process holding it dies -- the resource returns to the registry intact and still live, ready for the next acquirer.
/// Since the registry outlives the components that use its resources, a component can be torn down and rebuilt without
/// the underlying resource being automatically released back to the operating system due to typical Rust drop
/// semantics.
///
/// # Groups
///
/// Resources are named by a [`ResourceSpecification`], which provides the blueprint for how to create a particular
/// resource, such as a network socket, when a caller attempts to acquire it. Resource specifications are generally tied
/// one-to-one with a particular type.
///
/// The specification provides both the information necessary to properly determine one unique resource from
/// another, as well as a mechanism for consistent creation of potentially complex resources, including asynchronous
/// initialization.
///
/// # Keys and conflicts
///
/// Entries are keyed by their [`ResourceSpecification`] alone, deliberately *not* by key and type together. Two
/// different Rust types can easily describe the same scarce thing -- a connection-oriented listener and a general one
/// over the same address -- and keying by type would let each of them claim it. Keys must therefore be unique across
/// every resource kind, not just within one, so namespace them by scheme (`udp://0.0.0.0:8125`).
#[derive(Clone, Default)]
pub struct ResourceRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

impl ResourceRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires the resource named by `spec`, creating it if it isn't registered yet.
    ///
    /// `owner` identifies the subsystem taking the lease and is recorded, alongside the current process identifier, for
    /// accounting.
    ///
    /// If the resource is already registered, the existing one is handed back rather than a new one being created. This
    /// is the mechanism by which a resource outlives the components that use it.
    ///
    /// # Errors
    ///
    /// If the resource is already leased, if the key is registered to a different type of resource, or if creation
    /// fails, an error is returned.
    pub async fn acquire<R: Resource>(
        &self, owner: &SubsystemIdentifier, spec: R::Specification,
    ) -> Result<ResourceLease<R>, AcquireError> {
        let key = spec.key();
        let lease_info = LeaseInfo::from_owner(owner);

        // Attempt to lease the resource if it's already registered.
        //
        // Otherwise, start the registration process by insert an uninitialized entry that gives us lease ownership
        // prior to actually creating the resource and finalizing it.
        {
            let mut state = self.inner.lock().unwrap();
            if let Some(entry) = state.entries.get_mut(&key) {
                return entry.lease::<R>(self, &spec, lease_info);
            }

            let new_entry = Entry::new::<R>(&spec, lease_info.clone());
            state.entries.insert(key.clone(), new_entry);
        }

        // Create the resource.
        //
        // We establish a "creation guard" which is a drop guard that ensures we remove our pending entry if we fail to
        // create the resource, including if this asynchronous call is cancelled, so that we don't permanently tie up
        // the resource in an uninitialized state.
        let claim_guard = ClaimCreationGuard::from_key(self, &key);
        let created = R::create(&spec).await;
        claim_guard.disarm();

        let mut state = self.inner.lock().unwrap();
        match created {
            Ok(value) => {
                let entry = state
                    .entries
                    .get_mut(&key)
                    .expect("entry was inserted before creation and is only removed by this function");

                entry.acquisitions += 1;
                entry.state = EntryState::Leased(lease_info);

                debug!(%key, kind = R::KIND, %owner, "Created resource.");

                Ok(ResourceLease {
                    value: Some(value),
                    registry: self.clone(),
                    key,
                })
            }
            Err(source) => {
                // Drop the claim so that a later acquire can retry.
                state.entries.remove(&key);
                Err(AcquireError::CreationFailed {
                    kind: R::KIND,
                    key,
                    source,
                })
            }
        }
    }

    /// Returns a snapshot of every registered resource, ordered by key.
    pub fn snapshot(&self) -> Vec<ResourceStatus> {
        let state = self.inner.lock().unwrap();
        state.snapshot()
    }

    /// Creates an API handler for reporting the state of all registered resources.
    pub fn api_handler(&self) -> ResourceRegistryAPIHandler {
        ResourceRegistryAPIHandler::from_registry(self.clone())
    }

    /// Creates a [`ResourceRegistryWorker`] that publishes the registry over the control plane.
    pub fn worker(&self) -> ResourceRegistryWorker {
        ResourceRegistryWorker::new(self.clone())
    }

    /// Returns a resource to the registry, marking its entry idle.
    fn return_value(&self, key: &MetaString, value: Box<dyn Any + Send>) {
        let mut state = self.inner.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(key) {
            debug!(%key, "Resource returned to registry.");
            entry.state = EntryState::Idle(value);
        }
    }

    /// Drops a resource instead of returning it, so that the next acquisition creates a fresh one.
    fn discard_value(&self, key: &MetaString) {
        let mut state = self.inner.lock().unwrap();
        if state.entries.remove(key).is_some() {
            debug!(%key, "Resource discarded; it will be recreated on the next acquisition.");
        }
    }
}

/// Reported state of a single resource.
#[derive(Clone, Debug, Serialize)]
pub struct ResourceStatus {
    /// Key the resource is registered under.
    pub key: String,

    /// Kind of resource.
    pub kind: &'static str,

    /// Rendered specification the resource was created from.
    pub spec: String,

    /// Lifecycle state of the resource: `idle`, `creating`, or `leased`.
    pub state: &'static str,

    /// Subsystem holding the resource, if any.
    ///
    /// This is the authoritative answer to who holds the resource.
    pub owner: Option<String>,

    /// Process that acquired the resource, if any.
    ///
    /// This is the process that ran the acquisition, which is not necessarily the one using the resource now: a
    /// component acquires while it is being built, and only afterwards does it get a process of its own.
    ///
    /// Use [`owner`][Self::owner] to identify the holder.
    pub acquisition_process_id: Option<usize>,

    /// Number of times the resource has been acquired.
    pub acquisitions: u64,
}

/// An exclusive lease on a resource.
///
/// Dereferences to the resource itself. Dropping the lease returns the resource to the registry still live, so a lease
/// is a loan, never ownership. See [`ResourceRegistry`] for the full model.
pub struct ResourceLease<R: Resource> {
    value: Option<R>,
    registry: ResourceRegistry,
    key: MetaString,
}

impl<R: Resource> ResourceLease<R> {
    /// Returns the key this resource is registered under.
    pub fn key(&self) -> &MetaString {
        &self.key
    }

    /// Returns the resource to the registry.
    ///
    /// Equivalent to dropping the lease; useful where the return should be obvious at the call site.
    pub fn release(self) {}

    /// Drops the resource instead of returning it, so the next acquisition creates a fresh one.
    ///
    /// Use this when the resource has hit an error it can't recover from and handing it to the next acquirer would pass
    /// the problem along.
    pub fn discard(mut self) {
        // Dropping the value here is the point: it is what releases the underlying resource.
        let _ = self.value.take();
        self.registry.discard_value(&self.key);
    }
}

impl<R: Resource> Deref for ResourceLease<R> {
    type Target = R;

    fn deref(&self) -> &Self::Target {
        self.value.as_ref().expect("lease holds its value until dropped")
    }
}

impl<R: Resource> DerefMut for ResourceLease<R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().expect("lease holds its value until dropped")
    }
}

impl<R: Resource> Drop for ResourceLease<R> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            self.registry.return_value(&self.key, Box::new(value));
        }
    }
}

impl<R: Resource + fmt::Debug> fmt::Debug for ResourceLease<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResourceLease")
            .field("key", &self.key)
            .field("value", &self.value)
            .finish()
    }
}
