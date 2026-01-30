//! Simulation engine for driving workload generation.

use std::time::{Duration, Instant};

use rand::{rngs::StdRng, Rng as _, SeedableRng};
use saluki_error::GenericError;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;
use tokio_util::time::DelayQueue;
use tracing::{debug, error, info, trace, warn};

/// Progress statistics for tracking telemetry sent.
#[derive(Default)]
struct ProgressStats {
    // Interval counters (reset every report)
    interval_spans: u64,
    interval_metrics: u64,
    interval_logs: u64,
    interval_batches: u64,

    // Cumulative counters
    total_spans: u64,
    total_metrics: u64,
    total_logs: u64,

    start_time: Option<Instant>,
    last_report_time: Option<Instant>,
}

impl ProgressStats {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            start_time: Some(now),
            last_report_time: Some(now),
            ..Default::default()
        }
    }

    fn record_batch(&mut self, spans: usize, metrics: usize, logs: usize) {
        self.interval_spans += spans as u64;
        self.interval_metrics += metrics as u64;
        self.interval_logs += logs as u64;
        self.interval_batches += 1;

        self.total_spans += spans as u64;
        self.total_metrics += metrics as u64;
        self.total_logs += logs as u64;
    }

    /// Reports progress during active generation.
    fn report_progress(&mut self, queued: usize, in_flight: usize) {
        let elapsed = self.start_time.map(|t| t.elapsed()).unwrap_or_default();
        let interval_secs = self
            .last_report_time
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(1.0)
            .max(0.001); // Avoid division by zero

        let spans_per_sec = self.interval_spans as f64 / interval_secs;
        let metrics_per_sec = self.interval_metrics as f64 / interval_secs;
        let logs_per_sec = self.interval_logs as f64 / interval_secs;

        info!(
            "Generating for {:.1} seconds. queued={} in-flight={} spans={:.1}/s ({}) metrics={:.1}/s ({}) logs={:.1}/s ({})",
            elapsed.as_secs_f64(),
            queued,
            in_flight,
            spans_per_sec,
            self.total_spans,
            metrics_per_sec,
            self.total_metrics,
            logs_per_sec,
            self.total_logs
        );

        self.reset_interval();
    }

    /// Reports progress during draining (after generation stopped).
    fn report_draining(&mut self, queued: usize, in_flight: usize) {
        let interval_secs = self
            .last_report_time
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(1.0)
            .max(0.001);

        // Calculate completion rate (batches per second)
        let batches_per_sec = self.interval_batches as f64 / interval_secs;

        // Estimate time to complete remaining items
        let remaining = queued + in_flight;
        let eta_secs = if batches_per_sec > 0.0 {
            remaining as f64 / batches_per_sec
        } else {
            f64::INFINITY
        };

        info!(
            "Draining remaining enqueued/in-flight telemetry payloads. Estimated completion: {:.1} seconds. ({} remaining, {:.1}/s)",
            eta_secs, remaining, batches_per_sec
        );

        self.reset_interval();
    }

    fn reset_interval(&mut self) {
        self.interval_spans = 0;
        self.interval_metrics = 0;
        self.interval_logs = 0;
        self.interval_batches = 0;
        self.last_report_time = Some(Instant::now());
    }

    fn report_final(&self) {
        let elapsed = self.start_time.map(|t| t.elapsed()).unwrap_or_default();
        let elapsed_secs = elapsed.as_secs_f64().max(0.001);

        info!(
            "Generating and sending complete. Sent {} spans, {} metrics, {} logs in {:.1} seconds",
            self.total_spans, self.total_metrics, self.total_logs, elapsed_secs
        );
    }
}

use super::timing::{ns_to_ms, TimingGenerator};
use super::SimulatedRequest;
use crate::config::{Config, WorkloadConfig};
use crate::generator::{
    LogGenerator, MetricGenerator, ServiceTelemetry, SimulationResult, TelemetryBatch, TraceGenerator,
};
use crate::model::{ResolvedArchitecture, TraceContext};
use crate::sender::GrpcOtlpSender;

