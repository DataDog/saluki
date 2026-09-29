//! How the client presents itself to the Agent.

/// The kind of client the Agent sees, and the details that kind reports.
///
/// The Agent requires every client to be exactly one kind, and checks that the details for that kind are present.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ClientKind {
    // TODO: consider Tracer and Updater variants if a use case needs them; this is an enum so they can be added
    // without changing the settings. A tracer's details decide which configurations it receives, through the Agent's
    // tracer predicates, and a tracer advertises capabilities. An updater reports the state of the packages it
    // manages, which changes while it runs, so it would need a way to update that state on a running client.
    /// A Datadog Agent process, or a process that runs alongside one.
    Agent(AgentIdentity),
}

/// What an [`Agent`](ClientKind::Agent) client reports.
///
/// The Agent does not act on any of these fields. It forwards them to the Datadog backend and lists them among its
/// active clients in `datadog-agent remote-config`.
#[derive(Clone, Debug)]
pub struct AgentIdentity {
    /// The name of the running application, such as `agent-data-plane`.
    ///
    /// Sent as `client_agent.name` in every poll. Must not be empty.
    ///
    /// Has no default.
    pub name: String,

    /// The version of the running application, such as `1.7.0`.
    ///
    /// Sent as `client_agent.version` in every poll. Must not be empty.
    ///
    /// Has no default.
    pub version: String,

    /// The name of the Kubernetes cluster the application manages. Optional, and only for a cluster-level agent.
    ///
    /// Sent as `client_agent.cluster_name` in every poll. The Cluster Agent reports it; an agent that runs on a single
    /// host leaves it unset, which sends it empty.
    ///
    /// Defaults to `None`.
    pub cluster_name: Option<String>,

    /// The ID of the Kubernetes cluster the application manages. Optional, and only for a cluster-level agent.
    ///
    /// Sent as `client_agent.cluster_id` in every poll. The Cluster Agent reports it; an agent that runs on a single
    /// host leaves it unset, which sends it empty.
    ///
    /// Defaults to `None`.
    pub cluster_id: Option<String>,
}

impl AgentIdentity {
    /// Creates the identity of an agent named `name` at `version`, with no cluster.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            cluster_name: None,
            cluster_id: None,
        }
    }
}
