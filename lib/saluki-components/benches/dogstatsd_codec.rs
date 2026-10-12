//! What the DogStatsD source's decoders do per metric: frame a datagram, parse each line, filter it, and resolve its
//! context, through the source's own decode path (`sources::dogstatsd_measurement::DatagramDecoder`, with the
//! `measurement` feature), with the source's default configuration.
//!
//! - **`decode`**: one decoder, for two traffic profiles and two context counts, with one metric per datagram and with
//!   full datagrams: the cost per metric.
//! - **`shared_resolvers`**: 1, 2, 4, and 8 threads, each decoding its own share of the same traffic through a clone of
//!   one decoder, so that every thread resolves on the shared context cache, tag cache, and interner, as the source's
//!   datagram decoders do: resolver throughput by threads. The time is from the release of every thread to the end
//!   of the last one, so a flat curve means the shared resolvers, not the threads, set the rate.
//!
//! The profiles approximate lading's DogStatsD generator, deterministically:
//!
//! - **`long_lines`**: long lines, modeled on settings often given to lading's DogStatsD generator (names of 1 to 200
//!   bytes, 2 to 50 tags of 3 to 150 bytes, counts, gauges, distributions, sets, and histograms weighted 208, 66, 72,
//!   9, and 1, and 8% of lines packing 2 to 32 values), so that a datagram holds a few;
//! - **`compact`**: names of 10 to 40 bytes and 2 to 8 tags of 5 to 30 bytes, with the same kinds, so that a full
//!   datagram holds about 50 lines.
//!
//! Decoded events are dropped outside the timed section; resolving the same contexts again in later iterations hits
//! the context cache, as steady traffic does.

