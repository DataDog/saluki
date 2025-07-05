use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use stringtheory::MetaString;

fn create_slice_sampled<T: Clone>(inputs: &[T], len: usize) -> Vec<T> {
    let mut xs = Vec::with_capacity(len);
    for i in 0..len {
        let input = inputs[i % inputs.len()].clone();
        xs.push(input);
    }
    xs
}

fn sum_string_lengths<T: AsRef<str>>(xs: &[T]) -> usize {
    let mut total = 0;
    for s in xs {
        total += s.as_ref().len();
    }
    total
}

fn bench_deref(c: &mut Criterion) {
    c.bench_function("MetaString/deref_empty", |b| {
        let input = MetaString::empty();
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });

    c.bench_function("MetaString/deref_inline", |b| {
        let input = MetaString::try_inline("patty cake patty cake").unwrap();
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });

    c.bench_function("MetaString/deref_owned", |b| {
        let input = MetaString::from(String::from("bakers man, bake me a cake as fast as you can"));
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });

    c.bench_function("MetaString/deref_static", |b| {
        let input = MetaString::from_static("are you there god? it's me, margaret");
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });

    c.bench_function("MetaString/deref_shared", |b| {
        let input = MetaString::from(Arc::from("hello, world!"));
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });

    c.bench_function("stdlib/deref_string", |b| {
        let input = String::from("basic stdlib string");
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });

    c.bench_function("stdlib/deref_arc", |b| {
        let input = Arc::from("basic stdlib string");
        let inputs = create_slice_sampled(&[input], 1000);

        b.iter(|| sum_string_lengths(&inputs));
    });
}

criterion_group!(benches, bench_deref);
criterion_main!(benches);
