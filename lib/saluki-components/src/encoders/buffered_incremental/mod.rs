use std::time::{Duration, Instant};

use agent_data_plane_config::defaults::DEFAULT_ENCODER_FLUSH_TIMEOUT;
use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownCoordinator;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::runtime;
use saluki_core::{
    components::{encoders::*, BuildContext},
    data_model::{event::EventType, payload::PayloadType},
    observability::ComponentMetricsExt,
    topology::PayloadsDispatcher,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::{pin, select, time::sleep};
use tracing::{debug, error};

mod telemetry;
use self::telemetry::ComponentTelemetry;

/// Buffered incremental encoder.
///
/// Wraps an `IncrementalEncoder` and drives it with incoming events, allowing buffering by utilizing a configurable
/// flush timeout. Payloads are encoded on the global thread pool to avoid affecting latency-sensitive tasks.
pub struct BufferedIncrementalConfiguration<EB> {
    /// Flush timeout for pending requests.
    ///
    /// When the encoder has written events to the in-flight request payload, but it hasn't yet reached the
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
            flush_timeout: DEFAULT_ENCODER_FLUSH_TIMEOUT,
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

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
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

        // Run our encoder task on the worker pool.
        //
        // The encoder task owns _everything_ -- health checking, encoding, dispatching payloads, etc -- so we just
        // end up as a glorified watchdog here, where we block until the encoder task has completed. The encoder task
        // itself is what waits for the shutdown signal and all of that.
        //
        // We're abusing `ShutdownCoordinator` here to basically detect when the encoder task exits: we immediately
        // call `shutdown_and_wait` after spawning the task, which will wait until the `ShutdownHandle` we've given to
        // the encoder tasks is dropped... which only happens when the task exits.
        let worker_pool = context.topology_context().global_thread_pool().clone();

        let mut shutdown_coordinator = ShutdownCoordinator::default();
        let task_shutdown_handle = shutdown_coordinator.register();

        runtime::worker("incremental_encoder", async move {
            // TODO: Conceptually, all components downstream of sources/relays should drain their incoming events
            // consumer until it's empty, so on and so forth until forwarders/destinations have done so and nothing
            // is left to process.
            //
            // However, this isn't feasible if we ever want to support restarting individual components or stitching
            // in new components to a topology... which we eventually do. We'd have to respond to shutdown (which
            // we're ignoring here by aliasing the handle as `_shutdown`) to shut down in a timely fashion.
            //
            // We should consider a mechanism to expose consumers/dispatchers such that they're borrowed in an owned
            // fashion, allowing a component to hold on to them when running, but then effectively release them when
            // they exit. This would allow us to stop a component, and without dropping its interconnect channel and
            // losing all of the events or payloads within it, reconnect it to the new instance of the component (or
            // whatever component takes its place, etc) which would allow us to more cleanly respond to shutdown
            // here as more of an exceptional thing: an immediate stoppage of work, without throwing away _pending_
            // work.
            let _task_shutdown_handle = task_shutdown_handle;
            run_incremental_encoder(context, encoder, telemetry, flush_timeout).await
        })
        .on_runtime(worker_pool)
        .spawn();

        debug!("Buffered Incremental encoder started.");

        shutdown_coordinator.shutdown_and_wait().await;

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
    pin!(pending_flush_timeout);

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
                    flush_encoder(&mut encoder, context.dispatcher(), &telemetry).await?;

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

                flush_encoder(&mut encoder, context.dispatcher(), &telemetry).await?;

                debug!("All pending payloads flushed.");
            }
        }
    }

    // Do a final flush since we may have had a pending payloads before breaking out of the loop.
    flush_encoder(&mut encoder, context.dispatcher(), &telemetry).await?;

    Ok(())
}

/// Flushes the encoder, recording flush telemetry for the events it dispatched.
///
/// The telemetry counts the events in the payloads the encoder actually dispatched, not the events it accepted: an
/// encoder can discard a payload it can't build, such as one that exceeds its size limit and can't be split, and those
/// events were never flushed. Events dispatched before a flush fails are still counted. Flushes that dispatch nothing
/// aren't recorded, so the last-flush telemetry always describes a flush that sent something.
async fn flush_encoder<E>(
    encoder: &mut E, dispatcher: &PayloadsDispatcher, telemetry: &ComponentTelemetry,
) -> Result<(), GenericError>
where
    E: IncrementalEncoder,
{
    let flush_start = Instant::now();
    let result = encoder.flush(dispatcher).await;

    let dispatched_events = match &result {
        Ok(dispatched_events) => *dispatched_events,
        Err(e) => e.dispatched_events(),
    };
    if dispatched_events > 0 {
        telemetry.record_flush(dispatched_events as u64, flush_start.elapsed());
    }

    result.map(|_| ()).map_err(FlushError::into_source)
}

