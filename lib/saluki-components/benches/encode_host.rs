//! The metrics encoders as ADP runs them: their front task on a primary runtime of two workers and their request
//! builders on a dedicated global pool of eight threads, with zstd at the default level 3 and the default payload
//! limits.
//!
//! Two encoders run the same shapes:
//!
//! - **`native_datadog_metrics`**: the native Datadog metrics encoder, `DatadogMetrics`, with its one request builder;
//! - **`metrics_encode_host`**: the multiplexed encode host, `MetricsEncodeHost`, with one lease whose settings are the
//!   native encoder's: buffers name the lease in their header's entry, and the front task moves each whole buffer into
//!   the lease's builder shard.
//!
//! Each iteration sends one flush's worth of aggregated output, one metric per context in buffers of 1,024 events,
//! and waits until payloads carrying every one of them have come out:
//!
//! - **`series`**: counters and gauges, encoded as V2 series;
//! - **`series_v3`**: the same metrics encoded as V3 series, which a Datadog intake takes by default;
//! - **`sketches`**: distributions, encoded as V2 sketches;
//! - **`per_buffer`**: 16,384 buffers of one series metric each: the cost per buffer, less the cost per metric of
//!   `series`, is the encoder's front task's cost per buffer.
//!
//! The measure is the CPU time the process used during the iteration (`getrusage`), not wall time: the encoders flush
//! the payloads that are not full when their flush timeout (2 s) expires, which is idle time that would otherwise
//! dominate. Building the iteration's buffers happens before the measured section. Throughput is therefore metrics
//! per CPU-second: its inverse is the CPU cost per metric of the front task, the builder, and the compression, with
//! the bench's channel sends and payload drops included. Iterations take at least the flush timeout of wall time.
//!
//! `SALUKI_BENCH_ENCODE_CONTEXTS` sets the contexts per iteration (default 1,000,000).

use std::{
    num::NonZeroU8,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use agent_data_plane_config::{
    shared::{Compression, Endpoints, MetricsEncoding, SharedConfiguration, V3SeriesMode},
    ConfigValue,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode, Throughput};
use saluki_components::{
    encoders::{encode_host::MetricsEncodeHostConfiguration, DatadogMetricsConfiguration},
    forwarders::delivery::{EncodeLeaseSpec, LeaseKeyDigest, PayloadRecipients, RecipientSlot, ResolvedRecipients},
};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryLimiter},
    components::{
        encoders::{EncoderBuilder, EncoderContext},
        BuildContext, ComponentContext,
    },
    data_model::{
        event::{
            metric::{context::Context, Metric},
            Event,
        },
        payload::Payload,
        tags::{Tag, TagSet},
    },
    execution::EntryId,
    health::HealthRegistry,
    runtime::{
        self,
        state::{DataspaceRegistry, ResourceRegistry},
        Supervisor,
    },
    support::SubsystemIdentifier,
    topology::{
        interconnect::{Consumer, Dispatcher},
        ComponentId, EventsBuffer, OutputName, TopologyContext,
    },
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::{mpsc, oneshot, Notify},
};

/// The events per buffer, as the buffers of a topology hold.
const BUFFER_EVENTS: usize = 1_024;

/// The one-metric buffers of the `per_buffer` shape.
const ONE_METRIC_BUFFERS: usize = 16_384;

/// The lease the encode host's buffers name.
const LEASE: EntryId = EntryId::new(1);

