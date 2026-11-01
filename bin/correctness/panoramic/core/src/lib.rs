//! Case lifecycle primitives for the panoramic test runner.
//!
//! This crate is independent of the products under test. Knowledge of a product belongs to its participant type definition.

#![deny(warnings)]
#![deny(missing_docs)]

pub mod deadline;
pub mod interrupts;
pub mod probe;
pub mod tasks;
