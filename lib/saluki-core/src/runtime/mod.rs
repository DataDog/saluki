//! Runtime system.
//!
//! This module contains the core components of the runtime system, including supervisors and processes. It is directly
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
//! Processes cannot run by themselves, however. They must be _supervised_.
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
//! # Examples
//!
//! See the `basic_supervisor` example which shows how supervisors and workers are composed together, as well as how
//! failed workers and supervisors are restarted.

mod process;

mod restart;
pub use self::restart::{RestartMode, RestartStrategy};

mod supervisor;
pub use self::supervisor::{ShutdownStrategy, Supervisable, Supervisor, SupervisorError, SupervisorFuture};

mod shutdown;
pub use self::shutdown::{ProcessShutdown, ShutdownHandle};
