//! Shared Datadog APM building blocks for trace-processing components.
//!
//! - [`semantics`] maps attribute concepts to the names used by OpenTelemetry and Datadog conventions, and serves the
//!   embedded registry or the latest one delivered by Remote Configuration.
//! - [`sampling`] receives trace sampling settings from Remote Configuration and resolves them against a consumer's
//!   local settings.

#![deny(missing_docs)]

pub mod sampling;
pub mod semantics;
