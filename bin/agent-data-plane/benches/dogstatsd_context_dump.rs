use std::fs;
use std::hint::black_box;
use std::path::Path;

use agent_data_plane::{count_dogstatsd_context_dump_records, publish_dogstatsd_context_dump};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use stringtheory::MetaString;

const CONTEXT_COUNTS: [usize; 3] = [10_000, 100_000, 1_000_000];
const CLIENT_TAGS: [&str; 5] = [
    "env:production",
    "service:payments-api",
    "version:2025.03.0",
    "region:us-east-1",
    "availability-zone:us-east-1a",
];
const ORIGIN_TAGS: [&str; 5] = [
    "container_id:7c9f0e2a4b1d",
    "pod_name:payments-api-7d8f9c6b5f-k2m4p",
    "namespace:production",
    "kube_deployment:payments-api",
    "image_name:registry.example/payments-api",
];

fn benchmark_dogstatsd_context_dump(c: &mut Criterion) {
    let mut group = c.benchmark_group("dogstatsd_context_dump");

    for context_count in CONTEXT_COUNTS {
        let snapshot = build_snapshot(context_count);
        assert_eq!(snapshot.len(), context_count);

        let run_directory = tempfile::tempdir().expect("benchmark run directory should be created");
        let canonical_path = publish_dogstatsd_context_dump(run_directory.path(), &snapshot)
            .expect("benchmark preflight context dump should publish");
        assert!(canonical_path.is_file());

        let decoded_records = count_dogstatsd_context_dump_records(&canonical_path)
            .expect("benchmark preflight context dump should decode through the production reader");
        assert_eq!(decoded_records, context_count);
        assert_only_canonical_artifact(run_directory.path(), &canonical_path);

        let compressed_bytes = fs::metadata(&canonical_path)
            .expect("benchmark preflight artifact metadata should be available")
            .len();
        eprintln!("dogstatsd_context_dump setup: contexts={context_count}, compressed_bytes={compressed_bytes}");

        let benchmark_id = BenchmarkId::from_parameter(context_count);
        group.throughput(Throughput::Elements(context_count as u64));
        group.bench_function(benchmark_id, |b| {
            b.iter(|| {
                black_box(
                    publish_dogstatsd_context_dump(run_directory.path(), &snapshot)
                        .expect("benchmark context dump should publish"),
                )
            });
        });

        let postflight_path = publish_dogstatsd_context_dump(run_directory.path(), &snapshot)
            .expect("benchmark postflight context dump should publish");
        assert_eq!(postflight_path, canonical_path);
        assert!(postflight_path.is_file());
        assert_only_canonical_artifact(run_directory.path(), &canonical_path);
    }

    group.finish();
}

fn build_snapshot(context_count: usize) -> Vec<AggregateContextSnapshotEntry> {
    let base_context = Context::from_parts("dogstatsd.benchmark.metric", shared_tag_set(&CLIENT_TAGS))
        .with_host(Some(MetaString::from_static("benchmark-node-001.internal")))
        .with_origin_tags(shared_tag_set(&ORIGIN_TAGS));

    (0..context_count)
        .map(|index| {
            let context = base_context.with_name(format!("dogstatsd.benchmark.metric.{index:07}"));
            let (metric_type, unit) = match index % 3 {
                0 => (AggregateMetricType::Counter, MetaString::empty()),
                1 => (AggregateMetricType::Gauge, MetaString::empty()),
                _ => (AggregateMetricType::Histogram, MetaString::from_static("millisecond")),
            };
            AggregateContextSnapshotEntry::for_test(context, metric_type, unit)
        })
        .collect()
}

fn shared_tag_set(values: &[&'static str]) -> TagSet {
    let mut tags = TagSet::with_capacity(values.len());
    for value in values {
        tags.insert_tag(Tag::from_static(value));
    }
    tags.into_shared().to_mutable()
}

fn assert_only_canonical_artifact(run_path: &Path, canonical_path: &Path) {
    let mut entries: Vec<_> = fs::read_dir(run_path)
        .expect("benchmark run directory should be readable")
        .map(|entry| entry.expect("benchmark run directory entry should be readable").path())
        .collect();
    entries.sort_unstable();
    assert_eq!(entries, [canonical_path.to_path_buf()]);
}

criterion_group!(benches, benchmark_dogstatsd_context_dump);
criterion_main!(benches);
