//! Priority sampler for trace sampling based on service rates.
//!
//! As the trace-agent's priority sampler, it keeps the decision the tracer made, and feeds the auto-priority traces
//! into a TPS feedback loop whose rates, by service and environment, it publishes to the [`ServiceRates`] the trace
//! intake answers tracers with.
#![allow(dead_code)]
use std::time::SystemTime;

#[cfg(test)]
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, Trace};
use stringtheory::MetaString;

use super::{
    catalog::ServiceKeyCatalog,
    core_sampler::Sampler,
    score_sampler::weight_root,
    signature::{ServiceSignature, Signature},
    PRIORITY_AUTO_DROP, PRIORITY_AUTO_KEEP, PRIORITY_USER_KEEP,
};
use crate::common::datadog::{as_double_value, get_trace_env};
use crate::common::traces::rates::{RateByService, ServiceRates};

const DEPRECATED_RATE_KEY: &str = "_sampling_priority_rate_v1";
const AGENT_RATE_KEY: &str = "_dd.agent_psr";
const RULE_RATE_KEY: &str = "_dd.rule_psr";

/// Priority sampler for traces with sampling priority set by the tracer.
pub struct PrioritySampler {
    agent_env: MetaString,
    sampler: Sampler,
    catalog: ServiceKeyCatalog,
    rate_by_service: RateByService,
}
// the logic for this class is taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/prioritysampler.go#L39
impl PrioritySampler {
    /// Creates a new priority sampler with the given configuration.
    pub(super) fn new(
        agent_env: MetaString, extra_sample_rate: f64, target_tps: f64, max_catalog_entries: usize,
        service_rates: ServiceRates,
    ) -> Self {
        PrioritySampler {
            agent_env,
            sampler: Sampler::new(extra_sample_rate, target_tps),
            catalog: ServiceKeyCatalog::with_max_entries(max_catalog_entries),
            rate_by_service: RateByService::new(service_rates),
        }
    }

    /// Updates the target traces per second.
    pub(super) fn update_target_tps(&mut self, target_tps: f64) {
        self.sampler.update_target_tps(target_tps);
    }

    /// Returns the number of service signatures tracked by the underlying sampler.
    pub(super) fn tracked_signature_count(&self) -> u64 {
        self.sampler.size() as u64
    }

    /// Returns the current target traces per second.
    pub(super) fn get_target_tps(&self) -> f64 {
        self.sampler.target_tps()
    }

    /// Sample a trace that already has a sampling priority set.
    ///
    /// The decision is based on the priority value; the sampler only updates
    /// feedback rates for auto-priority traces.
    pub(super) fn sample(
        &mut self, now: SystemTime, trace: &mut Trace, root_idx: usize, priority: i32, client_dropped_p0s_weight: f64,
    ) -> bool {
        if trace.spans().is_empty() || root_idx >= trace.spans().len() {
            return false;
        }

        let sampled = priority == PRIORITY_AUTO_KEEP || priority == PRIORITY_USER_KEEP;

        // Only auto-priority traces (0 or 1) participate in the feedback loop.
        if !(PRIORITY_AUTO_DROP..=PRIORITY_AUTO_KEEP).contains(&priority) {
            return sampled;
        }

        let (service_name, tracer_env, weight) = {
            let root = &trace.spans()[root_idx];
            let tracer_env = get_trace_env(trace, root_idx).map(|env| env.as_ref()).unwrap_or("");
            let weight = weight_root(root) + client_dropped_p0s_weight as f32;
            (root.service(), tracer_env, weight)
        };

        let sampler_env = to_sampler_env(tracer_env, &self.agent_env);
        let svc_sig = ServiceSignature::new(service_name, sampler_env);
        let signature = self.catalog.register(svc_sig);

        if self.sampler.count_weighted_sig(now, &signature, weight) {
            self.update_rates();
        }

        if sampled {
            self.apply_rate(trace, root_idx, &signature);
        }

        sampled
    }

    /// Publishes the rates of every cataloged service, as the trace-agent does.
    fn update_rates(&mut self) {
        let (rates, default_rate) = self.sampler.get_all_signature_sample_rates();
        let by_service = self.catalog.rates_by_service(&self.agent_env, &rates, default_rate);
        self.rate_by_service.set_all(
            by_service
                .into_iter()
                .map(|(service, rate)| (service.to_string(), rate)),
        );
    }

