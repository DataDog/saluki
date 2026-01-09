//! Span weight calculation for APM stats.

use saluki_core::data_model::event::trace::Span;

const KEY_SAMPLING_RATE_GLOBAL: &str = "_sample_rate";

pub(super) fn weight(span: &Span) -> f64 {
    if let Some(&rate) = span.metrics().get(KEY_SAMPLING_RATE_GLOBAL) {
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
