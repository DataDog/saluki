use std::{
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    task::{Context, Poll},
};

use memory_accounting::allocator::{AllocationGroupRegistry, AllocationGroupToken, Track as _, Tracked};
use pin_project::{pin_project, pinned_drop};
use tracing::{debug_span, instrument::Instrumented, Instrument as _};

use super::state::{DataspaceRegistry, CURRENT_DATASPACE};

static GLOBAL_PROCESS_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

/// Process identifier.
///
/// A simple, numeric identifier that uniquely identifies a process.
///
/// Guaranteed to be unique.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(usize);

tokio::task_local! {
    pub(crate) static CURRENT_PROCESS_ID: Id;
}

impl Id {
    /// The root process identifier, representing the global/unnamed process context.
    pub const ROOT: Self = Self(0);

    /// Creates a new process identifier.
    pub fn new() -> Self {
        let id = GLOBAL_PROCESS_ID_COUNTER.fetch_add(1, Relaxed);
        Self(id)
    }

    /// Returns the process identifier for the currently executing process.
    ///
    /// If called outside of a process context, returns `Id::ROOT`.
    pub fn current() -> Self {
        CURRENT_PROCESS_ID.try_with(|id| *id).unwrap_or(Self::ROOT)
    }

    /// Returns the raw numeric value of this identifier.
    pub fn as_usize(&self) -> usize {
        self.0
    }
}

/// Process name.
///
/// A human-readable name for a process that only contains alphanumeric characters, underscores, and periods.
///
/// Process names are scoped, such that the resulting process name is nested. For example, if a supervisor has a name of
/// `topology_sup`, and a child process is added to that supervisor with a name of `worker`, the resulting process name
/// for the child process will be `topology_sup.worker`. Process names can be arbitrarily nested in this way.
///
/// Process names will be sanitized if they contain invalid characters, such as hyphens or spaces. Invalid characters
/// will be replaced with underscores.
///
/// Not guaranteed to be unique.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Name(Arc<str>);

impl Name {
    pub(crate) fn root<N: AsRef<str>>(name: N) -> Option<Self> {
        let name = name.as_ref();
        if name.is_empty() {
            return None;
        }

        Some(Self(get_sanitized_name(name)))
    }

    pub(crate) fn scoped<N: AsRef<str>>(parent: &Name, name: N) -> Option<Self> {
        let name = name.as_ref();
        if name.is_empty() {
            return None;
        }

        let sanitized_name = get_sanitized_name(name);
        let scoped_name = format!("{}.{}", parent.0, sanitized_name);
        Some(Self(scoped_name.into()))
    }
}

impl Deref for Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A runtime process.
#[derive(Clone)]
pub struct Process {
    id: Id,
    name: Name,
    alloc_group_token: AllocationGroupToken,
    dataspace: DataspaceRegistry,
}

impl Process {
    pub(crate) fn supervisor<N: AsRef<str>>(name: N, parent: Option<&Process>) -> Option<Self> {
        let name = parent
            .and_then(|p| Name::scoped(&p.name, &name))
            .or_else(|| Name::root(name))?;
        let alloc_group_token = AllocationGroupRegistry::global().register_allocation_group(&*name);
        let dataspace = parent.map(|p| p.dataspace.clone()).unwrap_or_default();
        Some(Self::from_parts(Id::new(), name, alloc_group_token, dataspace))
    }

    pub(crate) fn supervisor_with_dataspace<N: AsRef<str>>(
        name: N, parent: Option<&Process>, dataspace: Option<DataspaceRegistry>,
    ) -> Option<Self> {
        let name = parent
            .and_then(|p| Name::scoped(&p.name, &name))
            .or_else(|| Name::root(name))?;
        let alloc_group_token = AllocationGroupRegistry::global().register_allocation_group(&*name);
        let dataspace = dataspace
            .or_else(|| parent.map(|p| p.dataspace.clone()))
            .unwrap_or_default();
        Some(Self::from_parts(Id::new(), name, alloc_group_token, dataspace))
    }

    pub(crate) fn worker<N: AsRef<str>>(name: N, parent: &Process) -> Option<Self> {
        let name = Name::scoped(&parent.name, name)?;
        Some(Self::from_parts(
            Id::new(),
            name,
            parent.alloc_group_token,
            parent.dataspace.clone(),
        ))
    }

    fn from_parts(id: Id, name: Name, alloc_group_token: AllocationGroupToken, dataspace: DataspaceRegistry) -> Self {
        Self {
            id,
            name,
            alloc_group_token,
            dataspace,
        }
    }

    /// Returns the process identifier.
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// Returns the dataspace registry associated with this process.
    pub(crate) fn dataspace(&self) -> &DataspaceRegistry {
        &self.dataspace
    }

