//! Reads recorded Datadog Agent configuration for tests of Agent Data Plane (ADP) compatibility.
//!
//! The Agent and ADP read configuration in Go and Rust respectively. Their conversions can differ:
//! a value sent to ADP as a string may be read by the Agent as a map. These tests need examples from
//! the Agent's own code, not assumed values or copies of its conversion logic.
//!
//! The **config recorder**, a Go program in `lib/datadog-agent/config-recorder/`, runs that code
//! with specified inputs. For each test **case**, it records the values and sources sent over the
//! Agent's config stream, plus results from methods such as `GetInt` and `GetStringMap`. These
//! methods are called **getters**. Their results show what Agent components read from configuration.
//! The saved collection of cases is the **corpus**, checked in as `config-recorder/corpus.jsonl`.
//!
//! This crate lets Rust tests use those recordings without running Go or Docker. [`read`] validates
//! the file and returns a [`Corpus`] with typed inputs, stream settings, and getter results. It fills
//! in omitted fields according to the format rules and reports invalid records as [`Violation`]s.
//! It does not run ADP or compare ADP's behavior with the Agent's.
//!
//! The format is defined in `config-recorder/docs/record.md`, with input values in `case.md` and
//! getter selection and result encoding in `getter-map.md`. The reader follows these documents
//! independently of the Go writer, so a writer bug is not copied into both implementations.
//! Unit tests also check that the saved recordings match the current schema and recorder inputs.
#![deny(missing_docs)]

mod getter;
mod json;
mod lists;
mod model;
mod reader;

pub use getter::{GetterResult, GoFloat, GoValue, Number};
pub use lists::{Getter, Group, Level, Source, REVIEWED_AT_AGENT_COMMIT};
pub use model::*;
pub use reader::read;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod corpus_checks;
