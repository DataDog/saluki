use saluki_metrics::static_metrics;

// Exercises the no-labels macro arm (the rule that omits `labels => [...]` entirely) across all three metric kinds.
static_metrics! {
	name => Metrics,
	prefix => no_labels,
	metrics => [
		counter(requests),
		gauge(in_flight),
		histogram(latency),
	],
}

fn main() {
	// With no labels declared, `new` takes no arguments.
	let metrics = Metrics::new();
	metrics.requests().increment(1);
	metrics.in_flight().set(2.0);
	metrics.latency().record(0.5);
}
