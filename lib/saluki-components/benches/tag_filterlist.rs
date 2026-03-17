use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use saluki_components::transforms::tag_filterlist::{
    compile_filters, filter_metric_tags, CompiledFilters, FilterAction, MetricTagFilterEntry,
};
use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use saluki_core::data_model::event::metric::Metric;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tags(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("tag{i}:val{i}")).collect()
}

fn make_tags_static(n: usize) -> Vec<&'static str> {
    // Leak allocations so we can hand &'static str to Context::from_static_parts.
    // Benchmarks run for a bounded time so the leak is acceptable.
    (0..n)
        .map(|i| Box::leak(format!("tag{i}:val{i}").into_boxed_str()) as &'static str)
        .collect()
}

fn make_origin_tags_static(n: usize) -> Vec<&'static str> {
    (0..n)
        .map(|i| Box::leak(format!("orig_tag{i}:val{i}").into_boxed_str()) as &'static str)
        .collect()
}

fn distribution_metric(name: &'static str, tags: &[&'static str]) -> Metric {
    Metric::distribution(Context::from_static_parts(name, tags), 1.0)
}

fn distribution_metric_with_origin_tags(
    name: &'static str, tags: &[&'static str], origin_tags: &[&'static str],
) -> Metric {
    let origin_tag_set: TagSet = origin_tags.iter().map(|s| Tag::from(*s)).collect();
    let context = Context::from_static_parts(name, tags).with_origin_tags(origin_tag_set.into_shared());
    Metric::distribution(context, 1.0)
}

fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
    Metric::counter(Context::from_static_parts(name, tags), 1.0)
}

fn exclude_filter(metric_name: &str, tag_keys: &[&str]) -> Vec<MetricTagFilterEntry> {
    vec![MetricTagFilterEntry {
        metric_name: metric_name.to_string(),
        action: FilterAction::Exclude,
        tags: tag_keys.iter().map(|s| s.to_string()).collect(),
    }]
}

fn include_filter(metric_name: &str, tag_keys: &[&str]) -> Vec<MetricTagFilterEntry> {
    vec![MetricTagFilterEntry {
        metric_name: metric_name.to_string(),
        action: FilterAction::Include,
        tags: tag_keys.iter().map(|s| s.to_string()).collect(),
    }]
}

/// Build tag key names by index, matching the `tag{i}:val{i}` pattern above.
fn tag_keys(indices: &[usize]) -> Vec<String> {
    indices.iter().map(|i| format!("tag{i}")).collect()
}

// ---------------------------------------------------------------------------
// Benchmark: exclude with varying tag-set sizes
// ---------------------------------------------------------------------------