    pub fn into_instrumented<F>(self, inner: F) -> InstrumentedProcess<F>
    where
        F: Future,
    {
        InstrumentedProcess::new(self, inner)
    }
}

/// An instrumented process.
#[pin_project(PinnedDrop)]
pub struct InstrumentedProcess<F> {
    process_id: Id,
    dataspace: DataspaceRegistry,
    #[pin]
    inner: Instrumented<Tracked<F>>,
}

impl<F> InstrumentedProcess<F>
where
    F: Future,
{
    pub(crate) fn new(process: Process, inner: F) -> Self {
        let span = debug_span!(
            "process",
            process_id = process.id().as_usize(),
            process_name = &*process.name,
        );

        let process_id = process.id;
        let dataspace = process.dataspace.clone();
        let inner = inner.track_allocations(process.alloc_group_token).instrument(span);

        Self {
            process_id,
            dataspace,
            inner,
        }
    }
}

impl<F> Future for InstrumentedProcess<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        CURRENT_PROCESS_ID.sync_scope(*this.process_id, || {
            CURRENT_DATASPACE.sync_scope(this.dataspace.clone(), || this.inner.poll(cx))
        })
    }
}

#[pinned_drop]
impl<F> PinnedDrop for InstrumentedProcess<F> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        this.dataspace.retract_all_for_process(*this.process_id);
    }
}

/// Helper trait for running process futures with instrumentation.
pub trait ProcessExt {
    /// Converts the process future into an instrumented future.
    fn into_instrumented(self, process: Process) -> InstrumentedProcess<Self>
    where
        Self: Future + Sized;
}

impl<F> ProcessExt for F
where
    F: Future,
{
    fn into_instrumented(self, process: Process) -> InstrumentedProcess<Self>
    where
        Self: Future + Sized,
    {
        process.into_instrumented(self)
    }
}

fn is_process_name_segment_valid(name: &str) -> bool {
    // Process name cannot be empty.
    if name.is_empty() {
        return false;
    }

    // Process names cannot start or end with anything other than alphanumeric characters.
    if !name.starts_with(|c: char| c.is_alphanumeric()) || !name.ends_with(|c: char| c.is_alphanumeric()) {
        return false;
    }

    // Process name segments can only include alphanumeric characters and underscores.
    //
    // Periods are allowed in process names overall, but they're only used as separators between segments.
    for c in name.chars() {
        if !c.is_alphanumeric() && c != '_' {
            return false;
        }
    }

    true
}

fn get_sanitized_name(name: &str) -> Arc<str> {
    if is_process_name_segment_valid(name) {
        name.into()
    } else {
        // Replace invalid characters with underscores, and collapses multiple underscores into a single one.
        let raw_sanitized = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' });
        let mut sanitized = String::with_capacity(name.len());

        let mut last_was_underscore = true;
        for c in raw_sanitized {
            if c == '_' {
                if !last_was_underscore {
                    sanitized.push(c);
                    last_was_underscore = true;
                }
            } else {
                sanitized.push(c);
                last_was_underscore = false;
            }
        }

        // Remove all non-alphanumeric characters from beginning and end.
        let trimmed = sanitized.trim_matches(|c: char| !c.is_alphanumeric());
        Arc::from(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_name_root() {
        let cases = [
            ("topology_sup", Some("topology_sup")),
            ("worker", Some("worker")),
            ("worker.", Some("worker")),
            ("_worker_", Some("worker")),
            ("worker-123", Some("worker_123")),
            ("--worker_123", Some("worker_123")),
            ("worker 123", Some("worker_123")),
            ("worker===123", Some("worker_123")),
            ("topology.worker", Some("topology_worker")),
            ("", None),
        ];

        for (input, expected) in cases {
            let name = Name::root(input);
            assert_eq!(name.as_deref(), expected);
        }
    }

    #[test]
    fn test_process_name_scoped() {
        let parent = Name::root("topology_sup").unwrap();
        let cases = [
            ("worker", Some("topology_sup.worker")),
            ("worker.", Some("topology_sup.worker")),
            ("_worker_", Some("topology_sup.worker")),
            ("worker-123", Some("topology_sup.worker_123")),
            ("--worker_123", Some("topology_sup.worker_123")),
            ("worker 123", Some("topology_sup.worker_123")),
            ("worker===123", Some("topology_sup.worker_123")),
            ("nested.worker", Some("topology_sup.nested_worker")),
            ("", None),
        ];

        for (input, expected) in cases {
            let name = Name::scoped(&parent, input);
            assert_eq!(name.as_deref(), expected);
        }
    }
}
