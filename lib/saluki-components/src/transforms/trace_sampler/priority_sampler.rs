//! Priority sampler for trace sampling based on service rates.
#![allow(dead_code)]
use std::time::SystemTime;

use saluki_core::data_model::event::trace::{Span, Trace};
use stringtheory::MetaString;

use super::{
    catalog::ServiceKeyCatalog,
    core_sampler::Sampler,
    signature::{ServiceSignature, Signature},
    PRIORITY_AUTO_DROP, PRIORITY_AUTO_KEEP, PRIORITY_USER_KEEP,
};
use crate::common::datadog::get_trace_env;

const DEPRECATED_RATE_KEY: &str = "_sampling_priority_rate_v1";
const KEY_SAMPLING_RATE_GLOBAL: &str = "_sample_rate";
const KEY_SAMPLING_RATE_PRE_SAMPLER: &str = "_dd1.sr.rapre";

/// Priority sampler for traces with sampling priority set by the tracer.
pub struct PrioritySampler {
    agent_env: MetaString,
    sampler: Sampler,
    catalog: ServiceKeyCatalog,
}
// the logic for this class is taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/prioritysampler.go#L39
// note that any logic involving tracers were removed because ADP does not currently support tracers.
impl PrioritySampler {
    /// Creates a new priority sampler with the given configuration.
    pub(super) fn new(agent_env: MetaString, extra_sample_rate: f64, target_tps: f64) -> Self {
        PrioritySampler {
            agent_env,
            sampler: Sampler::new(extra_sample_rate, target_tps),
            catalog: ServiceKeyCatalog::new(),
        }
    }

    /// Updates the target traces per second.
    pub(super) fn update_target_tps(&mut self, target_tps: f64) {
        self.sampler.update_target_tps(target_tps);
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

        let sampler_env = to_sampler_env(tracer_env, self.agent_env.as_ref());
        let svc_sig = ServiceSignature::new(service_name, sampler_env);
        let signature = self.catalog.register(svc_sig);

        let _ = self.sampler.count_weighted_sig(now, &signature, weight);

        if sampled {
            self.apply_rate(trace, root_idx, &signature);
        }

        sampled
    }

    fn apply_rate(&self, trace: &mut Trace, root_idx: usize, signature: &Signature) -> f64 {
        let root = &mut trace.spans_mut()[root_idx];
        if root.parent_id() != 0 {
            return 1.0;
        }

        // ignore the tracer specific logic

        let rate = self.sampler.get_signature_sample_rate(signature);
        root.metrics_mut()
            .insert(MetaString::from_static(DEPRECATED_RATE_KEY), rate);
        rate
    }
}

fn to_sampler_env(tracer_env: &str, agent_env: &str) -> String {
    if tracer_env.is_empty() {
        agent_env.to_string()
    } else {
        tracer_env.to_string()
    }
}

fn weight_root(span: &Span) -> f32 {
    let client_rate = span
        .metrics()
        .get(&MetaString::from_static(KEY_SAMPLING_RATE_GLOBAL))
        .copied()
        .filter(|&r| r > 0.0 && r <= 1.0)
        .unwrap_or(1.0);

    let pre_sampler_rate = span
        .metrics()
        .get(&MetaString::from_static(KEY_SAMPLING_RATE_PRE_SAMPLER))
        .copied()
        .filter(|&r| r > 0.0 && r <= 1.0)
        .unwrap_or(1.0);

    (1.0 / (pre_sampler_rate * client_rate)) as f32
}

#[cfg(test)]
mod tests {
    // logic for these tests are taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/prioritysampler_test.go
    use std::time::{Duration, SystemTime};

    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::{Span, Trace};
    use stringtheory::MetaString;

    use super::*;
    use crate::transforms::trace_sampler::signature::ServiceSignature;

    const BUCKET_DURATION: Duration = Duration::from_secs(5);
    const PRIORITY_USER_DROP: i32 = -1;

    fn get_test_priority_sampler(target_tps: f64) -> PrioritySampler {
        PrioritySampler::new(MetaString::from("agent-env"), 1.0, target_tps)
    }

    fn get_test_trace_with_service(service: &str, trace_id: u64) -> (Trace, usize) {
        let root = Span::new(
            MetaString::from(service),
            MetaString::from("root-operation"),
            MetaString::from("root-resource"),
            MetaString::from("web"),
            trace_id,
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
            trace_id,
            2,      // span_id
            1,      // parent_id
            100,    // start
            200000, // duration
            0,      // error
        );

        let trace = Trace::new(vec![root, child], TagSet::default());
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