/// The simulation engine that drives workload generation.
pub struct SimulationEngine {
    architecture: ResolvedArchitecture,
    rng: StdRng,
    trace_generators: Vec<Option<TraceGenerator>>,
    metric_generators: Vec<Option<MetricGenerator>>,
    log_generators: Vec<Option<LogGenerator>>,
    sender: GrpcOtlpSender,
}

impl SimulationEngine {
    /// Creates a new simulation engine from the configuration.
    pub async fn new(config: &Config) -> Result<Self, GenericError> {
        let architecture = ResolvedArchitecture::from_config(config)?;
        let rng = StdRng::from_seed(config.seed.0);

        // Create generators for each service
        let mut trace_generators = Vec::with_capacity(architecture.service_count());
        let mut metric_generators = Vec::with_capacity(architecture.service_count());
        let mut log_generators = Vec::with_capacity(architecture.service_count());

        for service in &architecture.services {
            // Trace generator
            let trace_gen = service
                .template
                .traces
                .as_ref()
                .map(|t| TraceGenerator::new(service.name.clone(), t.clone()));
            trace_generators.push(trace_gen);

            // Metric generator
            let metric_gen = service
                .template
                .metrics
                .as_ref()
                .map(|m| MetricGenerator::new(service.name.clone(), m.clone()));
            metric_generators.push(metric_gen);

            // Log generator
            let log_gen = service
                .template
                .logs
                .as_ref()
                .map(|l| LogGenerator::new(service.name.clone(), l.clone()));
            log_generators.push(log_gen);
        }

        let endpoint = config.output.grpc_endpoint();
        let sender = GrpcOtlpSender::new(&endpoint).await?;

        Ok(Self {
            architecture,
            rng,
            trace_generators,
            metric_generators,
            log_generators,
            sender,
        })
    }

    /// Runs the simulation for the configured duration.
    pub async fn run(self, workload: &WorkloadConfig) -> Result<(), GenericError> {
        // Destructure self to separate sender from simulation state
        let Self {
            architecture,
            mut rng,
            trace_generators,
            mut metric_generators,
            log_generators,
            sender,
        } = self;

        let interval = workload.interval();
        let deadline = Instant::now() + workload.duration;

        info!(
            "Starting simulation with {} services, {} entry points, generating at {:?} intervals for {:?}.",
            architecture.service_count(),
            architecture.entry_point_names().len(),
            interval,
            workload.duration
        );

        // Bounded channel for sending telemetry items to the sender task.
        // The bound provides backpressure if the sender can't keep up.
        const CHANNEL_BOUND: usize = 256;
        let (tx, rx) = mpsc::channel::<(ServiceTelemetry, Instant)>(CHANNEL_BOUND);

        // Spawn the sender task that uses DelayQueue
        let sender_handle = tokio::spawn(async move { run_sender_task(rx, sender).await });

        let mut requests_generated = 0u64;

        while Instant::now() < deadline {
            // Generate a request
            let entry_idx = architecture.random_entry_point(&mut rng);
            let request = SimulatedRequest::new(&mut rng);
            let simulation_start = Instant::now();

            // Simulate the request through the service graph
            let result = simulate_request(
                &architecture,
                &mut rng,
                &trace_generators,
                &mut metric_generators,
                &log_generators,
                entry_idx,
                &request.root_context,
                simulation_start,
            )?;

            // Enqueue all telemetry items with their target send times
            for telemetry in result.telemetry_items {
                if telemetry.has_data() {
                    let send_at = simulation_start + telemetry.send_after;
                    if tx.send((telemetry, send_at)).await.is_err() {
                        // Receiver dropped, sender task must have failed
                        break;
                    }
                }
            }

            requests_generated += 1;

            if requests_generated.is_multiple_of(100) {
                debug!("Generated {} requests.", requests_generated);
            }

            // Wait for the next interval
            tokio::time::sleep(interval).await;
        }

        // Drop the sender to signal completion
        drop(tx);

        // Wait for the sender task to finish processing remaining items
        info!("Generation complete. Waiting for sender to flush remaining telemetry...");
        match sender_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(saluki_error::generic_error!("Sender task panicked: {}", e)),
        }

