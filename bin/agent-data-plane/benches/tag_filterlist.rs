use std::collections::HashSet as StdHashSet;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use foldhash::fast::RandomState as FoldHashState;
use hashbrown::HashSet as HbHashSet;
use trie_hard::TrieHard;

const SMALL_TAGS: &[&str] = &["env", "host", "service", "region", "version"];

const MEDIUM_TAGS: &[&str] = &[
    "env",
    "host",
    "service",
    "region",
    "version",
    "availability_zone",
    "instance_type",
    "cluster",
    "namespace",
    "pod_name",
];

const LARGE_TAGS: &[&str] = &[
    "env",
    "host",
    "service",
    "region",
    "version",
    "availability_zone",
    "instance_type",
    "cluster",
    "namespace",
    "pod_name",
    "container_name",
    "image_tag",
    "team",
    "cost_center",
    "deployment",
    "shard",
    "replica",
    "datacenter",
    "cloud_provider",
    "account_id",
];

const TAG_SETS: &[(&str, &[&str])] = &[("5", SMALL_TAGS), ("10", MEDIUM_TAGS), ("20", LARGE_TAGS)];

// A key present in all sets.
const HIT_KEY: &str = "env";
// A key not present in any set.
const MISS_KEY: &str = "nonexistent_tag_name";

// Realistic metric tags for the full-scan benchmark.
// 20 tags, of which 5 keys match the EXCLUDE_FILTER_NAMES (~75% miss rate).
const METRIC_TAGS: &[&str] = &[
    "env:prod",
    "host:i-abc123def456",
    "service:web-frontend",
    "region:us-east-1",
    "version:3.14.1",
    "availability_zone:us-east-1a",
    "instance_type:c5.2xlarge",
    "cluster:main-prod",
    "namespace:default",
    "pod_name:web-frontend-6f8b9c7d4-x2k9m",
    "container_name:app",
    "image_tag:sha-a1b2c3d",
    "team:platform",
    "cost_center:eng-1234",
    "deployment:canary",
    "shard:03",
    "replica:2",
    "datacenter:dc1",
    "cloud_provider:aws",
    "account_id:123456789012",
];

// 5 tag names to exclude — matches env, host, region, availability_zone, instance_type.
const EXCLUDE_FILTER_NAMES: &[&str] = &["env", "host", "region", "availability_zone", "instance_type"];

fn build_std_hashset(names: &[&str]) -> StdHashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn build_hb_hashset(names: &[&str]) -> HbHashSet<String, FoldHashState> {
    let mut set = HbHashSet::with_capacity_and_hasher(names.len(), FoldHashState::default());
    set.extend(names.iter().map(|s| s.to_string()));
    set
}

fn build_trie(names: &[&'static str]) -> TrieHard<'static, ()> {
    let values: Vec<(&'static [u8], ())> = names.iter().map(|s| (s.as_bytes(), ())).collect();
    TrieHard::new(values)
}

/// Extract the tag name (part before ':') from a "key:value" tag string.
fn tag_name(tag: &str) -> &str {
    tag.split_once(':').map_or(tag, |(name, _)| name)
}

fn bench_lookup_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("should_keep_tag/hit");

    for &(label, tags) in TAG_SETS {
        let std_set = build_std_hashset(tags);
        let hb_set = build_hb_hashset(tags);
        let trie = build_trie(tags);

        group.bench_with_input(BenchmarkId::new("std_hashset", label), &std_set, |b, set| {
            b.iter(|| set.contains(HIT_KEY));
        });

        group.bench_with_input(BenchmarkId::new("hb_hashset", label), &hb_set, |b, set| {
            b.iter(|| set.contains(HIT_KEY));
        });

        group.bench_with_input(BenchmarkId::new("trie", label), &trie, |b, trie| {
            b.iter(|| trie.get(HIT_KEY).is_some());
        });
    }

    group.finish();
}

fn bench_lookup_miss(c: &mut Criterion) {
    let mut group = c.benchmark_group("should_keep_tag/miss");

    for &(label, tags) in TAG_SETS {
        let std_set = build_std_hashset(tags);
        let hb_set = build_hb_hashset(tags);
        let trie = build_trie(tags);

        group.bench_with_input(BenchmarkId::new("std_hashset", label), &std_set, |b, set| {
            b.iter(|| set.contains(MISS_KEY));
        });

        group.bench_with_input(BenchmarkId::new("hb_hashset", label), &hb_set, |b, set| {
            b.iter(|| set.contains(MISS_KEY));
        });

        group.bench_with_input(BenchmarkId::new("trie", label), &trie, |b, trie| {
            b.iter(|| trie.get(MISS_KEY).is_some());
        });
    }

    group.finish();
}

fn bench_full_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("should_keep_tag/full_scan");

    // Pre-extract tag names so we only measure the set membership check.
    let metric_tag_names: Vec<&str> = METRIC_TAGS.iter().map(|t| tag_name(t)).collect();

    let std_set = build_std_hashset(EXCLUDE_FILTER_NAMES);
    let hb_set = build_hb_hashset(EXCLUDE_FILTER_NAMES);
    let trie = build_trie(EXCLUDE_FILTER_NAMES);

    group.bench_function("std_hashset", |b| {
        b.iter(|| {
            let mut kept = 0u32;
            for name in &metric_tag_names {
                if !std_set.contains(*name) {
                    kept += 1;
                }
            }
            kept
        });
    });

    group.bench_function("hb_hashset", |b| {
        b.iter(|| {
            let mut kept = 0u32;
            for name in &metric_tag_names {
                if !hb_set.contains(*name) {
                    kept += 1;
                }
            }
            kept
        });
    });

    group.bench_function("trie", |b| {
        b.iter(|| {
            let mut kept = 0u32;
            for name in &metric_tag_names {
                if trie.get(name).is_none() {
                    kept += 1;
                }
            }
            kept
        });
    });

    group.finish();
}

criterion_group!(benches, bench_lookup_hit, bench_lookup_miss, bench_full_scan);
criterion_main!(benches);
