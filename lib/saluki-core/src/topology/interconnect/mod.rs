//! Component interconnects.

use crate::execution::BatchHeader;

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

    /// Returns the batch header this value carries, if it carries one.
    ///
    /// A dispatcher stamps the header of every value it sends on an output that is an
    /// [admission point][crate::execution::AdmissionPoint]. Values without a header, which is the default, are sent
    /// unchanged.
    fn header_mut(&mut self) -> Option<&mut BatchHeader> {
        None
    }
}
