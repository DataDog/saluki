//! APM domain: the Datadog v1.0 (`idx`/ETP) trace receiver.
//!
//! This domain covers ingress only: where the receiver listens, how large a request it accepts, and
//! whether it may bind a non-loopback address. Trace *processing* (obfuscation, sampling, stats) is
//! shared with the OTLP path and lives in the [`traces`](super::traces) domain.
//!
//! Every key here is Saluki-only. The Datadog schema's `apm_config.receiver_port`,
//! `apm_config.receiver_socket`, `apm_config.max_payload_size`, and
//! `apm_config.apm_non_local_traffic` all sit in the excluded block of the overlay: they configure
//! the Core Agent trace-agent's receiver, which keeps running alongside ADP, so reusing them would
//! create a duplicate source of truth for two listeners that must not collide.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Serialize;

use crate::defaults::{
    DEFAULT_APM_DISPATCH_TIMEOUT, DEFAULT_APM_MAX_PAYLOAD_SIZE, DEFAULT_APM_NON_LOCAL_TRAFFIC,
    DEFAULT_APM_RECEIVER_ENDPOINT,
};
use crate::Error;

/// Host name that always denotes a loopback address, whichever family it resolves to.
const LOCALHOST: &str = "localhost";

/// Resolved APM configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Domain {
    /// TCP address the v1.0 trace receiver listens on, as `host:port`.
    ///
    /// Defaults to `localhost:8127`. An empty value disables the TCP listener; when the Unix domain
    /// socket is also disabled, enabling the APM pipeline is an error rather than a silent no-op.
    /// Binding a non-loopback address additionally requires
    /// [`non_local_traffic`](Self::non_local_traffic).
    pub receiver_endpoint: String,

    /// Unix domain socket path the v1.0 trace receiver listens on.
    ///
    /// Defaults to empty, which disables the Unix domain socket listener. Set this for deployments
    /// where tracers share a filesystem with ADP but not a network namespace. Not subject to
    /// [`non_local_traffic`](Self::non_local_traffic), which governs TCP only.
    pub receiver_socket: String,

    /// Maximum accepted v1.0 trace request body size, in bytes.
    ///
    /// Defaults to 25 MB, matching the reference trace-agent. Requests over this size are rejected
    /// before any decoding, so this bounds peak memory per in-flight request. A value of `0` rejects
    /// every request and is not a way to disable the limit.
    pub max_payload_size: usize,

    /// Whether the v1.0 trace receiver may bind a non-loopback TCP address.
    ///
    /// Defaults to `false`: a [`receiver_endpoint`](Self::receiver_endpoint) whose host is not a
    /// loopback address is rejected at startup rather than quietly exposing the receiver. Set this to
    /// `true` when tracers run outside ADP's network namespace, such as application containers
    /// reaching an agent container, and the port is not reachable from untrusted networks.
    pub non_local_traffic: bool,

    /// How long the receiver waits for the pipeline to accept a payload before refusing it.
    ///
    /// Defaults to 1 second, matching the reference trace-agent's `apm_config.decoder_timeout`. The
    /// receiver waits this long for memory-limiter capacity and for room in the queue feeding the
    /// decoder; past it, the payload is dropped and the tracer gets a refusal, so a stalled pipeline
    /// sheds load instead of holding tracer connections open indefinitely.
    ///
    /// Raise this to tolerate longer downstream stalls at the cost of slower refusals; lower it to
    /// shed load sooner. A value of `0` refuses any payload the pipeline cannot accept immediately.
    pub dispatch_timeout: Duration,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            receiver_endpoint: DEFAULT_APM_RECEIVER_ENDPOINT.to_string(),
            receiver_socket: String::new(),
            max_payload_size: DEFAULT_APM_MAX_PAYLOAD_SIZE,
            non_local_traffic: DEFAULT_APM_NON_LOCAL_TRAFFIC,
            dispatch_timeout: DEFAULT_APM_DISPATCH_TIMEOUT,
        }
    }
}

