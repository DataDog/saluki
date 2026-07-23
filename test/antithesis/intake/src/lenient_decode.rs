//! UTF-8-tolerant `/api/v2/series` `MetricPayload` and `/api/beta/sketches` `SketchPayload` decode.
//!
//! rust-protobuf validates UTF-8 on `string` fields and rejects the entire payload on
//! any non-UTF-8 byte. The Datadog Agent forwards feral non-UTF-8 `DogStatsD` names and
//! tags into those fields unchecked, so a strict `parse_from_bytes` drops every legitimate
//! agent-lane payload that carries one bad byte, along with all the contexts it holds.
//!
//! v2 tag leniency is source-gated exactly as the production intake gates it. On a failed strict
//! `proto.Unmarshal` the backend retries into a tags-as-`bytes` message ONLY when the request's
//! `User-Agent` classifies as `SourceAgent` (`getUserAgent`, apps/propjoe/util.go:284-342, retry at
//! api_series_v2_handler.go:225-231). So a non-UTF-8 v2 `tags` value is lossy-converted to U+FFFD for
//! the `datadog-agent` source and whole-payload-rejected for every other source. Every non-tag v2
//! string field (metric name, unit, source-type, resource) stays UTF-8-strict for ALL sources, since
//! those fields remain `string` in the bytes-tag variant too, so a non-UTF-8 name drops the payload
//! regardless of source, exactly as production does.
//!
//! The `/api/beta/sketches` path is source-independent: the backend decodes it with a single
//! `proto.Unmarshal` and no bytes-tag fallback (common.go:447), so a non-UTF-8 byte in ANY sketch
//! string field, `tags` included, whole-payload-rejects. The v3 path is source-independent too and
//! lives in `decode_series_v3`. Genuinely malformed wire always fails.

use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, Resource};
use datadog_protos::metrics::sketch_payload::sketch::{Distribution, Dogsketch};
use datadog_protos::metrics::sketch_payload::Sketch;
use datadog_protos::metrics::v3::{MetricData as V3MetricData, Payload as V3Payload};
use datadog_protos::metrics::{CommonMetadata, Metadata, MetricPayload, SketchPayload};
use protobuf::rt::WireType;
use protobuf::{CodedInputStream, EnumOrUnknown, Message, MessageField};

use crate::capture::{
    metric_name_kept, BucketValue, MetricKind, SketchValue, MAX_HOST_NAME_LEN, MAX_RESOURCE_COUNT, MAX_TAG_COUNT,
};

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

/// The submitter source classified from the HTTP `User-Agent`, mirroring the backend's `getUserAgent`
/// (apps/propjoe/util.go:284-342). Only the `datadog-agent` case is load-bearing for v2 decode: the
/// backend retries a failed strict `proto.Unmarshal` into the tags-as-`bytes` variant ONLY for
/// `SourceAgent` (api_series_v2_handler.go:225-231), so a feral v2 tag is U+FFFD-sanitized for the
/// Agent and whole-payload-rejected for every other source. v3 and sketch decode never consult this.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    /// `User-Agent` lowercased begins `datadog-agent`. The one source whose v2 tags are sanitized.
    DatadogAgent,
    /// Any other `User-Agent`, present or absent (the backend's `SourceNone`/`SourceUnknown`/other
    /// categories). A non-UTF-8 v2 tag whole-payload-rejects, matching the skipped bytes-tag retry.
    Other,
}

impl Source {
    /// Classify the submitter from the `User-Agent` value, matching the backend's `SourceAgent` test
    /// (util.go:286-287): lowercase the header, then check the `datadog-agent` prefix. An absent header
    /// is `Other` (the backend's `SourceNone`). The prefix is ASCII, so ASCII lowercasing is faithful.
    pub(crate) fn from_user_agent(user_agent: Option<&str>) -> Self {
        match user_agent {
            Some(ua) if ua.to_ascii_lowercase().starts_with("datadog-agent") => Self::DatadogAgent,
            _ => Self::Other,
        }
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

/// Decode a `/api/v2/series` body into a `MetricPayload`. Tag leniency is source-gated: for
/// `Source::DatadogAgent` a non-UTF-8 `tags` value is lossy-converted, for `Source::Other` it returns
/// `NonUtf8StrictField` and drops the payload, matching the backend's `SourceAgent`-only bytes-tag
/// retry. A non-UTF-8 non-tag string field returns `NonUtf8StrictField` for every source, matching
/// production's reject. Structurally malformed protobuf returns `MalformedWire`.
pub(crate) fn decode_metric_payload(body: &[u8], source: Source) -> Result<MetricPayload, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut payload = MetricPayload::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => payload.series.push(decode_series(&is.read_bytes()?, source)?),
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(payload)
}

fn decode_series(body: &[u8], source: Source) -> Result<MetricSeries, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut series = MetricSeries::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => series.resources.push(decode_resource(&is.read_bytes()?)?),
            (2, WireType::LengthDelimited) => series.metric = strict(is.read_bytes()?)?,
            // Source-gated: the Agent's feral tag is sanitized; any other source rejects it, since the
            // backend runs its bytes-tag retry only for `SourceAgent`.
            (3, WireType::LengthDelimited) => {
                let bytes = is.read_bytes()?;
                let tag = match source {
                    Source::DatadogAgent => to_valid_utf8(&bytes),
                    Source::Other => strict(bytes)?,
                };
                series.tags.push(tag);
            }
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

/// Decode an `/api/beta/sketches` body into a `SketchPayload`. The sketch path is source-independent
/// and has NO bytes-tag retry: the backend decodes it with a single `proto.Unmarshal` (common.go:447),
/// so a non-UTF-8 byte in ANY string field, `tags` included, whole-payload-rejects with
/// `NonUtf8StrictField`, unlike the v2 `tags` field. The `distributions`, `dogsketches`, and metadata
/// sub-messages carry no feral strings, so a strict sub-message parse stays production-faithful.
pub(crate) fn decode_sketch_payload(body: &[u8]) -> Result<SketchPayload, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut payload = SketchPayload::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => payload.sketches.push(decode_sketch(&is.read_bytes()?)?),
            (2, WireType::LengthDelimited) => {
                payload.metadata = MessageField::some(CommonMetadata::parse_from_bytes(&is.read_bytes()?)?);
            }
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(payload)
}

