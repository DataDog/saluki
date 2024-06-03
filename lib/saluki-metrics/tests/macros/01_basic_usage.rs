use saluki_metrics::static_metrics;

static_metrics! {
	name => Metrics,
	prefix => basic_usage,
	labels => [id: String],
	metrics => [counter(requests_handled)],
}

fn main() {
	let metrics = Metrics::new("test".to_string());
	metrics.requests_handled().increment(1);
}
