//! DogStatsD source.
//!
//! # Missing
//!
//! - Create a health handle for each listener.
//! - Handle UDS stream framing without treating EOF the same way as UDP and UDS datagram framing.
//! - Track dispatch failures without depending on whether all events were already iterated.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use bytesize::ByteSize;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::spawn_traced_named,
};
use saluki_component_config::dogstatsd::DogStatsDConfig;
use saluki_context::{
    origin::RawOrigin,
    tags::{RawTags, RawTagsFilter},
    TagsResolver,
};
use saluki_core::data_model::event::{
    eventd::EventD,
    metric::{Metric, MetricMetadata, MetricOrigin},
    service_check::ServiceCheck,
    Event, EventType,
};
use saluki_core::{
    components::{sources::*, ComponentContext},
    pooling::FixedSizeObjectPool,
    topology::{interconnect::EventBufferManager, EventsBuffer, OutputDefinition},
};
use saluki_env::{workload::CaptureEntityResolver, WorkloadProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, ClearableIoBuffer as _, FixedSizeVec},
    deser::{
        codec::dogstatsd::*,
        framing::{Framer as _, FramingError, LengthDelimitedFramer},
    },
    net::{
        listener::{Listener, ListenerError},
        ConnectionAddress, ListenAddress, ProcessCredentials, ProcessIdentity, Stream,
    },
};
use snafu::{ResultExt as _, Snafu};
use stringtheory::MetaString;
use tokio::{
    pin, select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

mod forwarder;
use self::forwarder::{PacketForwarder, PacketForwarderTarget};

mod framer;
use self::framer::{get_framer, DsdFramer};
use crate::sources::dogstatsd::tags::{WellKnownTags, WellKnownTagsFilterPredicate};

mod filters;
use self::filters::EnablePayloadsFilter;

mod io_buffer;
use self::io_buffer::IoBufferManager;

mod metrics;
use self::metrics::{build_metrics, Metrics};

mod replay;
use self::replay::{CaptureRecord, CapturedTaggerHandle, TrafficCapture};
pub use self::replay::{
    DogStatsDCaptureAPIHandler, DogStatsDCaptureControl, DogStatsDReplayAPIHandler, DogStatsDReplayControl,
    ReplaySession, TimestampResolution, TrafficCaptureReader, DEFAULT_REPLAY_LOOPS, REPLAY_CREDENTIALS_GID,
};

mod origin;
use self::origin::{
    mark_replay_process_id, origin_from_event_packet, origin_from_metric_packet, origin_from_service_check_packet,
    DogStatsDOriginTagResolver, OriginEnrichmentConfiguration,
};

mod resolver;
use self::resolver::ContextResolvers;

mod tags;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Failed to create {} listener: {}", listener_type, source))]
    FailedToCreateListener {
        listener_type: &'static str,
        source: ListenerError,
    },

    #[snafu(display("No listeners configured. Please specify a port (`dogstatsd_port`) or a socket path (`dogstatsd_socket` or `dogstatsd_stream_socket`) to enable a listener."))]
    NoListenersConfigured,

    #[snafu(display("Could not resolve bind_host '{}': {}", host, source))]
    UnresolvableBindHost { host: String, source: std::io::Error },

    #[snafu(display("bind_host '{}' resolved to zero IP addresses.", host))]
    BindHostHasNoAddresses { host: String },
}

/// Baseline byte cost per interner entry, used to convert the Core Agent's entry-count-based
/// `dogstatsd_string_interner_size` to a byte size.
///
/// 4096 entries × 512 bytes = 2 MiB, matching ADP's previous default.
const INTERNER_BASELINE_BYTES_PER_ENTRY: u64 = 512;

const MIN_CAPTURE_DEPTH: usize = 1024;

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
pub struct DogStatsDConfiguration {
    /// The component-native configuration for the DogStatsD source.
    config: DogStatsDConfig,

    /// Workload provider to utilize for origin detection/enrichment.
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,

    /// Resolver to use for mapping live sender PIDs to container entities during traffic capture.
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,

    /// Shared control handle for DogStatsD traffic capture.
    capture_control: DogStatsDCaptureControl,

    /// Shared control handle for DogStatsD traffic replay.
    replay_control: DogStatsDReplayControl,
}

impl Default for DogStatsDConfiguration {
    fn default() -> Self {
        Self::from_native(DogStatsDConfig::default())
    }
}

#[derive(Clone, Copy, Default)]
struct EolRequired {
    udp: bool,
    uds: bool,
}

impl EolRequired {
    fn from_config_values(values: &[String]) -> Self {
        let mut eol_required = Self::default();

        for value in values {
            match value.as_str() {
                "udp" => eol_required.udp = true,
                "uds" => eol_required.uds = true,
                "named_pipe" => {}
                _ => warn!(
                    value,
                    "Invalid dogstatsd_eol_required value. Expected 'udp', 'uds', or 'named_pipe'."
                ),
            }
        }

        eol_required
    }

    fn for_listener(&self, listen_addr: &ListenAddress) -> bool {
        match listen_addr {
            ListenAddress::Udp(_) => self.udp,
            ListenAddress::Tcp(_) => false,
            ListenAddress::Unixgram(_) | ListenAddress::Unix(_) => self.uds,
        }
    }
}

