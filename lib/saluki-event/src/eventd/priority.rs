/// Event priority
#[derive(Clone, Debug)]
pub enum EventPriority {
    /// The event has normal priority.
    Normal,

    /// The event has low priority.
    Low,
}
