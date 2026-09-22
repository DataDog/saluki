//! APM event extraction.
//!
//! Promotes spans of a trace to standalone events the backend indexes even when the trace itself
//! is dropped. A chain of extractors nominates spans, a per-event rate flip thins the nominees,
//! and a decaying events-per-second budget caps the survivors. Survivors are marked with the
//! analyzed key and their sampling-rate metadata whether the trace is kept or dropped; callers
//! forward survivors as standalone events only when the trace is dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use saluki_common::collections::FastHashMap;
use saluki_context::{tags::TagSet, Context};
use saluki_core::data_model::event::trace::{AttributeValue, Span, Trace};
use saluki_core::data_model::event::{metric::Metric, Event};
use stringtheory::MetaString;
use tracing::warn;

use super::rare_sampler::KEY_TOP_LEVEL;
use super::{sample_by_rate, KEY_ANALYZED_SPANS, PRIORITY_USER_KEEP};

/// Sampling-rate metadata stamped on extracted events. Values below 1 are written; values of 1
/// or above are omitted because the backend assumes 1.0 for absent keys.
const KEY_EVENT_EXTRACTION_RATE: &str = "_dd1.sr.eausr";
const KEY_MAX_EPS_RATE: &str = "_dd1.sr.eamax";
const KEY_CLIENT_RATE: &str = "_dd1.sr.rcusr";
const KEY_PRE_SAMPLE_RATE: &str = "_dd1.sr.rapre";

const METRIC_EVENTS_EXTRACTED: &str = "datadog.trace_agent.receiver.events_extracted";
const METRIC_EVENTS_SAMPLED: &str = "datadog.trace_agent.receiver.events_sampled";
const METRIC_MAX_EPS_MAX_RATE: &str = "datadog.trace_agent.events.max_eps.max_rate";
const METRIC_MAX_EPS_CURRENT_RATE: &str = "datadog.trace_agent.events.max_eps.current_rate";
const METRIC_MAX_EPS_SAMPLE_RATE: &str = "datadog.trace_agent.events.max_eps.sample_rate";
const METRIC_MAX_EPS_REACHED_MAX: &str = "datadog.trace_agent.events.max_eps.reached_max";

/// Fraction of the rate-estimate score kept at each decay step.
///
/// Each second the score is divided by this factor, so an event's contribution fades by 12.5%
/// per second and totals 9 points of score over its fading lifetime. That total makes
/// [`COUNT_SCALE_FACTOR`] the score-to-events-per-second divisor, and the decay gives the
/// estimate a ~6 second half-life: long enough to smooth bursts, short enough to follow real
/// rate changes within one report window. A fixed behavior constant: changing it changes the
/// budget's response curve and its verdict sequence.
const DECAY_FACTOR: f64 = 1.125;

/// Score-to-events-per-second divisor.
///
/// The largest lifetime score contribution of a single event, so dividing the score by this
/// yields a lower bound on the rate; multiplying by [`DECAY_FACTOR`] yields the upper bound the
/// budget decides against, erring toward sampling less.
const COUNT_SCALE_FACTOR: f64 = 9.0;

/// Extracted and sampled event counts for one trace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct EventCounts {
    /// Events that passed the extraction rate flip.
    pub(super) extracted: u64,
    /// Events that also passed the events-per-second budget.
    pub(super) sampled: u64,
}

/// Extracts and samples events from traces.
///
/// Cloning shares the extractor chain, the budget, and the event metrics, so the transform and
/// its window reports observe the same state.
#[derive(Clone)]
pub(super) struct EventProcessor {
    inner: Arc<EventProcessorInner>,
}

struct EventProcessorInner {
    extractors: Vec<Extractor>,
    limiter: MaxEpsLimiter,
    telemetry: EventTelemetry,
}

impl EventProcessor {
    /// Creates an event processor from the resolved analytics configuration.
    ///
    /// Spans carrying their own extraction rate are always nominated first; the configured maps
    /// follow, with per-(service, operation) rates taking precedence over the legacy per-service
    /// rates. At most one configured extractor is active.
    pub(super) fn new(
        analyzed_spans_by_service: &HashMap<String, HashMap<String, f64>>,
        analyzed_rate_by_service: &HashMap<String, f64>, max_events_per_second: f64,
    ) -> Self {
        let mut extractors = vec![Extractor::Metric];
        if !analyzed_spans_by_service.is_empty() {
            extractors.push(Extractor::FixedRate(analyzed_spans_by_service.clone()));
        } else if !analyzed_rate_by_service.is_empty() {
            warn!("analyzed_rate_by_service is deprecated, please use analyzed_spans instead");
            extractors.push(Extractor::Legacy(analyzed_rate_by_service.clone()));
        }

        Self {
            inner: Arc::new(EventProcessorInner {
                extractors,
                limiter: MaxEpsLimiter::new(max_events_per_second),
                telemetry: EventTelemetry::default(),
            }),
        }
    }

