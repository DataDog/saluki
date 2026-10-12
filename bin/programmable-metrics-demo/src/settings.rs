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

    /// The recording intake for team D's endpoint, which only a resources artifact declares.
    pub team_d: u16,

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
            team_d: 17105,
            api: 17110,
            secure_api: 17111,
            dogstatsd: 17125,
        }
    }
}

impl DemoPorts {
    /// Returns the default ports, each moved up by `offset`, or `None` if one would pass 65535.
    pub fn offset_from_default(offset: u16) -> Option<Self> {
        let ports = Self::default();
        let up = |port: u16| port.checked_add(offset);
        Some(Self {
            ui: up(ports.ui)?,
            stub: up(ports.stub)?,
            primary: up(ports.primary)?,
            team_b: up(ports.team_b)?,
            team_c: up(ports.team_c)?,
            mrf: up(ports.mrf)?,
            team_d: up(ports.team_d)?,
            api: up(ports.api)?,
            secure_api: up(ports.secure_api)?,
            dogstatsd: up(ports.dogstatsd)?,
        })
    }

    /// Returns the loopback socket address of the given port.
    pub fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((LOOPBACK, port))
    }

    /// Returns the URL of a loopback HTTP listener on the given port, as ADP's configuration writes endpoint URLs.
    pub fn http_url(port: u16) -> String {
        format!("http://127.0.0.1:{}", port)
    }

    /// Returns the recipients the demo records, in the order the UI shows them: the four ADP's configuration names,
    /// then team D, which only a resources artifact declares.
    pub fn recipients(&self) -> Vec<Recipient> {
        let configured = |name, label, port, api_key| Recipient {
            name,
            label,
            port,
            api_key,
            declared: false,
        };
        vec![
            configured("primary", "Primary", self.primary, "demo-primary"),
            configured("team-b", "Team B", self.team_b, "demo-team-b"),
            configured("team-c", "Team C", self.team_c, "demo-team-c"),
            configured("mrf", "Failover (MRF)", self.mrf, "demo-mrf"),
            Recipient {
                name: "team-d",
                label: "Team D",
                port: self.team_d,
                api_key: TEAM_D_CREDENTIAL_VALUE,
                declared: true,
            },
        ]
    }
}

/// The name of the credential the launcher declares for resources artifacts, which the team D template refers to as
/// `credentials/team_d`.
pub const TEAM_D_CREDENTIAL: &str = "team_d";

/// The value of [`TEAM_D_CREDENTIAL`]: the API key ADP sends to team D's endpoint once a resources artifact declares
/// it.
pub const TEAM_D_CREDENTIAL_VALUE: &str = "demo-team-d-key";

/// One endpoint ADP sends metrics to, and the recording intake that stands in for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recipient {
    /// The short name the UI and API use.
    pub name: &'static str,

    /// A display label.
    pub label: &'static str,

    /// The loopback port the recording intake listens on.
    pub port: u16,

    /// The API key ADP is configured to send to this endpoint: for a declared recipient, the value of the credential
    /// its resources artifact names.
    pub api_key: &'static str,

    /// Whether a resources artifact declares the recipient, rather than ADP's configuration file: ADP sends it nothing
    /// until one does.
    pub declared: bool,
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
