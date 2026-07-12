//! Diagnostics.
//!
//! This module provides a subsystem-scoped control surface, [`DiagnosticsEmitter`], for exposing diagnostics to the
//! rest of the system in a decoupled, eventually consistent way. A subsystem uses it to register on-demand artifact
//! [collectors][DiagnosticCollector] and to emit abstract [events][DiagnosticEvent], both carried over the runtime
//! dataspace.

mod collector;
pub use self::collector::DiagnosticCollector;

mod emitter;
pub use self::emitter::{subscribe_events, DiagnosticsEmitter, DiagnosticsEmitterError};

mod event;
pub use self::event::{DiagnosticDetails, DiagnosticEvent};
