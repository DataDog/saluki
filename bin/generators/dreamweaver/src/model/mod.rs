//! Resolved runtime model types.

mod architecture;
mod service;
pub mod trace_context;

pub use self::architecture::ResolvedArchitecture;
pub use self::service::ResolvedService;
pub use self::trace_context::TraceContext;