/// Resolves a `bind_host` string to an `IpAddr`.
///
/// Accepts either an IP literal (no DNS required) or a hostname (resolved via async DNS). Returns
/// `UnresolvableBindHost` if the lookup fails, or `BindHostHasNoAddresses` if it succeeds but
/// returns no addresses.
async fn resolve_bind_host(host: &str) -> Result<std::net::IpAddr, Error> {
    let mut addrs = tokio::net::lookup_host((host, 0u16))
        .await
        .context(UnresolvableBindHost { host: host.to_string() })?;
    addrs
        .next()
        .map(|sa| sa.ip())
        .ok_or_else(|| Error::BindHostHasNoAddresses { host: host.to_string() })
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` from the component-native configuration.
    ///
    /// Injected, non-config state (workload provider, capture-entity resolver, and the capture and replay control
    /// handles) starts at its default (unset/empty) value, exactly as it did before any `with_*` setter is applied.
    pub fn from_native(config: DogStatsDConfig) -> Self {
        Self {
            config,
            workload_provider: None,
            capture_entity_resolver: None,
            capture_control: DogStatsDCaptureControl::default(),
            replay_control: DogStatsDReplayControl::default(),
        }
    }

    /// Builds the origin-enrichment runtime configuration consumed by `DogStatsDOriginTagResolver`.
    ///
    /// The resolver's configuration type lives in the `origin` module with private fields and only a `Deserialize`
    /// constructor, so we project the component-native leaf values onto it through that constructor.
    fn origin_enrichment(&self) -> OriginEnrichmentConfiguration {
        let oe = &self.config.origin_enrichment;
        let value = serde_json::json!({
            "dogstatsd_origin_detection": oe.enabled,
            "dogstatsd_entity_id_precedence": oe.entity_id_precedence,
            "dogstatsd_tag_cardinality": oe.tag_cardinality.as_str(),
            "origin_detection_unified": oe.origin_detection_unified,
            "dogstatsd_origin_optout_enabled": oe.origin_detection_optout,
        });
        serde_json::from_value(value).expect("origin enrichment projection should always deserialize")
    }

    /// Gets both the `additional_tags` and any others specified by other configuration fields, such as `provider_kind`.
    fn additional_tags(&self) -> Vec<String> {
        if self.config.provider_kind.is_empty() {
            return self.config.additional_tags.clone();
        }

        let mut tags = self.config.additional_tags.clone();
        tags.push(format!("provider_kind:{}", self.config.provider_kind.clone()));
        tags
    }

    /// Returns the effective string interner size in bytes.
    ///
    /// If `dogstatsd_string_interner_size_bytes` is set, it's used directly. Otherwise,
    /// `dogstatsd_string_interner_size` (an entry count) is multiplied by 512 bytes per entry to derive the byte
    /// size.
    fn effective_context_string_interner_bytes(&self) -> ByteSize {
        match self.config.context_string_interner_size_bytes {
            Some(explicit_bytes) => explicit_bytes,
            None => ByteSize::b(self.config.context_string_interner_entry_count * INTERNER_BASELINE_BYTES_PER_ENTRY),
        }
    }

    fn eol_required(&self) -> EolRequired {
        EolRequired::from_config_values(&self.config.eol_required)
    }

    fn statsd_forward_target(&self) -> Option<(&MetaString, u16)> {
        let host = self.config.statsd_forward_host.as_ref()?;
        if self.config.statsd_forward_port == 0 {
            return None;
        }

        Some((host, self.config.statsd_forward_port))
    }

    fn packet_forwarder_target(&self) -> Option<PacketForwarderTarget> {
        let (host, port) = self.statsd_forward_target()?;
        Some(PacketForwarderTarget::new(host.clone(), port))
    }

    /// Returns the number of UDP stream handlers to spawn, derived from `dogstatsd_autoscale_udp_listeners` and
    /// the number of available vCPUs.
    ///
    /// Returns `None` when autoscaling is disabled, which keeps the legacy single-socket behavior. The platform
    /// gate for `SO_REUSEPORT` lives inside the listener—this method intentionally stays platform-agnostic.
    fn udp_streams_to_yield(&self) -> Option<NonZeroUsize> {
        if !self.config.autoscale_udp_listeners {
            return None;
        }

        #[cfg(not(target_os = "linux"))]
        if self.config.autoscale_udp_listeners {
            warn!("UDP stream handler autoscaling not supported on non-Linux platforms. Default to single stream handler.");
            return None;
        }

        let vcpus = std::thread::available_parallelism().map(NonZeroUsize::get).unwrap_or(1);
        let streams = (1 + vcpus / 8).min(4);
        NonZeroUsize::new(streams)
    }

    /// Sets the workload provider to use for configuring origin detection/enrichment.
    ///
    /// A workload provider must be set otherwise origin detection/enrichment won't be enabled.
    ///
    /// Defaults to unset.
    pub fn with_workload_provider<W>(mut self, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }

    /// Sets the resolver to use for mapping live sender PIDs while capturing DogStatsD traffic.
    ///
    /// This resolver is intentionally configured separately from the workload provider because capture only needs a
    /// narrow live-PID lookup, while normal origin enrichment uses the broader workload provider contract.
    ///
    /// Defaults to unset.
    pub fn with_capture_entity_resolver<R>(mut self, capture_entity_resolver: R) -> Self
    where
        R: CaptureEntityResolver + Send + Sync + 'static,
    {
        self.capture_entity_resolver = Some(Arc::new(capture_entity_resolver));
        self
    }

    /// Returns the shared control handle for DogStatsD traffic capture.
    pub fn capture_control(&self) -> DogStatsDCaptureControl {
        self.capture_control.clone()
    }

    /// Returns an HTTP API handler exposing the DogStatsD capture control surface.
    pub fn capture_api_handler(&self) -> DogStatsDCaptureAPIHandler {
        DogStatsDCaptureAPIHandler::new(self.capture_control.clone())
    }

    /// Returns the shared control handle for DogStatsD traffic replay.
    pub fn replay_control(&self) -> DogStatsDReplayControl {
        self.replay_control.clone()
    }

    /// Returns an HTTP API handler exposing the DogStatsD replay control surface.
    pub fn replay_api_handler(&self) -> DogStatsDReplayAPIHandler {
        DogStatsDReplayAPIHandler::new(self.replay_control.clone())
    }

    /// Using the current configuration, determines which listeners should be created and adds an address for each into
    /// a `Vec<ListenAddress>`. This function has no side effects so that it can be unit tested whereas build_listeners`
    /// actually binds the listeners on the system.
    ///
    /// `bind_host` is the pre-resolved IP that UDP and TCP listeners should bind to (provided by
    /// `resolve_bind_host`). Precedence matches the Agent:
    ///   - `non_local_traffic=true` → `0.0.0.0` (`bind_host` ignored)
    ///   - `bind_host=Some(ip)`     → `ip`
    ///   - `bind_host=None`         → `127.0.0.1`
    fn build_addresses(&self, bind_host: Option<std::net::IpAddr>) -> Vec<ListenAddress> {
        let bind_ip: std::net::IpAddr = if self.config.non_local_traffic {
            [0, 0, 0, 0].into()
        } else {
            bind_host.unwrap_or_else(|| [127, 0, 0, 1].into())
        };

        let mut addresses: Vec<ListenAddress> = Vec::new();

        if self.config.port != 0 {
            addresses.push(ListenAddress::Udp(std::net::SocketAddr::new(bind_ip, self.config.port)));
        }

        if self.config.tcp_port != 0 {
            addresses.push(ListenAddress::Tcp(std::net::SocketAddr::new(
                bind_ip,
                self.config.tcp_port,
            )));
        }

        if let Some(socket_path) = &self.config.socket_path {
            addresses.push(ListenAddress::Unixgram(socket_path.into()));
        }

        if let Some(socket_stream_path) = &self.config.socket_stream_path {
            addresses.push(ListenAddress::Unix(socket_stream_path.into()));
        }

        addresses
    }

    /// Builds the appropriate `Listener` objects.
    async fn build_listeners(&self) -> Result<Vec<Listener>, Error> {
        // Resolve `bind_host` to an IP (via DNS if needed). Skip the lookup when
        // `non_local_traffic=true` since `bind_host` is ignored in that branch—matches Go's
        // laziness and avoids failing startup on an unresolvable hostname that wouldn't be used.
        let bind_host: Option<std::net::IpAddr> = if self.config.non_local_traffic {
            None
        } else {
            match &self.config.bind_host {
                Some(host) => Some(resolve_bind_host(host).await?),
                None => None,
            }
        };

        let addresses = self.build_addresses(bind_host);
        let mut listeners = Vec::new();
        let socket_receive_buffer_size =
            (self.config.socket_receive_buffer_size != 0).then_some(self.config.socket_receive_buffer_size);
        let udp_streams_to_yield = self.udp_streams_to_yield();
        for address in addresses {
            let listener_type = address.listener_type();
            let listener_streams = matches!(address, ListenAddress::Udp(_))
                .then_some(udp_streams_to_yield)
                .flatten();
            let listener = Listener::from_listen_address(address, listener_streams)
                .await
                .context(FailedToCreateListener { listener_type })?
                .with_receive_buffer_size(socket_receive_buffer_size);

            listeners.push(listener);
        }
        Ok(listeners)
    }
}

#[async_trait]
impl SourceBuilder for DogStatsDConfiguration {
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let listeners = self.build_listeners().await?;
        if listeners.is_empty() {
            return Err(Error::NoListenersConfigured.into());
        }

        // Every listener requires at least one I/O buffer to ensure that all listeners can be serviced without
        // deadlocking any of the others. Connectionless listeners retain their buffer for the lifetime of the stream,
        // so multi-socket UDP listeners require one buffer per yielded socket.
        let min_buffers: usize = listeners.iter().map(Listener::min_buffer_reservation).sum();
        if self.config.buffer_count < min_buffers {
            return Err(generic_error!(
                "Must have a minimum of {} I/O buffers to service all configured listeners (have {}).",
                min_buffers,
                self.config.buffer_count,
            ));
        }

        let origin_enrichment = self.origin_enrichment();
        let origin_detection_enabled = origin_enrichment.enabled();
        // Single CapturedTaggerHandle is cloned to both the resolver (reader of the captured store) and the replay
        // control surface (writer). Both sides reference the same atomic slot.
        let captured_tagger = CapturedTaggerHandle::new();

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(|provider| {
            DogStatsDOriginTagResolver::new(origin_enrichment.clone(), provider, captured_tagger.clone())
        });
        let context_resolvers = ContextResolvers::new(self, &context, maybe_origin_tags_resolver)
            .error_context("Failed to create context resolvers.")?;

        let codec_config = DogStatsDCodecConfiguration::default()
            .with_timestamps(self.config.no_aggregation_pipeline_support)
            .with_permissive_mode(self.config.permissive_decoding)
            .with_minimum_sample_rate(self.config.minimum_sample_rate)
            .with_client_origin_detection(self.config.origin_enrichment.origin_detection_client);

        let codec: DogStatsDCodec = codec_config.into();
        let eol_required = self.eol_required();

        let enable_payloads_filter = EnablePayloadsFilter::default()
            .with_allow_series(self.config.enable_payloads.series)
            .with_allow_sketches(self.config.enable_payloads.sketches)
            .with_allow_events(self.config.enable_payloads.events)
            .with_allow_service_checks(self.config.enable_payloads.service_checks);
        let traffic_capture = TrafficCapture::with_workload_provider(
            self.config.capture_path.clone(),
            self.config.capture_depth.max(MIN_CAPTURE_DEPTH),
            self.workload_provider.clone(),
        );
        self.capture_control.bind(traffic_capture.clone());
        let packet_forwarder_target = self.packet_forwarder_target();

