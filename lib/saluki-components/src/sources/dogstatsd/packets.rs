//! Converting decoded packets into events, and dispatching them.

use std::time::{SystemTime, UNIX_EPOCH};

use saluki_core::{
    components::sources::*,
    data_model::{
        event::{
            eventd::EventD,
            metric::{Metric, MetricMetadata, MetricOrigin},
            service_check::ServiceCheck,
            Event, EventType,
        },
        tags::{RawTags, SharedTagSet},
    },
    topology::EventsBuffer,
};
use saluki_io::deser::codec::dogstatsd::*;
use stringtheory::MetaString;
use tracing::{debug, error};

use super::{origin_from_packet, ContextResolvers, ProcessOrigin, WellKnownTags};

pub(super) fn handle_metric_packet(
    packet: MetricPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String], default_hostname: &MetaString,
) -> Option<Metric> {
    let (well_known_tags, mut tags) = WellKnownTags::split_raw_tags(&packet.tags);
    tags.extend(additional_tags.iter().map(String::as_str));

    let origin = origin_from_packet(&packet.origin, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    // Choose the right context resolver based on whether or not this metric is pre-aggregated.
    let context_resolver = if packet.timestamp.is_some() {
        context_resolvers.no_agg()
    } else {
        context_resolvers.primary()
    };

    let hostname = well_known_tags.hostname.unwrap_or(default_hostname);

    // Try to resolve the context for this metric. This fails if the interner is out of capacity.
    let context =
        context_resolver.resolve_with_host_and_origin_tags(packet.metric_name, hostname, &tags, origin_tags)?;

    let metric_origin = well_known_tags
        .jmx_check_name
        .map(MetricOrigin::jmx_check)
        .unwrap_or_else(MetricOrigin::dogstatsd);
    let metadata = MetricMetadata::default()
        .with_origin(metric_origin)
        .with_unit(packet.unit.map_or_else(MetaString::empty, MetaString::from_static));

    Some(Metric::from_parts(context, packet.values, metadata))
}

/// Resolves the tags and origin tags of an event or service check, along with the value of any `host:` tag.
///
/// Returns `None` if the tags can't be resolved.
fn resolve_tags_and_origin_tags<'a>(
    context_resolvers: &mut ContextResolvers, raw_tags: &'a RawTags<'_>, client_origin: &ClientOrigin<'_>,
    process_origin: Option<&ProcessOrigin>, additional_tags: &'a [String],
) -> Option<(SharedTagSet, SharedTagSet, Option<&'a str>)> {
    let (well_known_tags, mut tags) = WellKnownTags::split_raw_tags(raw_tags);
    tags.extend(additional_tags.iter().map(String::as_str));

    let origin = origin_from_packet(client_origin, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    let tags = context_resolvers.tags().create_tag_set(&tags)?;
    Some((tags, origin_tags, well_known_tags.hostname))
}

pub(super) fn handle_event_packet(
    packet: EventPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String],
) -> Option<EventD> {
    let (tags, origin_tags, tagged_hostname) = resolve_tags_and_origin_tags(
        context_resolvers,
        &packet.tags,
        &packet.origin,
        process_origin,
        additional_tags,
    )?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior in pkg/aggregator/aggregator.go (addEvent), which sets e.Ts = time.Now().Unix()
    // for any event with Ts == 0.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let eventd = EventD::new(packet.title, packet.text)
        .with_timestamp(timestamp)
        // An `h:` field takes precedence; otherwise, fall back to a `host:` tag, matching the reference DogStatsD
        // server. An empty `host:` tag gives an explicitly empty hostname, which host enrichment leaves alone, as
        // the reference DogStatsD server does.
        .with_hostname(packet.hostname.or(tagged_hostname).map(|s| s.into()))
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
        .with_tags(tags)
        .with_origin_tags(origin_tags);

    Some(eventd)
}

pub(super) fn handle_service_check_packet(
    packet: ServiceCheckPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String],
) -> Option<ServiceCheck> {
    let (tags, origin_tags, tagged_hostname) = resolve_tags_and_origin_tags(
        context_resolvers,
        &packet.tags,
        &packet.origin,
        process_origin,
        additional_tags,
    )?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior, which sets the timestamp to time.Now().Unix() for any service check with a zero
    // timestamp.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let service_check = ServiceCheck::new(packet.name, packet.status)
        .with_timestamp(timestamp)
        // An `h:` field takes precedence; otherwise, fall back to a `host:` tag, matching the reference DogStatsD
        // server. An empty `host:` tag gives an explicitly empty hostname, which host enrichment leaves alone, as
        // the reference DogStatsD server does.
        .with_hostname(packet.hostname.or(tagged_hostname).map(|s| s.into()))
        .with_tags(tags)
        .with_origin_tags(origin_tags)
        .with_message(packet.message.map(|s| s.into()));

    Some(service_check)
}

pub(super) async fn dispatch_events(mut event_buffer: EventsBuffer, source_context: &SourceContext) {
    debug!(events_len = event_buffer.len(), "Forwarding events.");

    // TODO: This is maybe a little dicey because if we fail to dispatch the events, we may not have iterated over all of
    // them, so there might still be eventd events when get to the service checks point, and eventd events and/or service
    // check events when we get to the metrics point, and so on.
    //
    // There's probably something to be said for erroring out fully if this happens, since we should only fail to
    // dispatch if the downstream component fails entirely... and unless we have a way to restart the component, then
    // we're going to continue to fail to dispatch any more events until the process is restarted anyways.

    // Dispatch any eventd events, if present.
    if event_buffer.has_event_type(EventType::EventD) {
        let eventd_events = event_buffer.take_events::<EventD>().map(Event::EventD);
        let events_output = source_context.dispatcher().buffered_named("events");

        // The `events` output is always wired in the DSD topology, so a missing output is an invariant violation that
        // crashes this component.
        if events_output.is_err() {
            saluki_antithesis::unreachable!("dsd 'events' output missing at dispatch");
        }

        if let Err(e) = events_output
            .expect("events output should always exist")
            .send_all(eventd_events)
            .await
        {
            error!(error = %e, "Failed to dispatch eventd events.");

            saluki_antithesis::unreachable!("dsd dispatch failed mid-buffer", { "stream": "events" });
        }
    }

    // Dispatch any service check events, if present.
    if event_buffer.has_event_type(EventType::ServiceCheck) {
        let service_check_events = event_buffer.take_events::<ServiceCheck>().map(Event::ServiceCheck);
        let service_checks_output = source_context.dispatcher().buffered_named("service_checks");

        if service_checks_output.is_err() {
            saluki_antithesis::unreachable!("dsd 'service_checks' output missing at dispatch");
        }

        if let Err(e) = service_checks_output
            .expect("service checks output should always exist")
            .send_all(service_check_events)
            .await
        {
            error!(error = %e, "Failed to dispatch service check events.");

            saluki_antithesis::unreachable!("dsd dispatch failed mid-buffer", { "stream": "service_checks" });
        }
    }

    // Finally, if there are events left, they'll be metrics, so dispatch them.
    if !event_buffer.is_empty() {
        if let Err(e) = source_context
            .dispatcher()
            .dispatch_named("metrics", event_buffer)
            .await
        {
            error!(error = %e, "Failed to dispatch metric events.");

            saluki_antithesis::unreachable!("dsd dispatch failed mid-buffer", { "stream": "metrics" });
        }
    }
}