    /// Creates an event processor for tests, with the given rates and budget.
    #[cfg(test)]
    pub(super) fn for_tests(
        analyzed_spans_by_service: &HashMap<String, HashMap<String, f64>>,
        analyzed_rate_by_service: &HashMap<String, f64>, max_events_per_second: f64,
    ) -> Self {
        Self::new(
            analyzed_spans_by_service,
            analyzed_rate_by_service,
            max_events_per_second,
        )
    }

    /// Extracts and samples events from a trace, marking the surviving spans.
    ///
    /// Survivors carry the analyzed marker and their sampling-rate metadata whether the trace is
    /// kept or dropped, so kept traces forward the marks with their full span payload. The
    /// returned counts report how many events survived each flip.
    pub(super) fn process(&self, trace: &mut Trace, root_span_idx: Option<usize>, priority: i32) -> EventCounts {
        let trace_id = trace.trace_id_low;

        // Rate metadata is copied from the root span; an absent rate reads as 1.0 and is not
        // written back.
        let (client_rate, pre_sample_rate) = root_span_idx
            .and_then(|idx| trace.spans().get(idx))
            .map(|root| {
                (
                    rate_attribute(root, KEY_CLIENT_RATE),
                    rate_attribute(root, KEY_PRE_SAMPLE_RATE),
                )
            })
            .unwrap_or((1.0, 1.0));

        let mut counts = EventCounts::default();
        for span in trace.spans_mut() {
            // The first extractor to nominate a span decides its extraction rate.
            let Some(extraction_rate) = self.inner.extractors.iter().find_map(|e| e.extract(span, priority)) else {
                continue;
            };
            if !sample_by_rate(trace_id, extraction_rate) {
                continue;
            }
            counts.extracted += 1;

            let (sampled, eps_rate) = self.inner.limiter.sample(trace_id, priority);
            if !sampled {
                continue;
            }
            counts.sampled += 1;
            mark_event_span(span, extraction_rate, eps_rate, client_rate, pre_sample_rate);
        }

        self.inner.telemetry.record_extracted(counts.extracted);
        self.inner.telemetry.record_sampled(counts.sampled);
        counts
    }

    /// Reports the event telemetry window as metric events.
    ///
    /// The transform's window tick calls this, so the event metrics share the sampler's report
    /// cadence and flow through the same metrics output.
    pub(super) fn take_window_events(&self) -> Vec<Event> {
        self.inner.telemetry.take_window_events(&self.inner.limiter)
    }
}

/// Reads a sampling-rate attribute, defaulting to 1.0.
fn rate_attribute(span: &Span, key: &'static str) -> f64 {
    span.attributes.get(key).and_then(AttributeValue::as_num).unwrap_or(1.0)
}

/// Marks a span as a sampled event and stamps its sampling-rate metadata.
fn mark_event_span(span: &mut Span, extraction_rate: f64, eps_rate: f64, client_rate: f64, pre_sample_rate: f64) {
    span.attributes
        .insert(MetaString::from_static(KEY_ANALYZED_SPANS), AttributeValue::Float(1.0));
    set_rate_below_one(&mut span.attributes, KEY_EVENT_EXTRACTION_RATE, extraction_rate);
    set_rate_below_one(&mut span.attributes, KEY_MAX_EPS_RATE, eps_rate);
    set_rate_below_one(&mut span.attributes, KEY_CLIENT_RATE, client_rate);
    set_rate_below_one(&mut span.attributes, KEY_PRE_SAMPLE_RATE, pre_sample_rate);
}

/// Writes a rate below 1 and removes a rate of 1 or above, so absent keys read as 1.0.
fn set_rate_below_one(attributes: &mut FastHashMap<MetaString, AttributeValue>, key: &'static str, rate: f64) {
    if rate < 1.0 {
        attributes.insert(MetaString::from_static(key), AttributeValue::Float(rate));
    } else {
        attributes.remove(key);
    }
}

/// Nominates spans for event extraction.
enum Extractor {
    /// Nominates spans carrying their own extraction rate, whatever their service or operation.
    Metric,
    /// Nominates spans by configured (service, operation) extraction rates.
    FixedRate(HashMap<String, HashMap<String, f64>>),
    /// Nominates top-level spans by configured per-service extraction rates.
    Legacy(HashMap<String, f64>),
}

