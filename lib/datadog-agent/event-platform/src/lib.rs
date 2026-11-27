//! Event platform events, and the tracks of the Datadog intake that carry them.
//!
//! The event platform intake receives payloads on tracks, such as `dbm-samples` for the query samples of Database
//! Monitoring. The Datadog Agent forwards each payload without reading it: most tracks carry JSON, and some carry
//! protobuf.
//!
//! An [`EventPlatformEvent`] carries one payload across a topology, or from one process to another, in an
//! [`Opaque`](saluki_common::opaque::Opaque) value. [`TRACKS`] describes each track as the Datadog Agent does. The
//! producers and the senders of these events share this crate, so that they agree on the encoding and on the tracks.

#![deny(missing_docs)]

mod event;
pub use self::event::EventPlatformEvent;

mod track;
pub use self::track::{ContentType, Origin, Strategy, Track, TRACKS};
