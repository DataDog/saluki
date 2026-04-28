use std::io::Read;

use rmp::Marker;

/// Maximum allowed element count for any array or map in a single payload (mirrors Go agent's 25 MB cap).
const MAX_SIZE: u64 = 25_000_000;

// ── Wire-format error type ──────────────────────────────────────────────────

// The enum fields carry diagnostic detail for logging/debugging. They are matched but not always
// destructured in production code, so the compiler considers the inner values "unread".
#[allow(dead_code)]
#[derive(Debug)]
pub(super) enum DeserializeError {
    UnexpectedEof,
    UnexpectedMarker(Marker),
    InvalidStringIndex(u32),
    InvalidUtf8,
    LimitExceeded(u64),
    /// Attribute array length was not a multiple of 3.
    InvalidAttributeCount(u32),
    /// Array element count for an AnyValue::Array was not a multiple of 2.
    InvalidArrayElementCount(u32),
    /// Field 1 (strings bulk-insert) appeared after another field was already decoded.
    StringsNotFirst,
    UnknownAnyValueType(u32),
    /// TraceID binary payload was not exactly 16 bytes.
    InvalidTraceIdLength(u32),
}

impl std::fmt::Display for DeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for DeserializeError {}

// ── Wire-format string table ────────────────────────────────────────────────

/// Interned string table shared across an entire v1 payload.
///
/// Index 0 is always the empty string `""`. All subsequent indices are assigned in the order
/// strings are first encountered during deserialization.
#[derive(Debug)]
pub(super) struct StringTable {
    strings: Vec<String>,
}

impl StringTable {
    pub(super) fn new() -> Self {
        Self { strings: vec![String::new()] }
    }

    pub(super) fn push(&mut self, s: String) -> u32 {
        let idx = self.strings.len() as u32;
        self.strings.push(s);
        idx
    }

    #[cfg(test)]
    pub(super) fn get(&self, idx: u32) -> Option<&str> {
        self.strings.get(idx as usize).map(|s| s.as_str())
    }

