//! The ADP-specific `data_plane` configuration section.
//!
//! These keys live under the Datadog Agent's `data_plane` section, so a consumer reads them through
//! a nested struct rather than a flattened one. Each consumer composes only the sub-sections it
//! actually reads: a struct that also accepted unrelated `data_plane` keys would silently change
//! shape when one of them is set, which the configuration smoke tests treat as a defect.

use facet::Facet;
use serde::Deserialize;

/// The `data_plane` keys read by a payload encoder.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub(crate) struct EncoderDataPlaneConfiguration {
    /// ADP-specific zstd compression level, taking precedence over the Core Agent's
    /// `serializer_zstd_compressor_level`.
    ///
    /// Defaults to unset, in which case
    /// [`resolve_zstd_compressor_level`](super::resolve_zstd_compressor_level) picks the level.
    #[serde(default)]
    pub(crate) serializer_zstd_compressor_level: Option<i32>,
}