impl Domain {
    /// Validates [`receiver_endpoint`](Self::receiver_endpoint) against the non-local traffic gate.
    ///
    /// A disabled TCP listener, or an enabled [`non_local_traffic`](Self::non_local_traffic), permits
    /// anything. Otherwise the endpoint's host must be a loopback address.
    ///
    /// Only the host is examined. A host that is neither `localhost` nor a literal IP address cannot
    /// be shown to be loopback without resolving it, which startup validation does not do, so such a
    /// host is rejected while the gate is off.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP listener is enabled, `non_local_traffic` is `false`, and the
    /// configured host is not a loopback address.
    pub fn validate_receiver_endpoint(&self) -> Result<(), Error> {
        if self.non_local_traffic || self.receiver_endpoint.is_empty() {
            return Ok(());
        }

        if endpoint_host_is_loopback(&self.receiver_endpoint) {
            return Ok(());
        }

        Err(Error::new_without_source(format!(
            "`data_plane.apm.receiver_endpoint` is `{}`, which is not a loopback address, but \
             `data_plane.apm.non_local_traffic` is not set. Either bind a loopback address such as \
             `{DEFAULT_APM_RECEIVER_ENDPOINT}`, or set `data_plane.apm.non_local_traffic` to `true` to accept \
             traffic from outside this host.",
            self.receiver_endpoint
        )))
    }
}

/// Returns whether the host portion of a `host:port` endpoint denotes a loopback address.
///
/// A bare IP literal (`127.0.0.1:8127`, `[::1]:8127`) and the name `localhost` are loopback. An empty
/// host (`:8127`), a wildcard (`0.0.0.0:8127`), and any other name are not.
fn endpoint_host_is_loopback(endpoint: &str) -> bool {
    // A full socket address parses directly, which also handles the bracketed IPv6 form.
    if let Ok(address) = endpoint.parse::<SocketAddr>() {
        return address.ip().is_loopback();
    }

    let Some(host) = endpoint_host(endpoint) else {
        return false;
    };

    if host.eq_ignore_ascii_case(LOCALHOST) {
        return true;
    }

    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Extracts the host portion of a `host:port` endpoint, unwrapping a bracketed IPv6 literal.
///
/// Returns `None` when the endpoint carries no host at all.
fn endpoint_host(endpoint: &str) -> Option<&str> {
    let host = if let Some(rest) = endpoint.strip_prefix('[') {
        // Bracketed IPv6 literal: the host runs to the closing bracket, whatever follows it.
        rest.split(']').next()?
    } else {
        // Split from the right so an unbracketed IPv6 literal, which is full of colons, is not
        // mistaken for a `host:port` pair and truncated.
        match endpoint.rsplit_once(':') {
            Some((host, _port)) => host,
            None => endpoint,
        }
    };

    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::{endpoint_host_is_loopback, Domain};
    use crate::defaults::DEFAULT_APM_RECEIVER_ENDPOINT;

    #[test]
    fn the_default_endpoint_is_loopback() {
        // The gate defaults to off, so the default endpoint has to pass its own validation.
        assert!(endpoint_host_is_loopback(DEFAULT_APM_RECEIVER_ENDPOINT));
        assert!(Domain::default().validate_receiver_endpoint().is_ok());
    }

    #[test]
    fn loopback_hosts_are_recognized() {
        for endpoint in [
            "localhost:8127",
            "LocalHost:8127",
            "127.0.0.1:8127",
            "127.9.9.9:8127",
            "[::1]:8127",
            "::1:8127",
        ] {
            assert!(endpoint_host_is_loopback(endpoint), "expected loopback: {endpoint:?}");
        }
    }

    #[test]
    fn non_loopback_hosts_are_rejected() {
        // An unresolvable-at-startup name is treated as non-loopback: validation does not resolve.
        for endpoint in [
            "0.0.0.0:8127",
            "[::]:8127",
            ":8127",
            "10.0.0.5:8127",
            "trace-agent.internal:8127",
        ] {
            assert!(
                !endpoint_host_is_loopback(endpoint),
                "expected non-loopback: {endpoint:?}"
            );
        }
    }

    #[test]
    fn a_non_loopback_endpoint_needs_the_non_local_traffic_gate() {
        let mut domain = Domain {
            receiver_endpoint: "0.0.0.0:8127".to_string(),
            ..Default::default()
        };

        let error = domain
            .validate_receiver_endpoint()
            .expect_err("a wildcard bind must be rejected while the gate is off");
        assert!(error.to_string().contains("non_local_traffic"));

        domain.non_local_traffic = true;
        assert!(domain.validate_receiver_endpoint().is_ok());
    }

    #[test]
    fn a_disabled_tcp_listener_is_always_valid() {
        // Nothing is bound, so the gate has nothing to protect.
        let domain = Domain {
            receiver_endpoint: String::new(),
            receiver_socket: "/var/run/datadog/apm.socket".to_string(),
            ..Default::default()
        };

        assert!(domain.validate_receiver_endpoint().is_ok());
    }
}
