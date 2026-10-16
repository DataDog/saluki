//! Runtime system.
//!
//! This module contains the core components of the runtime system, including supervisors and processes. It's directly
//! inspired by [Erlang/OTP](https://www.erlang.org/docs/28/system/design_principles#supervision-trees).
//!
//! To quote the Erlang/OTP documentation:
//!
//! > Workers are processes that perform computations and other actual work. Supervisors are processes that monitor
//! > workers. A supervisor can restart a worker if something goes wrong. The supervision tree is a hierarchical
//! > arrangement of code into supervisors and workers, which makes it possible to design and program fault-tolerant
//! > software.
//!
//! # Processes
//!
//! An asynchronous system is composed of independent units of computation running concurrently, such as a set of tasks
//! executing on a thread pool. We refer to these as **processes.** In other systems, these might be called _actors_,
//! _tasks_, _fibers_, _virtual threads_, _goroutines_, or something else. Processes are lightweight and able to be
//! (generally) created and destroyed cheaply.
//!
//! Processes have a few key attributes and invariants:
//!
//! - every process is a future that runs as an independent asynchronous task on a Tokio runtime
//! - every process has a unique numerical identifier and a semi-unique name
//!
//! Unlike Erlang processes, Saluki processes don't have an inherent mailbox or message passing capabilities. As well,
//! processes can't run by themselves. They must be _supervised_.
//!
//! # Supervisors
//!
//! Supervisors are themselves processes whose only job is to _supervise_ other processes, also called _workers_. In a
//! supervisor, workers are added and configured through a common convention that allows defining how the worker is
//! created (or recreated on failure), how many times it can be restarted, and more. Supervisors themselves can also be
//! workers, and so nested _supervision trees_ can be constructed.
//!
//! Supervisors include a number of configurable settings that allow customizing the behavior of how workers are
//! managed, which in turn allows building fault-tolerant systems: we can restart workers for transient failures, give
//! up for permanent failures, and so on.
//!
//! # Spawning children
//!
//! Usually, you declare children in advance with [`Supervisor::add_worker`]. A process can also start new children
//! while it runs. There are three ways to do this. They differ in the owner of the new child:
//!
//! - [`spawn`], which makes the new child a child of the current process. It is the equivalent of [`tokio::spawn`] for
//!   supervision. It is the recommended way to spawn for code that runs under supervision.
//! - [`spawn_sibling`], which gives the new child to the nearest supervisor above the current process. The child is a
//!   sibling of the current process only if that supervisor runs the process directly, and not in a scope.
//! - [`SupervisorHandle`], which names a supervisor explicitly.
//!
//! For all three ways, a [`ChildBuilder`] describes the child. To get a builder, use [`worker`] or [`supervisable`].
//! For a named supervisor, use the methods with the same names on [`SupervisorHandle`]. The builder is the only way to
//! configure a child. It produces a [`ChildSpecification`], which has no settings of its own. Because of this, you
//! cannot use the specification to make a combination of settings that the builder does not offer.
//!
//! A child that is itself a [`Supervisor`] can be handed to any of the above directly, which is all most callers need.
//! [`nested_supervisor`] and [`SupervisorHandle::nested_supervisor`] give access to the one setting that is not
//! available that way: whether the subtree is needed or hosted.
//!
//! All three ways to spawn are synchronous and cannot fail. As with [`tokio::spawn`], an accepted child does not always
//! run. If its owner stops before it reaches the child, the owner never starts the child.
//!
//! # Restart policy
//!
//! You do not select the restart policy of a child directly. Two properties of the child decide it:
//!
//! - Whether it can start again. A child reports this through [`Supervisable::can_restart`]. A [`Supervisable`] builds
//!   its work each time it starts, so it can usually start again. A child that consumes something when it starts is
//!   _one-shot_. Examples are a child built from a closure, which consumes its body, and a [`Supervisable`] that takes
//!   a connection.
//! - Whether its owner can work without it. The owner of a _needed_ child cannot work without it. The owner of a
//!   _hosted_ child only runs it. See [`ChildBuilder::needed`] and [`ChildBuilder::hosted`].
//!
//! Together, these two properties decide what occurs when a child stops without a stop request:
//!
//! | | Needed | Hosted |
//! | --- | --- | --- |
//! | Can start again | always restarted ([`Permanent`]) | restarted if it failed ([`Transient`]) |
//! | One-shot | its owner fails | removed, and its owner continues |
//!
//! [`Permanent`]: RestartType::Permanent
//! [`Transient`]: RestartType::Transient
//!
//! A child that stops because of a stop request is never restarted. A child of a scope that stops after the owner of
//! the scope is told to stop counts as stopped by request. If a child stops more often than the restart limit of its
//! owner allows, the owner fails. This applies to needed and hosted children. The next level of the supervision tree
//! then handles the failure.
//!
//! A child that can start again, and that you declare in advance with [`Supervisor::add_worker`], is needed by default.
//! This is because it is part of the purpose of its supervisor. Every other child is hosted by default.
//!
//! # Scopes
//!
//! A supervisor owns its children. Every supervised worker also owns the children that it spawns, and keeps them in its
//! [scope].
//!
//! Each of these children runs as a separate task. The worker does not finish until all of these children finish. When
//! the body of the worker returns, the scope stops them, each according to its own shutdown strategy, as a supervisor
//! does.
//!
//! A value can also own a scope, so that the background tasks of a primitive stop when the primitive stops. See
//! [`scope::nested`]. You can also create a scope before the process that will own it exists, with [`UnadoptedScope`].
//! This lets work spawned during the construction of something belong to the process that later runs that thing.
//!
//! # Where to spawn a child
//!
//! Spawn most children into the scope of the current process with [`spawn`]. Use a different way only when one of these
//! conditions is true:
//!
//! - A value, not a process, must own the child. Spawn the child into a [`scope::nested`] guard.
//! - A process other than the one that spawns the child must own it. Get the scope of the owner with
//!   [`scope::current`], and spawn the child with [`ChildBuilder::spawn_in`].
//! - The child must continue to run after restarts of the process that spawns it. Make both of them children of a
//!   [`Supervisor`], and spawn the child through its [`SupervisorHandle`].
//! - Children need their own restart strategy, shutdown budget, resource group, or runtime. A scope restarts its
//!   children at most once in each five-second period, for all of its children together. Put these children under a
//!   [`Supervisor`].
//!
//! If a worker body only waits for a stop request while its children do the work, use a [`Supervisor`] instead.
//!
//! # Supervision trees
//!
//! As supervisors can be nested, this allows building a tree of supervisors (hence _supervision trees_) where leaf
//! supervisors manage workers specific to a certain area, and parent supervisors manage the leaf supervisors. For
//! example, for a server application serving multiple API endpoints, each endpoint might be managed by a separate
//! supervisor: a worker for accepting connections, a worker for each connection, and so on. Above those supervisors, a
//! parent supervisor manages each leaf supervisor, and potentially other workers that provide necessary services
//! utilized by each endpoint, such as logging, metrics, or other infrastructure services.
//!
//! As every supervisor can define its own specific restart strategy, and behavior, this allows for more granular
//! grouping and control over which set of workers must be restarted if a related worker fails, and how those failures
//! propagate up and down the supervision tree.
//!
//! # Inspecting the tree
//!
//! A supervision tree is also a description of what a process is doing, which makes it worth being able to read at
//! runtime. [`Supervisor::tree_handle`] returns a read-only handle whose [`snapshot`][SupervisionTreeHandle::snapshot]
//! walks the tree from that supervisor downward, reporting each node's name, process, restart policy, restart count,
//! lifetime, and resource usage, with its children nested beneath it. A handle can be taken before the supervisor
//! starts and stays valid across every restart of it, which makes it suitable for handing to a worker that serves an
//! operator-facing diagnostics endpoint.
//!
//! # Examples
//!
//! See the `basic_supervisor` example which shows how supervisors and workers are composed together, as well as how
//! failed workers and supervisors are restarted.