        info!("Simulation complete. Generated {} requests.", requests_generated);
        Ok(())
    }
}

fn simulate_request(
    architecture: &ResolvedArchitecture, rng: &mut StdRng, trace_generators: &[Option<TraceGenerator>],
    metric_generators: &mut [Option<MetricGenerator>], log_generators: &[Option<LogGenerator>], service_idx: usize,
    ctx: &TraceContext, simulation_start: Instant,
) -> Result<SimulationResult, GenericError> {
    let mut result = SimulationResult::new(simulation_start);
    simulate_service(
        architecture,
        rng,
        trace_generators,
        metric_generators,
        log_generators,
        service_idx,
        ctx,
        Duration::ZERO,
        &mut result,
    )?;
    Ok(result)
}

/// Simulates a service and its downstream calls, collecting telemetry.
///
/// `time_offset` is the simulated time offset from the start of the request
/// when this service begins processing.
#[allow(clippy::too_many_arguments)]
fn simulate_service(
    architecture: &ResolvedArchitecture, rng: &mut StdRng, trace_generators: &[Option<TraceGenerator>],
    metric_generators: &mut [Option<MetricGenerator>], log_generators: &[Option<LogGenerator>], service_idx: usize,
    ctx: &TraceContext, time_offset: Duration, result: &mut SimulationResult,
) -> Result<Duration, GenericError> {
    let service = architecture.get_service(service_idx);
    let service_name = service.name.clone();
    let downstream_indices = service.downstream_indices.clone();

    // Generate the span duration
    let base_duration_ns = trace_generators[service_idx]
        .as_ref()
        .map(|g| g.base_duration_ns())
        .unwrap_or(50_000_000); // 50ms default

    let timing = TimingGenerator::new(base_duration_ns);
    let span_duration_ns = timing.generate_duration_ns(rng);
    let span_duration = Duration::from_nanos(span_duration_ns);
    let span_duration_ms = ns_to_ms(span_duration_ns);

    // Track time within this span for downstream calls
    let mut current_offset = time_offset;

    // Add a small offset before calling downstream services (simulates processing before the call)
    let pre_call_offset_ns = timing.generate_child_offset_ns(rng, span_duration_ns);
    current_offset += Duration::from_nanos(pre_call_offset_ns);

    // Recursively simulate downstream services
    // Each downstream service finishes and sends its telemetry before we finish
    for downstream_idx in downstream_indices {
        // Create child context with appropriate start time
        let child_start_offset_ns = current_offset.as_nanos() as u64 - time_offset.as_nanos() as u64;
        let child_ctx = ctx.child(rng, child_start_offset_ns);

        // Simulate the downstream service
        let child_end_offset = simulate_service(
            architecture,
            rng,
            trace_generators,
            metric_generators,
            log_generators,
            downstream_idx,
            &child_ctx,
            current_offset,
            result,
        )?;

        // Add a small gap after the child returns
        let post_call_gap = Duration::from_nanos(rng.random_range(100_000..1_000_000)); // 0.1-1ms
        current_offset = child_end_offset + post_call_gap;
    }

    // Calculate when this service finishes (at least span_duration from start)
    let service_end_time = time_offset + span_duration;
    let actual_end_time = service_end_time.max(current_offset);

    // Generate telemetry for this service
    let resource_spans = trace_generators[service_idx]
        .as_ref()
        .map(|g| g.generate_span(rng, ctx, span_duration_ns));

    let resource_metrics = metric_generators[service_idx]
        .as_mut()
        .map(|g| g.generate_metrics(rng, span_duration_ms));

    let resource_logs = log_generators[service_idx]
        .as_ref()
        .map(|g| g.generate_logs(rng, Some(ctx)));

    // This service sends its telemetry when it finishes
    let telemetry = ServiceTelemetry {
        service_name,
        resource_spans,
        resource_metrics,
        resource_logs,
        send_after: actual_end_time,
    };

    result.add(telemetry);

    Ok(actual_end_time)
}

