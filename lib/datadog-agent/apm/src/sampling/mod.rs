//! Trace sampling settings delivered by the `APM_SAMPLING` Remote Configuration product.
//!
//! [`TraceSamplingSubscription`] receives updates from a Remote Configuration client. Each update is a
//! [`RemoteSampling`] value: either a [`SamplerConfig`] or a marker that no configuration is assigned. A trace sampler
//! calls [`SamplerConfig::resolve`] with its local [`SamplerSettings`] to get the effective settings for its
//! environment. This module covers only the settings; applying them to sampler state is up to the consumer.

mod remote_config;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

pub use remote_config::{RemoteSampling, SamplerConfig, SamplerSettings, TraceSamplingSubscription};
