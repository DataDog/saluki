use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saluki_context::tags::{SharedTagSet, Tag, TagSet};

/// Build a SharedTagSet with the given number of tags.
fn make_base(n: usize) -> SharedTagSet {
    let ts: TagSet = (0..n).map(|i| Tag::from(format!("key{}:value{}", i, i))).collect();
    ts.into_shared()
}

/// Build a SharedTagSet with two chained groups.
fn make_chained_base(n: usize) -> SharedTagSet {
    let half = n / 2;
    let ts1: TagSet = (0..half).map(|i| Tag::from(format!("key{}:value{}", i, i))).collect();
    let ts2: TagSet = (half..n).map(|i| Tag::from(format!("key{}:value{}", i, i))).collect();
    let mut shared = ts1.into_shared();
    shared.extend_from_shared(&ts2.into_shared());
    shared
}

fn bench_insert_fresh(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/insert_fresh");

    for &size in &sizes {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let mut ts = TagSet::with_capacity(size);
                for i in 0..size {
                    ts.insert_tag(Tag::from(format!("key{}:value{}", i, i)));
                }
                ts
            });
        });
    }

    group.finish();
}

fn bench_insert_over_base(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/insert_over_base");

    for &size in &sizes {
        let base = make_base(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let mut ts = TagSet::from(base.clone());
                // Insert a tag that shadows the middle tag in the base.
                ts.insert_tag(Tag::from(format!("key{}:new_value", size / 2)));
                ts
            });
        });
    }

    group.finish();
}

fn bench_remove_from_base(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/remove_from_base");

    for &size in &sizes {
        let base = make_base(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let mut ts = TagSet::from(base.clone());
                ts.remove_tags(format!("key{}", size / 2));
                ts
            });
        });
    }

    group.finish();
}

fn bench_remove_from_additions(c: &mut Criterion) {
    let mut group = c.benchmark_group("TagSet/remove_from_additions");

    for &size in &[10, 25, 50] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let mut ts = TagSet::with_capacity(size);
                for i in 0..size {
                    ts.insert_tag(Tag::from(format!("key{}:value{}", i, i)));
                }
                ts.remove_tags(format!("key{}", size / 2));
                ts
            });
        });
    }

    group.finish();
}

fn bench_get_single_tag_in_base(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/get_single_tag_in_base");

    for &size in &sizes {
        let base = make_base(size);
        let ts = TagSet::from(base);
        let key = format!("key{}", size / 2);

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| ts.get_single_tag(&key));
        });
    }

    group.finish();
}

fn bench_get_single_tag_in_additions(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/get_single_tag_in_additions");

    for &size in &sizes {
        let base = make_base(size);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("lookup_target:found"));

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| ts.get_single_tag("lookup_target"));
        });
    }

    group.finish();
}

fn bench_iterate_unmodified(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/iterate_unmodified");

    for &size in &sizes {
        let base = make_base(size);
        let ts = TagSet::from(base);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let mut count = 0u32;
                for _ in &ts {
                    count += 1;
                }
                count
            });
        });
    }

    group.finish();
}

fn bench_iterate_with_removals(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/iterate_with_removals");

    for &size in &sizes {
        let base = make_base(size);
        let mut ts = TagSet::from(base);
        // Remove ~20% of tags.
        for i in (0..size).step_by(5) {
            ts.remove_tags(format!("key{}", i));
        }

        group.throughput(Throughput::Elements(ts.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let mut count = 0u32;
                for _ in &ts {
                    count += 1;
                }
                count
            });
        });
    }

    group.finish();
}

fn bench_into_shared_unmodified(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/into_shared_unmodified");

    for &size in &sizes {
        let base = make_base(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let ts = TagSet::from(base.clone());
                ts.into_shared()
            });
        });
    }

    group.finish();
}

fn bench_into_shared_modified(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/into_shared_modified");

    for &size in &sizes {
        let base = make_base(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let mut ts = TagSet::from(base.clone());
                ts.insert_tag(Tag::from("extra:tag"));
                ts.remove_tags("key0");
                ts.into_shared()
            });
        });
    }

    group.finish();
}

fn bench_clone(c: &mut Criterion) {
    let sizes = [10, 25, 50];
    let mut group = c.benchmark_group("TagSet/clone");

    for &size in &sizes {
        let base = make_chained_base(size);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("extra:tag"));
        ts.remove_tags("key0");

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| ts.clone());
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_insert_fresh,
    bench_insert_over_base,
    bench_remove_from_base,
    bench_remove_from_additions,
    bench_get_single_tag_in_base,
    bench_get_single_tag_in_additions,
    bench_iterate_unmodified,
    bench_iterate_with_removals,
    bench_into_shared_unmodified,
    bench_into_shared_modified,
    bench_clone,
);
criterion_main!(benches);