use std::{
    hint::black_box,
    sync::Barrier,
    thread,
    time::{Duration, Instant},
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saluki_components::sources::dogstatsd_measurement::DatagramDecoder;

/// The largest datagram the source's default receive buffer takes, in bytes.
const DATAGRAM_BYTES: usize = 8_192;

/// The metrics decoded per iteration.
const METRICS: usize = 65_536;

/// A small deterministic generator, so that every run decodes the same traffic.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Returns a value in `min..=max`.
    fn between(&mut self, min: usize, max: usize) -> usize {
        min + (self.next() % (max - min + 1) as u64) as usize
    }

    fn ascii(&mut self, len: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
        (0..len)
            .map(|_| ALPHABET[(self.next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }
}

/// A traffic profile: the shape of a context and of the line that carries it.
#[derive(Clone, Copy)]
struct Profile {
    name: &'static str,
    name_len: (usize, usize),
    tags: (usize, usize),
    tag_len: (usize, usize),
}

const PROFILES: [Profile; 2] = [
    Profile {
        name: "long_lines",
        name_len: (1, 200),
        tags: (2, 50),
        tag_len: (3, 150),
    },
    Profile {
        name: "compact",
        name_len: (10, 40),
        tags: (2, 8),
        tag_len: (5, 30),
    },
];

/// Returns `contexts` distinct contexts of `profile`, each as the part of a line before its value: the name, and the
/// tags after `|#`, which the line's type and value surround.
fn contexts(profile: Profile, contexts: usize, rng: &mut XorShift) -> Vec<(String, String)> {
    (0..contexts)
        .map(|index| {
            let name_len = rng.between(profile.name_len.0, profile.name_len.1);
            let name = format!("{}.{}", rng.ascii(name_len), index);
            let tags = (0..rng.between(profile.tags.0, profile.tags.1))
                .map(|_| {
                    let len = rng.between(profile.tag_len.0, profile.tag_len.1).max(3);
                    let key = rng.ascii(len / 3);
                    let value = rng.ascii(len - len / 3 - 1);
                    format!("{}:{}", key, value)
                })
                .collect::<Vec<_>>()
                .join(",");
            (name, tags)
        })
        .collect()
}

/// Returns `count` lines over `contexts`, in rotation, with lading's kinds and multi-value packing.
fn lines(contexts: &[(String, String)], count: usize, rng: &mut XorShift) -> Vec<String> {
    (0..count)
        .map(|index| {
            let (name, tags) = &contexts[index % contexts.len()];
            let kind = match rng.next() % 356 {
                0..208 => "c",
                208..274 => "g",
                274..346 => "d",
                346..355 => "s",
                _ => "h",
            };
            let values = if rng.next() % 100 < 8 { rng.between(2, 32) } else { 1 };
            let values = (0..values)
                .map(|_| (rng.next() % 1_000).to_string())
                .collect::<Vec<_>>()
                .join(":");
            format!("{}:{}|{}|#{}", name, values, kind, tags)
        })
        .collect()
}

/// Packs `lines` into datagrams of at most `per_datagram` lines and [`DATAGRAM_BYTES`] bytes, and returns them with the
/// number of lines each holds.
fn datagrams(lines: &[String], per_datagram: usize) -> Vec<(Vec<u8>, usize)> {
    let mut datagrams = Vec::new();
    let mut current = Vec::new();
    let mut held = 0;
    for line in lines {
        let fits = current.len() + usize::from(!current.is_empty()) + line.len() <= DATAGRAM_BYTES;
        if held > 0 && (held == per_datagram || !fits) {
            datagrams.push((std::mem::take(&mut current), held));
            held = 0;
        }
        if !current.is_empty() {
            current.push(b'\n');
        }
        current.extend_from_slice(line.as_bytes());
        held += 1;
    }
    if held > 0 {
        datagrams.push((current, held));
    }
    datagrams
}

/// Decodes every datagram once with `decoder`, and returns the time it took. The events are dropped afterwards.
fn decode_all(
    decoder: &mut DatagramDecoder, datagrams: &[(Vec<u8>, usize)],
    events: &mut Vec<saluki_core::data_model::event::Event>,
) -> Duration {
    let started = Instant::now();
    for (datagram, _) in datagrams {
        decoder.decode(datagram, events);
    }
    let elapsed = started.elapsed();
    black_box(&events);
    events.clear();
    elapsed
}

/// Returns a runtime for the context resolvers' expiry tasks, which decoders are created in.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("the runtime builds")
}

fn decode(c: &mut Criterion) {
    let runtime = runtime();
    let _runtime = runtime.enter();
    let mut group = c.benchmark_group("dogstatsd_codec_decode");
    group.sample_size(20);
    for profile in PROFILES {
        for context_count in [3_000, 100_000] {
            let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
            let contexts = contexts(profile, context_count, &mut rng);
            let lines = lines(&contexts, METRICS.max(context_count), &mut rng);
            for per_datagram in [1, 50] {
                let datagrams = datagrams(&lines, per_datagram);
                let metrics = datagrams.iter().map(|(_, held)| held).sum::<usize>();
                let per = metrics as f64 / datagrams.len() as f64;
                let mut decoder = DatagramDecoder::with_defaults().expect("the default decoder builds");
                let mut events = Vec::with_capacity(metrics);
                // Warm the caches, so that every iteration measures steady traffic.
                decode_all(&mut decoder, &datagrams, &mut events);
                group.throughput(Throughput::Elements(metrics as u64));
                let id = format!(
                    "{}/{}_contexts/{}_per_datagram_(mean_{:.1})",
                    profile.name, context_count, per_datagram, per
                );
                group.bench_function(BenchmarkId::from_parameter(id), |b| {
                    b.iter_custom(|iterations| {
                        (0..iterations)
                            .map(|_| decode_all(&mut decoder, &datagrams, &mut events))
                            .sum()
                    });
                });
            }
        }
    }
    group.finish();
}

fn shared_resolvers(c: &mut Criterion) {
    let runtime = runtime();
    let _runtime = runtime.enter();
    let mut group = c.benchmark_group("dogstatsd_codec_shared_resolvers");
    group.sample_size(20);
    let profile = PROFILES[1];
    for context_count in [3_000, 100_000] {
        let mut rng = XorShift(0x2545_f491_4f6c_dd1d);
        let contexts = contexts(profile, context_count, &mut rng);
        let lines = lines(&contexts, METRICS.max(context_count), &mut rng);
        for per_datagram in [1, 50] {
            let datagrams = datagrams(&lines, per_datagram);
            let metrics = datagrams.iter().map(|(_, held)| held).sum::<usize>();
            let decoder = DatagramDecoder::with_defaults().expect("the default decoder builds");
            decode_all(&mut decoder.clone(), &datagrams, &mut Vec::new());
            group.throughput(Throughput::Elements(metrics as u64));
            for threads in [1, 2, 4, 8] {
                // Each thread decodes an interleaved share of the datagrams, so every thread sees every context.
                let shares = (0..threads)
                    .map(|thread| {
                        datagrams
                            .iter()
                            .skip(thread)
                            .step_by(threads)
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                let mut decoders = (0..threads).map(|_| decoder.clone()).collect::<Vec<_>>();
                let id = format!(
                    "{}_contexts/{}_per_datagram/{}_threads",
                    context_count, per_datagram, threads
                );
                group.bench_function(BenchmarkId::from_parameter(id), |b| {
                    b.iter_custom(|iterations| (0..iterations).map(|_| decode_together(&mut decoders, &shares)).sum());
                });
            }
        }
    }
    group.finish();
}

/// Decodes each share on its own thread with its own decoder, all released at once, and returns the time from their
/// release to the end of the last one.
fn decode_together(decoders: &mut [DatagramDecoder], shares: &[Vec<(Vec<u8>, usize)>]) -> Duration {
    let start = Barrier::new(decoders.len() + 1);
    thread::scope(|scope| {
        let workers = decoders
            .iter_mut()
            .zip(shares)
            .map(|(decoder, share)| {
                let start = &start;
                scope.spawn(move || {
                    let mut events = Vec::with_capacity(share.iter().map(|(_, held)| held).sum());
                    start.wait();
                    for (datagram, _) in share {
                        decoder.decode(datagram, &mut events);
                    }
                    let finished = Instant::now();
                    black_box(&events);
                    finished
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        let released = Instant::now();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a decoder thread does not panic"))
            .max()
            .expect("there is a decoder thread")
            .saturating_duration_since(released)
    })
}

criterion_group!(benches, decode, shared_resolvers);
criterion_main!(benches);