mod process;
pub use self::process::get_sanitized_name;
pub use self::process::Id as ProcessId;
#[cfg(test)]
pub(crate) use self::process::Name;

pub mod state;

mod dedicated;
pub use self::dedicated::{RuntimeConfiguration, RuntimeMode};

mod restart;
pub use self::restart::{RestartMode, RestartStrategy, RestartType};

mod supervisable;
pub use self::supervisable::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};

mod supervisor;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use self::supervisor::ExitObserver;
pub use self::supervisor::{
    AutoShutdown, ChildId, ChildSpecification, ChildState, LoweredChild, Supervisor, SupervisorError, SupervisorHandle,
    SupervisorSpec, WorkerSpec,
};

mod tree;
pub use self::tree::{
    NodeKind, NodeSnapshot, NodeState, ResourceUsage, SupervisionSettings, SupervisionTreeAPIHandler,
    SupervisionTreeHandle, SupervisionTreeState, SupervisionTreeWorker, TreeSnapshot, TreeTotals, UnixMillis,
    SUPERVISION_TREE_ROUTE,
};

mod spawn;
pub use self::spawn::{spawn, spawn_sibling};

pub mod scope;
pub use self::scope::{Scope, ScopeError, ScopeGuard, UnadoptedScope};

mod builder;
pub use self::builder::{
    nested_supervisor, supervisable, worker, worker_with_shutdown, BuilderState, ChildBuilder, NestedSupervisorBuilder,
    OneShot, Restartable,
};

mod workers;
pub use self::workers::{FnWorker, IntoWorkerResult};

mod worker_state;