    pub(super) fn len(&self) -> usize {
        self.strings.len()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &str> {
        self.strings.iter().map(|s| s.as_str())
    }
}

// ── Raw wire-format payload types (private deserialization intermediates) ───

#[derive(Debug)]
pub(super) struct RawTracerPayload {
    pub(super) string_table: StringTable,
    pub(super) container_id: u32,
    pub(super) language_name: u32,
    pub(super) language_version: u32,
    pub(super) tracer_version: u32,
    pub(super) runtime_id: u32,
    pub(super) env: u32,
    pub(super) hostname: u32,
    pub(super) app_version: u32,
    pub(super) attributes: Vec<RawKeyValue>,
    pub(super) chunks: Vec<RawTraceChunk>,
}

#[derive(Debug)]
pub(super) struct RawTraceChunk {
    pub(super) priority: i32,
    pub(super) origin: u32,
    pub(super) attributes: Vec<RawKeyValue>,
    pub(super) spans: Vec<RawSpan>,
    pub(super) dropped_trace: bool,
    pub(super) trace_id_high: u64,
    pub(super) trace_id_low: u64,
    pub(super) sampling_mechanism: u32,
}

#[derive(Debug)]
pub(super) struct RawSpan {
    pub(super) service: u32,
    pub(super) name: u32,
    pub(super) resource: u32,
    pub(super) span_id: u64,
    pub(super) parent_id: u64,
    pub(super) start: u64,
    pub(super) duration: u64,
    pub(super) error: bool,
    pub(super) attributes: Vec<RawKeyValue>,
    pub(super) span_type: u32,
    pub(super) links: Vec<RawSpanLink>,
    pub(super) events: Vec<RawSpanEvent>,
    pub(super) env: u32,
    pub(super) version: u32,
    pub(super) component: u32,
    pub(super) kind: u32,
}

#[derive(Debug)]
pub(super) struct RawSpanLink {
    pub(super) trace_id_high: u64,
    pub(super) trace_id_low: u64,
    pub(super) span_id: u64,
    pub(super) attributes: Vec<RawKeyValue>,
    pub(super) tracestate: u32,
    pub(super) flags: u32,
}

#[derive(Debug)]
pub(super) struct RawSpanEvent {
    pub(super) time_unix_nano: u64,
    pub(super) name: u32,
    pub(super) attributes: Vec<RawKeyValue>,
}

#[derive(Debug)]
pub(super) struct RawKeyValue {
    pub(super) key: u32,
    pub(super) value: RawAnyValue,
}

#[derive(Debug)]
pub(super) enum RawAnyValue {
    String(u32),
    Bool(bool),
    Double(f64),
    Int(i64),
    Bytes(Vec<u8>),
    Array(Vec<RawAnyValue>),
    KeyValueList(Vec<RawKeyValue>),
}

// ── Error conversion helpers ────────────────────────────────────────────────

fn vr_err(e: rmp::decode::ValueReadError<std::io::Error>) -> DeserializeError {
    match e {
        rmp::decode::ValueReadError::InvalidMarkerRead(_) | rmp::decode::ValueReadError::InvalidDataRead(_) => {
            DeserializeError::UnexpectedEof
        }
        rmp::decode::ValueReadError::TypeMismatch(m) => DeserializeError::UnexpectedMarker(m),
    }
}

fn nvr_err(e: rmp::decode::NumValueReadError<std::io::Error>) -> DeserializeError {
    match e {
        rmp::decode::NumValueReadError::InvalidMarkerRead(_)
        | rmp::decode::NumValueReadError::InvalidDataRead(_)
        | rmp::decode::NumValueReadError::OutOfRange => DeserializeError::UnexpectedEof,
        rmp::decode::NumValueReadError::TypeMismatch(m) => DeserializeError::UnexpectedMarker(m),
    }
}

// ── Low-level byte helpers ──────────────────────────────────────────────────

fn skip_bytes<R: Read>(rd: &mut R, mut n: usize) -> Result<(), DeserializeError> {
    let mut buf = [0u8; 1024];
    while n > 0 {
        let chunk = n.min(buf.len());
        rd.read_exact(&mut buf[..chunk]).map_err(|_| DeserializeError::UnexpectedEof)?;
        n -= chunk;
    }
    Ok(())
}

fn read_u8_raw<R: Read>(rd: &mut R) -> Result<u8, DeserializeError> {
    let mut b = [0u8; 1];
    rd.read_exact(&mut b).map_err(|_| DeserializeError::UnexpectedEof)?;
    Ok(b[0])
}

fn read_u16_be<R: Read>(rd: &mut R) -> Result<u16, DeserializeError> {
    let mut b = [0u8; 2];
    rd.read_exact(&mut b).map_err(|_| DeserializeError::UnexpectedEof)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32_be<R: Read>(rd: &mut R) -> Result<u32, DeserializeError> {
    let mut b = [0u8; 4];
    rd.read_exact(&mut b).map_err(|_| DeserializeError::UnexpectedEof)?;
    Ok(u32::from_be_bytes(b))
}

// ── String helpers ──────────────────────────────────────────────────────────

/// Read the body of a msgpack string given that the leading marker has already been consumed.
fn read_str_body<R: Read>(rd: &mut R, marker: Marker) -> Result<String, DeserializeError> {
    let len = match marker {
        Marker::FixStr(n) => n as u32,
        Marker::Str8 => read_u8_raw(rd)? as u32,
        Marker::Str16 => read_u16_be(rd)? as u32,
        Marker::Str32 => read_u32_be(rd)?,
        _ => return Err(DeserializeError::UnexpectedMarker(marker)),
    };
    let mut buf = vec![0u8; len as usize];
    rd.read_exact(&mut buf).map_err(|_| DeserializeError::UnexpectedEof)?;
    String::from_utf8(buf).map_err(|_| DeserializeError::InvalidUtf8)
}

/// Read a complete msgpack string (marker + body).
fn read_raw_string<R: Read>(rd: &mut R) -> Result<String, DeserializeError> {
    let marker = rmp::decode::read_marker(rd).map_err(|_| DeserializeError::UnexpectedEof)?;
    read_str_body(rd, marker)
}

/// Read a uint given that the leading marker has already been consumed.
fn read_uint_from_marker<R: Read>(rd: &mut R, marker: Marker) -> Result<u32, DeserializeError> {
    match marker {
        Marker::FixPos(v) => Ok(v as u32),
        Marker::U8 => Ok(read_u8_raw(rd)? as u32),
        Marker::U16 => Ok(read_u16_be(rd)? as u32),
        Marker::U32 => Ok(read_u32_be(rd)?),
        Marker::U64 => {
            let mut b = [0u8; 8];
            rd.read_exact(&mut b).map_err(|_| DeserializeError::UnexpectedEof)?;
            let v = u64::from_be_bytes(b);
            u32::try_from(v).map_err(|_| DeserializeError::UnexpectedMarker(marker))
        }
        _ => Err(DeserializeError::UnexpectedMarker(marker)),
    }
}

/// Decode a streaming string field.
///
/// If the next msgpack value is a string, it is a new entry added to the table.
/// If it is a uint, it is a back-reference to a previously-seen string.
fn decode_streaming_string<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<u32, DeserializeError> {
    let marker = rmp::decode::read_marker(rd).map_err(|_| DeserializeError::UnexpectedEof)?;
    match marker {
        Marker::FixStr(_) | Marker::Str8 | Marker::Str16 | Marker::Str32 => {
            let s = read_str_body(rd, marker)?;
            Ok(table.push(s))
        }
        Marker::FixPos(_) | Marker::U8 | Marker::U16 | Marker::U32 | Marker::U64 => {
            let idx = read_uint_from_marker(rd, marker)?;
            if idx as usize >= table.len() {
                return Err(DeserializeError::InvalidStringIndex(idx));
            }
            Ok(idx)
        }
        _ => Err(DeserializeError::UnexpectedMarker(marker)),
    }
}

// ── Skip helper ─────────────────────────────────────────────────────────────

/// Discard one complete msgpack value from `rd`, regardless of type.
pub(super) fn skip_msgpack_value<R: Read>(rd: &mut R) -> Result<(), DeserializeError> {
    let marker = rmp::decode::read_marker(rd).map_err(|_| DeserializeError::UnexpectedEof)?;
    match marker {
        Marker::Null | Marker::True | Marker::False | Marker::FixPos(_) | Marker::FixNeg(_) => Ok(()),
        Marker::U8 | Marker::I8 => skip_bytes(rd, 1),
        Marker::U16 | Marker::I16 => skip_bytes(rd, 2),
        Marker::U32 | Marker::I32 | Marker::F32 => skip_bytes(rd, 4),
        Marker::U64 | Marker::I64 | Marker::F64 => skip_bytes(rd, 8),
        Marker::FixStr(n) => skip_bytes(rd, n as usize),
        Marker::Str8 => {
            let len = read_u8_raw(rd)? as usize;
            skip_bytes(rd, len)
        }
        Marker::Str16 => {
            let len = read_u16_be(rd)? as usize;
            skip_bytes(rd, len)
        }
        Marker::Str32 => {
            let len = read_u32_be(rd)? as usize;
            skip_bytes(rd, len)
        }
        Marker::Bin8 => {
            let len = read_u8_raw(rd)? as usize;
            skip_bytes(rd, len)
        }
        Marker::Bin16 => {
            let len = read_u16_be(rd)? as usize;
            skip_bytes(rd, len)
        }
        Marker::Bin32 => {
            let len = read_u32_be(rd)? as usize;
            skip_bytes(rd, len)
        }
        Marker::FixArray(n) => {
            for _ in 0..n {
                skip_msgpack_value(rd)?;
            }
            Ok(())
        }
        Marker::Array16 => {
            let len = read_u16_be(rd)?;
            for _ in 0..len {
                skip_msgpack_value(rd)?;
            }
            Ok(())
        }
        Marker::Array32 => {
            let len = read_u32_be(rd)?;
            for _ in 0..len {
                skip_msgpack_value(rd)?;
            }
            Ok(())
        }
        Marker::FixMap(n) => {
            for _ in 0..n {
                skip_msgpack_value(rd)?;
                skip_msgpack_value(rd)?;
            }
            Ok(())
        }
        Marker::Map16 => {
            let len = read_u16_be(rd)?;
            for _ in 0..len {
                skip_msgpack_value(rd)?;
                skip_msgpack_value(rd)?;
            }
            Ok(())
        }
        Marker::Map32 => {
            let len = read_u32_be(rd)?;
            for _ in 0..len {
                skip_msgpack_value(rd)?;
                skip_msgpack_value(rd)?;
            }
            Ok(())
        }
        Marker::FixExt1 => skip_bytes(rd, 2),
        Marker::FixExt2 => skip_bytes(rd, 3),
        Marker::FixExt4 => skip_bytes(rd, 5),
        Marker::FixExt8 => skip_bytes(rd, 9),
        Marker::FixExt16 => skip_bytes(rd, 17),
        Marker::Ext8 => {
            let len = read_u8_raw(rd)? as usize;
            skip_bytes(rd, 1 + len)
        }
        Marker::Ext16 => {
            let len = read_u16_be(rd)? as usize;
            skip_bytes(rd, 1 + len)
        }
        Marker::Ext32 => {
            let len = read_u32_be(rd)? as usize;
            skip_bytes(rd, 1 + len)
        }
        Marker::Reserved => Err(DeserializeError::UnexpectedMarker(marker)),
    }
}

// ── Attribute / AnyValue decoding ───────────────────────────────────────────

fn decode_attributes<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<Vec<RawKeyValue>, DeserializeError> {
    let num_elements = rmp::decode::read_array_len(rd).map_err(vr_err)?;
    if num_elements as u64 > MAX_SIZE {
        return Err(DeserializeError::LimitExceeded(num_elements as u64));
    }
    if num_elements % 3 != 0 {
        return Err(DeserializeError::InvalidAttributeCount(num_elements));
    }
    let mut kvs = Vec::with_capacity(num_elements as usize / 3);
    for _ in 0..num_elements / 3 {
        let key = decode_streaming_string(rd, table)?;
        let value = decode_any_value(rd, table)?;
        kvs.push(RawKeyValue { key, value });
    }
    Ok(kvs)
}

enum AnyValueTypeTag {
    String = 1,
    Bool = 2,
    Double = 3,
    Int = 4,
    Bytes = 5,
    Array = 6,
    KeyValueList = 7,
}

impl AnyValueTypeTag {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::String),
            2 => Some(Self::Bool),
            3 => Some(Self::Double),
            4 => Some(Self::Int),
            5 => Some(Self::Bytes),
            6 => Some(Self::Array),
            7 => Some(Self::KeyValueList),
            _ => None,
        }
    }
}

