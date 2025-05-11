//! Component interconnects.

mod event_buffer;
pub use self::event_buffer::{EventBufferManager, FixedSizeEventBuffer, FixedSizeEventBufferInner};

mod event_stream;
pub use self::event_stream::EventStream;

mod dispatcher;
pub use self::dispatcher::{BufferedDispatcher, Dispatcher};