    fn apply_rate(&self, trace: &mut Trace, root_idx: usize, signature: &Signature) -> f64 {
        let root = &mut trace.spans_mut()[root_idx];
        if root.parent_id() != 0 {
            return 1.0;
        }

        // A rate the tracer already applied wins, as in the trace-agent.
        for key in [AGENT_RATE_KEY, RULE_RATE_KEY, DEPRECATED_RATE_KEY] {
            if let Some(rate) = root.attributes.get(key).and_then(as_double_value) {
                return rate;
            }
        }

        let rate = self.sampler.get_signature_sample_rate(signature);
        root.attributes.insert(
            MetaString::from_static(DEPRECATED_RATE_KEY),
            AttributeValue::Float(rate),
        );
        rate
    }
}

fn to_sampler_env(tracer_env: &str, agent_env: &MetaString) -> MetaString {
    if tracer_env.is_empty() {
        agent_env.clone()
    } else {
        MetaString::from(tracer_env)
    }
}

#[cfg(test)]
impl PrioritySampler {
    pub(super) fn test_signature_sample_rates(&self) -> (FastHashMap<Signature, f64>, f64) {
        self.sampler.get_all_signature_sample_rates()
    }

    pub(super) fn test_size(&self) -> i64 {
        self.sampler.size()
    }

    pub(super) fn test_total_seen_weight(&self) -> f32 {
        self.sampler.total_seen_weight()
    }
}

#[cfg(test)]
mod tests {
    // logic for these tests are taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/prioritysampler_test.go
    use std::time::{Duration, SystemTime};

    use saluki_core::data_model::event::trace::{Span, Trace, TraceIngestInfo};
    use stringtheory::MetaString;

    use super::*;
    use crate::transforms::trace_sampler::signature::ServiceSignature;

    const BUCKET_DURATION: Duration = Duration::from_secs(5);
    const PRIORITY_USER_DROP: i32 = -1;

    fn get_test_priority_sampler(target_tps: f64) -> PrioritySampler {
        PrioritySampler::new(
            MetaString::from("agent-env"),
            1.0,
            target_tps,
            5000,
            ServiceRates::default(),
        )
    }

    fn get_test_trace_with_service(service: &str, trace_id: u64) -> (Trace, usize) {
        let root = Span::new(
            MetaString::from(service),
            MetaString::from("root-operation"),
            MetaString::from("root-resource"),
            MetaString::from("web"),
            1,       // span_id
            0,       // parent_id
            42,      // start
            1000000, // duration
            0,       // error
        );

        let child = Span::new(
            MetaString::from(service),
            MetaString::from("child-operation"),
            MetaString::from("child-resource"),
            MetaString::from("sql"),
            2,      // span_id
            1,      // parent_id
            100,    // start
            200000, // duration
            0,      // error
        );

        let mut trace = Trace::new(vec![root, child], TraceIngestInfo::Datadog);
        trace.trace_id_low = trace_id;
        (trace, 0)
    }

    #[test]
    fn test_priority_sample() {
        let test_cases = [
            (PRIORITY_USER_DROP, false), // user drop
            (PRIORITY_AUTO_DROP, false), // auto drop
            (PRIORITY_AUTO_KEEP, true),  // auto keep
            (PRIORITY_USER_KEEP, true),  // user keep
        ];

        for (idx, (priority, expected_sampled)) in test_cases.iter().copied().enumerate() {
            let mut sampler = get_test_priority_sampler(0.0);
            let (mut trace, root_idx) = get_test_trace_with_service("service-a", idx as u64 + 1);
            let sampled = sampler.sample(SystemTime::now(), &mut trace, root_idx, priority, 0.0);
            assert_eq!(
                sampled, expected_sampled,
                "priority {} should sample={}",
                priority, expected_sampled
            );
        }
    }

    fn published_body(rates: &ServiceRates) -> String {
        String::from_utf8(rates.load().body.to_vec()).unwrap()
    }

    // Rates are published each time the sliding window moves.
    #[test]
    fn rates_are_published_when_the_window_moves() {
        let rates = ServiceRates::default();
        let mut sampler = PrioritySampler::new(MetaString::from("agent-env"), 1.0, 10.0, 5000, rates.clone());
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        let (mut trace, root_idx) = get_test_trace_with_service("web", 1);
        sampler.sample(start, &mut trace, root_idx, PRIORITY_AUTO_KEEP, 0.0);
        // As in the trace-agent, the window first moves before anything is counted, so only the default is known.
        let first = rates.load().version.clone();
        assert_ne!(first, "");
        assert_eq!(published_body(&rates), "{\"rate_by_service\":{\"service:,env:\":1}}\n");

        let (mut trace, root_idx) = get_test_trace_with_service("web", 2);
        sampler.sample(start + BUCKET_DURATION, &mut trace, root_idx, PRIORITY_AUTO_KEEP, 0.0);
        assert_ne!(rates.load().version, first);
        // The service runs in the agent's environment, so it is also published under an empty one.
        assert_eq!(
            published_body(&rates),
            "{\"rate_by_service\":{\"service:,env:\":1,\"service:web,env:\":1,\"service:web,env:agent-env\":1}}\n"
        );
    }

