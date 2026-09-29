//! Panic reporting: a global hook counts panics by truncated message, and a source reports the
//! counts as metric events.
//!
//! The hook is installed unconditionally and chains whatever hook is current, so instrumentation
//! registered first stays intact. A panicking task is already isolated by the runtime—the unwind
//! stops at the task boundary—so the hook only observes: it counts the panic under its truncated
//! message tag, logs the message with a bounded stack, and defers to the previous hook. The
//! reporter source drains the counts every window and forwards them through its metrics output,
//! so the metric reaches the backend under its published name.

use std::backtrace::Backtrace;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use saluki_common::collections::FastHashMap;
use saluki_context::{tags::TagSet, Context};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{sources::*, BuildContext},
    data_model::event::{metric::Metric, Event, EventType},
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::{
    pin, select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, warn};

/// How long the reporter waits between reporting windows.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Metric name for panic counts.
const PANIC_METRIC_NAME: &str = "datadog.trace_agent.panic";

/// How many characters of the panic message become the metric tag, keeping the series count
/// bounded.
const MESSAGE_TAG_CHARS: usize = 17;

/// How many bytes of the stack are kept for the log line.
const STACK_LOG_BYTES: usize = 4096;

/// Counts panics by their truncated message tag.
#[derive(Default)]
struct PanicCounts {
    counts: FastHashMap<MetaString, u64>,
}

impl PanicCounts {
    fn record(&mut self, tag: MetaString) {
        *self.counts.entry(tag).or_insert(0) += 1;
    }
}

static PANIC_COUNTS: LazyLock<Mutex<PanicCounts>> = LazyLock::new(|| Mutex::new(PanicCounts::default()));

fn lock_counts() -> MutexGuard<'static, PanicCounts> {
    // A poisoned lock means the hook itself panicked mid-record; recover the state rather than
    // stopping the counts over it.
    PANIC_COUNTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Installs the global panic reporting hook.
///
/// Must be called before any panic can occur, after any instrumentation hook that should stay
/// active: the installed hook chains the current one, so panic reporting observes first and the
/// previous hook—the default formatter or test instrumentation—still runs.
pub fn install_panic_reporter() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        let tag = truncate_chars(&message, MESSAGE_TAG_CHARS);
        lock_counts().record(tag.clone());

        // Forced capture ignores the backtrace environment, so the stack is available in every
        // build configuration.
        let backtrace = Backtrace::force_capture().to_string();
        let stack = truncate_bytes(&backtrace, STACK_LOG_BYTES);
        error!(panic = %tag, stack, "A task panicked.");

        previous_hook(info);
    }));
}

/// Extracts the panic message, wherever the payload carries one.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    payload_message(info.payload())
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|value| (*value).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

/// Truncates to at most `max_chars` characters, without splitting a character.
fn truncate_chars(value: &str, max_chars: usize) -> MetaString {
    match value.char_indices().nth(max_chars) {
        Some((split_at, _)) => MetaString::from(&value[..split_at]),
        None => MetaString::from(value),
    }
}

/// Truncates to at most `max_bytes` bytes, backing off to a character boundary.
fn truncate_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Reports panic counts as metric events.
///
/// Each window drains the counts and reports every tag as a delta counter, so quiet windows
/// report nothing and an alert window count is the number of panics it contained.
pub struct PanicReporterConfiguration;

#[async_trait]
impl SourceBuilder for PanicReporterConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(PanicReporter))
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("metrics", EventType::Metric)]);
        &OUTPUTS
    }
}

impl MemoryBounds for PanicReporterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<PanicReporter>("panic reporter source");
    }
}

struct PanicReporter;

#[async_trait]
impl Source for PanicReporter {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let mut tick_interval = interval(REPORT_INTERVAL);
        tick_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        health.mark_ready();
        debug!("Panic reporter source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                _ = tick_interval.tick() => {
                    for (tag, count) in drain_panic_counts() {
                        let event = panic_metric_event(&tag, count);
                        if let Err(error) = context.dispatcher().dispatch_one_named("metrics", event).await {
                            warn!(error = %error, "Failed to dispatch panic metric.");
                        }
                    }
                },
            }
        }

        debug!("Panic reporter source stopped.");
        Ok(())
    }
}

/// Takes the current window's counts, leaving the map empty and its capacity for the next one.
fn drain_panic_counts() -> Vec<(MetaString, u64)> {
    let mut counts = lock_counts();
    counts.counts.drain().collect()
}

/// Builds the metric event for one panic tag's window count.
fn panic_metric_event(tag: &str, count: u64) -> Event {
    let mut tags = TagSet::with_capacity(1);
    tags.insert_tag(MetaString::from(format!("err:{}", tag)));
    Event::Metric(Metric::counter(
        Context::from_parts(PANIC_METRIC_NAME, tags),
        count as f64,
    ))
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;

    fn record(tag: &str, times: u64) {
        let tag = MetaString::from(tag);
        let mut counts = lock_counts();
        for _ in 0..times {
            counts.record(tag.clone());
        }
    }

    #[test]
    fn panic_message_extracts_string_payloads() {
        // The hook itself is global, so installing it in tests would observe every other test's
        // panics; the payload path is exercised directly instead.
        let static_str: &(dyn std::any::Any + Send) = &"index out of bounds: the len is 3";
        assert_eq!(payload_message(static_str), "index out of bounds: the len is 3");

        let owned: &(dyn std::any::Any + Send) = &String::from("owned message");
        assert_eq!(payload_message(owned), "owned message");

        let other: &(dyn std::any::Any + Send) = &17;
        assert_eq!(payload_message(other), "<non-string panic payload>");
    }

    #[test]
    fn message_tags_truncate_to_the_char_bound() {
        assert_eq!(&*truncate_chars("abcdefghij", 17), "abcdefghij");
        assert_eq!(&*truncate_chars(&"abcdefghij".repeat(3), 17), "abcdefghijabcdefg");

        // The cut lands inside a multi-byte character and backs off to the boundary.
        let multibyte = "é".repeat(20);
        let truncated = truncate_chars(&multibyte, 17);
        assert_eq!(truncated.chars().count(), 17);
        assert!(truncated.chars().all(|c| c == 'é'));
    }

    #[test]
    fn stacks_truncate_to_the_byte_bound() {
        assert_eq!(truncate_bytes("short", 4096), "short");

        let long = "a".repeat(8192);
        assert_eq!(truncate_bytes(&long, 4096).len(), 4096);

        // The cut lands inside a multi-byte character and backs off to the boundary.
        let multibyte = "é".repeat(4096);
        let truncated = truncate_bytes(&multibyte, 4096);
        assert!(multibyte.is_char_boundary(truncated.len()));
        assert!(truncated.chars().all(|c| c == 'é'));
    }

    #[test]
    fn window_counts_drain_and_reset() {
        // Tests share the global counts, so drain before and after to stay isolated.
        let _ = drain_panic_counts();
        record("panicked at 'db query'", 3);
        let drained = drain_panic_counts();
        assert_eq!(drained, vec![(MetaString::from("panicked at 'db query'"), 3)]);
        assert!(drain_panic_counts().is_empty());
    }

    #[test]
    fn panic_metrics_carry_the_tag_and_count() {
        let event = panic_metric_event("out of memory", 4);
        let metric = event.try_as_metric().expect("panic metrics are metric events");
        assert_eq!(metric.context().name(), "datadog.trace_agent.panic");
        assert!(metric.context().tags().has_tag("err:out of memory"));
        assert_eq!(*metric.values(), MetricValues::counter(4.0));
    }
}