/// Returns the contexts per iteration: `SALUKI_BENCH_ENCODE_CONTEXTS`, or a million.
fn contexts_per_iteration() -> usize {
    std::env::var("SALUKI_BENCH_ENCODE_CONTEXTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000_000)
}

/// Returns the shared configuration a translated Agent configuration carries for the metrics encoder: zstd at the
/// default level, the default payload limits, the default 2 s flush timeout, and series in their V2 form unless `v3`.
fn shared_configuration(v3: bool) -> SharedConfiguration {
    SharedConfiguration {
        endpoints: Endpoints {
            api_key: "bench-api-key".to_string(),
            site: ConfigValue::defaulted("datadoghq.com".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            compression: Compression {
                compressor_kind: "zstd".to_string(),
                ..Default::default()
            },
            ..Default::default()
        },
        metrics_encoding: MetricsEncoding {
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            max_series_payload_size: 512_000,
            max_series_uncompressed_payload_size: 5_242_880,
            max_series_points_per_payload: 10_000,
            use_v2_series_api: true,
            // The default sends V3 series to a Datadog intake.
            v3_series_mode: if v3 {
                V3SeriesMode::DatadogOnly
            } else {
                V3SeriesMode::Disabled
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Returns `count` contexts: 1,000 names, each with a host tag, an environment tag, and a tag of its own, as
/// aggregated DogStatsD contexts carry.
fn contexts(count: usize) -> Vec<Context> {
    (0..count)
        .map(|index| {
            let tags = [
                format!("host:host-{}", index % 64),
                "env:prod".to_string(),
                format!("context:{}", index),
            ];
            Context::from_parts(
                format!("bench.metric.{}", index % 1_000),
                tags.iter().map(|tag| Tag::from(tag.as_str())).collect::<TagSet>(),
            )
        })
        .collect()
}

/// What a shape sends per iteration.
#[derive(Clone, Copy)]
enum Shape {
    Series,
    SeriesV3,
    Sketches,
    PerBuffer,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Self::Series => "series",
            Self::SeriesV3 => "series_v3",
            Self::Sketches => "sketches",
            Self::PerBuffer => "per_buffer",
        }
    }

    /// Returns whether the shape's series are encoded in their V3 form.
    fn v3(self) -> bool {
        matches!(self, Self::SeriesV3)
    }

    /// Returns the metrics an iteration sends.
    fn metrics(self, contexts: &[Context]) -> usize {
        match self {
            Self::PerBuffer => ONE_METRIC_BUFFERS,
            Self::Series | Self::SeriesV3 | Self::Sketches => contexts.len(),
        }
    }

    /// Returns one iteration's buffers, each naming `entry` in its header, and the metrics they hold.
    fn buffers(self, contexts: &[Context], entry: Option<EntryId>) -> (Vec<EventsBuffer>, usize) {
        let metric = |index: usize| {
            let context = contexts[index % contexts.len()].clone();
            match (self, index % 2) {
                (Self::Sketches, _) => Metric::distribution(context, (index % 100) as f64),
                (_, 0) => Metric::counter(context, 1.0),
                (_, _) => Metric::gauge(context, (index % 100) as f64),
            }
        };
        let (count, per_buffer) = match self {
            Self::Series | Self::SeriesV3 | Self::Sketches => (contexts.len(), BUFFER_EVENTS),
            Self::PerBuffer => (ONE_METRIC_BUFFERS, 1),
        };
        let new_buffer = || {
            let mut buffer = EventsBuffer::with_capacity(per_buffer);
            buffer.header_mut().entry = entry;
            buffer
        };
        let mut buffers = Vec::with_capacity(count.div_ceil(per_buffer));
        let mut buffer = new_buffer();
        for index in 0..count {
            if buffer.len() == per_buffer {
                buffers.push(std::mem::replace(&mut buffer, new_buffer()));
            }
            assert!(buffer.try_push(Event::Metric(metric(index))).is_none());
        }
        buffers.push(buffer);
        (buffers, count)
    }
}

/// Which encoder a benchmark runs.
#[derive(Clone, Copy)]
enum Encoder {
    Native,
    Host,
}

impl Encoder {
    fn name(self) -> &'static str {
        match self {
            Self::Native => "native_datadog_metrics",
            Self::Host => "metrics_encode_host",
        }
    }

    /// Returns the entry the encoder's buffers name.
    fn entry(self) -> Option<EntryId> {
        match self {
            Self::Native => None,
            Self::Host => Some(LEASE),
        }
    }

    /// Returns the encoder's builder: the native encoder, or an encode host running one lease with its settings.
    fn builder(self, v3: bool) -> Box<dyn EncoderBuilder + Send + Sync> {
        let native = DatadogMetricsConfiguration::from_configuration(&shared_configuration(v3));
        match self {
            Self::Native => Box::new(native),
            Self::Host => {
                let recipients = PayloadRecipients::new(Arc::from([RecipientSlot::new(0)]));
                let spec = EncodeLeaseSpec {
                    lease: LEASE,
                    key: LeaseKeyDigest {
                        output: "dd_metrics_encode".into(),
                        binding: [0; 32],
                    },
                    telemetry_id: "dd_metrics_encode".into(),
                    settings: native.encoder_settings().expect("the endpoints resolve"),
                    recipients: Arc::new(ResolvedRecipients {
                        v2_series: recipients.clone(),
                        v2_sketches: recipients.clone(),
                        v3_series: recipients.clone(),
                        v3_sketches: recipients,
                    }),
                    builders: NonZeroU8::MIN,
                };
                Box::new(MetricsEncodeHostConfiguration::new().with_lease(spec))
            }
        }
    }
}

/// Returns the CPU time the process has used so far, user and system.
fn process_cpu_time() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable memory of the size `getrusage` writes, and `RUSAGE_SELF` is valid.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(result, 0, "getrusage(RUSAGE_SELF) succeeds");
    // SAFETY: `getrusage` succeeded, so it initialized `usage`.
    let usage = unsafe { usage.assume_init() };
    let time = |time: libc::timeval| Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1_000);
    time(usage.ru_utime) + time(usage.ru_stime)
}

/// A running encoder: its input, and the count of metrics its payloads have carried out.
struct Running {
    input: mpsc::Sender<EventsBuffer>,
    encoded: Arc<AtomicU64>,
    progress: Arc<Notify>,
    _shutdown: oneshot::Sender<()>,
}

impl Running {
    /// Builds an encoder and runs it under a supervisor on `primary`, with its request builders on `global`.
    fn start(primary: &Runtime, global: &Runtime, builder: Box<dyn EncoderBuilder + Send + Sync>) -> Self {
        let global = global.handle().clone();
        primary.block_on(async move {
            let topology = SubsystemIdentifier::from_segments(["bench"]);
            let component = ComponentContext::encoder(&topology, ComponentId::try_from("dd_metrics_encode").unwrap());
            let encoder = builder
                .build(BuildContext::new(component.clone(), ResourceRegistry::new()))
                .await
                .expect("the encoder builds");

            let health_registry = HealthRegistry::new();
            let health = health_registry
                .register_component(&component.identity())
                .expect("the encoder registers once");
            let topology_context = TopologyContext::new(
                Arc::from("bench"),
                MemoryLimiter::noop(),
                health_registry,
                global,
                DataspaceRegistry::new(),
            );
            let (payloads_tx, mut payloads_rx) = mpsc::channel::<Payload>(8);
            let mut dispatcher = Dispatcher::new(component.clone());
            dispatcher.add_output(OutputName::Default).unwrap();
            dispatcher
                .attach_sender_to_output(&OutputName::Default, payloads_tx)
                .unwrap();
            let (input, input_rx) = mpsc::channel(128);
            let context = EncoderContext::new(
                &topology_context,
                &component,
                ComponentRegistry::default(),
                health,
                dispatcher,
                Consumer::new(component.clone(), input_rx),
            );

            // The delivery that discards: count what each payload carries, and drop it.
            let encoded = Arc::new(AtomicU64::new(0));
            let progress = Arc::new(Notify::new());
            let (counted, notified) = (Arc::clone(&encoded), Arc::clone(&progress));
            tokio::spawn(async move {
                while let Some(payload) = payloads_rx.recv().await {
                    if let Payload::Http(payload) = payload {
                        let (metadata, _) = payload.into_parts();
                        counted.fetch_add(metadata.event_count() as u64, Ordering::SeqCst);
                        notified.notify_one();
                    }
                }
            });

            let mut supervisor = Supervisor::new("encode-host-bench").expect("the supervisor is created");
            supervisor.add_worker(runtime::worker("dd_metrics_encode", encoder.run(context)).build());
            let (shutdown, shutdown_rx) = oneshot::channel();
            tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });
            Self {
                input,
                encoded,
                progress,
                _shutdown: shutdown,
            }
        })
    }

    /// Sends `buffers`, which hold `metrics` metrics, and waits until payloads carrying all of them have come out.
    async fn encode(&self, buffers: Vec<EventsBuffer>, metrics: usize) {
        let target = self.encoded.load(Ordering::SeqCst) + metrics as u64;
        for buffer in buffers {
            self.input.send(buffer).await.expect("the encoder runs");
        }
        while self.encoded.load(Ordering::SeqCst) < target {
            let _ = tokio::time::timeout(Duration::from_millis(50), self.progress.notified()).await;
        }
    }
}

