//! Programmable metric pipeline intermediate representation (IR), static validation, and telemetry analysis.
//!
//! A [`MetricPlan`] is a whole-program directed acyclic graph over a small set of composable metric operators.
//! Policy settings are written as literals inside the plan; only runtime host facts and live operational signals
//! remain bindings.

#![deny(warnings)]
#![deny(missing_docs)]

pub mod effects;
pub mod ir;
pub mod telemetry_check;
pub mod validate;

pub use self::effects::{operator_spec, EffectFlags, OperatorSpec, PortArity};
pub use self::ir::{Edge, MetricPlan, MetricPlanBuilder, Node, NodeId, Op, RouteFilterKind};
pub use self::telemetry_check::{analyze_telemetry_contracts, TelemetryContract, TelemetryWarning};
pub use self::validate::{validate_plan, ValidatedPlan, ValidationError};
