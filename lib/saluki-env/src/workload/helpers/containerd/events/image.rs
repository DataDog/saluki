use super::{containerd_events, ContainerdEvent};

impl From<containerd_events::ImageCreate> for ContainerdEvent {
    fn from(event: containerd_events::ImageCreate) -> Self {
        ContainerdEvent::ImageCreated { name: event.name }
    }
}

impl From<containerd_events::ImageUpdate> for ContainerdEvent {
    fn from(event: containerd_events::ImageUpdate) -> Self {
        ContainerdEvent::ImageUpdated { name: event.name }
    }
}

impl From<containerd_events::ImageDelete> for ContainerdEvent {
    fn from(event: containerd_events::ImageDelete) -> Self {
        ContainerdEvent::ImageDeleted { name: event.name }
    }
}
