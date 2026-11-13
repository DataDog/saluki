//! Golden payload harness for the v1.0 traces endpoint.
//!
//! This crate integrates the golden value tests recorded by the Go trace-agent harness
//! (`pkg/trace/agent/golden_v1_test.go` in `datadog-agent`). It has two independent halves:
//!
//! - [`cases`] loads the golden input specs, resolves their dynamic span times against a caller-supplied clock, and
//!   encodes them as `/v1.0/traces` requests.
//! - [`compare`] checks actual output (APM stats and trace payloads) against the golden outputs semantically: stats
//!   are compared after aggregating across buckets, and trace payloads are compared independently of string table
//!   ordering and absolute span start times.
//!
//! The v1.0 encoder is hand-written to match the Go encoder exactly; see the [`cases`] encoder docs for why the
//! `libdd-trace-utils` v1 encoder isn't used.
//!
//! The fixtures are bundled under `testdata/v1/`; see the `README.md` there for their provenance.
#![deny(warnings)]
#![deny(missing_docs)]

use std::path::PathBuf;

pub mod cases;
pub mod compare;

/// Agent hostname configured by the Go harness when the golden files were recorded.
pub const GOLDEN_AGENT_HOSTNAME: &str = "agent-host";

/// API key configured by the Go harness when the golden files were recorded.
pub const GOLDEN_API_KEY: &str = "test";

/// Returns the directory holding the bundled fixtures (`cases/` and `golden/`).
pub fn bundled_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join("v1")
}

/// Returns the path of the bundled golden stats file (`v1_<case>.json`) for a case.
pub fn golden_stats_path(case: &str) -> PathBuf {
    bundled_fixtures_dir().join("golden").join(format!("v1_{}.json", case))
}

/// Returns the path of the bundled golden wire file (`v1_<case>_wire.json`) for a case.
pub fn golden_wire_path(case: &str) -> PathBuf {
    bundled_fixtures_dir()
        .join("golden")
        .join(format!("v1_{}_wire.json", case))
}