fn decode_sketch(body: &[u8]) -> Result<Sketch, Rejection> {
    let mut is = CodedInputStream::from_bytes(body);
    let mut sketch = Sketch::new();
    while let Some(tag) = is.read_raw_tag_or_eof()? {
        match unpack(tag)? {
            (1, WireType::LengthDelimited) => sketch.metric = strict(is.read_bytes()?)?,
            (2, WireType::LengthDelimited) => sketch.host = strict(is.read_bytes()?)?,
            (3, WireType::LengthDelimited) => sketch
                .distributions
                .push(Distribution::parse_from_bytes(&is.read_bytes()?)?),
            // Sketch tags are UTF-8-strict: the backend has no bytes-tag retry on this path, so a feral
            // tag whole-payload-rejects like the metric/host fields do, source-independent.
            (4, WireType::LengthDelimited) => sketch.tags.push(strict(is.read_bytes()?)?),
            (7, WireType::LengthDelimited) => sketch.dogsketches.push(Dogsketch::parse_from_bytes(&is.read_bytes()?)?),
            (8, WireType::LengthDelimited) => {
                sketch.metadata = MessageField::some(Metadata::parse_from_bytes(&is.read_bytes()?)?);
            }
            (_, wire) => is.skip_field(wire)?,
        }
    }
    Ok(sketch)
}

/// Running cursors into the flat, cross-metric point columns, mirroring the production reader's
/// per-column state. They advance for every metric, since production drains a validation-dropped
/// series' points too.
#[derive(Default)]
struct PointCursors {
    timestamp: usize,
    f64: usize,
    f32: usize,
    sint: usize,
    numbins: usize,
    bins: usize,
}

/// Read one point's worth of the flat columns exactly as the production intake does, advancing `cur`
/// and materializing the `(wire bucket-start, value)` it carries, or `None` when a column it touches
/// underruns. A metric keeps its leading points up to the first `None`; the backend truncates the rest,
/// and `cur` still advances so a dropped series' points drain from the shared columns. `timestamps` is
/// already delta-decoded to absolute bucket-starts; `bin_keys` is delta-decoded per sketch in place.
#[allow(
    clippy::cast_precision_loss,
    reason = "sketch sum/min/max ride the wire as sint64 and are materialized as f64, matching the Agent's own lossy widening"
)]
fn read_point(
    data: &V3MetricData, is_sketch: bool, value_type: u64, timestamps: &[i64], bin_keys: &mut [i32],
    cur: &mut PointCursors,
) -> Option<(u64, BucketValue)> {
    let ts_idx = cur.timestamp;
    cur.timestamp += 1;
    if cur.timestamp > timestamps.len() {
        return None;
    }

    if is_sketch {
        let nb_idx = cur.numbins;
        cur.numbins += 1;
        if cur.numbins > data.sketchNumBins.len() {
            return None;
        }
        // A bin count too large for `usize` can only exceed the bin columns, so treat it as a truncated
        // point and drop the rest of this series, never the whole payload.
        let Ok(numbins) = usize::try_from(data.sketchNumBins[nb_idx]) else {
            return None;
        };
        let bins_start = cur.bins;
        cur.bins = cur.bins.saturating_add(numbins);
        let (f64_pre, f32_pre, sint_pre) = (cur.f64, cur.f32, cur.sint);
        // The Agent writes a sketch as sum, min, max in the value column, then count in sint64.
        match value_type {
            0x30 => {
                cur.f64 += 3;
                cur.sint += 1;
            }
            0x20 => {
                cur.f32 += 3;
                cur.sint += 1;
            }
            0x10 => cur.sint += 4,
            _ => cur.sint += 1,
        }
        if cur.f64 > data.valsFloat64.len()
            || cur.f32 > data.valsFloat32.len()
            || cur.sint > data.valsSint64.len()
            || cur.bins > data.sketchBinKeys.len()
            || cur.bins > data.sketchBinCnts.len()
        {
            return None;
        }
        let (sum, min, max, count) = match value_type {
            0x30 => (
                data.valsFloat64[f64_pre],
                data.valsFloat64[f64_pre + 1],
                data.valsFloat64[f64_pre + 2],
                data.valsSint64[sint_pre],
            ),
            0x20 => (
                f64::from(data.valsFloat32[f32_pre]),
                f64::from(data.valsFloat32[f32_pre + 1]),
                f64::from(data.valsFloat32[f32_pre + 2]),
                data.valsSint64[sint_pre],
            ),
            0x10 => (
                data.valsSint64[sint_pre] as f64,
                data.valsSint64[sint_pre + 1] as f64,
                data.valsSint64[sint_pre + 2] as f64,
                data.valsSint64[sint_pre + 3],
            ),
            _ => (0.0, 0.0, 0.0, data.valsSint64[sint_pre]),
        };
        let bins_end = bins_start + numbins;
        delta_decode_i32(&mut bin_keys[bins_start..bins_end]);
        let bins: Vec<(i32, u32)> = bin_keys[bins_start..bins_end]
            .iter()
            .copied()
            .zip(data.sketchBinCnts[bins_start..bins_end].iter().copied())
            .collect();
        let bucket_start = u64::try_from(timestamps[ts_idx]).ok()?;
        Some((
            bucket_start,
            BucketValue::Sketch(SketchValue {
                count,
                sum,
                min,
                max,
                bins,
            }),
        ))
    } else {
        let (f64_pre, f32_pre, sint_pre) = (cur.f64, cur.f32, cur.sint);
        match value_type {
            0x30 => cur.f64 += 1,
            0x20 => cur.f32 += 1,
            0x10 => cur.sint += 1,
            _ => {}
        }
        if cur.f64 > data.valsFloat64.len() || cur.f32 > data.valsFloat32.len() || cur.sint > data.valsSint64.len() {
            return None;
        }
        let value = match value_type {
            0x30 => data.valsFloat64[f64_pre],
            0x20 => f64::from(data.valsFloat32[f32_pre]),
            0x10 => data.valsSint64[sint_pre] as f64,
            _ => 0.0,
        };
        let bucket_start = u64::try_from(timestamps[ts_idx]).ok()?;
        Some((bucket_start, BucketValue::Scalar(value)))
    }
}

