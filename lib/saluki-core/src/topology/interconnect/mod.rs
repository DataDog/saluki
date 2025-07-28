//! Component interconnects.

mod event_buffer;
pub use self::event_buffer::{EventBufferManager, FixedSizeEventBuffer};

mod consumer;
pub use self::consumer::Consumer;

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
