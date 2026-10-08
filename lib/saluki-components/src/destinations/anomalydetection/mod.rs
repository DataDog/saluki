//! Anomaly detection forwarder destination.
//!
//! Forwards scalar metrics from ADP's pipeline to an isolated Agent Anomaly Detection
//! process over the FIT shared-memory transport. The receiving process owns the ring and
//! listens on the configured setup endpoint; this destination connects as the producer.
//!
//! Metrics are forwarded as `DDCHECKS` scalar metric records, one per data point, using
//! the point's own timestamp when present and the wall clock otherwise. Non-scalar
//! metric types (sets, histograms, distributions) are skipped, as are the non-metric
//! event kinds this destination never subscribes to.

use std::io;
use std::num::NonZeroU64;
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use datadog_checks_protocol::{BatchOutcome, Message, Metric as ChecksMetric, Producer as FitProducer};
use datadog_protos::checks::metric::MetricType;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{destinations::*, BuildContext},
    data_model::event::{
        metric::{Metric, MetricValues},
        Event, EventType,
    },
};
use saluki_error::{generic_error, GenericError};
use saluki_fit::{CancellationToken, ProducerConfig, SetupEndpoint};
use tokio::select;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// Records buffered between the async event loop and the FIT sender thread.
const SEND_CHANNEL_CAPACITY: usize = 4096;
/// FIT records published per `send_batch` call, the transport's measured sweet spot.
const SEND_BATCH_MAX: usize = 64;
/// How often the forwarder logs its forwarding counters while traffic flows.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// FIT forwarder destination for the isolated anomaly detection process.
#[derive(Debug)]
pub struct AnomalyDetectionForwarderConfiguration {
    setup_endpoint: SetupEndpoint,
}

impl AnomalyDetectionForwarderConfiguration {
    /// Creates a forwarder targeting the given FIT setup endpoint, which the anomaly
    /// detection process owns and listens on.
    pub fn new(setup_endpoint: SetupEndpoint) -> Self {
        Self { setup_endpoint }
    }
}

#[async_trait]
impl DestinationBuilder for AnomalyDetectionForwarderConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(AnomalyDetectionForwarder {
            setup_endpoint: self.setup_endpoint.clone(),
        }))
    }
}

impl MemoryBounds for AnomalyDetectionForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built, plus the
        // bounded hand-off buffer to the FIT sender thread.
        builder
            .minimum()
            .with_single_value::<AnomalyDetectionForwarder>("anomalydetection forwarder");
    }
}

struct AnomalyDetectionForwarder {
    setup_endpoint: SetupEndpoint,
}

#[derive(Default)]
struct ForwardingCounters {
    forwarded: u64,
    channel_dropped: u64,
    ring_dropped: u64,
    non_scalar_skipped: u64,
}

impl ForwardingCounters {
    fn log(&self) {
        info!(
            "Anomaly detection forwarding: forwarded={} channel_dropped={} ring_dropped={} non_scalar_skipped={}.",
            self.forwarded, self.channel_dropped, self.ring_dropped, self.non_scalar_skipped
        );
    }
}

#[async_trait]
impl Destination for AnomalyDetectionForwarder {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        // The FIT producer is blocking, so it runs on a dedicated thread fed by a bounded
        // channel. The async loop converts metrics and hands them off; the thread
        // publishes them in batches of up to `SEND_BATCH_MAX` records.
        let (sender, receiver) = mpsc::sync_channel::<Message>(SEND_CHANNEL_CAPACITY);
        let (connected_tx, connected_rx) = oneshot::channel::<io::Result<()>>();
        let cancellation = CancellationToken::new();
        let thread_cancellation = cancellation.clone();
        let setup_endpoint = self.setup_endpoint.clone();

        let sender_thread = thread::Builder::new()
            .name("anomalydetection_fit".to_string())
            .spawn(move || fit_sender_thread(setup_endpoint, thread_cancellation, receiver, connected_tx))
            .map_err(|error| generic_error!("Failed to spawn anomaly detection FIT thread: {error}"))?;

        // Wait for the FIT session before declaring readiness. The anomaly detection
        // process owns the ring, so it must be listening first; a connection failure
        // here is a configuration or ordering problem worth failing loudly for.
        let connected = async {
            connected_rx
                .await
                .unwrap_or_else(|_| Err(io::Error::other("FIT sender thread exited")))
        };
        tokio::pin!(connected);

