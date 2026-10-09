//! Shared runtime infrastructure for a process that runs as a subagent of the Datadog Agent.
//!
//! A subagent is a process that runs alongside the Datadog Agent and attaches to it over the Remote Agent Registry
//! (RAR): it registers itself, receives its configuration from the Agent, and serves the status, flare, and telemetry
//! services that the Agent queries. This crate provides those pieces, so that each subagent only supplies what is truly
//! its own.
//!
//! # Conventions
//!
//! Constructors never spawn tasks. Long-running work is returned as a [`Supervisable`][saluki_core::runtime::Supervisable]
//! worker or [`Supervisor`][saluki_core::runtime::Supervisor] for the caller to place in its supervision tree.
//!
//! Each subsystem that talks to the Datadog Agent opens its own connection with
//! [`RemoteAgentClient::connect`][datadog_agent_commons::ipc::client::RemoteAgentClient::connect], rather than sharing a
//! clone of another subsystem's client.
//!
//! # Features
//!
//! - `taskdump`: includes a Tokio task dump (`runtime-dump.txt`) in flares. Tokio only supports task dumps on Linux, on
//!   `aarch64`, `x86`, `x86_64`, and `s390x`, and only when building with `--cfg tokio_unstable`; enabling the feature
//!   anywhere else fails the build. Enable it only for targets that meet both requirements.
#![deny(missing_docs)]

pub mod remote_agent;
