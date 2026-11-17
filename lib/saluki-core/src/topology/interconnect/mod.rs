//! Component interconnects.

mod event_buffer;
pub(crate) use self::event_buffer::EventStorage;
pub use self::event_buffer::{EventBufferIntoIter, EventBufferManager, FixedSizeEventBuffer};

mod typed_event_buffer;
pub use self::typed_event_buffer::TypedEventBuffer;

mod consumer;
pub use self::consumer::Consumer;

mod dispatcher;
pub(crate) use self::dispatcher::DispatchBuffer;
pub use self::dispatcher::{BufferedDispatcher, Dispatcher};

/// A value that can be dispatched.
pub trait Dispatchable: Clone {
    /// Returns the number of items in the dispatchable value.
    ///
    /// This determines how many underlying items are contained within the dispatchable value. For single item types, this will
    /// generally be one. For container types, this will be the number of items contained within the container.
    fn item_count(&self) -> usize;
}

/// Converts a value received from an interconnect into the type a component consumes.
///
/// Interconnects carry a single type, such as [`FixedSizeEventBuffer`], between every pair of connected components. A
/// component can consume a different type, such as a [`TypedEventBuffer`], as long as it can be converted from what the
/// interconnect carries. The conversion runs once per received value.
///
/// Every type converts from itself, which is how components consume what the interconnect carries as is.
pub trait FromInterconnect<W>: Sized {
    /// Converts the value, or hands it back if it can't be converted.
    ///
    /// # Errors
    ///
    /// If the value can't be converted, such as when an event buffer holds events of a type the component doesn't
    /// consume, the original value is returned.
    fn from_interconnect(value: W) -> Result<Self, W>;
}

impl<T> FromInterconnect<T> for T {
    fn from_interconnect(value: T) -> Result<Self, T> {
        Ok(value)
    }
}
