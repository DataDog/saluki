//! Container tag resolution for traces and stats.
//!
//! A container's tags aren't in the tagger when its first span arrives. A [`ContainerTagsResolver`] resolves the tags of
//! a container for an item that carries the container's ID, such as a trace or a stats payload. It has two
//! implementations:
//!
//! - The no-op resolver looks the container up once and never waits.
//! - The buffering resolver holds the item until the container has a `kube_` tag, or until the wait runs out.
//!
//! The binary builds one [`ContainerTagsBuffer`] and gives a clone to each consumer. Each consumer owns a buffering
//! resolver with its own pending items. The clones share the pending-bytes cap and the deny list. A container that
//! timed out for one consumer doesn't make the other consumer wait again.
//!
//! The buffering resolver follows the container tags buffer of the Datadog Agent, pinned at
//! [`92824ec`](https://github.com/DataDog/datadog-agent/blob/92824ec3d3f4a4c5b0926452999a39df5b4eb850/pkg/trace/containertags/buffer.go).
//! The differences are:
//!
//! - The resolver is a state machine without tasks or timers. The consumer passes the current time to each call and
//!   calls [`ContainerTagsResolver::tick`] every [`RETRY_INTERVAL`]. A consumer that stops calls
//!   [`ContainerTagsResolver::flush_all`], so no item stays held.
//! - The Agent has a soft limit of 12 seconds and a hard limit one second later. This resolver has one limit. A
//!   container expires on the first tick after 12 seconds, so an item waits at most 13 seconds.
//! - A container is complete when it has a `kube_` tag. The workload provider doesn't report whether all tag sources
//!   have answered.
//! - A new item never overtakes the held items of its container. When the new item can't wait, because the container
//!   is complete, denied, or over the cap, the resolver releases the held items first. The Agent lets the new item
//!   go first.
//!
//! # Missing
//!
//! The lookup uses low cardinality tags. The Agent uses high cardinality tags.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
    time::{Duration, Instant},
};

use saluki_core::data_model::{origin::OriginTagCardinality, tags::SharedTagSet};
use saluki_env::{workload::EntityId, WorkloadProvider};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

mod buffer;
use self::buffer::BufferingResolver;

mod denylist;
use self::denylist::DeniedContainers;

mod telemetry;

#[cfg(test)]
pub(crate) mod test_util;

/// How often a consumer calls [`ContainerTagsResolver::tick`] while items are pending.
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// How long a container can hold items before its wait runs out.
const MAX_BUFFER_DURATION: Duration = Duration::from_secs(12);

/// How long a container that ran out of wait time stays denied after it was last seen.
const DENY_DURATION: Duration = Duration::from_secs(2 * 60);

/// Pending-bytes cap that applies when the trace-agent memory limit is `0`.
const DEFAULT_MAX_PENDING_BYTES: usize = 7_000_000;

/// Fraction of the trace-agent memory limit that pending items can use.
const MAX_PENDING_MEMORY_FRACTION: f64 = 0.1;

/// Cardinality of the tags that the resolver looks up.
const TAG_CARDINALITY: OriginTagCardinality = OriginTagCardinality::Low;

/// Prefix of the tags that a Kubernetes workload provider supplies. A container with such a tag is complete.
const KUBERNETES_TAG_PREFIX: &str = "kube_";

/// Workload provider shared by the components of a pipeline.
pub(crate) type SharedWorkloadProvider = Arc<dyn WorkloadProvider + Send + Sync>;

/// State that the buffering resolvers of one pipeline share.
///
/// Cloning the buffer shares the state. A configuration that has no buffer uses the no-op resolver.
#[derive(Clone)]
pub struct ContainerTagsBuffer {
    shared: Arc<Shared>,
}

impl ContainerTagsBuffer {
    /// Creates a buffer whose pending items use at most `max_pending_bytes` bytes, as the resolvers estimate them.
    pub fn new(max_pending_bytes: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                max_pending_bytes,
                pending_bytes: AtomicUsize::new(0),
                denied: Mutex::new(DeniedContainers::default()),
            }),
        }
    }

    /// Creates a buffer for the trace-agent memory limit, in bytes.
    ///
    /// Pending items can use 10% of the limit. A limit of `0` means no limit, and the cap is 7 MB then.
    pub fn from_memory_limit(max_memory: f64) -> Self {
        // A float-to-integer cast saturates, and maps a negative or `NaN` limit to `0`.
        let cap = (max_memory * MAX_PENDING_MEMORY_FRACTION) as usize;
        Self::new(if cap == 0 { DEFAULT_MAX_PENDING_BYTES } else { cap })
    }

    /// Returns the cap on the bytes of pending items.
    pub fn max_pending_bytes(&self) -> usize {
        self.shared.max_pending_bytes
    }
}

struct Shared {
    max_pending_bytes: usize,
    pending_bytes: AtomicUsize,
    denied: Mutex<DeniedContainers>,
}

