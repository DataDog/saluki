//! High-level application primitives.
//!
//! This crate provides common primitives necessary for bootstrapping an application prior to running, such as
//! initializing logging, metrics, and memory management.
#![deny(warnings)]
#![deny(missing_docs)]
#![allow(unused_imports)]
#![allow(dead_code)]

pub mod api;
pub mod bootstrap;
pub mod config;
pub mod logging;
pub mod memory;
pub mod metrics;
mod tls;
