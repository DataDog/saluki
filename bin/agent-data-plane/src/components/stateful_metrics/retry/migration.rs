//! Restartable repartitioning of persisted logical retries before any worker opens its queues.

use std::{num::NonZeroUsize, path::Path};

use chrono::DateTime;
use foldspace_core::LogicalMetricBatch;
use saluki_common::hash::hash_single_stable;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::util::retry::persisted_retry_file_timestamp;
use tokio::{fs, io::AsyncWriteExt as _};
use tracing::info;

use super::{super::sharding::series_hash, RetryBatch, LANE_INFIX};

const STAGED: &str = "staged";
const ORIGINAL: &str = "original";
const READY: &str = "ready";
const ARCHIVED: &str = "archived";

/// Recognizes only worker directories belonging to this destination, excluding migration state and manifests.
fn worker_lane<'a>(name: &'a str, prefix: &str) -> Option<Option<&'a str>> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let (worker, lane) = match rest.split_once(LANE_INFIX) {
        Some((worker, lane)) => (worker, Some(lane)),
        None => (rest, None),
    };
    worker.parse::<usize>().ok()?;
    Some(lane)
}

async fn sync_directory(path: &Path) -> Result<(), GenericError> {
    fs::File::open(path).await?.sync_all().await?;
    Ok(())
}

async fn create_directory(path: &Path) -> Result<(), GenericError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).await?;
    Ok(())
}

async fn write_synced(path: &Path, contents: &[u8]) -> Result<(), GenericError> {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(path).await?;
    file.write_all(contents).await?;
    file.sync_all().await?;
    Ok(())
}

/// Atomically publishes the worker count after the replacement queues are durable.
pub(super) async fn write_manifest(root: &Path, prefix: &str, workers: NonZeroUsize) -> Result<(), GenericError> {
    let manifest = root.join(format!("{prefix}-workers"));
    let temporary = manifest.with_extension("tmp");
    let mut file = fs::File::create(&temporary).await?;
    file.write_all(workers.to_string().as_bytes()).await?;
    file.sync_all().await?;
    fs::rename(temporary, manifest).await?;
    sync_directory(root).await
}

/// Completes a prepared migration, or discards an incomplete staging attempt whose sources are untouched.
///
/// Recovery precedes reading the worker manifest: an interrupted migration must finish even if the operator changes
/// the requested count again. No worker can observe both the source and destination queues.
pub(super) async fn recover(root: &Path, prefix: &str) -> Result<(), GenericError> {
    let migration = root.join(format!("{prefix}-migration"));
    let garbage = root.join(format!("{prefix}-migration-garbage"));
    if fs::try_exists(&garbage).await? {
        fs::remove_dir_all(&garbage).await?;
    }
    if !fs::try_exists(&migration).await? {
        return Ok(());
    }
    let ready = migration.join(READY);
    if !fs::try_exists(&ready).await? {
        fs::remove_dir_all(migration).await?;
        return Ok(());
    }
    let workers: NonZeroUsize = fs::read_to_string(ready)
        .await?
        .parse()
        .error_context("Invalid stateful metrics retry migration worker count")?;
    let original = migration.join(ORIGINAL);
    let staged = migration.join(STAGED);
    let archived = migration.join(ARCHIVED);
    if !fs::try_exists(&archived).await? {
        let mut entries = fs::read_dir(root).await?;
        while let Some(entry) = entries.next_entry().await? {
            if worker_lane(&entry.file_name().to_string_lossy(), prefix).is_some() && entry.file_type().await?.is_dir()
            {
                fs::rename(entry.path(), original.join(entry.file_name())).await?;
            }
        }
        sync_directory(&original).await?;
        sync_directory(root).await?;
        // An empty marker is enough: existence means every original directory has been archived.
        write_synced(&archived, b"").await?;
        sync_directory(&migration).await?;
    }
    let mut entries = fs::read_dir(&staged).await?;
    while let Some(entry) = entries.next_entry().await? {
        let destination = root.join(entry.file_name());
        if fs::try_exists(&destination).await? {
            return Err(generic_error!(
                "Retry migration destination '{}' already exists",
                destination.display()
            ));
        }
        fs::rename(entry.path(), destination).await?;
    }
    sync_directory(&staged).await?;
    sync_directory(root).await?;
    write_manifest(root, prefix, workers).await?;
    // Remove the journal atomically before recursive cleanup, which itself may be interrupted.
    fs::rename(&migration, &garbage).await?;
    sync_directory(root).await?;
    fs::remove_dir_all(garbage).await?;
    Ok(())
}