impl Extractor {
    /// Returns the extraction rate for a span this extractor nominates, or `None`.
    fn extract(&self, span: &Span, priority: i32) -> Option<f64> {
        match self {
            Self::Metric => span
                .attributes
                .get(KEY_EVENT_EXTRACTION_RATE)
                .and_then(AttributeValue::as_num)
                .map(|rate| upscale_user_keep(rate, priority)),
            Self::FixedRate(rates_by_service) => {
                let service = span.service().to_lowercase();
                let operation = span.name().to_lowercase();
                rates_by_service
                    .get(service.as_str())?
                    .get(operation.as_str())
                    .map(|rate| upscale_user_keep(*rate, priority))
            }
            Self::Legacy(rates_by_service) => {
                // Only top-level spans are nominated.
                let top_level = span
                    .attributes
                    .get(KEY_TOP_LEVEL)
                    .and_then(AttributeValue::as_num)
                    .is_some_and(|value| value == 1.0);
                if !top_level {
                    return None;
                }
                let service = span.service().to_lowercase();
                rates_by_service.get(service.as_str()).copied()
            }
        }
    }
}

/// Raises a positive extraction rate to 1 for manually kept traces, whose events the customer
/// explicitly asked to keep.
fn upscale_user_keep(rate: f64, priority: i32) -> f64 {
    if rate > 0.0 && priority >= PRIORITY_USER_KEEP {
        1.0
    } else {
        rate
    }
}

/// The decaying events-per-second budget.
///
/// The budget keeps a score that gains one point per event decided and loses 12.5% per second,
/// making it a smoothed estimate of the recent event rate rather than a windowed count. Events of
/// manually kept traces bypass the budget and do not count against it, so they cannot squeeze
/// other events out of the budget.
struct MaxEpsLimiter {
    max_eps: f64,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    score: f64,
    last_decay: Instant,
}

impl MaxEpsLimiter {
    fn new(max_eps: f64) -> Self {
        Self {
            max_eps,
            state: Mutex::new(LimiterState {
                score: 0.0,
                last_decay: Instant::now(),
            }),
        }
    }

    /// Decides whether an event fits the budget, returning the applied rate.
    ///
    /// The event counts against the budget before the rate is computed, so the estimate includes
    /// the event being decided.
    fn sample(&self, trace_id: u64, priority: i32) -> (bool, f64) {
        if priority >= PRIORITY_USER_KEEP {
            return (true, 1.0);
        }

        let mut state = self.state.lock().unwrap();
        state.decay();
        state.score += 1.0;
        let rate = self.sample_rate_for(state.score);
        (sample_by_rate(trace_id, rate), rate)
    }

    /// Returns the rates reported by the budget gauges.
    fn snapshot(&self) -> BudgetSnapshot {
        let mut state = self.state.lock().unwrap();
        state.decay();
        let sample_rate = self.sample_rate_for(state.score);
        BudgetSnapshot {
            max_rate: self.max_eps,
            current_rate: (state.score / COUNT_SCALE_FACTOR) * DECAY_FACTOR,
            sample_rate,
        }
    }

    /// Rate applied at the current score, capped by the budget.
    fn sample_rate_for(&self, score: f64) -> f64 {
        let current_rate = (score / COUNT_SCALE_FACTOR) * DECAY_FACTOR;
        if current_rate > self.max_eps {
            self.max_eps / current_rate
        } else {
            1.0
        }
    }
}

impl LimiterState {
    /// Decays the score by the whole seconds elapsed since the last step.
    fn decay(&mut self) {
        let elapsed = self.last_decay.elapsed();
        let ticks = elapsed.as_secs();
        if ticks > 0 {
            self.score /= DECAY_FACTOR.powi(ticks.min(u32::MAX as u64) as i32);
            self.last_decay += Duration::from_secs(ticks);
        }
    }
}

/// The budget rates at report time.
struct BudgetSnapshot {
    max_rate: f64,
    current_rate: f64,
    sample_rate: f64,
}

/// The event metrics.
///
/// Extracted and sampled counts are window deltas, accumulated between reports and reset on each
/// one; the budget gauges report the current state every window.
#[derive(Default)]
struct EventTelemetry {
    extracted_delta: AtomicU64,
    sampled_delta: AtomicU64,
}

impl EventTelemetry {
    fn record_extracted(&self, extracted: u64) {
        self.extracted_delta.fetch_add(extracted, Ordering::Relaxed);
    }