fn encode_host(c: &mut Criterion) {
    let primary = Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("primary")
        .enable_all()
        .build()
        .unwrap();
    let global = Builder::new_multi_thread()
        .worker_threads(8)
        .thread_name("global")
        .enable_all()
        .build()
        .unwrap();
    let contexts = contexts(contexts_per_iteration());

    let mut group = c.benchmark_group("encode_host");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(1));
    for shape in [Shape::Series, Shape::SeriesV3, Shape::Sketches, Shape::PerBuffer] {
        for encoder in [Encoder::Native, Encoder::Host] {
            let running = Running::start(&primary, &global, encoder.builder(shape.v3()));
            let metrics = shape.metrics(&contexts);
            group.throughput(Throughput::Elements(metrics as u64));
            let id = BenchmarkId::new(encoder.name(), format!("{}/{}_metrics", shape.name(), metrics));
            group.bench_function(id, |b| {
                b.iter_custom(|iterations| {
                    let mut cpu = Duration::ZERO;
                    for _ in 0..iterations {
                        let (buffers, metrics) = shape.buffers(&contexts, encoder.entry());
                        let before = process_cpu_time();
                        primary.block_on(running.encode(buffers, metrics));
                        cpu += process_cpu_time().saturating_sub(before);
                    }
                    cpu
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, encode_host);
criterion_main!(benches);
