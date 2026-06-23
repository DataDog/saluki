//! # Configuration System for `agent-data-plane`.
//!
//! This system is a facade over one or more input languages that can configure ADP. It provides
//! a black box abstraction of Datadog Agent configuration so that knowledge of that domain does
//! not leak throughout other systems.
//!
//! ## Responsibilities
//!
//! - Map Datadog configuration to Saluki and ADP types
//! - Own the loading and lifecycle of configuration sources
//! - Receive and translate a remote configuration stream including live updates
//!
//! ## Workspace Dependency Boundaries
//!
//! This is the only ADP production crate permitted access to `saluki-config-tools` raw-map APIs,
//! and it must not depend on `saluki-components`.
//!
//! ```text
//!             +----------------------------+
//!             |   bin/agent-data-plane     |
//!             +----------------------------+
//!               |           |            |
//!               |           |            +------------------+
//!               |           |                               |
//!               |           |           +-------------------v--------------------+
//!               |           |           |  agent-data-plane-config-system        |
//!               |           |           |  facade: loading, translate, lifecycle |
//!               |           |           +----------------------------------------+
//!               |           |             |        |        |            |
//!               |           |       +-----+        |        |            |
//!               |           |       |              |        |            |
//!               v           v       v              |        |            |
//!    +----------------+  +----------------------+  |        |            |
//!    | saluki-        |  | agent-data-plane-    |  |        |            |
//!    | components     |  | config               |  |        |            |
//!    +----------------+  +----------------------+  |        |            |
//!             |                     |              |        |            |
//!             |                     v              |        |            |
//!             |           +----------------------+ |        |            |
//!             +---------->| saluki-component-    |<+        |            |
//!                         | config               |          |            |
//!                         +----------------------+          |            |
//!                                                           v            v
//!                                               +----------------+  +----------------+
//!                                               | saluki-config- |  | datadog-agent- |
//!                                               | tools          |  | config         |
//!                                               +----------------+  +----------------+
//! ```
//!

// TODO(visibility): add crate-boundary architectural guard when arch tests are wired up
mod system;
pub(crate) mod translate;

pub use self::system::{ConfigurationSystem, ConfigurationSystemLoader};

#[cfg(test)]
mod smoke_test;