        let mut counters = ForwardingCounters::default();
        let mut last_log = Instant::now();

        select! {
            _ = health.live() => {
                let _ = cancellation.cancel();
                drop(sender);
                let _ = sender_thread.join();
                return Ok(());
            },
            result = &mut connected => match result {
                Ok(()) => {},
                Err(error) => {
                    let _ = sender_thread.join();
                    return Err(generic_error!(
                        "Could not connect to the anomaly detection process at `{:?}`: {error}. Start the anomaly detection process before ADP.",
                        self.setup_endpoint
                    ));
                },
            },
        }

        health.mark_ready();
        info!(
            "Anomaly detection FIT forwarder connected to `{:?}`.",
            self.setup_endpoint
        );

        loop {
            select! {
                _ = health.live() => break,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        let mut forwarded_any = false;
                        for event in events {
                            if let Event::Metric(metric) = event {
                                let messages = convert_metric(metric, &mut counters);
                                for message in messages {
                                    forwarded_any = true;
                                    match sender.try_send(message) {
                                        Ok(()) => counters.forwarded += 1,
                                        Err(TrySendError::Full(_)) => counters.channel_dropped += 1,
                                        Err(TrySendError::Disconnected(_)) => {
                                            warn!("Anomaly detection FIT sender thread has stopped; destination shutting down.");
                                            let _ = sender_thread.join();
                                            return Ok(());
                                        },
                                    }
                                }
                            }
                        }
                        if forwarded_any && last_log.elapsed() > STATS_LOG_INTERVAL {
                            counters.log();
                            last_log = Instant::now();
                        }
                    },
                    None => break,
                },
            }
        }

        // Signal shutdown and let the sender thread flush its remaining batches.
        drop(sender);
        match sender_thread.join() {
            Ok(()) => {},
            Err(_) => warn!("Anomaly detection FIT sender thread panicked while shutting down."),
        }
        let _ = cancellation.cancel();
        counters.log();
        debug!("Anomaly detection FIT forwarder stopped.");

        Ok(())
    }
}

/// Runs the FIT producer session: connects once, then publishes batches until the
/// channel closes, reporting ring rejections without retrying.
fn fit_sender_thread(
    setup_endpoint: SetupEndpoint, cancellation: CancellationToken, receiver: Receiver<Message>,
    connected_tx: oneshot::Sender<io::Result<()>>,
) {
    let config = ProducerConfig::for_endpoint(setup_endpoint);
    let mut producer = match FitProducer::connect_with_cancel(config, &cancellation) {
        Ok(producer) => {
            let _ = connected_tx.send(Ok(()));
            producer
        },
        Err(error) => {
            let _ = connected_tx.send(Err(error));
            return;
        },
    };

    let mut batch = Vec::with_capacity(SEND_BATCH_MAX);
    // Block for the first record, then opportunistically fill a batch. When the channel
    // closes, flush what we have and stop.
    while let Ok(message) = receiver.recv() {
        batch.push(message);
        while batch.len() < SEND_BATCH_MAX {
            match receiver.try_recv() {
                Ok(message) => batch.push(message),
                Err(_) => break,
            }
        }

        match producer.send_batch(&batch) {
            Ok(outcome) => report_outcome(&outcome, batch.len()),
            Err(error) => {
                warn!("Anomaly detection FIT send failed: {error}. Stopping forwarder thread.");
                break;
            },
        }

        batch.clear();
    }
}

