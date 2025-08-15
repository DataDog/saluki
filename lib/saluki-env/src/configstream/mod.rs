//! Config stream.
//!
//! This modules provides the `ConfigStreamer` struct, which deals with streaming config events from the remote agent client.
mod stream;

pub use self::stream::ConfigStreamer;