#[cfg(test)]
mod tests {
    use saluki_core::{
        data_model::event::{
            eventd::EventD,
            service_check::{CheckStatus, ServiceCheck},
            Event,
        },
        topology::{OutputName, PayloadsBuffer},
    };
    use saluki_error::generic_error;
    use saluki_metrics::test::TestRecorder;
    use tokio::sync::mpsc;

    use super::*;
    use crate::encoders::datadog::{DatadogEventsConfiguration, DatadogServiceChecksConfiguration};

    const PAYLOAD_SIZE_LIMIT: usize = 4 * 1024 * 1024;

    fn dispatcher_with_receiver(context: &BuildContext) -> (PayloadsDispatcher, mpsc::Receiver<PayloadsBuffer>) {
        let mut dispatcher = PayloadsDispatcher::new(context.component_context().clone());
        dispatcher.add_output(OutputName::Default).unwrap();
        let (tx, rx) = mpsc::channel(8);
        dispatcher.attach_sender_to_output(&OutputName::Default, tx).unwrap();
        (dispatcher, rx)
    }

    fn dispatched_payloads(rx: &mut mpsc::Receiver<PayloadsBuffer>) -> usize {
        let mut payloads = 0;
        while rx.try_recv().is_ok() {
            payloads += 1;
        }
        payloads
    }

    async fn events_encoder(context: BuildContext, max_payload_size: usize) -> impl IncrementalEncoder {
        DatadogEventsConfiguration {
            max_payload_size,
            max_uncompressed_payload_size: PAYLOAD_SIZE_LIMIT,
            compressor_kind: "zstd".to_string(),
            zstd_level: 3,
            log_payloads: false,
        }
        .build(context)
        .await
        .expect("events encoder should build")
    }

    async fn accept_events(encoder: &mut impl IncrementalEncoder, count: usize) {
        for i in 0..count {
            let event = Event::EventD(EventD::new(format!("title {i}"), "text"));
            assert!(matches!(
                encoder.process_event(event).await.unwrap(),
                ProcessResult::Continue
            ));
        }
    }

    #[tokio::test]
    async fn flush_records_the_events_in_dispatched_payloads() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let context = BuildContext::test_encoder("flush_telemetry");
        let (dispatcher, mut rx) = dispatcher_with_receiver(&context);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut encoder = events_encoder(context, PAYLOAD_SIZE_LIMIT).await;

        accept_events(&mut encoder, 3).await;
        flush_encoder(&mut encoder, &dispatcher, &telemetry).await.unwrap();

