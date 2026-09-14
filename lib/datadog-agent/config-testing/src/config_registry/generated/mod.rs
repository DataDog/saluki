//! Annotation tables for every configuration key ADP knows about.
//!
//! Every other file in this directory is written by `build.rs` from `schema_overlay.yaml` and
//! `SALUKI_KEYS`, and is excluded by the `.gitignore` beside this one: the tables restate the
//! overlay, so committing them adds nothing to a review. Run `make build-schema-overlay`, or any
//! build of this crate, to produce them.
//!
//! The generated files import the annotation types with `use super::*`, which the glob below
//! resolves to the parent module.

#[allow(unused_imports)]
use super::*;

include!("annotations_index.rs");