fn bench_exclude(c: &mut Criterion) {
    let cases: &[(&str, usize, &[usize])] = &[
        // at most half of configured keys match the metric's tags (realistic: over-specified configs)
        // tag keys ≥ n do not exist on the metric
        ("10tags_exclude5", 10, &[0, 2, 10, 12, 14]), // 2/5  match
        (
            "10tags_exclude50",
            10,
            &[
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, // 10/50 match
                10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,
                36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49,
            ],
        ),
        ("50tags_exclude5", 50, &[0, 10, 50, 60, 70]), // 2/5  match
        (
            "50tags_exclude50",
            50,
            &[
                0, 2, 4, 6, 8, 10, 12, 14, 16, 18, // 25/50 match (even 0-48 ∩ metric)
                20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64, 66,
                68, // these don't exist on metric
                70, 72, 74, 76, 78, 80, 82, 84, 86, 88, 90, 92, 94, 96, 98,
            ],
        ),
        ("100tags_exclude5", 100, &[0, 50, 100, 110, 120]), // 2/5  match
        (
            "100tags_exclude50",
            100,
            &[
                0, 4, 8, 12, 16, 20, 24, 28, 32, 36, // 25/50 match (every 4th 0-96)
                40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80, 84, 88, 92, 96, 100, 104, 108, 112, 116, 120, 124, 128,
                132, 136, // don't exist
                140, 144, 148, 152, 156, 160, 164, 168, 172, 176, 180, 184, 188, 192, 196,
            ],
        ),
    ];

    let mut group = c.benchmark_group("tag_filterlist/exclude");
    for (label, n_tags, excluded_indices) in cases {
        let tags_static = make_tags_static(*n_tags);
        let excluded_keys: Vec<String> = tag_keys(excluded_indices);
        let excluded_key_refs: Vec<&str> = excluded_keys.iter().map(|s| s.as_str()).collect();
        let filters: CompiledFilters = compile_filters(&exclude_filter("bench.dist", &excluded_key_refs));

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("", label), |b| {
            b.iter_batched(
                || distribution_metric("bench.dist", &tags_static),
                |mut metric| {
                    filter_metric_tags(&mut metric, &filters);
                    metric
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: include with varying tag-set sizes
// ---------------------------------------------------------------------------

fn bench_include(c: &mut Criterion) {
    let cases: &[(&str, usize, &[usize])] = &[
        ("10tags_include2", 10, &[2, 7]),
        ("10tags_include5", 10, &[0, 2, 4, 6, 8]),
        ("100tags_include2", 100, &[20, 80]),
        ("100tags_include5", 100, &[10, 30, 50, 70, 90]),
    ];

    let mut group = c.benchmark_group("tag_filterlist/include");
    for (label, n_tags, included_indices) in cases {
        let tags_static = make_tags_static(*n_tags);
        let included_keys: Vec<String> = tag_keys(included_indices);
        let included_key_refs: Vec<&str> = included_keys.iter().map(|s| s.as_str()).collect();
        let filters: CompiledFilters = compile_filters(&include_filter("bench.dist", &included_key_refs));

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("", label), |b| {
            b.iter_batched(
                || distribution_metric("bench.dist", &tags_static),
                |mut metric| {
                    filter_metric_tags(&mut metric, &filters);
                    metric
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: fast-path — metric name not in filterlist
// ---------------------------------------------------------------------------

fn bench_no_match_passthrough(c: &mut Criterion) {
    let tags_static = make_tags_static(20);
    // Filter is for a different metric name.
    let entries = exclude_filter("other.metric", &["tag0", "tag5"]);
    let filters = compile_filters(&entries);

    let mut group = c.benchmark_group("tag_filterlist/passthrough");
    group.throughput(Throughput::Elements(1));
    group.bench_function("no_match", |b| {
        b.iter_batched(
            || distribution_metric("bench.dist", &tags_static),
            |mut metric| {
                filter_metric_tags(&mut metric, &filters);
                metric
            },
            BatchSize::SmallInput,
        );
    });

    // Non-distribution (counter) — type check is done before calling filter_metric_tags;
    // benchmark a simulated path that checks is_sketch() and skips.
    let counter_tags = make_tags_static(20);
    group.bench_function("non_distribution", |b| {
        b.iter_batched(
            || counter_metric("bench.dist", &counter_tags),
            |metric| {
                // Simulate the guard in Transform::run: only call filter_metric_tags for sketches.
                if metric.values().is_sketch() {
                    unreachable!("counter is not a sketch");
                }
                metric
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: filterlist table size (hash lookup cost)
// ---------------------------------------------------------------------------

fn bench_filterlist_size(c: &mut Criterion) {
    let tags_static = make_tags_static(10);
    let excluded_keys = ["tag0", "tag5"];

    // Build filterlist with N entries; the matching entry is always last.
    let sizes = [1usize, 10, 100];

    let mut group = c.benchmark_group("tag_filterlist/filterlist_size");
    for &n in &sizes {
        group.throughput(Throughput::Elements(1));

        // Build entries: n-1 non-matching entries + 1 matching entry.
        let entries: Vec<MetricTagFilterEntry> = (0..n - 1)
            .map(|i| MetricTagFilterEntry {
                metric_name: format!("other.metric.{i}"),
                action: FilterAction::Exclude,
                tags: vec!["tag0".to_string()],
            })
            .chain(std::iter::once(MetricTagFilterEntry {
                metric_name: "bench.dist".to_string(),
                action: FilterAction::Exclude,
                tags: excluded_keys.iter().map(|s| s.to_string()).collect(),
            }))
            .collect();

        let filters = compile_filters(&entries);

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter_batched(
                || distribution_metric("bench.dist", &tags_static),
                |mut metric| {
                    filter_metric_tags(&mut metric, &filters);
                    metric
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: compile_filters itself (cold-path: RC update)
// ---------------------------------------------------------------------------

fn bench_compile_filters(c: &mut Criterion) {
    let mut group = c.benchmark_group("tag_filterlist/compile");

    let sizes = [1usize, 10, 100];
    for &n in &sizes {
        let entries: Vec<MetricTagFilterEntry> = (0..n)
            .map(|i| MetricTagFilterEntry {
                metric_name: format!("metric.{i}"),
                action: FilterAction::Exclude,
                tags: make_tags(5)
                    .into_iter()
                    .map(|s| s.split(':').next().unwrap().to_string())
                    .collect(),
            })
            .collect();

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, entries| {
            b.iter(|| compile_filters(entries));
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: origin_tags exclude with varying origin tag-set sizes
// ---------------------------------------------------------------------------

fn bench_origin_tags_exclude(c: &mut Criterion) {
    let sizes = [10usize, 50, 100];
    let tags_static = make_tags_static(5);

    let mut group = c.benchmark_group("tag_filterlist/origin_tags_exclude");
    for &n in &sizes {
        let origin_tags_static = make_origin_tags_static(n);
        // Exclude the first third of orig_tag keys.
        let excluded: Vec<String> = (0..n / 3).map(|i| format!("orig_tag{i}")).collect();
        let excluded_refs: Vec<&str> = excluded.iter().map(|s| s.as_str()).collect();
        let filters = compile_filters(&exclude_filter("bench.dist", &excluded_refs));

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter_batched(
                || distribution_metric_with_origin_tags("bench.dist", &tags_static, &origin_tags_static),
                |mut metric| {
                    filter_metric_tags(&mut metric, &filters);
                    metric
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: origin_tags include with varying origin tag-set sizes
// ---------------------------------------------------------------------------

fn bench_origin_tags_include(c: &mut Criterion) {
    let sizes = [10usize, 50, 100];
    let tags_static = make_tags_static(5);

    let mut group = c.benchmark_group("tag_filterlist/origin_tags_include");
    for &n in &sizes {
        let origin_tags_static = make_origin_tags_static(n);
        // Keep only the first third of orig_tag keys.
        let included: Vec<String> = (0..n / 3).map(|i| format!("orig_tag{i}")).collect();
        let included_refs: Vec<&str> = included.iter().map(|s| s.as_str()).collect();
        let filters = compile_filters(&include_filter("bench.dist", &included_refs));

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter_batched(
                || distribution_metric_with_origin_tags("bench.dist", &tags_static, &origin_tags_static),
                |mut metric| {
                    filter_metric_tags(&mut metric, &filters);
                    metric
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: origin_tags passthrough (fast path — no origin_tags on metric)
// ---------------------------------------------------------------------------

fn bench_origin_tags_passthrough(c: &mut Criterion) {
    let tags_static = make_tags_static(20);
    let filters = compile_filters(&exclude_filter("bench.dist", &["tag0", "tag5"]));

    let mut group = c.benchmark_group("tag_filterlist/origin_tags_passthrough");
    group.throughput(Throughput::Elements(1));
    group.bench_function("no_origin_tags", |b| {
        b.iter_batched(
            || distribution_metric("bench.dist", &tags_static),
            |mut metric| {
                // No origin_tags: exercises the fast path in filter_metric_tags.
                filter_metric_tags(&mut metric, &filters);
                metric
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: combined instrumented + origin_tags filtering (single alloc path)
// ---------------------------------------------------------------------------

fn bench_combined(c: &mut Criterion) {
    let tags_static = make_tags_static(50);
    let origin_tags_static = make_origin_tags_static(20);

    // Exclude a mix of instrumented tag keys and origin tag keys.
    let excluded_keys: Vec<String> = (0..10)
        .map(|i| format!("tag{i}"))
        .chain((0..5).map(|i| format!("orig_tag{i}")))
        .collect();
    let excluded_refs: Vec<&str> = excluded_keys.iter().map(|s| s.as_str()).collect();
    let filters = compile_filters(&exclude_filter("bench.dist", &excluded_refs));

    let mut group = c.benchmark_group("tag_filterlist/combined");
    group.throughput(Throughput::Elements(1));
    group.bench_function("50tags_20origin", |b| {
        b.iter_batched(
            || distribution_metric_with_origin_tags("bench.dist", &tags_static, &origin_tags_static),
            |mut metric| {
                filter_metric_tags(&mut metric, &filters);
                metric
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_exclude,
    bench_include,
    bench_no_match_passthrough,
    bench_filterlist_size,
    bench_compile_filters,
    bench_origin_tags_exclude,
    bench_origin_tags_include,
    bench_origin_tags_passthrough,
    bench_combined,
);
criterion_main!(benches);
