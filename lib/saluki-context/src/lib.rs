//! Metric context and context resolving.
#![deny(warnings)]
#![deny(missing_docs)]

mod context;
pub use self::context::Context;

mod expiry;

mod hash;

mod resolver;
pub use self::resolver::{ContextResolver, ContextResolverBuilder};

pub mod tags;
