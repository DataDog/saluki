#![cfg(feature = "context-dump-benchmark")]

//! Benchmark-only access to DogStatsD context artifact production code.

use std::path::{Path, PathBuf};

use saluki_components::transforms::AggregateContextSnapshotEntry;
use saluki_error::GenericError;

#[cfg(feature = "context-dump-benchmark")]
#[allow(
    dead_code,
    unused_imports,
    reason = "the benchmark facade includes the complete binary-owned context dump module"
)]
#[path = "dogstatsd_contexts/mod.rs"]
mod dogstatsd_contexts;

/// Publishes a DogStatsD context dump through the production artifact writer.
#[doc(hidden)]
pub fn publish_dogstatsd_context_dump(
    run_path: &Path, snapshot: &[AggregateContextSnapshotEntry],
) -> Result<PathBuf, GenericError> {
    dogstatsd_contexts::publish_context_dump(run_path, snapshot)
}

/// Counts DogStatsD context dump records through the production artifact reader.
#[doc(hidden)]
pub fn count_dogstatsd_context_dump_records(path: &Path) -> Result<usize, GenericError> {
    let mut record_count = 0;
    dogstatsd_contexts::for_each_record(path, |_| record_count += 1)?;
    Ok(record_count)
}
