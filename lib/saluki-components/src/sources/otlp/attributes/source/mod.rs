//! OTLP source representation.
//!
//! This module re-exports the shared OTLP source types defined in
//! `common/otlp/util.rs` so callers that previously depended on this path can
//! keep their imports unchanged.

pub use crate::common::otlp::util::{Source, SourceKind};
