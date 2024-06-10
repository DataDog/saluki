use containerd_client::services::v1::Envelope;
use prost::{DecodeError, Name};

mod containerd_events {
    pub use containerd_client::events::*;
}

/// Containerd event topics.
pub enum ContainerdTopic {
    /// Container task started.
    TaskStarted,

    /// Container task deleted.
    TaskDeleted,
}

impl ContainerdTopic {
    /// Returns the topic as a string.
    ///
    /// This can be used to match against the topic string present in the envelope.
    pub fn as_topic_str(&self) -> &'static str {
        match self {
            Self::TaskStarted => "/tasks/start",
            Self::TaskDeleted => "/tasks/delete",
        }
    }

    /// Attempts to parse a topic string into a `ContainerdTopic`.
    ///
    /// Returns `None` if the topic string is not recognized/supported.
    pub fn from_topic_str(topic: &str) -> Option<Self> {
        match topic {
            "/tasks/start" => Some(Self::TaskStarted),
            "/tasks/delete" => Some(Self::TaskDeleted),
            _ => None,
        }
    }
}

/// A containerd event.
///
/// This is a minimal unified representation of all envelope events that we have decoding support for. Generally, the
/// event includes only the necessary information to allow a subscriber to react to the event, such as the container ID
/// for any container-specific events, which then allows the subscriber to fetch additional information about the
/// container, and so on.
#[derive(Debug)]
pub enum ContainerdEvent {
    /// A task was started.
    TaskStarted { id: String, pid: u32 },

    /// A task was deleted.
    TaskDeleted { pid: u32 },
}

/// Decodes an event envelope into a minimal internal representation.
///
/// If the topic of the event is not recognized/supported, `Ok(None)` is returned.
///
/// ## Errors
///
/// If the envelope payload cannot be decoded, `Err` is returned with the decoder error.
pub fn decode_envelope_to_event(mut envelope: Envelope) -> Result<Option<ContainerdEvent>, DecodeError> {
    let topic = match ContainerdTopic::from_topic_str(envelope.topic.as_str()) {
        Some(topic) => topic,
        None => return Ok(None),
    };

    // Alter the envelope event's type URL, if present, to make `prost` happy.
    //
    // This boils down to the Protocol Buffer's specification around `Any`, and how it wants the "type URL" field -- the
    // field that declares the FQDN of the message type's Protocol Buffers definition, essentially -- to include at
    // least one forward slash.. but containerd doesn't send payloads with the type URL containing a forward slash at
    // all.. so `prost`, following the specification, generates the expected type URL as "/foo.bar" while te event has
    // "foo.bar" and decoding fails.
    if let Some(event) = &mut envelope.event {
        if !event.type_url.starts_with('/') {
            event.type_url.insert(0, '/');
        }
    }

    match topic {
        ContainerdTopic::TaskStarted => decode_envelope::<containerd_events::TaskStart>(envelope),
        ContainerdTopic::TaskDeleted => decode_envelope::<containerd_events::TaskDelete>(envelope),
    }
}

fn decode_envelope<D>(envelope: Envelope) -> Result<Option<ContainerdEvent>, DecodeError>
where
    D: Default + Name,
    ContainerdEvent: From<D>,
{
    match envelope.event {
        Some(event) => event.to_msg::<D>().map(|v| Some(v.into())),
        None => Ok(None),
    }
}

impl From<containerd_events::TaskStart> for ContainerdEvent {
    fn from(event: containerd_events::TaskStart) -> Self {
        ContainerdEvent::TaskStarted {
            id: event.container_id,
            pid: event.pid,
        }
    }
}

impl From<containerd_events::TaskDelete> for ContainerdEvent {
    fn from(event: containerd_events::TaskDelete) -> Self {
        ContainerdEvent::TaskDeleted { pid: event.pid }
    }
}
