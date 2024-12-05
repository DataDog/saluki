//! Host provider.
//!
//! This module provides the `HostProvider` trait, which deals with providing information about the process host itself.
//!
//! A default host provider implementation, based on the Datadog Agent, is included.

pub mod providers;

use async_trait::async_trait;

/// Provides information about the process host itself.
#[async_trait]
pub trait HostProvider {
    /// Errors produced by the provider.
    type Error;

    /// Gets the hostname of the process host.
    ///
    /// # Errors
    ///
    /// If an error occurs while querying the hostname, an error is returned.
    async fn get_hostname(&self) -> Result<String, Self::Error>;
}