/// Copies one logical batch at a time, preserving age and endpoint lane, then publishes the complete new layout.
///
/// Staging uses additional disk space. Read, serialization, or write failures leave the original queues intact.
/// Queue age and capacity policies run normally when the workers open the installed queues.
pub(super) async fn migrate(root: &Path, prefix: &str, workers: NonZeroUsize) -> Result<(), GenericError> {
    stage(root, prefix, workers).await.with_error_context(|| {
        format!(
            "Failed to stage stateful metrics retry migration in '{}'; original queues remain intact",
            root.display()
        )
    })?;
    recover(root, prefix).await?;
    info!(
        workers = workers.get(),
        "Redistributed persisted stateful metrics retries across sender workers."
    );
    Ok(())
}

async fn stage(root: &Path, prefix: &str, workers: NonZeroUsize) -> Result<(), GenericError> {
    let migration = root.join(format!("{prefix}-migration"));
    let staged = migration.join(STAGED);
    create_directory(&staged).await?;
    create_directory(&migration.join(ORIGINAL)).await?;
    let mut queues = fs::read_dir(root).await?;
    while let Some(queue) = queues.next_entry().await? {
        let name = queue.file_name();
        let name = name.to_string_lossy();
        let Some(lane) = worker_lane(&name, prefix) else {
            continue;
        };
        if !queue.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = fs::read_dir(queue.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let Some(timestamp) = persisted_retry_file_timestamp(&entry.path()) else {
                continue;
            };
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let serialized = fs::read(entry.path()).await?;
            let batch: RetryBatch = serde_json::from_slice(&serialized)
                .with_error_context(|| format!("Cannot migrate persisted retry '{}'", entry.path().display()))?;
            drop(serialized);
            let mut shards: Vec<Vec<_>> = (0..workers.get()).map(|_| Vec::new()).collect();
            for series in batch.0.into_series() {
                shards[(series_hash(&series) % workers.get() as u64) as usize].push(series);
            }
            // Preserve the timestamp, while distinguishing files originating in different worker/lane directories.
            let timestamp =
                DateTime::from_timestamp((timestamp / 1_000_000_000) as i64, (timestamp % 1_000_000_000) as u32)
                    .ok_or_else(|| generic_error!("Invalid retry timestamp"))?;
            let nonce = hash_single_stable((&*name, entry.file_name()));
            let filename = format!("retry-{}-{nonce}.json", timestamp.format("%Y%m%d%H%M%S%f"));
            for (worker, series) in shards.into_iter().enumerate() {
                if series.is_empty() {
                    continue;
                }
                let queue_name = match lane {
                    Some(lane) => format!("{prefix}-{worker}{LANE_INFIX}{lane}"),
                    None => format!("{prefix}-{worker}"),
                };
                let destination = staged.join(queue_name);
                create_directory(&destination).await?;
                let serialized = serde_json::to_vec(&RetryBatch(LogicalMetricBatch::new(series), 0))?;
                write_synced(&destination.join(&filename), &serialized).await?;
                sync_directory(&destination).await?;
            }
        }
    }
    sync_directory(&staged).await?;
    sync_directory(&migration).await?;
    sync_directory(root).await?;
    let temporary = migration.join("ready.tmp");
    write_synced(&temporary, workers.to_string().as_bytes()).await?;
    fs::rename(temporary, migration.join(READY)).await?;
    sync_directory(&migration).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use foldspace_core::{LogicalMetricSeries, MetricPoint, MetricSeriesType};
    use tempfile::TempDir;

    use super::*;

    const PREFIX: &str = "stateful-metrics-v1-test";
    const FILENAME: &str = "retry-20250101000000000000000-100000000.json";

    async fn seed(root: &Path) -> Vec<String> {
        let mut expected = Vec::new();
        for worker in 0..2 {
            for lane in ["", "-endpoint-abcdef"] {
                let queue = root.join(format!("{PREFIX}-{worker}{lane}"));
                create_directory(&queue).await.unwrap();
                let series = (0..8)
                    .map(|index| {
                        let name = format!("{worker}{lane}-{index}");
                        expected.push(name.clone());
                        LogicalMetricSeries::new(name, MetricSeriesType::Gauge, vec![MetricPoint::new(123, 4.5)])
                    })
                    .collect();
                let batch = RetryBatch(LogicalMetricBatch::new(series), 0);
                fs::write(queue.join(FILENAME), serde_json::to_vec(&batch).unwrap())
                    .await
                    .unwrap();
            }
        }
        expected.sort();
        expected
    }

    async fn assert_migrated(root: &Path, expected: &[String]) {
        let mut actual = Vec::new();
        let mut queues = fs::read_dir(root).await.unwrap();
        while let Some(queue) = queues.next_entry().await.unwrap() {
            let name = queue.file_name();
            let name = name.to_string_lossy();
            let Some(lane) = worker_lane(&name, PREFIX) else {
                continue;
            };
            let worker: usize = name
                .strip_prefix(&format!("{PREFIX}-"))
                .unwrap()
                .split('-')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert!(worker < 3);
            let mut entries = fs::read_dir(queue.path()).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                assert_eq!(
                    persisted_retry_file_timestamp(&entry.path()),
                    persisted_retry_file_timestamp(Path::new(FILENAME))
                );
                let batch: RetryBatch = serde_json::from_slice(&fs::read(entry.path()).await.unwrap()).unwrap();
                for series in batch.0.into_series() {
                    assert_eq!(series_hash(&series) % 3, worker as u64);
                    assert_eq!(series.name().contains(LANE_INFIX), lane.is_some());
                    assert_eq!(series.points(), &[MetricPoint::new(123, 4.5)]);
                    actual.push(series.name().to_owned());
                }
            }
        }
        actual.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            fs::read_to_string(root.join(format!("{PREFIX}-workers")))
                .await
                .unwrap(),
            "3"
        );
    }

    #[tokio::test]
    async fn recovery_at_every_installation_phase_preserves_each_retry_once() {
        for phase in 0..7 {
            let dir = TempDir::new().unwrap();
            let root = dir.path();
            let expected = seed(root).await;
            let workers = NonZeroUsize::new(3).unwrap();
            stage(root, PREFIX, workers).await.unwrap();
            let migration = root.join(format!("{PREFIX}-migration"));
            let original = migration.join(ORIGINAL);
            if phase == 0 {
                // Interrupted staging has not made any source queue inaccessible.
                fs::remove_file(migration.join(READY)).await.unwrap();
                recover(root, PREFIX).await.unwrap();
                migrate(root, PREFIX, workers).await.unwrap();
            } else {
                if phase >= 2 {
                    fs::rename(root.join(format!("{PREFIX}-0")), original.join(format!("{PREFIX}-0")))
                        .await
                        .unwrap();
                }
                if phase >= 3 {
                    let mut queues = fs::read_dir(root).await.unwrap();
                    while let Some(queue) = queues.next_entry().await.unwrap() {
                        if worker_lane(&queue.file_name().to_string_lossy(), PREFIX).is_some() {
                            fs::rename(queue.path(), original.join(queue.file_name()))
                                .await
                                .unwrap();
                        }
                    }
                    write_synced(&migration.join(ARCHIVED), b"").await.unwrap();
                }
                if phase >= 4 {
                    let mut queues = fs::read_dir(migration.join(STAGED)).await.unwrap();
                    let queue = queues.next_entry().await.unwrap().unwrap();
                    fs::rename(queue.path(), root.join(queue.file_name())).await.unwrap();
                }
                if phase >= 5 {
                    let mut queues = fs::read_dir(migration.join(STAGED)).await.unwrap();
                    while let Some(queue) = queues.next_entry().await.unwrap() {
                        fs::rename(queue.path(), root.join(queue.file_name())).await.unwrap();
                    }
                    write_manifest(root, PREFIX, workers).await.unwrap();
                }
                if phase == 6 {
                    fs::rename(&migration, root.join(format!("{PREFIX}-migration-garbage")))
                        .await
                        .unwrap();
                }
                recover(root, PREFIX).await.unwrap();
            }
            assert_migrated(root, &expected).await;
            // Repeated startups do not expose the archived originals again.
            recover(root, PREFIX).await.unwrap();
            assert_migrated(root, &expected).await;
        }
    }

    #[tokio::test]
    async fn staging_decode_and_write_failures_leave_originals_available() {
        for corrupt in [true, false] {
            let dir = TempDir::new().unwrap();
            let root = dir.path();
            let expected = seed(root).await;
            let source = root.join(format!("{PREFIX}-0")).join(FILENAME);
            let contents = fs::read(&source).await.unwrap();
            if corrupt {
                fs::write(&source, b"corrupt").await.unwrap();
            } else {
                let staged = root.join(format!("{PREFIX}-migration")).join(STAGED);
                create_directory(&staged).await.unwrap();
                // A file in place of each destination directory makes staging writes fail deterministically.
                for worker in 0..3 {
                    for lane in ["", "-endpoint-abcdef"] {
                        fs::write(staged.join(format!("{PREFIX}-{worker}{lane}")), b"occupied")
                            .await
                            .unwrap();
                    }
                }
            }
            let workers = NonZeroUsize::new(3).unwrap();
            assert!(stage(root, PREFIX, workers).await.is_err());
            for worker in 0..2 {
                for lane in ["", "-endpoint-abcdef"] {
                    assert!(
                        fs::try_exists(root.join(format!("{PREFIX}-{worker}{lane}")).join(FILENAME))
                            .await
                            .unwrap()
                    );
                }
            }
            recover(root, PREFIX).await.unwrap();
            fs::write(source, contents).await.unwrap();
            migrate(root, PREFIX, workers).await.unwrap();
            assert_migrated(root, &expected).await;
        }
    }
}
