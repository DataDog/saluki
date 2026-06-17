//! Workload-domain translation: the CRI client timeouts and the remote-agent string interner
//! budget.
//!
//! Mirrors the conversions the original workload/environment provider construction performed. The
//! hostname and host-tag timing are environment-derived or Saluki-only and are not witnessed.

use saluki_component_config::workload::WorkloadConfig;

/// `cri_connection_timeout` (seconds) -> CRI client connection timeout.
pub fn set_cri_connection_timeout_secs(config: &mut WorkloadConfig, value: i64) {
    config.cri_connection_timeout_secs = value.max(0) as u64;
}

/// `cri_query_timeout` (seconds) -> CRI client query timeout.
pub fn set_cri_query_timeout_secs(config: &mut WorkloadConfig, value: i64) {
    config.cri_query_timeout_secs = value.max(0) as u64;
}

/// `agent_ipc.grpc_max_message_size` (bytes) -> remote-agent string interner budget.
///
/// Mirrors the original, which sized the remote-agent workload provider's string interner from the
/// configured IPC gRPC message size budget.
pub fn set_remote_agent_string_interner_size_bytes(config: &mut WorkloadConfig, value: i64) {
    config.remote_agent_string_interner_size_bytes = value.max(0) as usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cri_timeouts_cast_to_u64() {
        let mut c = WorkloadConfig::default();
        set_cri_connection_timeout_secs(&mut c, 3);
        set_cri_query_timeout_secs(&mut c, 9);
        assert_eq!(c.cri_connection_timeout_secs, 3);
        assert_eq!(c.cri_query_timeout_secs, 9);
    }
}
