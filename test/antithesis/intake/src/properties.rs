//! Payload property checks. Each property is a self-contained unit: its check,
//! its Antithesis `assert_always!`, and its response error message live
//! together. The Pyld01-Pyld22 definitions and the Category grouping live in the
//! crate `README.md`. The checks read the `rust-protobuf` types in
//! `datadog_protos`.

pub(crate) mod payload;
