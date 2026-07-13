//! Subsystem diagnostics.
//!
//! This module provides a generalized system for exposing diagnostic information from a subsystem, both in a pull and push fashion.

mod collector;
pub use self::collector::DiagnosticCollector;

mod emitter;
pub use self::emitter::{subscribe_events, DiagnosticsEmitter, DiagnosticsEmitterError};

mod event;
pub use self::event::{DiagnosticDetails, DiagnosticEvent};
