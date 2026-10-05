//! Checks telemetry over FIT shared memory, with a custom versioned byte layout.
//!
//! Payload semantics come from the existing Checks proto definitions. This crate
//! does not use protobuf serialization or depend on either application's event model.
#![cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_endian = "little",
    target_has_atomic = "32",
    any(target_os = "linux", target_os = "macos")
))]

mod model;
mod wire;

#[cfg(test)]
mod tests;

pub use model::{Event, Log, Metric, Payload, ServiceCheck};

use std::io;

use saluki_fit::{
    CancellationToken, ConsumerConfig, ProducerConfig, ProtocolDescriptor, Record, Rejection, SendResult,
};

const DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
    id: *b"DDCHECKS",
    version: 1,
    message_types: &[Metric::TYPE_ID, Log::TYPE_ID, ServiceCheck::TYPE_ID, Event::TYPE_ID],
};

/// One Checks datum carried in a FIT ring record.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Scalar metric.
    Metric(Metric),
    /// Check-produced log.
    Log(Log),
    /// Service-check status.
    ServiceCheck(ServiceCheck),
    /// Submitted event.
    Event(Event),
}

impl Message {
    fn encoded_len(&self) -> io::Result<usize> {
        match self {
            Self::Metric(value) => value.encoded_len(),
            Self::Log(value) => value.encoded_len(),
            Self::ServiceCheck(value) => value.encoded_len(),
            Self::Event(value) => value.encoded_len(),
        }
    }

    /// Encodes a complete logical Checks datum as one custom FIT payload.
    pub fn encode_payload(&self) -> io::Result<(u32, Vec<u8>)> {
        match self {
            Self::Metric(value) => Ok((Metric::TYPE_ID, value.encode_payload()?)),
            Self::Log(value) => Ok((Log::TYPE_ID, value.encode_payload()?)),
            Self::ServiceCheck(value) => Ok((ServiceCheck::TYPE_ID, value.encode_payload()?)),
            Self::Event(value) => Ok((Event::TYPE_ID, value.encode_payload()?)),
        }
    }

    /// Decodes a record, rejecting unknown types and malformed payloads.
    pub fn decode_payload(kind: u32, bytes: &[u8]) -> io::Result<Self> {
        match kind {
            Metric::TYPE_ID => Metric::decode_payload(bytes).map(Self::Metric),
            Log::TYPE_ID => Log::decode_payload(bytes).map(Self::Log),
            ServiceCheck::TYPE_ID => ServiceCheck::decode_payload(bytes).map(Self::ServiceCheck),
            Event::TYPE_ID => Event::decode_payload(bytes).map(Self::Event),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown Checks FIT record type",
            )),
        }
    }
}

/// Why a batch stopped accepting records.
#[derive(Debug)]
pub enum BatchRejection {
    /// The shared ring rejected the next record.
    Queue(Rejection),
    /// Encoding the next record failed.
    Encoding(io::Error),
}

/// Publication result for an explicitly submitted batch.
#[derive(Debug)]
pub struct BatchOutcome {
    /// Number of complete application records published to the ring.
    pub accepted: usize,
    /// First rejection, if any. Later records were not attempted.
    pub rejection: Option<BatchRejection>,
    /// Native wake failure after publication. Accepted records remain published.
    pub notification_error: Option<io::Error>,
}

impl BatchOutcome {
    fn from_core(result: SendResult) -> Self {
        Self {
            accepted: result.accepted,
            rejection: result.rejection.map(BatchRejection::Queue),
            notification_error: result.notification_error,
        }
    }
}

/// Typed, single-threaded Checks producer.
pub struct Producer(saluki_fit::Producer);

impl Producer {
    /// Establishes a Checks session using the configured setup endpoint.
    pub fn connect(config: ProducerConfig) -> io::Result<Self> {
        saluki_fit::Producer::connect(config, DESCRIPTOR).map(Self)
    }

    /// Establishes a Checks session with local setup cancellation.
    pub fn connect_with_cancel(config: ProducerConfig, cancellation: &CancellationToken) -> io::Result<Self> {
        saluki_fit::Producer::connect_with_cancel(config, DESCRIPTOR, cancellation).map(Self)
    }

    /// Returns this session's identifier.
    pub fn session_id(&self) -> u64 {
        self.0.session_id()
    }

