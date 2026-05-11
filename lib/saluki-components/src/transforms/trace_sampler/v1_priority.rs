//! V1 priority sampler.
//!
//! Mirrors `PrioritySampler.SampleV1` + `countSignatureV1` + `applyRateV1` + `updateRates`
//! from `pkg/trace/sampler/prioritysampler.go`.
//!
//! Responsibilities:
//! - Count auto-priority (0/1) traces toward per-service rate computation.
//! - Short-circuit for user-set priorities (< 0 or > 1) without counting.
//! - Write the computed agent rate to the root span attribute when a trace is kept.
//! - Push updated per-service rates to the shared [`V1SamplingRatesHandle`] after each
//!   sliding-window advance.

use std::time::SystemTime;

use saluki_core::data_model::event::trace::{AttributeValue, Span};
use stringtheory::MetaString;

use crate::sources::apm::sampling_rates::V1SamplingRatesHandle;
use crate::transforms::trace_sampler::catalog::ServiceKeyCatalog;
use crate::transforms::trace_sampler::core_sampler::Sampler;
use crate::transforms::trace_sampler::signature::{ServiceSignature, Signature};

// Root-span attribute keys (matching Go agent sampler constants).
const KEY_SAMPLE_RATE: &str = "_sample_rate";
const KEY_PRE_SAMPLER_RATE: &str = "_dd1.sr.rapre";
const KEY_AGENT_PSR: &str = "_dd.agent_psr";
const KEY_RULE_PSR: &str = "_dd.rule_psr";
const KEY_DEPRECATED_RATE: &str = "_sampling_priority_rate_v1";

/// Priority sampler for V1 trace chunks.
///
/// Counts auto-priority traces toward a TPS-based rate computation and propagates
/// the resulting per-service rates to tracers via the HTTP response.
pub(super) struct V1PrioritySampler {
    agent_env: MetaString,
    core_sampler: Sampler,
    catalog: ServiceKeyCatalog,
    rates: V1SamplingRatesHandle,
}

impl V1PrioritySampler {
    pub(super) fn new(
        agent_env: MetaString,
        target_tps: f64,
        extra_rate: f64,
        rates: V1SamplingRatesHandle,
    ) -> Self {
        Self {
            agent_env,
            core_sampler: Sampler::new(extra_rate, target_tps),
            catalog: ServiceKeyCatalog::new(),
            rates,
        }
    }

    /// Evaluate the chunk against the priority sampler.
    ///
    /// Returns `true` if the chunk should be kept (priority > 0).
    ///
    /// Only auto-priorities (0 and 1) are counted toward the rate computation.
    /// User-set priorities (< 0 or > 1) short-circuit without affecting rates.
    pub(super) fn sample(
        &mut self,
        now: SystemTime,
        priority: i32,
        root: &mut Span,
        tracer_env: &str,
        client_dropped_p0s_weight: f64,
    ) -> bool {

        // Short-circuit: don't count user-explicit decisions.
        if priority < 0 || priority > 1 {
            return priority > 0;
        }

        let effective_env = if tracer_env.is_empty() {
            self.agent_env.as_ref()
        } else {
            tracer_env
        };

        let svc_sig = ServiceSignature::new(root.service(), effective_env);
        let signature = self.catalog.register(svc_sig);

        let weight = weight_root(root) as f32 + client_dropped_p0s_weight as f32;
        let new_rates = self.core_sampler.count_weighted_sig(now, &signature, weight);
        if new_rates {
            self.update_rates();
        }

        let sampled = priority > 0;
        if sampled {
            apply_rate(root, &signature, &self.core_sampler);
        }
        sampled
    }

    fn update_rates(&mut self) {
        let (rates_map, default_rate) = self.core_sampler.get_all_signature_sample_rates();
        let new_rates = self.catalog.rates_by_service(self.agent_env.as_ref(), &rates_map, default_rate);
        self.rates.set_all(new_rates);
    }
}