fn decode_any_value<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<RawAnyValue, DeserializeError> {
    let raw: u32 = rmp::decode::read_int(rd).map_err(nvr_err)?;
    let tag = AnyValueTypeTag::from_u32(raw).ok_or(DeserializeError::UnknownAnyValueType(raw))?;
    match tag {
        AnyValueTypeTag::String => Ok(RawAnyValue::String(decode_streaming_string(rd, table)?)),
        AnyValueTypeTag::Bool => Ok(RawAnyValue::Bool(rmp::decode::read_bool(rd).map_err(vr_err)?)),
        AnyValueTypeTag::Double => Ok(RawAnyValue::Double(rmp::decode::read_f64(rd).map_err(vr_err)?)),
        AnyValueTypeTag::Int => {
            let v: i64 = rmp::decode::read_int(rd).map_err(nvr_err)?;
            Ok(RawAnyValue::Int(v))
        }
        AnyValueTypeTag::Bytes => {
            let bin_len = rmp::decode::read_bin_len(rd).map_err(vr_err)?;
            let mut buf = vec![0u8; bin_len as usize];
            rd.read_exact(&mut buf).map_err(|_| DeserializeError::UnexpectedEof)?;
            Ok(RawAnyValue::Bytes(buf))
        }
        AnyValueTypeTag::Array => {
            let num_elements = rmp::decode::read_array_len(rd).map_err(vr_err)?;
            if num_elements as u64 > MAX_SIZE {
                return Err(DeserializeError::LimitExceeded(num_elements as u64));
            }
            if num_elements % 2 != 0 {
                return Err(DeserializeError::InvalidArrayElementCount(num_elements));
            }
            let mut values = Vec::with_capacity(num_elements as usize / 2);
            for _ in 0..num_elements / 2 {
                values.push(decode_any_value(rd, table)?);
            }
            Ok(RawAnyValue::Array(values))
        }
        AnyValueTypeTag::KeyValueList => Ok(RawAnyValue::KeyValueList(decode_attributes(rd, table)?)),
    }
}

// ── Wire field-number constants ─────────────────────────────────────────────

mod span_link {
    pub const FIELD_TRACE_ID: u32 = 1;
    pub const FIELD_SPAN_ID: u32 = 2;
    pub const FIELD_ATTRIBUTES: u32 = 3;
    pub const FIELD_TRACESTATE: u32 = 4;
    pub const FIELD_FLAGS: u32 = 5;
}

mod span_event {
    pub const FIELD_TIME_UNIX_NANO: u32 = 1;
    pub const FIELD_NAME: u32 = 2;
    pub const FIELD_ATTRIBUTES: u32 = 3;
}

mod span {
    pub const FIELD_SERVICE: u32 = 1;
    pub const FIELD_NAME: u32 = 2;
    pub const FIELD_RESOURCE: u32 = 3;
    pub const FIELD_SPAN_ID: u32 = 4;
    pub const FIELD_PARENT_ID: u32 = 5;
    pub const FIELD_START: u32 = 6;
    pub const FIELD_DURATION: u32 = 7;
    pub const FIELD_ERROR: u32 = 8;
    pub const FIELD_ATTRIBUTES: u32 = 9;
    pub const FIELD_TYPE: u32 = 10;
    pub const FIELD_LINKS: u32 = 11;
    pub const FIELD_EVENTS: u32 = 12;
    pub const FIELD_ENV: u32 = 13;
    pub const FIELD_VERSION: u32 = 14;
    pub const FIELD_COMPONENT: u32 = 15;
    pub const FIELD_KIND: u32 = 16;
}

mod trace_chunk {
    pub const FIELD_PRIORITY: u32 = 1;
    pub const FIELD_ORIGIN: u32 = 2;
    pub const FIELD_ATTRIBUTES: u32 = 3;
    pub const FIELD_SPANS: u32 = 4;
    pub const FIELD_DROPPED_TRACE: u32 = 5;
    pub const FIELD_TRACE_ID: u32 = 6;
    pub const FIELD_SAMPLING_MECHANISM: u32 = 7;
}

mod tracer_payload {
    pub const FIELD_STRINGS: u32 = 1;
    pub const FIELD_CONTAINER_ID: u32 = 2;
    pub const FIELD_LANGUAGE_NAME: u32 = 3;
    pub const FIELD_LANGUAGE_VERSION: u32 = 4;
    pub const FIELD_TRACER_VERSION: u32 = 5;
    pub const FIELD_RUNTIME_ID: u32 = 6;
    pub const FIELD_ENV: u32 = 7;
    pub const FIELD_HOSTNAME: u32 = 8;
    pub const FIELD_APP_VERSION: u32 = 9;
    pub const FIELD_ATTRIBUTES: u32 = 10;
    pub const FIELD_CHUNKS: u32 = 11;
}

// ── SpanLink / SpanEvent ────────────────────────────────────────────────────

fn decode_span_link<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<RawSpanLink, DeserializeError> {
    let map_len = rmp::decode::read_map_len(rd).map_err(vr_err)?;
    if map_len as u64 > MAX_SIZE {
        return Err(DeserializeError::LimitExceeded(map_len as u64));
    }

    let mut link = RawSpanLink {
        trace_id_high: 0,
        trace_id_low: 0,
        span_id: 0,
        attributes: Vec::new(),
        tracestate: 0,
        flags: 0,
    };

    for _ in 0..map_len {
        let field_num: u32 = rmp::decode::read_int(rd).map_err(nvr_err)?;
        match field_num {
            span_link::FIELD_TRACE_ID => {
                let bin_len = rmp::decode::read_bin_len(rd).map_err(vr_err)?;
                if bin_len != 16 {
                    return Err(DeserializeError::InvalidTraceIdLength(bin_len));
                }
                let mut buf = [0u8; 16];
                rd.read_exact(&mut buf).map_err(|_| DeserializeError::UnexpectedEof)?;
                link.trace_id_high = u64::from_be_bytes(buf[..8].try_into().unwrap());
                link.trace_id_low = u64::from_be_bytes(buf[8..].try_into().unwrap());
            }
            span_link::FIELD_SPAN_ID => link.span_id = rmp::decode::read_int(rd).map_err(nvr_err)?,
            span_link::FIELD_ATTRIBUTES => link.attributes = decode_attributes(rd, table)?,
            span_link::FIELD_TRACESTATE => link.tracestate = decode_streaming_string(rd, table)?,
            span_link::FIELD_FLAGS => link.flags = rmp::decode::read_int(rd).map_err(nvr_err)?,
            _ => {
                skip_msgpack_value(rd)?;
            }
        }
    }
    Ok(link)
}

fn decode_span_event<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<RawSpanEvent, DeserializeError> {
    let map_len = rmp::decode::read_map_len(rd).map_err(vr_err)?;
    if map_len as u64 > MAX_SIZE {
        return Err(DeserializeError::LimitExceeded(map_len as u64));
    }

    let mut event = RawSpanEvent { time_unix_nano: 0, name: 0, attributes: Vec::new() };

    for _ in 0..map_len {
        let field_num: u32 = rmp::decode::read_int(rd).map_err(nvr_err)?;
        match field_num {
            span_event::FIELD_TIME_UNIX_NANO => event.time_unix_nano = rmp::decode::read_int(rd).map_err(nvr_err)?,
            span_event::FIELD_NAME => event.name = decode_streaming_string(rd, table)?,
            span_event::FIELD_ATTRIBUTES => event.attributes = decode_attributes(rd, table)?,
            _ => {
                skip_msgpack_value(rd)?;
            }
        }
    }
    Ok(event)
}

// ── Span ────────────────────────────────────────────────────────────────────

