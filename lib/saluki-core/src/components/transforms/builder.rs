use std::num::NonZeroU16;

use async_trait::async_trait;
use saluki_error::GenericError;

use super::{SynchronousTransform, Transform};
use crate::accounting::MemoryBounds;
use crate::{
    components::BuildContext,
    data_model::event::EventType,
    topology::{OutputDefinition, DEFAULT_EVENTS_BUFFER_CAPACITY},
};

/// A transform builder.
///
/// Transform builders are responsible for creating instances of [`Transform`]s, as well as describing high-level
/// aspects of the built transform, such as the data types allowed for input events and the outputs exposed by the
/// transform.
#[async_trait]
pub trait TransformBuilder: MemoryBounds {
    /// Event type allowed as input events to this transform.
    fn input_event_type(&self) -> EventType;

    /// Event outputs exposed by this transform.
    fn outputs(&self) -> &[OutputDefinition<EventType>];

    /// Returns the number of input lanes of this transform: the input channels it reads, one per task it runs its work
    /// on (see [`TransformContext::take_lane_inputs`][super::TransformContext::take_lane_inputs]).
    ///
    /// The topology reads this when the transform is added, as it reads its outputs, and builds one bounded channel per
    /// lane. Edges into a transform with lanes name how their sender picks a lane
    /// ([`LaneEdge`][crate::topology::LaneEdge]). Defaults to one lane: one channel that every edge feeds.
    fn input_lanes(&self) -> NonZeroU16 {
        NonZeroU16::MIN
    }

    /// Returns the number of events each buffer sent to this transform has storage allocated for at most, which the
    /// memory bounds of the topology count per buffer of its input channels.
    ///
    /// Defaults to the capacity of an events buffer, 1,024 events. A transform whose senders split their buffers into
    /// smaller ones, such as a lane of a partitioned state owner fed by buffers routed per key, holds more buffers of
    /// fewer events each in the same memory. The bounds count at least one event per buffer, and at most the capacity
    /// of an events buffer.
    ///
    /// Every sender must keep to it: a buffer's storage is what it allocated
    /// ([`EventsBuffer::allocated_capacity`][crate::topology::EventsBuffer::allocated_capacity]), not the events it
    /// holds, so a sender with a buffer allocated for more events copies them into one of this size rather than moving
    /// it whole, and fills a buffer it allocated with this size no further. In a debug build, the transform's input
    /// panics on a buffer that does not keep to it.
    fn input_buffer_events(&self) -> usize {
        DEFAULT_EVENTS_BUFFER_CAPACITY
    }

    /// Builds an instance of the transform.
    ///
    /// ## Errors
    ///
    /// If the transform can't be built for any reason, an error is returned.
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError>;
}

/// A synchronous transform builder.
///
/// Synchronous transforms are a special case of transforms that are executed synchronously, meaning that they don't
/// run as their own task. This is used for certain transforms, typically those where it's more efficient to run
/// multiple transformation steps in a single task.
#[async_trait]
pub trait SynchronousTransformBuilder: MemoryBounds {
    /// Builds an instance of the synchronous transform.
    ///
    /// The provided context is relative to the parent component that's building this synchronous transform.
    ///
    /// ## Errors
    ///
    /// If the synchronous transform can't be built for any reason, an error is returned.
    async fn build(&self, context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError>;
}
