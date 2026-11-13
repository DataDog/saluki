//! DogStatsD source.
//!
//! # Missing
//!
//! - Create a health handle for each listener.
//! - Handle UDS stream framing without treating EOF the same way as UDP and UDS datagram framing.
//! - Track dispatch failures without depending on whether all events were already iterated.

use crate::common::dogstatsd_capture::{
    CaptureRecord, DogStatsDCaptureControl, DogStatsDReplayControl, REPLAY_CREDENTIALS_GID,
};

mod forwarder;
use self::forwarder::{forwarder_queue_capacity, PacketForwarder, PacketForwarderTarget};

mod framer;
use self::framer::{get_framer, DsdFramer, Frame};

mod filters;
pub use self::filters::EnablePayloadsConfiguration;

mod metrics;
use self::metrics::{build_metrics, ForwarderMetrics, Metrics};

mod origin;
pub use self::origin::OriginEnrichmentConfiguration;
use self::origin::{origin_from_packet, DogStatsDOriginTagResolver, ProcessOrigin};

mod resolver;
use self::resolver::ContextResolvers;

mod tags;
use self::tags::WellKnownTags;

mod config;
use self::config::*;
pub use self::config::{DogStatsDConfiguration, ListenerRequirement};

mod decoder;
use self::decoder::*;

mod listener;
use self::listener::*;

mod packets;
use self::packets::*;

#[cfg(test)]
mod tests;
