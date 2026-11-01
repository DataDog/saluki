//! Case loading, lifecycle primitives and run reporting for the panoramic test runner.
//!
//! This crate is independent of the products under test. Knowledge of a product belongs to its participant type definition.

#![deny(warnings)]
#![deny(missing_docs)]

pub mod case;
pub mod deadline;
pub mod events;
pub mod interrupts;
pub mod layout;
pub mod ledger;
pub mod machine_output;
pub mod participant;
pub mod plan;
pub mod probe;
pub mod record;
pub mod result;
pub mod step;
pub mod tasks;
