//! Spec-derived bounds the payload property checks depend on. Sourced from the
//! crate `README.md` and the Agent's published intake limits.

/// `serializer_max_series_points_per_payload` default (Pyld08 / Pyld15).
pub(crate) const MAX_POINTS_PER_PAYLOAD: usize = 10_000;

/// `MaxTagLength` default (Pyld23). Per-tag byte cap. The intake clips longer tags, so a non-lax Agent
/// must not emit one. `AllowLongTag` raises the cap to 5120 only for `SourceDDGO`, which neither the
/// Datadog Agent nor ADP is, so 200 is the operative threshold on both lanes.
pub(crate) const MAX_TAG_LENGTH_BYTES: usize = 200;

/// `MaxTagSetSize` default (Pyld24). Per-series total tag-set byte cap.
pub(crate) const MAX_TAG_SET_SIZE_BYTES: usize = 100 * 1024;
