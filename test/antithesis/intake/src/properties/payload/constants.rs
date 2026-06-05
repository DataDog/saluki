//! Spec-derived bounds the payload property checks depend on. Sourced from the
//! crate `README.md` and the Agent's published intake limits.

/// `serializer_max_series_points_per_payload` default (Pyld08 / Pyld15).
pub(crate) const MAX_POINTS_PER_PAYLOAD: usize = 10_000;
