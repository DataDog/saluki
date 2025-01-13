//! Metric context and context resolving.
#![deny(warnings)]
#![deny(missing_docs)]

mod context;
pub use self::context::{ConcreteResolvable, Context, Resolvable, TagVisitor};

mod expiry;

mod hash;

pub mod origin;

mod resolver;
pub use self::resolver::{ContextResolver, ContextResolverBuilder};

pub mod tags;
