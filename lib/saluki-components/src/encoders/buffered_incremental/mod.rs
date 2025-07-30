use std::time::Duration;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::HandleExt as _;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{event::EventType, payload::PayloadType},
    observability::ComponentMetricsExt,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::{select, time::sleep};
use tracing::{debug, error};

mod telemetry;
use self::telemetry::ComponentTelemetry;

const DEFAULT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Buffered incremental encoder.
///
/// Wraps an `IncrementalEncoder` and drives it with incoming events, allowing buffering by utilizing a configurable
/// flush timeout. Payloads are encoded on the global thread pool to avoid affecting latency-sensitive tasks.
pub struct BufferedIncrementalConfiguration<EB> {
    /// Flush timeout for pending requests.
    ///
    /// When the encoder has written events to the in-flight request payload, but it has not yet reached the
    /// payload size limits that would force the payload to be flushed, the encoder will wait for a period of time
    /// before flushing the in-flight request payload. This allows for the possibility of other events to be processed
    /// and written into the request payload, thereby maximizing the payload size and reducing the number of requests
    /// generated and sent overall.
    ///
    /// Defaults to 2 seconds.
    flush_timeout: Duration,

    encoder_builder: EB,
}

impl<EB> BufferedIncrementalConfiguration<EB>
where
    EB: IncrementalEncoderBuilder,
{
    /// Creates a new `BufferedIncrementalConfiguration` from the given incremental encoder builder.
    pub fn from_encoder_builder(encoder_builder: EB) -> Self {
        Self {
            flush_timeout: DEFAULT_FLUSH_TIMEOUT,
            encoder_builder,
        }
    }
}

#[async_trait]
impl<EB> EncoderBuilder for BufferedIncrementalConfiguration<EB>
where
    EB: IncrementalEncoderBuilder + Sync,
    EB::Output: Send + 'static,
{
    fn input_event_type(&self) -> EventType {
        self.encoder_builder.input_event_type()
    }

    fn output_payload_type(&self) -> PayloadType {
        self.encoder_builder.output_payload_type()
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        let encoder = self.encoder_builder.build(context).await?;

        let flush_timeout = match self.flush_timeout {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            Duration::ZERO => Duration::from_millis(10),
            dur => dur,
        };

        Ok(Box::new(BufferedIncremental {
            encoder,
            telemetry,
            flush_timeout,
        }))
    }
}

impl<EB> MemoryBounds for BufferedIncrementalConfiguration<EB>
where
    EB: IncrementalEncoderBuilder,
{
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<BufferedIncremental<EB::Output>>("component struct");

        self.encoder_builder.specify_bounds(builder);
    }
}

pub struct BufferedIncremental<E> {
    encoder: E,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl<E> Encoder for BufferedIncremental<E>
where
    E: IncrementalEncoder + Send + 'static,
{
    async fn run(mut self: Box<Self>, context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            encoder,
            telemetry,
            flush_timeout,
        } = *self;

        // Spawn our background incremental encoder task.
        let thread_pool_handle = context.topology_context().global_thread_pool().clone();
        let runner_name = format!(
            "{}-incremental-encoder",
            context.component_context().component_id().replace("_", "-")
        );
        let runner = run_incremental_encoder(context, encoder, telemetry, flush_timeout);
        let runner_handle = thread_pool_handle.spawn_traced_named(runner_name, runner);

        debug!("Buffered Incremental encoder started.");

        // Simply wait for the runner to finish.
        //
        // It handles all of the health checking, event consuming, encoding, dispatching, etc.
        match runner_handle.await {
            Ok(Ok(())) => debug!("Incremental encoder task stopped."),
            Ok(Err(e)) => error!(error = %e, "Incremental encoder task failed."),
            Err(e) => error!(error = %e, "Incremental encoder task panicked."),
        }

        debug!("Buffered Incremental encoder stopped.");

        Ok(())
    }
}

async fn run_incremental_encoder<E>(
    mut context: EncoderContext, mut encoder: E, telemetry: ComponentTelemetry, flush_timeout: Duration,
) -> Result<(), GenericError>
where
    E: IncrementalEncoder,
{
    let mut health = context.take_health_handle();

    health.mark_ready();

    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            _ = health.live() => continue,
            maybe_event_buffer = context.events().next() => {
                // Break out of our loop when the events channel is closed.
                let event_buffer = match maybe_event_buffer {
                    Some(event_buffer) => event_buffer,
                    None => break,
                };

                for event in event_buffer {
                    // Try to process the event.
                    //
                    // If we're informed that we need to flush, we'll hold on to this event before triggering a flush and then
                    // retry processing it after flushing.
                    let event_to_retry = match encoder.process_event(event).await? {
                        ProcessResult::Continue => continue,
                        ProcessResult::FlushRequired(event) => event,
                    };

                    // Flush the encoder, waiting any payloads it has generated.
                    encoder.flush(context.dispatcher()).await?;

                    // Now try to process the event again.
                    //
                    // If this fails, then we drop the event because it's a logical bug to not be able to encode an event after
                    // flushing, and we don't want to get stuck in an infinite loop.
                    match encoder.process_event(event_to_retry).await? {
                        ProcessResult::Continue => {},
                        ProcessResult::FlushRequired(_) => {
                            error!("Failed to process event after flushing.");
                            telemetry.events_dropped_encoder().increment(1);
                        },
                    }
                }

                debug!("Processed event buffer.");

                // If we're not already pending a flush, we'll start the countdown.
                if !pending_flush {
                    pending_flush_timeout.as_mut().reset(tokio::time::Instant::now() + flush_timeout);
                    pending_flush = true;
                }
            },
            _ = &mut pending_flush_timeout, if pending_flush => {
                debug!("Flushing encoder of any pending payload(s).");

                pending_flush = false;

                encoder.flush(context.dispatcher()).await?;

                debug!("All pending payloads flushed.");
            }
        }
    }

    // Do a final flush since we may have had a pending payloads before breaking out of the loop.
    encoder.flush(context.dispatcher()).await?;

    Ok(())
}
