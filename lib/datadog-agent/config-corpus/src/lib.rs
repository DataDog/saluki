//! A strict, typed reader of the config recorder's corpus (`lib/datadog-agent/config-recorder/corpus.jsonl`).
//!
//! The corpus format is fixed by its contract, record.md, with getter results encoded by
//! getter-map.md §3 and typed inputs by case.md §7. This crate is written from those documents
//! alone, never from the recorder's Go code, so that the two sides can disagree visibly.
//!
//! [`read`] is the only parser of the corpus. It checks every format rule a file alone can show, and
//! returns a [`Corpus`] in which every member a writer may omit is filled in:
//!
//! - `inputs.keys` (record.md §3.1);
//! - each event's `update` (record.md §5.2);
//! - each read's `source` (record.md §5.3).
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
