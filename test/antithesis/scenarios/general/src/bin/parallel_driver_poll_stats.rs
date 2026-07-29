//! Antithesis parallel driver that polls ADP's `/dogstatsd/stats` privileged
//! endpoint. Sends two overlapping `GET /dogstatsd/stats` requests to the
//! privileged API at `:5101` over HTTPS with the shared IPC mTLS identity. One
//! request runs on a background thread. A second follows after a short delay
//! while the first is still outstanding.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use antithesis_sdk::prelude::*;
use clap::Parser;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "parallel_driver_poll_stats")]
struct Config {
    #[arg(
        long = "adp-secure-api-addr",
        env = "ADP_SECURE_API_ADDR",
        default_value = "agent:5101"
    )]
    adp_secure_api_addr: String,
    /// Combined certificate and private key PEM used for IPC mTLS.
    #[arg(
        long = "ipc-cert-path",
        env = "IPC_CERT_PATH",
        default_value = "/ipc-auth/ipc_cert.pem"
    )]
    ipc_cert_path: PathBuf,
    /// Collection window each request asks the SUT to hold, in seconds.
    #[arg(long = "collection-secs", env = "STATS_COLLECTION_SECS", default_value_t = 5)]
    collection_secs: u64,
}

/// Sends one `GET /dogstatsd/stats` and returns the HTTP status, or `None` on a
/// transport error.
fn poll(client: &reqwest::blocking::Client, addr: &str, collection_secs: u64) -> Option<u16> {
    let url = format!("https://{addr}/dogstatsd/stats?collection_duration_secs={collection_secs}");
    client.get(&url).send().ok().map(|resp| resp.status().as_u16())
}

fn build_client(ipc_cert_path: &Path, timeout: Duration) -> anyhow::Result<reqwest::blocking::Client> {
    let identity_pem = fs::read(ipc_cert_path)?;
    let identity = reqwest::Identity::from_pem(&identity_pem)?;

    // The Antithesis client reaches the generated test certificate through the cross-container `agent` hostname,
    // which is not guaranteed to appear in its SANs. Server-certificate validation is disabled only for this test
    // client; the privileged server still authenticates the client by exact DER equality with this shared identity.
    Ok(reqwest::blocking::Client::builder()
        .identity(identity)
        .danger_accept_invalid_certs(true)
        .timeout(timeout)
        .build()?)
}

fn client_build_succeeded(client: &anyhow::Result<reqwest::blocking::Client>) -> bool {
    client.is_ok()
}

fn completed_statuses(opener_status: Option<u16>, overlap_status: Option<u16>) -> Option<(u16, u16)> {
    opener_status.zip(overlap_status)
}

