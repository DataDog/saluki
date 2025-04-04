//! Component interconnects.

mod event_buffer;
pub use self::event_buffer::{EventBufferManager, FixedSizeEventBuffer, FixedSizeEventBufferInner};

mod event_stream;
pub use self::event_stream::EventStream;

mod forwarder;
pub use self::forwarder::{BufferedForwarder, Forwarder};