impl Shared {
    /// Reserves `size` bytes for a pending item, and returns `false` if the cap doesn't allow it.
    fn try_reserve(&self, size: usize) -> bool {
        self.pending_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                pending.checked_add(size).filter(|total| *total <= self.max_pending_bytes)
            })
            .is_ok()
    }

    fn release(&self, size: usize) {
        let _ = self
            .pending_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                Some(pending.saturating_sub(size))
            });
    }

    fn pending_bytes(&self) -> usize {
        self.pending_bytes.load(Ordering::Relaxed)
    }

    fn denied(&self) -> MutexGuard<'_, DeniedContainers> {
        self.denied.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The result of resolving the tags of a container.
#[derive(Clone, Debug)]
pub(crate) enum Resolution {
    /// The tags that the workload provider knows. They can be empty or incomplete.
    Tags(SharedTagSet),

    /// The workload provider doesn't know the container.
    Failed,
}

/// An item with the resolution of its container.
#[derive(Debug)]
pub(crate) struct Released<T> {
    /// The item that a resolver held or passed through.
    pub item: T,

    /// The resolution of the item's container.
    pub resolution: Resolution,
}

/// Resolves the container tags of items.
///
/// A resolver returns each item in `released`, either at once or when the container resolves. An item of a container
/// that has held items waits behind them.
pub(crate) trait ContainerTagsResolver<T>: Send {
    /// Submits an item of the container with the given ID.
    ///
    /// `size` estimates the bytes that the item holds while it waits. `now` is the current time.
    fn submit(&mut self, container_id: &MetaString, item: T, size: usize, now: Instant, released: &mut Vec<Released<T>>);

    /// Retries the pending containers, and releases the items of each container that resolved or ran out of time.
    fn tick(&mut self, now: Instant, released: &mut Vec<Released<T>>);

    /// Releases every pending item with the tags that the provider knows now.
    fn flush_all(&mut self, released: &mut Vec<Released<T>>);

    /// Returns `true` if the resolver holds items, so the consumer must call [`tick`](Self::tick).
    fn has_pending(&self) -> bool;
}

/// Builds the resolver for a consumer.
///
/// The resolver buffers if `buffer` is set, and looks the container up once otherwise. A consumer without a workload
/// provider adds no tags and keeps its container IDs.
pub(crate) fn resolver<T>(
    provider: Option<SharedWorkloadProvider>, buffer: Option<&ContainerTagsBuffer>, metrics: &MetricsBuilder,
) -> Box<dyn ContainerTagsResolver<T>>
where
    T: Send + 'static,
{
    let lookup = ContainerTagLookup { provider };
    match buffer {
        Some(buffer) if lookup.provider.is_some() => Box::new(BufferingResolver::new(
            lookup,
            Arc::clone(&buffer.shared),
            telemetry::BufferTelemetry::new(metrics),
        )),
        _ => Box::new(NoopResolver { lookup }),
    }
}

/// A resolver that looks the container up once and never waits.
struct NoopResolver {
    lookup: ContainerTagLookup,
}

impl<T> ContainerTagsResolver<T> for NoopResolver
where
    T: Send,
{
    fn submit(&mut self, container_id: &MetaString, item: T, _: usize, _: Instant, released: &mut Vec<Released<T>>) {
        released.push(Released {
            item,
            resolution: self.lookup.lookup(container_id).into_resolution(),
        });
    }

    fn tick(&mut self, _: Instant, _: &mut Vec<Released<T>>) {}

    fn flush_all(&mut self, _: &mut Vec<Released<T>>) {}

    fn has_pending(&self) -> bool {
        false
    }
}

/// A one-shot lookup of a container's tags.
#[derive(Clone)]
struct ContainerTagLookup {
    provider: Option<SharedWorkloadProvider>,
}

impl ContainerTagLookup {
    fn lookup(&self, container_id: &MetaString) -> Lookup {
        let tags = match &self.provider {
            Some(provider) => {
                provider.get_tags_for_entity(&EntityId::Container(container_id.clone()), TAG_CARDINALITY)
            }
            None => Some(SharedTagSet::default()),
        };
        Lookup { tags }
    }
}

/// What the workload provider knows about a container at one time.
struct Lookup {
    /// The tags of the container, or `None` if the provider doesn't know the container.
    tags: Option<SharedTagSet>,
}

impl Lookup {
    /// Returns `true` if the container has a `kube_` tag.
    fn is_complete(&self) -> bool {
        self.tags
            .iter()
            .flatten()
            .any(|tag| tag.as_str().starts_with(KUBERNETES_TAG_PREFIX))
    }

    /// Returns the resolution that reports the lookup as it is.
    fn into_resolution(self) -> Resolution {
        match self.tags {
            Some(tags) => Resolution::Tags(tags),
            None => Resolution::Failed,
        }
    }

    /// Returns the resolution of a container whose wait ran out.
    ///
    /// As in the Agent, an unknown container isn't a failure then. It resolves to no tags.
    fn into_expired_resolution(self) -> Resolution {
        Resolution::Tags(self.tags.unwrap_or_default())
    }
}
