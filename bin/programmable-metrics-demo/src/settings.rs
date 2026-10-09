//! Ports, recipients, and other settings shared by the demo's components.
//!
//! Everything the demo listens on or sends to is on the loopback interface. The defaults match the demo section of the
//! implementation design (`docs/design/programmable-metrics-implementation.md`), and every port can be changed on the
//! command line.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::Serialize;

/// The address every demo listener binds and every demo client connects to.
pub const LOOPBACK: [u8; 4] = [127, 0, 0, 1];

/// The ports the demo and ADP use.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DemoPorts {
    /// The demo UI and its JSON API.
    pub ui: u16,

    /// The artifact channel stub that ADP polls.
    pub stub: u16,

    /// The recording intake for the primary endpoint (`dd_url`).
    pub primary: u16,

    /// The recording intake for the first additional endpoint (team B).
    pub team_b: u16,

    /// The recording intake for the second additional endpoint (team C).
    pub team_c: u16,

    /// The recording intake for the multi-region failover endpoint.
    pub mrf: u16,

    /// ADP's unprivileged API (`/ready`, `/metrics`).
    pub api: u16,

    /// ADP's privileged API (mTLS with the IPC certificate).
    pub secure_api: u16,

    /// ADP's DogStatsD UDP port.
    pub dogstatsd: u16,
}

impl Default for DemoPorts {
    fn default() -> Self {
        Self {
            ui: 17080,
            stub: 17100,
            primary: 17101,
            team_b: 17102,
            team_c: 17103,
            mrf: 17104,
            api: 17110,
            secure_api: 17111,
            dogstatsd: 17125,
        }
    }
}

impl DemoPorts {
    /// Returns the loopback socket address of the given port.
    pub fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((LOOPBACK, port))
    }

    /// Returns the URL of a loopback HTTP listener on the given port, as ADP's configuration writes endpoint URLs.
    pub fn http_url(port: u16) -> String {
        format!("http://127.0.0.1:{}", port)
    }

    /// Returns the recipients the demo records, in the order the UI shows them.
    pub fn recipients(&self) -> Vec<Recipient> {
        vec![
            Recipient {
                name: "primary",
                label: "Primary",
                port: self.primary,
                api_key: "demo-primary",
            },
            Recipient {
                name: "team-b",
                label: "Team B",
                port: self.team_b,
                api_key: "demo-team-b",
            },
            Recipient {
                name: "team-c",
                label: "Team C",
                port: self.team_c,
                api_key: "demo-team-c",
            },
            Recipient {
                name: "mrf",
                label: "Failover (MRF)",
                port: self.mrf,
                api_key: "demo-mrf",
            },
        ]
    }
}

/// One endpoint ADP sends metrics to, and the recording intake that stands in for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recipient {
    /// The short name the UI and API use.
    pub name: &'static str,

    /// A display label.
    pub label: &'static str,

    /// The loopback port the recording intake listens on.
    pub port: u16,

    /// The API key ADP is configured to send to this endpoint.
    pub api_key: &'static str,
}

impl Recipient {
    /// Returns the endpoint URL as ADP's configuration writes it.
    pub fn url(&self) -> String {
        DemoPorts::http_url(self.port)
    }
}

/// Settings for one demo run.
#[derive(Clone, Debug)]
pub struct RunSettings {
    /// The ports.
    pub ports: DemoPorts,

    /// The run directory: generated certificate and configuration, ADP's logs, and the artifact store.
    pub run_dir: PathBuf,

    /// How often ADP polls the artifact channel stub.
    pub poll_interval: Duration,

    /// The aggregation window ADP uses, in seconds.
    pub aggregate_window_secs: u64,

    /// How long ADP keeps sending zeros for a counter that stopped receiving updates, in seconds (`0`: not at all).
    pub counter_expiry_secs: u64,

    /// The DogStatsD datagram rate the traffic generator starts with, in datagrams per second.
    pub traffic_rate: f64,

    /// Whether the traffic generator starts paused.
    pub traffic_paused: bool,
}

impl RunSettings {
    /// Returns the path of the IPC certificate and key (one PEM file) in the run directory.
    pub fn ipc_cert_path(&self) -> PathBuf {
        self.run_dir.join("ipc_cert.pem")
    }

    /// Returns the path of the generated ADP configuration file in the run directory.
    pub fn adp_config_path(&self) -> PathBuf {
        self.run_dir.join("datadog.yaml")
    }

    /// Returns the path ADP writes its log file to.
    pub fn adp_log_path(&self) -> PathBuf {
        self.run_dir.join("adp.log")
    }

    /// Returns the path the launcher writes ADP's standard output and error to.
    pub fn adp_console_log_path(&self) -> PathBuf {
        self.run_dir.join("adp.console.log")
    }

    /// Returns the directory of the durable artifact store.
    pub fn store_dir(&self) -> PathBuf {
        self.run_dir.join("channel")
    }
}
