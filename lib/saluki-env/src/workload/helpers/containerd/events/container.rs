use super::{containerd_events, ContainerdEvent};

impl From<containerd_events::ContainerCreate> for ContainerdEvent {
    fn from(event: containerd_events::ContainerCreate) -> Self {
        ContainerdEvent::ContainerCreated { id: event.id }
    }
}

impl From<containerd_events::ContainerUpdate> for ContainerdEvent {
    fn from(event: containerd_events::ContainerUpdate) -> Self {
        ContainerdEvent::ContainerUpdated { id: event.id }
    }
}

impl From<containerd_events::ContainerDelete> for ContainerdEvent {
    fn from(event: containerd_events::ContainerDelete) -> Self {
        ContainerdEvent::ContainerDeleted { id: event.id }
    }
}