    #[test]
    fn manual_priorities_are_not_published() {
        let rates = ServiceRates::default();
        let mut sampler = PrioritySampler::new(MetaString::from("agent-env"), 1.0, 10.0, 5000, rates.clone());
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for (i, priority) in [PRIORITY_USER_KEEP, PRIORITY_USER_DROP].into_iter().enumerate() {
            let (mut trace, root_idx) = get_test_trace_with_service("manual", i as u64);
            sampler.sample(start + BUCKET_DURATION * i as u32, &mut trace, root_idx, priority, 0.0);
        }
        assert!(
            sampler
                .catalog
                .rates_by_service("agent-env", &FastHashMap::default(), 0.0)
                .len()
                == 1
        );
    }

    #[test]
    fn a_rate_the_tracer_applied_is_kept() {
        for key in [AGENT_RATE_KEY, RULE_RATE_KEY, DEPRECATED_RATE_KEY] {
            let mut sampler = get_test_priority_sampler(0.0);
            let (mut trace, root_idx) = get_test_trace_with_service("web", 1);
            trace.spans_mut()[root_idx]
                .attributes
                .insert(MetaString::from(key), AttributeValue::Float(0.25));

            sampler.sample(SystemTime::now(), &mut trace, root_idx, PRIORITY_AUTO_KEEP, 0.0);

            let attributes = &trace.spans()[root_idx].attributes;
            let expected_deprecated = (key == DEPRECATED_RATE_KEY).then_some(&AttributeValue::Float(0.25));
            assert_eq!(attributes.get(DEPRECATED_RATE_KEY), expected_deprecated, "{key}");
        }
    }

    #[test]
    fn test_priority_sampler_tps_feedback_loop() {
        struct TestCase {
            target_tps: f64,
            generated_tps: f64,
            service: &'static str,
            expected_tps: f64,
            relative_error: f64,
        }

        let test_cases = [
            TestCase {
                target_tps: 5.0,
                generated_tps: 50.0,
                expected_tps: 5.0,
                relative_error: 0.25,
                service: "bim",
            },
            TestCase {
                target_tps: 3.0,
                generated_tps: 200.0,
                expected_tps: 3.0,
                relative_error: 0.25,
                service: "2",
            },
            TestCase {
                target_tps: 10.0,
                generated_tps: 10.0,
                expected_tps: 10.0,
                relative_error: 0.03,
                service: "4",
            },
            TestCase {
                target_tps: 10.0,
                generated_tps: 3.0,
                expected_tps: 3.0,
                relative_error: 0.03,
                service: "10",
            },
            TestCase {
                target_tps: 0.5,
                generated_tps: 100.0,
                expected_tps: 0.5,
                relative_error: 0.6,
                service: "0.5",
            },
        ];

        for tc in test_cases {
            let mut sampler = get_test_priority_sampler(tc.target_tps);
            let signature = ServiceSignature::new(tc.service, "agent-env").hash();
            let expected_rate = tc.expected_tps / tc.generated_tps;

            let warm_up_duration = 5;
            let test_duration = 20;
            let mut test_time = SystemTime::now();

            let mut sampled_count = 0;
            let mut handled_count = 0;

            for time_elapsed in 0..(warm_up_duration + test_duration) {
                let traces_per_period = (tc.generated_tps * BUCKET_DURATION.as_secs_f64()) as usize;
                test_time += BUCKET_DURATION;

                for i in 0..traces_per_period {
                    let trace_id = (time_elapsed as u64) << 32 | i as u64;
                    let (mut trace, root_idx) = get_test_trace_with_service(tc.service, trace_id);
                    let sampled = sampler.sample(test_time, &mut trace, root_idx, PRIORITY_AUTO_KEEP, 0.0);

                    if time_elapsed < warm_up_duration {
                        continue;
                    }

                    let rate = sampler.sampler.get_signature_sample_rate(&signature);
                    assert!(
                        (rate - expected_rate).abs() <= expected_rate * tc.relative_error,
                        "rate mismatch for service {}: got {}, want {}",
                        tc.service,
                        rate,
                        expected_rate
                    );

                    handled_count += 1;
                    if sampled {
                        sampled_count += 1;
                    }
                }
            }

            assert_eq!(
                sampled_count, handled_count,
                "auto-keep priority should sample every handled trace"
            );
        }
    }
}
