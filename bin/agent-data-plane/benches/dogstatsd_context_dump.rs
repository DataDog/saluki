use std::fs;
use std::hint::black_box;
use std::path::Path;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use stringtheory::MetaString;

#[path = "../src/dogstatsd_contexts/artifact.rs"]
mod artifact;

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
        let canonical_path = run_directory.path().join(artifact::CONTEXT_DUMP_FILENAME);
        let preflight_path = artifact::publish_context_dump(run_directory.path(), &snapshot)
            .expect("benchmark preflight context dump should publish");
        assert_eq!(preflight_path, canonical_path);
        assert!(canonical_path.is_file());

        let mut decoded_records = 0usize;
        artifact::for_each_record(&canonical_path, |record| {
            assert!(!record.name.is_empty());
            assert_eq!(record.host, "benchmark-node-001.internal");
            assert!(matches!(record.metric_type.as_str(), "Counter" | "Gauge" | "Historate"));
            assert_eq!(record.tagger_tags.len(), ORIGIN_TAGS.len());
            assert_eq!(record.metric_tags.len(), CLIENT_TAGS.len());
            assert!(!record.no_index);
            assert_eq!(record.source, 1);
            decoded_records += 1;
        })
        .expect("benchmark preflight context dump should decode through the production reader");
        assert_eq!(decoded_records, context_count);
        assert_only_canonical_artifact(run_directory.path());

        let compressed_bytes = fs::metadata(&canonical_path)
            .expect("benchmark preflight artifact metadata should be available")
            .len();
        let benchmark_id =
            BenchmarkId::from_parameter(format!("{context_count}_contexts_{compressed_bytes}_compressed_bytes"));
        group.throughput(Throughput::Elements(context_count as u64));
        group.bench_function(benchmark_id, |b| {
            b.iter(|| {
                let published_path = artifact::publish_context_dump(run_directory.path(), &snapshot)
                    .expect("benchmark context dump should publish");
                assert_eq!(published_path, canonical_path);
                assert!(published_path.is_file());
                black_box(published_path)
            });
        });

        assert_only_canonical_artifact(run_directory.path());
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

fn assert_only_canonical_artifact(run_path: &Path) {
    let mut entries: Vec<_> = fs::read_dir(run_path)
        .expect("benchmark run directory should be readable")
        .map(|entry| {
            entry
                .expect("benchmark run directory entry should be readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    entries.sort_unstable();
    assert_eq!(entries, [artifact::CONTEXT_DUMP_FILENAME]);
}

criterion_group!(benches, benchmark_dogstatsd_context_dump);
criterion_main!(benches);
