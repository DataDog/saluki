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

    /// Returns the recipients ADP's configuration names at boot, in the order the UI shows them: the primary endpoint,
    /// teams B and C, which are additional endpoints, and the multi-region failover endpoint. Destinations added while
    /// the demo runs ([`Recipient::added`]) follow them.
    pub fn recipients(&self) -> Vec<Recipient> {
        let recipient = |name: &str, label: &str, port, api_key: &str, kind| Recipient {
            name: name.to_string(),
            label: label.to_string(),
            port,
            api_key: api_key.to_string(),
            kind,
            added: false,
        };
        vec![
            recipient(
                "primary",
                "Primary",
                self.primary,
                "demo-primary",
                RecipientKind::Primary,
            ),
            recipient(
                "team-b",
                "Team B",
                self.team_b,
                "demo-team-b",
                RecipientKind::Additional,
            ),
            recipient(
                "team-c",
                "Team C",
                self.team_c,
                "demo-team-c",
                RecipientKind::Additional,
            ),
            recipient("mrf", "Failover (MRF)", self.mrf, "demo-mrf", RecipientKind::Failover),
        ]
    }
}

/// What kind of endpoint a recipient is in ADP's configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipientKind {
    /// The primary endpoint, `dd_url`.
    Primary,

    /// An additional endpoint, under `additional_endpoints`: natively, it receives everything the primary endpoint
    /// does (dual shipping).
    Additional,

    /// The multi-region failover endpoint, `multi_region_failover.dd_url`.
    Failover,
}

/// One endpoint ADP sends metrics to, and the recording intake that stands in for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recipient {
    /// The short name the UI and API use, such as `team-b` or `dest-d`.
    pub name: String,

    /// A display label, such as `Team B` or `Destination D`. ADP never sees it: it knows an additional endpoint by its
    /// URL only.
    pub label: String,

    /// The loopback port the recording intake listens on.
    pub port: u16,

    /// The API key ADP is configured to send to this endpoint.
    pub api_key: String,

    /// What kind of endpoint it is in ADP's configuration.
    pub kind: RecipientKind,

    /// Whether it was added while the demo runs, with **Add destination**.
    pub added: bool,
}

impl Recipient {
    /// Returns a destination added while the demo runs, as an additional endpoint: its name, label, and API key follow
    /// from `letter` (`dest-d`, `Destination D`, `demo-dest-d` for `D`).
    pub fn added(letter: char, port: u16) -> Self {
        let lower = letter.to_ascii_lowercase();
        Self {
            name: format!("dest-{}", lower),
            label: format!("Destination {}", letter.to_ascii_uppercase()),
            port,
            api_key: format!("demo-dest-{}", lower),
            kind: RecipientKind::Additional,
            added: true,
        }
    }

    /// Returns the endpoint URL as ADP's configuration writes it, which is also the name ADP's recipient directory
    /// gives an additional endpoint: the forwarder resolves `http://127.0.0.1:<port>` to itself.
    pub fn url(&self) -> String {
        DemoPorts::http_url(self.port)
    }

    /// Returns `true` if the recipient is an additional endpoint, which the demo can remove from the configuration.
    pub fn removable(&self) -> bool {
        self.kind == RecipientKind::Additional
    }
}

/// The lanes the launcher asks ADP to run the metrics path on: the counts given on the command line, each `None` when
/// not given.
///
/// With none given, the launcher writes no lane keys, and ADP lays its metrics path out as its own defaults say. With
/// any given, it writes `experimental.programmable_metrics.lanes.mode: auto` and the counts given; ADP derives the
/// others from the host, and `0` is ADP's automatic count, as its keys define it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LaneFlags {
    /// The DogStatsD decoders, `dogstatsd_workers_count`, which are the producer lanes up to 16.
    pub decoders: Option<u16>,

    /// The state lanes of DogStatsD aggregation, `experimental.programmable_metrics.lanes.state`.
    pub state: Option<u16>,

    /// The post-aggregation lanes, `experimental.programmable_metrics.lanes.post`.
    pub post: Option<u16>,
}

impl LaneFlags {
    /// Returns `true` if any count is given: the metrics path then runs on lanes.
    pub fn any(&self) -> bool {
        self.decoders.is_some() || self.state.is_some() || self.post.is_some()
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

    /// How many load contexts the traffic generator cycles through besides the demo's metric set (`0`: none).
    pub contexts: u64,

    /// The lanes ADP is asked to run its metrics path on.
    pub lanes: LaneFlags,
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