    fn record_sampled(&self, sampled: u64) {
        self.sampled_delta.fetch_add(sampled, Ordering::Relaxed);
    }

    /// Reports the window deltas and the budget gauges as metric events.
    fn take_window_events(&self, limiter: &MaxEpsLimiter) -> Vec<Event> {
        let mut events = Vec::new();

        let extracted = self.extracted_delta.swap(0, Ordering::Relaxed);
        if extracted > 0 {
            events.push(counter_event(METRIC_EVENTS_EXTRACTED, extracted));
        }
        let sampled = self.sampled_delta.swap(0, Ordering::Relaxed);
        if sampled > 0 {
            events.push(counter_event(METRIC_EVENTS_SAMPLED, sampled));
        }

        let snapshot = limiter.snapshot();
        events.push(gauge_event(METRIC_MAX_EPS_MAX_RATE, snapshot.max_rate));
        events.push(gauge_event(METRIC_MAX_EPS_CURRENT_RATE, snapshot.current_rate));
        events.push(gauge_event(METRIC_MAX_EPS_SAMPLE_RATE, snapshot.sample_rate));
        if snapshot.sample_rate < 1.0 {
            events.push(gauge_event(METRIC_MAX_EPS_REACHED_MAX, 1.0));
            warn!(
                "Max events per second reached (current={:.2}/s, max={:.2}/s). Some events are now being dropped \
                 (sample rate={:.2}). Consider adjusting event sampling rates.",
                snapshot.current_rate, snapshot.max_rate, snapshot.sample_rate
            );
        } else {
            events.push(gauge_event(METRIC_MAX_EPS_REACHED_MAX, 0.0));
        }

        events
    }
}

fn counter_event(name: &str, value: u64) -> Event {
    Event::Metric(Metric::counter(
        Context::from_parts(name, TagSet::default()),
        value as f64,
    ))
}

