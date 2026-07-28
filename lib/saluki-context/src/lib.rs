//! Metric context and context resolving.
#![deny(warnings)]
#![deny(missing_docs)]

mod context;
pub use self::context::{Context, TagSetMutView, TagSetMutViewState};

mod hash;
pub use self::hash::{hash_context_with_host, ContextKey};

pub mod origin;

mod resolver;
pub use self::resolver::{ContextResolver, ContextResolverBuilder, TagsResolver, TagsResolverBuilder};

pub mod tags;
