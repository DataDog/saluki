//! Shared runtime infrastructure for a process that acts as a remote agent for the Datadog Agent.
//!
//! A remote agent is a process that runs alongside the Datadog Agent and attaches to it over the Remote Agent Registry
//! (RAR): it registers itself, receives its configuration from the Agent, and serves the status, flare, and telemetry
//! services that the Agent queries. This crate provides those pieces, so that each remote agent only supplies what is
//! truly its own.
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