        self.replay_control.bind(captured_tagger);

        Ok(Box::new(DogStatsD {
            listeners,
            io_buffer_pool: FixedSizeObjectPool::with_builder("dsd_packet_bufs", self.config.buffer_count, || {
                FixedSizeVec::with_capacity(get_adjusted_buffer_size(self.config.buffer_size))
            }),
            codec,
            context_resolvers,
            enabled_filter: enable_payloads_filter,
            origin_detection_enabled,
            stream_log_too_big: self.config.stream_log_too_big,
            disable_verbose_logs: self.config.disable_verbose_logs,
            eol_required,
            additional_tags: self.additional_tags().into(),
            capture_entity_resolver: self.capture_entity_resolver.clone(),
            traffic_capture,
            packet_forwarder_target,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("events", EventType::EventD),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
            ]
        });
        &OUTPUTS
    }
}

impl MemoryBounds for DogStatsDConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DogStatsD>("source struct")
            // We allocate our I/O buffers entirely up front.
            .with_expr(UsageExpr::product(
                "buffers",
                UsageExpr::config("dogstatsd_buffer_count", self.config.buffer_count),
                UsageExpr::config(
                    "dogstatsd_buffer_size",
                    get_adjusted_buffer_size(self.config.buffer_size),
                ),
            ))
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_expr(UsageExpr::config(
                "dogstatsd_string_interner_size_bytes",
                self.effective_context_string_interner_bytes().as_u64() as usize,
            ));
    }
}

/// DogStatsD source.
pub struct DogStatsD {
    listeners: Vec<Listener>,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    enabled_filter: EnablePayloadsFilter,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    eol_required: EolRequired,
    additional_tags: Arc<[String]>,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    traffic_capture: TrafficCapture,
    packet_forwarder_target: Option<PacketForwarderTarget>,
}

struct ListenerContext {
    shutdown_handle: ShutdownHandle,
    listener: Listener,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    eol_required: EolRequired,
    additional_tags: Arc<[String]>,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    traffic_capture: TrafficCapture,
    packet_forwarder_target: Option<PacketForwarderTarget>,
}

struct HandlerContext {
    listen_addr: ListenAddress,
    framer: DsdFramer,
    codec: DogStatsDCodec,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    metrics: Metrics,
    context_resolvers: ContextResolvers,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    additional_tags: Arc<[String]>,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    traffic_capture: TrafficCapture,
    packet_forwarder: Option<PacketForwarder>,
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();

        let mut listener_shutdown_coordinator = ShutdownCoordinator::default();

        // For each listener, spawn a dedicated task to run it.
        for listener in self.listeners {
            let task_name = format!("dogstatsd-listener-{}", listener.listen_address().listener_type());

            // TODO: Create a health handle for each listener.
            //
            // We need to rework `HealthRegistry` to look a little more like `ComponentRegistry` so that we can have it
            // already be scoped properly, otherwise all we can do here at present is either have a relative name, like
            // `uds-stream`, or try and hardcode the full component name, which we will inevitably forget to update if
            // we tweak the topology configuration, etc.
            let listener_context = ListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                listener,
                io_buffer_pool: self.io_buffer_pool.clone(),
                codec: self.codec.clone(),
                context_resolvers: self.context_resolvers.clone(),
                origin_detection_enabled: self.origin_detection_enabled,
                stream_log_too_big: self.stream_log_too_big,
                disable_verbose_logs: self.disable_verbose_logs,
                eol_required: self.eol_required,
                additional_tags: self.additional_tags.clone(),
                capture_entity_resolver: self.capture_entity_resolver.clone(),
                traffic_capture: self.traffic_capture.clone(),
                packet_forwarder_target: self.packet_forwarder_target.clone(),
            };

            spawn_traced_named(
                task_name,
                process_listener(context.clone(), listener_context, self.enabled_filter),
            );
        }

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

        debug!("Stopping DogStatsD source...");

        listener_shutdown_coordinator.shutdown_and_wait().await;

        debug!("DogStatsD source stopped.");

        Ok(())
    }
}

async fn process_listener(
    source_context: SourceContext, listener_context: ListenerContext, enabled_filter: EnablePayloadsFilter,
) {
    let ListenerContext {
        shutdown_handle,
        mut listener,
        io_buffer_pool,
        codec,
        context_resolvers,
        origin_detection_enabled,
        stream_log_too_big,
        disable_verbose_logs,
        eol_required,
        additional_tags,
        capture_entity_resolver,
        traffic_capture,
        packet_forwarder_target,
    } = listener_context;

    pin!(shutdown_handle);

    let listen_addr = listener.listen_address().clone();
    let metrics = build_metrics(&listen_addr, source_context.component_context());
    let packet_forwarder = packet_forwarder_target
        .as_ref()
        .map(|target| target.to_forwarder(metrics.clone()));
    if let Some(packet_forwarder) = &packet_forwarder {
        packet_forwarder.spawn_connect();
    }

    let mut stream_shutdown_coordinator = ShutdownCoordinator::default();
    let mut stream_idx: u32 = 0;

    info!(%listen_addr, "DogStatsD listener started.");

    loop {
        select! {
            _ = &mut shutdown_handle => {
                debug!(%listen_addr, "Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            result = listener.accept() => match result {
                Ok(stream) => {
                    debug!(%listen_addr, "Spawning new stream handler.");

                    let handler_context = HandlerContext {
                        listen_addr: listen_addr.clone(),
                        framer: get_framer(&listen_addr, eol_required.for_listener(&listen_addr)),
                        codec: codec.clone(),
                        io_buffer_pool: io_buffer_pool.clone(),
                        metrics: metrics.clone(),
                        context_resolvers: context_resolvers.clone(),
                        origin_detection_enabled,
                        stream_log_too_big,
                        disable_verbose_logs,
                        additional_tags: additional_tags.clone(),
                        capture_entity_resolver: capture_entity_resolver.clone(),
                        traffic_capture: traffic_capture.clone(),
                        packet_forwarder: packet_forwarder.clone(),
                    };

                    let task_name = format!(
                        "dogstatsd-stream-handler-{}-{}",
                        listen_addr.listener_type(),
                        stream_idx,
                    );
                    stream_idx = stream_idx.wrapping_add(1);
                    spawn_traced_named(task_name, process_stream(stream, source_context.clone(), handler_context, stream_shutdown_coordinator.register(), enabled_filter));
                }
                Err(e) => {
                    error!(%listen_addr, error = %e, "Failed to accept connection. Stopping listener.");
                    break
                }
            }
        }
    }

    stream_shutdown_coordinator.shutdown_and_wait().await;

    info!(%listen_addr, "DogStatsD listener stopped.");
}

async fn process_stream(
    stream: Stream, source_context: SourceContext, handler_context: HandlerContext, shutdown_handle: ShutdownHandle,
    enabled_filter: EnablePayloadsFilter,
) {
    select! {
        _ = shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, handler_context, enabled_filter) => {},
    }
}

