//! Logical plans for programmable metric processing.
//!
//! A **plan** describes what happens to metrics -- filtering, transformation, routing, aggregation, and delivery -- as
//! a directed acyclic graph of operators, independently of how it is executed. This crate defines the semantic model
//! that plans are written against, the plan's intermediate representation (IR) and its JSON serialization, the
//! predicate language used for routing, canonical forms and hashes, and the contracts for the extension points that
//! later capabilities plug into.
//!
//! - [`model`]: metric kinds, port types, fields, and [`EventView`][model::EventView], the read-only view of a metric
//!   that predicates evaluate against.
//! - [`ir`]: plans, nodes, edges, parameters, outputs, scopes, and policy artifacts, with versioning and a generated
//!   JSON Schema.
//! - [`canonical`]: RFC 8785 canonical JSON, and the content hashes computed over it.
//! - [`predicate`]: the predicate language, its canonical form, and its evaluation.
//! - [`limits`]: the resource and timing limits that plans are checked and run against.
//! - [`extension`]: the extension slots that the core reserves, each with its native default.
//! - [`operators`]: the closed set of operators, with their parameters, ports, and effect contracts.
//! - [`validate`]: plan validation, including the duplicate contribution analysis.
//! - [`interpret`]: the reference interpreter, which executes a plan over a trace of events literally.
//! - [`equivalence`]: the equivalence oracle, which compares what executions deliver, point by point.
//! - [`explain`]: the versioned description of a plan, as JSON and as text.
//!
//! With the `test-util` feature, the `testing` module provides property-testing strategies for metrics and predicates.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod canonical;
pub mod equivalence;
pub mod explain;
pub mod extension;
pub mod interpret;
pub mod ir;
pub mod limits;
pub mod model;
pub mod operators;
pub mod predicate;
pub mod validate;

#[cfg(any(test, feature = "test-util"))]
pub mod testing;
