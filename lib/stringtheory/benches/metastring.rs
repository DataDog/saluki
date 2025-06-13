use criterion::{criterion_group, criterion_main, Criterion};
use stringtheory::MetaString;

fn deref_str(s: &str) -> &str {
    std::hint::black_box(s)
}

fn bench_deref(c: &mut Criterion) {
    let mut group = c.benchmark_group("MetaString");
    group.bench_function("inline_deref", |b| {
        let ms_inline = MetaString::try_inline("hello world!").unwrap();
        b.iter(|| deref_str(&ms_inline));
    });
    group.bench_function("static_deref", |b| {
        let ms_static = MetaString::from_static("heee heee heeeeeeeeeeeeeeeeeeeeeeee");
        b.iter(|| deref_str(&ms_static));
    });
    group.finish();

    /*let mut group = c.benchmark_group("stdlib");
    group.bench_function("string_deref", |b| {
        let s = String::from("the weather outside is frightful");
        b.iter(|| deref_str(&s));
    });
    group.bench_function("arcstr_deref", |b| {
        let arc_str = std::sync::Arc::from("hello darkness my old friend");
        b.iter(|| deref_str(&arc_str));
    });
    group.finish();*/
}

criterion_group!(benches, bench_deref);
criterion_main!(benches);
