/// Service status
#[derive(Clone, Debug)]
pub enum ServiceCheckStatus {
    /// The service is operating normally.
    Ok,

    /// The service is in a warning state.
    Warn,

    /// The service is in a critical state.
    Critical,

    /// The service is in an unknown state.
    Unknown,
}
