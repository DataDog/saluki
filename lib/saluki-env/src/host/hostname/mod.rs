use std::sync::OnceLock;

use async_trait::async_trait;
use regex::Regex;

mod os;
pub use self::os::OperatingSystemHostnameProvider;

mod maybe_file;
pub use self::maybe_file::MaybeFileHostnameProvider;

mod kubernetes;
pub use self::kubernetes::KubernetesHostnameProvider;

mod maybe_static;
pub use self::maybe_static::MaybeStaticHostnameProvider;
mod util;

const HOSTNAME_MAX_LENGTH: usize = 255;

/// Provides the hostname of the host.
#[async_trait]
pub trait HostnameProvider {
    // TODO: The Agent passes whatever the latest detected hostname is so far to each equivalent call of our
    // `get_hostname` method, but it only ever uses it for the EC2 provider, which doesn't actually use the value itself
    // but uses it somewhat to drive further hostname detection logic.
    //
    // We probably need to do the same here, but we _should_ be OK for now I _think_.
    async fn get_hostname(&self) -> Option<String>;
}

/// Checks that the given hostname is valid.
///
/// The hostname must satisfy the following conditions:
///
/// - hostname is not empty
/// - hostname is not a local hostname
/// - hostname is not longer than 255 characters
/// - hostname is [RFC1123][rfc1123] compliant
///
/// [rfc1123]: https://tools.ietf.org/html/rfc1123
pub fn validate_hostname(hostname: &str) -> Result<(), String> {
    if hostname.is_empty() {
        return Err("hostname is empty".to_string());
    }

    if is_local_hostname(hostname) {
        return Err(format!("{} is a local hostname", hostname));
    }

    if hostname.len() > HOSTNAME_MAX_LENGTH {
        return Err(format!(
            "name exceeded the maximum length of {} characters",
            HOSTNAME_MAX_LENGTH
        ));
    }

    if !is_rfc1123_compliant_hostname(hostname) {
        return Err(format!("{} is not RFC1123 compliant", hostname));
    }

    Ok(())
}

/// Checks if the given hostname is a local hostname.
///
/// Local hostnames are hostnames that are commonly used to refer to the local machine, and are generally mapped by
/// default to the loopback addresses (127.0.0.1 and ::1).
pub fn is_local_hostname(hostname: &str) -> bool {
    static LOCAL_HOSTNAME_IDS: [&str; 4] = [
        "localhost",
        "localhost.localdomain",
        "localhost6.localdomain6",
        "ip6-localhost",
    ];

    let hostname_lower = hostname.to_lowercase();
    for local_hostname in LOCAL_HOSTNAME_IDS.iter() {
        if hostname_lower.as_str() == *local_hostname {
            return true;
        }
    }

    false
}

/// Checks if the given hostname is [RFC1123][rfc1123] compliant.
///
/// [rfc1123]: https://tools.ietf.org/html/rfc1123
pub fn is_rfc1123_compliant_hostname(hostname: &str) -> bool {
    static HOSTNAME_REGEX: OnceLock<Regex> = OnceLock::new();

    let regex = HOSTNAME_REGEX.get_or_init(|| Regex::new(r"^(([a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9\-]*[a-zA-Z0-9])\.)*([A-Za-z0-9]|[A-Za-z0-9][A-Za-z0-9\-]*[A-Za-z0-9])$").unwrap());

    regex.is_match(hostname)
}