/// Materialize a metric's kept points, draining `cur` across the full `declared` count. Points after the
/// first column underrun truncate away, matching the backend; `cur` still advances so later metrics read
/// from the right offsets. The timestamp column is shared, so once it is spent no later point survives.
fn materialize_points(
    data: &V3MetricData, is_sketch: bool, value_type: u64, declared: u64, timestamps: &[i64], bin_keys: &mut [i32],
    cur: &mut PointCursors,
) -> Vec<(u64, BucketValue)> {
    let mut points = Vec::new();
    let mut truncated = false;
    for _ in 0..declared {
        match read_point(data, is_sketch, value_type, timestamps, bin_keys, cur) {
            Some(point) if !truncated => points.push(point),
            _ => truncated = true,
        }
        if cur.timestamp > timestamps.len() {
            break;
        }
    }
    points
}

/// Delta-decode a column in place: each entry becomes the running sum of itself and all before it. Go
/// int64 addition wraps, so `wrapping_add` mirrors the production reader.
fn delta_decode(column: &mut [i64]) {
    for i in 1..column.len() {
        column[i] = column[i].wrapping_add(column[i - 1]);
    }
}

/// Delta-decode a 32-bit column in place, used per sketch for its bin-key sequence.
fn delta_decode_i32(column: &mut [i32]) {
    for i in 1..column.len() {
        column[i] = column[i].wrapping_add(column[i - 1]);
    }
}

/// A v3 series the intake kept: its metric name, its tagset with any `host` resource folded in as a
/// `host:<name>` tag, and its metric kind. This mirrors what the v2 lane's `Context` carries, so v3
/// and v2 contexts compare equal for equivalent input.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct V3Series {
    pub(crate) name: String,
    pub(crate) tags: Vec<String>,
    pub(crate) kind: MetricKind,
    /// The rate interval in seconds, `0` for non-rate kinds.
    pub(crate) interval: u64,
    /// The kept points as `(wire bucket-start, value)`, in wire order.
    pub(crate) points: Vec<(u64, BucketValue)>,
}

/// Bounds-check a delta-decoded reference into a dictionary of `len` entries. A negative index or one
/// at or past `len` is `MalformedWire`, matching the production intake's `ref < 0 || ref >= len`
/// checks. `try_from` folds the sign check and the 32-bit-target width check into the same rejection.
fn index(v: i64, len: usize) -> Result<usize, Rejection> {
    let idx = usize::try_from(v).map_err(|_| Rejection::MalformedWire)?;
    if idx >= len {
        return Err(Rejection::MalformedWire);
    }
    Ok(idx)
}

