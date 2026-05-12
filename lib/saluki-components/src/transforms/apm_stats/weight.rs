//! Span weight calculation for APM stats.

use saluki_core::data_model::event::trace::{AttributeValue, Span};

const KEY_SAMPLING_RATE_GLOBAL: &str = "_sample_rate";

// TODO: `Trace::client_dropped_p0s_weight` (populated from the `Datadog-Client-Dropped-P0-Traces`
// HTTP header on the V1 APM path) is not factored into stats weight. In the Go trace agent,
// dropped P0 client traces inflate the weight to compensate for traces the client discarded before
// sending. This function only accounts for agent-side sampling rate (`_sample_rate`), so V1 APM
// stats may be undercounted when clients are dropping P0s. The fix would be to accept `&Trace`
// here (or take the multiplier as a parameter) and multiply the result by
// `trace.client_dropped_p0s_weight` when it is non-zero.
pub(super) fn weight(span: &Span) -> f64 {
    if let Some(rate) = span.attributes.get(KEY_SAMPLING_RATE_GLOBAL).and_then(AttributeValue::as_float) {
        if rate > 0.0 && rate <= 1.0 {
            return 1.0 / rate;
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use stringtheory::MetaString;

    use super::*;

    #[test]
    fn test_weight() {
        // Default - no sampling rate metric
        let span = Span::default();
        assert_eq!(weight(&span), 1.0);

        // Negative rate - should use default
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_SAMPLING_RATE_GLOBAL), -1.0);
        let span = Span::default().with_metrics(metrics);
        assert_eq!(weight(&span), 1.0);

        // Zero rate - should use default
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_SAMPLING_RATE_GLOBAL), 0.0);
        let span = Span::default().with_metrics(metrics);
        assert_eq!(weight(&span), 1.0);

        // Valid rate 0.25 - weight should be 4.0
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_SAMPLING_RATE_GLOBAL), 0.25);
        let span = Span::default().with_metrics(metrics);
        assert_eq!(weight(&span), 4.0);

        // Rate = 1.0 - weight should be 1.0
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_SAMPLING_RATE_GLOBAL), 1.0);
        let span = Span::default().with_metrics(metrics);
        assert_eq!(weight(&span), 1.0);

        // Rate > 1 - should use default
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_SAMPLING_RATE_GLOBAL), 1.5);
        let span = Span::default().with_metrics(metrics);
        assert_eq!(weight(&span), 1.0);
    }
}