async fn drive_stream(
    mut stream: Stream, source_context: SourceContext, handler_context: HandlerContext,
    enabled_filter: EnablePayloadsFilter,
) {
    let HandlerContext {
        listen_addr,
        mut framer,
        codec,
        io_buffer_pool,
        metrics,
        mut context_resolvers,
        origin_detection_enabled,
        stream_log_too_big,
        disable_verbose_logs,
        additional_tags,
        capture_entity_resolver,
        traffic_capture,
        packet_forwarder,
    } = handler_context;

    debug!(%listen_addr, "Stream handler started.");

    if !stream.is_connectionless() {
        metrics.connections_active().increment(1);
    }

    let mut stream_capture = StreamCaptureState::new();
    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();
    let mut io_buffer_manager = IoBufferManager::new(&io_buffer_pool, &stream);
    let memory_limiter = source_context.topology_context().memory_limiter();

    'read: loop {
        let mut eof = false;

        let mut io_buffer = io_buffer_manager.get_buffer_mut().await;

        memory_limiter.wait_for_capacity().await;

        select! {
            // We read from the stream.
            read_result = stream.receive(&mut io_buffer) => match read_result {
                Ok((bytes_read, peer_addr)) => {
                    if bytes_read == 0 {
                        eof = true;
                    }

                    let is_connectionless = stream.is_connectionless();
                    let payload = received_payload(io_buffer, bytes_read);

                    capture_uds_traffic(
                        &listen_addr,
                        &traffic_capture,
                        capture_entity_resolver.as_deref(),
                        &peer_addr,
                        payload,
                        &mut stream_capture,
                    );

                    if is_connectionless {
                        metrics.packet_receive_success().increment(1);
                    }
                    metrics.bytes_received().increment(bytes_read as u64);
                    metrics.bytes_received_size().record(bytes_read as f64);
                    let origin_detection_failed =
                        origin_detection_enabled && bytes_read > 0 && peer_addr.has_process_credential_error();
                    if origin_detection_failed && is_connectionless {
                        metrics.origin_detection_errors().increment(1);
                    }

                    // When we're actually at EOF, or we're dealing with a connectionless stream, we try to decode in EOF mode.
                    //
                    // For connectionless streams, we always try to decode the buffer as if it's EOF, since it effectively _is_
                    // always the end of file after a receive. For connection-oriented streams, we only want to do this once we've
                    // actually hit true EOF.
                    let reached_eof = eof || is_connectionless;

                    trace!(
                        buffer_len = io_buffer.remaining(),
                        buffer_cap = io_buffer.remaining_mut(),
                        eof = reached_eof,
                        %listen_addr,
                        %peer_addr,
                        "Received {} bytes from stream.",
                        bytes_read
                    );

                    'frame: loop {
                        let frame_result = framer.next_frame(io_buffer, reached_eof);
                        let completed_outer_frames = framer.take_completed_outer_frames();
                        if !is_connectionless && completed_outer_frames > 0 {
                            metrics.packet_receive_success().increment(completed_outer_frames as u64);
                        }
                        if origin_detection_failed && completed_outer_frames > 0 {
                            metrics.origin_detection_errors().increment(completed_outer_frames as u64);
                        }

                        match frame_result {
                            Ok(Some(frame)) => {
                                trace!(%listen_addr, %peer_addr, ?frame, "Decoded frame.");
                                if let Some(forwarder) = &packet_forwarder {
                                    forwarder.forward(frame.clone()).await;
                                }
                                match handle_frame(&frame[..], &codec, &mut context_resolvers, &metrics, &peer_addr, enabled_filter, &additional_tags) {
                                    Ok(Some(event)) => {
                                        if let Some(event_buffer) = event_buffer_manager.try_push(event) {
                                            debug!(%listen_addr, %peer_addr, "Event buffer is full. Forwarding events.");
                                            dispatch_events(event_buffer, &source_context, &listen_addr).await;
                                        }
                                    },
                                    Ok(None) => {
                                        // We didn't decode an event, but there was no inherent error. This is likely
                                        // due to hitting resource limits, etc.
                                        //
                                        // Simply continue on.
                                        continue
                                    },
                                    Err(e) => {
                                        log_parse_failure(disable_verbose_logs, &listen_addr, &peer_addr, &frame, &e);
                                    },
                                }
                            }
                            Err(e) => {
                                metrics.framing_errors().increment(1);
                                if should_warn_stream_log_too_big(&listen_addr, &e, stream_log_too_big) {
                                    warn!(
                                        %listen_addr,
                                        %peer_addr,
                                        error = %e,
                                        "DogStatsD stream frame exceeded the configured buffer size."
                                    );
                                }

                                if stream.is_connectionless() {
                                    io_buffer.clear();
                                    // For connectionless streams, we don't want to shutdown the stream since we can just keep
                                    // reading more packets.
                                    debug!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Continuing stream.");
                                    continue 'read;
                                } else {
                                    debug!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Stopping stream.");
                                    break 'read;
                                }
                            }
                            Ok(None) => {
                                trace!(%listen_addr, %peer_addr, "Not enough data to decode another frame.");
                                if eof && !stream.is_connectionless() {
                                    debug!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                                    break 'read;
                                } else {
                                    break 'frame;
                                }
                            }
                        }
                    }
                },
                Err(e) => {
                    metrics.packet_receive_failure().increment(1);

                    if stream.is_connectionless() {
                        // For connectionless streams, we don't want to shutdown the stream since we can just keep
                        // reading more packets.
                        warn!(%listen_addr, error = %e, "I/O error while decoding. Continuing stream.");
                        continue 'read;
                    } else {
                        warn!(%listen_addr, error = %e, "I/O error while decoding. Stopping stream.");
                        break 'read;
                    }
                }
            },

            _ = buffer_flush.tick() => {
                if let Some(event_buffer) = event_buffer_manager.consume() {
                    dispatch_events(event_buffer, &source_context, &listen_addr).await;
                }
            },

        }
    }

    if let Some(event_buffer) = event_buffer_manager.consume() {
        dispatch_events(event_buffer, &source_context, &listen_addr).await;
    }

    metrics.connections_active().decrement(1);

    debug!(%listen_addr, "Stream handler stopped.");
}

fn should_warn_stream_log_too_big(listen_addr: &ListenAddress, error: &FramingError, stream_log_too_big: bool) -> bool {
    stream_log_too_big
        && matches!(listen_addr, ListenAddress::Unix(_))
        && matches!(error, FramingError::InvalidFrame { .. })
}

fn log_parse_failure(
    disable_verbose_logs: bool, listen_addr: &ListenAddress, peer_addr: &ConnectionAddress, frame: &[u8],
    error: &ParseError,
) {
    let frame = String::from_utf8_lossy(frame);
    if disable_verbose_logs {
        debug!(%listen_addr, %peer_addr, %frame, %error, "Failed to parse frame.");
    } else {
        warn!(%listen_addr, %peer_addr, %frame, %error, "Failed to parse frame.");
    }
}

fn capture_uds_traffic(
    listen_addr: &ListenAddress, traffic_capture: &TrafficCapture,
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, peer_addr: &ConnectionAddress,
    payload: &[u8], stream_capture: &mut StreamCaptureState,
) {
    if payload.is_empty() || !traffic_capture.is_ongoing() {
        return;
    }

    match listen_addr {
        ListenAddress::Unixgram(_) => {
            let _ = traffic_capture.enqueue(build_capture_record(
                capture_entity_resolver,
                process_id_from_peer_addr(peer_addr),
                payload,
            ));
        }
        ListenAddress::Unix(_) => {
            stream_capture.update_peer_metadata(peer_addr);
            stream_capture.pending.extend(payload);

            while let Ok(Some(outer_payload)) = stream_capture
                .outer_framer
                .next_frame(&mut stream_capture.pending, false)
            {
                let _ = traffic_capture.enqueue(build_capture_record(
                    capture_entity_resolver,
                    stream_capture.last_pid,
                    &outer_payload,
                ));
            }
        }
        _ => {}
    }
}

struct StreamCaptureState {
    outer_framer: LengthDelimitedFramer,
    pending: VecDeque<u8>,
    last_pid: Option<i32>,
}

impl StreamCaptureState {
    fn new() -> Self {
        Self {
            outer_framer: LengthDelimitedFramer,
            pending: VecDeque::new(),
            last_pid: None,
        }
    }

    fn update_peer_metadata(&mut self, peer_addr: &ConnectionAddress) {
        if let Some(process_id) = process_id_from_peer_addr(peer_addr) {
            self.last_pid = Some(process_id);
        }
    }
}

fn build_capture_record(
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, process_id: Option<i32>,
    payload: &[u8],
) -> CaptureRecord {
    CaptureRecord {
        timestamp_ns: capture_timestamp_ns(),
        payload: payload.to_vec(),
        pid: process_id,
        ancillary: Vec::new(),
        container_id: resolve_capture_container_id(capture_entity_resolver, process_id),
    }
}

fn resolve_capture_container_id(
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, process_id: Option<i32>,
) -> Option<String> {
    let process_id = u32::try_from(process_id?).ok()?;
    capture_entity_resolver
        .and_then(|resolver| resolver.resolve_container_entity_for_live_pid(process_id))
        .map(|entity_id| entity_id.to_string())
}

fn process_id_from_peer_addr(peer_addr: &ConnectionAddress) -> Option<i32> {
    match peer_addr {
        ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(creds)) => Some(creds.pid),
        _ => None,
    }
}

