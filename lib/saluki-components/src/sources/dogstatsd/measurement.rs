//! Entry points into the DogStatsD source for measurements, with the `measurement` feature.
//!
//! [`DatagramDecoder`] runs a datagram through the source's own decode path, so that a benchmark measures what the
//! source's datagram decoders do per metric, on the context resolvers they share, without sockets, event buffers, or
//! dispatch.

use std::{net::SocketAddr, sync::Arc};

use saluki_core::{
    components::ComponentContext, data_model::event::Event, pooling::helpers::get_pooled_object_via_builder,
    topology::ComponentId,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, ClearableIoBuffer as _, FixedSizeVec},
    deser::{
        codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration},
        framing::Framer as _,
    },
    net::ListenAddress,
};
use stringtheory::MetaString;

use super::{
    build_metrics, filters::EnablePayloadsFilter, get_adjusted_buffer_size, get_framer, handle_frame,
    resolver::ContextResolvers, DogStatsDConfiguration, Metrics,
};

/// Decodes DogStatsD datagrams as the source's datagram decoders do.
///
/// A datagram is framed by lines, as a UDP datagram is, and each frame is parsed, filtered by payload type, and
/// resolved to a context with its well-known and origin tags, into an event: everything a decoder does per metric
/// except pushing the event into an event buffer and dispatching it. The source's telemetry is recorded as the source
/// records it.
///
/// Clones share the context resolvers, and so their caches and interner, as the source's decoders share them: give
/// each thread of a benchmark a clone to measure decoding on shared resolvers.
pub struct DatagramDecoder {
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    metrics: Arc<Metrics>,
    listen_addr: ListenAddress,
    eol_required: bool,
    enabled_filter: EnablePayloadsFilter,
    additional_tags: Arc<[String]>,
    default_hostname: MetaString,
    origin_detection_enabled: bool,
    buffer_capacity: usize,

    // The receive buffer, reused across datagrams as a pooled buffer is; each clone has its own.
    buffer: Option<BytesBuffer>,
}

impl Clone for DatagramDecoder {
    fn clone(&self) -> Self {
        Self {
            codec: self.codec.clone(),
            context_resolvers: self.context_resolvers.clone(),
            metrics: Arc::clone(&self.metrics),
            listen_addr: self.listen_addr.clone(),
            eol_required: self.eol_required,
            enabled_filter: self.enabled_filter,
            additional_tags: Arc::clone(&self.additional_tags),
            default_hostname: self.default_hostname.clone(),
            origin_detection_enabled: self.origin_detection_enabled,
            buffer_capacity: self.buffer_capacity,
            buffer: None,
        }
    }
}

impl DatagramDecoder {
    /// Creates a decoder configured as the source is by `config`, decoding as its UDP listener does, with neither
    /// origin detection nor a workload provider.
    ///
    /// Call it within a Tokio runtime: the context resolvers' caches start their expiry tasks on it.
    ///
    /// # Errors
    ///
    /// If the context resolvers cannot be created from `config`, an error is returned.
    pub fn new(config: &DogStatsDConfiguration) -> Result<Self, GenericError> {
        let topology = saluki_core::support::SubsystemIdentifier::from_segments(["topology", "measurement"]);
        let component_id = ComponentId::try_from("dsd_in").expect("`dsd_in` is a valid component ID");
        let component_context = ComponentContext::source(&topology, component_id);
        let context_resolvers = ContextResolvers::new(config, &component_context, None)
            .error_context("Failed to create context resolvers.")?;
        let codec = DogStatsDCodec::from_configuration(
            DogStatsDCodecConfiguration::default()
                .with_timestamps(config.no_aggregation_pipeline_support)
                .with_permissive_mode(config.permissive_decoding)
                .with_minimum_sample_rate(config.minimum_sample_rate)
                .with_client_origin_detection(config.origin_enrichment.origin_detection_client),
        );
        let listen_addr = ListenAddress::Udp(SocketAddr::from(([127, 0, 0, 1], config.port)));
        let eol_required = config.eol_required().for_listener(&listen_addr);
        let metrics = Arc::new(build_metrics(
            &listen_addr,
            &component_context,
            config.origin_telemetry_enabled,
        ));
        let enabled_filter = EnablePayloadsFilter::default()
            .with_allow_series(config.enable_payloads.series)
            .with_allow_sketches(config.enable_payloads.sketches)
            .with_allow_events(config.enable_payloads.events)
            .with_allow_service_checks(config.enable_payloads.service_checks);
        Ok(Self {
            codec,
            context_resolvers,
            metrics,
            listen_addr,
            eol_required,
            enabled_filter,
            additional_tags: config.additional_tags().into(),
            default_hostname: config.default_hostname.clone(),
            origin_detection_enabled: config.origin_enrichment.enabled,
            buffer_capacity: get_adjusted_buffer_size(config.buffer_size),
            buffer: None,
        })
    }

    /// Creates a decoder with the source's default configuration: the defaults the configuration system resolves.
    ///
    /// Call it within a Tokio runtime, as [`new`][Self::new] says.
    ///
    /// # Errors
    ///
    /// If the context resolvers cannot be created, an error is returned.
    pub fn with_defaults() -> Result<Self, GenericError> {
        Self::new(&DogStatsDConfiguration::for_test())
    }

    /// Decodes `datagram`, appending an event for each frame that decodes to one, and returns how many were appended.
    ///
    /// Frames that fail to parse or resolve are counted in the source's telemetry and skipped, as the source skips
    /// them. A datagram longer than the source's receive buffer is truncated to it, as a socket read would be.
    pub fn decode(&mut self, datagram: &[u8], events: &mut Vec<Event>) -> usize {
        let capacity = self.buffer_capacity;
        let mut buffer = self
            .buffer
            .take()
            .unwrap_or_else(|| get_pooled_object_via_builder(|| FixedSizeVec::with_capacity(capacity)));
        let received = datagram.len().min(capacity);
        bytes::BufMut::put_slice(&mut buffer, &datagram[..received]);

        let before = events.len();
        let mut framer = get_framer(&self.listen_addr, self.eol_required);
        while let Ok(Some(frame)) = framer.next_frame(&mut buffer, true) {
            if let Ok(Some(event)) = handle_frame(
                &frame[..],
                &self.codec,
                &mut self.context_resolvers,
                &self.metrics,
                self.origin_detection_enabled,
                None,
                self.enabled_filter,
                &self.additional_tags,
                &self.default_hostname,
            ) {
                events.push(event);
            }
        }
        buffer.clear();
        self.buffer = Some(buffer);
        events.len() - before
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;

    #[tokio::test]
    async fn a_datagram_decodes_to_one_event_per_metric_line_on_shared_resolvers() {
        let mut first = DatagramDecoder::with_defaults().unwrap();
        let mut second = first.clone();
        let mut events = Vec::new();
        let decoded = first.decode(
            b"a.count:1|c|#env:prod\nb.gauge:2|g\nnot a metric\nc.dist:3|d",
            &mut events,
        );
        assert_eq!(decoded, 3);
        assert_eq!(second.decode(b"a.count:5|c|#env:prod", &mut events), 1);

        let metrics = events
            .into_iter()
            .map(|event| event.try_into_metric().expect("every event is a metric"))
            .collect::<Vec<_>>();
        let names = metrics
            .iter()
            .map(|metric| metric.context().name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a.count", "b.gauge", "c.dist", "a.count"]);
        assert!(matches!(metrics[0].values(), MetricValues::Counter(_)));
        assert!(matches!(metrics[2].values(), MetricValues::Distribution(_)));
        assert_eq!(metrics[0].context(), metrics[3].context());
    }
}
