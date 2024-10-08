//! Component interconnects.

mod event_buffer;
pub use self::event_buffer::EventBuffer;

mod event_stream;
pub use self::event_stream::EventStream;

mod fixed_event_buffer;
pub use self::fixed_event_buffer::{FixedSizeEventBuffer, FixedSizeEventBufferInner};

mod forwarder;
pub use self::forwarder::Forwarder;
