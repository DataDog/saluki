use saluki_metrics::static_metrics;

// Exercises the full metric-kind x level matrix (with labels): counter/gauge/histogram at the default INFO level, plus
// each kind behind the `debug_` and `trace_` level prefixes. The trailing comma after the last metric and after the
// `metrics` list also exercise the macro's optional-trailing-comma arms, and the two labels exercise multi-label
// registration.
static_metrics! {
	name => Metrics,
	prefix => matrix,
	labels => [id: String, shard: u32],
	metrics => [
		counter(tasks_completed),
		gauge(active_tasks),
		histogram(task_duration),
		debug_counter(tasks_preempted),
		debug_gauge(pending_wakeups),
		debug_histogram(debug_task_duration),
		trace_counter(poll_count),
		trace_gauge(queue_depth),
		trace_histogram(poll_duration),
	],
}

fn main() {
	let metrics = Metrics::new("worker".to_string(), 1);

	// Counters expose `increment`, gauges expose `set`, and histograms expose `record`; calling each confirms the
	// metric kind maps to the expected `metrics` type at every level.
	metrics.tasks_completed().increment(1);
	metrics.active_tasks().set(3.0);
	metrics.task_duration().record(1.5);
	metrics.tasks_preempted().increment(1);
	metrics.pending_wakeups().set(2.0);
	metrics.debug_task_duration().record(0.5);
	metrics.poll_count().increment(1);
	metrics.queue_depth().set(4.0);
	metrics.poll_duration().record(0.25);

	// The generated `<metric>_name()` accessor returns the fully-qualified metric name.
	let _ = Metrics::tasks_completed_name();
}
