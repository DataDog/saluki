//! Shared runtime infrastructure for a process that acts as a remote agent for the Datadog Agent.
//!
//! A remote agent is a process that runs alongside the Datadog Agent and attaches to it over the Remote Agent Registry
//! (RAR): it registers itself, receives its configuration from the Agent, and serves the status, flare, and telemetry
//! services that the Agent queries. This crate provides those pieces, so that each remote agent only supplies what is
//! truly its own: its configuration system, its topology, its control plane supervisor, its status sections, its
//! telemetry remapping rules, and any extra API handlers.
//!
//! # Getting started
//!
//! Most remote agents use the [`facade`] module, which drives the whole startup sequence. A binary bootstraps the
//! process with `saluki_app::bootstrap::AppBootstrapper`, implements [`LocalConfiguration`][facade::LocalConfiguration]
//! and [`AuthoritativeConfiguration`][facade::AuthoritativeConfiguration] for its configuration system, ties them
//! together with [`RemoteAgentSpecification`][facade::RemoteAgentSpecification] on a marker type, and then walks
//! through the stages:
//!
//! 1. [`RemoteAgent::start`][facade::RemoteAgent::start] registers with the Datadog Agent, waits for the initial
//!    configuration from its configuration stream, and reloads logging to match. In standalone mode, the local
//!    configuration becomes authoritative instead.
//! 2. [`RemoteAgent::initialize`][facade::RemoteAgent::initialize] creates the component and health registries and the
//!    [`RemoteAgentEnvironmentProvider`][env::RemoteAgentEnvironmentProvider], which connects to the Datadog Agent for
//!    the hostname, autodiscovery, and workload metadata.
//! 3. [`RemoteAgent::run`][facade::RemoteAgent::run] takes the binary's topology and control plane supervisor, creates
//!    the rest of the supervisors, validates the memory bounds, and runs the supervision tree until the process is asked
//!    to shut down.
//!
//! Each stage is a type parameter of [`RemoteAgent`][facade::RemoteAgent], so only the methods for the current stage
//! are available. The binary regains control between stages: to check its configuration before anything starts, and to
//! build its topology and its control plane. `examples/minimal_remote_agent.rs` shows a complete binary.
//!
//! # Supervision tree
//!
//! The facade builds this tree, where `{identifier}` is the identifier of the application details that the binary
//! registers with `saluki_metadata::set_app_details`:
//!
//! ```text
//! {identifier}-root                 one-for-one, no restarts
//! ├── bootstrap supervisor          from AppBootstrapper
//! ├── internal-sup
//! │   ├── control plane             from the binary
//! │   │   └── remote-agent          from RemoteAgent::create_remote_agent_supervisor
//! │   ├── resource registry
//! │   ├── config-updates            connected mode only
//! │   └── env-provider              connected mode only
//! │       ├── workload
//! │       └── autodiscovery
//! └── the binary's topology
//! ```
//!
//! A configuration stream that closes fails `config-updates` on every restart, which escalates to the root and stops the
//! process, rather than leaving it to run on a configuration that can no longer change.
//!
//! # Control plane
//!
//! Each remote agent builds its own control plane supervisor from the workers it needs. ADP's `ctrl-pln` runs on a
//! dedicated single-threaded runtime and holds:
//!
//! - the health registry's worker, and [`ResourceTelemetryWorker`][saluki_app::accounting::ResourceTelemetryWorker]
//! - [`InternalTelemetryAPIWorker`][saluki_core::observability::metrics::InternalTelemetryAPIWorker], for `/metrics`
//!   and `/compat/metrics`
//! - [`DynamicLogLevelWorker`][saluki_app::logging::DynamicLogLevelWorker], fed by
//!   [`LoggingTranslator::translate_log_levels`][logging::LoggingTranslator::translate_log_levels]
//! - [`ConfigWorker`][saluki_app::config::ConfigWorker] for `/config`, and
//!   [`ConfigRuntimeWorker`][control_plane::ConfigRuntimeWorker] for `/config/runtime`
//! - the unprivileged and privileged API endpoints, built with [`APIBuilder`][saluki_app::api::APIBuilder], where the
//!   privileged endpoint requires the Agent IPC certificate
//! - the `remote-agent` supervisor from [`RemoteAgent::create_remote_agent_supervisor`][facade::RemoteAgent::create_remote_agent_supervisor]
//!
//! These workers publish their routes as dynamic routes, so whichever API endpoint runs in the same supervision tree
//! serves them.
//!
//! # Hooks
//!
//! A remote agent can:
//!
//! - add sections to the status output with [`StatusSectionProvider`][remote_agent::StatusSectionProvider]
//! - select and rename the internal metrics reported to the Datadog Agent as telemetry
//! - serve `/compat/metrics` with its own remapping rules
//! - serve its own routes on the unprivileged and privileged API endpoints
//!
//! Its [`LoggingTranslator`][logging::LoggingTranslator] sets its default log file and adds its own crates to the
//! first-party log targets that a plain log level applies to.
//!
//! # Building blocks
//!
//! Each module can also be used on its own, without the facade:
//!
//! - [`remote_agent`]: registration, the configuration stream, and the workers that serve the status, flare, and
//!   telemetry services
//! - [`logging`]: translation of the Datadog Agent's logging settings and runtime log level changes
//! - [`env`](mod@env): the host, autodiscovery, and workload providers
//! - [`control_plane`]: workers that serve the internal APIs and keep the configuration current
//!
//! # Conventions
//!
//! Constructors don't spawn tasks. Long-running work is returned as a
//! [`Supervisable`][saluki_core::runtime::Supervisable] worker or [`Supervisor`][saluki_core::runtime::Supervisor] for
//! the caller to place in its supervision tree.
//!
//! [`RemoteAgentBootstrap`][remote_agent::RemoteAgentBootstrap] is the one exception for now. Registration has to
//! finish before the supervision tree exists, so [`RemoteAgentBootstrap::new`][remote_agent::RemoteAgentBootstrap::new]
//! spawns the registration loop, and
//! [`RemoteAgentBootstrap::create_config_stream`][remote_agent::RemoteAgentBootstrap::create_config_stream] spawns the
//! configuration stream loop. Neither task is supervised, and neither stops on shutdown.
//!
//! Each subsystem that talks to the Datadog Agent opens its own connection with
//! [`RemoteAgentClient::connect`][datadog_agent_commons::ipc::client::RemoteAgentClient::connect], rather than sharing a
//! clone of another subsystem's client.
//!
//! Code in this crate logs under the `datadog_agent_runtime` target, which the `datadog_agent` prefix in
//! [`FIRST_PARTY_LOG_TARGETS`][logging::FIRST_PARTY_LOG_TARGETS] covers.
//!
//! # Features
//!
//! - `taskdump`: includes a Tokio task dump (`runtime-dump.txt`) in flares. Tokio only supports task dumps on Linux, on
//!   `aarch64`, `x86`, `x86_64`, and `s390x`, so the feature does nothing on any other target. On those targets, the
//!   build also requires `--cfg tokio_unstable`.
#![deny(missing_docs)]

pub mod agent_stream;
pub mod control_plane;
pub mod env;
pub mod facade;
pub mod logging;
pub mod remote_agent;