/// Applies SCM_CREDENTIALS to the origin, dispatching on the replay marker GID.
///
/// Live packets carry the sender process's real PID/UID/GID; we use `creds.pid` as the origin's process ID. Replay
/// packets carry `gid == REPLAY_CREDENTIALS_GID` and pack the captured (original) PID into `creds.uid`; we recover
/// that PID with an internal marker so downstream tag resolution consults the captured tagger store.
fn apply_credentials_to_origin(origin: &mut RawOrigin<'_>, creds: &ProcessCredentials) {
    if creds.gid == REPLAY_CREDENTIALS_GID {
        origin.set_process_id(mark_replay_process_id(creds.uid));
    } else {
        origin.set_process_id(creds.pid as u32);
    }
}

fn received_payload(buffer: &BytesBuffer, bytes_read: usize) -> &[u8] {
    let chunk = buffer.chunk();
    let start = chunk.len().saturating_sub(bytes_read);
    &chunk[start..]
}

fn capture_timestamp_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn handle_frame(
    frame: &[u8], codec: &DogStatsDCodec, context_resolvers: &mut ContextResolvers, source_metrics: &Metrics,
    peer_addr: &ConnectionAddress, enabled_filter: EnablePayloadsFilter, additional_tags: &[String],
) -> Result<Option<Event>, ParseError> {
    let parsed = match codec.decode_packet(frame) {
        Ok(parsed) => parsed,
        Err(e) => {
            // Try and determine what the message type was, if possible, to increment the correct error counter.
            match parse_message_type(frame) {
                MessageType::MetricSample => source_metrics.metric_decode_failed().increment(1),
                MessageType::Event => source_metrics.event_decode_failed().increment(1),
                MessageType::ServiceCheck => source_metrics.service_check_decode_failed().increment(1),
            }

            return Err(e);
        }
    };

    let event = match parsed {
        ParsedPacket::Metric(metric_packet) => {
            if metric_packet.num_points == 0 {
                return Ok(None);
            }
            let events_len = metric_packet.num_points;
            if !enabled_filter.allow_metric(&metric_packet) {
                trace!(
                    metric.name = metric_packet.metric_name,
                    "Skipping metric due to filter configuration."
                );
                return Ok(None);
            }

            match handle_metric_packet(metric_packet, context_resolvers, peer_addr, additional_tags) {
                Some(metric) => {
                    source_metrics.metrics_received().increment(events_len);
                    Event::Metric(metric)
                }
                None => {
                    // We can only fail to get a metric back if we failed to resolve the context.
                    source_metrics.failed_context_resolve_total().increment(1);
                    return Ok(None);
                }
            }
        }
        ParsedPacket::Event(event) => {
            if !enabled_filter.allow_event(&event) {
                trace!("Skipping event {} due to filter configuration.", event.title);
                return Ok(None);
            }
            let tags_resolver = context_resolvers.tags();
            match handle_event_packet(event, tags_resolver, peer_addr, additional_tags) {
                Some(event) => {
                    source_metrics.events_received().increment(1);
                    Event::EventD(event)
                }
                None => {
                    source_metrics.failed_context_resolve_total().increment(1);
                    return Ok(None);
                }
            }
        }
        ParsedPacket::ServiceCheck(service_check) => {
            if !enabled_filter.allow_service_check(&service_check) {
                trace!(
                    "Skipping service check {} due to filter configuration.",
                    service_check.name
                );
                return Ok(None);
            }
            let tags_resolver = context_resolvers.tags();
            match handle_service_check_packet(service_check, tags_resolver, peer_addr, additional_tags) {
                Some(service_check) => {
                    source_metrics.service_checks_received().increment(1);
                    Event::ServiceCheck(service_check)
                }
                None => {
                    source_metrics.failed_context_resolve_total().increment(1);
                    return Ok(None);
                }
            }
        }
    };

    Ok(Some(event))
}

fn handle_metric_packet(
    packet: MetricPacket, context_resolvers: &mut ContextResolvers, peer_addr: &ConnectionAddress,
    additional_tags: &[String],
) -> Option<Metric> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let mut origin = origin_from_metric_packet(&packet, &well_known_tags);
    if let Some(creds) = peer_addr.process_credentials() {
        apply_credentials_to_origin(&mut origin, creds);
    }

    // Choose the right context resolver based on whether or not this metric is pre-aggregated.
    let context_resolver = if packet.timestamp.is_some() {
        context_resolvers.no_agg()
    } else {
        context_resolvers.primary()
    };

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);

    // Try to resolve the context for this metric.
    match context_resolver.resolve(packet.metric_name, tags, Some(origin)) {
        Some(context) => {
            let metric_origin = well_known_tags
                .jmx_check_name
                .map(MetricOrigin::jmx_check)
                .unwrap_or_else(MetricOrigin::dogstatsd);
            let metadata = MetricMetadata::default()
                .with_origin(metric_origin)
                .with_hostname(well_known_tags.hostname.map(Arc::from))
                .with_unit(packet.unit.map_or_else(MetaString::empty, MetaString::from_static));

            Some(Metric::from_parts(context, packet.values, metadata))
        }
        // We failed to resolve the context, likely due to not having enough interner capacity.
        None => None,
    }
}

fn handle_event_packet(
    packet: EventPacket, tags_resolver: &mut TagsResolver, peer_addr: &ConnectionAddress, additional_tags: &[String],
) -> Option<EventD> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let mut origin = origin_from_event_packet(&packet, &well_known_tags);
    if let Some(creds) = peer_addr.process_credentials() {
        apply_credentials_to_origin(&mut origin, creds);
    }
    let origin_tags = tags_resolver.resolve_origin_tags(Some(origin));

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);
    let tags = tags_resolver.create_tag_set(tags)?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior in pkg/aggregator/aggregator.go (addEvent), which sets e.Ts = time.Now().Unix()
    // for any event with Ts == 0.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let eventd = EventD::new(packet.title, packet.text)
        .with_timestamp(timestamp)
        .with_hostname(packet.hostname.map(|s| s.into()))
        .with_aggregation_key(packet.aggregation_key.map(|s| s.into()))
        .with_alert_type(packet.alert_type)
        .with_priority(packet.priority)
        // When no source type is provided, default to "api"—the same default the stock Datadog
        // Agent applies when serializing DogStatsD events to the intake JSON format. The agent
        // groups events by source type name and uses "api" as the key for events without an
        // explicit `s:` field. See: pkg/serializer/internal/metrics/events.go (writeItem).
        .with_source_type_name(Some(
            packet
                .source_type_name
                .map(|s| s.into())
                .unwrap_or_else(|| "api".into()),
        ))
        .with_alert_type(packet.alert_type)
        .with_tags(tags)
        .with_origin_tags(origin_tags);

    Some(eventd)
}

fn handle_service_check_packet(
    packet: ServiceCheckPacket, tags_resolver: &mut TagsResolver, peer_addr: &ConnectionAddress,
    additional_tags: &[String],
) -> Option<ServiceCheck> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let mut origin = origin_from_service_check_packet(&packet, &well_known_tags);
    if let Some(creds) = peer_addr.process_credentials() {
        apply_credentials_to_origin(&mut origin, creds);
    }
    let origin_tags = tags_resolver.resolve_origin_tags(Some(origin));

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);
    let tags = tags_resolver.create_tag_set(tags)?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior, which sets the timestamp to time.Now().Unix() for any service check with a zero
    // timestamp.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let service_check = ServiceCheck::new(packet.name, packet.status)
        .with_timestamp(timestamp)
        .with_hostname(packet.hostname.map(|s| s.into()))
        .with_tags(tags)
        .with_origin_tags(origin_tags)
        .with_message(packet.message.map(|s| s.into()));

    Some(service_check)
}

fn get_filtered_tags_iterator<'a>(
    raw_tags: RawTags<'a>, additional_tags: &'a [String],
) -> impl Iterator<Item = &'a str> + Clone {
    // This filters out "well-known" tags from the raw tags in the DogStatsD packet, and then chains on any additional tags
    // that were configured on the source.
    RawTagsFilter::exclude(raw_tags, WellKnownTagsFilterPredicate).chain(additional_tags.iter().map(|s| s.as_str()))
}