/// Natively decode a `/api/intake/metrics/v3/series` body into the series the intake keeps. This is an
/// independent reimplementation of the production intake's v3 decode and per-series validation, not a
/// wrapper over the data plane's own decoder, so it catches divergences from production rather than
/// masking them.
///
/// The v3 native series API is dictionary + delta encoded columnar protobuf. Production applies a
/// two-tier failure model that this mirrors exactly:
///
/// 1. Whole-payload abort (return `Err`, keep nothing): any dictionary-decode error, any structural,
///    bounds, or reference error while reconstructing a metric, and non-UTF-8 bytes in the name, unit,
///    resource, or source-type string dictionaries. Production aborts the whole payload here too.
/// 2. Per-series drop-and-continue (drop that series, keep the rest): a per-series validation failure
///    only. A single bad series never drops the whole payload.
///
/// Only the tag dictionary tolerates non-UTF-8, sanitized to match the backend's tag handling, keeping
/// the v2 names-strict/tags-lenient policy.
pub(crate) fn decode_series_v3(body: &[u8]) -> Result<Vec<V3Series>, Rejection> {
    let payload = V3Payload::parse_from_bytes(body)?;
    // Production errors on nil metric data; both paths keep nothing, so abort the whole payload.
    let data = payload.metricData.as_ref().ok_or(Rejection::MalformedWire)?;

    let empty_tags: &[String] = &[];
    let empty_resources: &[String] = &[];
    let meta_tags = payload.metadata.as_ref().map_or(empty_tags, |m| &m.tags);
    let meta_resources = payload.metadata.as_ref().map_or(empty_resources, |m| &m.resources);

    // Decode dictionaries. Only the tag dictionary is UTF-8-lenient; the rest reject non-UTF-8, as
    // production does, aborting the whole payload.
    let dict_name = unpack_str_dict(&data.dictNameStr, false)?;
    let dict_tag = unpack_str_dict(&data.dictTagStr, true)?;
    let dict_unit = unpack_str_dict(&data.dictUnitStr, false)?;
    let tagsets = unpack_tagsets(&data.dictTagsets, &dict_tag, meta_tags)?;
    let dict_resource = unpack_str_dict(&data.dictResourceStr, false)?;
    let resources = unpack_resources(
        &data.dictResourceLen,
        &data.dictResourceType,
        &data.dictResourceName,
        &dict_resource,
        meta_resources,
    )?;
    let dict_source_type = unpack_str_dict(&data.dictSourceTypeName, false)?;
    let origin_dict_len = origin_dict_len(&data.dictOriginInfo)?;

    // Persistent delta accumulators, matching the production reader's per-column running state. Go
    // int64 addition wraps, so `wrapping_add` mirrors it and the sign/bounds checks catch the rest.
    let mut name_ref = 0i64;
    let mut tagset_ref = 0i64;
    let mut resources_ref = 0i64;
    let mut source_type_ref = 0i64;
    let mut origin_ref = 0i64;
    let mut unit_ref = 0i64;
    let mut kept = Vec::new();
    let mut cur = PointCursors::default();
    // Absolute wire bucket-starts, delta-decoded once for the whole shared column.
    let mut timestamps = data.timestamps.clone();
    delta_decode(&mut timestamps);
    // Sketch bin keys are delta-decoded per sketch during the walk, so keep a mutable copy.
    let mut bin_keys = data.sketchBinKeys.clone();

    for i in 0..data.types.len() {
        // Every per-metric column must have an entry for this metric. Any short column aborts the
        // whole payload, matching the production reader's bounds checks.
        if i >= data.nameRefs.len()
            || i >= data.tagsetRefs.len()
            || i >= data.resourcesRefs.len()
            || i >= data.sourceTypeNameRefs.len()
            || i >= data.originInfoRefs.len()
            || i >= data.intervals.len()
            || i >= data.numPoints.len()
        {
            return Err(Rejection::MalformedWire);
        }

        name_ref = name_ref.wrapping_add(data.nameRefs[i]);
        let name_idx = index(name_ref, dict_name.len())?;
        tagset_ref = tagset_ref.wrapping_add(data.tagsetRefs[i]);
        let tagset_idx = index(tagset_ref, tagsets.len())?;
        // The production reader advances the unit column only when this metric has a unit ref.
        if i < data.unitRefs.len() {
            unit_ref = unit_ref.wrapping_add(data.unitRefs[i]);
            index(unit_ref, dict_unit.len())?;
        }
        resources_ref = resources_ref.wrapping_add(data.resourcesRefs[i]);
        let resources_idx = index(resources_ref, resources.len())?;
        source_type_ref = source_type_ref.wrapping_add(data.sourceTypeNameRefs[i]);
        index(source_type_ref, dict_source_type.len())?;
        origin_ref = origin_ref.wrapping_add(data.originInfoRefs[i]);
        index(origin_ref, origin_dict_len)?;

        let name = &dict_name[name_idx];
        let tags = &tagsets[tagset_idx];
        let series_resources = &resources[resources_idx];

        let kind = match data.types[i] & 0xF {
            1 => MetricKind::Count,
            2 => MetricKind::Rate,
            3 => MetricKind::Gauge,
            4 => MetricKind::Sketch,
            // An out-of-range nibble: production keeps the series and forwards its type verbatim, so
            // keep it as Other rather than dropping it and masking a producer bug.
            _ => MetricKind::Other,
        };

        // Drain this metric's declared points from the shared flat columns. Run for every metric,
        // kept or dropped, because production drains a dropped series' points too and the cursors are
        // shared across metrics. The truncated count itself is not kept, only the draining matters.
        let is_sketch = (data.types[i] & 0xF) == 4;
        let value_type = data.types[i] & 0xF0;
        let points = materialize_points(
            data,
            is_sketch,
            value_type,
            data.numPoints[i],
            &timestamps,
            &mut bin_keys,
            &mut cur,
        );

        // Per-series validation and host-tag folding. A dropped series continues to the next, never
        // aborting the whole payload; point cursors have already advanced above, matching production
        // draining a dropped series' points.
        let Some(out_tags) = kept_series_tags(name, tags, series_resources) else {
            continue;
        };

        kept.push(V3Series {
            name: name.clone(),
            tags: out_tags,
            kind,
            interval: data.intervals[i],
            points,
        });
    }

    Ok(kept)
}

/// Apply the production intake's per-series keep checks (in order) and fold the host resource into a tag.
/// Returns the series' output tags when kept, or `None` when any check drops it. Lane parity with v2:
/// the first `host` resource with a non-empty name becomes a `host:<name>` tag so v3 contexts equal v2
/// contexts for equivalent input.
///
/// The backend's `validateSeries` also drops a series on a `processOrigin` failure
/// (api_series_v3_handler.go:538-544), the one source-dependent v3 check. The rig omits it knowingly:
/// v3 decode folds origin into numeric-only reference fields that never enter a context's identity, so
/// a dropped origin can never change the emitted context set, and modeling it would need the source
/// threaded into v3, which must stay source-independent.
fn kept_series_tags(name: &str, tags: &[String], resources: &[(String, String)]) -> Option<Vec<String>> {
    if tags.len() > MAX_TAG_COUNT || resources.len() > MAX_RESOURCE_COUNT || !metric_name_kept(name) {
        return None;
    }
    let host = resources.iter().find(|(type_, _)| type_ == "host");
    if let Some((_, host_name)) = host {
        if host_name.len() > MAX_HOST_NAME_LEN {
            return None;
        }
    }
    let mut out_tags = tags.to_vec();
    if let Some((_, host_name)) = host {
        if !host_name.is_empty() {
            out_tags.push(format!("host:{host_name}"));
        }
    }
    Some(out_tags)
}

