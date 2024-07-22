/// ServiceCheckStatus is the status type for service checks
#[derive(Clone, Debug)]
pub enum ServiceCheckStatus {
    /// Ok is the "ok" ServiceCheck status
    Ok,

    /// Warn is the "warning" ServiceCheck status
    Warn,

    /// Critical is the "critical" ServiceCheck status
    Critical,

    /// Unknown is the "unknown" ServiceCheck status
    Unknown,
}
