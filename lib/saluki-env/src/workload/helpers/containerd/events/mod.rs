use containerd_client::services::v1::Envelope;
use prost::{DecodeError, Name};

mod container;
mod image;
mod task;

mod containerd_events {
    pub use containerd_client::events::*;
    pub use containerd_client::services::v1::{ImageCreate, ImageDelete, ImageUpdate};
}

/// Containerd event topics.
pub enum ContainerdTopic {
    /// Container created.
    ContainerCreated,

    /// Container updated.
    ContainerUpdated,

    /// Container deleted.
    ContainerDeleted,

    /// Container image created.
    ImageCreated,

    /// Container image updated.
    ImageUpdated,

    /// Container image deleted.
    ImageDeleted,

    /// Container task started.
    TaskStarted,

    /// Container task exited due to out of memory.
    TaskOutOfMemory,

    /// Container task exited.
    TaskExited,

    /// Container task deleted.
    TaskDeleted,

    /// Container task paused.
    TaskPaused,

    /// Container task resumed.
    TaskResumed,
}

impl ContainerdTopic {
    /// Returns the topic as a string.
    ///
    /// This can be used to match against the topic string present in the envelope.
    pub fn as_topic_str(&self) -> &'static str {
        match self {
            Self::ContainerCreated => "/containers/create",
            Self::ContainerUpdated => "/containers/update",
            Self::ContainerDeleted => "/containers/delete",
            Self::ImageCreated => "/images/create",
            Self::ImageUpdated => "/images/update",
            Self::ImageDeleted => "/images/delete",
            Self::TaskStarted => "/tasks/start",
            Self::TaskOutOfMemory => "/tasks/oom",
            Self::TaskExited => "/tasks/exit",
            Self::TaskDeleted => "/tasks/delete",
            Self::TaskPaused => "/tasks/paused",
            Self::TaskResumed => "/tasks/resumed",
        }
    }

    /// Attempts to parse a topic string into a `ContainerdTopic`.
    ///
    /// Returns `None` if the topic string is not recognized/supported.
    pub fn from_topic_str(topic: &str) -> Option<Self> {
        match topic {
            "/containers/create" => Some(Self::ContainerCreated),
            "/containers/update" => Some(Self::ContainerUpdated),
            "/containers/delete" => Some(Self::ContainerDeleted),
            "/images/create" => Some(Self::ImageCreated),
            "/images/update" => Some(Self::ImageUpdated),
            "/images/delete" => Some(Self::ImageDeleted),
            "/tasks/start" => Some(Self::TaskStarted),
            "/tasks/oom" => Some(Self::TaskOutOfMemory),
            "/tasks/exit" => Some(Self::TaskExited),
            "/tasks/delete" => Some(Self::TaskDeleted),
            "/tasks/paused" => Some(Self::TaskPaused),
            "/tasks/resumed" => Some(Self::TaskResumed),
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
    /// A container was created.
    ContainerCreated { id: String },

    /// A container was updated.
    ContainerUpdated { id: String },

    /// A container was deleted.
    ContainerDeleted { id: String },

    /// An image was created.
    ImageCreated { name: String },

    /// An image was updated.
    ImageUpdated { name: String },

    /// An image was deleted.
    ImageDeleted { name: String },

    /// A task was started.
    TaskStarted { id: String, pid: u32 },

    /// A task exited due to out of memory.
    TaskOutOfMemory { id: String },

    /// A task exited.
    TaskExited { id: String, pid: u32, exit_status: u32 },

    /// A task was deleted.
    TaskDeleted { id: String, pid: u32, exit_status: u32 },

    /// A task was paused.
    TaskPaused { id: String },

    /// A task was resumed.
    TaskResumed { id: String },
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
    // field that declares to the FQDN of the message type, essentially -- to include at least one forward slash.. but
    // containerd doesn't send payloads with the type URL containing a forward slash at all.. so `prost`, following the
    // specification, generates the expected type URL as "/foo.bar" while te event has "foo.bar" and decoding fails.
    if let Some(event) = &mut envelope.event {
        if !event.type_url.starts_with('/') {
            event.type_url.insert(0, '/');
        }
    }

    match topic {
        ContainerdTopic::ContainerCreated => decode_envelope::<containerd_events::ContainerCreate>(envelope),
        ContainerdTopic::ContainerUpdated => decode_envelope::<containerd_events::ContainerUpdate>(envelope),
        ContainerdTopic::ContainerDeleted => decode_envelope::<containerd_events::ContainerDelete>(envelope),
        ContainerdTopic::ImageCreated => decode_envelope::<containerd_events::ImageCreate>(envelope),
        ContainerdTopic::ImageUpdated => decode_envelope::<containerd_events::ImageUpdate>(envelope),
        ContainerdTopic::ImageDeleted => decode_envelope::<containerd_events::ImageDelete>(envelope),
        ContainerdTopic::TaskStarted => decode_envelope::<containerd_events::TaskStart>(envelope),
        ContainerdTopic::TaskOutOfMemory => decode_envelope::<containerd_events::TaskOom>(envelope),
        ContainerdTopic::TaskExited => decode_envelope::<containerd_events::TaskExit>(envelope),
        ContainerdTopic::TaskDeleted => decode_envelope::<containerd_events::TaskDelete>(envelope),
        ContainerdTopic::TaskPaused => decode_envelope::<containerd_events::TaskPaused>(envelope),
        ContainerdTopic::TaskResumed => decode_envelope::<containerd_events::TaskResumed>(envelope),
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
