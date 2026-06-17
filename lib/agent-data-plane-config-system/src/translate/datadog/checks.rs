//! Checks-domain translation: the checks IPC source endpoint.
//!
//! The original `ChecksIPCConfiguration` reads a dedicated `checks_ipc_endpoint` key, which is not
//! part of the supported Datadog witness set. None of the witnessed keys own the checks IPC
//! endpoint, so this module currently exposes no setters; the endpoint takes its seeded/default
//! value (`tcp://0.0.0.0:5105`). It exists to keep the module shape complete and to host a
//! destination should a checks key become witnessed.

#[cfg(test)]
mod tests {
    use saluki_component_config::checks::ChecksIPCConfig;
    use saluki_io::net::ListenAddress;

    #[test]
    fn default_endpoint_is_5105() {
        let c = ChecksIPCConfig::default();
        assert_eq!(c.grpc_endpoint, ListenAddress::any_tcp(5105));
    }
}