fn gauge_event(name: &str, value: f64) -> Event {
    Event::Metric(Metric::gauge(Context::from_parts(name, TagSet::default()), value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top_level_span(id: u64) -> Span {
        let mut span = Span::new("checkout", "request", "/checkout", "web", id, 0, 1, 100, 0);
        span.attributes
            .insert(MetaString::from_static(KEY_TOP_LEVEL), AttributeValue::Float(1.0));
        span
    }

    fn map_of(entries: &[(&'static str, f64)]) -> HashMap<String, f64> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn metric_extractor_nominates_rate_carrying_spans() {
        let mut span = top_level_span(1);
        span.attributes.insert(
            MetaString::from_static(KEY_EVENT_EXTRACTION_RATE),
            AttributeValue::Float(0.5),
        );
        let processor = EventProcessor::for_tests(&HashMap::new(), &HashMap::new(), 200.0);
        let extractor = &processor.inner.extractors[0];

        assert_eq!(extractor.extract(&span, 0), Some(0.5));

        // Manually kept traces upscale positive rates so every event is extracted.
        assert_eq!(extractor.extract(&span, PRIORITY_USER_KEEP), Some(1.0));

        // Spans without the key are not nominated.
        let plain = top_level_span(2);
        assert_eq!(extractor.extract(&plain, 0), None);
    }

    #[test]
    fn fixed_rate_extractor_matches_service_and_operation_case_insensitively() {
        let rates = HashMap::from([("checkout".to_string(), map_of(&[("request", 0.75)]))]);
        let processor = EventProcessor::for_tests(&rates, &HashMap::new(), 200.0);
        let extractor = &processor.inner.extractors[1];

        // Mixed-case span identity matches the lowercase config.
        let mut span = top_level_span(1);
        span = span.with_service("Checkout").with_name("Request");
        assert_eq!(extractor.extract(&span, 0), Some(0.75));

        // Unknown operation or service is not nominated.
        let other = Span::new("checkout", "other", "r", "web", 2, 0, 1, 100, 0);
        assert_eq!(extractor.extract(&other, 0), None);
        let other = Span::new("payments", "request", "r", "web", 3, 0, 1, 100, 0);
        assert_eq!(extractor.extract(&other, 0), None);
    }

    #[test]
    fn legacy_extractor_nominates_top_level_spans_of_configured_services() {
        let rates = map_of(&[("checkout", 0.25)]);
        let processor = EventProcessor::for_tests(&HashMap::new(), &rates, 200.0);
        let extractor = &processor.inner.extractors[1];

        assert_eq!(extractor.extract(&top_level_span(1), 0), Some(0.25));

        // Non-top-level spans are never nominated.
        let child = Span::new("checkout", "db", "r", "web", 2, 1, 1, 100, 0);
        assert_eq!(extractor.extract(&child, 0), None);

        // Unknown services are not nominated.
        let mut other = Span::new("payments", "db", "r", "web", 3, 0, 1, 100, 0);
        other
            .attributes
            .insert(MetaString::from_static(KEY_TOP_LEVEL), AttributeValue::Float(1.0));
        assert_eq!(extractor.extract(&other, 0), None);
    }

    #[test]
    fn fixed_rate_takes_precedence_over_legacy_when_both_configured() {
        let legacy = map_of(&[("checkout", 0.25)]);
        let processor = EventProcessor::for_tests(&HashMap::new(), &legacy, 200.0);
        // Only the metric extractor plus the legacy map are active.
        assert_eq!(processor.inner.extractors.len(), 2);
        assert!(matches!(processor.inner.extractors[1], Extractor::Legacy(_)));

        let fixed = HashMap::from([("checkout".to_string(), map_of(&[("request", 0.5)]))]);
        let processor = EventProcessor::for_tests(&fixed, &legacy, 200.0);
        // The legacy map is ignored when fixed rates are configured.
        assert_eq!(processor.inner.extractors.len(), 2);
        assert!(matches!(processor.inner.extractors[1], Extractor::FixedRate(_)));
    }

    #[test]
    fn process_marks_survivors_with_metadata() {
        let rates = map_of(&[("checkout", 1.0)]);
        let processor = EventProcessor::for_tests(&HashMap::new(), &rates, 1_000_000_000.0);

        let root = top_level_span(1);
        let child = Span::new("checkout", "child", "r", "web", 2, 1, 1, 100, 0);
        let mut trace = Trace::new(vec![root, child]);

        let counts = processor.process(&mut trace, Some(0), 0);
        assert_eq!(counts.extracted, 1);
        assert_eq!(counts.sampled, 1);

        // The root span survived: it is marked analyzed, and no rate keys are written at 1.0.
        let root = trace.spans().first().unwrap();
        assert_eq!(
            root.attributes.get(KEY_ANALYZED_SPANS).and_then(AttributeValue::as_num),
            Some(1.0)
        );
        assert!(!root.attributes.contains_key(KEY_EVENT_EXTRACTION_RATE));
        assert!(!root.attributes.contains_key(KEY_MAX_EPS_RATE));
    }

    #[test]
    fn extraction_rate_zero_extracts_but_never_samples() {
        let rates = map_of(&[("checkout", 0.0)]);
        let processor = EventProcessor::for_tests(&HashMap::new(), &rates, 200.0);

        let mut trace = Trace::new(vec![top_level_span(1)]);
        let counts = processor.process(&mut trace, Some(0), 0);
        // The extractor nominated the span, but the rate flip never keeps at rate 0.
        assert_eq!(counts, EventCounts::default());
        let root = trace.spans().first().unwrap();
        assert!(!root.attributes.contains_key(KEY_ANALYZED_SPANS));
    }

    #[test]
    fn budget_caps_survivors_of_a_burst() {
        // With a budget of 1 and no decay elapsed, the estimate reaches the budget once the
        // score holds 8 events: (8/9)*1.125 = 1.0.
        let limiter = MaxEpsLimiter::new(1.0);

        // The first eight events fit at rate 1.
        for id in 1..=8 {
            let (sampled, rate) = limiter.sample(id, 0);
            assert!(sampled);
            assert_eq!(rate, 1.0);
        }

        // From the ninth event on, the estimate exceeds the budget and the rate drops below 1.
        for id in 9..=11 {
            let (_, rate) = limiter.sample(id, 0);
            assert!(rate < 1.0, "expected a capped rate, got {rate}");
        }
    }

    #[test]
    fn user_keep_bypasses_the_budget_without_counting() {
        let limiter = MaxEpsLimiter::new(1.0);
        for id in 1..=10 {
            let (sampled, rate) = limiter.sample(id, PRIORITY_USER_KEEP);
            assert!(sampled);
            assert_eq!(rate, 1.0);
        }
        // Bypassed events do not count: the next non-user-keep event still sees rate 1.
        let (_, rate) = limiter.sample(11, 0);
        assert_eq!(rate, 1.0);
    }

    #[test]
    fn budget_gauges_report_the_capped_state() {
        let limiter = MaxEpsLimiter::new(1.0);
        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.max_rate, 1.0);
        assert_eq!(snapshot.current_rate, 0.0);
        assert_eq!(snapshot.sample_rate, 1.0);
    }
}