/// Unpack a v3 varint-length-prefixed string dictionary. Index 0 is the empty string (base-1
/// indexing). When `sanitize` is set the tag dictionary's non-UTF-8 entries collapse to `to_valid_utf8`
/// (Go's `strings.ToValidUTF8`); otherwise non-UTF-8 aborts the whole payload as production does. A
/// length prefix that overruns the buffer is `MalformedWire`.
fn unpack_str_dict(raw: &[u8], sanitize: bool) -> Result<Vec<String>, Rejection> {
    let mut dict = vec![String::new()];
    let mut offset = 0;
    while offset < raw.len() {
        let (len, consumed) = read_uvarint(&raw[offset..])?;
        offset += consumed;
        let len = usize::try_from(len).map_err(|_| Rejection::MalformedWire)?;
        let end = offset.checked_add(len).ok_or(Rejection::MalformedWire)?;
        if end > raw.len() {
            return Err(Rejection::MalformedWire);
        }
        let bytes = &raw[offset..end];
        let str = if sanitize {
            to_valid_utf8(bytes)
        } else {
            strict(bytes.to_vec())?
        };
        dict.push(str);
        offset = end;
    }
    Ok(dict)
}

/// Unpack the tagsets dictionary: a flat `[]i64` of length-prefixed groups. Index 0 is the empty
/// tagset. Within a group a delta accumulator resets to 0; a positive index selects
/// `dictTagStr[idx]` and a negative index splices the earlier `tagsets[-idx]` back-reference, both
/// bounds-checked. Any bounds or reference error aborts the whole payload. Payload metadata tags are
/// then unioned into every tagset, series tags first.
fn unpack_tagsets(packed: &[i64], tag_dict: &[String], meta_tags: &[String]) -> Result<Vec<Vec<String>>, Rejection> {
    let mut tagsets: Vec<Vec<String>> = vec![Vec::new()];
    let mut i = 0usize;
    while i < packed.len() {
        let size = packed[i];
        i += 1;
        let size = usize::try_from(size).map_err(|_| Rejection::MalformedWire)?;
        if size > packed.len() - i {
            return Err(Rejection::MalformedWire);
        }
        let mut tags = Vec::with_capacity(size + meta_tags.len());
        let mut idx: i64 = 0;
        for offset in 0..size {
            idx = idx.wrapping_add(packed[i + offset]);
            if idx < 0 {
                // A negative index is a `tagsets[-idx]` back-reference. `i64::MIN` has no positive
                // negation, so `checked_neg` folds that into the same bounds rejection.
                let back = idx.checked_neg().ok_or(Rejection::MalformedWire)?;
                tags.extend_from_slice(&tagsets[index(back, tagsets.len())?]);
            } else {
                tags.push(tag_dict[index(idx, tag_dict.len())?].clone());
            }
        }
        i += size;
        tagsets.push(tags);
    }

    union_metadata_tags(&mut tagsets, meta_tags);
    Ok(tagsets)
}

/// Union payload metadata tags into every tagset. An empty tagset takes all metadata tags; otherwise
/// only metadata tags not already present are appended, series tags first.
fn union_metadata_tags(tagsets: &mut [Vec<String>], meta_tags: &[String]) {
    if meta_tags.is_empty() {
        return;
    }
    for tags in tagsets.iter_mut() {
        if tags.is_empty() {
            tags.extend_from_slice(meta_tags);
            continue;
        }
        for mt in meta_tags {
            if !tags.contains(mt) {
                tags.push(mt.clone());
            }
        }
    }
}

/// Unpack the resources dictionary from the parallel length, type, and name columns. Index 0 is the
/// empty resource set. Each group has its own delta accumulators for the type and name references,
/// both indexing `dictResourceStr`; each entry is a `(Type, Name)` pair. Any bounds or reference
/// error aborts the whole payload. Payload metadata resource pairs are appended to every group.
fn unpack_resources(
    len_col: &[i64], type_col: &[i64], name_col: &[i64], res_dict: &[String], meta_resources: &[String],
) -> Result<Vec<Vec<(String, String)>>, Rejection> {
    let meta_pairs = if meta_resources.is_empty() {
        Vec::new()
    } else {
        if !meta_resources.len().is_multiple_of(2) {
            return Err(Rejection::MalformedWire);
        }
        meta_resources
            .chunks_exact(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect::<Vec<_>>()
    };

    let mut resources: Vec<Vec<(String, String)>> = vec![Vec::new()];
    let mut start = 0usize;
    for &size in len_col {
        let size = usize::try_from(size).map_err(|_| Rejection::MalformedWire)?;
        let end = start.checked_add(size).ok_or(Rejection::MalformedWire)?;
        if end > type_col.len() || end > name_col.len() {
            return Err(Rejection::MalformedWire);
        }
        let mut type_ref = 0i64;
        let mut name_ref = 0i64;
        let mut set = Vec::with_capacity(size + meta_pairs.len());
        for pos in start..end {
            type_ref = type_ref.wrapping_add(type_col[pos]);
            name_ref = name_ref.wrapping_add(name_col[pos]);
            let type_idx = index(type_ref, res_dict.len())?;
            let name_idx = index(name_ref, res_dict.len())?;
            set.push((res_dict[type_idx].clone(), res_dict[name_idx].clone()));
        }
        set.extend(meta_pairs.iter().cloned());
        resources.push(set);
        start = end;
    }
    Ok(resources)
}

/// Return the base-1 length of the origin-info dictionary. Production packs it as `(product,
/// category, service)` triples, so a length that is not a multiple of three aborts the whole payload.
/// Only the length is needed to bounds-check origin references.
fn origin_dict_len(raw: &[i32]) -> Result<usize, Rejection> {
    let nelem = raw.len() / 3;
    if raw.len() != nelem * 3 {
        return Err(Rejection::MalformedWire);
    }
    Ok(nelem + 1)
}

/// Reads a base-128 unsigned varint, matching Go's `encoding/binary.Uvarint` exactly, including its
/// overflow rule: a value needing more than 64 bits is rejected, so a 10th byte greater than 1 fails
/// as production would rather than silently truncating a mutated length prefix.
fn read_uvarint(data: &[u8]) -> Result<(u64, usize), Rejection> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in data.iter().enumerate() {
        if i == 10 {
            return Err(Rejection::MalformedWire);
        }
        if byte & 0x80 == 0 {
            if i == 9 && byte > 1 {
                return Err(Rejection::MalformedWire);
            }
            return Ok((value | (u64::from(byte) << shift), i + 1));
        }
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
    }
    Err(Rejection::MalformedWire)
}

