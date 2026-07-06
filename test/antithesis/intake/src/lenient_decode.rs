//! UTF-8-tolerant `/api/v2/series` `MetricPayload` decode.
//!
//! rust-protobuf validates UTF-8 on `string` fields and rejects the entire payload on
//! any non-UTF-8 byte. The Datadog Agent forwards feral non-UTF-8 `DogStatsD` names and
//! tags into those fields unchecked, so a strict `MetricPayload::parse_from_bytes` drops
//! every legitimate agent-lane payload that carries one bad byte, along with all the
//! contexts it holds.
//!
//! This mirrors the production intake, which retries a failed strict decode into a
//! tags-as-`bytes` message and keeps the payload. So only the `tags` field tolerates
//! non-UTF-8 here, lossy-converted. Every other string field stays UTF-8-strict, so a
//! non-UTF-8 metric name still drops the payload exactly as production does. Genuinely
//! malformed wire also fails.

use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, Resource};
use datadog_protos::metrics::{Metadata, MetricPayload};
use protobuf::rt::WireType;
use protobuf::{CodedInputStream, EnumOrUnknown, Message, MessageField};

/// Why the intake did not decode a `/api/v2/series` body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Rejection {
    /// A non-tag string field held non-UTF-8 bytes. Production rejects the payload too, so this
    /// is expected behavior on feral input, not a decode defect.
    NonUtf8StrictField,
    /// The bytes are not a well-formed protobuf message. No real producer should emit this.
    MalformedWire,
}

impl From<protobuf::Error> for Rejection {
    fn from(_: protobuf::Error) -> Self {
        Rejection::MalformedWire
    }
}

/// Replace each contiguous run of invalid UTF-8 bytes with a single U+FFFD, matching Go's
/// `strings.ToValidUTF8` as production's `sanitizePayload` applies it to Agent tags. Rust's
/// `from_utf8_lossy` emits one replacement per maximal subpart, which yields a different tag
/// string for a multi-byte invalid run and so a false divergence. Only tags use this.
fn to_valid_utf8(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut prev_invalid = false;
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        if !valid.is_empty() {
            out.push_str(valid);
            prev_invalid = false;
        }
        if !chunk.invalid().is_empty() {
            if !prev_invalid {
                out.push('\u{FFFD}');
            }
            prev_invalid = true;
        }
    }
    out
}

/// Strict UTF-8. Production keeps every non-tag string field UTF-8-validated, so a
/// non-UTF-8 byte here drops the payload as it would in production.
fn strict(bytes: Vec<u8>) -> Result<String, Rejection> {
    String::from_utf8(bytes).map_err(|_| Rejection::NonUtf8StrictField)
}

fn unpack(tag: u32) -> Result<(u32, WireType), Rejection> {
    let wire = WireType::new(tag & 0x7).ok_or(Rejection::MalformedWire)?;
    let field = tag >> 3;
    if field == 0 {
        return Err(Rejection::MalformedWire);
    }
    Ok((field, wire))
}

/// Decode a `/api/v2/series` body into a `MetricPayload`. Only `tags` tolerate non-UTF-8,
/// lossy-converted. A non-UTF-8 non-tag string field returns `NonUtf8StrictField`, matching
/// production's reject. Structurally malformed protobuf returns `MalformedWire`.
pub(crate) fn decode_metric_payload(body: &[u8]) -> Result<MetricPayload, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut payload = MetricPayload::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => payload.series.push(decode_series(&is.read_bytes()?)?),
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(payload)
}