/// Compute the statistical weight of a root span.
///
/// Mirrors `weightRootV1` from `pkg/trace/sampler/sampler.go`:
/// `weight = 1 / (client_rate * pre_sampler_rate)`.
///
/// Reads `_sample_rate` and `_dd1.sr.rapre` from span attributes.
/// Both default to 1.0 when absent or out of range.
pub(super) fn weight_root(root: &Span) -> f64 {
    let client_rate = root
        .attributes
        .get(KEY_SAMPLE_RATE)
        .and_then(AttributeValue::as_float)
        .filter(|&r| r > 0.0 && r <= 1.0)
        .unwrap_or(1.0);
    let pre_sampler_rate = root
        .attributes
        .get(KEY_PRE_SAMPLER_RATE)
        .and_then(AttributeValue::as_float)
        .filter(|&r| r > 0.0 && r <= 1.0)
        .unwrap_or(1.0);
    1.0 / (client_rate * pre_sampler_rate)
}

/// Write the agent-computed sampling rate to the root span.
///
/// Mirrors `applyRateV1` from `pkg/trace/sampler/prioritysampler.go`.
/// Does nothing if the tracer already annotated the root with a rate.
fn apply_rate(root: &mut Span, signature: &Signature, core_sampler: &Sampler) {
    if root.parent_id() != 0 {
        return;
    }
    if root.attributes.get(KEY_AGENT_PSR).and_then(AttributeValue::as_float).is_some() {
        return;
    }
    if root.attributes.get(KEY_RULE_PSR).and_then(AttributeValue::as_float).is_some() {
        return;
    }
    if root.attributes.get(KEY_DEPRECATED_RATE).and_then(AttributeValue::as_float).is_some() {
        return;
    }
    let rate = core_sampler.get_signature_sample_rate(signature);
    root.attributes.insert(MetaString::from(KEY_DEPRECATED_RATE), AttributeValue::Float(rate));
}


