//! Running the source: accepting connections, reading streams, and pinning the origin of what is read.

use std::{future::Future, io, num::NonZeroUsize, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Buf;
use saluki_common::sync::{
    shutdown::ShutdownHandle,
    work_queue::{self, WorkQueueReceiver, WorkQueueSender},
};
use saluki_core::{
    accounting::MemoryLimiter,
    components::sources::*,
    pooling::{ElasticObjectPool, ObjectPool as _},
    runtime::{self, state::ResourceLease, ShutdownStrategy},
};
use saluki_env::workload::CaptureEntityResolver;
use saluki_error::GenericError;
use saluki_io::{
    buf::{BytesBuffer, CollapsibleReadWriteIoBuffer as _, FixedSizeVec, ReadIoBuffer as _},
    net::{listener::Listener, ConnectionAddress, ListenAddress, ProcessIdentity, Stream},
};
use tokio::{
    pin, select,
    time::{interval, Interval, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

use super::{
    build_metrics, get_adjusted_buffer_size, BufferDecodeContext, BufferDecodeMode, DecodeOutcome, DecoderContext,
    DogStatsDCaptureControl, DogStatsDDecoder, EolRequired, ForwarderMetrics, Metrics, PacketForwarder,
    PacketForwarderTarget, ProcessOrigin, REPLAY_CREDENTIALS_GID,
};

/// DogStatsD source.
pub struct DogStatsD {
    pub(super) listeners: Vec<ResourceLease<Listener>>,
    pub(super) decoder_worker_count: NonZeroUsize,
    pub(super) io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    pub(super) io_buffer_queue_capacity: usize,
    pub(super) io_buffer_pool_shrinker: Pin<Box<dyn Future<Output = ()> + Send>>,
    pub(super) decoder_context: DecoderContext,
    pub(super) origin_telemetry_enabled: bool,
    pub(super) eol_required: EolRequired,
    pub(super) capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    pub(super) packet_forwarder_target: Option<PacketForwarderTarget>,
}

/// State shared by every stream of one listener, and by the datagrams queued from them.
pub(super) struct ListenerState {
    pub(super) listen_addr: ListenAddress,
    pub(super) eol_required: bool,
    pub(super) metrics: Metrics,
    pub(super) packet_forwarder: Option<PacketForwarder>,
    pub(super) io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    pub(super) decoder_context: DecoderContext,
    pub(super) capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
}

/// A datagram read by a listener, along with the origin pinned at read time.
pub(super) struct ReceivedDatagram {
    pub(super) buffer: BytesBuffer,
    pub(super) bytes_read: usize,
    pub(super) peer_addr: ConnectionAddress,
    pub(super) process_origin: Option<ProcessOrigin>,
}

pub(super) struct QueuedDatagram {
    pub(super) result: io::Result<ReceivedDatagram>,
    pub(super) listener: Arc<ListenerState>,
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();

        // This source spawns everything below as its own children. Thus, the source does not finish until they finish.
        // When the body of the source returns, the children receive the shutdown signal.
        //
        // Brutal: the shrinker loops forever with no terminal condition of its own, so waiting for it would hold
        // every shutdown open until its timeout expired.
        runtime::worker("io_buffer_pool_shrinker", self.io_buffer_pool_shrinker)
            .with_shutdown_strategy(ShutdownStrategy::Brutal)
            .spawn();

        // Each decoder gets its own queue. Sharing one receiver between decoders serializes decoding: handing the
        // receiver to the next decoder puts it behind the previous decoder's decode work on the same runtime thread.
        let (datagram_sender, datagram_receivers) =
            work_queue::bounded(self.decoder_worker_count, self.io_buffer_queue_capacity);
        let decoder_context = self.decoder_context;

        // Decoders must drain their queue to completion, so they deliberately ignore the shutdown signal and stop only
        // once their queue closes -- which happens when every listener and stream handler has dropped its datagram
        // sender. This makes shutdown lossless. The listeners stop when the body of this source returns. The decoders
        // can see the close only after the listeners and their stream handlers stop. Then the decoders drain the
        // datagrams that remain.
        //
        // When dispatch is backpressured, the drain can take longer than five seconds, the default timeout of a
        // closure-based child. Thus, the decoders have no timeout of their own, and are halted with the source if the
        // component's shutdown timeout expires.
        for (worker_id, datagram_receiver) in datagram_receivers.into_iter().enumerate() {
            let decoder_source_context = context.clone();
            let decoder_context = decoder_context.clone();

            runtime::worker(
                format!("datagram_decoder_{worker_id}"),
                drive_datagram_decoder(datagram_receiver, decoder_source_context, decoder_context),
            )
            .with_shutdown_timeout(Duration::MAX)
            .spawn();
        }

        // One packet forwarder serves every listener. Its forwarding loop stops once every listener, stream handler, and
        // decoder has dropped its handle, after sending whatever is still queued.
        let packet_forwarder = self
            .packet_forwarder_target
            .as_ref()
            .map(|target| target.to_forwarder(ForwarderMetrics::new(context.component_context())));
        if let Some(packet_forwarder) = &packet_forwarder {
            packet_forwarder.spawn();
        }

        // For each listener, spawn a dedicated task to run it. There may be none, when no listener could be created and
        // the listener requirement is `Optional`. The source then only waits for shutdown.
        for listener in self.listeners {
            let task_name = format!("listener_{}", listener.listen_address().listener_type());
            let listener_source_context = context.clone();

            // TODO: Create a health handle for each listener.
            //
            // We need to rework `HealthRegistry` to look a little more like `ComponentRegistry` so that we can have it
            // already be scoped properly, otherwise all we can do here at present is either have a relative name, like
            // `uds-stream`, or try and hardcode the full component name, which we will inevitably forget to update if
            // we tweak the topology configuration, etc.
            let listen_addr = listener.listen_address().clone();
            let metrics = build_metrics(&listen_addr, context.component_context(), self.origin_telemetry_enabled);
            let listener_state = Arc::new(ListenerState {
                eol_required: self.eol_required.for_listener(&listen_addr),
                listen_addr,
                metrics,
                packet_forwarder: packet_forwarder.clone(),
                io_buffer_pool: self.io_buffer_pool.clone(),
                decoder_context: decoder_context.clone(),
                capture_entity_resolver: self.capture_entity_resolver.clone(),
            });
            let datagram_sender = datagram_sender.clone();

            // An accept loop has no terminal condition of its own, so the listener needs the shutdown signal. It has no
            // shutdown timeout of its own: the topology shutdown timeout applies to the source and everything it spawned.
            //
            // Each listener is needed. A fatal accept error stops a listener without a request to stop. Then the source
            // has nothing to listen on, so the listener stops the source. The source does not continue to run.
            runtime::worker_with_shutdown(task_name, move |process_shutdown| {
                process_listener(
                    listener_source_context,
                    listener,
                    listener_state,
                    datagram_sender,
                    process_shutdown,
                )
            })
            .with_shutdown_timeout(Duration::MAX)
            .needed()
            .spawn();
        }
        drop(datagram_sender);
        drop(packet_forwarder);

        health.mark_ready();
        debug!("DogStatsD source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        //
        // We also handle liveness here, which doesn't really matter for _this_ task, since the real work is happening
        // in the listeners, but we need to satisfy the health checker.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
            }
        }

        // This return stops the listeners, and after they stop, the decoders drain. The source finishes when all of its
        // children finish.
        debug!("Stopping DogStatsD source...");

        Ok(())
    }
}

pub(super) fn build_io_buffer_pool(
    min_buffers: usize, max_buffers: usize, buffer_size: usize,
) -> (ElasticObjectPool<BytesBuffer>, impl Future<Output = ()> + Send) {
    saluki_antithesis::always_le!(
        buffer_size,
        usize::MAX - 4,
        "dogstatsd buffer size add does not overflow",
        { "buffer_size": buffer_size }
    );
    let adjusted_buffer_size = get_adjusted_buffer_size(buffer_size);
    ElasticObjectPool::with_builder("dsd_packet_bufs", min_buffers, max_buffers, move || {
        FixedSizeVec::with_capacity(adjusted_buffer_size)
    })
}

async fn process_listener(
    source_context: SourceContext, mut listener: ResourceLease<Listener>, listener_state: Arc<ListenerState>,
    datagram_sender: WorkQueueSender<QueuedDatagram>, process_shutdown: ShutdownHandle,
) {
    pin!(process_shutdown);

    let listen_addr = &listener_state.listen_addr;

    info!(%listen_addr, "DogStatsD listener started.");

    loop {
        select! {
            // This signal fires when the body of the source returns. That return closes the scope that this listener
            // belongs to.
            _ = &mut process_shutdown => {
                debug!(%listen_addr, "Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            result = listener.accept() => match result {
                Ok(stream) => {
                    debug!(%listen_addr, "Spawning new stream handler.");

                    let task_name = format!("conn_{}", listen_addr.listener_type());

                    // Each handler is a child of the listener. Thus, the listener does not finish until its stream
                    // handlers finish. The handlers receive the shutdown signal when the listener no longer accepts
                    // connections.
                    let handler_source_context = source_context.clone();
                    let handler_state = Arc::clone(&listener_state);
                    let handler_datagram_sender = datagram_sender.clone();
                    runtime::worker_with_shutdown(task_name, move |shutdown| {
                        process_stream(stream, handler_source_context, handler_state, handler_datagram_sender, shutdown)
                    })
                    .spawn();
                }
                Err(e) => {
                    error!(%listen_addr, error = %e, "Failed to accept connection. Stopping listener.");
                    break
                }
            }
        }
    }

    info!(%listen_addr, "DogStatsD listener stopped accepting connections.");
}

async fn process_stream(
    stream: Stream, source_context: SourceContext, listener_state: Arc<ListenerState>,
    datagram_sender: WorkQueueSender<QueuedDatagram>, shutdown_handle: ShutdownHandle,
) {
    select! {
        _ = shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, listener_state, datagram_sender) => {},
    }
}

pub(super) async fn receive_connectionless_stream(
    mut stream: Stream, memory_limiter: MemoryLimiter, mut datagram_sender: WorkQueueSender<QueuedDatagram>,
    listener_state: Arc<ListenerState>,
) {
    let ListenerState {
        listen_addr,
        io_buffer_pool,
        decoder_context,
        capture_entity_resolver,
        ..
    } = listener_state.as_ref();

    debug!(%listen_addr, "Datagram reader started.");

    loop {
        memory_limiter.wait_for_capacity().await;

        let mut buffer = acquire_io_buffer(io_buffer_pool).await;
        let result = match stream.receive(&mut buffer).await {
            Ok((bytes_read, peer_addr)) => {
                // Pin the origin now: the sender's PID may be reused before a decoder reaches this datagram.
                let process_origin = resolve_process_origin_if_needed(
                    decoder_context.origin_detection_enabled,
                    &decoder_context.traffic_capture,
                    capture_entity_resolver.as_deref(),
                    &peer_addr,
                );
                Ok(ReceivedDatagram {
                    buffer,
                    bytes_read,
                    peer_addr,
                    process_origin,
                })
            }
            Err(error) => Err(error),
        };

        let queued = QueuedDatagram {
            result,
            listener: Arc::clone(&listener_state),
        };
        if datagram_sender.send(queued).await.is_err() {
            debug!(%listen_addr, "Failed to enqueue DogStatsD packet for decoding: receiver dropped.");
            break;
        }
    }

    debug!(%listen_addr, "Datagram reader stopped.");
}

async fn acquire_io_buffer(io_buffer_pool: &ElasticObjectPool<BytesBuffer>) -> BytesBuffer {
    let buffer = io_buffer_pool.acquire().await;
    trace!(
        remaining = buffer.remaining(),
        capacity = buffer.capacity(),
        "Acquired new buffer from pool."
    );
    buffer
}

/// Builds the interval that flushes buffered events at least every 100 milliseconds while a decoder is otherwise idle.
fn buffer_flush_interval() -> Interval {
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
    buffer_flush
}

async fn drive_datagram_decoder(
    mut datagram_receiver: WorkQueueReceiver<QueuedDatagram>, source_context: SourceContext,
    decoder_context: DecoderContext,
) {
    let mut decoder = DogStatsDDecoder::new(source_context, decoder_context);
    let mut buffer_flush = buffer_flush_interval();

    loop {
        select! {
            maybe_datagram = datagram_receiver.recv() => {
                let Some(QueuedDatagram { result, listener }) = maybe_datagram else {
                    break;
                };

                let mut received = match result {
                    Ok(received) => received,
                    Err(error) => {
                        listener.metrics.packet_receive_failure().increment(1);
                        warn!(listen_addr = %listener.listen_addr, %error, "I/O error while reading datagram. Continuing listener.");
                        continue;
                    }
                };

                let mut buffer_decode_context = BufferDecodeContext::new(&listener, BufferDecodeMode::Connectionless);
                let outcome = decoder
                    .decode_buffer(
                        &mut buffer_decode_context,
                        &mut received.buffer,
                        received.bytes_read,
                        &received.peer_addr,
                        received.process_origin.as_ref(),
                    )
                    .await;
                debug_assert_eq!(outcome, DecodeOutcome::Continue);
            }
            _ = buffer_flush.tick() => {
                decoder.flush_events().await;
            }
        }
    }

    decoder.flush_events().await;
    debug!("Datagram decoder drained its queue.");
}

async fn drive_stream(
    stream: Stream, source_context: SourceContext, listener_state: Arc<ListenerState>,
    datagram_sender: WorkQueueSender<QueuedDatagram>,
) {
    let memory_limiter = source_context.topology_context().memory_limiter().clone();
    if stream.is_connectionless() {
        receive_connectionless_stream(stream, memory_limiter, datagram_sender, listener_state).await;
    } else {
        drive_connected_stream(stream, source_context, memory_limiter, &listener_state).await;
    }
}

/// Reads and decodes a connected stream until it ends or fails.
///
/// Reads and decodes alternate on this one task: a stream is ordered, so there is nothing to decode until the read
/// before it is done.
pub(super) async fn drive_connected_stream(
    mut stream: Stream, source_context: SourceContext, memory_limiter: MemoryLimiter, listener_state: &ListenerState,
) {
    let ListenerState {
        listen_addr,
        metrics,
        io_buffer_pool,
        decoder_context,
        capture_entity_resolver,
        ..
    } = listener_state;

    let mut decoder = DogStatsDDecoder::new(source_context, decoder_context.clone());
    let mut buffer_decode_context = BufferDecodeContext::new(listener_state, BufferDecodeMode::Connected);
    let mut buffer_flush = buffer_flush_interval();

    debug!(%listen_addr, "Stream handler started.");
    metrics.connections_active().increment(1);

    // Holds a partial frame between reads, and the buffer acquired for the next read across flush ticks.
    let mut retained_buffer: Option<BytesBuffer> = None;
    loop {
        memory_limiter.wait_for_capacity().await;

        let mut buffer = match retained_buffer.take() {
            Some(mut buffer) => {
                buffer.collapse();
                buffer
            }
            None => acquire_io_buffer(io_buffer_pool).await,
        };

        // `Stream::receive` is cancel-safe, so a flush tick that wins the race loses no bytes.
        select! {
            result = stream.receive(&mut buffer) => match result {
                Ok((bytes_read, peer_addr)) => {
                    let process_origin = resolve_process_origin_if_needed(
                        decoder_context.origin_detection_enabled,
                        &decoder_context.traffic_capture,
                        capture_entity_resolver.as_deref(),
                        &peer_addr,
                    );
                    let outcome = decoder
                        .decode_buffer(
                            &mut buffer_decode_context,
                            &mut buffer,
                            bytes_read,
                            &peer_addr,
                            process_origin.as_ref(),
                        )
                        .await;
                    if outcome == DecodeOutcome::Stop {
                        break;
                    }

                    // Return a drained buffer to the pool rather than holding it while the connection idles.
                    if !buffer.has_remaining() {
                        continue;
                    }
                }
                Err(e) => {
                    metrics.packet_receive_failure().increment(1);
                    warn!(%listen_addr, error = %e, "I/O error while decoding. Stopping stream.");
                    break;
                }
            },
            _ = buffer_flush.tick() => {
                decoder.flush_events().await;
            },
        }

        retained_buffer = Some(buffer);
    }

    decoder.flush_events().await;
    metrics.connections_active().decrement(1);

    debug!(%listen_addr, "Stream handler stopped.");
}

pub(super) fn process_id_from_peer_addr(peer_addr: &ConnectionAddress) -> Option<i32> {
    match peer_addr {
        ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(creds)) => Some(creds.pid),
        _ => None,
    }
}

pub(super) fn resolve_process_origin(
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, peer_addr: &ConnectionAddress,
) -> Option<ProcessOrigin> {
    let creds = peer_addr.process_credentials()?;
    if creds.gid == REPLAY_CREDENTIALS_GID {
        return Some(ProcessOrigin::Replay(creds.uid));
    }

    let process_id = u32::try_from(creds.pid).ok()?;
    Some(match capture_entity_resolver {
        Some(resolver) => ProcessOrigin::Pinned(resolver.resolve_container_entity_for_live_pid(process_id)),
        None => ProcessOrigin::Unpinned(process_id),
    })
}

pub(super) fn resolve_process_origin_if_needed(
    origin_detection_enabled: bool, traffic_capture: &DogStatsDCaptureControl,
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, peer_addr: &ConnectionAddress,
) -> Option<ProcessOrigin> {
    if !origin_detection_enabled && !traffic_capture.is_ongoing() {
        return None;
    }

    resolve_process_origin(capture_entity_resolver, peer_addr)
}