async fn dispatch_events(mut event_buffer: EventsBuffer, source_context: &SourceContext, listen_addr: &ListenAddress) {
    debug!(%listen_addr, events_len = event_buffer.len(), "Forwarding events.");

    // TODO: This is maybe a little dicey because if we fail to dispatch the events, we may not have iterated over all of
    // them, so there might still be eventd events when get to the service checks point, and eventd events and/or service
    // check events when we get to the metrics point, and so on.
    //
    // There's probably something to be said for erroring out fully if this happens, since we should only fail to
    // dispatch if the downstream component fails entirely... and unless we have a way to restart the component, then
    // we're going to continue to fail to dispatch any more events until the process is restarted anyways.

    // Dispatch any eventd events, if present.
    if event_buffer.has_event_type(EventType::EventD) {
        let eventd_events = event_buffer.extract(Event::is_eventd);
        if let Err(e) = source_context
            .dispatcher()
            .buffered_named("events")
            .expect("events output should always exist")
            .send_all(eventd_events)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch eventd events.");
        }
    }

    // Dispatch any service check events, if present.
    if event_buffer.has_event_type(EventType::ServiceCheck) {
        let service_check_events = event_buffer.extract(Event::is_service_check);
        if let Err(e) = source_context
            .dispatcher()
            .buffered_named("service_checks")
            .expect("service checks output should always exist")
            .send_all(service_check_events)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch service check events.");
        }
    }

    // Finally, if there are events left, they'll be metrics, so dispatch them.
    if !event_buffer.is_empty() {
        if let Err(e) = source_context
            .dispatcher()
            .dispatch_named("metrics", event_buffer)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch metric events.");
        }
    }
}