fn decode_series(body: &[u8]) -> Result<MetricSeries, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut series = MetricSeries::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => series.resources.push(decode_resource(&is.read_bytes()?)?),
            (2, WireType::LengthDelimited) => series.metric = strict(is.read_bytes()?)?,
            (3, WireType::LengthDelimited) => series.tags.push(to_valid_utf8(&is.read_bytes()?)),
            (4, WireType::LengthDelimited) => series.points.push(decode_point(&is.read_bytes()?)?),
            (5, WireType::Varint) => series.type_ = EnumOrUnknown::from_i32(is.read_int32()?),
            (6, WireType::LengthDelimited) => series.unit = strict(is.read_bytes()?)?,
            (7, WireType::LengthDelimited) => series.source_type_name = strict(is.read_bytes()?)?,
            (8, WireType::Varint) => series.interval = is.read_int64()?,
            // Origin metadata. It carries only numeric fields, so UTF-8 leniency does not apply
            // and a strict sub-message parse stays production-faithful. Dropping it would leave
            // series.metadata unset and make the Pyld16 origin check vacuous.
            (9, WireType::LengthDelimited) => {
                series.metadata = MessageField::some(Metadata::parse_from_bytes(&is.read_bytes()?)?);
            }
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(series)
}

fn decode_resource(body: &[u8]) -> Result<Resource, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut resource = Resource::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => resource.type_ = strict(is.read_bytes()?)?,
            (2, WireType::LengthDelimited) => resource.name = strict(is.read_bytes()?)?,
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(resource)
}

fn decode_point(body: &[u8]) -> Result<MetricPoint, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut point = MetricPoint::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::Fixed64) => point.value = is.read_double()?,
            (2, WireType::Varint) => point.timestamp = is.read_int64()?,
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(point)
}