fn decode_span<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<RawSpan, DeserializeError> {
    let map_len = rmp::decode::read_map_len(rd).map_err(vr_err)?;
    if map_len as u64 > MAX_SIZE {
        return Err(DeserializeError::LimitExceeded(map_len as u64));
    }

    let mut s = RawSpan {
        service: 0,
        name: 0,
        resource: 0,
        span_id: 0,
        parent_id: 0,
        start: 0,
        duration: 0,
        error: false,
        attributes: Vec::new(),
        span_type: 0,
        links: Vec::new(),
        events: Vec::new(),
        env: 0,
        version: 0,
        component: 0,
        kind: 0,
    };

    for _ in 0..map_len {
        let field_num: u32 = rmp::decode::read_int(rd).map_err(nvr_err)?;
        match field_num {
            span::FIELD_SERVICE => s.service = decode_streaming_string(rd, table)?,
            span::FIELD_NAME => s.name = decode_streaming_string(rd, table)?,
            span::FIELD_RESOURCE => s.resource = decode_streaming_string(rd, table)?,
            span::FIELD_SPAN_ID => s.span_id = rmp::decode::read_int(rd).map_err(nvr_err)?,
            span::FIELD_PARENT_ID => s.parent_id = rmp::decode::read_int(rd).map_err(nvr_err)?,
            span::FIELD_START => s.start = rmp::decode::read_int(rd).map_err(nvr_err)?,
            span::FIELD_DURATION => s.duration = rmp::decode::read_int(rd).map_err(nvr_err)?,
            span::FIELD_ERROR => s.error = rmp::decode::read_bool(rd).map_err(vr_err)?,
            span::FIELD_ATTRIBUTES => s.attributes = decode_attributes(rd, table)?,
            span::FIELD_TYPE => s.span_type = decode_streaming_string(rd, table)?,
            span::FIELD_LINKS => {
                let arr_len = rmp::decode::read_array_len(rd).map_err(vr_err)?;
                if arr_len as u64 > MAX_SIZE {
                    return Err(DeserializeError::LimitExceeded(arr_len as u64));
                }
                s.links = (0..arr_len)
                    .map(|_| decode_span_link(rd, table))
                    .collect::<Result<_, _>>()?;
            }
            span::FIELD_EVENTS => {
                let arr_len = rmp::decode::read_array_len(rd).map_err(vr_err)?;
                if arr_len as u64 > MAX_SIZE {
                    return Err(DeserializeError::LimitExceeded(arr_len as u64));
                }
                s.events = (0..arr_len)
                    .map(|_| decode_span_event(rd, table))
                    .collect::<Result<_, _>>()?;
            }
            span::FIELD_ENV => s.env = decode_streaming_string(rd, table)?,
            span::FIELD_VERSION => s.version = decode_streaming_string(rd, table)?,
            span::FIELD_COMPONENT => s.component = decode_streaming_string(rd, table)?,
            span::FIELD_KIND => s.kind = rmp::decode::read_int(rd).map_err(nvr_err)?,
            _ => {
                skip_msgpack_value(rd)?;
            }
        }
    }
    Ok(s)
}

// ── TraceChunk ──────────────────────────────────────────────────────────────

fn decode_chunk<R: Read>(rd: &mut R, table: &mut StringTable) -> Result<RawTraceChunk, DeserializeError> {
    let map_len = rmp::decode::read_map_len(rd).map_err(vr_err)?;
    if map_len as u64 > MAX_SIZE {
        return Err(DeserializeError::LimitExceeded(map_len as u64));
    }

    let mut chunk = RawTraceChunk {
        priority: 0,
        origin: 0,
        attributes: Vec::new(),
        spans: Vec::new(),
        dropped_trace: false,
        trace_id_high: 0,
        trace_id_low: 0,
        sampling_mechanism: 0,
    };

    for _ in 0..map_len {
        let field_num: u32 = rmp::decode::read_int(rd).map_err(nvr_err)?;
        match field_num {
            trace_chunk::FIELD_PRIORITY => chunk.priority = rmp::decode::read_int(rd).map_err(nvr_err)?,
            trace_chunk::FIELD_ORIGIN => chunk.origin = decode_streaming_string(rd, table)?,
            trace_chunk::FIELD_ATTRIBUTES => chunk.attributes = decode_attributes(rd, table)?,
            trace_chunk::FIELD_SPANS => {
                let arr_len = rmp::decode::read_array_len(rd).map_err(vr_err)?;
                if arr_len as u64 > MAX_SIZE {
                    return Err(DeserializeError::LimitExceeded(arr_len as u64));
                }
                chunk.spans = (0..arr_len)
                    .map(|_| decode_span(rd, table))
                    .collect::<Result<_, _>>()?;
            }
            trace_chunk::FIELD_DROPPED_TRACE => chunk.dropped_trace = rmp::decode::read_bool(rd).map_err(vr_err)?,
            trace_chunk::FIELD_TRACE_ID => {
                let bin_len = rmp::decode::read_bin_len(rd).map_err(vr_err)?;
                if bin_len != 16 {
                    return Err(DeserializeError::InvalidTraceIdLength(bin_len));
                }
                let mut buf = [0u8; 16];
                rd.read_exact(&mut buf).map_err(|_| DeserializeError::UnexpectedEof)?;
                chunk.trace_id_high = u64::from_be_bytes(buf[..8].try_into().unwrap());
                chunk.trace_id_low = u64::from_be_bytes(buf[8..].try_into().unwrap());
            }
            trace_chunk::FIELD_SAMPLING_MECHANISM => {
                chunk.sampling_mechanism = rmp::decode::read_int(rd).map_err(nvr_err)?
            }
            _ => {
                skip_msgpack_value(rd)?;
            }
        }
    }
    Ok(chunk)
}

// ── TracerPayload ───────────────────────────────────────────────────────────

