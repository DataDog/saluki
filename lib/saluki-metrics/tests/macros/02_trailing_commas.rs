use saluki_metrics::static_metrics;

static_metrics! {
	name => Metrics,
	prefix => basic_usage,
	labels => [id: String],
	metrics => [counter(basic_counter)],
}

fn main() {}