const fn get_adjusted_buffer_size(buffer_size: usize) -> usize {
    // This is a little goofy, but hear me out:
    //
    // In the Datadog Agent, the way the UDS listener works is that if it's in stream mode, it will do a standalone
    // socket read to get _just_ the length delimiter, which is 4 bytes. After that, it will do a read to get the packet
    // data itself, up to the limit of `dogstatsd_buffer_size`. This means that a _full_ UDS stream packet can be up to
    // `dogstatsd_buffer_size + 4` bytes.
    //
    // This isn't a problem in the Agent due to how it does the reads, but it's a problem for us because we want to be
    // able to get an entire frame in a single buffer for the purpose of decoding the frame. Rather than rewriting our
    // read loop such that we have to change the logic depending on UDP/UDS datagram vs UDS stream, we simply increase
    // the buffer size by 4 bytes to account for the length delimiter.
    //
    // We do it this way so that we don't have to change the buffer size in the configuration, since if you just ported
    // over a Datadog Agent configuration, the value would be too small, and vise versa.
    buffer_size + 4
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::ErrorKind,
        net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
        sync::{Arc, OnceLock},
        time::Duration,
    };

    use bytes::Bytes;
    use bytesize::ByteSize;
    use saluki_component_config::dogstatsd::DogStatsDConfig;
    use saluki_context::{origin::RawOrigin, ContextResolverBuilder, TagsResolverBuilder};
    use saluki_core::{components::ComponentContext, topology::ComponentId};
    use saluki_env::workload::{CaptureEntityResolver, EntityId};
    use saluki_io::{
        deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration, ParsedPacket},
        net::{ConnectionAddress, ListenAddress, ProcessCredentials, ProcessIdentity},
    };
    use saluki_metrics::test::TestRecorder;
    use stringtheory::MetaString;
    use tokio::{net::UdpSocket, sync::mpsc, time::timeout};

    use super::{
        forwarder::{
            ConnectedPacketForwarder, ForwardPacket, PacketForwarder, PacketForwarderTarget, FORWARDER_QUEUE_CAPACITY,
        },
        handle_metric_packet,
        metrics::build_metrics,
        resolve_capture_container_id, ContextResolvers, DogStatsDConfiguration,
    };

    const LINUX_EAFNOSUPPORT: i32 = 97;
    const MACOS_EAFNOSUPPORT: i32 = 47;

    fn is_ipv6_unavailable_error(error: &std::io::Error) -> bool {
        matches!(error.kind(), ErrorKind::AddrNotAvailable | ErrorKind::Unsupported)
            || matches!(error.raw_os_error(), Some(LINUX_EAFNOSUPPORT | MACOS_EAFNOSUPPORT))
    }

    #[derive(Default)]
    struct CaptureTestEntityResolver {
        pid_map: HashMap<u32, EntityId>,
    }

    impl CaptureTestEntityResolver {
        fn with_pid_mapping(process_id: u32, entity_id: EntityId) -> Self {
            let mut pid_map = HashMap::new();
            pid_map.insert(process_id, entity_id);
            Self { pid_map }
        }
    }

    impl CaptureEntityResolver for CaptureTestEntityResolver {
        fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
            self.pid_map.get(&process_id).cloned()
        }
    }

    fn packet_forwarder_from_sender(
        target_port: u16, packets_tx: mpsc::Sender<ForwardPacket>, metrics: super::metrics::Metrics,
    ) -> PacketForwarder {
        let mut forwarder =
            PacketForwarderTarget::new(MetaString::from_static("127.0.0.1"), target_port).to_forwarder(metrics);
        forwarder.connected = Arc::new(OnceLock::from(packets_tx));
        forwarder
    }

    #[test]
    fn no_metrics_when_interner_full_allocations_disallowed() {
        // We're specifically testing here that when we don't allow outside allocations, we should not be able to
        // resolve a context if the interner is full. A no-op interner has the smallest possible size, so that's going
        // to assure we can't intern anything... but we also need a string (name or one of the tags) that can't be
        // _inlined_ either, since that will get around the interner being full.
        //
        // We set our metric name to be longer than 31 bytes (the inlining limit) to ensure this.

        let codec: DogStatsDCodec = DogStatsDCodecConfiguration::default().into();
        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_heap_allocations(false)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();
        let mut context_resolvers = ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver);
        let peer_addr = ConnectionAddress::from("1.1.1.1:1234".parse::<SocketAddr>().unwrap());

        let input = "big_metric_name_that_cant_possibly_be_inlined:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
            panic!("Failed to parse packet.");
        };

        let maybe_metric = handle_metric_packet(packet, &mut context_resolvers, &peer_addr, &[]);
        assert!(maybe_metric.is_none());
    }

    #[test]
    fn metric_with_additional_tags() {
        let codec: DogStatsDCodec = DogStatsDCodecConfiguration::default().into();
        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_heap_allocations(false)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();
        let mut context_resolvers = ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver);
        let peer_addr = ConnectionAddress::from("1.1.1.1:1234".parse::<SocketAddr>().unwrap());

        let existing_tags = ["tag1:value1", "tag2:value2", "tag3:value3"];
        let existing_tags_str = existing_tags.join(",");

        let input = format!("test_metric_name:1|c|#{}", existing_tags_str);
        let additional_tags = [
            "tag4:value4".to_string(),
            "tag5:value5".to_string(),
            "tag6:value6".to_string(),
        ];

        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
            panic!("Failed to parse packet.");
        };
        let maybe_metric = handle_metric_packet(packet, &mut context_resolvers, &peer_addr, &additional_tags);
        assert!(maybe_metric.is_some());

        let metric = maybe_metric.unwrap();
        let context = metric.context();

        for tag in existing_tags {
            assert!(context.tags().has_tag(tag));
        }

        for tag in additional_tags {
            assert!(context.tags().has_tag(tag));
        }
    }

    /// Builds a `DogStatsDConfiguration` from a component-native leaf config, mirroring how the source is constructed
    /// in production (no injected workload provider, resolvers, or control handles unless set explicitly).
    fn native_config(config: DogStatsDConfig) -> DogStatsDConfiguration {
        DogStatsDConfiguration::from_native(config)
    }

    fn udp_listen_address() -> ListenAddress {
        ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    fn tcp_listen_address() -> ListenAddress {
        ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    #[test]
    fn interner_size_defaults_to_2mib() {
        let config = native_config(DogStatsDConfig::default());
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
    }

    #[test]
    fn statsd_forward_defaults_disabled() {
        let config = native_config(DogStatsDConfig::default());
        assert!(config.config.statsd_forward_host.is_none());
        assert_eq!(config.config.statsd_forward_port, 0);
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_zero_port_disabled() {
        let config = native_config(DogStatsDConfig {
            statsd_forward_host: Some(MetaString::from_static("127.0.0.1")),
            statsd_forward_port: 0,
            ..Default::default()
        });
        assert_eq!(config.config.statsd_forward_host.as_deref(), Some("127.0.0.1"));
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_host_and_port_enabled() {
        let config = native_config(DogStatsDConfig {
            statsd_forward_host: Some(MetaString::from_static("127.0.0.1")),
            statsd_forward_port: 9125,
            ..Default::default()
        });
        let (host, port) = config.statsd_forward_target().expect("forwarding should be enabled");
        assert_eq!(host.as_ref(), "127.0.0.1");
        assert_eq!(port, 9125);
    }

    #[test]
    fn statsd_forward_invalid_target_still_builds_forwarder_handle() {
        let config = native_config(DogStatsDConfig {
            statsd_forward_host: Some(MetaString::from_static("not a valid host")),
            statsd_forward_port: 9125,
            ..Default::default()
        });
        assert!(config.packet_forwarder_target().is_some());
    }

    #[tokio::test]
    async fn packet_forwarder_sends_payload_bytes() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("receiver should bind");
        let receiver_addr = receiver.local_addr().expect("receiver should have an address");
        let forwarder = ConnectedPacketForwarder::connect("127.0.0.1", receiver_addr.port())
            .await
            .expect("forwarder should connect");
        let payload = b"daemon:666|g|#sometag1:somevalue1,sometag2:somevalue2";

        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = ComponentContext::source(ComponentId::try_from("dogstatsd_test").expect("valid component ID"));
        let metrics = build_metrics(&listen_addr, &context);
        let (packets_tx, packets_rx) = mpsc::channel(1);
        let worker = tokio::spawn(forwarder.run(packets_rx, metrics.clone()));
        let packet_forwarder = packet_forwarder_from_sender(receiver_addr.port(), packets_tx, metrics);

        packet_forwarder.forward(Bytes::copy_from_slice(payload)).await;

        let mut actual = [0u8; 128];
        let (received_len, _) = timeout(Duration::from_secs(1), receiver.recv_from(&mut actual))
            .await
            .expect("receive should not time out")
            .expect("receiver should receive payload");

        assert_eq!(&actual[..received_len], payload);
        assert_eq!(
            recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "ok"),
                ]
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_bytes_forwarded_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                ]
            )),
            Some(payload.len() as u64)
        );
        worker.abort();
    }

    #[tokio::test]
    async fn packet_forwarder_sends_payload_bytes_to_ipv6_target() {
        let receiver = match UdpSocket::bind("[::1]:0").await {
            Ok(receiver) => receiver,
            Err(e) if is_ipv6_unavailable_error(&e) => return,
            Err(e) => panic!("receiver should bind: {e}"),
        };
        let receiver_addr = receiver.local_addr().expect("receiver should have an address");
        let forwarder = ConnectedPacketForwarder::connect("::1", receiver_addr.port())
            .await
            .expect("forwarder should connect");
        let payload = b"daemon:666|g|#ip:6";

        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = ComponentContext::source(ComponentId::try_from("dogstatsd_test").expect("valid component ID"));
        let metrics = build_metrics(&listen_addr, &context);
        let (packets_tx, packets_rx) = mpsc::channel(1);
        let worker = tokio::spawn(forwarder.run(packets_rx, metrics.clone()));
        let packet_forwarder = packet_forwarder_from_sender(receiver_addr.port(), packets_tx, metrics);

        packet_forwarder.forward(Bytes::copy_from_slice(payload)).await;

        let mut actual = [0u8; 128];
        let (received_len, _) = timeout(Duration::from_secs(1), receiver.recv_from(&mut actual))
            .await
            .expect("receive should not time out")
            .expect("receiver should receive payload");

        assert_eq!(&actual[..received_len], payload);
        worker.abort();
    }

    #[tokio::test]
    async fn packet_forwarder_waits_when_queue_is_full() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = ComponentContext::source(ComponentId::try_from("dogstatsd_test").expect("valid component ID"));
        let metrics = build_metrics(&listen_addr, &context);
        let (packets_tx, _packets_rx) = mpsc::channel(FORWARDER_QUEUE_CAPACITY);
        let packet_forwarder = packet_forwarder_from_sender(9125, packets_tx, metrics);

        for _ in 0..FORWARDER_QUEUE_CAPACITY {
            packet_forwarder.forward(Bytes::from_static(b"queued:1|c")).await;
        }

        assert!(
            timeout(
                Duration::from_millis(100),
                packet_forwarder.forward(Bytes::from_static(b"blocked:1|c")),
            )
            .await
            .is_err(),
            "forwarding should wait for queue capacity instead of dropping"
        );
    }

    #[tokio::test]
    async fn packet_forwarder_send_error_increments_error_telemetry() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = ComponentContext::source(ComponentId::try_from("dogstatsd_test").expect("valid component ID"));
        let metrics = build_metrics(&listen_addr, &context);
        let socket = UdpSocket::bind("127.0.0.1:0").await.expect("socket should bind");
        let forwarder = ConnectedPacketForwarder {
            socket,
            target: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9125)),
        };
        let (packets_tx, packets_rx) = mpsc::channel(1);
        let worker = tokio::spawn(forwarder.run(packets_rx, metrics.clone()));
        let packet_forwarder = packet_forwarder_from_sender(9125, packets_tx, metrics);

        packet_forwarder.forward(Bytes::from_static(b"daemon:666|g")).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            if recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "error"),
                ],
            )) == Some(1)
            {
                break;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "forwarding error telemetry should be recorded"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        worker.abort();
    }

    #[test]
    fn autoscale_udp_listeners_defaults_to_false() {
        let config = native_config(DogStatsDConfig::default());
        assert!(!config.config.autoscale_udp_listeners);
        assert!(config.udp_streams_to_yield().is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn autoscale_udp_listeners_from_config_linux() {
        let config = native_config(DogStatsDConfig {
            autoscale_udp_listeners: true,
            ..Default::default()
        });
        assert!(config.config.autoscale_udp_listeners);

        let streams = config
            .udp_streams_to_yield()
            .expect("autoscale yields at least 1 stream");
        let n = streams.get();
        assert!(
            (1..=4).contains(&n),
            "expected 1..=4 streams from vCPU formula, got {n}"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn autoscale_udp_listeners_from_config_non_linux() {
        let config = native_config(DogStatsDConfig {
            autoscale_udp_listeners: true,
            ..Default::default()
        });
        assert!(config.config.autoscale_udp_listeners);

        assert_eq!(None, config.udp_streams_to_yield());
    }

    #[test]
    fn eol_required_defaults_to_no_listeners() {
        let config = native_config(DogStatsDConfig::default());
        let eol_required = config.eol_required();

        assert!(!eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));
    }

    #[test]
    fn eol_required_matches_configured_listener_types() {
        let config = native_config(DogStatsDConfig {
            eol_required: vec!["udp".to_string(), "uds".to_string()],
            ..Default::default()
        });
        let eol_required = config.eol_required();

        assert!(eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));

        #[cfg(unix)]
        {
            assert!(eol_required.for_listener(&ListenAddress::Unixgram("/tmp/dsd.sock".into())));
            assert!(eol_required.for_listener(&ListenAddress::Unix("/tmp/dsd-stream.sock".into())));
        }
    }

    #[test]
    fn stream_log_too_big_only_warns_for_enabled_unix_invalid_frames() {
        let uds_stream = ListenAddress::Unix("/tmp/dsd-stream.sock".into());
        let tcp_stream = ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let error = saluki_io::deser::framing::FramingError::InvalidFrame {
            frame_len: 8193,
            reason: "frame length exceeds buffer capacity",
        };

        assert!(super::should_warn_stream_log_too_big(&uds_stream, &error, true));
        assert!(!super::should_warn_stream_log_too_big(&uds_stream, &error, false));
        assert!(!super::should_warn_stream_log_too_big(&tcp_stream, &error, true));
    }

    #[test]
    fn interner_size_from_entry_count() {
        // A Core Agent migration config with entry count 4096 should yield 2 MiB, not 4096 bytes.
        let config = native_config(DogStatsDConfig {
            context_string_interner_entry_count: 4096,
            ..Default::default()
        });
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
    }

    #[test]
    fn interner_size_from_explicit_bytes() {
        let config = native_config(DogStatsDConfig {
            context_string_interner_size_bytes: Some(ByteSize::b(4194304)),
            ..Default::default()
        });
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(4194304));
    }

    #[test]
    fn interner_size_explicit_bytes_takes_priority() {
        let config = native_config(DogStatsDConfig {
            context_string_interner_entry_count: 4096,
            context_string_interner_size_bytes: Some(ByteSize::b(8388608)),
            ..Default::default()
        });
        // The explicit byte size (8 MiB) takes priority over the entry count.
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(8388608));
    }

    #[test]
    fn interner_size_custom_entry_count() {
        let config = native_config(DogStatsDConfig {
            context_string_interner_entry_count: 8192,
            ..Default::default()
        });
        // 8192 entries * 512 bytes = 4 MiB
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(4));
    }

    /// Asserts that two lists of ListenAddress are equivalent.
    fn address_list_eq(expected: &mut [ListenAddress], actual: &mut [ListenAddress]) -> Result<(), String> {
        if expected.len() != actual.len() {
            return Err(format!(
                "length mismatch: expected {} addresses, got {}",
                expected.len(),
                actual.len()
            ));
        }

        expected.sort_by_key(|a| a.to_string());
        actual.sort_by_key(|a| a.to_string());

        for (e, a) in expected.iter().zip(actual.iter()) {
            let (es, as_) = (e.to_string(), a.to_string());
            if es != as_ {
                return Err(format!("address mismatch: expected {}, got {}", es, as_));
            }
        }

        Ok(())
    }

    /// This test verifies that we didn't accidentally break the `build_addresses_no_listeners` helper function which
    /// would render all further tests useless.
    #[test]
    fn build_addresses_assertion_function_works() {
        let config = native_config(DogStatsDConfig {
            port: 0,
            tcp_port: 123,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
            // Close, but not quite! This is intentionally *not* 127.0.0.1 to test that the assertion will fail
            Ipv4Addr::new(127, 0, 0, 2),
            123,
        )))];
        let mut actual = config.build_addresses(None);
        assert!(address_list_eq(&mut expected, &mut actual).is_err())
    }

    /// With all four listener gates off, `build_addresses` returns an empty Vec.
    #[test]
    fn build_addresses_no_listeners() {
        let config = native_config(DogStatsDConfig {
            port: 0,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// UDP port set, `non_local_traffic=false` -> UDP listener bound to `127.0.0.1`.
    #[test]
    fn build_addresses_udp_local_only() {
        let config = native_config(DogStatsDConfig {
            port: 8125,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            8125,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// UDP port set, `non_local_traffic=true` -> UDP listener bound to `0.0.0.0`.
    #[test]
    fn build_addresses_udp_non_local_only() {
        let config = native_config(DogStatsDConfig {
            port: 8125,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: true,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            8125,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// TCP port set, `non_local_traffic=false` -> TCP listener bound to `127.0.0.1`.
    #[test]
    fn build_addresses_tcp_local_only() {
        let config = native_config(DogStatsDConfig {
            port: 0,
            tcp_port: 9000,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// TCP port set, `non_local_traffic=true` -> TCP listener bound to `0.0.0.0`.
    #[test]
    fn build_addresses_tcp_non_local_only() {
        let config = native_config(DogStatsDConfig {
            port: 0,
            tcp_port: 9000,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: true,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            9000,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// `socket_path` set -> a `Unixgram` address is produced with that path.
    #[test]
    fn build_addresses_unixgram_only() {
        let config = native_config(DogStatsDConfig {
            port: 0,
            tcp_port: 0,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Unixgram("/tmp/dsd.sock".into())];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// `socket_stream_path` set -> a `Unix` (stream) address is produced with that path.
    #[test]
    fn build_addresses_unix_stream_only() {
        let config = native_config(DogStatsDConfig {
            port: 0,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![ListenAddress::Unix("/tmp/dsd-stream.sock".into())];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// All four listener types enabled at once, with `non_local_traffic=true`.
    #[test]
    fn build_addresses_all_four_non_local() {
        let config = native_config(DogStatsDConfig {
            port: 8125,
            tcp_port: 9000,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: true,
            ..Default::default()
        });
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 9000))),
            ListenAddress::Unixgram("/tmp/dsd.sock".into()),
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
        ];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// All four listener types enabled at once, with `non_local_traffic=false`.
    #[test]
    fn build_addresses_all_four_local() {
        let config = native_config(DogStatsDConfig {
            port: 8125,
            tcp_port: 9000,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: false,
            ..Default::default()
        });
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9000))),
            ListenAddress::Unixgram("/tmp/dsd.sock".into()),
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
        ];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// Passing `Some(ip)` to `build_addresses` with `non_local_traffic=false` -> both UDP and TCP
    /// bind to that IP. Includes a UDS datagram socket to confirm `bind_host` doesn't affect it.
    #[test]
    fn build_addresses_bind_host_applies_to_udp_and_tcp() {
        let config = native_config(DogStatsDConfig {
            port: 8125,
            tcp_port: 9000,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        });
        let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 9000))),
            ListenAddress::Unixgram("/tmp/dsd.sock".into()),
        ];
        let mut actual = config.build_addresses(bind_host);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// Passing `Some(ip)` to `build_addresses` with `non_local_traffic=true` -> both UDP and TCP
    /// bind to `0.0.0.0`; the `bind_host` parameter is ignored (precedence matches the Agent).
    /// Includes a UDS stream socket to confirm `bind_host` doesn't affect it.
    #[test]
    fn build_addresses_non_local_clobbers_bind_host() {
        let config = native_config(DogStatsDConfig {
            port: 8125,
            tcp_port: 9000,
            socket_path: None,
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: true,
            ..Default::default()
        });
        let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 9000))),
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
        ];
        let mut actual = config.build_addresses(bind_host);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    #[test]
    fn non_finite_metric_values_are_silently_dropped() {
        // The Datadog Agent sends NaN gauges (for example, encode_ms.avg computed as 0.0/0.0 in Go).
        // FloatIter skips non-finite values with a debug log, so decode_packet returns Ok with
        // num_points == 0. handle_frame then returns Ok(None) for zero-point packets, which is
        // the existing silent-drop path (no warning emitted).
        let codec: DogStatsDCodec = DogStatsDCodecConfiguration::default().into();
        for input in &[b"my.gauge:NaN|g" as &[u8], b"my.gauge:inf|g", b"my.gauge:-inf|g"] {
            match codec.decode_packet(input).expect("should decode without error") {
                ParsedPacket::Metric(packet) => assert_eq!(
                    packet.num_points, 0,
                    "non-finite value should be dropped, leaving 0 valid points"
                ),
                _ => panic!("expected Metric packet"),
            }
        }
    }

    #[test]
    fn capture_entity_resolver_is_configured_separately_from_workload_provider() {
        let config =
            DogStatsDConfiguration::default().with_capture_entity_resolver(CaptureTestEntityResolver::default());

        assert!(config.capture_entity_resolver.is_some());
        assert!(config.workload_provider.is_none());
    }

    #[test]
    fn resolve_capture_container_id_uses_live_pid_mapping() {
        let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );

        assert_eq!(
            resolve_capture_container_id(Some(&capture_entity_resolver), Some(42)),
            Some("container_id://pid-container".to_string())
        );
    }

    #[test]
    fn build_capture_record_ignores_payload_local_data() {
        let record = super::build_capture_record(None, None, b"test.metric:1|c|c:ci-local-container\n");

        assert_eq!(record.container_id, None);
        assert!(record.ancillary.is_empty());
    }

    #[test]
    fn stream_capture_state_preserves_last_pid_without_new_creds() {
        let mut stream_capture = super::StreamCaptureState::new();

        stream_capture.update_peer_metadata(&ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(
            ProcessCredentials {
                pid: 42,
                uid: 0,
                gid: 0,
            },
        )));
        stream_capture.update_peer_metadata(&ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable));

        assert_eq!(stream_capture.last_pid, Some(42));
    }

    #[test]
    fn apply_credentials_uses_live_pid_for_normal_packet() {
        let mut origin = RawOrigin::default();
        let creds = ProcessCredentials {
            pid: 12345,
            uid: 1000,
            gid: 1000,
        };
        super::apply_credentials_to_origin(&mut origin, &creds);

        assert_eq!(origin.process_id(), Some(12345));
    }

    #[test]
    fn apply_credentials_unpacks_captured_pid_when_replay_gid_present() {
        let mut origin = RawOrigin::default();
        let captured_pid: u32 = 99887766;
        let creds = ProcessCredentials {
            pid: 12345,        // our PID (irrelevant for replay)
            uid: captured_pid, // captured PID packed by the sender
            gid: super::REPLAY_CREDENTIALS_GID,
        };
        super::apply_credentials_to_origin(&mut origin, &creds);

        assert_eq!(
            origin.process_id(),
            Some(super::origin::mark_replay_process_id(captured_pid))
        );
    }
}