    /// Encodes and publishes one datum.
    pub fn send<T: Payload>(&mut self, value: &T) -> io::Result<BatchOutcome> {
        let size = match value.encoded_len() {
            Ok(size) => size,
            Err(error) => {
                return Ok(BatchOutcome {
                    accepted: 0,
                    rejection: Some(BatchRejection::Encoding(error)),
                    notification_error: None,
                });
            }
        };
        if record_size(size)? > self.0.ring_capacity() - 8 {
            return Ok(BatchOutcome {
                accepted: 0,
                rejection: Some(BatchRejection::Queue(Rejection::Oversized)),
                notification_error: None,
            });
        }
        let bytes = match value.encode_payload() {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(BatchOutcome {
                    accepted: 0,
                    rejection: Some(BatchRejection::Encoding(error)),
                    notification_error: None,
                });
            }
        };
        self.0
            .send_batch(&[Record {
                kind: T::TYPE_ID,
                payload: &bytes,
            }])
            .map(BatchOutcome::from_core)
    }

    /// Encodes in order and publishes the fitting prefix once.
    pub fn send_batch(&mut self, messages: &[Message]) -> io::Result<BatchOutcome> {
        let EncodedPrefix {
            records: encoded,
            failure,
        } = encode_prefix(
            messages,
            self.0.ring_capacity() - 8,
            Message::encoded_len,
            Message::encode_payload,
        )?;
        let records: Vec<_> = encoded
            .iter()
            .map(|(kind, bytes)| Record {
                kind: *kind,
                payload: bytes,
            })
            .collect();
        let mut outcome = BatchOutcome::from_core(self.0.send_batch(&records)?);
        if outcome.rejection.is_none() {
            outcome.rejection = failure;
        }
        Ok(outcome)
    }
}

struct EncodedPrefix {
    records: Vec<(u32, Vec<u8>)>,
    failure: Option<BatchRejection>,
}

fn record_size(payload_size: usize) -> io::Result<usize> {
    payload_size
        .checked_add(15)
        .map(|size| size & !7)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Checks record size overflow"))
}

fn encode_prefix<T>(
    items: &[T], budget: usize, mut encoded_len: impl FnMut(&T) -> io::Result<usize>,
    mut encode: impl FnMut(&T) -> io::Result<(u32, Vec<u8>)>,
) -> io::Result<EncodedPrefix> {
    let mut encoded = Vec::new();
    let mut used = 0usize;
    for item in items {
        let size = match encoded_len(item).and_then(record_size) {
            Ok(size) => size,
            Err(error) => {
                return Ok(EncodedPrefix {
                    records: encoded,
                    failure: Some(BatchRejection::Encoding(error)),
                })
            }
        };
        if size > budget {
            return Ok(EncodedPrefix {
                records: encoded,
                failure: Some(BatchRejection::Queue(Rejection::Oversized)),
            });
        }
        if used > budget - size {
            return Ok(EncodedPrefix {
                records: encoded,
                failure: Some(BatchRejection::Queue(Rejection::Full)),
            });
        }
        match encode(item) {
            Ok(record) => {
                encoded.try_reserve(1).map_err(io::Error::other)?;
                encoded.push(record);
                used += size;
            }
            Err(error) => {
                return Ok(EncodedPrefix {
                    records: encoded,
                    failure: Some(BatchRejection::Encoding(error)),
                })
            }
        }
    }
    Ok(EncodedPrefix {
        records: encoded,
        failure: None,
    })
}

/// Typed, single-threaded Checks consumer.
pub struct Consumer(saluki_fit::Consumer);

impl Consumer {
    /// Creates the setup listener and a fresh Checks ring.
    pub fn open(config: ConsumerConfig) -> io::Result<Self> {
        saluki_fit::Consumer::open(config, DESCRIPTOR).map(Self)
    }

    /// Creates a Checks ring with local setup cancellation.
    pub fn open_with_cancel(config: ConsumerConfig, cancellation: &CancellationToken) -> io::Result<Self> {
        saluki_fit::Consumer::open_with_cancel(config, DESCRIPTOR, cancellation).map(Self)
    }

    /// Returns this session's identifier.
    pub fn session_id(&self) -> u64 {
        self.0.session_id()
    }

    /// Returns the next owned Checks datum after releasing its ring bytes.
    pub fn receive(&mut self) -> io::Result<Message> {
        let (kind, bytes) = self.0.receive()?;
        Message::decode_payload(kind, &bytes)
    }

    /// Returns the next datum, or `None` after local cancellation.
    pub fn receive_with_cancel(&mut self, cancellation: &CancellationToken) -> io::Result<Option<Message>> {
        self.0
            .receive_with_cancel(cancellation)?
            .map(|(kind, bytes)| Message::decode_payload(kind, &bytes))
            .transpose()
    }
}
