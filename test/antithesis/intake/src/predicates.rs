//! W-property predicates evaluated by the intake handler.
//!
//! Each W function answers "does this property hold for the given input?" --
//! typically returning `bool` or `Option<T>` where `Some` means a violation
//! was observed and carries the offending value. The intake handler turns each
//! return into an `assert_always!` call against the Antithesis SDK.
//!
//! Ported from the `invariant-jig` checker. The property definitions live in
//! that project's `README.md` §Properties.Payloads. The numbering (W1-W22) and
//! semantics are preserved verbatim; only the proto access layer changes, from
//! `prost` to the `rust-protobuf` types in `datadog_protos`.

pub mod constants;
pub mod metric_point;
pub mod payload;
pub mod series;