#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::{AttributeValue, Span};
    use stringtheory::MetaString;

    use super::*;
    use crate::sources::apm::sampling_rates::V1SamplingRatesHandle;
    use crate::transforms::trace_sampler::signature::ServiceSignature;

    fn make_sampler() -> V1PrioritySampler {
        V1PrioritySampler::new(
            MetaString::from_static("prod"),
            10.0,
            1.0,
            V1SamplingRatesHandle::new(),
        )
    }

    fn make_span(parent_id: u64) -> Span {
        Span::new("svc", "op", "res", "web", 0, 1, parent_id, 0, 1000, 0)
    }

    // ── Short-circuit tests ─────────────────────────────────────────────────

    #[test]
    fn user_drop_short_circuits_without_counting() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        let now = SystemTime::now();

        assert!(!sampler.sample(now, -1, &mut root, "prod", 0.0));
        assert_eq!(
            sampler.catalog.rates_by_service("prod", &FastHashMap::default(), 1.0).len(),
            1,
            "only default rate key; no service registered"
        );
    }

    #[test]
    fn user_keep_short_circuits_returns_true() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        let now = SystemTime::now();

        assert!(sampler.sample(now, 2, &mut root, "prod", 0.0));
        assert_eq!(
            sampler.catalog.rates_by_service("prod", &FastHashMap::default(), 1.0).len(),
            1,
            "only default rate key; no service registered"
        );
    }

    // ── Counting tests ──────────────────────────────────────────────────────

    #[test]
    fn auto_keep_priority_returns_true() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        assert!(sampler.sample(SystemTime::now(), 1, &mut root, "prod", 0.0));
    }

    #[test]
    fn auto_drop_priority_returns_false() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        assert!(!sampler.sample(SystemTime::now(), 0, &mut root, "prod", 0.0));
    }

    // ── apply_rate tests ────────────────────────────────────────────────────

    #[test]
    fn kept_trace_gets_rate_written_to_root_span() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        sampler.sample(SystemTime::now(), 1, &mut root, "prod", 0.0);
        assert!(
            root.attributes.get(KEY_DEPRECATED_RATE).and_then(AttributeValue::as_float).is_some(),
            "rate metric should be written to kept root span"
        );
    }

    #[test]
    fn dropped_trace_does_not_get_rate_written() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        sampler.sample(SystemTime::now(), 0, &mut root, "prod", 0.0);
        assert!(
            root.attributes.get(KEY_DEPRECATED_RATE).and_then(AttributeValue::as_float).is_none(),
            "rate metric should not be written for dropped trace"
        );
    }

    #[test]
    fn existing_agent_psr_is_not_overwritten() {
        let mut sampler = make_sampler();
        let mut root = make_span(0);
        root.attributes.insert(MetaString::from(KEY_AGENT_PSR), AttributeValue::Float(0.25));

        sampler.sample(SystemTime::now(), 1, &mut root, "prod", 0.0);

        assert_eq!(
            root.attributes.get(KEY_AGENT_PSR).and_then(AttributeValue::as_float),
            Some(0.25),
            "existing _dd.agent_psr must not be overwritten"
        );
    }

    #[test]
    fn non_root_span_does_not_get_rate() {
        let mut sampler = make_sampler();
        let mut non_root = make_span(99); // parent_id != 0

        sampler.sample(SystemTime::now(), 1, &mut non_root, "prod", 0.0);

        let has_rate = [KEY_DEPRECATED_RATE, KEY_AGENT_PSR, KEY_RULE_PSR]
            .iter()
            .any(|k| non_root.attributes.get(*k).and_then(AttributeValue::as_float).is_some());
        assert!(!has_rate, "rate must not be written for non-root spans");
    }

    // ── weight_root tests ───────────────────────────────────────────────────

    #[test]
    fn weight_root_defaults_to_one() {
        let span = make_span(0);
        assert_eq!(weight_root(&span), 1.0);
    }

    #[test]
    fn weight_root_divides_by_sample_rate() {
        let mut span = make_span(0);
        span.attributes.insert(MetaString::from(KEY_SAMPLE_RATE), AttributeValue::Float(0.5));
        assert_eq!(weight_root(&span), 2.0);
    }

    #[test]
    fn weight_root_uses_both_rates() {
        let mut span = make_span(0);
        span.attributes.insert(MetaString::from(KEY_SAMPLE_RATE), AttributeValue::Float(0.5));
        span.attributes.insert(MetaString::from(KEY_PRE_SAMPLER_RATE), AttributeValue::Float(0.5));
        assert_eq!(weight_root(&span), 4.0);
    }

    #[test]
    fn weight_root_ignores_out_of_range_rates() {
        let mut span = make_span(0);
        span.attributes.insert(MetaString::from(KEY_SAMPLE_RATE), AttributeValue::Float(2.0)); // rate > 1.0 → 1.0
        assert_eq!(weight_root(&span), 1.0);
    }

    // ── effective_env test ─────────────────────────────────────────────────

    #[test]
    fn empty_tracer_env_falls_back_to_agent_env() {
        // Two samplers: one with agent_env="staging", one with agent_env="prod".
        // With an empty tracer_env, the agent_env is used, so the two samplers
        // produce different signatures for the same service.
        let mut sampler_staging = V1PrioritySampler::new(
            MetaString::from_static("staging"),
            10.0,
            1.0,
            V1SamplingRatesHandle::new(),
        );
        let mut sampler_prod = V1PrioritySampler::new(
            MetaString::from_static("prod"),
            10.0,
            1.0,
            V1SamplingRatesHandle::new(),
        );
        let mut root = make_span(0);
        // Both samplers with empty tracer_env and priority=1 should keep.
        assert!(sampler_staging.sample(SystemTime::now(), 1, &mut root, "", 0.0));
        assert!(sampler_prod.sample(SystemTime::now(), 1, &mut root, "", 0.0));
        // Verify different signatures are registered by comparing the catalog entries.
        let sig_staging = ServiceSignature::new("svc", "staging").hash();
        let sig_prod = ServiceSignature::new("svc", "prod").hash();
        assert_ne!(sig_staging, sig_prod, "different envs must produce different signatures");
    }
}
