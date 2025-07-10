//! Component interconnects.

mod event_buffer;
pub use self::event_buffer::{EventBufferManager, FixedSizeEventBuffer};

mod event_stream;
pub use self::event_stream::EventStream;

mod dispatcher;
pub use self::dispatcher::{BufferedDispatcher, Dispatcher};

/// A value that can be dispatched.
pub trait Dispatchable: Clone {
    /// Returns the number of items in the dispatchable value.
    ///
    /// This determines how many underlying items are contained within the dispatchable value. For single item types, this will
    /// generally be one. For container types, this will be the number of items contained within the container.
    fn item_count(&self) -> usize;
}

impl<T> Dispatchable for T
where
    T: Clone,
{
    fn item_count(&self) -> usize {
        1
    }
}
