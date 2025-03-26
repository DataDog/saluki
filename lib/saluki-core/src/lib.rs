//! Core primitives for building Saluki-based data planes.
#![deny(warnings)]
#![deny(missing_docs)]

#[doc(hidden)]
pub mod reexport {
    pub use paste::paste;
}

pub mod components;
pub mod constants;
pub mod observability;
pub mod pooling;
pub mod state;
pub mod task;
pub mod topology;