fn report_outcome(outcome: &BatchOutcome, offered: usize) {
    if outcome.accepted < offered {
        warn!(
            "Anomaly detection FIT ring rejected {} of {} records in a batch; the consumer is behind and those records are dropped.",
            offered - outcome.accepted,
            offered
        );
    }
    if let Some(error) = &outcome.notification_error {
        warn!("Anomaly detection FIT consumer notification failed: {error}.");
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Converts one scalar metric event into Checks FIT records, one per data point.
///
/// Sets, histograms, and distributions have no scalar representation in the Checks
/// protocol and are skipped (counted in `counters`). Points without a timestamp use the
/// wall clock, which is also how the receiving process interprets them.
fn convert_metric(metric: Metric, counters: &mut ForwardingCounters) -> Vec<Message> {
    let (context, values, _metadata) = metric.into_parts();
    let name = context.name().to_string();
    let hostname = context.host().unwrap_or_default().to_string();
    let tags: Vec<String> = context
        .tags()
        .clone()
        .into_iter()
        .map(|tag| tag.to_string())
        .collect();

    let (metric_type, points, interval_secs) = match values {
        MetricValues::Counter(points) => (MetricType::Counter as i32, points, 0),
        MetricValues::Gauge(points) => (MetricType::Gauge as i32, points, 0),
        MetricValues::Rate(points, interval) => (MetricType::Rate as i32, points, interval.as_secs()),
        _ => {
            counters.non_scalar_skipped += 1;
            return Vec::new();
        },
    };

    let now = unix_now_secs();
    points
        .into_iter()
        .map(|(timestamp, value)| {
            let timestamp = timestamp.map(NonZeroU64::get).unwrap_or(now);
            Message::Metric(ChecksMetric {
                metric_type,
                name: name.clone(),
                value,
                timestamp,
                tags: tags.clone(),
                hostname: hostname.clone(),
                interval_secs,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use saluki_core::data_model::event::metric::context::Context;
    use saluki_core::data_model::tags::{Tag, TagSet};
    use std::collections::BTreeSet;

    fn context_with_tags(name: &str, host: Option<&str>, tags: &[&str]) -> Context {
        let mut tag_set = TagSet::default();
        for tag in tags {
            tag_set.insert_tag(Tag::from(*tag));
        }
        let context = Context::from_parts(name, tag_set.into_shared());
        match host {
            Some(host) => context.with_host(Some(host.into())),
            None => context,
        }
    }

    fn counters() -> ForwardingCounters {
        ForwardingCounters::default()
    }

    fn unwrap_metric(messages: Vec<Message>) -> Vec<ChecksMetric> {
        messages
            .into_iter()
            .map(|message| match message {
                Message::Metric(metric) => metric,
                _ => panic!("expected metric message"),
            })
            .collect()
    }

    #[test]
    fn gauge_conversion_carries_identity_and_timestamp() {
        let metric = Metric::gauge(context_with_tags("kafka.lag", Some("host-1"), &["env:prod"]), (1776943265, 42.5));
        let records = unwrap_metric(convert_metric(metric, &mut counters()));

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].metric_type, MetricType::Gauge as i32);
        assert_eq!(records[0].name, "kafka.lag");
        assert_eq!(records[0].value, 42.5);
        assert_eq!(records[0].timestamp, 1776943265);
        assert_eq!(records[0].tags, vec!["env:prod".to_string()]);
        assert_eq!(records[0].hostname, "host-1");
        assert_eq!(records[0].interval_secs, 0);
    }

    #[test]
    fn counter_without_host_forwards_empty_hostname() {
        let metric = Metric::counter(context_with_tags("requests", None, &[]), (1, 7.0));
        let records = unwrap_metric(convert_metric(metric, &mut counters()));

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].hostname, "");
        assert!(records[0].tags.is_empty());
    }

    #[test]
    fn untimestamped_points_use_the_wall_clock() {
        let before = unix_now_secs();
        let metric = Metric::gauge(context_with_tags("mem.usage", None, &[]), 10.0);
        let records = unwrap_metric(convert_metric(metric, &mut counters()));

        assert_eq!(records.len(), 1);
        assert!(records[0].timestamp >= before);
    }

    #[test]
    fn rate_conversion_carries_the_interval() {
        let metric = Metric::rate(context_with_tags("events", None, &[]), (5, 3.0), Duration::from_secs(15));
        let records = unwrap_metric(convert_metric(metric, &mut counters()));

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].metric_type, MetricType::Rate as i32);
        assert_eq!(records[0].interval_secs, 15);
    }

    #[test]
    fn multiple_points_become_multiple_records() {
        let metric = Metric::gauge(context_with_tags("cpu", None, &[]), [(10, 1.0), (11, 2.0), (12, 3.0)]);
        let records = unwrap_metric(convert_metric(metric, &mut counters()));

        assert_eq!(records.len(), 3);
        let timestamps: BTreeSet<u64> = records.iter().map(|record| record.timestamp).collect();
        assert_eq!(timestamps, [10, 11, 12].into_iter().collect());
    }

    #[test]
    fn non_scalar_metrics_are_skipped_and_counted() {
        let histogram = Metric::histogram(context_with_tags("latency", None, &[]), (10, 1.0));

        let mut counters = counters();
        assert!(convert_metric(histogram, &mut counters).is_empty());
        assert_eq!(counters.non_scalar_skipped, 1);
    }
}
