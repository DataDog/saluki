/// Event alert type
#[derive(Clone, Debug)]
pub enum EventAlertType {
    /// Indicates an informational event.
    Info,

    /// Indicates an error event.
    Error,

    /// Indicates a warning event.
    Warning,

    /// Indicates a successful event.
    Success,
}
