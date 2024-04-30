use super::{containerd_events, ContainerdEvent};

impl From<containerd_events::TaskStart> for ContainerdEvent {
    fn from(event: containerd_events::TaskStart) -> Self {
        ContainerdEvent::TaskStarted {
            id: event.container_id,
            pid: event.pid,
        }
    }
}

impl From<containerd_events::TaskOom> for ContainerdEvent {
    fn from(event: containerd_events::TaskOom) -> Self {
        ContainerdEvent::TaskOutOfMemory { id: event.container_id }
    }
}

impl From<containerd_events::TaskExit> for ContainerdEvent {
    fn from(event: containerd_events::TaskExit) -> Self {
        ContainerdEvent::TaskExited {
            id: event.container_id,
            pid: event.pid,
            exit_status: event.exit_status,
        }
    }
}

impl From<containerd_events::TaskDelete> for ContainerdEvent {
    fn from(event: containerd_events::TaskDelete) -> Self {
        ContainerdEvent::TaskDeleted {
            id: event.container_id,
            pid: event.pid,
            exit_status: event.exit_status,
        }
    }
}

impl From<containerd_events::TaskPaused> for ContainerdEvent {
    fn from(event: containerd_events::TaskPaused) -> Self {
        ContainerdEvent::TaskPaused { id: event.container_id }
    }
}

impl From<containerd_events::TaskResumed> for ContainerdEvent {
    fn from(event: containerd_events::TaskResumed) -> Self {
        ContainerdEvent::TaskResumed { id: event.container_id }
    }
}
