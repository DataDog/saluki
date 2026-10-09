use std::io;

use crate::wire::{payload_size, string_size, strings_size, Reader, Writer};

/// One scalar check metric, with the fields from `metric.proto`.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    /// Numeric `MetricType` from `metric.proto`, including unknown values.
    pub metric_type: i32,
    /// Metric name.
    pub name: String,
    /// Scalar value.
    pub value: f64,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Metric tags in order.
    pub tags: Vec<String>,
    /// Explicit hostname, or empty for the receiver's default.
    pub hostname: String,
    /// Rate interval in seconds, or zero for other metric types.
    pub interval_secs: u64,
}

/// One check-produced log, with the fields from `log.proto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    /// Log message.
    pub message: String,
    /// Numeric `LogLevel` from `log.proto`, including unknown values.
    pub level: i32,
}

/// One check status, with the fields from `service_check.proto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCheck {
    /// Numeric `Status` from `service_check.proto`, including unknown values.
    pub status: i32,
    /// Service-check name.
    pub name: String,
    /// Optional description, represented as an empty string when absent.
    pub message: String,
    /// Tags in order.
    pub tags: Vec<String>,
    /// Explicit hostname, or empty for the receiver's default.
    pub hostname: String,
}

/// One submitted event, with the fields from `event.proto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Event title.
    pub title: String,
    /// Event text.
    pub text: String,
    /// Numeric `Priority` from `event.proto`, including unknown values.
    pub priority: i32,
    /// Explicit hostname, or empty when absent.
    pub hostname: String,
    /// Tags in order.
    pub tags: Vec<String>,
    /// Numeric `AlertType` from `event.proto`, including unknown values.
    pub alert_type: i32,
    /// Aggregation key, or empty when absent.
    pub aggregation_key: String,
    /// Source type name, or empty when absent.
    pub source_type_name: String,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
}

/// A Checks payload that can be encoded as one FIT ring record.
pub trait Payload: Sized {
    /// FIT record type ID.
    const TYPE_ID: u32;

    /// Returns the exact encoded payload size, rejecting unsupported lengths.
    fn encoded_len(&self) -> io::Result<usize>;

    /// Encodes the payload with the versioned Checks FIT byte layout.
    fn encode_payload(&self) -> io::Result<Vec<u8>>;

    /// Decodes and validates one complete FIT record payload.
    fn decode_payload(bytes: &[u8]) -> io::Result<Self>;
}

impl Payload for Metric {
    const TYPE_ID: u32 = 1;

    fn encoded_len(&self) -> io::Result<usize> {
        payload_size(&[
            4,
            string_size(&self.name)?,
            8,
            8,
            strings_size(&self.tags)?,
            string_size(&self.hostname)?,
            8,
        ])
    }

    fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let size = self.encoded_len()?;
        let mut out = Writer::new(size)?;
        out.i32(self.metric_type);
        out.string(&self.name);
        out.f64(self.value);
        out.u64(self.timestamp);
        out.strings(&self.tags);
        out.string(&self.hostname);
        out.u64(self.interval_secs);
        Ok(out.finish(size))
    }

    fn decode_payload(bytes: &[u8]) -> io::Result<Self> {
        let mut input = Reader::new(bytes)?;
        let value = Self {
            metric_type: input.i32()?,
            name: input.string()?,
            value: input.f64()?,
            timestamp: input.u64()?,
            tags: input.strings()?,
            hostname: input.string()?,
            interval_secs: input.u64()?,
        };
        input.finish()?;
        Ok(value)
    }
}

impl Payload for Log {
    const TYPE_ID: u32 = 2;

    fn encoded_len(&self) -> io::Result<usize> {
        payload_size(&[string_size(&self.message)?, 4])
    }

    fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let size = self.encoded_len()?;
        let mut out = Writer::new(size)?;
        out.string(&self.message);
        out.i32(self.level);
        Ok(out.finish(size))
    }

    fn decode_payload(bytes: &[u8]) -> io::Result<Self> {
        let mut input = Reader::new(bytes)?;
        let value = Self {
            message: input.string()?,
            level: input.i32()?,
        };
        input.finish()?;
        Ok(value)
    }
}

impl Payload for ServiceCheck {
    const TYPE_ID: u32 = 3;

    fn encoded_len(&self) -> io::Result<usize> {
        payload_size(&[
            4,
            string_size(&self.name)?,
            string_size(&self.message)?,
            strings_size(&self.tags)?,
            string_size(&self.hostname)?,
        ])
    }

    fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let size = self.encoded_len()?;
        let mut out = Writer::new(size)?;
        out.i32(self.status);
        out.string(&self.name);
        out.string(&self.message);
        out.strings(&self.tags);
        out.string(&self.hostname);
        Ok(out.finish(size))
    }

    fn decode_payload(bytes: &[u8]) -> io::Result<Self> {
        let mut input = Reader::new(bytes)?;
        let value = Self {
            status: input.i32()?,
            name: input.string()?,
            message: input.string()?,
            tags: input.strings()?,
            hostname: input.string()?,
        };
        input.finish()?;
        Ok(value)
    }
}

impl Payload for Event {
    const TYPE_ID: u32 = 4;

    fn encoded_len(&self) -> io::Result<usize> {
        payload_size(&[
            string_size(&self.title)?,
            string_size(&self.text)?,
            4,
            string_size(&self.hostname)?,
            strings_size(&self.tags)?,
            4,
            string_size(&self.aggregation_key)?,
            string_size(&self.source_type_name)?,
            8,
        ])
    }

    fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let size = self.encoded_len()?;
        let mut out = Writer::new(size)?;
        out.string(&self.title);
        out.string(&self.text);
        out.i32(self.priority);
        out.string(&self.hostname);
        out.strings(&self.tags);
        out.i32(self.alert_type);
        out.string(&self.aggregation_key);
        out.string(&self.source_type_name);
        out.u64(self.timestamp);
        Ok(out.finish(size))
    }

    fn decode_payload(bytes: &[u8]) -> io::Result<Self> {
        let mut input = Reader::new(bytes)?;
        let value = Self {
            title: input.string()?,
            text: input.string()?,
            priority: input.i32()?,
            hostname: input.string()?,
            tags: input.strings()?,
            alert_type: input.i32()?,
            aggregation_key: input.string()?,
            source_type_name: input.string()?,
            timestamp: input.u64()?,
        };
        input.finish()?;
        Ok(value)
    }
}
