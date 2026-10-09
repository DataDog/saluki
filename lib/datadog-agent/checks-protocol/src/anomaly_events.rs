//! Anomaly detection events published by the isolated AAD process over FIT.
//!
//! This is a separate application protocol from the Checks records in [`crate::model`],
//! with its own descriptor (`AAD-EVNT`), one record kind, and three fields. AAD is the
//! producer; both ADP and the Agent subscribe to it over the broadcast shared-memory
//! transport, so each subscriber receives every event.
//!
//! The byte layout mirrors the Checks conventions: little-endian scalars with `u32`
//! length-prefixed strings, in fixed field order, with every field always present.

use std::io;

use saluki_fit::{
    BroadcastOutcome, BroadcastPublisher, BroadcastPublisherConfig, CancellationToken, ProtocolDescriptor, Record,
    Rejection, SubscriberConfig, Subscription,
};

use crate::wire::{payload_size, string_size, Reader, Writer};

/// FIT record type ID of one anomaly event.
pub const ANOMALY_EVENT_TYPE_ID: u32 = 1;

/// Protocol descriptor of the anomaly event channel.
pub const ANOMALY_EVENTS_DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
    id: *b"AAD-EVNT",
    version: 1,
    message_types: &[ANOMALY_EVENT_TYPE_ID],
};

/// Largest encoded anomaly event either side accepts.
///
/// The transport allows payloads up to a gigabyte, but an event carries a title and a
/// description; anything past this bound is a producer bug, so both sides reject it
/// early instead of moving it through shared memory.
pub const MAX_EVENT_PAYLOAD: usize = 64 * 1024;

/// One anomaly notification, from AAD to every subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnomalyEvent {
    /// Short human-readable summary, for example `AAD anomaly: system.load.1`.
    pub title: String,
    /// Details of the event, for example the severity transition and anomaly count.
    pub description: String,
    /// Unix timestamp in seconds of the anomaly, not of the publication.
    pub timestamp: u64,
}

impl AnomalyEvent {
    /// Returns the exact encoded payload size, rejecting oversized events.
    pub fn encoded_len(&self) -> io::Result<usize> {
        let size = payload_size(&[string_size(&self.title)?, string_size(&self.description)?, 8])?;
        if size > MAX_EVENT_PAYLOAD {
            return Err(too_large());
        }
        Ok(size)
    }

    /// Encodes this event with the versioned `AAD-EVNT` byte layout.
    pub fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let size = self.encoded_len()?;
        let mut out = Writer::new(size)?;
        out.string(&self.title);
        out.string(&self.description);
        out.u64(self.timestamp);
        Ok(out.finish(size))
    }

    /// Decodes and validates one complete anomaly event payload.
    pub fn decode_payload(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_EVENT_PAYLOAD {
            return Err(too_large());
        }
        let mut input = Reader::new(bytes)?;
        let value = Self {
            title: input.string()?,
            description: input.string()?,
            timestamp: input.u64()?,
        };
        input.finish()?;
        Ok(value)
    }
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "anomaly event exceeds the 64 KiB payload cap",
    )
}

/// Outcome of one event batch publication.
///
/// `accepted` always reports how many events the broadcast ring published, including
/// when the call stopped for cancellation or a fatal failure: a partial result cannot be
/// retried as a whole, because those events are already visible to every subscriber.
#[derive(Debug)]
pub struct EventBatchOutcome {
    /// Events published by this call.
    pub accepted: usize,
    /// First event that could not be encoded. Later events were not attempted.
    pub encoding_error: Option<io::Error>,
    /// First transport rejection, if any.
    pub rejection: Option<Rejection>,
    /// Wake failure after publication. The accepted events stay published.
    pub notification_error: Option<io::Error>,
    /// Local cancellation stopped the batch.
    pub cancelled: bool,
    /// Fatal transport failure. The accepted events are published.
    pub failure: Option<io::Error>,
}

/// One typed publisher for the anomaly event protocol.
pub struct EventPublisher(BroadcastPublisher);

impl EventPublisher {
    /// Creates the broadcast mapping and the listener, and starts the control worker.
    ///
    /// It returns without waiting for a subscriber: AAD binds this endpoint and keeps
    /// it open for the whole session so subscribers can join late.
    pub fn open(config: BroadcastPublisherConfig) -> io::Result<Self> {
        BroadcastPublisher::open(config, ANOMALY_EVENTS_DESCRIPTOR).map(Self)
    }