        assert_eq!(dispatched_payloads(&mut rx), 1);
        assert_eq!(recorder.counter("encoder_flushed_events_total"), Some(3));
        assert_eq!(recorder.gauge("encoder_last_flush_events"), Some(3.0));
    }

    // The labels on the encoder's "dropped because the payload couldn't be built" counter.
    fn encoder_dropped_events_labels(component_id: &'static str) -> [(&'static str, &'static str); 4] {
        [
            ("component_id", component_id),
            ("component_type", "encoder"),
            ("intentional", "false"),
            ("drop_reason", "encoder_failure"),
        ]
    }

    #[tokio::test]
    async fn flush_does_not_record_events_in_discarded_payloads() {
        // A compressed payload that exceeds the size limit and can't be split is discarded and logged, but the flush
        // itself succeeds. The events it held were never flushed, so they must not be counted as flushed. They were
        // lost, though, so they are counted as dropped.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let context = BuildContext::test_encoder("flush_telemetry_discarded");
        let (dispatcher, mut rx) = dispatcher_with_receiver(&context);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut encoder = events_encoder(context, 1).await;

        accept_events(&mut encoder, 1).await;
        flush_encoder(&mut encoder, &dispatcher, &telemetry).await.unwrap();

        assert_eq!(dispatched_payloads(&mut rx), 0);
        assert_eq!(recorder.counter("encoder_flushed_events_total"), Some(0));
        assert_eq!(recorder.gauge("encoder_last_flush_events"), Some(0.0));
        assert_eq!(
            recorder.counter((
                "component_events_dropped_total",
                &encoder_dropped_events_labels("flush_telemetry_discarded")[..]
            )),
            Some(1)
        );
    }

    #[tokio::test]
    async fn flush_does_not_report_dropped_events_when_everything_is_dispatched() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let context = BuildContext::test_encoder("flush_no_drops");
        let (dispatcher, mut rx) = dispatcher_with_receiver(&context);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut encoder = events_encoder(context, PAYLOAD_SIZE_LIMIT).await;

        accept_events(&mut encoder, 3).await;
        flush_encoder(&mut encoder, &dispatcher, &telemetry).await.unwrap();

        assert_eq!(dispatched_payloads(&mut rx), 1);
        assert_eq!(
            recorder.counter((
                "component_events_dropped_total",
                &encoder_dropped_events_labels("flush_no_drops")[..]
            )),
            Some(0)
        );
    }

    #[tokio::test]
    async fn flush_returns_the_number_of_dispatched_events() {
        let context = BuildContext::test_encoder("flush_count");
        let (dispatcher, mut rx) = dispatcher_with_receiver(&context);

        let mut events = events_encoder(context.clone(), PAYLOAD_SIZE_LIMIT).await;
        accept_events(&mut events, 2).await;
        assert_eq!(events.flush(&dispatcher).await.unwrap(), 2);

        let mut service_checks = DatadogServiceChecksConfiguration {
            max_payload_size: PAYLOAD_SIZE_LIMIT,
            max_uncompressed_payload_size: PAYLOAD_SIZE_LIMIT,
            compressor_kind: "zstd".to_string(),
            zstd_level: 3,
            log_payloads: false,
        }
        .build(context)
        .await
        .expect("service checks encoder should build");
        for i in 0..4 {
            let service_check = Event::ServiceCheck(ServiceCheck::new(format!("check {i}"), CheckStatus::Ok));
            assert!(matches!(
                service_checks.process_event(service_check).await.unwrap(),
                ProcessResult::Continue
            ));
        }
        assert_eq!(service_checks.flush(&dispatcher).await.unwrap(), 4);

        // An empty encoder has nothing to dispatch.
        assert_eq!(service_checks.flush(&dispatcher).await.unwrap(), 0);
        assert_eq!(dispatched_payloads(&mut rx), 2);
    }

    // An encoder whose flush dispatches some events and then fails, as when a later payload can't be dispatched.
    struct FailingFlushEncoder {
        dispatched_before_failure: usize,
    }

    #[async_trait]
    impl IncrementalEncoder for FailingFlushEncoder {
        async fn process_event(&mut self, _event: Event) -> Result<ProcessResult, GenericError> {
            Ok(ProcessResult::Continue)
        }

        async fn flush(&mut self, _dispatcher: &PayloadsDispatcher) -> Result<usize, FlushError> {
            Err(FlushError::new(
                self.dispatched_before_failure,
                generic_error!("output closed"),
            ))
        }
    }

    #[tokio::test]
    async fn failed_flush_still_records_the_events_it_dispatched() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let context = BuildContext::test_encoder("flush_failure");
        let (dispatcher, _rx) = dispatcher_with_receiver(&context);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut encoder = FailingFlushEncoder {
            dispatched_before_failure: 3,
        };

        let error = flush_encoder(&mut encoder, &dispatcher, &telemetry)
            .await
            .expect_err("the flush should fail");

        assert_eq!(error.to_string(), "output closed");
        assert_eq!(recorder.counter("encoder_flushed_events_total"), Some(3));
        assert_eq!(recorder.gauge("encoder_last_flush_events"), Some(3.0));
    }

    #[tokio::test]
    async fn failed_flush_that_dispatched_nothing_records_nothing() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let context = BuildContext::test_encoder("flush_failure_empty");
        let (dispatcher, _rx) = dispatcher_with_receiver(&context);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut encoder = FailingFlushEncoder {
            dispatched_before_failure: 0,
        };

        flush_encoder(&mut encoder, &dispatcher, &telemetry)
            .await
            .expect_err("the flush should fail");

        assert_eq!(recorder.counter("encoder_flushed_events_total"), Some(0));
        assert_eq!(recorder.gauge("encoder_last_flush_events"), Some(0.0));
    }

    #[tokio::test]
    async fn events_encoder_reports_an_error_when_its_output_is_closed() {
        let context = BuildContext::test_encoder("flush_closed_output");
        let (dispatcher, rx) = dispatcher_with_receiver(&context);
        drop(rx);
        let mut encoder = events_encoder(context, PAYLOAD_SIZE_LIMIT).await;

        accept_events(&mut encoder, 1).await;
        let error = encoder
            .flush(&dispatcher)
            .await
            .expect_err("dispatching to a closed output should fail");

        // Nothing was dispatched before the failure, so there is nothing to account for.
        assert_eq!(error.dispatched_events(), 0);
    }
}