#[cfg(test)]
mod tests {
    use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, MetricType, Resource};
    use datadog_protos::metrics::{Metadata, MetricPayload, Origin};
    use proptest::prelude::*;
    use protobuf::{EnumOrUnknown, Message, MessageField};

    use super::{decode_metric_payload, to_valid_utf8, Rejection};

    // Non-UTF-8 in a non-tag field is the rejection production also makes, not a decode defect.
    #[test]
    fn non_utf8_name_is_a_production_faithful_rejection() {
        let mut payload = MetricPayload::new();
        let mut series = MetricSeries::new();
        series.set_metric("NAME".into());
        series.set_type(MetricType::COUNT);
        let mut point = MetricPoint::new();
        point.value = 1.0;
        point.timestamp = 1;
        series.points.push(point);
        payload.series.push(series);
        let mut bytes = payload.write_to_bytes().expect("serialize");
        let pos = bytes.windows(4).position(|w| w == b"NAME").expect("marker");
        for b in &mut bytes[pos..pos + 4] {
            *b = 0xFF;
        }
        assert_eq!(decode_metric_payload(&bytes), Err(Rejection::NonUtf8StrictField));
    }

    // Truncated wire (field 1, LEN, incomplete length varint) is genuinely malformed.
    #[test]
    fn malformed_wire_is_rejected() {
        assert_eq!(decode_metric_payload(&[0x0a, 0xff]), Err(Rejection::MalformedWire));
    }

    // A repeated singular message field (metadata, field 9) must decode exactly as the strict
    // parser does. rust-protobuf takes the last occurrence rather than deep-merging split origin
    // metadata, so replacing series.metadata per occurrence stays production-faithful. First
    // field 9 sets origin_product=7, second sets origin_service=9; both paths keep only the last.
    #[test]
    fn repeated_metadata_matches_strict() {
        let payload = [
            0x0a, 0x0c, // series, 12-byte body
            0x4a, 0x04, 0x0a, 0x02, 0x20, 0x07, // field 9 -> Metadata{origin{origin_product=7}}
            0x4a, 0x04, 0x0a, 0x02, 0x30, 0x09, // field 9 -> Metadata{origin{origin_service=9}}
        ];
        let strict = MetricPayload::parse_from_bytes(&payload).expect("strict");
        let lenient = decode_metric_payload(&payload).expect("lenient");
        assert_eq!(lenient, strict);
    }

    // Tag byte 0x00 is field 0, wire Varint. Field 0 is invalid protobuf, rejected like strict.
    #[test]
    fn field_zero_tag_is_malformed_wire() {
        assert_eq!(decode_metric_payload(&[0x00, 0x01]), Err(Rejection::MalformedWire));
        // The contrast that keeps the field-0 rejection honest: strict rejects field 0 outright.
        assert!(MetricPayload::parse_from_bytes(&[0x00, 0x01]).is_err());
    }

    // A schema-known field carrying the wrong wire type is not malformed wire. rust-protobuf, and
    // so production's strict decode, routes it to unknown fields and returns Ok, so the lenient
    // walker skipping it stays production-faithful. Only unknown field numbers and invalid field
    // number 0 differ from a plain skip.
    #[test]
    fn wrong_wire_type_on_known_field_is_accepted_like_strict() {
        // Top-level field 1 (series, expects length-delimited) encoded as a varint.
        let top = [0x08, 0x01];
        assert!(MetricPayload::parse_from_bytes(&top).is_ok());
        assert!(decode_metric_payload(&top).is_ok());

        // Series (field 1, length-delimited, 2-byte body) whose field 2 (metric name, expects
        // length-delimited) is encoded as a varint.
        let framed = [0x0a, 0x02, 0x10, 0x01];
        assert!(MetricPayload::parse_from_bytes(&framed).is_ok());
        assert!(decode_metric_payload(&framed).is_ok());
    }

    // Must match Go strings.ToValidUTF8: each contiguous invalid run collapses to one U+FFFD,
    // unlike from_utf8_lossy which emits one per maximal subpart.
    #[test]
    fn to_valid_utf8_collapses_invalid_runs() {
        assert_eq!(to_valid_utf8(b"ok"), "ok");
        assert_eq!(to_valid_utf8(b"a\xff\xfeb"), "a\u{FFFD}b");
        assert_eq!(to_valid_utf8(b"\xff\xff\xff"), "\u{FFFD}");
        assert_eq!(to_valid_utf8(b"a\xffb\xffc"), "a\u{FFFD}b\u{FFFD}c");
        assert_eq!(to_valid_utf8(b"caf\xc3\xa9"), "café");
    }

    // Guards against wire-layout drift: for valid UTF-8 the lenient walker must recover
    // exactly what the strict parser does, or a renumbered field silently corrupts contexts.
    #[test]
    fn lenient_decode_matches_strict_for_valid_utf8() {
        let mut payload = MetricPayload::new();
        let mut series = MetricSeries::new();
        series.set_metric("adp.requests".into());
        series.set_type(MetricType::COUNT);
        series.tags.push("env:prod".into());
        series.tags.push("host:antithesis-differential".into());
        series.unit = "request".into();
        series.source_type_name = "dogstatsd".into();
        series.interval = 10;
        let mut origin = Origin::new();
        origin.origin_product = 10;
        origin.origin_category = 11;
        origin.origin_service = 12;
        let mut metadata = Metadata::new();
        metadata.origin = MessageField::some(origin);
        series.metadata = MessageField::some(metadata);
        let mut host = Resource::new();
        host.set_type("host".into());
        host.set_name("server-1".into());
        series.resources.push(host);
        let mut point = MetricPoint::new();
        point.value = 2.5;
        point.timestamp = 1_600_000_000;
        series.points.push(point);
        payload.series.push(series);

        let bytes = payload.write_to_bytes().expect("serialize");
        let strict = MetricPayload::parse_from_bytes(&bytes).expect("strict parse");
        let lenient = decode_metric_payload(&bytes).expect("lenient decode");
        assert_eq!(lenient, strict);
    }

    // Frame `bytes` as one length-delimited protobuf field. Bodies stay under 128 bytes so the
    // length is a single varint byte, which keeps the hand-built wire in these properties simple.
    fn framed(field: u8, bytes: &[u8]) -> Vec<u8> {
        let len = u8::try_from(bytes.len()).expect("field body under 128 bytes");
        assert!(len < 0x80, "field body under 128 bytes");
        let mut out = vec![(field << 3) | 2, len];
        out.extend_from_slice(bytes);
        out
    }

    // A `MetricSeries` with valid UTF-8 strings and finite point values. Finite keeps NaN out of
    // the round-trip equality; leniency on non-UTF-8 tags is exercised separately over raw wire.
    fn arb_series() -> impl Strategy<Value = MetricSeries> {
        (
            prop::collection::vec(".{0,8}", 0..3),
            ".{0,8}",
            ".{0,8}",
            ".{0,8}",
            prop::collection::vec((-1e9f64..1e9f64, any::<i64>()), 0..4),
            0i32..4,
            any::<i64>(),
            prop::option::of((any::<u32>(), any::<u32>(), any::<u32>())),
            prop::collection::vec((".{0,8}", ".{0,8}"), 0..2),
        )
            .prop_map(
                |(tags, metric, unit, source_type_name, points, ty, interval, origin, resources)| {
                    let mut series = MetricSeries::new();
                    for tag in tags {
                        series.tags.push(tag);
                    }
                    series.metric = metric;
                    series.unit = unit;
                    series.source_type_name = source_type_name;
                    for (value, timestamp) in points {
                        let mut point = MetricPoint::new();
                        point.value = value;
                        point.timestamp = timestamp;
                        series.points.push(point);
                    }
                    series.type_ = EnumOrUnknown::from_i32(ty);
                    series.interval = interval;
                    if let Some((product, category, service)) = origin {
                        let mut o = Origin::new();
                        o.origin_product = product;
                        o.origin_category = category;
                        o.origin_service = service;
                        let mut metadata = Metadata::new();
                        metadata.origin = MessageField::some(o);
                        series.metadata = MessageField::some(metadata);
                    }
                    for (type_, name) in resources {
                        let mut resource = Resource::new();
                        resource.type_ = type_;
                        resource.name = name;
                        series.resources.push(resource);
                    }
                    series
                },
            )
    }

    proptest! {
        // The whole point of the lenient walker: for any valid UTF-8 payload it must recover
        // exactly what the strict parser does. A renumbered or mishandled field would silently
        // corrupt contexts, and this catches it across the full message shape rather than one case.
        #[test]
        fn prop_lenient_matches_strict_for_valid_payloads(series in prop::collection::vec(arb_series(), 0..4)) {
            let mut payload = MetricPayload::new();
            for s in series {
                payload.series.push(s);
            }
            let bytes = payload.write_to_bytes().expect("serialize");
            let strict = MetricPayload::parse_from_bytes(&bytes).expect("strict parse");
            let lenient = decode_metric_payload(&bytes).expect("lenient decode");
            prop_assert_eq!(lenient, strict);
        }

        // The intake faces feral traffic, so the walker must be total: any byte string decodes to
        // Ok or a Rejection, never a panic.
        #[test]
        fn prop_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_metric_payload(&bytes);
        }

        // Tags tolerate arbitrary bytes: a payload whose only non-UTF-8 lives in tags always decodes,
        // and each tag is sanitized exactly as to_valid_utf8 (Go's strings.ToValidUTF8) would.
        #[test]
        fn prop_non_utf8_tags_accepted_and_sanitized(
            metric in "[a-z]{1,6}",
            tags in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..12), 0..4),
        ) {
            let mut body = framed(2, metric.as_bytes());
            for tag in &tags {
                body.extend(framed(3, tag));
            }
            let decoded = decode_metric_payload(&framed(1, &body)).expect("tags never reject");
            let want: Vec<String> = tags.iter().map(|t| to_valid_utf8(t)).collect();
            prop_assert_eq!(&decoded.series[0].tags, &want);
            prop_assert_eq!(&decoded.series[0].metric, &metric);
        }

        // A non-UTF-8 byte in a strict string field (metric name) is the rejection production also
        // makes. The leading 0xFF guarantees invalid UTF-8 regardless of the generated suffix.
        #[test]
        fn prop_non_utf8_strict_field_rejected(suffix in prop::collection::vec(any::<u8>(), 0..8)) {
            let mut name = vec![0xff];
            name.extend_from_slice(&suffix);
            let body = framed(2, &name);
            prop_assert_eq!(
                decode_metric_payload(&framed(1, &body)),
                Err(Rejection::NonUtf8StrictField)
            );
        }
    }
}