/// Result of sending a single telemetry batch.
struct SendResult {
    spans: usize,
    metrics: usize,
    logs: usize,
}

/// Sender task that uses a DelayQueue to send telemetry at the appropriate times.
///
/// Uses a JoinSet to spawn individual send tasks for concurrent sending, with the concurrency limited by the underlying
/// gRPC channel configuration.
async fn run_sender_task(
    mut rx: mpsc::Receiver<(ServiceTelemetry, Instant)>, sender: GrpcOtlpSender,
) -> Result<(), GenericError> {
    use tokio::task::JoinSet;

    let mut delay_queue: DelayQueue<ServiceTelemetry> = DelayQueue::new();
    let mut send_tasks: JoinSet<Result<SendResult, GenericError>> = JoinSet::new();
    let mut stats = ProgressStats::new();
    let mut progress_interval = tokio::time::interval(Duration::from_secs(5));
    let mut generation_stopped = false;

    loop {
        // Check if we're done: generation stopped, queue empty, and no pending send tasks
        if generation_stopped && delay_queue.is_empty() && send_tasks.is_empty() {
            break;
        }

        tokio::select! {
            // Receive new items to enqueue (only if generation hasn't stopped)
            item = rx.recv(), if !generation_stopped => {
                match item {
                    Some((telemetry, send_at)) => {
                        let now = Instant::now();
                        let delay = if send_at > now {
                            send_at - now
                        } else {
                            Duration::ZERO
                        };
                        trace!(
                            service = %telemetry.service_name,
                            delay_ms = delay.as_millis(),
                            "Enqueueing telemetry."
                        );
                        delay_queue.insert(telemetry, delay);
                    }
                    None => {
                        // Channel closed
                        generation_stopped = true;
                        info!(
                            queued = delay_queue.len(),
                            in_flight = send_tasks.len(),
                            "Generation stopped, draining remaining telemetry."
                        );
                    }
                }
            }

            // Send items whose delay has expired
            Some(expired) = delay_queue.next(), if !delay_queue.is_empty() => {
                let telemetry = expired.into_inner();
                trace!(service = %telemetry.service_name, "Sending telemetry.");

                // Spawn a task to send this telemetry concurrently
                let sender_clone = sender.clone();
                send_tasks.spawn(async move {
                    send_telemetry(sender_clone, telemetry).await
                });
            }

            // Collect results from completed send tasks
            Some(result) = send_tasks.join_next(), if !send_tasks.is_empty() => {
                match result {
                    Ok(Ok(send_result)) => {
                        stats.record_batch(send_result.spans, send_result.metrics, send_result.logs);
                    }
                    Ok(Err(e)) => {
                        // Log error but continue processing
                        warn!(error = %e, "Failed to send telemetry batch");
                    }
                    Err(e) => {
                        // Task panicked
                        error!(error = %e, "Send task panicked");
                    }
                }
            }

            // Periodic progress reporting
            _ = progress_interval.tick() => {
                if generation_stopped {
                    stats.report_draining(delay_queue.len(), send_tasks.len());
                } else {
                    stats.report_progress(delay_queue.len(), send_tasks.len());
                }
            }
        }
    }

    stats.report_final();
    Ok(())
}

/// Sends a single telemetry item (spans, metrics, logs) to the OTLP receiver.
async fn send_telemetry(sender: GrpcOtlpSender, telemetry: ServiceTelemetry) -> Result<SendResult, GenericError> {
    let mut batch = TelemetryBatch::new();
    batch.add(telemetry);

    let mut spans = 0;
    let mut metrics = 0;
    let mut logs = 0;

    if batch.has_traces() {
        let request = batch.take_trace_request();
        spans = sender.send_traces(request).await?;
    }

    if batch.has_metrics() {
        let request = batch.take_metrics_request();
        metrics = sender.send_metrics(request).await?;
    }

    if batch.has_logs() {
        let request = batch.take_logs_request();
        logs = sender.send_logs(request).await?;
    }

    Ok(SendResult { spans, metrics, logs })
}
