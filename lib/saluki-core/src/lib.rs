//! Core primitives for building Saluki-based data planes.
#![deny(warnings)]
#![deny(missing_docs)]
// This deals with some usages that only show up in test code and which aren't real issues.
#![cfg_attr(test, allow(clippy::mutable_key_type))]

#[doc(hidden)]
pub mod reexport {
    pub use paste::paste;
}

pub mod components;
pub mod constants;
pub mod data_model;
pub mod observability;
pub mod pooling;
pub mod runtime;
pub mod state;
pub mod topology;