pub(super) fn decode_tracer_payload<R: Read>(rd: &mut R) -> Result<RawTracerPayload, DeserializeError> {
    let map_len = rmp::decode::read_map_len(rd).map_err(vr_err)?;
    if map_len as u64 > MAX_SIZE {
        return Err(DeserializeError::LimitExceeded(map_len as u64));
    }

    let mut table = StringTable::new();
    let mut container_id = 0u32;
    let mut language_name = 0u32;
    let mut language_version = 0u32;
    let mut tracer_version = 0u32;
    let mut runtime_id = 0u32;
    let mut env = 0u32;
    let mut hostname = 0u32;
    let mut app_version = 0u32;
    let mut attributes = Vec::new();
    let mut chunks = Vec::new();

    let mut non_strings_seen = false;

    for _ in 0..map_len {
        let field_num: u32 = rmp::decode::read_int(rd).map_err(nvr_err)?;
        match field_num {
            tracer_payload::FIELD_STRINGS => {
                if non_strings_seen {
                    return Err(DeserializeError::StringsNotFirst);
                }
                let arr_len = rmp::decode::read_array_len(rd).map_err(vr_err)?;
                if arr_len as u64 > MAX_SIZE {
                    return Err(DeserializeError::LimitExceeded(arr_len as u64));
                }
                for _ in 0..arr_len {
                    let s = read_raw_string(rd)?;
                    if !s.is_empty() {
                        table.push(s);
                    }
                }
            }
            tracer_payload::FIELD_CONTAINER_ID => {
                non_strings_seen = true;
                container_id = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_LANGUAGE_NAME => {
                non_strings_seen = true;
                language_name = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_LANGUAGE_VERSION => {
                non_strings_seen = true;
                language_version = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_TRACER_VERSION => {
                non_strings_seen = true;
                tracer_version = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_RUNTIME_ID => {
                non_strings_seen = true;
                runtime_id = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_ENV => {
                non_strings_seen = true;
                env = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_HOSTNAME => {
                non_strings_seen = true;
                hostname = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_APP_VERSION => {
                non_strings_seen = true;
                app_version = decode_streaming_string(rd, &mut table)?;
            }
            tracer_payload::FIELD_ATTRIBUTES => {
                non_strings_seen = true;
                attributes = decode_attributes(rd, &mut table)?;
            }
            tracer_payload::FIELD_CHUNKS => {
                non_strings_seen = true;
                let arr_len = rmp::decode::read_array_len(rd).map_err(vr_err)?;
                if arr_len as u64 > MAX_SIZE {
                    return Err(DeserializeError::LimitExceeded(arr_len as u64));
                }
                chunks = (0..arr_len)
                    .map(|_| decode_chunk(rd, &mut table))
                    .collect::<Result<_, _>>()?;
            }
            _ => {
                non_strings_seen = true;
                skip_msgpack_value(rd)?;
            }
        }
    }

    Ok(RawTracerPayload {
        string_table: table,
        container_id,
        language_name,
        language_version,
        tracer_version,
        runtime_id,
        env,
        hostname,
        app_version,
        attributes,
        chunks,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Encoding helpers ────────────────────────────────────────────────────

    fn encode_fixmap_header(count: u8) -> Vec<u8> {
        assert!(count <= 15, "fixmap supports 0-15 entries; use encode_map16_header for more");
        vec![0x80 | (count & 0x0f)]
    }

    fn encode_map16_header(count: u16) -> Vec<u8> {
        let mut b = vec![0xde];
        b.extend_from_slice(&count.to_be_bytes());
        b
    }

    fn encode_fixarray_header(count: u8) -> Vec<u8> {
        assert!(count <= 15, "fixarray supports 0-15 entries; use encode_array16_header for more");
        vec![0x90 | (count & 0x0f)]
    }

    fn encode_array16_header(count: u16) -> Vec<u8> {
        let mut b = vec![0xdc];
        b.extend_from_slice(&count.to_be_bytes());
        b
    }

    fn encode_fixpos(v: u8) -> Vec<u8> {
        vec![v]
    }

    fn encode_u8(v: u8) -> Vec<u8> {
        vec![0xcc, v]
    }

    fn encode_i32(v: i32) -> Vec<u8> {
        let mut b = vec![0xd2];
        b.extend_from_slice(&v.to_be_bytes());
        b
    }

    fn encode_i64(v: i64) -> Vec<u8> {
        let mut b = vec![0xd3];
        b.extend_from_slice(&v.to_be_bytes());
        b
    }

    fn encode_u64(v: u64) -> Vec<u8> {
        let mut b = vec![0xcf];
        b.extend_from_slice(&v.to_be_bytes());
        b
    }

    fn encode_f64(v: f64) -> Vec<u8> {
        let mut b = vec![0xcb];
        b.extend_from_slice(&v.to_bits().to_be_bytes());
        b
    }

    fn encode_bool(v: bool) -> Vec<u8> {
        vec![if v { 0xc3 } else { 0xc2 }]
    }

    fn encode_nil() -> Vec<u8> {
        vec![0xc0]
    }

    fn encode_fixstr(s: &str) -> Vec<u8> {
        assert!(s.len() <= 31, "use encode_str8 for longer strings");
        let mut b = vec![0xa0 | s.len() as u8];
        b.extend_from_slice(s.as_bytes());
        b
    }

    fn encode_str8(s: &str) -> Vec<u8> {
        assert!(s.len() <= 255);
        let mut b = vec![0xd9, s.len() as u8];
        b.extend_from_slice(s.as_bytes());
        b
    }

    fn encode_bin8(data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= 255);
        let mut b = vec![0xc4, data.len() as u8];
        b.extend_from_slice(data);
        b
    }

    fn encode_trace_id(high: u64, low: u64) -> Vec<u8> {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&high.to_be_bytes());
        data.extend_from_slice(&low.to_be_bytes());
        encode_bin8(&data)
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    }

    // ── StringTable tests ───────────────────────────────────────────────────

    #[test]
    fn string_table_index_zero_is_empty() {
        let table = StringTable::new();
        assert_eq!(table.get(0), Some(""));
    }

    #[test]
    fn string_table_push_and_get() {
        let mut table = StringTable::new();
        let idx = table.push("hello".to_owned());
        assert_eq!(idx, 1);
        assert_eq!(table.get(1), Some("hello"));
    }

    #[test]
    fn string_table_out_of_bounds_returns_none() {
        let table = StringTable::new();
        assert_eq!(table.get(1), None);
        assert_eq!(table.get(999), None);
    }

    // ── decode_streaming_string ─────────────────────────────────────────────

    #[test]
    fn streaming_string_new_inline_string_added_to_table() {
        let mut table = StringTable::new();
        let data = encode_fixstr("hello");
        let mut rd = data.as_slice();
        let idx = decode_streaming_string(&mut rd, &mut table).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(table.get(1), Some("hello"));
    }

    #[test]
    fn streaming_string_back_reference_resolves_correctly() {
        let mut table = StringTable::new();
        table.push("world".to_owned());

        let data = encode_fixpos(1);
        let mut rd = data.as_slice();
        let idx = decode_streaming_string(&mut rd, &mut table).unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn streaming_string_index_zero_resolves_to_empty() {
        let mut table = StringTable::new();
        let data = encode_fixpos(0);
        let mut rd = data.as_slice();
        let idx = decode_streaming_string(&mut rd, &mut table).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(table.get(0), Some(""));
    }

    #[test]
    fn streaming_string_out_of_bounds_index_is_error() {
        let mut table = StringTable::new();
        let data = encode_fixpos(5);
        let mut rd = data.as_slice();
        let err = decode_streaming_string(&mut rd, &mut table).unwrap_err();
        assert!(matches!(err, DeserializeError::InvalidStringIndex(5)));
    }

    #[test]
    fn streaming_string_u8_encoded_index() {
        let mut table = StringTable::new();
        table.push("a".to_owned());

        let data = encode_u8(1);
        let mut rd = data.as_slice();
        let idx = decode_streaming_string(&mut rd, &mut table).unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn streaming_string_str8_encoding() {
        let mut table = StringTable::new();
        let s = "x".repeat(50);
        let data = encode_str8(&s);
        let mut rd = data.as_slice();
        let idx = decode_streaming_string(&mut rd, &mut table).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(table.get(1), Some(s.as_str()));
    }

    // ── Field 1 bulk-insert ─────────────────────────────────────────────────

    #[test]
    fn payload_field1_bulk_inserts_strings() {
        let strings_arr = concat(&[
            encode_fixarray_header(3),
            encode_fixstr("svc"),
            encode_fixstr("web"),
            encode_fixstr("prod"),
        ]);
        let data = concat(&[encode_fixmap_header(1), encode_fixpos(1), strings_arr]);
        let mut rd = data.as_slice();
        let payload = decode_tracer_payload(&mut rd).unwrap();
        assert_eq!(payload.string_table.get(1), Some("svc"));
        assert_eq!(payload.string_table.get(2), Some("web"));
        assert_eq!(payload.string_table.get(3), Some("prod"));
        assert_eq!(payload.chunks.len(), 0);
    }

    #[test]
    fn payload_field1_after_other_field_is_error() {
        let data = concat(&[
            encode_fixmap_header(2),
            encode_fixpos(2),
            encode_fixstr("mycontainer"),
            encode_fixpos(1),
            concat(&[encode_fixarray_header(1), encode_fixstr("x")]),
        ]);
        let mut rd = data.as_slice();
        let err = decode_tracer_payload(&mut rd).unwrap_err();
        assert!(matches!(err, DeserializeError::StringsNotFirst));
    }

    // ── AnyValue decoding ───────────────────────────────────────────────────

    fn decode_av(data: &[u8]) -> RawAnyValue {
        let mut table = StringTable::new();
        let mut rd = data;
        decode_any_value(&mut rd, &mut table).unwrap()
    }

    #[test]
    fn anyvalue_type1_string_inline() {
        let mut table = StringTable::new();
        let data = concat(&[encode_fixpos(1), encode_fixstr("hello")]);
        let mut rd = data.as_slice();
        let av = decode_any_value(&mut rd, &mut table).unwrap();
        assert!(matches!(av, RawAnyValue::String(1)));
        assert_eq!(table.get(1), Some("hello"));
    }

    #[test]
    fn anyvalue_type1_string_via_index() {
        let mut table = StringTable::new();
        table.push("hello".to_owned());
        let data = concat(&[encode_fixpos(1), encode_fixpos(1)]);
        let mut rd = data.as_slice();
        let av = decode_any_value(&mut rd, &mut table).unwrap();
        assert!(matches!(av, RawAnyValue::String(1)));
    }

    #[test]
    fn anyvalue_type2_bool_true() {
        let data = concat(&[encode_fixpos(2), encode_bool(true)]);
        assert!(matches!(decode_av(&data), RawAnyValue::Bool(true)));
    }

    #[test]
    fn anyvalue_type2_bool_false() {
        let data = concat(&[encode_fixpos(2), encode_bool(false)]);
        assert!(matches!(decode_av(&data), RawAnyValue::Bool(false)));
    }

    #[test]
    fn anyvalue_type3_double() {
        let data = concat(&[encode_fixpos(3), encode_f64(3.14)]);
        let RawAnyValue::Double(v) = decode_av(&data) else { panic!("expected Double") };
        assert!((v - 3.14).abs() < 1e-9);
    }

    #[test]
    fn anyvalue_type4_int() {
        let data = concat(&[encode_fixpos(4), encode_i64(-42)]);
        assert!(matches!(decode_av(&data), RawAnyValue::Int(-42)));
    }

    #[test]
    fn anyvalue_type5_bytes() {
        let data = concat(&[encode_fixpos(5), encode_bin8(&[0xde, 0xad, 0xbe, 0xef])]);
        let RawAnyValue::Bytes(b) = decode_av(&data) else { panic!("expected Bytes") };
        assert_eq!(b, &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn anyvalue_type6_array() {
        let data = concat(&[
            encode_fixpos(6),
            encode_fixarray_header(4),
            encode_fixpos(2), encode_bool(true),
            encode_fixpos(4), encode_fixpos(7),
        ]);
        let RawAnyValue::Array(arr) = decode_av(&data) else { panic!("expected Array") };
        assert_eq!(arr.len(), 2);
        assert!(matches!(arr[0], RawAnyValue::Bool(true)));
        assert!(matches!(arr[1], RawAnyValue::Int(7)));
    }

    #[test]
    fn anyvalue_type6_odd_element_count_is_error() {
        let data = concat(&[
            encode_fixpos(6),
            encode_fixarray_header(3),
            encode_fixpos(2), encode_bool(true),
            encode_fixpos(4),
        ]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let err = decode_any_value(&mut rd, &mut table).unwrap_err();
        assert!(matches!(err, DeserializeError::InvalidArrayElementCount(3)));
    }

    #[test]
    fn anyvalue_type7_kvlist() {
        let data = concat(&[
            encode_fixpos(7),
            encode_fixarray_header(3),
            encode_fixstr("k"),
            encode_fixpos(2), encode_bool(true),
        ]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let RawAnyValue::KeyValueList(kvl) = decode_any_value(&mut rd, &mut table).unwrap()
        else {
            panic!("expected KeyValueList")
        };
        assert_eq!(kvl.len(), 1);
        assert_eq!(table.get(kvl[0].key), Some("k"));
        assert!(matches!(kvl[0].value, RawAnyValue::Bool(true)));
    }

    #[test]
    fn anyvalue_unknown_type_tag_is_error() {
        let data = concat(&[encode_fixpos(99)]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let err = decode_any_value(&mut rd, &mut table).unwrap_err();
        assert!(matches!(err, DeserializeError::UnknownAnyValueType(99)));
    }

    // ── Attribute array ─────────────────────────────────────────────────────

    #[test]
    fn attributes_empty_array() {
        let data = encode_fixarray_header(0);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let attrs = decode_attributes(&mut rd, &mut table).unwrap();
        assert!(attrs.is_empty());
    }

    #[test]
    fn attributes_multiple_mixed_types() {
        let data = concat(&[
            encode_fixarray_header(6),
            encode_fixstr("k1"), encode_fixpos(2), encode_bool(true),
            encode_fixstr("k2"), encode_fixpos(4), encode_fixpos(99),
        ]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let attrs = decode_attributes(&mut rd, &mut table).unwrap();
        assert_eq!(attrs.len(), 2);
        assert_eq!(table.get(attrs[0].key), Some("k1"));
        assert!(matches!(attrs[0].value, RawAnyValue::Bool(true)));
        assert_eq!(table.get(attrs[1].key), Some("k2"));
        assert!(matches!(attrs[1].value, RawAnyValue::Int(99)));
    }

    #[test]
    fn attributes_non_multiple_of_three_is_error() {
        let data = encode_fixarray_header(4);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let err = decode_attributes(&mut rd, &mut table).unwrap_err();
        assert!(matches!(err, DeserializeError::InvalidAttributeCount(4)));
    }

    // ── Span decoding ───────────────────────────────────────────────────────

    #[test]
    fn span_all_fields_round_trip() {
        let data = concat(&[
            encode_map16_header(16),
            encode_fixpos(1), encode_fixstr("my-svc"),
            encode_fixpos(2), encode_fixstr("http.request"),
            encode_fixpos(3), encode_fixstr("/api/v1"),
            encode_fixpos(4), encode_u64(0xdeadbeef_cafebabe),
            encode_fixpos(5), encode_u64(0x0102030405060708),
            encode_fixpos(6), encode_u64(1_700_000_000_000_000_000),
            encode_fixpos(7), encode_u64(500_000),
            encode_fixpos(8), encode_bool(true),
            encode_fixpos(9), encode_fixarray_header(0),
            encode_fixpos(10), encode_fixstr("web"),
            encode_fixpos(11), encode_fixarray_header(0),
            encode_fixpos(12), encode_fixarray_header(0),
            encode_fixpos(13), encode_fixstr("prod"),
            encode_fixpos(14), encode_fixstr("1.0.0"),
            encode_fixpos(15), encode_fixstr("net/http"),
            encode_fixpos(16), encode_fixpos(1),
        ]);

        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let span = decode_span(&mut rd, &mut table).unwrap();

        assert_eq!(table.get(span.service), Some("my-svc"));
        assert_eq!(table.get(span.name), Some("http.request"));
        assert_eq!(table.get(span.resource), Some("/api/v1"));
        assert_eq!(span.span_id, 0xdeadbeef_cafebabe);
        assert_eq!(span.parent_id, 0x0102030405060708);
        assert_eq!(span.start, 1_700_000_000_000_000_000);
        assert_eq!(span.duration, 500_000);
        assert!(span.error);
        assert_eq!(table.get(span.span_type), Some("web"));
        assert_eq!(table.get(span.env), Some("prod"));
        assert_eq!(table.get(span.version), Some("1.0.0"));
        assert_eq!(table.get(span.component), Some("net/http"));
        assert_eq!(span.kind, 1);
        assert!(span.links.is_empty());
        assert!(span.events.is_empty());
    }

    #[test]
    fn span_unknown_field_is_skipped() {
        let data = concat(&[
            encode_fixmap_header(2),
            encode_fixpos(4), encode_u64(42),
            encode_fixpos(99), encode_nil(),
        ]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let span = decode_span(&mut rd, &mut table).unwrap();
        assert_eq!(span.span_id, 42);
    }

    #[test]
    fn chunk_trace_id_splits_into_high_low() {
        let trace_id_high: u64 = 0xaaaaaaaaaaaaaaaa;
        let trace_id_low: u64 = 0xbbbbbbbbbbbbbbbb;
        let data = concat(&[
            encode_fixmap_header(1),
            encode_fixpos(6), encode_trace_id(trace_id_high, trace_id_low),
        ]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let chunk = decode_chunk(&mut rd, &mut table).unwrap();
        assert_eq!(chunk.trace_id_high, trace_id_high);
        assert_eq!(chunk.trace_id_low, trace_id_low);
    }

    #[test]
    fn chunk_priority_negative() {
        let data = concat(&[encode_fixmap_header(1), encode_fixpos(1), encode_i32(-1)]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let chunk = decode_chunk(&mut rd, &mut table).unwrap();
        assert_eq!(chunk.priority, -1);
    }

    #[test]
    fn chunk_dropped_trace_bool() {
        let data = concat(&[encode_fixmap_header(1), encode_fixpos(5), encode_bool(true)]);
        let mut table = StringTable::new();
        let mut rd = data.as_slice();
        let chunk = decode_chunk(&mut rd, &mut table).unwrap();
        assert!(chunk.dropped_trace);
    }

    // ── TracerPayload ───────────────────────────────────────────────────────

    #[test]
    fn payload_empty_map_decodes_without_error() {
        let data = encode_fixmap_header(0);
        let mut rd = data.as_slice();
        let payload = decode_tracer_payload(&mut rd).unwrap();
        assert!(payload.chunks.is_empty());
    }

    #[test]
    fn payload_all_string_fields() {
        let data = concat(&[
            encode_fixmap_header(8),
            encode_fixpos(2), encode_fixstr("ctr-123"),
            encode_fixpos(3), encode_fixstr("python"),
            encode_fixpos(4), encode_fixstr("3.11"),
            encode_fixpos(5), encode_fixstr("ddtrace-1.0"),
            encode_fixpos(6), encode_fixstr("runtime-abc"),
            encode_fixpos(7), encode_fixstr("staging"),
            encode_fixpos(8), encode_fixstr("host-1"),
            encode_fixpos(9), encode_fixstr("v2"),
        ]);
        let mut rd = data.as_slice();
        let p = decode_tracer_payload(&mut rd).unwrap();

        assert_eq!(p.string_table.get(p.container_id), Some("ctr-123"));
        assert_eq!(p.string_table.get(p.language_name), Some("python"));
        assert_eq!(p.string_table.get(p.language_version), Some("3.11"));
        assert_eq!(p.string_table.get(p.tracer_version), Some("ddtrace-1.0"));
        assert_eq!(p.string_table.get(p.runtime_id), Some("runtime-abc"));
        assert_eq!(p.string_table.get(p.env), Some("staging"));
        assert_eq!(p.string_table.get(p.hostname), Some("host-1"));
        assert_eq!(p.string_table.get(p.app_version), Some("v2"));
    }

    #[test]
    fn payload_multiple_chunks() {
        let chunk_data = encode_fixmap_header(0);
        let data = concat(&[
            encode_fixmap_header(1),
            encode_fixpos(11),
            concat(&[encode_fixarray_header(2), chunk_data.clone(), chunk_data]),
        ]);
        let mut rd = data.as_slice();
        let payload = decode_tracer_payload(&mut rd).unwrap();
        assert_eq!(payload.chunks.len(), 2);
    }

    // ── Error / structural cases ────────────────────────────────────────────

    #[test]
    fn empty_slice_is_error() {
        let data: &[u8] = &[];
        let mut rd = data;
        let err = decode_tracer_payload(&mut rd).unwrap_err();
        assert!(matches!(err, DeserializeError::UnexpectedEof));
    }

    #[test]
    fn truncated_input_is_error() {
        let data = vec![0x81];
        let mut rd = data.as_slice();
        let err = decode_tracer_payload(&mut rd).unwrap_err();
        assert!(matches!(err, DeserializeError::UnexpectedEof));
    }

    #[test]
    fn wrong_type_for_map_header_is_error() {
        let data = encode_fixstr("oops");
        let mut rd = data.as_slice();
        let err = decode_tracer_payload(&mut rd).unwrap_err();
        assert!(matches!(err, DeserializeError::UnexpectedMarker(_)));
    }

    #[test]
    fn attribute_count_exceeds_limit_is_error() {
        let count = (MAX_SIZE + 1) as u32;
        let mut b = vec![0xdd];
        b.extend_from_slice(&count.to_be_bytes());
        let mut table = StringTable::new();
        let mut rd = b.as_slice();
        let err = decode_attributes(&mut rd, &mut table).unwrap_err();
        assert!(matches!(err, DeserializeError::LimitExceeded(_)));
    }

    // ── skip_msgpack_value ──────────────────────────────────────────────────

    #[test]
    fn skip_nil() {
        let data = encode_nil();
        let mut rd = data.as_slice();
        skip_msgpack_value(&mut rd).unwrap();
        assert!(rd.is_empty());
    }

    #[test]
    fn skip_bool() {
        for b in [true, false] {
            let data = encode_bool(b);
            let mut rd = data.as_slice();
            skip_msgpack_value(&mut rd).unwrap();
            assert!(rd.is_empty());
        }
    }

    #[test]
    fn skip_int_variants() {
        for data in [
            vec![0x05],
            encode_u8(200),
            encode_i32(-1),
            encode_u64(u64::MAX),
        ] {
            let mut rd = data.as_slice();
            skip_msgpack_value(&mut rd).unwrap();
            assert!(rd.is_empty());
        }
    }

    #[test]
    fn skip_str() {
        let data = encode_fixstr("hello");
        let mut rd = data.as_slice();
        skip_msgpack_value(&mut rd).unwrap();
        assert!(rd.is_empty());
    }

    #[test]
    fn skip_bin() {
        let data = encode_bin8(&[1, 2, 3, 4]);
        let mut rd = data.as_slice();
        skip_msgpack_value(&mut rd).unwrap();
        assert!(rd.is_empty());
    }

    #[test]
    fn skip_nested_array() {
        let data = concat(&[encode_fixarray_header(3), encode_nil(), encode_nil(), encode_nil()]);
        let mut rd = data.as_slice();
        skip_msgpack_value(&mut rd).unwrap();
        assert!(rd.is_empty());
    }

    #[test]
    fn skip_nested_map() {
        let data = concat(&[encode_fixmap_header(1), encode_fixpos(1), encode_nil()]);
        let mut rd = data.as_slice();
        skip_msgpack_value(&mut rd).unwrap();
        assert!(rd.is_empty());
    }

    #[test]
    fn skip_deeply_nested() {
        let inner1 = concat(&[encode_fixarray_header(2), encode_nil(), encode_nil()]);
        let inner2 = concat(&[encode_fixarray_header(1), encode_nil()]);
        let data = concat(&[encode_fixarray_header(2), inner1, inner2]);
        let mut rd = data.as_slice();
        skip_msgpack_value(&mut rd).unwrap();
        assert!(rd.is_empty());
    }

    // ── Realistic golden-input test ─────────────────────────────────────────

    fn test_payload() -> Vec<u8> {
        let strings_arr = concat(&[
            encode_fixarray_header(10),
            encode_fixstr("my-service"),
            encode_fixstr("http.get"),
            encode_fixstr("/users/{id}"),
            encode_fixstr("web"),
            encode_fixstr("prod"),
            encode_fixstr("host-1"),
            encode_fixstr("v1"),
            encode_fixstr("component"),
            encode_fixstr("attr-key"),
            encode_fixstr("staging"),
        ]);

        let simple_span = |env_idx: u8| {
            concat(&[
                encode_fixmap_header(8),
                encode_fixpos(1), encode_fixpos(1_u8),
                encode_fixpos(2), encode_fixpos(2_u8),
                encode_fixpos(3), encode_fixpos(3_u8),
                encode_fixpos(4), encode_u64(0xaaaa_0000_0000_0001),
                encode_fixpos(7), encode_u64(100_000_u64),
                encode_fixpos(8), encode_bool(false),
                encode_fixpos(9), encode_fixarray_header(0),
                encode_fixpos(13), encode_fixpos(env_idx),
            ])
        };

        let rich_span = concat(&[
            encode_fixmap_header(4),
            encode_fixpos(1), encode_fixpos(1_u8),
            encode_fixpos(2), encode_fixpos(2_u8),
            encode_fixpos(4), encode_u64(0xbbbb_0000_0000_0002),
            encode_fixpos(9),
            concat(&[
                encode_array16_header(21),
                encode_fixpos(9), encode_fixpos(1), encode_fixpos(4),
                encode_fixpos(9), encode_fixpos(2), encode_bool(true),
                encode_fixpos(9), encode_fixpos(3), encode_f64(1.5),
                encode_fixpos(9), encode_fixpos(4), encode_i64(-1),
                encode_fixpos(9), encode_fixpos(5), encode_bin8(&[0xab]),
                encode_fixpos(9), encode_fixpos(6),
                concat(&[
                    encode_fixarray_header(4),
                    encode_fixpos(2), encode_bool(false),
                    encode_fixpos(4), encode_fixpos(0),
                ]),
                encode_fixpos(9), encode_fixpos(7),
                concat(&[
                    encode_fixarray_header(3),
                    encode_fixpos(9), encode_fixpos(2), encode_bool(true),
                ]),
            ]),
        ]);

        let linked_span = concat(&[
            encode_fixmap_header(4),
            encode_fixpos(1), encode_fixpos(1_u8),
            encode_fixpos(4), encode_u64(0xcccc_0000_0000_0003),
            encode_fixpos(11),
            concat(&[
                encode_fixarray_header(1),
                concat(&[
                    encode_fixmap_header(3),
                    encode_fixpos(1), encode_trace_id(0x1234, 0x5678),
                    encode_fixpos(2), encode_u64(0xdeadbeef),
                    encode_fixpos(5), encode_fixpos(1),
                ]),
            ]),
            encode_fixpos(12),
            concat(&[
                encode_fixarray_header(1),
                concat(&[
                    encode_fixmap_header(2),
                    encode_fixpos(1), encode_u64(999_999_999_u64),
                    encode_fixpos(2), encode_fixpos(2_u8),
                ]),
            ]),
        ]);

        let chunk1 = concat(&[
            encode_fixmap_header(4),
            encode_fixpos(1), encode_i32(1),
            encode_fixpos(4),
            concat(&[encode_fixarray_header(3), simple_span(5), rich_span, linked_span]),
            encode_fixpos(5), encode_bool(false),
            encode_fixpos(6), encode_trace_id(0xfeed_face_dead_beef, 0xcafe_babe_1234_5678),
        ]);

        let chunk2 = concat(&[
            encode_fixmap_header(3),
            encode_fixpos(1), encode_i32(-1),
            encode_fixpos(4),
            concat(&[
                encode_fixarray_header(3),
                simple_span(10),
                simple_span(10),
                simple_span(10),
            ]),
            encode_fixpos(5), encode_bool(true),
        ]);

        concat(&[
            encode_fixmap_header(3),
            encode_fixpos(1), strings_arr,
            encode_fixpos(8), encode_fixpos(6_u8),
            encode_fixpos(11),
            concat(&[encode_fixarray_header(2), chunk1, chunk2]),
        ])
    }

    #[test]
    fn golden_payload_decodes_end_to_end() {
        let data = test_payload();
        let mut rd = data.as_slice();
        let payload = decode_tracer_payload(&mut rd).unwrap();

        assert_eq!(rd.len(), 0, "all bytes should be consumed");
        assert_eq!(payload.string_table.get(1), Some("my-service"));
        assert_eq!(payload.string_table.get(6), Some("host-1"));
        assert_eq!(payload.string_table.get(payload.hostname), Some("host-1"));
        assert_eq!(payload.chunks.len(), 2);

        let c0 = &payload.chunks[0];
        assert_eq!(c0.priority, 1);
        assert!(!c0.dropped_trace);
        assert_eq!(c0.trace_id_high, 0xfeed_face_dead_beef);
        assert_eq!(c0.trace_id_low, 0xcafe_babe_1234_5678);
        assert_eq!(c0.spans.len(), 3);

        let rich = &c0.spans[1];
        assert_eq!(rich.attributes.len(), 7);
        assert!(matches!(rich.attributes[0].value, RawAnyValue::String(_)));
        assert!(matches!(rich.attributes[1].value, RawAnyValue::Bool(true)));
        assert!(matches!(rich.attributes[2].value, RawAnyValue::Double(_)));
        assert!(matches!(rich.attributes[3].value, RawAnyValue::Int(-1)));
        assert!(matches!(rich.attributes[4].value, RawAnyValue::Bytes(_)));
        assert!(matches!(rich.attributes[5].value, RawAnyValue::Array(_)));
        assert!(matches!(rich.attributes[6].value, RawAnyValue::KeyValueList(_)));

        let linked = &c0.spans[2];
        assert_eq!(linked.links.len(), 1);
        assert_eq!(linked.links[0].trace_id_high, 0x1234);
        assert_eq!(linked.links[0].trace_id_low, 0x5678);
        assert_eq!(linked.links[0].span_id, 0xdeadbeef);
        assert_eq!(linked.events.len(), 1);
        assert_eq!(linked.events[0].time_unix_nano, 999_999_999);

        let c1 = &payload.chunks[1];
        assert_eq!(c1.priority, -1);
        assert!(c1.dropped_trace);
        assert_eq!(c1.spans.len(), 3);
    }

    fn test_payload_streaming() -> Vec<u8> {
        let span = |first: bool| {
            if first {
                concat(&[
                    encode_fixmap_header(7),
                    encode_fixpos(span::FIELD_SERVICE as u8),    encode_fixstr("my-service"),
                    encode_fixpos(span::FIELD_NAME as u8),       encode_fixstr("http.get"),
                    encode_fixpos(span::FIELD_RESOURCE as u8),   encode_fixstr("/users/{id}"),
                    encode_fixpos(span::FIELD_SPAN_ID as u8),    encode_u64(0x0000_0001),
                    encode_fixpos(span::FIELD_ATTRIBUTES as u8), encode_fixarray_header(0),
                    encode_fixpos(span::FIELD_TYPE as u8),       encode_fixstr("web"),
                    encode_fixpos(span::FIELD_ENV as u8),        encode_fixstr("prod"),
                ])
            } else {
                concat(&[
                    encode_fixmap_header(7),
                    encode_fixpos(span::FIELD_SERVICE as u8),    encode_fixpos(2),
                    encode_fixpos(span::FIELD_NAME as u8),       encode_fixpos(3),
                    encode_fixpos(span::FIELD_RESOURCE as u8),   encode_fixpos(4),
                    encode_fixpos(span::FIELD_SPAN_ID as u8),    encode_u64(0x0000_0002),
                    encode_fixpos(span::FIELD_ATTRIBUTES as u8), encode_fixarray_header(0),
                    encode_fixpos(span::FIELD_TYPE as u8),       encode_fixpos(5),
                    encode_fixpos(span::FIELD_ENV as u8),        encode_fixpos(6),
                ])
            }
        };

        let chunk = concat(&[
            encode_fixmap_header(2),
            encode_fixpos(trace_chunk::FIELD_PRIORITY as u8), encode_i32(1),
            encode_fixpos(trace_chunk::FIELD_SPANS as u8),
            concat(&[encode_fixarray_header(3), span(true), span(false), span(false)]),
        ]);

        concat(&[
            encode_fixmap_header(2),
            encode_fixpos(tracer_payload::FIELD_HOSTNAME as u8), encode_fixstr("host-1"),
            encode_fixpos(tracer_payload::FIELD_CHUNKS as u8),
            concat(&[encode_fixarray_header(1), chunk]),
        ])
    }

    #[test]
    fn golden_streaming_payload_decodes_end_to_end() {
        let data = test_payload_streaming();
        let mut rd = data.as_slice();
        let payload = decode_tracer_payload(&mut rd).unwrap();

        assert_eq!(rd.len(), 0, "all bytes should be consumed");
        assert_eq!(payload.string_table.get(payload.hostname), Some("host-1"));

        assert_eq!(payload.chunks.len(), 1);
        let chunk = &payload.chunks[0];
        assert_eq!(chunk.priority, 1);
        assert_eq!(chunk.spans.len(), 3);

        for span in &chunk.spans {
            assert_eq!(payload.string_table.get(span.service), Some("my-service"));
            assert_eq!(payload.string_table.get(span.name), Some("http.get"));
            assert_eq!(payload.string_table.get(span.resource), Some("/users/{id}"));
            assert_eq!(payload.string_table.get(span.span_type), Some("web"));
            assert_eq!(payload.string_table.get(span.env), Some("prod"));
        }
    }
}
