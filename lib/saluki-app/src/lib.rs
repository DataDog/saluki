//! High-level application primitives.
//!
//! This crate provides common primitives necessary for bootstrapping an application prior to running, such as
//! initializing logging, metrics, and memory management.
#![deny(warnings)]
#![deny(missing_docs)]
#![allow(unused_imports)]
#![allow(dead_code)]

#[cfg(feature = "api")]
pub mod api;

pub mod bootstrap;

#[cfg(feature = "logging")]
pub mod logging;

#[cfg(feature = "memory")]
pub mod memory;

#[cfg(feature = "metrics")]
pub mod metrics;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "config")]
pub mod config;