#[cfg(test)]
mod tests {
    use datadog_protos::metrics::metric_payload::{MetricPoint, MetricSeries, MetricType, Resource};
    use datadog_protos::metrics::sketch_payload::sketch::Dogsketch;
    use datadog_protos::metrics::sketch_payload::Sketch;
    use datadog_protos::metrics::v3::{MetricData as V3MetricData, Payload as V3Payload};
    use datadog_protos::metrics::{Metadata, MetricPayload, Origin, SketchPayload};
    use proptest::prelude::*;
    use protobuf::{EnumOrUnknown, Message, MessageField};

    use super::{
        decode_metric_payload, decode_series_v3, decode_sketch_payload, read_uvarint, to_valid_utf8, BucketValue,
        MetricKind, Rejection, SketchValue, Source,
    };

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
        // The metric name is strict for EVERY source, so even the Agent source rejects a bad name.
        assert_eq!(
            decode_metric_payload(&bytes, Source::DatadogAgent),
            Err(Rejection::NonUtf8StrictField)
        );
    }

    // Truncated wire (field 1, LEN, incomplete length varint) is genuinely malformed.
    #[test]
    fn malformed_wire_is_rejected() {
        assert_eq!(
            decode_metric_payload(&[0x0a, 0xff], Source::DatadogAgent),
            Err(Rejection::MalformedWire)
        );
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
        let lenient = decode_metric_payload(&payload, Source::DatadogAgent).expect("lenient");
        assert_eq!(lenient, strict);
    }

    // Tag byte 0x00 is field 0, wire Varint. Field 0 is invalid protobuf, rejected like strict.
    #[test]
    fn field_zero_tag_is_malformed_wire() {
        assert_eq!(
            decode_metric_payload(&[0x00, 0x01], Source::DatadogAgent),
            Err(Rejection::MalformedWire)
        );
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
        assert!(decode_metric_payload(&top, Source::DatadogAgent).is_ok());

        // Series (field 1, length-delimited, 2-byte body) whose field 2 (metric name, expects
        // length-delimited) is encoded as a varint.
        let framed = [0x0a, 0x02, 0x10, 0x01];
        assert!(MetricPayload::parse_from_bytes(&framed).is_ok());
        assert!(decode_metric_payload(&framed, Source::DatadogAgent).is_ok());
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
        let lenient = decode_metric_payload(&bytes, Source::DatadogAgent).expect("lenient decode");
        assert_eq!(lenient, strict);
    }

    // A non-UTF-8 sketch tag whole-payload-rejects, source-independent: the backend has no bytes-tag
    // retry on the sketch path, so a feral sketch tag drops the payload like the metric/host fields.
    // This is the sketch analogue of the v2 non-agent tag reject, not the v2 Agent sanitize.
    #[test]
    fn sketch_non_utf8_tag_rejected() {
        let sketch_body = [framed(1, b"latency"), framed(4, b"shard:\xff\xffeast")].concat();
        let payload = framed(1, &sketch_body);

        assert_eq!(decode_sketch_payload(&payload), Err(Rejection::NonUtf8StrictField));
    }

    // A non-UTF-8 sketch metric name still drops the payload, matching production's strict treatment
    // of non-tag fields.
    #[test]
    fn sketch_non_utf8_metric_rejected() {
        let sketch_body = framed(1, b"\xfflatency");
        let payload = framed(1, &sketch_body);

        assert_eq!(decode_sketch_payload(&payload), Err(Rejection::NonUtf8StrictField));
    }

    // For a valid-UTF-8 sketch the lenient walker must recover exactly what the strict parser does,
    // dogsketch bins and all, or a renumbered field would silently corrupt distribution contexts.
    #[test]
    fn sketch_lenient_matches_strict_for_valid_utf8() {
        let mut payload = SketchPayload::new();
        let mut sketch = Sketch::new();
        sketch.metric = "latency".into();
        sketch.host = "server-1".into();
        sketch.tags.push("shard:us-east-1".into());
        sketch.tags.push("host:antithesis-differential".into());
        let mut dogsketch = Dogsketch::new();
        dogsketch.ts = 1_600_000_000;
        dogsketch.cnt = 3;
        dogsketch.min = 1.0;
        dogsketch.max = 9.0;
        dogsketch.avg = 4.0;
        dogsketch.sum = 12.0;
        dogsketch.k = vec![-1, 0, 2];
        dogsketch.n = vec![1, 1, 1];
        sketch.dogsketches.push(dogsketch);
        payload.sketches.push(sketch);

        let bytes = payload.write_to_bytes().expect("serialize");
        let strict = SketchPayload::parse_from_bytes(&bytes).expect("strict parse");
        let lenient = decode_sketch_payload(&bytes).expect("lenient decode");
        assert_eq!(lenient, strict);
    }

    // Build a v3 varint-length-prefixed string dictionary. Entries stay under 128 bytes so each
    // length prefix is a single byte.
    fn v3_str_dict(entries: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            let len = u8::try_from(entry.len()).expect("dict entry under 128 bytes");
            assert!(len < 0x80, "dict entry under 128 bytes");
            out.push(len);
            out.extend_from_slice(entry);
        }
        out
    }

    fn v3_payload_bytes(data: V3MetricData) -> Vec<u8> {
        let mut payload = V3Payload::new();
        payload.metricData = MessageField::some(data);
        payload.write_to_bytes().expect("serialize v3 payload")
    }

    // Delta-encode absolute indices into the running deltas the wire format carries.
    fn delta(absolute: &[i64]) -> Vec<i64> {
        let mut prev = 0;
        absolute
            .iter()
            .map(|&a| {
                let d = a - prev;
                prev = a;
                d
            })
            .collect()
    }

    // Fill the per-metric columns the production reader requires for scalar COUNT metrics, given each
    // metric's absolute name/tagset/resource ref. Source-type and origin ref columns are padded with
    // zeros (ref 0 -> the base-1 empty dict entry), which the reader still requires to be present.
    fn v3_scalar_metrics(data: &mut V3MetricData, names: &[i64], tagsets: &[i64], resources: &[i64]) {
        let n = names.len();
        data.types = vec![1 | 0x30; n]; // Count | Float64
        data.nameRefs = delta(names);
        data.tagsetRefs = delta(tagsets);
        data.resourcesRefs = delta(resources);
        data.sourceTypeNameRefs = vec![0; n];
        data.originInfoRefs = vec![0; n];
        data.intervals = vec![0; n];
        data.numPoints = vec![1; n];
    }

    // A v3 payload round-trips its points: the decoder materializes each point's absolute wire
    // bucket-start (delta-decoded) and Float64 value exactly as encoded. This is the decode-fidelity
    // gate -- an unfaithful lane would poison every curve, the e886e39c capture-artifact class.
    #[test]
    fn v3_materializes_scalar_points_faithfully() {
        let mut data = V3MetricData::new();
        data.dictNameStr = v3_str_dict(&[b"app.gauge"]);
        data.dictTagStr = v3_str_dict(&[b"env:prod"]);
        data.dictTagsets = vec![1, 1];
        data.types = vec![3 | 0x30]; // Gauge | Float64
        data.nameRefs = vec![1];
        data.tagsetRefs = vec![1];
        data.resourcesRefs = vec![0];
        data.sourceTypeNameRefs = vec![0];
        data.originInfoRefs = vec![0];
        data.intervals = vec![0];
        data.numPoints = vec![2];
        // Absolute bucket-starts 100 and 110, delta-encoded on the wire.
        data.timestamps = delta(&[100, 110]);
        data.valsFloat64 = vec![1.5, 2.5];

        let kept = decode_series_v3(&v3_payload_bytes(data)).expect("valid v3 payload decodes");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, MetricKind::Gauge);
        assert_eq!(
            kept[0].points,
            vec![(100, BucketValue::Scalar(1.5)), (110, BucketValue::Scalar(2.5))]
        );
    }

    // A v3 sketch point round-trips its full distribution: summary (sum/min/max in the value column,
    // count in sint64) plus the delta-decoded log-grid bins. The full bins are what the oracle's
    // 1-Wasserstein ground distance needs, so losing them here would blind the sketch comparison.
    #[test]
    fn v3_materializes_sketch_point_faithfully() {
        let mut data = V3MetricData::new();
        data.dictNameStr = v3_str_dict(&[b"app.dist"]);
        data.dictTagStr = v3_str_dict(&[b"env:prod"]);
        data.dictTagsets = vec![1, 1];
        data.types = vec![4 | 0x30]; // Sketch | Float64
        data.nameRefs = vec![1];
        data.tagsetRefs = vec![1];
        data.resourcesRefs = vec![0];
        data.sourceTypeNameRefs = vec![0];
        data.originInfoRefs = vec![0];
        data.intervals = vec![0];
        data.numPoints = vec![1];
        data.timestamps = delta(&[200]);
        // sum, min, max in the Float64 column; count in sint64.
        data.valsFloat64 = vec![30.0, 1.0, 9.0];
        data.valsSint64 = vec![5];
        // One sketch, two bins. Keys are delta-encoded on the wire ([3, 2] -> absolute [3, 5]).
        data.sketchNumBins = vec![2];
        data.sketchBinKeys = vec![3, 2];
        data.sketchBinCnts = vec![2, 3];

        let kept = decode_series_v3(&v3_payload_bytes(data)).expect("valid v3 sketch decodes");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, MetricKind::Sketch);
        assert_eq!(
            kept[0].points,
            vec![(
                200,
                BucketValue::Sketch(SketchValue {
                    count: 5,
                    sum: 30.0,
                    min: 1.0,
                    max: 9.0,
                    bins: vec![(3, 2), (5, 3)],
                })
            )]
        );
    }

    // A non-UTF-8 tag survives the protobuf parse (dictTagStr is `bytes`), is sanitized with
    // to_valid_utf8, and the series is kept. This is exactly the feral-tag case the intake must not drop.
    #[test]
    fn v3_non_utf8_tag_sanitized_and_series_kept() {
        let mut data = V3MetricData::new();
        data.dictNameStr = v3_str_dict(&[b"app.count"]);
        data.dictTagStr = v3_str_dict(&[b"shard:\xff\xffeast"]);
        data.dictTagsets = vec![1, 1]; // one tagset: size 1, tag dict index 1
        v3_scalar_metrics(&mut data, &[1], &[1], &[0]);

        let kept = decode_series_v3(&v3_payload_bytes(data)).expect("v3 tags never reject");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "app.count");
        assert_eq!(kept[0].tags, vec![to_valid_utf8(b"shard:\xff\xffeast")]);
    }

    // A non-UTF-8 name dictionary entry aborts the whole payload, production-faithful: the name
    // dictionary is UTF-8-strict, unlike the tag dictionary.
    #[test]
    fn v3_non_utf8_name_aborts_whole_payload() {
        let mut data = V3MetricData::new();
        data.dictNameStr = v3_str_dict(&[b"\xffbad.name"]);
        v3_scalar_metrics(&mut data, &[1], &[0], &[0]);
        assert_eq!(
            decode_series_v3(&v3_payload_bytes(data)),
            Err(Rejection::NonUtf8StrictField)
        );
    }

    // Genuinely malformed protobuf wire (field 3 metricData, LEN, truncated length varint) rejects.
    #[test]
    fn v3_malformed_wire_rejected() {
        assert_eq!(decode_series_v3(&[0x1a, 0xff]), Err(Rejection::MalformedWire));
    }

    // A 10-byte varint whose final byte exceeds 1 needs more than 64 bits; Go's `binary.Uvarint`
    // rejects it, so a mutated length prefix aborts rather than silently truncating. A final byte of
    // 1 is the legal maximum width and is accepted.
    #[test]
    fn uvarint_rejects_overlong_value() {
        let overflow = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert_eq!(read_uvarint(&overflow), Err(Rejection::MalformedWire));
        let widest = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert!(read_uvarint(&widest).is_ok());
    }

    // A tag dictionary whose length prefix runs past the end of the buffer aborts the whole payload.
    #[test]
    fn v3_tag_dict_overrun_rejected() {
        let mut data = V3MetricData::new();
        data.dictTagStr = vec![10, b'a', b'b'];
        assert_eq!(decode_series_v3(&v3_payload_bytes(data)), Err(Rejection::MalformedWire));
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
            // Valid UTF-8 decodes identically for every source; the source only gates feral tags.
            let lenient = decode_metric_payload(&bytes, Source::DatadogAgent).expect("lenient decode");
            prop_assert_eq!(lenient, strict);
        }

        // The intake faces feral traffic, so the walker must be total for BOTH sources: any byte string
        // decodes to Ok or a Rejection, never a panic, whether tags sanitize or reject.
        #[test]
        fn prop_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_metric_payload(&bytes, Source::DatadogAgent);
            let _ = decode_metric_payload(&bytes, Source::Other);
        }

        // Agent-source tags tolerate arbitrary bytes: a payload whose only non-UTF-8 lives in tags always
        // decodes for `datadog-agent`, and each tag is sanitized exactly as to_valid_utf8 would.
        #[test]
        fn prop_agent_non_utf8_tags_accepted_and_sanitized(
            metric in "[a-z]{1,6}",
            tags in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..12), 0..4),
        ) {
            let mut body = framed(2, metric.as_bytes());
            for tag in &tags {
                body.extend(framed(3, tag));
            }
            let decoded =
                decode_metric_payload(&framed(1, &body), Source::DatadogAgent).expect("agent tags never reject");
            let want: Vec<String> = tags.iter().map(|t| to_valid_utf8(t)).collect();
            prop_assert_eq!(&decoded.series[0].tags, &want);
            prop_assert_eq!(&decoded.series[0].metric, &metric);
        }

        // Every non-agent source whole-payload-rejects a non-UTF-8 tag: the backend runs its bytes-tag
        // retry only for `SourceAgent`, so any other source drops the payload. The leading 0xFF forces
        // an invalid tag regardless of the generated suffix.
        #[test]
        fn prop_non_agent_non_utf8_tag_rejected(suffix in prop::collection::vec(any::<u8>(), 0..8)) {
            let mut tag = vec![0xff];
            tag.extend_from_slice(&suffix);
            let body = [framed(2, b"metric"), framed(3, &tag)].concat();
            prop_assert_eq!(
                decode_metric_payload(&framed(1, &body), Source::Other),
                Err(Rejection::NonUtf8StrictField)
            );
        }

        // A non-UTF-8 byte in a strict string field (metric name) is the rejection production makes for
        // EVERY source, agent included: the name stays `string` in the bytes-tag variant too. The
        // leading 0xFF guarantees invalid UTF-8 regardless of the generated suffix.
        #[test]
        fn prop_non_utf8_name_rejected_for_all_sources(suffix in prop::collection::vec(any::<u8>(), 0..8)) {
            let mut name = vec![0xff];
            name.extend_from_slice(&suffix);
            let body = framed(2, &name);
            for source in [Source::DatadogAgent, Source::Other] {
                prop_assert_eq!(
                    decode_metric_payload(&framed(1, &body), source),
                    Err(Rejection::NonUtf8StrictField)
                );
            }
        }
    }

    // getUserAgent parity: `SourceAgent` iff the lowercased header begins "datadog-agent". The prefix
    // test is case-insensitive; every other value, and an absent header, is `Other`.
    #[test]
    fn source_from_user_agent_matches_getuseragent() {
        assert_eq!(
            Source::from_user_agent(Some("datadog-agent/7.55.0")),
            Source::DatadogAgent
        );
        assert_eq!(
            Source::from_user_agent(Some("Datadog-Agent/7.55.0")),
            Source::DatadogAgent
        );
        assert_eq!(Source::from_user_agent(Some("DATADOG-AGENT")), Source::DatadogAgent);
        // "Datadog Agent" (space, not hyphen) is the backend's SourceOlderAgent, a non-agent source.
        assert_eq!(Source::from_user_agent(Some("Datadog Agent/5.0")), Source::Other);
        assert_eq!(Source::from_user_agent(Some("go-http-client/1.1")), Source::Other);
        assert_eq!(Source::from_user_agent(Some("Vector/0.30")), Source::Other);
        assert_eq!(Source::from_user_agent(Some("")), Source::Other);
        assert_eq!(Source::from_user_agent(None), Source::Other);
    }

    // The v2 tag branch is source-gated on the same bytes: the Agent sanitizes a feral tag and keeps the
    // payload, every other source whole-payload-rejects it. The name stays strict either way.
    #[test]
    fn v2_tag_gating_splits_on_source() {
        let body = [framed(2, b"latency"), framed(3, b"shard:\xff\xffeast")].concat();
        let payload = framed(1, &body);

        let agent = decode_metric_payload(&payload, Source::DatadogAgent).expect("agent sanitizes feral tag");
        assert_eq!(agent.series[0].tags, vec![to_valid_utf8(b"shard:\xff\xffeast")]);

        assert_eq!(
            decode_metric_payload(&payload, Source::Other),
            Err(Rejection::NonUtf8StrictField)
        );
    }
}