fn main() -> anyhow::Result<()> {
    antithesis_init();

    // reqwest is built without selecting a crypto provider, so install the
    // process-wide one before any Rustls client configuration is built.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let config = Config::try_parse()?;

    let client = build_client(&config.ipc_cert_path, Duration::from_secs(config.collection_secs + 10));
    let client_ready = client_build_succeeded(&client);
    let ipc_cert_file = config
        .ipc_cert_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>");
    assert_sometimes!(
        client_ready,
        "workload loaded the IPC identity and built an authenticated client",
        &json!({ "client_ready": client_ready, "ipc_cert_file": ipc_cert_file })
    );
    let Ok(client) = client else {
        return Ok(());
    };

    let addr = config.adp_secure_api_addr;
    let secs = config.collection_secs;

    // Send the first request from a background thread, then a second after a short delay.
    let opener_client = client.clone();
    let opener_addr = addr.clone();
    let opener = thread::spawn(move || poll(&opener_client, &opener_addr, secs));

    thread::sleep(Duration::from_millis(250));
    let overlap_status = poll(&client, &addr, secs);

    let opener_status = opener.join().unwrap_or(None);
    let opener_reached_http = opener_status.is_some();
    let overlap_reached_http = overlap_status.is_some();
    let statuses = completed_statuses(opener_status, overlap_status);
    assert_sometimes!(
        statuses.is_some(),
        "both overlapping privileged requests completed TLS and HTTP transport",
        &json!({
            "opener_reached_http": opener_reached_http,
            "overlap_reached_http": overlap_reached_http,
        })
    );
    let Some((opener_status, overlap_status)) = statuses else {
        return Ok(());
    };

    assert_reachable!(
        "workload polled dogstatsd stats with overlapping requests",
        &json!({
            "adp_secure_api_addr": addr,
            "opener_status": opener_status,
            "overlap_status": overlap_status,
        })
    );

    assert_sometimes!(
        opener_status == 429 || overlap_status == 429,
        "workload overlapped an active dogstatsd stats collection",
        &json!({ "opener_status": opener_status, "overlap_status": overlap_status })
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    use clap::Parser as _;
    use rcgen::{generate_simple_self_signed, CertifiedKey};

    use super::{build_client, client_build_succeeded, completed_statuses, Config};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(value: Option<&Path>) -> Self {
            let previous = env::var_os("IPC_CERT_PATH");
            match value {
                Some(path) => env::set_var("IPC_CERT_PATH", path),
                None => env::remove_var("IPC_CERT_PATH"),
            }
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var("IPC_CERT_PATH", value),
                None => env::remove_var("IPC_CERT_PATH"),
            }
        }
    }

    fn install_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    fn write_identity(path: &Path) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_owned()]).expect("self-signed identity should be generated");
        fs::write(path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
            .expect("combined identity PEM should be written");
    }

    #[test]
    fn build_client_accepts_combined_certificate_and_key_pem() {
        install_crypto_provider();
        let temp_dir = tempfile::tempdir().expect("temporary identity directory should be created");
        let identity_path = temp_dir.path().join("ipc_cert.pem");
        write_identity(&identity_path);

        build_client(&identity_path, Duration::from_secs(1))
            .expect("client should be built from a combined certificate and key PEM");
    }

    #[test]
    fn build_client_rejects_invalid_identity_pem() {
        install_crypto_provider();
        let temp_dir = tempfile::tempdir().expect("temporary identity directory should be created");
        let identity_path = temp_dir.path().join("ipc_cert.pem");
        fs::write(&identity_path, b"not an identity").expect("invalid identity fixture should be written");

        assert!(
            build_client(&identity_path, Duration::from_secs(1)).is_err(),
            "invalid identity PEM must not build a client"
        );
    }

    #[test]
    fn build_client_rejects_missing_identity_file() {
        install_crypto_provider();
        let temp_dir = tempfile::tempdir().expect("temporary identity directory should be created");

        assert!(
            build_client(&temp_dir.path().join("missing.pem"), Duration::from_secs(1)).is_err(),
            "a missing identity file must not build a client"
        );
    }

    #[test]
    fn client_readiness_requires_a_successful_authenticated_client_build() {
        install_crypto_provider();
        let temp_dir = tempfile::tempdir().expect("temporary identity directory should be created");
        let identity_path = temp_dir.path().join("ipc_cert.pem");
        write_identity(&identity_path);

        let valid_client = build_client(&identity_path, Duration::from_secs(1));
        assert!(client_build_succeeded(&valid_client));

        let missing_client = build_client(&temp_dir.path().join("missing.pem"), Duration::from_secs(1));
        assert!(!client_build_succeeded(&missing_client));
    }

    #[test]
    fn completed_statuses_requires_both_requests_to_reach_http() {
        assert_eq!(completed_statuses(Some(200), Some(429)), Some((200, 429)));
        assert_eq!(completed_statuses(Some(200), None), None);
        assert_eq!(completed_statuses(None, Some(429)), None);
        assert_eq!(completed_statuses(None, None), None);
    }

    #[test]
    fn ipc_cert_path_defaults_to_read_only_ipc_auth_volume() {
        let _lock = ENV_LOCK.lock().expect("environment lock should not be poisoned");
        let _env = EnvGuard::set(None);

        let config = Config::try_parse_from(["parallel_driver_poll_stats"]).expect("default config should parse");

        assert_eq!(config.ipc_cert_path, PathBuf::from("/ipc-auth/ipc_cert.pem"));
    }

    #[test]
    fn ipc_cert_path_uses_environment_value() {
        let _lock = ENV_LOCK.lock().expect("environment lock should not be poisoned");
        let expected = Path::new("/tmp/from-env.pem");
        let _env = EnvGuard::set(Some(expected));

        let config = Config::try_parse_from(["parallel_driver_poll_stats"]).expect("environment config should parse");

        assert_eq!(config.ipc_cert_path, expected);
    }

    #[test]
    fn explicit_ipc_cert_path_overrides_environment_value() {
        let _lock = ENV_LOCK.lock().expect("environment lock should not be poisoned");
        let _env = EnvGuard::set(Some(Path::new("/tmp/from-env.pem")));

        let config = Config::try_parse_from(["parallel_driver_poll_stats", "--ipc-cert-path", "/tmp/from-flag.pem"])
            .expect("explicit config should parse");

        assert_eq!(config.ipc_cert_path, PathBuf::from("/tmp/from-flag.pem"));
    }
}
