// Display names for the V3 columns, indexed by their Protocol Buffers field number (from
// `lib/protos/datadog/proto/agent-payload/intake_v3.proto`, also mirrored in
// `datadog_metrics_v3::writer` as the `*_FIELD_NUMBER` constants). Index 0 is unused since
// field numbers start at 1.
pub(super) const COLUMN_NAMES: [&str; 27] = [
    "reserved",
    "DictNameStr",
    "DictTagsStr",
    "DictTagsets",
    "DictResourceStr",
    "DictResourcesLen",
    "DictResourceType",
    "DictResourceName",
    "DictSourceTypeName",
    "DictOriginInfo",
    "Type",
    "Name",
    "Tags",
    "Resources",
    "Interval",
    "NumPoints",
    "Timestamp",
    "ValueSint64",
    "ValueFloat32",
    "ValueFloat64",
    "SketchNBins",
    "SketchBinKeys",
    "SketchBinCounts",
    "SourceTypeName",
    "OriginInfo",
    "DictUnitStr",
    "UnitRef",
];