    /// Returns this session's identifier.
    pub fn session_id(&self) -> u64 {
        self.0.session_id()
    }

    /// Returns a diagnostic count of active subscribers.
    ///
    /// It is a sampling of a moving set: do not treat it as proof that a publication
    /// will be delivered to anyone.
    pub fn subscriber_count(&self) -> usize {
        self.0.subscriber_count()
    }

    /// Encodes the events in order and publishes the fitting prefix.
    ///
    /// The call blocks for ring capacity and, while no subscriber is active, until one
    /// joins; `cancellation` is the only way out of that wait.
    pub fn send_batch(&mut self, events: &[AnomalyEvent], cancellation: &CancellationToken) -> EventBatchOutcome {
        let mut payloads = Vec::with_capacity(events.len());
        let mut encoding_error = None;
        for event in events {
            match event.encode_payload() {
                Ok(payload) => payloads.push(payload),
                Err(error) => {
                    encoding_error = Some(error);
                    break;
                }
            }
        }
        let records: Vec<Record<'_>> = payloads
            .iter()
            .map(|payload| Record {
                kind: ANOMALY_EVENT_TYPE_ID,
                payload,
            })
            .collect();
        let outcome = self.0.send_batch_with_cancel(&records, cancellation);
        EventBatchOutcome {
            accepted: outcome.accepted,
            encoding_error,
            rejection: outcome.rejection,
            notification_error: outcome.notification_error,
            cancelled: outcome.cancelled,
            failure: outcome.failure,
        }
    }
}

impl From<BroadcastOutcome> for EventBatchOutcome {
    fn from(outcome: BroadcastOutcome) -> Self {
        Self {
            accepted: outcome.accepted,
            encoding_error: None,
            rejection: outcome.rejection,
            notification_error: outcome.notification_error,
            cancelled: outcome.cancelled,
            failure: outcome.failure,
        }
    }
}

/// One typed subscriber for the anomaly event protocol.
pub struct EventSubscriber(Subscription);

impl EventSubscriber {
    /// Maps the publisher's ring and activates one slot.
    ///
    /// A late subscriber starts at its activation boundary: it receives only events
    /// published after this call returns, never history.
    pub fn subscribe(config: SubscriberConfig) -> io::Result<Self> {
        Subscription::subscribe(config, ANOMALY_EVENTS_DESCRIPTOR).map(Self)
    }

    /// Subscribes while allowing a local supervisor to cancel the handshake.
    pub fn subscribe_with_cancel(config: SubscriberConfig, cancellation: &CancellationToken) -> io::Result<Self> {
        Subscription::subscribe_with_cancel(config, ANOMALY_EVENTS_DESCRIPTOR, cancellation).map(Self)
    }

    /// Returns this subscription's session identifier.
    pub fn session_id(&self) -> u64 {
        self.0.session_id()
    }

    /// Returns this subscription's slot identifier.
    pub fn slot_id(&self) -> u32 {
        self.0.slot_id()
    }

    /// Releases this subscription's slot, so it stops pinning publication.
    pub fn unsubscribe(&mut self) -> io::Result<()> {
        self.0.unsubscribe()
    }

    /// Waits for one event.
    pub fn receive(&mut self) -> io::Result<AnomalyEvent> {
        let (kind, payload) = self.0.receive()?;
        decode_event(kind, &payload)
    }

    /// Waits for one event, or returns `None` after local cancellation.
    pub fn receive_with_cancel(&mut self, cancellation: &CancellationToken) -> io::Result<Option<AnomalyEvent>> {
        self.0
            .receive_with_cancel(cancellation)?
            .map(|(kind, payload)| decode_event(kind, &payload))
            .transpose()
    }
}

/// Decodes one record, rejecting other record types.
///
/// The transport already rejects a record type outside the session's declared message
/// types, so this guard is defence in depth: it keeps a subscriber from mis-decoding a
/// record if a peer ever publishes one.
pub(crate) fn decode_event(kind: u32, payload: &[u8]) -> io::Result<AnomalyEvent> {
    if kind != ANOMALY_EVENT_TYPE_ID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown AAD-EVNT record type {kind}"),
        ));
    }
    AnomalyEvent::decode_payload(payload)
}
